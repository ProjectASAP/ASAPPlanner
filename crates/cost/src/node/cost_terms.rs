//! One node's formula CPU operations per execution, split into named
//! per-unit constants times the units of work each multiplies, plus the
//! operations no named constant charges.
//!
//! Example: a per-series `max_over_time(latency[5m])` hash aggregate over
//! 300,000 rows of 1,000 series at 1 Hz visits each series' group once per
//! 10 s raw batch: 30,000 units of `OperationsPerGroupPerRawBatch` (170
//! operations each), beside 300,000 units of `HashAggregateOperationsPerRow`
//! (16), 300,000 of `AggregateMeasureOperationsPerRow` (4) and 1,000 of
//! `OutputRowOperationsPerGroup` (2,500): 13.6 M operations, the formula's
//! charge. A refit of one constant changes the charge by its units times the
//! change.
//!
//! The terms describe the formulas only: they do not depend on
//! `CostInputs::measured_operation_costs` or `frozen_calibration_factor`.

// Tests-only stage: the bodies below are `todo!()` until the tests are
// approved; the declarations are the interface the tests check.

use std::collections::BTreeMap;

use asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode;
use asap_types::workload::DataWorkload;
use serde::{Deserialize, Serialize};

use crate::node::analytical::AnalyticalCostError;
use crate::{CostInputs, ProducerOutput};

/// A named per-unit constant of the cost formulas: CPU operations per unit
/// of work, e.g. `OperationsPerGroupPerRawBatch` (170 per group visit).
/// JSON: snake_case, e.g. `"operations_per_group_per_raw_batch"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostConstant {
    TimeRangeOperationsPerRow,
    OperationsPerGroupPerRawBatch,
    ChainOperationsPerArrivingRow,
    HashAggregateOperationsPerRow,
    HashAggregateOperationsPerFurtherKey,
    AggregateMeasureOperationsPerRow,
    SortOperationsPerRowPerLevel,
    DdsketchUpdateOperationsPerRow,
    KllUpdateOperationsPerRow,
    ExactUpdateOperationsPerRow,
    CounterUpdateOperationsPerRow,
    CarriedLabelOperationsPerRowPerLabel,
    KmvUpdateOperationsPerRow,
    KmvOperationsPerComparedHash,
    ExactDistinctOperationsPerRow,
    ExactDistinctOperationsPerValueDoubling,
    ExactDistinctOperationsPerRowAtFullValueMiss,
    ExactDistinctOperationsPerRowAtFullPairMiss,
    ExactDistinctOperationsPerInsertedValue,
    ExactDistinctOperationsPerInsertedPair,
    SketchUpdateOperationsPerCounter,
    ExactMergeOperationsPerState,
    ExtremumMergeOperationsPerState,
    CounterMergeOperationsPerState,
    DdsketchMergeOperationsPerState,
    KllMergeOperationsPerState,
    KllMergeOperationsPerInputByte,
    KllMergeOperationsPerMergedByte,
    MergeOperationsPerOutputGroup,
    SketchMergeOperationsPerByte,
    KllEstimateOperationsPerByte,
    KllEstimateOperationsPerUnsortedItem,
    SketchEstimateOperationsPerGroup,
    OutputRowOperationsPerGroup,
    SketchChainEmitOperationsPerState,
    ExactChainEmitOperationsPerState,
    KllMergeOperationsPerOutputGroup,
}

impl CostConstant {
    /// Every constant. Empty in this tests-only stage.
    pub const ALL: &'static [CostConstant] = &[];

    /// Its built-in value, e.g. 170.0 for `OperationsPerGroupPerRawBatch`.
    pub fn operations_per_unit(self) -> f64 {
        todo!("CostConstant::operations_per_unit: implemented after the tests are approved")
    }
}

/// One node's formula CPU operations per execution, as units of work per
/// named constant plus the operations no named constant charges.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeCostTerms {
    /// Per constant, the units it multiplies in one execution, e.g. 30,000
    /// group visits for `OperationsPerGroupPerRawBatch`.
    pub units_per_constant: BTreeMap<CostConstant, f64>,
    /// Operations charged without a named constant: one per row a `Scan`
    /// reads, a predicate's operations per row, the rounding a hash
    /// aggregate applies to its per-row operations.
    pub operations_without_constant: f64,
}

impl NodeCostTerms {
    /// `operations_without_constant + Σ units × value(constant)`; with
    /// [`CostConstant::operations_per_unit`] it is the formula's charge.
    pub fn operations_with(&self, value: impl Fn(CostConstant) -> f64) -> f64 {
        let _ = value;
        todo!("NodeCostTerms::operations_with: implemented after the tests are approved")
    }
}

/// The terms of `node`'s formula CPU per execution, from the inputs
/// `node_resource_usage_and_cost` takes except the rate (which scales
/// executions, not units). Independent of
/// `settings.measured_operation_costs` and `frozen_calibration_factor`.
pub fn node_cost_terms<'a>(
    node: &'a PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    window_ms: Option<u64>,
    data: &DataWorkload,
    settings: &'a CostInputs,
) -> Result<NodeCostTerms, AnalyticalCostError> {
    let _ = (node, producers, window_ms, data, settings);
    todo!("node_cost_terms: implemented after the tests are approved")
}

#[cfg(test)]
mod tests;
