//! Stage 1 (logical ASAP-aware optimization), Pass 1: per-computation
//! candidate generation, as defined in
//! `docs/design_docs/proposals/planner-layering.md`.
//!
//! [`generate_per_computation_candidates`] takes the frontends' logical DAGs
//! (`QueryRoot`s) and returns, for every eligible computation, its unranked
//! candidates. A computation ([`ComputationTarget`]) is one measure of an
//! `Aggregate` whose intent has a realization besides plain exact execution;
//! nested aggregates and operators read by scalar roots are found too.
//! Pass 1 does not consider sharing across computations; that is Pass 2.
//!
//! - Each target has exactly one exact option: the exact accumulator
//!   (`ExactAccumulator`, or `AvgAsSumCount` for `avg`) when it can be built,
//!   otherwise plain `Exact`. Whether it is materialized is decided in Stage 2.
//! - Summary candidates are unsized; they are sized after Pass 2 sharing.
//! - Candidates are pruned only when provably unable to meet the accuracy
//!   requirement, and each pruned candidate keeps its [`PruneReason`].
//! - A measure filter or `HAVING` prunes nothing. The target records the
//!   measure filter (applied as `SummaryAgg.filter`) and one [`HavingEntry`]
//!   per consumer `Filter` directly above the aggregate: a key-only predicate
//!   applied by every consumer may go below the aggregate, anything else stays
//!   above the output (approximate outputs then get guarantee `None`).
//!
//! The input must be a logical DAG: ASAP operators, execution timing or a
//! materialization are rejected. No cost or accuracy model is used; ranking
//! and discarding valid candidates belong to Stage 3.
//!
//! # Whole-workload candidates (logical ASAP-aware optimization, end of Pass 1)
//!
//! [`compose_logical_candidate`] is Algorithm 1's
//! `SUBSTITUTE(d, s_1 ↦ c_1, …)`: it picks one per-computation candidate per
//! target (`choice[i]` indexes `targets[i].choices.candidates`) and builds one
//! whole-workload candidate DAG. An aggregate whose chosen candidates are all
//! `Exact` is kept. Otherwise each of its measures is substituted on its own
//! over the aggregate's input:
//!
//! - `ExactAccumulator`: `SummaryAgg` with the exact family →
//!   `FinalizeExactAccumulator`.
//! - `Summary`: `SummaryAgg` → `SummaryEstimate`.
//! - `AvgAsSumCount`: the finalized `sum` and `count` accumulators divided:
//!   a PromQL `BinaryOp`, or for SQL rows a join on the group keys (NULL keys
//!   match) and a SQL division, NULL where the count is 0.
//! - `Exact` (or a measure that is no target): a single-measure `Aggregate`.
//!
//! The substituted output keeps the aggregate's column names. The measures
//! of a multi-measure aggregate are joined back on the group keys (NULL keys
//! match, as grouping treats them). A measure filter becomes
//! `SummaryAgg.filter`, so a group with no matching row stays. A
//! `BelowAggregate` `HAVING` becomes a `Filter` below the state, with its key
//! columns renumbered to the input's, and the consumer `Filter` it came from
//! is dropped; it is not a `SummaryAgg.filter`, which would keep the dropped
//! groups. An `AboveOutput` `HAVING` stays a `Filter` above the output.
//!
//! Summaries are sized here, each for its own measure's accuracy requirement.
//! Sizing belongs after Pass 2, where a shared summary takes the strictest
//! requirement among its consumers; Pass 2 sharing is not implemented yet, so
//! this is that post-Pass-2 sizing step with no sharing.
//!
//! Every node of the candidate gets a theoretical guarantee: exact over
//! exact inputs; a `SummaryEstimate` over exact input the sized family's
//! bound where that bound answers the estimate (not for Top-K, where a heap
//! sketch bounds item frequencies, not membership; not for a Count-Min
//! family over weights not proven non-negative); and `None` otherwise and for
//! anything computed from an approximate value. Composing guarantees through
//! operators is #669; plan selection's accuracy model decides a candidate without
//! one.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator_properties::{BinaryOpKind, JoinKind, Reduction, Source};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, OperatorResultKind,
    Predicate, ProjectItem, QueryRoot, ScalarExpr, SchemaDerivationError,
};
use asap_types::post_asap::guarantee::ResultGuarantee;
use asap_types::post_asap::{
    EntityIdentity, ExactKind, ExactParams, FieldDataType, GroupingStrategy, Materialization,
    NonNegativeWeightProof, SketchAlgorithm, SketchKind, SketchStatistic, SummaryInputExpr,
    SummaryUpdate, WeightDomain,
};
use asap_types::pre_asap::expr_ir::ArithmeticOpKind;
use asap_types::pre_asap::{
    resolve_column_ref, AggIntent, ColumnRef, CompareOpKind, DataType, ScalarValue, Schema,
};
use asap_types::types::AccuracyTarget;
use asap_types::workload::MetricType;
use thiserror::Error;

use crate::accuracy::{AccuracyModel, DefaultAccuracyModel};
use crate::realization::{
    accuracy_budget, accuracy_target, default_size_params, summary_candidates,
};

/// One Pass 1 candidate for a computation (one measure of an aggregate).
///
/// Each target has exactly one exact option: `ExactAccumulator` or
/// `AvgAsSumCount` when the intent has one and it can be built, otherwise
/// `Exact`. The plain form would duplicate "accumulator, not materialized";
/// Stage 2/3 decide materialization and sharing.
#[derive(Debug, Clone, PartialEq)]
pub enum ComputationCandidate {
    /// Plain exact execution of the original sub-DAG; the exact option only
    /// when the intent has no exact accumulator form or that form is pruned.
    Exact,
    /// An exact mergeable accumulator state.
    ExactAccumulator {
        kind: ExactKind,
        params: ExactParams,
    },
    /// An unsized summary family, sized later for the measure's accuracy.
    Summary(SketchAlgorithm),
    /// `avg` rewritten as exact `sum` and `count` accumulator states.
    AvgAsSumCount,
    /// A whole-expression top-k: one heap summary (Count-Min or CountSketch
    /// with a heap) over the input of the per-item `sum` or `count` beneath
    /// the top-k, keyed by the ranked item and weighted by the summed value,
    /// instead of a summary over that aggregate's exact result. It absorbs
    /// the target beneath ([`ComputationTarget::absorbs`]): that aggregate
    /// is not computed. Unsized, like `Summary`.
    WholeExpression(SketchAlgorithm),
}

/// Why Pass 1 proved a candidate unable to realize its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneReason {
    /// The measure requires an exact answer and the summary is approximate.
    RequiresExact,
    /// `count` counts rows, so a possibly-null `avg` input changes the result.
    NullableAvgInput,
}

/// Unranked candidates of one measure and the candidates pruned for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputationChoices {
    pub candidates: Vec<ComputationCandidate>,
    pub pruned: Vec<(ComputationCandidate, PruneReason)>,
}

impl ComputationChoices {
    /// Prune matching candidates; a pruned exact accumulator form is replaced by
    /// plain `Exact`, so the target keeps one exact option.
    fn prune(&mut self, reason: PruneReason, applies: impl Fn(&ComputationCandidate) -> bool) {
        let (pruned, kept): (Vec<_>, _) = std::mem::take(&mut self.candidates)
            .into_iter()
            .partition(|choice| *choice != ComputationCandidate::Exact && applies(choice));
        self.candidates = kept;
        if pruned.iter().any(|choice| {
            matches!(
                choice,
                ComputationCandidate::ExactAccumulator { .. } | ComputationCandidate::AvgAsSumCount
            )
        }) {
            self.candidates.insert(0, ComputationCandidate::Exact);
        }
        self.pruned.extend(
            pruned
                .into_iter()
                .map(|choice: ComputationCandidate| (choice, reason)),
        );
    }
}

