//! The deployment's measured operation costs: the CPU time its runtime pays
//! per unit of work of each physical operator, which plan selection prices
//! from instead of its formulas.
//!
//! The Planner defines the table's structure; the deployment's runtime
//! measures the numbers (e.g. ASAPQuery-backend's `measure_costs`, or
//! sketch-bench's `rqe_atomic_costs.json` converted by `asap-devtools`'
//! `convert_rqe_atomic_costs`) and puts the table in
//! [`PlanSelectionSettings::measured_operation_costs`]. The planner reads
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
//! measured one (module doc of the crate, "Measured operation costs").
//!
//! A measured time enters [`ResourceUsage`] as CPU operations: `t` ns is
//! `t · 10⁻⁶ / ms_per_cpu_op` operations of [`ResourceSpeed`], so latency
//! (operations × `ms_per_cpu_op`) is the measured time, and every cost model
//! prices it as it prices any operation.
//!
//! The table of earlier versions, a JSON array of [`MeasuredOperationCost`]
//! (one sketch operation each), still loads: each entry becomes the
//! [`OperatorCost`] of its operator at both phases.
//!
//! [`PlanSelectionSettings::measured_operation_costs`]: super::PlanSelectionSettings::measured_operation_costs
//! [`ResourceUsage`]: super::ResourceUsage
//! [`ResourceSpeed`]: super::ResourceSpeed

use std::mem::discriminant;

use asap_types::ir::operator::AggIntent;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload as Payload;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{ExactKind, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use serde::{Deserialize, Serialize};

use crate::cost::analytical_cost::AnalyticalCostError;

/// The deployment's measured cost table
/// ([`PlanSelectionSettings::measured_operation_costs`]). In JSON, an object
/// with these fields, or (the format of earlier versions) an array of
/// [`MeasuredOperationCost`]. Default: empty, every node priced from the
/// formulas.
///
/// [`PlanSelectionSettings::measured_operation_costs`]: super::PlanSelectionSettings::measured_operation_costs
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MeasuredOperationCosts {
    /// One entry per (operator, phase, family) the runtime measured.
    pub operators: Vec<OperatorCost>,
    /// CPU nanoseconds the runtime pays once per query request, whatever
    /// its plan: scheduling, request handling and result encoding. Default 0.
    pub ns_per_evaluation: f64,
    /// Which executor and machine the numbers were measured with; the table
    /// is re-measured when either changes. Recorded, not read.
    pub provenance: Option<CostTableProvenance>,
}

/// Where a measured table's numbers come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostTableProvenance {
    /// The executor build measured, e.g. a crate version or commit.
    pub executor_version: String,
    /// The machine it ran on, e.g. a CPU model or instance type.
    pub machine: String,
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
    /// CPU nanoseconds per unit the operator processes ([`OperatorKind`]).
    pub ns_per_unit: f64,
    /// CPU nanoseconds per computation of the node, whatever its units:
    /// per evaluation at query time, per second of arriving rows at
    /// ingestion time. Default 0.
    #[serde(default)]
    pub ns_per_invocation: f64,
    /// For a node that keeps summary state (`SummaryAgg`, `PaneChain`):
    /// the bytes one state holds, from the kernel's own `memory_bytes()`
    /// (e.g. KLL after lazy allocation). `None`: the bytes the formulas
    /// derive from the sketch's parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes_per_state: Option<f64>,
}

/// Every physical operator a physical plan DAG can contain, by the name it
/// has there. The unit each is priced per is the row it reads unless said
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OperatorKind {
    /// Per sample read. At ingestion time, an arriving sample; at query
    /// time, a sample fetched and decoded from the database holding raw
    /// data, part of the deployment's cost.
    Scan,
    Values,
    Filter,
    Project,
    /// Per input row, for each measure.
    Aggregate,
    Join,
    SetOp,
    Concat,
    Dedup,
    Sort,
    Limit,
    BinaryOp,
    SQLWindowFunc,
    TimeRange,
    TimeShift,
    PromqlVectorFromScalar,
    PromqlRelabel,
    PromqlInfoEnrich,
    PromqlSeriesSample,
    PromqlSubquery,
    /// Per input row inserted into its group's state.
    SummaryAgg,
    /// Per group read.
    SummaryEstimate,
    /// Per accumulator finalized (one per group).
    FinalizeExactAccumulator,
    MaintainPopulation,
    EvaluatePopulation,
    /// Per input state merged (each pane of a chain is a state).
    SummaryMerge,
    /// Per input row placed in its pane and inserted into that pane's state.
    PaneChain,
    SummarySubtract,
    SummaryDelete,
    SummaryJoin,
    Extension,
}

