//! The deployment's measured operation costs: the CPU time its runtime pays
//! per unit of work of each physical operator, which plan selection prices
//! from instead of its formulas.
//!
//! The Planner defines the table's structure; the deployment's runtime
//! measures the numbers (e.g. sketch-bench's `rqe_atomic_costs.json`
//! converted by `asap-devtools`' `convert_rqe_atomic_costs`) and puts the
//! table in
//! [`CostInputs::measured_operation_costs`]. The planner reads
//! only this table, never a benchmark file.
//!
//! Each [`OperatorCost`] prices one operator kind at one phase (ingestion
//! or query time), for one summary family or aggregate measure: a node's
//! CPU per computation is `units × ns_per_unit + ns_per_invocation`, its
//! units being what the operator processes ([`OperatorKind`]). A sketch
//! entry also applies to the same sketch with other parameters,
//! interpolated between the two nearest measured sizes ([`lookup`]). A
//! node without an entry is priced from the formulas, scaled by one
//! calibration factor fitted from the nodes that have one, so measured and
//! formula-priced nodes stay on one scale. The factor is a global
//! heuristic: a partial table can still mis-rank plans, and only a table
//! covering every operator kind the plans use makes the choice the
//! measured one (plan selection's module doc, "Measured operation costs").
//!
//! Without a table, the formulas are not unit-free guesses: they carry the
//! reference executor's measured time per unit of work
//! (`node::operations_per_unit`), the built-in fallback. A table measured
//! on the deployment's own machine (by a calibration run of its executor)
//! overrides them node by node.
//!
//! A measured time enters [`ResourceUsage`] as CPU operations: `t` ns is
//! `t · 10⁻⁶ / ms_per_cpu_op` operations of [`ResourceSpeed`], so latency
//! (operations × `ms_per_cpu_op`) is the measured time, and every cost model
//! prices it as it prices any operation.
//!
//! [`CostInputs::measured_operation_costs`]: crate::CostInputs::measured_operation_costs
//! [`ResourceUsage`]: crate::ResourceUsage
//! [`ResourceSpeed`]: crate::ResourceSpeed

use std::mem::discriminant;

use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::summary_type::{ExactKind, SketchParams};
use serde::{Deserialize, Serialize};

use crate::node::analytical::AnalyticalCostError;
/// The operator vocabulary the table is keyed by; it lives with the IR.
pub use asap_types::ir::operator::OperatorKind;

/// The deployment's measured cost table
/// ([`CostInputs::measured_operation_costs`]). In JSON, an object with
/// these fields; an omitted field takes its default, an unknown one is
/// refused. Default: empty, every node priced from the formulas.
///
/// [`CostInputs::measured_operation_costs`]: crate::CostInputs::measured_operation_costs
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeasuredOperationCosts {
    /// One entry per (operator, phase, family) the runtime measured.
    pub operators: Vec<OperatorCost>,
    /// CPU nanoseconds the runtime pays once per query request, whatever
    /// its plan: scheduling, request handling and result encoding. Default 0.
    pub ns_per_evaluation: f64,
    /// CPU nanoseconds the runtime pays once per engine query run: one run
    /// per step of a request (an instant request is one step). Default 0.
    #[serde(skip_serializing_if = "is_zero")]
    pub ns_per_evaluation_step: f64,
    /// CPU nanoseconds per arriving sample outside every plan node (decode,
    /// append), the same for every plan: it counts in the ingest role and in
    /// cost model 3's CPU limit, not in cost model 1's cost. Default 0.
    #[serde(skip_serializing_if = "is_zero")]
    pub ns_per_ingested_sample: f64,
    /// Wall time a window-close refresh waits on I/O (writing views,
    /// syncs), not CPU. Default none.
    #[serde(skip_serializing_if = "RefreshIoWait::is_none")]
    pub refresh_io_wait: RefreshIoWait,
}

/// Wall time a refresh waits on I/O, beside its CPU
/// ([`MeasuredOperationCosts::refresh_io_wait`]). It adds to the refresh's
/// latency and to the precompute load (a waiting thread is busy), not to
/// CPU or cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshIoWait {
    /// Wall nanoseconds per refresh-group run (per window close).
    pub ns_per_refresh: f64,
    /// Wall nanoseconds per window state a refresh emits.
    pub ns_per_state_emitted: f64,
}

