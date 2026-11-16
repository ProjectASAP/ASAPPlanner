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

use crate::ir::operator::arguments::Reduction;
use crate::ir::operator::asap::summary_type_of_operator;
use crate::ir::operator::{AggIntent, OperatorKind};
use crate::ir::properties::ExecutionTiming;
use crate::ir::schema::FieldDataType;
use crate::ir::{ASAPOp, NonASAPOp, Operator};
use crate::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPDAGNode};
use crate::stages::PhysicalProperties;
use crate::summary_type::{
    ExactKind, SketchAlgorithm, SketchParameterRange, SketchParams, StatisticKind, SummaryFamily,
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
        let read_summary_type = match &node.payload {
            Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => plan
                .nodes
                .iter()
                .find(|state| state.id == *summary_input)
                .and_then(|state| state.output_schema.summary_state_type()),
            _ => None,
        };
        Self {
            operator: &node.payload,
            parameters: node.parameters.as_ref(),
            physical_properties: PhysicalProperties::new(node.output_state.timing, node.lifetime)
                .expect("a plan node's timing and lifetime go together"),
            read_summary_type,
        }
    }
}

impl CandidateExclusion {
    /// Whether `candidate` matches every field that is set.
    pub fn matches<C>(&self, candidate: &CandidateDescription<'_, C>) -> bool {
        let operator = candidate.operator;
        let properties = candidate.physical_properties;
        let build = summary_type_of_operator(operator);
        // Each field that is set, as a test of `candidate`; an unset field
        // passes.
        let passes = |field: Option<bool>| field.is_none_or(|matched| matched);
        passes(self.timing.map(|timing| properties.timing() == timing))
            && passes(
                self.kept_across_evaluations
                    .map(|kept| properties.is_across_evaluations() == kept),
            )
            && passes(self.operator.map(|kind| OperatorKind::of(operator) == kind))
            && passes(self.per_entity_reduction.map(|per_entity| {
                reduction_of(operator)
                    .is_some_and(|reduction| (*reduction == Reduction::PerEntity) == per_entity)
            }))
            && passes(self.aggregate_measure.as_deref().map(|name| {
                aggregate_measures(operator).any(|measure| measure.kind_name() == name)
            }))
            && passes(self.counter_semantics.map(|semantics| {
                aggregate_measures(operator).any(|measure| semantics.needed_by_measure(measure))
                    || build.is_some_and(|summary_type| semantics.needed_by_state(summary_type))
            }))
            && passes(self.summary_family.map(|family| {
                build.is_some_and(|summary_type| SummaryFamily::of(summary_type) == Some(family))
            }))
            && passes(self.sketch_parameters.map(|range| {
                build.is_some()
                    && candidate
                        .parameters
                        .is_some_and(|parameters| range.contains(parameters))
            }))
            && passes(self.merged_across_windows.map(|merged| {
                build.is_some() && merged_across_windows(operator, properties) == merged
            }))
            && passes(self.readout.map(|readout| match operator {
                Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                    query.kind() == readout.statistic
                        && candidate.read_summary_type
                            == Some(&FieldDataType::Sketch(readout.summary_type))
                }
                _ => false,
            }))
    }

    /// Why this exclusion is bad input, or `None` when it can be applied:
    /// it sets no field (so it would exclude every candidate), its θ range
    /// is invalid ([`SketchParameterRange::invalid_reason`]) or ranges over
    /// another algorithm than its `summary_family` (a KLL `k` range of a
    /// DDSketch family matches nothing), or its `aggregate_measure` is no
    /// measure's kind (`"rte"`).
    pub fn invalid_reason(&self) -> Option<String> {
        if *self == Self::default() {
            return Some("it sets no field, so it would exclude every candidate".to_string());
        }
        if let Some(range) = &self.sketch_parameters {
            if let Some(reason) = range.invalid_reason() {
                return Some(reason);
            }
            if let Some(family) = self.summary_family {
                if family != SummaryFamily::Sketch(range.algorithm()) {
                    return Some(format!(
                        "its sketch_parameters range over {:?}, which is not its summary_family {family:?}",
                        range.algorithm()
                    ));
                }
            }
        }
        if let Some(name) = &self.aggregate_measure {
            if !AggIntent::is_kind_name(name) {
                return Some(format!("aggregate_measure {name:?} is no measure's kind"));
            }
        }
        None
    }
}

impl CounterSemantics {
    /// Whether the counter function `measure` needs this semantics:
    /// extrapolation `rate`, `increase` and `delta`; reset handling
    /// `rate`, `increase`, `irate` and `resets`.
    fn needed_by_measure(self, measure: &AggIntent) -> bool {
        match self {
            Self::Extrapolation => {
                matches!(
                    measure,
                    AggIntent::Rate | AggIntent::Increase | AggIntent::Delta
                )
            }
            Self::ResetHandling => matches!(
                measure,
                AggIntent::Rate | AggIntent::Increase | AggIntent::IRate | AggIntent::Resets
            ),
        }
    }

    /// Whether a state of `summary_type` needs this semantics: the `Rate`
    /// and `Increase` states both, the `IRate` state reset handling only.
    fn needed_by_state(self, summary_type: &FieldDataType) -> bool {
        let FieldDataType::ExactAggregate(kind, _) = summary_type else {
            return false;
        };
        match self {
            Self::Extrapolation => matches!(kind, ExactKind::Rate | ExactKind::Increase),
            Self::ResetHandling => kind.tracks_counter_increase(),
        }
    }
}

/// The reduction of an operator that has one: `Aggregate`, `SummaryAgg`,
/// `SummaryTumblingWindowChain`.
fn reduction_of<C>(operator: &Operator<C>) -> Option<&Reduction> {
    match operator {
        Operator::NonASAP(NonASAPOp::Aggregate { reduction, .. })
        | Operator::ASAP(
            ASAPOp::SummaryAgg { reduction, .. }
            | ASAPOp::SummaryTumblingWindowChain { reduction, .. },
        ) => Some(reduction),
        _ => None,
    }
}

/// The measures of an `Aggregate`; none for any other operator.
fn aggregate_measures<C>(operator: &Operator<C>) -> impl Iterator<Item = &AggIntent> {
    let measures: &[AggIntent] = match operator {
        Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => measures,
        _ => &[],
    };
    measures.iter()
}

/// Whether a state build is merged across windows: a tumbling-window chain
/// (a `SummaryMerge` combines its windows), or a build kept across
/// evaluations (each evaluation merges in the rows that arrived since).
fn merged_across_windows<C>(operator: &Operator<C>, properties: PhysicalProperties) -> bool {
    matches!(
        operator,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
    ) || properties.is_across_evaluations()
}
