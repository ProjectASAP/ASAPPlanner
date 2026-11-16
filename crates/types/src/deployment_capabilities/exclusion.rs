//! Which physical candidates a deployment rules out: a
//! [`CandidateExclusion`] matches a candidate's operator, θ, timing and
//! lifetime ([`CandidateDescription`]). Every field that is set must match
//! (AND); a field left unset matches anything; the exclusions of one
//! deployment are combined with OR.
//!
//! The fields that read a state's family (`summary_family`,
//! `sketch_parameters`, `merged_across_windows`) match the operators that
//! build state (`SummaryAgg`, `SummaryTumblingWindowChain`) only: a merge,
//! estimate or finalization over an excluded build has no producer left,
//! so it is pruned as unbuildable without guessing its family from its
//! output schema.

use serde::{Deserialize, Serialize};

use crate::ir::operator::OperatorKind;
use crate::ir::properties::ExecutionTiming;
use crate::ir::schema::FieldDataType;
use crate::ir::Operator;
use crate::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPDAGNode};
use crate::stages::PhysicalProperties;
use crate::summary_type::{
    SketchAlgorithm, SketchParameterRange, SketchParams, StatisticKind, SummaryFamily,
};

/// Physical candidates whose description matches every field that is set.
/// Example: `{"timing": "query_time", "operator": "Scan"}` excludes every
/// query-time scan of raw data. JSON: an object; an omitted field is
/// unset, an unknown one is refused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CandidateExclusion {
    /// Matches a candidate run at this timing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<ExecutionTiming>,
    /// `Some(true)` matches a candidate whose output is kept across
    /// evaluations (lifetime `AcrossEvaluations`), `Some(false)` one whose
    /// output is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_across_evaluations: Option<bool>,
    /// Matches a candidate whose operator is of this kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<OperatorKind>,
    /// Matches an operator with a `Reduction` (`Aggregate`, `SummaryAgg`,
    /// `SummaryTumblingWindowChain`): `Some(true)` per entity, `Some(false)`
    /// grouped. An operator without one never matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_entity_reduction: Option<bool>,
    /// Matches an `Aggregate` one of whose measures is of this kind, named
    /// as an `AggIntent`'s `kind` in JSON, e.g. `"rate"`: with `timing:
    /// query_time`, a query-time `rate` computed from raw rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_measure: Option<String>,
    /// Matches a candidate whose counter function needs this semantics:
    /// an `Aggregate` computing such a function, or a build of such a
    /// counter state (`Rate`, `Increase`, `IRate`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counter_semantics: Option<CounterSemantics>,
    /// Matches a state build of this family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_family: Option<SummaryFamily>,
    /// Matches a state build whose θ is in this range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sketch_parameters: Option<SketchParameterRange>,
    /// Matches a state build by how its state is used: `Some(true)` a
    /// state merged across windows (a tumbling-window chain, whose windows
    /// a `SummaryMerge` combines, or a build kept across evaluations),
    /// `Some(false)` a state built and finalized (or read) within one
    /// evaluation (a `SummaryAgg` not kept across evaluations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_across_windows: Option<bool>,
    /// Matches a `SummaryEstimate` reading this statistic from a state of
    /// this sketch algorithm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readout: Option<SketchReadout>,
}

/// What a counter function may need beyond reading its samples. JSON:
/// `"reset_handling"`, `"extrapolation"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterSemantics {
    /// A drop in a counter is a reset and is added back: `rate`,
    /// `increase`, `irate` and `resets`, and the `Rate`, `Increase` and
    /// `IRate` states.
    ResetHandling,
    /// The increase over the samples is extrapolated to the range's ends:
    /// `rate`, `increase` and `delta`, and the `Rate` and `Increase` states.
    Extrapolation,
}

/// A statistic read from a sketch algorithm's state, e.g. a quantile from
/// a KLL. JSON: `{"summary_type": "Kll", "statistic": "Quantile"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SketchReadout {
    pub summary_type: SketchAlgorithm,
    pub statistic: StatisticKind,
}

/// One physical candidate (or plan node) as an exclusion matches it: its
/// operator, θ and physical properties, and, for a `SummaryEstimate`, the
/// summary type of the state it reads.
#[derive(Debug, Clone, Copy)]
pub struct CandidateDescription<'a, C> {
    pub operator: &'a Operator<C>,
    /// θ: the candidate's `LogicalProperties::parameters`.
    pub parameters: Option<&'a SketchParams>,
    pub physical_properties: PhysicalProperties,
    /// For a `SummaryEstimate`: the summary type of the state it reads.
    /// `None` otherwise.
    pub read_summary_type: Option<&'a FieldDataType>,
}

impl<'a> CandidateDescription<'a, crate::ir::flat::NodeId> {
    /// `node` of `plan` as Stage 2 described its candidate: its payload,
    /// `parameters`, `output_state.timing` and `lifetime`, and for a
    /// `SummaryEstimate` the state field type of its input node's schema.
    pub fn of_plan_node(plan: &'a PhysicalASAPDAG, node: &'a PhysicalASAPDAGNode) -> Self {
        let _ = (plan, node);
        todo!("tests-only PR: CandidateDescription::of_plan_node")
    }
}

impl CandidateExclusion {
    /// Whether `candidate` matches every field that is set.
    pub fn matches<C>(&self, candidate: &CandidateDescription<'_, C>) -> bool {
        let _ = candidate;
        todo!("tests-only PR: CandidateExclusion::matches")
    }
}