impl RefreshIoWait {
    /// No wait.
    pub fn is_none(&self) -> bool {
        self.ns_per_refresh == 0.0 && self.ns_per_state_emitted == 0.0
    }
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

/// The measured CPU time of one operator kind, at one phase, for one family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCost {
    /// The operator measured.
    pub operator: OperatorKind,
    /// When it runs: `ingestion_time` or `query_time`; `None` (omitted)
    /// for both. An entry for the node's own phase wins over one for both.
    #[serde(default)]
    pub phase: Option<ExecutionTiming>,
    /// For a summary operator over a sketch: the sketch and its parameters,
    /// e.g. `{"Kll": {"k": 200}}`. Other parameters of the same sketch are
    /// priced by interpolation ([`lookup`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sketch: Option<SketchParams>,
    /// For a summary operator over an exact accumulator: its kind, e.g.
    /// `"Sum"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<ExactKind>,
    /// For an `Aggregate`: the measure's `kind`, e.g. `"quantile"` (an
    /// `AggIntent` in JSON). An aggregate is priced from the table only
    /// when each of its measures has an entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<String>,
    /// CPU nanoseconds per unit the operator processes. The unit is the
    /// row the operator reads unless listed here:
    ///
    /// * `Scan`: per sample read. At ingestion time, an arriving sample; at
    ///   query time, a sample fetched and decoded from the database holding
    ///   raw data, part of the deployment's cost;
    /// * `Aggregate`: per input row, for each measure;
    /// * `SummaryAgg`: per input row inserted into its group's state;
    /// * `SummaryEstimate`: per group read;
    /// * `FinalizeExactAccumulator`: per accumulator finalized (one per
    ///   group);
    /// * `SummaryMerge`: per input state merged (each tumbling window of a
    ///   chain is a state);
    /// * `SummaryTumblingWindowChain`: per input row placed in its tumbling
    ///   window and inserted into that window's state.
    ///
    /// It includes what the formulas charge apart, at the measured shape:
    /// the output rows a merge, an aggregate or an estimate builds per
    /// group and the label columns a grouping carries per row. Example: a
    /// `SummaryMerge` entry measured over 360 states per group holds its
    /// per-group output row as a 360th of it per state; at 5 states per
    /// group that row is under-charged.
    pub ns_per_unit: f64,
    /// CPU nanoseconds per computation of the node, whatever its units:
    /// per evaluation at query time, per second of arriving rows at
    /// ingestion time. Default 0.
    #[serde(default)]
    pub ns_per_invocation: f64,
    /// For a node that keeps summary state (`SummaryAgg`, `SummaryTumblingWindowChain`):
    /// the bytes one state holds, from the kernel's own `memory_bytes()`
    /// (e.g. KLL after lazy allocation). `None`: the bytes the formulas
    /// derive from the sketch's parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes_per_state: Option<f64>,
    /// At ingestion time: CPU nanoseconds per window state the node emits
    /// at a refresh (window close), charged beside `ns_per_unit` and
    /// `ns_per_invocation`. Default 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ns_per_state_emitted: f64,
    /// The series the node's sources held when it was measured. Entries of
    /// one class at several counts are interpolated by the node's own
    /// series count, linearly in log(price) over log(series) ([`lookup`]);
    /// `None`: one price at every scale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_series_count: Option<u64>,
    /// For an `Aggregate`: the exact combination of measures the entry was
    /// measured on (their `kind`s, e.g. `["sum", "count"]`); it prices an
    /// `Aggregate` with exactly those measures, winning over the
    /// single-measure entries. Empty: not a combination entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub measures: Vec<String>,
}

/// The state family a summary node works on, which selects its entries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StateFamily<'a> {
    /// No family: a non-summary operator, or a shared (Hydra) structure.
    None,
    Sketch(&'a SketchParams),
    Exact(&'a ExactKind),
}

/// A node's unit price, from its entry or interpolated between two.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitPrice {
    pub ns_per_unit: f64,
    pub ns_per_invocation: f64,
    pub memory_bytes_per_state: Option<f64>,
    /// CPU ns per window state emitted at a refresh.
    pub ns_per_state_emitted: f64,
    /// Whether the node's series count is outside the measured series
    /// counts, so the nearest scale's price was taken.
    pub outside_measured_scale: bool,
}