/// Where a target's `HAVING` is applied. Composition (#575) applies it.
///
/// Only the adjacent form is recorded for push-down: the aggregate's own
/// `having`, or a `Filter` directly above the aggregate (the form the SQL
/// frontend lowers `HAVING` to). A value predicate anywhere above an
/// approximate output gets `guarantee = None` through guarantee propagation
/// (#575); multi-level push-down is tracked in #664.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HavingPlacement {
    /// The predicate reads only group keys and every consumer of the
    /// aggregate applies it, so it is pushed below the aggregate as a row
    /// predicate (a `Filter` on its input, which drops the group).
    BelowAggregate,
    /// The predicate reads aggregate values, so it stays a `Filter` above the
    /// finalize/estimate output; approximate candidates of this target carry
    /// `guarantee = None`, since membership near the threshold may change.
    AboveOutput,
}

/// One `HAVING` over a target's aggregate output.
#[derive(Debug, Clone, PartialEq)]
pub struct HavingEntry {
    /// The consumer `Filter` that applies it; `None` for the aggregate's own
    /// `having`, which every consumer reads.
    pub filter: Option<Rc<OperatorNode>>,
    pub predicate: Predicate,
    pub placement: HavingPlacement,
}

/// One measure of an aggregate with at least one realization besides exact
/// execution. The target retains source, grouping, filters, input expressions
/// and evaluation context.
#[derive(Debug, Clone)]
pub struct ComputationTarget {
    pub target: Rc<OperatorNode>,
    /// Index into the aggregate's measures.
    pub measure: usize,
    pub choices: ComputationChoices,
    /// The measure's own filter; only rows that pass it update the state
    /// (`SummaryAgg.filter`).
    pub filter: Option<Predicate>,
    /// The aggregate's own `having`, then one entry per consumer `Filter`
    /// directly above the aggregate.
    pub having: Vec<HavingEntry>,
    /// The target directly beneath this one that its
    /// [`ComputationCandidate::WholeExpression`] candidates absorb. A
    /// whole-workload choice of such a candidate takes the absorbed target's
    /// first candidate: the target is not computed, so its own choice does
    /// not matter.
    pub absorbs: Option<usize>,
    /// Whether every value the aggregate reads is a sample of a metric
    /// declared a counter, or a sum of such samples ([`counter_samples`]):
    /// heap-sketch weights read from it are proven non-negative.
    pub counter_input: bool,
}

/// Compact Pass 1 inventory; roots and nested producer dependencies are retained.
#[derive(Debug, Clone)]
pub struct PerComputationCandidates<Id> {
    pub roots: Vec<(Id, QueryRoot)>,
    pub targets: Vec<ComputationTarget>,
}

#[derive(Debug, Error)]
pub enum PerComputationCandidateError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error("logical candidate input already has assigned execution timing or materialization")]
    AssignedPhysicalAnnotation,
    #[error("logical candidate input already contains ASAP operators")]
    ContainsAsap,
    /// `root` (the root id's `Debug` form) and `measure` locate the target;
    /// [`computation_candidates_for_intent`] alone leaves them `None`.
    #[error(
        "approximate accuracy of root {}, measure {} requires epsilon and delta in (0, 1)",
        root.as_deref().unwrap_or("?"),
        measure.map_or_else(|| "?".to_string(), |measure| measure.to_string())
    )]
    InvalidAccuracy {
        root: Option<String>,
        measure: Option<usize>,
    },
    #[error("choice must name one listed candidate per target")]
    InvalidChoice,
    #[error("unsupported substitution in logical ASAP-aware optimization: {0}")]
    Unsupported(&'static str),
}

/// Enumerate exact, rewrite and summary choices of one measure in stable catalog
/// order, without ranking or empirical assessment. A summary is not a claim that
/// a deployment meets the measure's accuracy requirement. The one exact option
/// comes first: the exact accumulator form if the intent has one, else `Exact`.
pub fn computation_candidates_for_intent(
    intent: &AggIntent,
) -> Result<ComputationChoices, PerComputationCandidateError> {
    let exact = match intent {
        AggIntent::Count { .. } => Some((ExactKind::Count, ExactParams::Count)),
        AggIntent::Sum { .. } => Some((ExactKind::Sum, ExactParams::Sum)),
        AggIntent::Min { .. } => Some((ExactKind::Min, ExactParams::Min)),
        AggIntent::Max { .. } => Some((ExactKind::Max, ExactParams::Max)),
        AggIntent::Rate => Some((ExactKind::Rate, ExactParams::Rate)),
        AggIntent::IRate => Some((ExactKind::IRate, ExactParams::IRate)),
        AggIntent::Increase => Some((ExactKind::Increase, ExactParams::Increase)),
        _ => None,
    };
    let exact = match exact {
        Some((kind, params)) => ComputationCandidate::ExactAccumulator { kind, params },
        None if matches!(intent, AggIntent::Avg { .. }) => ComputationCandidate::AvgAsSumCount,
        None => ComputationCandidate::Exact,
    };
    let mut choices = ComputationChoices {
        candidates: vec![exact],
        pruned: vec![],
    };
    if let Some(target) = accuracy_target(intent) {
        if *target != AccuracyTarget::Exact {
            let (epsilon, delta) = accuracy_budget(target);
            let unit = |value: f64| value > 0.0 && value < 1.0;
            if !unit(epsilon) || !unit(delta) {
                return Err(PerComputationCandidateError::InvalidAccuracy {
                    root: None,
                    measure: None,
                });
            }
        }
        let summaries = summary_candidates(intent).iter().cloned();
        if *target == AccuracyTarget::Exact {
            choices.pruned.extend(summaries.map(|algorithm| {
                (
                    ComputationCandidate::Summary(algorithm),
                    PruneReason::RequiresExact,
                )
            }));
        } else {
            choices
                .candidates
                .extend(summaries.map(ComputationCandidate::Summary));
        }
    }
    Ok(choices)
}

/// Whether an `avg` measure's input may be null, so `count` rows differ from
/// counted values. An unresolvable input is treated as possibly null.
fn nullable_avg_input(node: &OperatorNode, intent: &AggIntent) -> bool {
    let Some(NonASAPOp::Aggregate { child, .. }) = node.non_asap() else {
        return true;
    };
    let column = match intent.input_cols().first() {
        Some(column) => Some(*column),
        None => resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok(),
    };
    column
        .and_then(|column| child.schema.fields.get(column))
        .is_none_or(|field| field.nullable)
}

/// Whether `having` over `aggregate`'s output reads only `by(...)` group keys.
/// A global aggregate (no group keys) never qualifies: its one output row
/// exists even when no input row does, so no row predicate can stand in.
fn key_only_having(aggregate: &OperatorNode, having: &Predicate) -> bool {
    let keys = match aggregate.non_asap() {
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(by),
            ..
        }) if !by.is_without() => by.len(),
        _ => 0,
    };
    keys > 0
        && having.0.operator_refs().is_empty()
        && having.0.columns_referenced().iter().all(|id| *id < keys)
}

fn placement(below: bool) -> HavingPlacement {
    if below {
        HavingPlacement::BelowAggregate
    } else {
        HavingPlacement::AboveOutput
    }
}

