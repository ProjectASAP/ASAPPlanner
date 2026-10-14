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
//! Every node of the candidate gets a theoretical guarantee, composed with
//! the built-in accuracy model through operators (#669): exact over exact
//! inputs; a `SummaryEstimate` the sized family's bound where that bound
//! answers the estimate (not for Top-K, where a heap sketch bounds item
//! frequencies, not membership; not for a Count-Min family over weights not
//! proven non-negative); aggregation and arithmetic over approximate values
//! by their composition rules, with statistics left symbolic for plan
//! selection to bind; and `None` where no rule applies (comparisons and
//! value predicates, rank errors). Plan selection's accuracy model decides
//! a candidate without one.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator_properties::{BinaryOpKind, JoinKind, Reduction};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, OperatorResultKind,
    Predicate, ProjectItem, QueryRoot, ScalarExpr, SchemaDerivationError,
};
use asap_types::post_asap::guarantee::{
    BoundExpr, CompositionOperator, ErrorMetric, GuaranteeSource, ProbabilityExpr, ResultGuarantee,
};
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
use thiserror::Error;

use crate::accuracy::composition::{additive_as_absolute, DIVISION_OPERAND_DOMAINS};
use crate::accuracy::{AccuracyModel, DefaultAccuracyModel, PropagationStats};
use crate::function_rules::function_rules;
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
            });
        }
    }
    Ok(PerComputationCandidates { roots, targets })
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

/// The theoretical guarantee of a composed node, given its children's,
/// composed with the built-in accuracy model ([`DefaultAccuracyModel`]);
/// Stage 1 reads no data statistics or deployment evidence. Exact over exact
/// inputs; a summary estimate has its sized family's bound when
/// [`estimate_has_bound`], composed with its input's when that is
/// approximate; an exact aggregation or accumulator over approximate values
/// uses its function's rule ([`function_rules`]); arithmetic composes as
/// [`arithmetic_guarantee`]. A rule that needs a statistic leaves it
/// symbolic (`BoundExpr::Unknown`), and plan selection (Stage 3) binds it.
/// `None` where no rule applies: a comparison, a value `HAVING` or any other
/// predicate over an approximate value, a rank error through any operator (a
/// rank interval counts as none in the MVP). A projection of columns only
/// keeps its child's guarantee. A summary state carries none itself.
fn composed_guarantee(node: &OperatorNode) -> Option<ResultGuarantee> {
    let exact = |node: &OperatorNode| {
        node.guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact)
    };
    // A summary state's input rows and any subquery in its filter.
    let exact_inputs = |state: &OperatorNode| state.children().into_iter().all(|c| exact(c));
    // Over approximate input rows, a filter's subqueries must still be exact.
    let exact_filter =
        |state: &OperatorNode| state.children().into_iter().skip(1).all(|c| exact(c));
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match child.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(..),
                ..
            }) if exact_inputs(child) => {
                Some(ResultGuarantee::exact("exact accumulator over exact input"))
            }
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(kind, _),
                child: input,
                ..
            }) if exact_filter(child) => {
                let input = input.guarantee.clone()?;
                let op = match kind {
                    // Counts rows, not values.
                    ExactKind::Count => return Some(ResultGuarantee::exact("count of rows")),
                    ExactKind::Sum => CompositionOperator::ExactSum,
                    ExactKind::Min | ExactKind::Max => CompositionOperator::ExactExtremum,
                    ExactKind::Rate => CompositionOperator::CounterRate,
                    ExactKind::IRate => CompositionOperator::InstantCounterRate,
                    ExactKind::Increase => CompositionOperator::CounterIncrease,
                };
                compose(&op, vec![input], None)
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
                child: input,
                ..
            }) if estimate_has_bound(family, update, query) => {
                let local = DefaultAccuracyModel.local_guarantee(family, query)?;
                if exact_inputs(summary_input) {
                    Some(local)
                } else if exact_filter(summary_input) {
                    compose(
                        &CompositionOperator::ApproximateAggregate,
                        vec![input.guarantee.clone()?],
                        Some(&local),
                    )
                } else {
                    None
                }
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
        Operator::NonASAP(_) if node.children().into_iter().all(|child| exact(child)) => {
            Some(ResultGuarantee::exact("exact over exact inputs"))
        }
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) => project_guarantee(cols, child),
        // A measure filter or `HAVING` over approximate values changes
        // which rows or groups count: no rule.
        Operator::NonASAP(NonASAPOp::Aggregate {
            measures,
            filters,
            having: None,
            child,
            ..
        }) if filters.iter().all(Option::is_none) => match measures.as_slice() {
            [intent] => compose(
                &function_rules(intent)?.accuracy,
                vec![child.guarantee.clone()?],
                None,
            ),
            _ => None,
        },
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator:
                BinaryOperator {
                    kind: BinaryOpKind::Arithmetic(op),
                    ..
                },
            lhs,
            rhs,
            ..
        }) => arithmetic_guarantee(
            op.clone(),
            (lhs.guarantee.clone(), None),
            (rhs.guarantee.clone(), None),
            true,
            Rc::ptr_eq(lhs, rhs),
        ),
        Operator::NonASAP(_) => None,
    }
}

