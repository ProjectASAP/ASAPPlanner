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
/// summary type of the state it reads. `C` is how the operator names its
/// inputs: memo groups in Stage 2, plan node ids in a selected plan.
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

/// The state builds: the operators the family, θ and
/// `merged_across_windows` fields match.
const STATE_BUILDS: &[OperatorKind] = &[
    OperatorKind::SummaryAgg,
    OperatorKind::SummaryTumblingWindowChain,
];

/// An `Aggregate` or a state build: the operators with a reduction
/// (`per_entity_reduction`), and those a counter semantics can be needed by
/// (an `Aggregate`'s counter function, a counter state's build).
const AGGREGATE_OR_STATE_BUILD: &[OperatorKind] = &[
    OperatorKind::Aggregate,
    OperatorKind::SummaryAgg,
    OperatorKind::SummaryTumblingWindowChain,
];

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

    /// The family this exclusion rules out outright, when it sets
    /// `summary_family` and no other field: every state of the family, as
    /// `supported_summaries` does for an algorithm it does not list. Pass 1
    /// offers no way to compute through such a family. `None` for any
    /// narrower exclusion, e.g. KLL only when merged across windows.
    pub fn family_excluded_outright(&self) -> Option<SummaryFamily> {
        let family = self.summary_family?;
        let family_only = Self {
            summary_family: Some(family),
            ..Self::default()
        };
        (*self == family_only).then_some(family)
    }

    /// Why this exclusion is bad input, or `None` when it can be applied:
    /// it sets no field (so it would exclude every candidate), two of its
    /// fields match no operator in common (`{"operator": "SummaryMerge",
    /// "summary_family": {"exact": "Rate"}}` matches nothing, since a
    /// family matches state builds only), its θ range is invalid
    /// ([`SketchParameterRange::invalid_reason`]) or ranges over another
    /// algorithm than its `summary_family` (a KLL `k` range of a DDSketch
    /// family matches nothing), or its `aggregate_measure` is no measure's
    /// kind (`"rte"`).
    pub fn invalid_reason(&self) -> Option<String> {
        if *self == Self::default() {
            return Some("it sets no field, so it would exclude every candidate".to_string());
        }
        let fields = self.operators_each_field_matches();
        for (index, (first, first_operators)) in fields.iter().enumerate() {
            for (second, second_operators) in &fields[index + 1..] {
                if !first_operators
                    .iter()
                    .any(|operator| second_operators.contains(operator))
                {
                    return Some(format!(
                        "its {first} matches {first_operators:?} only and its {second} \
                         {second_operators:?} only, so it matches no candidate"
                    ));
                }
            }
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

    /// Each field that is set and matches only some operators, by its JSON
    /// name, with those operators, as [`Self::matches`] tests them. E.g.
    /// `{"operator": "SummaryMerge", "summary_family": {"exact": "Rate"}}`
    /// gives `[("operator", [SummaryMerge]), ("summary_family",
    /// [SummaryAgg, SummaryTumblingWindowChain])]`.
    fn operators_each_field_matches(&self) -> Vec<(&'static str, &[OperatorKind])> {
        let set = |field_is_set: bool, operators| field_is_set.then_some(operators);
        [
            ("operator", self.operator.as_ref().map(std::slice::from_ref)),
            (
                "per_entity_reduction",
                set(
                    self.per_entity_reduction.is_some(),
                    AGGREGATE_OR_STATE_BUILD,
                ),
            ),
            (
                "aggregate_measure",
                set(
                    self.aggregate_measure.is_some(),
                    &[OperatorKind::Aggregate][..],
                ),
            ),
            (
                "counter_semantics",
                set(self.counter_semantics.is_some(), AGGREGATE_OR_STATE_BUILD),
            ),
            (
                "summary_family",
                set(self.summary_family.is_some(), STATE_BUILDS),
            ),
            (
                "sketch_parameters",
                set(self.sketch_parameters.is_some(), STATE_BUILDS),
            ),
            (
                "merged_across_windows",
                set(self.merged_across_windows.is_some(), STATE_BUILDS),
            ),
            (
                "readout",
                set(self.readout.is_some(), &[OperatorKind::SummaryEstimate][..]),
            ),
        ]
        .into_iter()
        .filter_map(|(field, operators)| Some((field, operators?)))
        .collect()
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