/// Discover one target per measure of each eligible aggregate, including
/// operator plans read by scalar roots. An aggregate measure is eligible when it
/// has a realization besides exact execution, even if every such realization
/// is pruned; the prune reasons are reported on the target. Measure filters
/// and `HAVING` keep every candidate; the target records how they are applied.
/// A consumer `Filter`'s key-only `HAVING` is pushed below only when every
/// consumer of the aggregate applies the same predicate.
pub fn generate_per_computation_candidates<Id: std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
) -> Result<PerComputationCandidates<Id>, PerComputationCandidateError> {
    generate_per_computation_candidates_with(roots, &BTreeMap::new())
}

/// [`generate_per_computation_candidates`] with the data workload's declared
/// metric types, which prove heap-sketch weights over counter samples
/// non-negative.
pub fn generate_per_computation_candidates_with<Id: std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
    metric_types: &BTreeMap<String, MetricType>,
) -> Result<PerComputationCandidates<Id>, PerComputationCandidateError> {
    let mut seen = HashSet::new();
    // Each aggregate with the first root that reaches it.
    let mut aggregates = Vec::new();
    // Every consumer of each node: `Some(parent)`, or `None` for a root or
    // scalar reference.
    let mut consumers: HashMap<*const OperatorNode, Vec<Option<Rc<OperatorNode>>>> = HashMap::new();
    for (id, root) in &roots {
        root.validate_structure()?;
        let operators = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for root in operators {
            if root.contains_asap() {
                return Err(PerComputationCandidateError::ContainsAsap);
            }
            consumers.entry(Rc::as_ptr(root)).or_default().push(None);
            for node in OperatorNode::reachable(root) {
                if !seen.insert(Rc::as_ptr(&node)) {
                    continue;
                }
                if node.timing.is_some() || node.materialization != Materialization::NotMaterialized
                {
                    return Err(PerComputationCandidateError::AssignedPhysicalAnnotation);
                }
                for child in node.children() {
                    consumers
                        .entry(Rc::as_ptr(child))
                        .or_default()
                        .push(Some(Rc::clone(&node)));
                }
                if matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })) {
                    aggregates.push((format!("{id:?}"), node));
                }
            }
        }
    }
    let mut targets = Vec::new();
    for (root, node) in aggregates {
        let Some(NonASAPOp::Aggregate {
            measures,
            filters,
            having,
            ..
        }) = node.non_asap()
        else {
            continue;
        };
        let mut entries: Vec<HavingEntry> = having
            .iter()
            .map(|having| HavingEntry {
                filter: None,
                predicate: having.clone(),
                placement: placement(key_only_having(&node, having)),
            })
            .collect();
        let parents = &consumers[&Rc::as_ptr(&node)];
        // Only a `Filter` whose input is this aggregate is its `HAVING`; one
        // that reads it from a scalar expression stays a plain consumer.
        let mut filters_above: Vec<(&Rc<OperatorNode>, &Predicate)> = Vec::new();
        for parent in parents.iter().flatten() {
            if let Some(NonASAPOp::Filter { child, pred }) = parent.non_asap() {
                if Rc::ptr_eq(child, &node)
                    && !filters_above
                        .iter()
                        .any(|(filter, _)| Rc::ptr_eq(filter, parent))
                {
                    filters_above.push((parent, pred));
                }
            }
        }
        for (filter, predicate) in &filters_above {
            let shared_by_all = filters_above.len() == parents.len()
                && filters_above.iter().all(|(_, other)| other == predicate);
            entries.push(HavingEntry {
                filter: Some(Rc::clone(filter)),
                predicate: (*predicate).clone(),
                placement: placement(shared_by_all && key_only_having(&node, predicate)),
            });
        }
        for (measure, intent) in measures.iter().enumerate() {
            let mut choices =
                computation_candidates_for_intent(intent).map_err(|error| match error {
                    PerComputationCandidateError::InvalidAccuracy { .. } => {
                        PerComputationCandidateError::InvalidAccuracy {
                            root: Some(root.clone()),
                            measure: Some(measure),
                        }
                    }
                    error => error,
                })?;
            if choices.candidates == [ComputationCandidate::Exact] && choices.pruned.is_empty() {
                continue;
            }
            if matches!(intent, AggIntent::Avg { .. }) && nullable_avg_input(&node, intent) {
                choices.prune(PruneReason::NullableAvgInput, |choice| {
                    *choice == ComputationCandidate::AvgAsSumCount
                });
            }
            targets.push(ComputationTarget {
                target: node.clone(),
                measure,
                choices,
                filter: filters.get(measure).cloned().flatten(),
                having: entries.clone(),
                absorbs: None,
                counter_input: node
                    .children()
                    .iter()
                    .all(|child| counter_samples(child, metric_types)),
            });
        }
    }
    add_whole_expression_candidates(&mut targets, &consumers);
    Ok(PerComputationCandidates { roots, targets })
}

/// Whole-expression top-k (#509 Example 1's Q2): a top-k over a per-item
/// `sum` or `count` that is itself a target gets one
/// [`ComputationCandidate::WholeExpression`] per heap family its own
/// candidates list, absorbing the inner target. Offered only when the inner
/// aggregate has no other consumer, so absorbing it removes its work, and
/// when the keyed-additive rule realizes it ([`whole_expression_input`]).
fn add_whole_expression_candidates(
    targets: &mut [ComputationTarget],
    consumers: &HashMap<*const OperatorNode, Vec<Option<Rc<OperatorNode>>>>,
) {
    for t in 0..targets.len() {
        let target = &targets[t];
        let Some(NonASAPOp::Aggregate { child, .. }) = target.target.non_asap() else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| Rc::ptr_eq(&other.target, child))
        else {
            continue;
        };
        if consumers.get(&Rc::as_ptr(child)).map(Vec::len) != Some(1)
            || target.filter.is_some()
            || !target.having.is_empty()
            || whole_expression_input(&target.target).is_none()
        {
            continue;
        }
        let heaps: Vec<_> = target
            .choices
            .candidates
            .iter()
            .filter_map(|candidate| match candidate {
                ComputationCandidate::Summary(
                    algorithm @ (SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap),
                ) => Some(ComputationCandidate::WholeExpression(algorithm.clone())),
                _ => None,
            })
            .collect();
        if heaps.is_empty() {
            continue;
        }
        let target = &mut targets[t];
        target.choices.candidates.extend(heaps);
        target.absorbs = Some(inner);
    }
}