/// The price of `operator` at `phase` on `family`, if the table has an
/// entry for them. Entries for the node's own phase win over entries for
/// both phases. A sketch with exactly an entry's parameters takes that
/// entry; otherwise, among the entries for the same sketch algorithm, the
/// two whose sizes (`size_of`: the bytes one state allocates) are nearest
/// below and above the node's are interpolated linearly in log size, and a
/// size outside the measured ones takes the nearest entry's price.
pub fn lookup(
    table: &MeasuredOperationCosts,
    operator: OperatorKind,
    phase: ExecutionTiming,
    family: StateFamily<'_>,
    size_of: impl Fn(&SketchParams) -> f64,
) -> Option<UnitPrice> {
    lookup_at_scale(table, operator, phase, family, size_of, None)
}

/// [`lookup`] for a node whose sources hold `series` series. When some of
/// the matching entries name a `measured_series_count`, only those are
/// used: the sketch parameters are chosen as [`lookup`] chooses them (the
/// exact parameters, else the nearest size), then the two entries of those
/// parameters whose series counts are nearest below and above `series`
/// are interpolated linearly in log(price) over log(series); a count
/// outside the measured ones (or unknown) takes the nearest entry's price
/// and marks the price [`UnitPrice::outside_measured_scale`].
///
/// Example: `SummaryMerge` entries of 100 ns per state at 1,000 series and
/// 1,000 ns at 10,000: about 316 ns at 3,162 series; 1,000 ns, outside
/// the measured scale, at 20,000.
pub fn lookup_at_scale(
    table: &MeasuredOperationCosts,
    operator: OperatorKind,
    phase: ExecutionTiming,
    family: StateFamily<'_>,
    size_of: impl Fn(&SketchParams) -> f64,
    series: Option<u64>,
) -> Option<UnitPrice> {
    let matching = |for_phase: Option<ExecutionTiming>| {
        table
            .operators
            .iter()
            .filter(move |entry| {
                entry.operator == operator
                    && entry.phase == for_phase
                    && entry.measure.is_none()
                    && entry.measures.is_empty()
                    && match family {
                        StateFamily::None => entry.sketch.is_none() && entry.exact.is_none(),
                        StateFamily::Exact(kind) => {
                            entry.sketch.is_none() && entry.exact.as_ref() == Some(kind)
                        }
                        StateFamily::Sketch(params) => {
                            entry.exact.is_none()
                                && entry
                                    .sketch
                                    .as_ref()
                                    .is_some_and(|s| discriminant(s) == discriminant(params))
                        }
                    }
            })
            .collect::<Vec<_>>()
    };
    let mut entries = matching(Some(phase));
    if entries.is_empty() {
        entries = matching(None);
    }
    if entries
        .iter()
        .any(|entry| entry.measured_series_count.is_some())
    {
        entries.retain(|entry| entry.measured_series_count.is_some());
        if let StateFamily::Sketch(params) = family {
            // The parameters measured: the node's own, else the nearest size.
            let target = size_of(params).ln();
            let chosen = entries
                .iter()
                .find(|entry| entry.sketch.as_ref() == Some(params))
                .or_else(|| {
                    entries.iter().min_by(|a, b| {
                        let distance = |entry: &&OperatorCost| {
                            (size_of(entry.sketch.as_ref().expect("a sketch entry")).ln() - target)
                                .abs()
                        };
                        distance(a).total_cmp(&distance(b))
                    })
                })?
                .sketch
                .clone();
            entries.retain(|entry| entry.sketch == chosen);
        }
        return at_series_count(&entries, series);
    }
    let StateFamily::Sketch(params) = family else {
        return entries.first().map(|entry| price_of(entry));
    };
    if let Some(entry) = entries.iter().find(|e| e.sketch.as_ref() == Some(params)) {
        return Some(price_of(entry));
    }
    let size = |entry: &OperatorCost| size_of(entry.sketch.as_ref().expect("a sketch entry"));
    let target = size_of(params);
    let below = entries
        .iter()
        .filter(|e| size(e) <= target)
        .max_by(|a, b| size(a).total_cmp(&size(b)));
    let above = entries
        .iter()
        .filter(|e| size(e) >= target)
        .min_by(|a, b| size(a).total_cmp(&size(b)));
    match (below, above) {
        (Some(low), Some(high)) if size(high) > size(low) => {
            let at = (target.ln() - size(low).ln()) / (size(high).ln() - size(low).ln());
            let (low, high) = (price_of(low), price_of(high));
            let mix = |a: f64, b: f64| a + (b - a) * at;
            Some(UnitPrice {
                ns_per_unit: mix(low.ns_per_unit, high.ns_per_unit),
                ns_per_invocation: mix(low.ns_per_invocation, high.ns_per_invocation),
                memory_bytes_per_state: low
                    .memory_bytes_per_state
                    .zip(high.memory_bytes_per_state)
                    .map(|(a, b)| mix(a, b)),
                ns_per_state_emitted: mix(low.ns_per_state_emitted, high.ns_per_state_emitted),
                outside_measured_scale: false,
            })
        }
        (Some(nearest), _) | (None, Some(nearest)) => Some(price_of(nearest)),
        (None, None) => None,
    }
}

