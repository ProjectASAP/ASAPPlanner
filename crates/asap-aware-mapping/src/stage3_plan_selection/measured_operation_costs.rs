//! Measured CPU time of summary operations, which plan selection prices
//! from instead of its formulas.
//!
//! A deployment measures what one update, merge or estimate of a sketch
//! with given parameters costs (e.g. sketch-bench's `rqe_atomic_costs.json`,
//! converted by `asap-devtools`' `convert_rqe_atomic_costs`) and puts the
//! table in [`PlanSelectionSettings::measured_operation_costs`]. Stage 3
//! prices a sketch node (an operator node of the physical plan DAG) from
//! the entry for its sketch parameters and operation when there is one, and
//! from its formulas otherwise, and reports which per node
//! ([`NodeCost::price_source`]). The planner reads
//! only this table, never a benchmark file.
//!
//! A measured time enters [`ResourceUsage`] as CPU operations: `t` ns is
//! `t · 10⁻⁶ / ms_per_cpu_op` operations of [`ResourceSpeed`], so latency
//! (operations × `ms_per_cpu_op`) is the measured time, and every cost model
//! prices it as it prices any operation.
//!
//! [`PlanSelectionSettings::measured_operation_costs`]: super::PlanSelectionSettings::measured_operation_costs
//! [`NodeCost::price_source`]: super::NodeCost::price_source
//! [`ResourceUsage`]: super::ResourceUsage
//! [`ResourceSpeed`]: super::ResourceSpeed

use asap_types::post_asap::SketchParams;
use serde::{Deserialize, Serialize};

use crate::analytical_cost::AnalyticalCostError;

/// One operation on a sketch's state, as plan selection prices it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryOperation {
    /// Insert one input row (a `SummaryAgg` or `PaneChain` build).
    Update,
    /// Merge one input state into another (a `SummaryMerge`).
    Merge,
    /// Read one result out of one state (a `SummaryEstimate`, per group).
    Estimate,
}

/// The measured CPU time of one operation on one sketch configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredOperationCost {
    /// The sketch and its parameters, e.g. `{"Kll": {"k": 200}}`; matched
    /// with `SketchParams`' equality, which ignores the error and failure
    /// probability a sketch was sized for.
    pub sketch: SketchParams,
    /// Which operation was measured.
    pub operation: SummaryOperation,
    /// CPU nanoseconds one such operation takes on one core.
    pub cpu_ns_per_operation: f64,
}

/// Which prices one node's CPU operations came from. A node is one operator
/// node of the candidate's physical plan DAG (`PhysicalASAPDAG`), keyed by
/// its `PhysicalASAPNodeId` in [`CandidateCost::per_node`]; not a machine.
///
/// [`CandidateCost::per_node`]: super::CandidateCost::per_node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePriceSource {
    /// Plan selection's formulas (`analytical_cost` and the sketch shapes).
    Analytical,
    /// The entry of [`MeasuredOperationCost`] for its sketch and operation.
    Measured,
}

/// The measured CPU nanoseconds of `operation` on `sketch`, if `table` has
/// an entry for them.
pub(super) fn measured_cpu_ns(
    table: &[MeasuredOperationCost],
    sketch: &SketchParams,
    operation: SummaryOperation,
) -> Option<f64> {
    table
        .iter()
        .find(|entry| entry.operation == operation && &entry.sketch == sketch)
        .map(|entry| entry.cpu_ns_per_operation)
}

/// Bad input: an entry whose time is negative or not finite, two entries
/// for one sketch and operation, or a table with `ms_per_cpu_op` 0 (a
/// measured time cannot then be counted in operations).
pub(super) fn validate(
    table: &[MeasuredOperationCost],
    ms_per_cpu_op: f64,
) -> Result<(), AnalyticalCostError> {
    if !table.is_empty() && ms_per_cpu_op <= 0.0 {
        return Err(AnalyticalCostError::MeasuredCostsWithoutCpuSpeed(
            ms_per_cpu_op,
        ));
    }
    for (index, entry) in table.iter().enumerate() {
        let ns = entry.cpu_ns_per_operation;
        if !ns.is_finite() || ns < 0.0 {
            return Err(AnalyticalCostError::InvalidCalibration(
                "measured_operation_costs.cpu_ns_per_operation",
                ns,
            ));
        }
        if table[..index]
            .iter()
            .any(|earlier| earlier.operation == entry.operation && earlier.sketch == entry.sketch)
        {
            return Err(AnalyticalCostError::DuplicateMeasuredOperationCost {
                sketch: entry.sketch.clone(),
                operation: entry.operation,
            });
        }
    }
    Ok(())
}