/// The input, update and partitions of a whole-expression top-k over
/// `target`'s inner aggregate, by the keyed-additive rule, or `None` when it
/// does not apply. Rows that carry the full series identity rank it as a
/// column, as [`summary_update`] does. The partitions are `target`'s, moved
/// onto the input the heap reads ([`partitions_over`]).
fn whole_expression_input(
    target: &OperatorNode,
) -> Option<(Rc<OperatorNode>, SummaryUpdate, Reduction)> {
    use crate::replacement::{
        realize_keyed_additive_summary_input, PhysicalSummaryInputRuleResult,
    };
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        having: None,
        ..
    }) = target.non_asap()
    else {
        return None;
    };
    let [intent @ AggIntent::TopK { .. }] = measures.as_slice() else {
        return None;
    };
    if asap_types::ir::non_asap::any_measure_filtered(filters) {
        return None;
    }
    // The rule reads only the family's algorithm, and rejects Count-Min over
    // signed weights. Ask with CountSketch so one update serves both heap
    // families; Stage 3 decides whether Count-Min is admissible, as it does
    // for every other Count-Min candidate.
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CountSketchWithHeap,
            asap_types::post_asap::SketchParams::CountSketchWithHeap {
                width: 1,
                depth: 1,
                heap_size: 1,
            },
        ),
        GroupingStrategy::default(),
    );
    let PhysicalSummaryInputRuleResult::Realized(realized) =
        realize_keyed_additive_summary_input(intent, &family, reduction, child)
    else {
        return None;
    };
    let mut input = realized.input;
    let per_series = matches!(
        child.non_asap(),
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            ..
        })
    );
    if per_series && realized.child.schema.has_promql_series_identity() {
        input.item = Some(SummaryInputExpr::Column(ColumnRef::Named(
            asap_types::pre_asap::schema::PROMQL_SERIES_IDENTITY.into(),
        )));
    } else if realized.child.schema.closed {
        // An encoded label set needs an open PromQL schema.
        return None;
    }
    let partitions = partitions_over(reduction, &child.schema, &realized.child.schema)?;
    Some((realized.child, input, partitions))
}

/// `reduction`'s keys, which index `inner` (the absorbed aggregate's
/// output), as indices into `input` (the rows the heap reads), matched by
/// column. `None` when a key is missing or ambiguous in `input`, or the keys
/// are given by `without`: `input` carries columns `inner` dropped.
fn partitions_over(reduction: &Reduction, inner: &Schema, input: &Schema) -> Option<Reduction> {
    let Reduction::Reduce(keys) = reduction else {
        return Some(Reduction::PerEntity);
    };
    if keys.is_without() {
        return None;
    }
    let column = |schema: &Schema, index: usize| {
        crate::realization::column_ref(&schema.fields[index])
    };
    let keys = keys
        .keys()
        .iter()
        .map(|&index| {
            inner.fields.get(index)?;
            let key = column(inner, index);
            let mut matches = (0..input.fields.len()).filter(|&i| column(input, i) == key);
            match (matches.next(), matches.next()) {
                (Some(i), None) => Some(i),
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    Some(Reduction::by(keys))
}

/// Whether every value `node` outputs is a sample of a metric declared a
/// counter, or a sum of such samples: never negative. Only a time-range
/// selection and a plain sum preserve that; any other operator (arithmetic
/// included) ends the proof. Read off the frontend DAG, this also holds for
/// the composed candidate, whose sums are exact accumulators.
fn counter_samples(node: &OperatorNode, metric_types: &BTreeMap<String, MetricType>) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            ..
        }) => metric_types.get(metric) == Some(&MetricType::Counter),
        Some(NonASAPOp::TimeRange { child, .. }) => counter_samples(child, metric_types),
        Some(NonASAPOp::Aggregate {
            measures, child, ..
        }) => {
            matches!(measures.as_slice(), [AggIntent::Sum { col: None }])
                && counter_samples(child, metric_types)
        }
        _ => false,
    }
}

/// `update` with its weights proven non-negative when they are the sample
/// values of counter input ([`ComputationTarget::counter_input`]).
fn with_counter_proof(mut update: SummaryUpdate, counter_input: bool) -> SummaryUpdate {
    if counter_input
        && update.item.is_some()
        && update.weight == SummaryInputExpr::Column(ColumnRef::SampleValue)
        && update.weight_domain == WeightDomain::UnknownOrSigned
    {
        update.weight_domain = WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::CounterSamples,
        };
    }
    update
}

/// Build one whole-workload candidate (end of Pass 1): `choice[i]` indexes
/// `inventory.targets[i].choices.candidates`. An aggregate with a chosen
/// candidate other than `Exact` is replaced measure by measure: an exact
/// accumulator by `SummaryAgg` → `FinalizeExactAccumulator`, a summary by
/// `SummaryAgg` → `SummaryEstimate`, `avg` by its `sum` and `count`
/// accumulators divided, and any other measure by its own single-measure
/// aggregate; several measures are joined back on the group keys. A summary
/// is sized here for its measure's accuracy requirement; Pass 1 leaves it
/// unsized. A measure filter becomes `SummaryAgg.filter`; `HAVING` is
/// applied as its target records. A sub-DAG shared between roots stays shared.
/// Every node carries the guarantee [`composed_guarantee`] gives it.
pub fn compose_logical_candidate<Id: Clone>(
    inventory: &PerComputationCandidates<Id>,
    choice: &[usize],
) -> Result<Vec<(Id, QueryRoot)>, PerComputationCandidateError> {
    if choice.len() != inventory.targets.len() {
        return Err(PerComputationCandidateError::InvalidChoice);
    }
    let mut chosen: Chosen = HashMap::new();
    for (target, &index) in inventory.targets.iter().zip(choice) {
        let candidate = target
            .choices
            .candidates
            .get(index)
            .ok_or(PerComputationCandidateError::InvalidChoice)?;
        chosen
            .entry(Rc::as_ptr(&target.target))
            .or_default()
            .push((target, candidate));
    }
    // Consumer `Filter`s whose key-only `HAVING` moves below a substituted
    // aggregate: each is replaced by its input.
    let pushed_below: HashSet<_> = chosen
        .values()
        .filter(|targets| substituted(targets))
        .flat_map(|targets| &targets[0].0.having)
        .filter(|entry| entry.placement == HavingPlacement::BelowAggregate)
        .filter_map(|entry| entry.filter.as_ref().map(Rc::as_ptr))
        .collect();
    let (mut memo, mut guaranteed) = (HashMap::new(), HashMap::new());
    let mut rewrite = |node: &Rc<OperatorNode>| {
        let node = rewrite(node, &chosen, &pushed_below, &mut memo)?;
        with_guarantees(&node, &mut guaranteed)
    };
    inventory
        .roots
        .iter()
        .map(|(id, root)| {
            let root = match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(rewrite(node)?),
                QueryRoot::Scalar(expr) => {
                    let mut rewritten = HashMap::new();
                    for node in expr.operator_refs() {
                        rewritten.insert(Rc::as_ptr(node), rewrite(node)?);
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| rewritten[&Rc::as_ptr(node)].clone()),
                    )
                }
            };
            // A substitution this module cannot express is an error, never an
            // invalid DAG.
            root.validate_structure()?;
            Ok((id.clone(), root))
        })
        .collect()
}

/// Each aggregate's targets with their chosen candidates.
type Chosen<'a> =
    HashMap<*const OperatorNode, Vec<(&'a ComputationTarget, &'a ComputationCandidate)>>;
type Memo = HashMap<*const OperatorNode, Rc<OperatorNode>>;

/// Whether any measure of the aggregate is substituted by something other than
/// its plain exact computation.
fn substituted(targets: &[(&ComputationTarget, &ComputationCandidate)]) -> bool {
    targets
        .iter()
        .any(|(_, candidate)| **candidate != ComputationCandidate::Exact)
}

fn rewrite(
    node: &Rc<OperatorNode>,
    chosen: &Chosen<'_>,
    pushed_below: &HashSet<*const OperatorNode>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        rewrite(child, chosen, pushed_below, memo)?;
    }
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &memo[&Rc::as_ptr(child)]));
    let rebuilt = match (chosen.get(&Rc::as_ptr(node)), node.non_asap()) {
        (Some(targets), _) if substituted(targets) => substitute(node, targets, memo)?,
        (_, Some(NonASAPOp::Filter { child, .. })) if pushed_below.contains(&Rc::as_ptr(node)) => {
            memo[&Rc::as_ptr(child)].clone()
        }
        _ if changed => Rc::new(node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?),
        _ => node.clone(),
    };
    memo.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// `node` and everything below it, each node with its [`composed_guarantee`].