/// The price of the entries `entries` (one class and parameters, each with
/// a `measured_series_count`) at `series`: interpolated linearly in
/// log(price) over log(series) between the nearest counts below and above,
/// else the nearest count's, marked outside the measured scale. A zero
/// price interpolates linearly.
fn at_series_count(entries: &[&OperatorCost], series: Option<u64>) -> Option<UnitPrice> {
    let count = |entry: &&&OperatorCost| entry.measured_series_count.expect("a scaled entry");
    let nearest_outside = |entry: &OperatorCost| {
        let mut price = price_of(entry);
        price.outside_measured_scale = true;
        Some(price)
    };
    let Some(series) = series else {
        return nearest_outside(entries.iter().max_by_key(count)?);
    };
    if let Some(entry) = entries.iter().find(|entry| count(entry) == series) {
        return Some(price_of(entry));
    }
    let below = entries
        .iter()
        .filter(|e| count(e) < series)
        .max_by_key(count);
    let above = entries
        .iter()
        .filter(|e| count(e) > series)
        .min_by_key(count);
    match (below, above) {
        (Some(low), Some(high)) => {
            let at = ((series as f64).ln() - (count(&low) as f64).ln())
                / ((count(&high) as f64).ln() - (count(&low) as f64).ln());
            let mix = |a: f64, b: f64| match a > 0.0 && b > 0.0 {
                true => (a.ln() + (b.ln() - a.ln()) * at).exp(),
                false => a + (b - a) * at,
            };
            let (low, high) = (price_of(low), price_of(high));
            Some(UnitPrice {
                ns_per_unit: mix(low.ns_per_unit, high.ns_per_unit),
                ns_per_invocation: mix(low.ns_per_invocation, high.ns_per_invocation),
                memory_bytes_per_state: low
                    .memory_bytes_per_state
                    .zip(high.memory_bytes_per_state)
                    .map(|(a, b)| mix(a, b)),
                ns_per_state_emitted: mix(low.ns_per_state_emitted, high.ns_per_state_emitted),
                outside_measured_scale: false,
            })
        }
        (Some(nearest), None) | (None, Some(nearest)) => nearest_outside(nearest),
        (None, None) => None,
    }
}

/// The price of an `Aggregate` at `phase` per input row: an entry for
/// exactly its combination of measures (`OperatorCost::measures`) when the
/// table has one, else the sum over its measures, when each has an entry
/// (one for its own phase winning over one for both).
pub(crate) fn aggregate_lookup(
    table: &MeasuredOperationCosts,
    phase: ExecutionTiming,
    measures: &[AggIntent],
) -> Option<UnitPrice> {
    let mut names: Vec<String> = measures.iter().map(AggIntent::kind_name).collect();
    names.sort();
    let combination = |for_phase: Option<ExecutionTiming>| {
        table.operators.iter().find(|entry| {
            entry.operator == OperatorKind::Aggregate
                && entry.phase == for_phase
                && !entry.measures.is_empty()
                && {
                    let mut measured = entry.measures.clone();
                    measured.sort();
                    measured == names
                }
        })
    };
    if let Some(entry) = combination(Some(phase)).or_else(|| combination(None)) {
        return Some(price_of(entry));
    }
    let mut total = UnitPrice {
        ns_per_unit: 0.0,
        ns_per_invocation: 0.0,
        memory_bytes_per_state: None,
        ns_per_state_emitted: 0.0,
        outside_measured_scale: false,
    };
    for measure in measures {
        let name = measure.kind_name();
        let find = |for_phase: Option<ExecutionTiming>| {
            table.operators.iter().find(|e| {
                e.operator == OperatorKind::Aggregate
                    && e.phase == for_phase
                    && e.measure.as_deref() == Some(name.as_str())
            })
        };
        let entry = find(Some(phase)).or_else(|| find(None))?;
        total.ns_per_unit += entry.ns_per_unit;
        total.ns_per_invocation += entry.ns_per_invocation;
        total.ns_per_state_emitted += entry.ns_per_state_emitted;
    }
    (!measures.is_empty()).then_some(total)
}