/// The built-in rule for `op`, with every statistic unknown (Stage 1 reads
/// none); `None` when no rule applies.
fn compose(
    op: &CompositionOperator,
    inputs: Vec<ResultGuarantee>,
    local: Option<&ResultGuarantee>,
) -> Option<ResultGuarantee> {
    DefaultAccuracyModel
        .propagate(op, &inputs, local, &PropagationStats::default())
        .ok()
}

/// A projection that computes over its child's sample value: the guarantee
/// of the one item that reads it. Other columns (labels, timestamps) are
/// exact. `None` when no single item reads the value.
fn project_guarantee(cols: &[ProjectItem], child: &OperatorNode) -> Option<ResultGuarantee> {
    let value = resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok()?;
    let reads_value = |expr: &ScalarExpr| {
        let mut reads = false;
        expr.map_columns(&mut |column| {
            reads |= column == value;
            column
        });
        reads
    };
    let [item] = <[_; 1]>::try_from(
        cols.iter()
            .filter(|item| reads_value(&item.expr))
            .collect::<Vec<_>>(),
    )
    .ok()?;
    let column = |column| {
        if column == value {
            child.guarantee.clone()
        } else {
            Some(ResultGuarantee::exact("column other than the sample value"))
        }
    };
    scalar_guarantee(&item.expr, &column)
}

/// The guarantee of a scalar expression, given each column's: `scalar(v)`
/// and a SQL scalar subquery have their relation's, a literal or the
/// evaluation time is exact, arithmetic composes as
/// [`arithmetic_guarantee`], and `clamp`/`clamp_min`/`clamp_max`/`abs` as
/// [`lipschitz_guarantee`]. Any other expression (a comparison, a rounding,
/// a non-linear function) is exact over exact operands and has none
/// otherwise.
pub(crate) fn scalar_guarantee(
    expr: &ScalarExpr,
    column: &dyn Fn(usize) -> Option<ResultGuarantee>,
) -> Option<ResultGuarantee> {
    let literal = |expr: &ScalarExpr| match expr {
        ScalarExpr::Literal(ScalarValue::Float64(value)) => Some(*value),
        ScalarExpr::Literal(ScalarValue::Int64(value)) => Some(*value as f64),
        _ => None,
    };
    let operand = |expr: &ScalarExpr| (scalar_guarantee(expr, column), literal(expr));
    match expr {
        ScalarExpr::Column(id) => column(*id),
        ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
            node.guarantee.clone()
        }
        ScalarExpr::Literal(_) | ScalarExpr::CurrentTimestamp | ScalarExpr::EvalTimestamp => {
            Some(ResultGuarantee::exact("constant"))
        }
        ScalarExpr::Negative { expr, .. } => {
            lipschitz_guarantee(vec![scalar_guarantee(expr, column)?])
        }
        ScalarExpr::Arithmetic {
            op, left, right, ..
        } => arithmetic_guarantee(
            op.clone(),
            operand(left),
            operand(right),
            false,
            same_value(left, right),
        ),
        ScalarExpr::FunctionCall { name, args } if is_monotone_lipschitz(name) => {
            lipschitz_guarantee(
                args.iter()
                    .map(|arg| scalar_guarantee(arg, column))
                    .collect::<Option<_>>()?,
            )
        }
        _ => {
            let mut columns = Vec::new();
            expr.map_columns(&mut |id| {
                columns.push(id);
                id
            });
            let exact = |g: Option<ResultGuarantee>| g.is_some_and(|g| g.is_exact());
            (columns.into_iter().all(|id| exact(column(id)))
                && expr
                    .operator_refs()
                    .into_iter()
                    .all(|node| exact(node.guarantee.clone())))
            .then(|| ResultGuarantee::exact("exact over exact operands"))
        }
    }
}