fn with_guarantees(
    node: &Rc<OperatorNode>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        with_guarantees(child, memo)?;
    }
    let mut rebuilt = node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?;
    rebuilt.guarantee = composed_guarantee(&rebuilt);
    let rebuilt = Rc::new(rebuilt);
    memo.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// The theoretical guarantee of a composed node, given its children's: exact
/// over exact inputs; a summary estimate over exact input has its sized
/// family's bound when [`estimate_has_bound`]. Anything computed from an
/// approximate value (arithmetic, a comparison, a value `HAVING` or any other
/// predicate, at any depth) gets `None`: composition through operators is
/// not defined yet (#669), and plan selection's accuracy model decides. A
/// projection of columns only keeps its child's guarantee. A summary state
/// carries none itself.
fn composed_guarantee(node: &OperatorNode) -> Option<ResultGuarantee> {
    let exact = |node: &OperatorNode| {
        node.guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact)
    };
    // A summary state's input rows and any subquery in its filter.
    let exact_inputs = |state: &OperatorNode| state.children().into_iter().all(|c| exact(c));
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match child.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(..),
                ..
            }) if exact_inputs(child) => {
                Some(ResultGuarantee::exact("exact accumulator over exact input"))
            }
            _ => None,
        },
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) => match summary_input.asap() {
            Some(ASAPOp::SummaryAgg {
                family,
                input: update,
                ..
            }) if exact_inputs(summary_input) && estimate_has_bound(family, update, query) => {
                DefaultAccuracyModel.local_guarantee(family, query)
            }
            _ => None,
        },
        Operator::ASAP(_) => None,
        // Renaming or reordering columns keeps the child's guarantee.
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
            if cols
                .iter()
                .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
        {
            child.guarantee.clone()
        }
        Operator::NonASAP(_) => node
            .children()
            .into_iter()
            .all(|child| exact(child))
            .then(|| ResultGuarantee::exact("exact over exact inputs")),
    }
}

/// Whether the default accuracy model's bound for `family` answers `query`
/// over `update`. A heap sketch's bound is per item frequency, not Top-K
/// membership; a Count-Min bound assumes non-negative weights.
fn estimate_has_bound(
    family: &FieldDataType,
    update: &SummaryUpdate,
    query: &SketchStatistic,
) -> bool {
    let count_min = matches!(
        family,
        FieldDataType::Sketch(kind, _)
            if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap)
    );
    !matches!(query, SketchStatistic::TopK { .. })
        && !(count_min && update.weight_domain == WeightDomain::UnknownOrSigned)
}

/// Replace an aggregate whose measures are `targets`' (or exact, for a
/// measure that is no target) by its substitution, with the same output
/// columns.
fn substitute(
    target: &OperatorNode,
    targets: &[(&ComputationTarget, &ComputationCandidate)],
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        ..
    }) = target.non_asap()
    else {
        return Err(PerComputationCandidateError::Unsupported(
            "target is not an aggregate",
        ));
    };
    // A key-only `HAVING` is a row predicate over the input: rows of a
    // dropped group never reach the state.
    let having = &targets[0].0.having;
    // Operator refs in a predicate (a subquery) are this aggregate's
    // children, already rewritten.
    let rewritten = |pred: &Predicate| {
        Predicate(
            pred.0
                .map_operator_refs(&mut |node| memo[&Rc::as_ptr(node)].clone()),
        )
    };
    let mut input = memo[&Rc::as_ptr(child)].clone();
    for entry in having {
        if entry.placement == HavingPlacement::BelowAggregate {
            let keys = reduction.group_keys().map_or(&[][..], |by| by.keys());
            let pred = Predicate(entry.predicate.0.map_columns(&mut |column| keys[column]));
            input = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred,
                child: input,
            }))?;
        }
    }
    let parts = measures
        .iter()
        .enumerate()
        .map(|(measure, intent)| {
            let (candidate, counter_input) = targets
                .iter()
                .find(|(target, _)| target.measure == measure)
                .map_or((&ComputationCandidate::Exact, false), |(target, candidate)| {
                    (*candidate, target.counter_input)
                });
            let filter = filters.get(measure).and_then(Option::as_ref).map(rewritten);
            if let ComputationCandidate::WholeExpression(algorithm) = candidate {
                return whole_expression(target, intent, algorithm, counter_input, memo);
            }
            substitute_measure(target, intent, candidate, filter, &input, counter_input)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut output = match <[_; 1]>::try_from(parts) {
        Ok([part]) => with_names_of(target, part)?,
        Err(parts) => join_measures(target, parts)?,
    };
    for entry in having {
        if entry.placement == HavingPlacement::AboveOutput && entry.filter.is_none() {
            output = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred: rewritten(&entry.predicate),
                child: output,
            }))?;
        }
    }
    Ok(output)
}

/// `output` with `target`'s column names: an estimate or accumulator names
/// its value after the operation (`quantile`, `sum`), not the aggregate's
/// output column.
fn with_names_of(
    target: &OperatorNode,
    output: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let names = |node: &OperatorNode| -> Vec<String> {
        node.schema
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect()
    };
    if names(&output) == names(target) {
        return Ok(output);
    }
    if output.schema.fields.len() != target.schema.fields.len() {
        return Err(PerComputationCandidateError::Unsupported(
            "substituted output with other columns than the aggregate",
        ));
    }
    let cols = names(target)
        .into_iter()
        .enumerate()
        .map(|(column, name)| ProjectItem {
            alias: Some(name),
            expr: ScalarExpr::Column(column),
        })
        .collect();
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: output,
        },
    ))?)
}

/// A whole-expression top-k for `target` ([`whole_expression_input`]): one
/// heap summary of `algorithm` over the absorbed aggregate's rewritten
/// input, partitioned as `target`.
fn whole_expression(
    target: &OperatorNode,
    intent: &AggIntent,
    algorithm: &SketchAlgorithm,
    counter_input: bool,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (input, update, partitions) = whole_expression_input(target).ok_or(
        PerComputationCandidateError::Unsupported("whole-expression top-k input"),
    )?;
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: memo[&Rc::as_ptr(&input)].clone(),
        input: with_counter_proof(update, counter_input),
        family: FieldDataType::Sketch(sized(algorithm, intent), GroupingStrategy::default()),
        reduction: partitions,
        grouping: GroupingStrategy::default(),
        filter: None,
    }))?;
    Ok(OperatorNode::new_shared(Operator::ASAP(
        ASAPOp::SummaryEstimate {
            summary_input: state,
            query: statistic(intent)?,
        },
    ))?)
}