impl OperatorKind {
    /// The kind of `payload`.
    pub fn of(payload: &Payload) -> Self {
        match payload {
            Operator::NonASAP(op) => match op {
                NonASAPOp::Scan { .. } => Self::Scan,
                NonASAPOp::Values { .. } => Self::Values,
                NonASAPOp::Filter { .. } => Self::Filter,
                NonASAPOp::Project { .. } => Self::Project,
                NonASAPOp::Aggregate { .. } => Self::Aggregate,
                NonASAPOp::Join { .. } => Self::Join,
                NonASAPOp::SetOp { .. } => Self::SetOp,
                NonASAPOp::Concat { .. } => Self::Concat,
                NonASAPOp::Dedup { .. } => Self::Dedup,
                NonASAPOp::Sort { .. } => Self::Sort,
                NonASAPOp::Limit { .. } => Self::Limit,
                NonASAPOp::BinaryOp { .. } => Self::BinaryOp,
                NonASAPOp::SQLWindowFunc { .. } => Self::SQLWindowFunc,
                NonASAPOp::TimeRange { .. } => Self::TimeRange,
                NonASAPOp::TimeShift { .. } => Self::TimeShift,
                NonASAPOp::PromqlVectorFromScalar(_) => Self::PromqlVectorFromScalar,
                NonASAPOp::PromqlRelabel { .. } => Self::PromqlRelabel,
                NonASAPOp::PromqlInfoEnrich { .. } => Self::PromqlInfoEnrich,
                NonASAPOp::PromqlSeriesSample { .. } => Self::PromqlSeriesSample,
                NonASAPOp::PromqlSubquery { .. } => Self::PromqlSubquery,
            },
            Operator::ASAP(op) => match op {
                ASAPOp::SummaryAgg { .. } => Self::SummaryAgg,
                ASAPOp::SummaryEstimate { .. } => Self::SummaryEstimate,
                ASAPOp::FinalizeExactAccumulator { .. } => Self::FinalizeExactAccumulator,
                ASAPOp::MaintainPopulation { .. } => Self::MaintainPopulation,
                ASAPOp::EvaluatePopulation { .. } => Self::EvaluatePopulation,
                ASAPOp::SummaryMerge { .. } => Self::SummaryMerge,
                ASAPOp::PaneChain { .. } => Self::PaneChain,
                ASAPOp::SummarySubtract { .. } => Self::SummarySubtract,
                ASAPOp::SummaryDelete { .. } => Self::SummaryDelete,
                ASAPOp::SummaryJoin { .. } => Self::SummaryJoin,
                ASAPOp::Extension { .. } => Self::Extension,
            },
        }
    }
}

/// The state family a summary node works on, which selects its entries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StateFamily<'a> {
    /// No family: a non-summary operator, or a shared (Hydra) structure.
    None,
    Sketch(&'a SketchParams),
    Exact(&'a ExactKind),
}

/// A node's unit price, from its entry or interpolated between two.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct UnitPrice {
    pub ns_per_unit: f64,
    pub ns_per_invocation: f64,
    pub memory_bytes_per_state: Option<f64>,
}

/// The price of `operator` at `phase` on `family`, if the table has an
/// entry for them. Entries for the node's own phase win over entries for
/// both phases. A sketch with exactly an entry's parameters takes that
/// entry; otherwise, among the entries for the same sketch algorithm, the
/// two whose sizes (`size_of`: the bytes one state allocates) are nearest
/// below and above the node's are interpolated linearly in log size, and a
/// size outside the measured ones takes the nearest entry's price.
pub(super) fn lookup(
    table: &MeasuredOperationCosts,
    operator: OperatorKind,
    phase: ExecutionTiming,
    family: StateFamily<'_>,
    size_of: impl Fn(&SketchParams) -> f64,
) -> Option<UnitPrice> {
    let matching = |for_phase: Option<ExecutionTiming>| {
        table
            .operators
            .iter()
            .filter(move |entry| {
                entry.operator == operator
                    && entry.phase == for_phase
                    && entry.measure.is_none()
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
            })
        }
        (Some(nearest), _) | (None, Some(nearest)) => Some(price_of(nearest)),
        (None, None) => None,
    }
}

/// The price of an `Aggregate` at `phase` per input row: the sum over its
/// measures, when each has an entry (one for its own phase winning over
/// one for both).
pub(super) fn aggregate_lookup(
    table: &MeasuredOperationCosts,
    phase: ExecutionTiming,
    measures: &[AggIntent],
) -> Option<UnitPrice> {
    let mut total = UnitPrice {
        ns_per_unit: 0.0,
        ns_per_invocation: 0.0,
        memory_bytes_per_state: None,
    };
    for measure in measures {
        let name = measure_name(measure);
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
    }
    (!measures.is_empty()).then_some(total)
}