/// Whether two scalar operands are one value: the same column, or the same
/// planned operator read twice.
fn same_value(left: &ScalarExpr, right: &ScalarExpr) -> bool {
    match (left, right) {
        (ScalarExpr::Column(a), ScalarExpr::Column(b)) => a == b,
        (
            ScalarExpr::PromqlScalarFromVector(a) | ScalarExpr::ScalarSubquery(a),
            ScalarExpr::PromqlScalarFromVector(b) | ScalarExpr::ScalarSubquery(b),
        ) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

/// Functions that are 1-Lipschitz and, for the relative-error rule,
/// monotone in each argument with `|f(x)|` scaling with `|x|`: `clamp`
/// (`min(max(x, lo), hi)`), `clamp_min`, `clamp_max` and `abs`.
fn is_monotone_lipschitz(name: &str) -> bool {
    matches!(
        name,
        "promql_clamp" | "promql_clamp_min" | "promql_clamp_max" | "promql_abs" | "abs"
    )
}

/// The guarantee of a 1-Lipschitz function of `args` (negation, `abs`,
/// `clamp`): its value is within the largest of its arguments' absolute
/// errors, by the union bound over them; likewise for relative errors, as
/// for an exact `max`/`min` (`ExactExtremum`, one value per argument).
/// Count-Min and Count Sketch bounds count as absolute errors. `None` for a
/// rank error or a mix of absolute and relative ones.
fn lipschitz_guarantee(args: Vec<ResultGuarantee>) -> Option<ResultGuarantee> {
    let approximate: Vec<_> = args
        .into_iter()
        .filter(|g| !g.is_exact())
        .map(additive_as_absolute)
        .collect();
    if approximate.is_empty() {
        return Some(ResultGuarantee::exact(
            "Lipschitz function of exact operands",
        ));
    }
    DefaultAccuracyModel
        .propagate(
            &CompositionOperator::ExactExtremum,
            &approximate,
            None,
            &PropagationStats {
                input_row_count: Some(1),
                ..PropagationStats::default()
            },
        )
        .ok()
}

/// An arithmetic operand: its guarantee, and its value when it is a literal.
type Operand = (Option<ResultGuarantee>, Option<f64>);

/// The guarantee of `left op right` (#669, MVP). Count-Min and Count Sketch
/// bounds count as absolute errors (normalized bound times stream norm).
/// - `x ± y`: absolute errors add (union bound on the failure
///   probabilities); `x ± c` (`c` exact) keeps an absolute error.
/// - `c · x`, `x / c` (`c` a nonzero literal): an absolute error scales by
///   `|c|` (or `1/|c|`); a relative error is kept, as it is by `x · y` or
///   `x / y` with `y` exact.
/// - `x / y` with `x` or `y` approximate (`ratio`, a vector division): a
///   relative error whose bound needs enforced operand domains, left
///   symbolic ([`DIVISION_OPERAND_DOMAINS`]) for plan selection to bind.
/// - Everything else (a product or scalar quotient of two estimates, a
///   relative error shifted by a constant or added to another, a rank
///   error, `%`, `^`, `atan2`) has none.
///
/// `same_operand`: both sides are one estimate (the same planned operator),
/// which fails or holds as one event, so its failure probability counts
/// once. Two estimates count once each, even over one shared summary: the
/// built-in guarantees hold per estimate, not simultaneously.
pub(crate) fn arithmetic_guarantee(
    op: ArithmeticOpKind,
    left: Operand,
    right: Operand,
    ratio: bool,
    same_operand: bool,
) -> Option<ResultGuarantee> {
    let (l, r) = (
        additive_as_absolute(left.0?),
        additive_as_absolute(right.0?),
    );
    let once = same_operand.then(|| l.failure_probability.clone());
    let composed = match (op, l.is_exact(), r.is_exact()) {
        (_, true, true) => return Some(ResultGuarantee::exact("arithmetic over exact operands")),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, true, false) => shifted(r),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, false, true) => shifted(l),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, false, false) => compose(
            &CompositionOperator::Lipschitz { constant: 1.0 },
            vec![l],
            Some(&r),
        ),
        (ArithmeticOpKind::Mul, true, false) => scaled(r, left.1.map(f64::abs)),
        (ArithmeticOpKind::Mul, false, true) => scaled(l, right.1.map(f64::abs)),
        (ArithmeticOpKind::Div, false, true) => match right.1 {
            Some(0.0) => None,
            c => scaled(l, c.map(|c| 1.0 / c.abs())),
        },
        (ArithmeticOpKind::Div, _, _) if ratio => symbolic_ratio(l, r),
        _ => None,
    }?;
    Some(match once {
        Some(failure_probability) => ResultGuarantee {
            failure_probability,
            ..composed
        },
        None => composed,
    })
}