/// One measure of `target` over `input`: its group keys, then its value.
fn substitute_measure(
    target: &OperatorNode,
    intent: &AggIntent,
    candidate: &ComputationCandidate,
    filter: Option<Predicate>,
    input: &Rc<OperatorNode>,
    counter_input: bool,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate { reduction, .. }) = target.non_asap() else {
        unreachable!("substitute checked the aggregate")
    };
    let state = |family: FieldDataType, intent: &AggIntent, read: SummaryInputExpr| {
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::SummaryAgg {
                child: input.clone(),
                input: with_counter_proof(
                    summary_update(intent, &family, reduction, &input.schema, read)?,
                    counter_input,
                ),
                family,
                reduction: reduction.clone(),
                grouping: GroupingStrategy::default(),
                filter: filter.clone(),
            },
        ))?)
    };
    let finalized = |kind: ExactKind, params: ExactParams, intent: &AggIntent, read| {
        let state = state(FieldDataType::ExactAggregate(kind, params), intent, read)?;
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator { child: state },
        ))?)
    };
    Ok(match candidate {
        ComputationCandidate::Exact => {
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                child: input.clone(),
                reduction: reduction.clone(),
                measures: vec![intent.clone()],
                output_names: vec![],
                filters: vec![filter.clone()],
                having: None,
            }))?
        }
        ComputationCandidate::ExactAccumulator { kind, params } => {
            let read = measure_input(intent, input, filter.as_ref())?;
            finalized(kind.clone(), params.clone(), intent, read)?
        }
        ComputationCandidate::Summary(algorithm) => {
            let family =
                FieldDataType::Sketch(sized(algorithm, intent), GroupingStrategy::default());
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state(
                    family,
                    intent,
                    measure_input(intent, input, filter.as_ref())?,
                )?,
                query: statistic(intent)?,
            }))?
        }
        ComputationCandidate::WholeExpression(_) => {
            return Err(PerComputationCandidateError::Unsupported(
                "whole-expression top-k is substituted with its aggregate",
            ))
        }
        ComputationCandidate::AvgAsSumCount => {
            let AggIntent::Avg { col } = intent else {
                return Err(PerComputationCandidateError::Unsupported(
                    "sum/count of a non-avg",
                ));
            };
            let sum_intent = AggIntent::Sum { col: *col };
            // The count counts the averaged column, as `avg` does: it is
            // non-null (a nullable input prunes this candidate), so it counts
            // the rows the sum reads.
            let read = measure_input(&sum_intent, input, filter.as_ref())?;
            let mut sum = finalized(ExactKind::Sum, ExactParams::Sum, &sum_intent, read.clone())?;
            let count = finalized(
                ExactKind::Count,
                ExactParams::Count,
                &AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                },
                read,
            )?;
            if input.result_kind == OperatorResultKind::Relation {
                return sql_avg(reduction, filter.is_some(), sum, count);
            }
            // `avg` is Float64 whatever the summed type, as `Avg`'s output is.
            // A per-series sum keeps the input's labels, so its value is the
            // sample-value column; a grouped one ends with it.
            let value = match reduction {
                Reduction::PerEntity => resolve_column_ref(&ColumnRef::SampleValue, &sum.schema)
                    .ok()
                    .and_then(|column| sum.schema.fields.get(column)),
                Reduction::Reduce(_) => sum.schema.fields.last(),
            };
            if value.map(|field| &field.dtype) != Some(&FieldDataType::Plain(DataType::Float64)) {
                let Reduction::Reduce(by) = reduction else {
                    return Err(PerComputationCandidateError::Unsupported(
                        "non-Float64 per-series avg",
                    ));
                };
                let mut cols = key_items(by.len());
                cols.push(ProjectItem {
                    alias: None,
                    expr: ScalarExpr::Cast {
                        expr: Box::new(ScalarExpr::Column(by.len())),
                        to: DataType::Float64,
                        try_cast: false,
                    },
                });
                sum = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
                    cols,
                    qualifier: None,
                    child: sum,
                }))?;
            }
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    // A per-series window may hold no finite sum to divide.
                    checked_finite_division: matches!(reduction, Reduction::PerEntity),
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                    vector_match: None,
                },
                return_bool: false,
                lhs: sum,
                rhs: count,
            }))?
        }
    })
}

/// SQL `avg` from its finalized `sum` and `count`: joined on the group keys
/// (NULL keys match), then `sum / count` as Float64. A group with no row
/// passing the measure filter, or an empty global input, counts 0 and its
/// `avg` is NULL.
fn sql_avg(
    reduction: &Reduction,
    filtered: bool,
    sum: Rc<OperatorNode>,
    count: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Reduction::Reduce(by) = reduction else {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    };
    if by.is_without() {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    }
    let keys = by.len();
    let float = |column| {
        Box::new(ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(column)),
            to: DataType::Float64,
            try_cast: false,
        })
    };
    let count_column = keys + 1 + keys;
    let mut avg = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Div,
        left: float(keys),
        right: float(count_column),
        semantics: ExprSemantics::Sql,
    };
    if filtered || keys == 0 {
        avg = ScalarExpr::Case {
            operand: None,
            branches: vec![(
                ScalarExpr::Compare {
                    left: Box::new(ScalarExpr::Column(count_column)),
                    op: CompareOpKind::Eq,
                    right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
                    semantics: ExprSemantics::Sql,
                },
                ScalarExpr::Literal(ScalarValue::Null),
            )],
            else_expr: Some(Box::new(avg)),
        };
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: avg,
    });
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: join_on_keys(keys, sum, count)?,
        },
    ))?)
}

/// `left` joined with `right` on their leading `keys` columns; NULL keys
/// match each other, as grouping treats them.
fn join_on_keys(
    keys: usize,
    left: Rc<OperatorNode>,
    right: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let width = left.schema.fields.len();
    let column = |id| Box::new(ScalarExpr::Column(id));
    let matches = (0..keys)
        .map(|key| {
            let equal = ScalarExpr::Compare {
                left: column(key),
                op: CompareOpKind::Eq,
                right: column(width + key),
                semantics: ExprSemantics::Sql,
            };
            if right.schema.fields[key].nullable {
                ScalarExpr::BoolOr(vec![
                    equal,
                    ScalarExpr::BoolAnd(vec![
                        ScalarExpr::IsNull(column(key)),
                        ScalarExpr::IsNull(column(width + key)),
                    ]),
                ])
            } else {
                equal
            }
        })
        .collect::<Vec<_>>();
    let (kind, pred) = match matches.len() {
        0 => (
            JoinKind::Cross,
            ScalarExpr::Literal(ScalarValue::Boolean(true)),
        ),
        _ => (JoinKind::Inner, ScalarExpr::BoolAnd(matches)),
    };
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Join {
            kind,
            pred: Predicate(pred),
            left,
            right,
        },
    ))?)
}

/// Join single-measure `parts` (each: group keys, then one value) on their
/// group keys and project `target`'s output columns. Every part has the same
/// groups, so an inner join keeps them all; NULL keys match each other, as
/// grouping treats them.
fn join_measures(
    target: &OperatorNode,
    parts: Vec<Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        ..
    }) = target.non_asap()
    else {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    };
    if by.is_without() {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    }
    let keys = by.len();
    let width = keys + 1;
    let mut parts = parts.into_iter();
    let mut joined = parts.next().expect("an aggregate has measures");
    for part in parts {
        joined = join_on_keys(keys, joined, part)?;
    }
    let mut cols = key_items(keys);
    cols.extend(
        (0..target.schema.fields.len() - keys).map(|measure| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(measure * width + keys),
        }),
    );
    for (item, field) in cols.iter_mut().zip(&target.schema.fields) {
        item.alias = Some(field.name.clone());
    }
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: joined,
        },
    ))?)
}

fn key_items(keys: usize) -> Vec<ProjectItem> {
    (0..keys)
        .map(|key| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(key),
        })
        .collect()
}

/// `algorithm` sized for `intent`'s accuracy requirement.
fn sized(algorithm: &SketchAlgorithm, intent: &AggIntent) -> SketchKind {
    let (epsilon, delta) =
        accuracy_budget(accuracy_target(intent).unwrap_or(&AccuracyTarget::Exact));
    SketchKind::new(
        algorithm.clone(),
        default_size_params(algorithm.clone(), intent, epsilon, delta),
    )
}