/// An aggregate measure's name as an entry states it: its JSON `kind`.
fn measure_name(measure: &AggIntent) -> String {
    serde_json::to_value(measure)
        .ok()
        .and_then(|value| value.get("kind")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{measure:?}"))
}

fn price_of(entry: &OperatorCost) -> UnitPrice {
    UnitPrice {
        ns_per_unit: entry.ns_per_unit,
        ns_per_invocation: entry.ns_per_invocation,
        memory_bytes_per_state: entry.memory_bytes_per_state,
    }
}

/// Bad input: a time or size that is negative or not finite, an entry with
/// more than one of `sketch`, `exact` and `measure`, two entries for one
/// operator, phase and family, or a non-empty table with `ms_per_cpu_op` 0
/// (a measured time cannot then be counted in operations).
pub(super) fn validate(
    table: &MeasuredOperationCosts,
    ms_per_cpu_op: f64,
) -> Result<(), AnalyticalCostError> {
    let measured = !table.operators.is_empty() || table.ns_per_evaluation != 0.0;
    if measured && ms_per_cpu_op <= 0.0 {
        return Err(AnalyticalCostError::MeasuredCostsWithoutCpuSpeed(
            ms_per_cpu_op,
        ));
    }
    let check = |name, value: f64| match value.is_finite() && value >= 0.0 {
        true => Ok(()),
        false => Err(AnalyticalCostError::InvalidCalibration(name, value)),
    };
    check(
        "measured_operation_costs.ns_per_evaluation",
        table.ns_per_evaluation,
    )?;
    for (index, entry) in table.operators.iter().enumerate() {
        check("measured_operation_costs.ns_per_unit", entry.ns_per_unit)?;
        check(
            "measured_operation_costs.ns_per_invocation",
            entry.ns_per_invocation,
        )?;
        if let Some(bytes) = entry.memory_bytes_per_state {
            check("measured_operation_costs.memory_bytes_per_state", bytes)?;
        }
        let families = [
            entry.sketch.is_some(),
            entry.exact.is_some(),
            entry.measure.is_some(),
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
        }) {
            return Err(AnalyticalCostError::DuplicateOperatorCost(key()));
        }
    }
    Ok(())
}

// ── The format of earlier versions ──────────────────────────────────────

/// One operation on a sketch's state, as an entry of the earlier format
/// ([`MeasuredOperationCost`]) names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryOperation {
    /// Insert one input row: a `SummaryAgg` or `PaneChain` entry.
    Update,
    /// Merge one input state into another: a `SummaryMerge` entry.
    Merge,
    /// Read one result out of one state: a `SummaryEstimate` entry.
    Estimate,
}

/// An entry of the earlier format, still loadable: the measured CPU time of
/// one operation on one sketch configuration. It becomes the
/// [`OperatorCost`] of its operator (both `SummaryAgg` and `PaneChain` for
/// an update) at both phases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredOperationCost {
    /// The sketch and its parameters, e.g. `{"Kll": {"k": 200}}`.
    pub sketch: SketchParams,
    /// Which operation was measured.
    pub operation: SummaryOperation,
    /// CPU nanoseconds one such operation takes on one core.
    pub cpu_ns_per_operation: f64,
}

impl From<Vec<MeasuredOperationCost>> for MeasuredOperationCosts {
    fn from(entries: Vec<MeasuredOperationCost>) -> Self {
        let operators = entries
            .into_iter()
            .flat_map(|entry| {
                let kinds: &[OperatorKind] = match entry.operation {
                    SummaryOperation::Update => {
                        &[OperatorKind::SummaryAgg, OperatorKind::PaneChain]
                    }
                    SummaryOperation::Merge => &[OperatorKind::SummaryMerge],
                    SummaryOperation::Estimate => &[OperatorKind::SummaryEstimate],
                };
                kinds
                    .iter()
                    .map(|&operator| OperatorCost {
                        operator,
                        phase: None,
                        sketch: Some(entry.sketch.clone()),
                        exact: None,
                        measure: None,
                        ns_per_unit: entry.cpu_ns_per_operation,
                        ns_per_invocation: 0.0,
                        memory_bytes_per_state: None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Self {
            operators,
            ..Self::default()
        }
    }
}

/// Reads the table, or an array in the earlier format.
impl<'de> Deserialize<'de> for MeasuredOperationCosts {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.is_array() {
            return Vec::<MeasuredOperationCost>::deserialize(value)
                .map(Self::from)
                .map_err(D::Error::custom);
        }
        let fields = TableFields::deserialize(value).map_err(D::Error::custom)?;
        Ok(Self {
            operators: fields.operators,
            ns_per_evaluation: fields.ns_per_evaluation,
            provenance: fields.provenance,
        })
    }
}

/// The table's fields as JSON states them; omitted ones take their default.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TableFields {
    operators: Vec<OperatorCost>,
    ns_per_evaluation: f64,
    provenance: Option<CostTableProvenance>,
}

/// Which prices one node's CPU operations came from. A node is one operator
/// node of the candidate's physical plan DAG (`PhysicalASAPDAG`), keyed by
/// its `PhysicalASAPNodeId` in [`CandidateCost::per_node`]; not a machine.
///
/// [`CandidateCost::per_node`]: super::CandidateCost::per_node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePriceSource {
    /// Plan selection's formulas (`analytical_cost` and the sketch shapes),
    /// scaled by the calibration factor when the table has entries.
    Analytical,
    /// The table's entry for its operator, phase and family, or two
    /// entries interpolated.
    Measured,
}