fn price_of(entry: &OperatorCost) -> UnitPrice {
    UnitPrice {
        ns_per_unit: entry.ns_per_unit,
        ns_per_invocation: entry.ns_per_invocation,
        memory_bytes_per_state: entry.memory_bytes_per_state,
        ns_per_state_emitted: entry.ns_per_state_emitted,
        outside_measured_scale: false,
    }
}

/// Bad input: a time or size that is negative or not finite, an entry with
/// more than one of `sketch`, `exact` and `measure`, two entries for one
/// operator, phase and family, or a non-empty table with `ms_per_cpu_op` 0
/// (a measured time cannot then be counted in operations).
pub(crate) fn validate(
    table: &MeasuredOperationCosts,
    ms_per_cpu_op: f64,
) -> Result<(), AnalyticalCostError> {
    let measured = !table.operators.is_empty()
        || table.ns_per_evaluation != 0.0
        || table.ns_per_evaluation_step != 0.0
        || table.ns_per_ingested_sample != 0.0;
    if measured && ms_per_cpu_op <= 0.0 {
        return Err(AnalyticalCostError::MeasuredCostsWithoutCpuSpeed(
            ms_per_cpu_op,
        ));
    }
    let check = |name, value: f64| match value.is_finite() && value >= 0.0 {
        true => Ok(()),
        false => Err(AnalyticalCostError::InvalidCalibration(name, value)),
    };
    for (name, value) in [
        (
            "measured_operation_costs.ns_per_evaluation",
            table.ns_per_evaluation,
        ),
        (
            "measured_operation_costs.ns_per_evaluation_step",
            table.ns_per_evaluation_step,
        ),
        (
            "measured_operation_costs.ns_per_ingested_sample",
            table.ns_per_ingested_sample,
        ),
        (
            "measured_operation_costs.refresh_io_wait.ns_per_refresh",
            table.refresh_io_wait.ns_per_refresh,
        ),
        (
            "measured_operation_costs.refresh_io_wait.ns_per_state_emitted",
            table.refresh_io_wait.ns_per_state_emitted,
        ),
    ] {
        check(name, value)?;
    }
    for (index, entry) in table.operators.iter().enumerate() {
        check("measured_operation_costs.ns_per_unit", entry.ns_per_unit)?;
        check(
            "measured_operation_costs.ns_per_invocation",
            entry.ns_per_invocation,
        )?;
        check(
            "measured_operation_costs.ns_per_state_emitted",
            entry.ns_per_state_emitted,
        )?;
        if let Some(bytes) = entry.memory_bytes_per_state {
            check("measured_operation_costs.memory_bytes_per_state", bytes)?;
        }
        let families = [
            entry.sketch.is_some(),
            entry.exact.is_some(),
            entry.measure.is_some(),
            !entry.measures.is_empty(),
        ];
        let key = || {
            format!(
                "{:?} at {} on {}",
                entry.operator,
                entry.phase.map_or("both phases", ExecutionTiming::as_str),
                match (&entry.sketch, &entry.exact, &entry.measure) {
                    (Some(sketch), _, _) => format!("{sketch:?}"),
                    (_, Some(exact), _) => format!("{exact:?}"),
                    (_, _, Some(measure)) => measure.clone(),
                    _ if !entry.measures.is_empty() => entry.measures.join("+"),
                    _ => "no family".to_string(),
                }
            )
        };
        if families.iter().filter(|set| **set).count() > 1 {
            return Err(AnalyticalCostError::AmbiguousOperatorCost(key()));
        }
        if table.operators[..index].iter().any(|earlier| {
            earlier.operator == entry.operator
                && earlier.phase == entry.phase
                && earlier.sketch == entry.sketch
                && earlier.exact == entry.exact
                && earlier.measure == entry.measure
                && earlier.measures == entry.measures
                && earlier.measured_series_count == entry.measured_series_count
        }) {
            return Err(AnalyticalCostError::DuplicateOperatorCost(key()));
        }
    }
    Ok(())
}

/// Which costs one node's CPU operations came from. A node is one operator
/// node of the candidate's physical plan DAG (`PhysicalASAPDAG`), keyed by
/// its `PhysicalASAPNodeId` in [`PlanCost::per_node`]; not a machine.
///
/// [`PlanCost::per_node`]: crate::PlanCost::per_node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeCostSource {
    /// Plan selection's formulas (`analytical_cost` and the sketch shapes),
    /// scaled by the calibration factor when the table has entries.
    Analytical,
    /// The table's entry for its operator, phase and family, or two
    /// entries interpolated.
    Measured,
}