/// What a measure's state reads from each row of `input`, resolvable there.
/// A SQL `count` reads [`counted_column`]; any other measure, a PromQL count
/// included, reads its input column (the sample value when it names none).
fn measure_input(
    intent: &AggIntent,
    input: &OperatorNode,
    filter: Option<&Predicate>,
) -> Result<SummaryInputExpr, PerComputationCandidateError> {
    let schema = &input.schema;
    let read = match intent {
        AggIntent::Count { .. } if input.result_kind == OperatorResultKind::Relation => {
            SummaryInputExpr::Column(counted_column(schema, filter)?)
        }
        _ => crate::realization::summarised_input(intent, schema).map_err(|_| {
            PerComputationCandidateError::Unsupported("input column outside child schema")
        })?,
    };
    if let SummaryInputExpr::Column(column) = &read {
        resolve_column_ref(column, schema).map_err(|_| {
            PerComputationCandidateError::Unsupported("summary input column does not resolve")
        })?;
    }
    Ok(read)
}

/// The column a SQL `count` state updates on. The runtime skips NULL updates,
/// so it is a column non-null on every counted row: one the measure filter
/// proves non-null (`count(col)` lowers to the filter `col IS NOT NULL`), else
/// a non-nullable one, Float64 first (the runtime's numeric update).
fn counted_column(
    schema: &Schema,
    filter: Option<&Predicate>,
) -> Result<ColumnRef, PerComputationCandidateError> {
    fn not_null(expr: &ScalarExpr) -> Vec<usize> {
        match expr {
            ScalarExpr::IsNotNull(inner) => match **inner {
                ScalarExpr::Column(column) => vec![column],
                _ => vec![],
            },
            ScalarExpr::BoolAnd(conjuncts) => conjuncts.iter().flat_map(not_null).collect(),
            _ => vec![],
        }
    }
    let non_nullable = |float: bool| {
        schema
            .fields
            .iter()
            .enumerate()
            .filter_map(move |(id, field)| {
                (!field.nullable
                    && (!float || field.dtype == FieldDataType::Plain(DataType::Float64)))
                .then_some(id)
            })
    };
    let reference = |id: usize| schema.fields.get(id).map(crate::realization::column_ref);
    filter
        .map_or_else(Vec::new, |filter| not_null(&filter.0))
        .into_iter()
        .chain(non_nullable(true))
        .chain(non_nullable(false))
        .find_map(|id| {
            reference(id).filter(|column| resolve_column_ref(column, schema).ok() == Some(id))
        })
        .ok_or(PerComputationCandidateError::Unsupported(
            "SQL count over rows with no column known to be non-null",
        ))
}

/// What each input row contributes, given what the measure reads
/// ([`measure_input`]), following the legacy realization rules: heap sketches
/// rank series identities, frequency sketches count values, and every other
/// family reads the measure's input.
fn summary_update(
    intent: &AggIntent,
    family: &FieldDataType,
    reduction: &Reduction,
    child: &Schema,
    weight: SummaryInputExpr,
) -> Result<SummaryUpdate, PerComputationCandidateError> {
    let algorithm = match family {
        FieldDataType::Sketch(kind, _) => Some(kind.algorithm()),
        _ => None,
    };
    Ok(match (intent, algorithm) {
        (AggIntent::TopK { .. }, Some(_)) => {
            if reduction.group_keys().is_some_and(|keys| keys.is_without()) {
                return Err(PerComputationCandidateError::Unsupported(
                    "Top-K partitions given by `without`",
                ));
            }
            let groups: Vec<usize> = reduction
                .group_keys()
                .into_iter()
                .flat_map(|keys| keys.iter().copied())
                .collect();
            let item = if child.has_promql_series_identity() {
                // Rows that carry the full series identity rank it as a
                // column, the item form the runtime builds keyed summaries
                // from.
                SummaryInputExpr::Column(ColumnRef::Named(
                    asap_types::pre_asap::schema::PROMQL_SERIES_IDENTITY.into(),
                ))
            } else if child.closed {
                // A closed PromQL child (an aggregate's output, say) lists
                // every label: the item is its labels outside the partition,
                // as the legacy current-series and counter rules rank them.
                // SQL rows carry no sample value to rank by.
                let value = resolve_column_ref(&ColumnRef::SampleValue, child).map_err(|_| {
                    PerComputationCandidateError::Unsupported(
                        "Top-K item identity for closed schemas without a sample value",
                    )
                })?;
                let items: Vec<_> = child
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != value && !groups.contains(index))
                    .map(|(_, field)| {
                        SummaryInputExpr::Column(crate::realization::column_ref(field))
                    })
                    .collect();
                if items.is_empty() {
                    return Err(PerComputationCandidateError::Unsupported(
                        "Top-K item has no label outside its partition",
                    ));
                }
                SummaryInputExpr::Tuple(items)
            } else {
                let excluding = groups
                    .iter()
                    .filter_map(|&index| child.fields.get(index))
                    .map(crate::realization::column_ref)
                    .collect();
                SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { excluding })
            };
            SummaryUpdate {
                item: Some(item),
                weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                // Not proven non-negative; selection decides whether CMS is admissible.
                weight_domain: WeightDomain::UnknownOrSigned,
            }
        }
        (_, Some(SketchAlgorithm::UnivMon))
        | (AggIntent::Count { .. }, Some(SketchAlgorithm::Cms | SketchAlgorithm::CountSketch)) => {
            SummaryUpdate {
                item: Some(weight),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            }
        }
        _ => SummaryUpdate {
            item: None,
            weight,
            weight_domain: WeightDomain::UnknownOrSigned,
        },
    })
}