/// `x` times an exact factor (`Some(|c|)` when it is a literal): a
/// relative error is kept; an absolute error scales by `|c|`.
fn scaled(input: ResultGuarantee, factor: Option<f64>) -> Option<ResultGuarantee> {
    match (input.metric, factor) {
        (ErrorMetric::RelativeValue, _) => Some(input),
        (_, Some(constant)) => compose(
            &CompositionOperator::Lipschitz { constant },
            vec![input],
            None,
        ),
        _ => None,
    }
}

/// `x` plus an exact value: an absolute error is kept.
fn shifted(input: ResultGuarantee) -> Option<ResultGuarantee> {
    compose(
        &CompositionOperator::Lipschitz { constant: 1.0 },
        vec![input],
        None,
    )
}

/// A ratio of relative-error (or exact) operands: its relative bound is
/// `(a + b) / (1 − b)` only under enforced operand domains, which Stage 1
/// does not read, so the bound is the symbolic [`DIVISION_OPERAND_DOMAINS`].
fn symbolic_ratio(l: ResultGuarantee, r: ResultGuarantee) -> Option<ResultGuarantee> {
    let relative = |g: &ResultGuarantee| g.is_exact() || g.metric == ErrorMetric::RelativeValue;
    if !(relative(&l) && relative(&r)) {
        return None;
    }
    let failure_probability = ProbabilityExpr::UnionBound {
        terms: vec![l.failure_probability.clone(), r.failure_probability.clone()],
    };
    let provenance = [l, r]
        .into_iter()
        .enumerate()
        .map(|(input_index, g)| GuaranteeSource::ChildGuarantee {
            input_index,
            guarantee: Box::new(g),
        })
        .chain([
            GuaranteeSource::UnavailableStatistic {
                statistic: DIVISION_OPERAND_DOMAINS.into(),
            },
            GuaranteeSource::CompositionStep {
                operator: CompositionOperator::ExactDivision,
                rule: "relative_division_needs_operand_domains".into(),
            },
        ])
        .collect();
    Some(ResultGuarantee {
        metric: ErrorMetric::RelativeValue,
        bound: BoundExpr::Unknown {
            statistic: DIVISION_OPERAND_DOMAINS.into(),
        },
        failure_probability,
        provenance,
    })
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
            let candidate = targets
                .iter()
                .find(|(target, _)| target.measure == measure)
                .map_or(&ComputationCandidate::Exact, |(_, candidate)| *candidate);
            let filter = filters.get(measure).and_then(Option::as_ref).map(rewritten);
            substitute_measure(target, intent, candidate, filter, &input)
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

/// One measure of `target` over `input`: its group keys, then its value.
fn substitute_measure(
    target: &OperatorNode,
    intent: &AggIntent,
    candidate: &ComputationCandidate,
    filter: Option<Predicate>,
    input: &Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate { reduction, .. }) = target.non_asap() else {
        unreachable!("substitute checked the aggregate")
    };
    let state = |family: FieldDataType, intent: &AggIntent, read: SummaryInputExpr| {
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::SummaryAgg {
                child: input.clone(),
                input: summary_update(intent, &family, reduction, &input.schema, read)?,
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

    /// Guarantee composition through scalar and vector arithmetic (#669).
    mod guarantee_composition {
        use super::*;
        use crate::test_support::lower_promql;

        fn with_bound(metric: ErrorMetric, bound: f64, delta: f64) -> ResultGuarantee {
            ResultGuarantee {
                metric,
                bound: BoundExpr::Constant { value: bound },
                failure_probability: ProbabilityExpr::Constant { value: delta },
                provenance: vec![],
            }
        }
        fn abs(bound: f64, delta: f64) -> ResultGuarantee {
            with_bound(ErrorMetric::AbsoluteValue, bound, delta)
        }
        fn rel(bound: f64) -> ResultGuarantee {
            with_bound(ErrorMetric::RelativeValue, bound, 0.0)
        }
        /// `sum(m)` carrying `guarantee`.
        fn node(guarantee: ResultGuarantee) -> Rc<OperatorNode> {
            let node = lower_promql("sum(m)", AccuracyTarget::Exact);
            Rc::new((*node).clone().with_guarantee(Some(guarantee)))
        }
        fn scalar(guarantee: ResultGuarantee) -> ScalarExpr {
            ScalarExpr::PromqlScalarFromVector(node(guarantee))
        }
        fn lit(value: f64) -> ScalarExpr {
            ScalarExpr::literal_f64(value)
        }
        fn arith(op: ArithmeticOpKind, left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
            ScalarExpr::Arithmetic {
                op,
                left: Box::new(left),
                right: Box::new(right),
                semantics: ExprSemantics::Promql,
            }
        }
        fn call(name: &str, args: Vec<ScalarExpr>) -> ScalarExpr {
            ScalarExpr::FunctionCall {
                name: name.into(),
                args,
            }
        }
        fn of(expr: &ScalarExpr) -> Option<ResultGuarantee> {
            scalar_guarantee(expr, &|_| None)
        }
        fn bound(g: &ResultGuarantee) -> f64 {
            g.bound.evaluate().expect("a resolved bound")
        }
        fn delta(g: &ResultGuarantee) -> f64 {
            g.failure_probability
                .evaluate()
                .expect("a resolved probability")
        }
        fn close(a: f64, b: f64) -> bool {
            (a - b).abs() < 1e-12
        }

        /// Absolute errors add through `x ± y`, with the union bound on the
        /// failure probability; `−x` and `x ± c` keep the error, `c · x` and
        /// `x / c` scale it by `|c|` and `1/|c|`, division by a literal zero
        /// has none, and so does a product or quotient of two estimates.
        #[test]
        fn absolute_errors_compose_through_arithmetic() {
            use ArithmeticOpKind::*;
            let (x, y) = (scalar(abs(0.01, 0.001)), scalar(abs(0.02, 0.002)));
            for op in [Add, Sub] {
                let g = of(&arith(op.clone(), x.clone(), y.clone())).unwrap();
                assert_eq!(g.metric, ErrorMetric::AbsoluteValue, "{op:?}");
                assert!(close(bound(&g), 0.03) && close(delta(&g), 0.003), "{op:?}");
                for shifted in [
                    arith(op.clone(), x.clone(), lit(5.0)),
                    arith(op.clone(), lit(5.0), x.clone()),
                    arith(op.clone(), x.clone(), ScalarExpr::EvalTimestamp),
                ] {
                    let g = of(&shifted).unwrap();
                    assert!(close(bound(&g), 0.01) && close(delta(&g), 0.001), "{op:?}");
                }
            }
            let negated = ScalarExpr::Negative {
                expr: Box::new(x.clone()),
                semantics: ExprSemantics::Promql,
            };
            assert!(close(bound(&of(&negated).unwrap()), 0.01));
            for (expr, expected) in [
                (arith(Mul, lit(-3.0), x.clone()), 0.03),
                (arith(Mul, x.clone(), lit(3.0)), 0.03),
                (arith(Div, x.clone(), lit(-4.0)), 0.0025),
            ] {
                let g = of(&expr).unwrap();
                assert!(
                    close(bound(&g), expected) && close(delta(&g), 0.001),
                    "{expr:?}"
                );
            }
            for none in [
                arith(Div, x.clone(), lit(0.0)),
                arith(Mul, x.clone(), ScalarExpr::EvalTimestamp),
                arith(Mul, x.clone(), y.clone()),
                arith(Div, x.clone(), y.clone()),
                arith(Div, lit(1.0), x.clone()),
                arith(Pow, x.clone(), lit(2.0)),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
        }

        /// Count-Min and Count Sketch errors are additive: through
        /// arithmetic they compose as absolute errors, each normalized bound
        /// times its (symbolic) stream norm, for plan selection to bind.
        #[test]
        fn normalized_additive_errors_compose_as_absolute_errors() {
            let cms = with_bound(ErrorMetric::Frequency, 0.01, 0.001);
            let count_sketch = with_bound(ErrorMetric::L2Frequency, 0.02, 0.002);
            let g = of(&arith(
                ArithmeticOpKind::Sub,
                scalar(cms.clone()),
                scalar(count_sketch),
            ))
            .unwrap();
            assert_eq!(g.metric, ErrorMetric::AbsoluteValue);
            assert!(close(delta(&g), 0.003));
            let norms = |statistic: &str| match statistic {
                "stream_l1_norm" => Some(100.0),
                "stream_l2_norm" => Some(10.0),
                _ => None,
            };
            assert!(g.bound.has_unknown());
            assert!(close(g.bind(&norms).bound.evaluate().unwrap(), 1.2));
            let doubled = of(&arith(ArithmeticOpKind::Mul, lit(2.0), scalar(cms))).unwrap();
            assert!(close(doubled.bind(&norms).bound.evaluate().unwrap(), 2.0));
        }

        /// A relative error survives negation and scaling by an exact value,
        /// but not a shift (`x + c`) or a sum or difference of estimates
        /// (cancellation); a rank error composes to no guarantee (a rank
        /// interval counts as none in the MVP).
        #[test]
        fn relative_and_rank_errors_compose_only_where_sound() {
            use ArithmeticOpKind::*;
            let x = scalar(rel(0.01));
            for kept in [
                ScalarExpr::Negative {
                    expr: Box::new(x.clone()),
                    semantics: ExprSemantics::Promql,
                },
                arith(Mul, x.clone(), lit(-2.0)),
                arith(Div, x.clone(), ScalarExpr::EvalTimestamp),
            ] {
                let g = of(&kept).unwrap();
                assert_eq!(g.metric, ErrorMetric::RelativeValue, "{kept:?}");
                assert!(close(bound(&g), 0.01), "{kept:?}");
            }
            for none in [
                arith(Add, x.clone(), lit(1.0)),
                arith(Sub, x.clone(), scalar(rel(0.01))),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
            let rank = scalar(with_bound(ErrorMetric::Rank, 0.01, 0.001));
            for none in [
                arith(Sub, rank.clone(), rank.clone()),
                arith(Mul, rank.clone(), lit(2.0)),
                call("promql_clamp_max", vec![rank, lit(1.0)]),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
        }

        /// `clamp`, `clamp_min`, `clamp_max` and `abs` are 1-Lipschitz and
        /// monotone in each argument: the result keeps the largest absolute
        /// (or relative) error of its arguments, with the union bound on the
        /// failure probability.
        #[test]
        fn clamp_and_abs_keep_the_largest_argument_error() {
            let x = scalar(abs(0.01, 0.001));
            for expr in [
                call("promql_clamp", vec![x.clone(), lit(0.0), lit(10.0)]),
                call("promql_clamp_min", vec![x.clone(), lit(0.0)]),
                call("promql_clamp_max", vec![x.clone(), lit(10.0)]),
                call("promql_abs", vec![x.clone()]),
                call("abs", vec![x.clone()]),
            ] {
                let g = of(&expr).unwrap();
                assert_eq!(g.metric, ErrorMetric::AbsoluteValue, "{expr:?}");
                assert!(
                    close(bound(&g), 0.01) && close(delta(&g), 0.001),
                    "{expr:?}"
                );
            }
            let bounded = of(&call(
                "promql_clamp",
                vec![x.clone(), scalar(abs(0.03, 0.002)), lit(10.0)],
            ))
            .unwrap();
            assert!(close(bound(&bounded), 0.03) && close(delta(&bounded), 0.003));
            let relative = of(&call(
                "promql_clamp",
                vec![scalar(rel(0.01)), scalar(rel(0.02)), lit(10.0)],
            ))
            .unwrap();
            assert_eq!(relative.metric, ErrorMetric::RelativeValue);
            assert!(close(bound(&relative), 0.02));
            let mixed = call("promql_clamp_min", vec![x, scalar(rel(0.02))]);
            assert_eq!(of(&mixed), None);
            assert_eq!(of(&call("promql_floor", vec![scalar(rel(0.01))])), None);
        }

        /// A comparison over an estimate has no guarantee on its output (a
        /// band guarantee is later work); over exact operands it is exact.
        #[test]
        fn comparisons_over_estimates_have_no_guarantee() {
            let compare = |left: ScalarExpr| ScalarExpr::Compare {
                left: Box::new(left),
                op: CompareOpKind::Gt,
                right: Box::new(lit(5.0)),
                semantics: ExprSemantics::Promql,
            };
            assert_eq!(of(&compare(scalar(abs(0.01, 0.001)))), None);
            assert!(
                of(&compare(scalar(ResultGuarantee::exact("exact")))).is_some_and(|g| g.is_exact())
            );
        }

        /// A SQL scalar subquery inherits its relation's guarantee, as
        /// PromQL `scalar(v)` does.
        #[test]
        fn a_scalar_subquery_inherits_its_guarantee() {
            let g = abs(0.01, 0.001);
            let expr = arith(
                ArithmeticOpKind::Add,
                ScalarExpr::ScalarSubquery(node(g.clone())),
                lit(1.0),
            );
            assert!(close(bound(&of(&expr).unwrap()), 0.01));
        }

        /// Two estimates count their failure probabilities once each (union
        /// bound), even when they read one shared summary: the built-in
        /// guarantees hold per estimate, not simultaneously. The same
        /// planned estimate read twice fails or holds as one event, so its
        /// failure probability counts once.
        #[test]
        fn failure_probability_counts_once_per_distinct_estimate() {
            let shared = node(abs(0.01, 0.001));
            // Equal to `shared`, but planned separately.
            let other = node(abs(0.01, 0.001));
            let same = ScalarExpr::PromqlScalarFromVector(shared.clone());
            let twice = of(&arith(ArithmeticOpKind::Add, same.clone(), same.clone())).unwrap();
            assert!(close(bound(&twice), 0.02) && close(delta(&twice), 0.001));
            let distinct = of(&arith(
                ArithmeticOpKind::Add,
                same,
                ScalarExpr::PromqlScalarFromVector(other.clone()),
            ))
            .unwrap();
            assert!(close(bound(&distinct), 0.02) && close(delta(&distinct), 0.002));
            let vector = |lhs: &Rc<OperatorNode>, rhs: &Rc<OperatorNode>| {
                let node = OperatorNode::new(Operator::NonASAP(NonASAPOp::BinaryOp {
                    operator: BinaryOperator {
                        checked_relative_division: false,
                        checked_finite_division: false,
                        kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Sub),
                        vector_match: None,
                    },
                    return_bool: false,
                    lhs: lhs.clone(),
                    rhs: rhs.clone(),
                }))
                .unwrap();
                composed_guarantee(&node).unwrap()
            };
            assert!(close(delta(&vector(&shared, &shared)), 0.001));
            assert!(close(delta(&vector(&shared, &other)), 0.002));
        }
    }
}