fn statistic(intent: &AggIntent) -> Result<SketchStatistic, PerComputationCandidateError> {
    Ok(match intent {
        AggIntent::Quantile { q, .. } => SketchStatistic::Quantile { q: *q },
        AggIntent::Cardinality { .. } => SketchStatistic::Cardinality,
        AggIntent::FrequencyL2 { .. } => SketchStatistic::FrequencyL2,
        AggIntent::FrequencyEntropy { .. } => SketchStatistic::FrequencyEntropy,
        AggIntent::TopK { k, .. } => SketchStatistic::TopK { k: *k },
        AggIntent::Count { .. } => SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        _ => {
            return Err(PerComputationCandidateError::Unsupported(
                "sketch evaluation",
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Approximate count keeps one exact option, its accumulator, beside the
    /// universal summary; plain exact execution is not offered.
    #[test]
    fn approximate_count_keeps_one_exact_accumulator_and_universal_choices() {
        let choices = computation_candidates_for_intent(&AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.05,
                delta: 0.01,
            },
        })
        .unwrap()
        .candidates;
        assert!(!choices.contains(&ComputationCandidate::Exact));
        assert_eq!(
            choices[0],
            ComputationCandidate::ExactAccumulator {
                kind: ExactKind::Count,
                params: ExactParams::Count
            }
        );
        assert!(choices.contains(&ComputationCandidate::Summary(SketchAlgorithm::UnivMon)));
    }

    fn inventory_with(
        query: &str,
        metric_types: &BTreeMap<String, MetricType>,
    ) -> PerComputationCandidates<usize> {
        let root = crate::test_support::lower_promql(
            query,
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            },
        );
        // Per-series state ranks each series' identity, which the rows carry
        // as a column; a query over aggregated rows is planned over labels.
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root)
            .unwrap_or(root);
        generate_per_computation_candidates_with(vec![(0, QueryRoot::Operator(root))], metric_types)
            .unwrap()
    }

    /// Every choice that picks a whole-expression candidate, with the
    /// absorbed target at its first candidate, composed.
    fn whole_expression_roots(inventory: &PerComputationCandidates<usize>) -> Vec<Rc<OperatorNode>> {
        let mut roots = Vec::new();
        for (t, target) in inventory.targets.iter().enumerate() {
            for (c, candidate) in target.choices.candidates.iter().enumerate() {
                if let ComputationCandidate::WholeExpression(_) = candidate {
                    let mut choice = vec![0; inventory.targets.len()];
                    choice[t] = c;
                    let composed = compose_logical_candidate(inventory, &choice).unwrap();
                    let QueryRoot::Operator(root) = &composed[0].1 else {
                        panic!("operator root")
                    };
                    roots.push(root.clone());
                }
            }
        }
        roots
    }

    fn heap_states(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
        OperatorNode::reachable(root)
            .into_iter()
            .filter(|node| {
                matches!(node.asap(), Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _), ..
                }) if matches!(
                    kind.algorithm(),
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                ))
            })
            .collect()
    }

    /// A top-k over a per-series `sum_over_time` gets a whole-expression
    /// Count-Min and CountSketch heap that absorb the inner sum: composed,
    /// the inner aggregate is not computed and the heap reads raw samples.
    #[test]
    fn whole_expression_topk_absorbs_the_inner_sum() {
        let inventory = inventory_with("topk by (job) (10, sum_over_time(m[1m]))", &BTreeMap::new());
        let (topk, inner) = inventory
            .targets
            .iter()
            .enumerate()
            .find_map(|(t, target)| Some((t, target.absorbs?)))
            .expect("a whole-expression target");
        let whole: Vec<_> = inventory.targets[topk]
            .choices
            .candidates
            .iter()
            .filter(|c| matches!(c, ComputationCandidate::WholeExpression(_)))
            .collect();
        assert_eq!(
            whole,
            [
                &ComputationCandidate::WholeExpression(SketchAlgorithm::CmsWithHeap),
                &ComputationCandidate::WholeExpression(SketchAlgorithm::CountSketchWithHeap),
            ]
        );
        let roots = whole_expression_roots(&inventory);
        assert_eq!(roots.len(), 2);
        for root in &roots {
            assert!(
                !OperatorNode::reachable(root)
                    .iter()
                    .any(|n| matches!(n.non_asap(), Some(NonASAPOp::Aggregate { .. }))
                        || Rc::ptr_eq(n, &inventory.targets[inner].target)),
                "the inner sum is not computed"
            );
            let [state] = &heap_states(root)[..] else {
                panic!("one heap")
            };
            let Some(ASAPOp::SummaryAgg { child, .. }) = state.asap() else {
                unreachable!()
            };
            assert!(matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })));
        }
    }

    /// The whole-expression heap reads the inner sum's input, so the outer
    /// top-k's partition keys (indices into the sum's output) are moved onto
    /// that input by column.
    #[test]
    fn whole_expression_topk_partitions_index_the_heap_input() {
        let root = crate::test_support::lower_promql(
            "topk by (job) (2, sum by (service, job) (rate(m[1m])))",
            AccuracyTarget::Epsilon(0.1),
        );
        let inventory =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
        let roots = whole_expression_roots(&inventory);
        assert!(!roots.is_empty(), "a whole-expression heap");
        for root in &roots {
            for state in heap_states(root) {
                let Some(ASAPOp::SummaryAgg {
                    child, reduction, ..
                }) = state.asap()
                else {
                    unreachable!()
                };
                let names: Vec<_> = reduction
                    .group_keys()
                    .expect("partitioned by job")
                    .keys()
                    .iter()
                    .map(|&index| child.schema.fields[index].name.as_str())
                    .collect();
                assert_eq!(names, ["job"]);
            }
        }
    }

    /// The weight domain of every heap-sketch update in every candidate of
    /// `query` that picks a heap for its top-k.
    fn heap_weight_domains(query: &str, metric_types: &[(&str, MetricType)]) -> Vec<WeightDomain> {
        let metric_types = metric_types
            .iter()
            .map(|(metric, kind)| (metric.to_string(), *kind))
            .collect();
        let inventory = inventory_with(query, &metric_types);
        let mut domains = Vec::new();
        for (t, target) in inventory.targets.iter().enumerate() {
            for c in 0..target.choices.candidates.len() {
                let mut choice = vec![0; inventory.targets.len()];
                choice[t] = c;
                let composed = compose_logical_candidate(&inventory, &choice).unwrap();
                let QueryRoot::Operator(root) = &composed[0].1 else {
                    panic!("operator root")
                };
                for state in heap_states(root) {
                    let Some(ASAPOp::SummaryAgg { input, .. }) = state.asap() else {
                        unreachable!()
                    };
                    domains.push(input.weight_domain.clone());
                }
            }
        }
        domains
    }

    const COUNTER_PROOF: WeightDomain = WeightDomain::NonNegative {
        proof: NonNegativeWeightProof::CounterSamples,
    };

    /// Raw samples of a declared counter, and their `sum_over_time`, are
    /// proven non-negative: the whole-expression heaps read the samples, the
    /// others read the sums.
    #[test]
    fn declared_counter_samples_prove_heap_weights_non_negative() {
        let domains = heap_weight_domains(
            "topk by (job) (10, sum_over_time(m[1m]))",
            &[("m", MetricType::Counter)],
        );
        // Count-Min and CountSketch, over the sums and whole-expression.
        assert_eq!(domains.len(), 4);
        assert!(domains.iter().all(|d| *d == COUNTER_PROOF), "{domains:?}");
    }

    /// No proof without a counter declaration: an undeclared metric (whatever
    /// its name), a gauge, or another metric declared a counter.
    #[test]
    fn undeclared_or_gauge_samples_are_not_proven_non_negative() {
        for (query, metric_types) in [
            ("topk by (job) (10, sum_over_time(m_total[1m]))", vec![]),
            (
                "topk by (job) (10, sum_over_time(m[1m]))",
                vec![("m", MetricType::Gauge)],
            ),
            (
                "topk by (job) (10, sum_over_time(m[1m]))",
                vec![("other", MetricType::Counter)],
            ),
        ] {
            let domains = heap_weight_domains(query, &metric_types);
            assert_eq!(domains.len(), 4, "{query}");
            assert!(
                domains.iter().all(|d| *d == WeightDomain::UnknownOrSigned),
                "{query} {metric_types:?}: {domains:?}"
            );
        }
    }

    /// Counter samples and their sums are proven; arithmetic over them,
    /// which can go negative, and a gauge are not.
    #[test]
    fn counter_samples_end_at_arithmetic() {
        let metric_types = BTreeMap::from([
            ("m".to_string(), MetricType::Counter),
            ("g".to_string(), MetricType::Gauge),
        ]);
        for (query, proven) in [
            ("sum_over_time(m[1m])", true),
            ("sum by (job) (sum_over_time(m[1m]))", true),
            ("sum_over_time(m[1m]) - 100", false),
            ("-sum_over_time(m[1m])", false),
            ("sum_over_time(g[1m])", false),
        ] {
            let root = crate::test_support::lower_promql(query, AccuracyTarget::Exact);
            assert_eq!(counter_samples(&root, &metric_types), proven, "{query}");
        }
    }
}
