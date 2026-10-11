//! Pass 1 local alternatives over the unified logical IR.
//!
//! Alternatives are nominal realization descriptors attached to their original
//! target, not ranked plans or accuracy certificates. Workload composition and
//! physical planning consume this inventory later; empirical models belong to
//! selection. The legacy search API remains until planner cutover.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::{AggIntent, Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::Schema;
use asap_types::ir::schema::{
    default_hydra_params, DataType, EntityIdentity, ExactKind, ExactParams, FieldDataType,
    GroupingStrategy, HydraKind, NonNegativeWeightProof, SketchAlgorithm, SketchKind, SketchParams,
    SketchStatistic, SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, SchemaDerivationError};
use asap_types::types::AccuracyTarget;
use asap_types::workload::MetricType;
use thiserror::Error;

use crate::pass1::replacement::{
    accuracy_budget, accuracy_target, default_size_params, summary_candidates, Realization,
};
use crate::pass2::window_composition::{tumbling_state, WindowForm};

/// All local realizations of one single-measure aggregate. The target retains
/// source, grouping, filters, input expressions and evaluation context.
#[derive(Debug, Clone)]
pub struct LocalLogicalTarget {
    pub target: Rc<OperatorNode>,
    pub alternatives: Vec<Realization>,
    /// Per alternative, the target directly beneath this one that it
    /// absorbs: its summary reads that target's input, so that target is
    /// not computed and has no choice of its own (#509 whole-expression
    /// realization). `None`: the alternative reads this target's input.
    pub absorbs: Vec<Option<usize>>,
    /// Per alternative, how its summary covers the query window (Pass 2's
    /// window-composition rule, [`add_window_forms`]).
    ///
    /// [`add_window_forms`]: crate::pass2::window_composition::add_window_forms
    pub windows: Vec<WindowForm>,
    /// Per alternative, how a sketch's state serves the groups: one
    /// instance per group, or one shared Hydra grid ([`add_hydra_alternatives`]).
    pub groupings: Vec<GroupingStrategy>,
    /// Whether the target's input values are [`counter_samples`]. Absorbing
    /// alternatives read the input's own input, which then is too.
    pub counter_input: bool,
}

/// Compact Pass 1 inventory; roots and nested producer dependencies are retained.
#[derive(Debug, Clone)]
pub struct LocalLogicalCandidates<Id> {
    pub roots: Vec<(Id, QueryRoot)>,
    pub targets: Vec<LocalLogicalTarget>,
}

#[derive(Debug, Error)]
pub enum LogicalCandidateError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error("logical candidate input already has assigned execution timing")]
    AssignedTiming,
    #[error("approximate accuracy requires finite positive epsilon and delta in (0, 1)")]
    InvalidAccuracy,
    #[error("choice must name one listed alternative per target")]
    InvalidChoice,
    #[error("unsupported local realization: {0}")]
    Unsupported(&'static str),
}

/// Enumerate exact and summary choices in stable catalog order, without ranking
/// or empirical assessment. Parameters are candidate dimensions, not a claim
/// that a deployment meets the request's accuracy requirement.
pub fn local_realizations_for_intent(
    intent: &AggIntent,
) -> Result<Vec<Realization>, LogicalCandidateError> {
    let mut choices = vec![Realization::PassThrough];
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
    if let Some((kind, params)) = exact {
        choices.push(Realization::ExactAggregate { kind, params });
    }
    if let Some(target) = accuracy_target(intent) {
        if *target != AccuracyTarget::Exact {
            let (epsilon, delta) = accuracy_budget(target);
            if !epsilon.is_finite()
                || epsilon <= 0.0
                || !delta.is_finite()
                || !(0.0..1.0).contains(&delta)
                || delta == 0.0
            {
                return Err(LogicalCandidateError::InvalidAccuracy);
            }
            for algorithm in summary_candidates(intent) {
                choices.push(Realization::Sketch(SketchKind::new(
                    algorithm.clone(),
                    default_size_params(algorithm.clone(), intent, epsilon, delta),
                )));
            }
        }
    }
    Ok(choices)
}

/// Discover single-measure targets, including operator plans read by scalar roots.
/// Multi-measure aggregates remain intact pending a semantics-preserving split.
pub fn enumerate_local_logical_candidates<Id>(
    roots: Vec<(Id, QueryRoot)>,
    metric_types: &BTreeMap<String, MetricType>,
) -> Result<LocalLogicalCandidates<Id>, LogicalCandidateError> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    // Consumers of each node: the distinct nodes reading it, plus roots.
    let mut consumers: HashMap<*const OperatorNode, usize> = HashMap::new();
    for (_, root) in &roots {
        root.validate_structure()?;
        let operators = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for root in operators {
            *consumers.entry(Rc::as_ptr(root)).or_default() += 1;
            for node in OperatorNode::reachable(root) {
                if !seen.insert(Rc::as_ptr(&node)) {
                    continue;
                }
                for child in node.children() {
                    *consumers.entry(Rc::as_ptr(child)).or_default() += 1;
                }
                if node.timing.is_some() {
                    return Err(LogicalCandidateError::AssignedTiming);
                }
                if let Some(NonASAPOp::Aggregate { measures, .. }) = node.non_asap() {
                    if let [intent] = measures.as_slice() {
                        let mut alternatives = local_realizations_for_intent(intent)?;
                        // A SQL `COUNT(*)` group's rows all hash its key, so
                        // a per-group sketch is a counter with extra memory;
                        // only the shared Hydra grid (added below) helps.
                        if matches!(intent, AggIntent::Count { .. })
                            && node.children().iter().all(|child| {
                                child.schema.closed && !child.schema.has_promql_series_identity()
                            })
                        {
                            alternatives.retain(|a| !matches!(a, Realization::Sketch(_)));
                        }
                        let mut target = LocalLogicalTarget {
                            absorbs: vec![None; alternatives.len()],
                            windows: vec![WindowForm::Whole; alternatives.len()],
                            groupings: vec![GroupingStrategy::default(); alternatives.len()],
                            alternatives,
                            counter_input: node
                                .children()
                                .iter()
                                .all(|child| counter_samples(child, metric_types)),
                            target: node,
                        };
                        add_hydra_alternatives(&mut target)?;
                        targets.push(target);
                    }
                }
            }
        }
    }
    add_whole_expression_alternatives(&mut targets, &consumers);
    Ok(LocalLogicalCandidates { roots, targets })
}

/// Whole-expression top-k (#509 Example 1's Q2): a top-k over a per-item sum
/// or count is realized as one heap sketch over the inner aggregate's input,
/// keyed by the ranked item and weighted by the summed value, instead of a
/// sketch over the inner aggregate's exact result. The decision is the
/// legacy keyed-additive rule's. Offered only when the inner target has no
/// other consumer, so absorbing it removes its work.
fn add_whole_expression_alternatives(
    targets: &mut [LocalLogicalTarget],
    consumers: &HashMap<*const OperatorNode, usize>,
) {
    let position: HashMap<_, _> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| (Rc::as_ptr(&t.target), i))
        .collect();
    for target in targets.iter_mut() {
        let Some(NonASAPOp::Aggregate { child, .. }) = target.target.non_asap() else {
            continue;
        };
        let Some(&inner) = position.get(&Rc::as_ptr(child)) else {
            continue;
        };
        if consumers.get(&Rc::as_ptr(child)) != Some(&1)
            || whole_expression_input(&target.target).is_none()
        {
            continue;
        }
        let heaps: Vec<_> = target
            .alternatives
            .iter()
            .filter(|a| {
                matches!(a, Realization::Sketch(kind) if matches!(
                    kind.algorithm(),
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                ))
            })
            .cloned()
            .collect();
        for heap in heaps {
            target.alternatives.push(heap);
            target.absorbs.push(Some(inner));
            target.windows.push(WindowForm::Whole);
            target.groupings.push(GroupingStrategy::default());
        }
    }
}

/// Hydra (#580 W7, count first): a grouped approximate count may keep one
/// shared Count-Min grid for all its groups instead of one sketch per group.
/// The grid's collision term adds to the inner sketch's error, so each half
/// of the budget sizes one: the inner sketch and the grid get ε/2 and δ/2.
/// Offered when the target has groups (`by`, not `without`) and an item
/// column the kernel can hash ([`hydra_update`]).
pub fn add_hydra_alternatives(
    target: &mut LocalLogicalTarget,
) -> Result<(), LogicalCandidateError> {
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        ..
    }) = target.target.non_asap()
    else {
        return Ok(());
    };
    let [intent @ AggIntent::Count { accuracy }] = measures.as_slice() else {
        return Ok(());
    };
    if *accuracy == AccuracyTarget::Exact
        || !crate::pass1::grouping::has_subpopulations(reduction)
        || reduction.group_keys().is_some_and(|keys| keys.is_without())
        || hydra_update(reduction, &child.schema).is_none()
    {
        return Ok(());
    }
    let (epsilon, delta) = accuracy_budget(accuracy);
    let SketchParams::Cms { width, depth } =
        default_size_params(SketchAlgorithm::Cms, intent, epsilon / 2.0, delta / 2.0)
    else {
        return Err(LogicalCandidateError::Unsupported("Count-Min sizing"));
    };
    let kind = SketchKind::new(SketchAlgorithm::Cms, SketchParams::Cms { width, depth });
    let params = default_hydra_params(HydraKind::HydraCms, kind.params())
        .ok_or(LogicalCandidateError::Unsupported("HydraCms parameters"))?;
    target.alternatives.push(Realization::Sketch(kind));
    target.absorbs.push(None);
    target.windows.push(WindowForm::Whole);
    target
        .groupings
        .push(GroupingStrategy::SharedMultiSubpopulation {
            kind: HydraKind::HydraCms,
            params,
        });
    Ok(())
}

/// A HydraCms update (#600's contract): a unit weight per row, hashed by
/// a [`count_item`].
fn hydra_update(reduction: &Reduction, child: &Schema) -> Option<SummaryUpdate> {
    Some(SummaryUpdate {
        item: Some(SummaryInputExpr::Column(
            crate::pass1::replacement::column_ref(count_item(reduction, child)?),
        )),
        weight: SummaryInputExpr::Constant(1.0),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
    })
}

/// The item a count sketch hashes per row: a group's count ignores its
/// value, so any non-null column the kernels hash (Utf8, Int64 or Bool)
/// serves, a grouping column first. PromQL labels are nullable; the series
/// identity is not.
fn count_item<'a>(
    reduction: &Reduction,
    child: &'a Schema,
) -> Option<&'a asap_types::ir::schema::Field> {
    let keys = reduction
        .group_keys()
        .filter(|keys| !keys.is_without())
        .into_iter()
        .flat_map(|keys| keys.iter().copied());
    keys.chain(0..child.fields.len())
        .filter_map(|index| child.fields.get(index))
        .find(|field| {
            !field.nullable
                && matches!(
                    field.dtype,
                    FieldDataType::Plain(DataType::Utf8 | DataType::Int64 | DataType::Bool)
                )
        })
}

/// The input and update of a whole-expression top-k over `target`'s inner
/// aggregate, by the legacy keyed-additive rule, or `None` when it does not
/// apply. Rows that carry the full series identity rank it as a column, as
/// [`summary_update`] does.
fn whole_expression_input(target: &OperatorNode) -> Option<(Rc<OperatorNode>, SummaryUpdate)> {
    use crate::pass1::replacement::{
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
    let ([intent @ AggIntent::TopK { .. }], true) = (measures.as_slice(), filters.is_empty())
    else {
        return None;
    };
    // The rule reads only the family's algorithm, and rejects Count-Min over
    // signed weights. Ask with CountSketch so one update serves both heap
    // sketches; Stage 3 decides whether Count-Min is admissible, as it does
    // for every other Count-Min candidate.
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CountSketchWithHeap,
            asap_types::ir::schema::SketchParams::CountSketchWithHeap {
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
            asap_types::ir::schema::PROMQL_SERIES_IDENTITY.into(),
        )));
    } else if realized.child.schema.closed {
        // An encoded label set needs an open PromQL schema.
        return None;
    }
    Some((realized.child, input))
}

/// Number of whole-workload candidates: one per choice of an alternative for
/// every target, where a target absorbed by the alternative above it takes
/// only its pass-through (it is not computed). Saturates rather than
/// overflowing.
pub fn combination_count<Id>(inventory: &LocalLogicalCandidates<Id>) -> usize {
    completions(inventory, &[])
}

/// Valid choices extending `prefix` (choices for the first `prefix.len()`
/// targets); 0 when `prefix` is invalid. A target and the target its
/// alternatives absorb are counted together.
fn completions<Id>(inventory: &LocalLogicalCandidates<Id>, prefix: &[usize]) -> usize {
    let targets = &inventory.targets;
    let mut paired = vec![false; targets.len()];
    let mut total = 1usize;
    for (t, target) in targets.iter().enumerate() {
        let Some(u) = target.absorbs.iter().flatten().next().copied() else {
            continue;
        };
        paired[t] = true;
        paired[u] = true;
        let absorbing = |c: usize| target.absorbs[c].is_some();
        let own = target.alternatives.len();
        let inner = targets[u].alternatives.len();
        let absorbing_count = (0..own).filter(|&c| absorbing(c)).count();
        let options = match (prefix.get(t), prefix.get(u)) {
            (Some(&c), Some(&d)) => usize::from(!absorbing(c) || d == 0),
            (Some(&c), None) if absorbing(c) => 1,
            (Some(_), None) => inner,
            (None, Some(0)) => own,
            (None, Some(_)) => own - absorbing_count,
            (None, None) => (own - absorbing_count)
                .saturating_mul(inner)
                .saturating_add(absorbing_count),
        };
        total = total.saturating_mul(options);
    }
    for (j, target) in targets.iter().enumerate().skip(prefix.len()) {
        if !paired[j] {
            total = total.saturating_mul(target.alternatives.len());
        }
    }
    total
}

/// The first `max` choices in enumeration order: mixed radix, the last target
/// varying fastest, skipping choices for a target its outer choice absorbs.
/// `choice[i]` indexes `inventory.targets[i].alternatives`.
pub fn enumerate_choices<Id>(
    inventory: &LocalLogicalCandidates<Id>,
    max: usize,
) -> Vec<Vec<usize>> {
    let count = combination_count(inventory).min(max);
    let mut choices = Vec::with_capacity(count);
    let mut choice = vec![0; inventory.targets.len()];
    while choices.len() < count {
        if completions(inventory, &choice) == 1 {
            choices.push(choice.clone());
        }
        for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
            *digit += 1;
            if *digit < target.alternatives.len() {
                break;
            }
            *digit = 0;
        }
    }
    choices
}

/// Position of `choice` in [`enumerate_choices`] order: the valid choices
/// that precede it.
pub fn choice_index<Id>(inventory: &LocalLogicalCandidates<Id>, choice: &[usize]) -> usize {
    let mut index = 0usize;
    let mut prefix = Vec::with_capacity(choice.len());
    for &digit in choice {
        for smaller in 0..digit {
            prefix.push(smaller);
            index = index.saturating_add(completions(inventory, &prefix));
            prefix.pop();
        }
        prefix.push(digit);
    }
    index
}

/// The targets alternative `c` of target `t` reads directly: those beneath
/// `t`, except one it absorbs, whose own targets beneath it are read instead.
/// `beneath` is [`nested_targets`].
pub fn read_targets<Id>(
    inventory: &LocalLogicalCandidates<Id>,
    beneath: &[Vec<usize>],
    t: usize,
    c: usize,
) -> Vec<usize> {
    match inventory.targets[t].absorbs[c] {
        None => beneath[t].clone(),
        Some(u) => {
            let mut read: Vec<_> = beneath[t].iter().copied().filter(|&v| v != u).collect();
            read.extend(&beneath[u]);
            read.sort_unstable();
            read.dedup();
            read
        }
    }
}

/// For each target, the targets directly beneath it: reachable from its input
/// without passing through another target.
pub fn nested_targets<Id>(inventory: &LocalLogicalCandidates<Id>) -> Vec<Vec<usize>> {
    let position: HashMap<_, _> = inventory
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| (Rc::as_ptr(&t.target), i))
        .collect();
    inventory
        .targets
        .iter()
        .map(|target| {
            let mut found = Vec::new();
            let mut seen = HashSet::new();
            let mut stack: Vec<_> = target.target.children().into_iter().cloned().collect();
            while let Some(node) = stack.pop() {
                if !seen.insert(Rc::as_ptr(&node)) {
                    continue;
                }
                match position.get(&Rc::as_ptr(&node)) {
                    Some(&index) => found.push(index),
                    None => stack.extend(node.children().into_iter().cloned()),
                }
            }
            found.sort_unstable();
            found
        })
        .collect()
}

/// Build one whole-workload candidate (#509 Stage 1): `choice[i]` indexes
/// `inventory.targets[i].alternatives`. Each chosen non-pass-through target is
/// replaced by `SummaryAgg` followed by `SummaryEstimate` (sketch) or
/// `FinalizeExactAccumulator` (exact accumulator). Untouched sub-DAGs keep
/// their identity, so sharing between roots is preserved.
pub fn compose_logical_candidate<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    choice: &[usize],
) -> Result<Vec<(Id, QueryRoot)>, LogicalCandidateError> {
    if choice.len() != inventory.targets.len() {
        return Err(LogicalCandidateError::InvalidChoice);
    }
    let chosen = inventory
        .targets
        .iter()
        .zip(choice)
        .map(|(target, &index)| {
            target
                .alternatives
                .get(index)
                .map(|alternative| {
                    let absorbs = target.absorbs[index].is_some();
                    (
                        Rc::as_ptr(&target.target),
                        Chosen {
                            realization: alternative,
                            absorbs,
                            counter_input: target.counter_input,
                            window: target.windows[index],
                            grouping: &target.groupings[index],
                        },
                    )
                })
                .ok_or(LogicalCandidateError::InvalidChoice)
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    let mut memo = HashMap::new();
    inventory
        .roots
        .iter()
        .map(|(id, root)| {
            let root = match root {
                QueryRoot::Operator(node) => {
                    QueryRoot::Operator(rewrite(node, &chosen, &mut memo)?)
                }
                QueryRoot::Scalar(expr) => {
                    for node in expr.operator_refs() {
                        rewrite(node, &chosen, &mut memo)?;
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| memo[&Rc::as_ptr(node)].clone()),
                    )
                }
            };
            Ok((id.clone(), root))
        })
        .collect()
}

type Memo = HashMap<*const OperatorNode, Rc<OperatorNode>>;

/// One target's chosen alternative.
struct Chosen<'a> {
    realization: &'a Realization,
    /// Whether it absorbs the target beneath.
    absorbs: bool,
    /// The target's [`LocalLogicalTarget::counter_input`].
    counter_input: bool,
    window: WindowForm,
    grouping: &'a GroupingStrategy,
}

fn rewrite(
    node: &Rc<OperatorNode>,
    chosen: &HashMap<*const OperatorNode, Chosen<'_>>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, LogicalCandidateError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        rewrite(child, chosen, memo)?;
    }
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &memo[&Rc::as_ptr(child)]));
    let rebuilt = match chosen.get(&Rc::as_ptr(node)) {
        Some(chosen) if *chosen.realization != Realization::PassThrough => {
            realize(node, chosen, memo)?
        }
        _ if changed => Rc::new(node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?),
        _ => node.clone(),
    };
    memo.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

fn realize(
    target: &OperatorNode,
    chosen: &Chosen<'_>,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, LogicalCandidateError> {
    let Chosen {
        realization,
        absorbs,
        counter_input,
        window,
        grouping,
    } = *chosen;
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        having,
        ..
    }) = target.non_asap()
    else {
        return Err(LogicalCandidateError::Unsupported(
            "target is not an aggregate",
        ));
    };
    let [intent] = measures.as_slice() else {
        return Err(LogicalCandidateError::Unsupported(
            "multi-measure aggregate",
        ));
    };
    if having.is_some() {
        return Err(LogicalCandidateError::Unsupported("HAVING aggregate"));
    }
    // A single measure's row filter (SQL `FILTER (WHERE …)`) becomes the
    // summary's filter over the same input rows; every group is kept. A
    // whole-expression target is never filtered.
    let filter = filters.first().cloned().flatten();
    let whole =
        match absorbs {
            true => Some(whole_expression_input(target).ok_or(
                LogicalCandidateError::Unsupported("whole-expression top-k input"),
            )?),
            false => None,
        };
    let child = match &whole {
        Some((input, _)) => memo[&Rc::as_ptr(input)].clone(),
        None => memo[&Rc::as_ptr(child)].clone(),
    };
    let (family, query) = match realization {
        Realization::ExactAggregate { kind, params } => (
            FieldDataType::ExactAggregate(kind.clone(), params.clone()),
            None,
        ),
        Realization::Sketch(kind) => (
            FieldDataType::Sketch(kind.clone(), grouping.clone()),
            Some(statistic(intent)?),
        ),
        _ => return Err(LogicalCandidateError::Unsupported("summary family")),
    };
    let mut input = match whole {
        Some((_, update)) => update,
        None if *grouping != GroupingStrategy::PerSubpopulationInstance => {
            hydra_update(reduction, &child.schema)
                .ok_or(LogicalCandidateError::Unsupported("Hydra item column"))?
        }
        None => summary_update(intent, &family, reduction, &child.schema)?,
    };
    if counter_input
        && input.item.is_some()
        && input.weight == SummaryInputExpr::Column(ColumnRef::SampleValue)
        && input.weight_domain == WeightDomain::UnknownOrSigned
    {
        input.weight_domain = WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::CounterSamples,
        };
    }
    let build = |child: Rc<OperatorNode>| {
        Ok::<_, LogicalCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::SummaryAgg {
                child,
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: filter.clone(),
            },
        ))?)
    };
    let state = match window {
        WindowForm::Whole => build(child)?,
        WindowForm::Tumbling { .. } | WindowForm::Segments { .. } => {
            tumbling_state(&child, window, build)?
        }
    };
    let evaluation = match query {
        Some(query) => ASAPOp::SummaryEstimate {
            summary_input: state,
            query,
        },
        None => ASAPOp::FinalizeExactAccumulator { child: state },
    };
    let mut evaluation = OperatorNode::new(Operator::ASAP(evaluation))?;
    keep_output_name(target, &mut evaluation);
    Ok(Rc::new(evaluation))
}

/// The evaluation answers `target`, so a measure the query named explicitly
/// (SQL `approx_percentile_cont(...)`) keeps its name; a synthetic name is
/// left as derived.
fn keep_output_name(target: &OperatorNode, evaluation: &mut OperatorNode) {
    let Some(NonASAPOp::Aggregate { output_names, .. }) = target.non_asap() else {
        return;
    };
    let [name] = output_names.as_slice() else {
        return;
    };
    if name.is_empty() || evaluation.schema.fields.len() != target.schema.fields.len() {
        return;
    }
    for (field, named) in evaluation
        .schema
        .fields
        .iter_mut()
        .zip(&target.schema.fields)
    {
        if named.name == *name {
            field.name = name.clone();
        }
    }
}

/// Whether every value `node` outputs is a sample of a metric declared a
/// counter, or a sum of such samples: never negative. Only a time-range
/// selection and a plain sum preserve that; any other operator (arithmetic
/// included) ends the proof. Read off the frontend DAG, this also holds for
/// the composed candidate because a sum has only exact realizations.
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

/// What each input row contributes, following the legacy realization rules:
/// heap sketches rank series identities, frequency sketches count values, and
/// every other family reads the measure's input column.
fn summary_update(
    intent: &AggIntent,
    family: &FieldDataType,
    reduction: &Reduction,
    child: &Schema,
) -> Result<SummaryUpdate, LogicalCandidateError> {
    let algorithm = match family {
        FieldDataType::Sketch(kind, _) => Some(kind.algorithm()),
        _ => None,
    };
    if let AggIntent::Count { .. } = intent {
        if child.closed && !child.has_promql_series_identity() {
            return sql_row_count_update(algorithm.is_some(), reduction, child);
        }
    }
    let weight = crate::pass1::replacement::summarised_input(intent, child)
        .map_err(|_| LogicalCandidateError::Unsupported("input column outside child schema"))?;
    Ok(match (intent, algorithm) {
        (AggIntent::TopK { .. }, Some(_)) => {
            // SQL rows carry no implicit series identity to rank; closed
            // PromQL rows carry it as a column.
            if child.closed && !child.has_promql_series_identity() {
                return Err(LogicalCandidateError::Unsupported(
                    "Top-K item identity for closed schemas",
                ));
            }
            if reduction.group_keys().is_some_and(|keys| keys.is_without()) {
                return Err(LogicalCandidateError::Unsupported(
                    "Top-K partitions given by `without`",
                ));
            }
            let excluding = reduction
                .group_keys()
                .into_iter()
                .flat_map(|keys| keys.iter())
                .filter_map(|&index| child.fields.get(index))
                .map(crate::pass1::replacement::column_ref)
                .collect();
            // Rows that carry the full series identity rank it as a column,
            // the item form the runtime builds keyed summaries from.
            let item = if child.has_promql_series_identity() {
                SummaryInputExpr::Column(ColumnRef::Named(
                    asap_types::ir::schema::PROMQL_SERIES_IDENTITY.into(),
                ))
            } else {
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

/// SQL `COUNT(*)`: rows have no sample value, and every row counts, so each
/// adds a unit weight. A sketch hashes a [`count_item`] per row.
fn sql_row_count_update(
    sketch: bool,
    reduction: &Reduction,
    child: &Schema,
) -> Result<SummaryUpdate, LogicalCandidateError> {
    let item = if sketch {
        let column = count_item(reduction, child).ok_or(LogicalCandidateError::Unsupported(
            "a COUNT(*) sketch needs a non-null item column",
        ))?;
        Some(SummaryInputExpr::Column(
            crate::pass1::replacement::column_ref(column),
        ))
    } else {
        None
    };
    Ok(SummaryUpdate {
        item,
        weight: SummaryInputExpr::Constant(1.0),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
    })
}

fn statistic(intent: &AggIntent) -> Result<SketchStatistic, LogicalCandidateError> {
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
        _ => return Err(LogicalCandidateError::Unsupported("sketch evaluation")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;

    /// Every realization of a top-k returns the same selected-rows schema, so
    /// an aggregate over a pass-through top-k composes like one over a sketch.
    #[test]
    fn aggregate_over_any_topk_realization_composes() {
        let root = lower_promql(
            "count(topk by (job) (10, sum_over_time(m[1m])))",
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            },
        );
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
        let inventory = enumerate_local_logical_candidates(
            vec![(0, QueryRoot::Operator(root))],
            &BTreeMap::new(),
        )
        .unwrap();
        let topk = inventory
            .targets
            .iter()
            .position(|t| {
                matches!(t.target.non_asap(), Some(NonASAPOp::Aggregate { measures, .. })
                    if matches!(measures.as_slice(), [AggIntent::TopK { .. }]))
            })
            .unwrap();
        let mut schemas = Vec::new();
        for (count, alternatives) in inventory.targets.iter().enumerate() {
            if count == topk {
                continue;
            }
            for outer in 0..alternatives.alternatives.len() {
                for inner in 0..inventory.targets[topk].alternatives.len() {
                    let mut choice = vec![0; inventory.targets.len()];
                    choice[count] = outer;
                    choice[topk] = inner;
                    compose_logical_candidate(&inventory, &choice)
                        .unwrap_or_else(|e| panic!("{choice:?}: {e}"));
                }
            }
        }
        for inner in 0..inventory.targets[topk].alternatives.len() {
            let mut choice = vec![0; inventory.targets.len()];
            choice[topk] = inner;
            let roots = compose_logical_candidate(&inventory, &choice).unwrap();
            let QueryRoot::Operator(root) = &roots[0].1 else {
                panic!("operator root")
            };
            let NonASAPOp::Aggregate { child, .. } = root.expect_non_asap() else {
                panic!("count over top-k")
            };
            schemas.push(child.schema.clone());
        }
        assert!(schemas.windows(2).all(|w| w[0] == w[1]), "{schemas:#?}");
    }
    /// A top-k over a per-series `sum_over_time` gets whole-expression heap
    /// sketches that absorb the inner target; enumeration skips the absorbed
    /// target's choices, and `choice_index` numbers choices in that order.
    #[test]
    fn whole_expression_topk_absorbs_the_inner_sum() {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            },
        );
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
        let inventory = enumerate_local_logical_candidates(
            vec![(0, QueryRoot::Operator(root))],
            &BTreeMap::new(),
        )
        .unwrap();
        let (topk, inner) = inventory
            .targets
            .iter()
            .enumerate()
            .find_map(|(t, target)| Some((t, target.absorbs.iter().flatten().next().copied()?)))
            .expect("an absorbing alternative");
        let absorbing = inventory.targets[topk].absorbs.iter().flatten().count();
        assert_eq!(absorbing, 2, "Count-Min and CountSketch with heap");
        // (pass-through, CMS+heap, CountSketch+heap) × (raw, Sum acc) + 2.
        let choices = enumerate_choices(&inventory, usize::MAX);
        assert_eq!(choices.len(), 8);
        assert_eq!(combination_count(&inventory), 8);
        for (index, choice) in choices.iter().enumerate() {
            assert_eq!(choice_index(&inventory, choice), index);
            if inventory.targets[topk].absorbs[choice[topk]].is_some() {
                assert_eq!(choice[inner], 0);
                let roots = compose_logical_candidate(&inventory, choice).unwrap();
                let QueryRoot::Operator(root) = &roots[0].1 else {
                    panic!("operator root")
                };
                assert!(
                    !OperatorNode::reachable(root)
                        .iter()
                        .any(|n| Rc::ptr_eq(n, &inventory.targets[inner].target)),
                    "the inner sum is not computed"
                );
            }
        }
    }

    /// The weight domain of every heap-sketch update (Count-Min or
    /// CountSketch + heap) in every candidate of `query`.
    fn heap_weight_domains(query: &str, metric_types: &[(&str, MetricType)]) -> Vec<WeightDomain> {
        let root = lower_promql(
            query,
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            },
        );
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
        let metric_types = metric_types
            .iter()
            .map(|(metric, kind)| (metric.to_string(), *kind))
            .collect();
        let inventory =
            enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(root))], &metric_types)
                .unwrap();
        let mut domains = Vec::new();
        for choice in enumerate_choices(&inventory, usize::MAX) {
            for (_, root) in compose_logical_candidate(&inventory, &choice).unwrap() {
                let QueryRoot::Operator(root) = root else {
                    panic!("operator root")
                };
                for node in OperatorNode::reachable(&root) {
                    if let Operator::ASAP(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _),
                        input,
                        ..
                    }) = &node.operator
                    {
                        if matches!(
                            kind.algorithm(),
                            SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                        ) {
                            domains.push(input.weight_domain.clone());
                        }
                    }
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
        // (CMS, CountSketch) × (raw sum, Sum accumulator) + 2 whole-expression.
        assert_eq!(domains.len(), 6);
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
            assert_eq!(domains.len(), 6, "{query}");
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
            let root = lower_promql(query, AccuracyTarget::Exact);
            assert_eq!(counter_samples(&root, &metric_types), proven, "{query}");
        }
    }

    /// Only an approximate count with `by` groups gets a HydraCms
    /// alternative, sized for half the budget, with a matching grouping.
    #[test]
    fn grouped_approximate_count_offers_hydra() {
        let approximate = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        };
        let hydra = |query: &str, accuracy: AccuracyTarget| {
            let root = lower_promql(query, accuracy);
            let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
            let inventory = enumerate_local_logical_candidates(
                vec![(0, QueryRoot::Operator(root))],
                &BTreeMap::new(),
            )
            .unwrap();
            let target = &inventory.targets[0];
            assert_eq!(target.groupings.len(), target.alternatives.len());
            target
                .alternatives
                .iter()
                .zip(&target.groupings)
                .filter(|(_, g)| **g != GroupingStrategy::default())
                .map(|(a, g)| (a.clone(), g.clone()))
                .collect::<Vec<_>>()
        };
        let offered = hydra("count by (job) (m)", approximate.clone());
        let [(Realization::Sketch(kind), GroupingStrategy::SharedMultiSubpopulation { params, .. })] =
            offered.as_slice()
        else {
            panic!("one HydraCms alternative: {offered:?}")
        };
        assert_eq!(kind.algorithm(), &SketchAlgorithm::Cms);
        // ⌈e/(ε/2)⌉ columns, ⌈ln(2/δ)⌉ rows, for the inner sketch and the grid.
        assert_eq!(
            *params,
            asap_types::ir::schema::HydraParams::HydraCms {
                width: 544,
                depth: 6,
                shared_rows: 6,
                shared_columns: 544
            }
        );
        assert!(hydra("count (m)", approximate.clone()).is_empty());
        assert!(hydra("count without (job) (m)", approximate).is_empty());
        assert!(hydra("count by (job) (m)", AccuracyTarget::Exact).is_empty());
    }

    /// A SQL `COUNT(*)` group's rows all hash its key, so Pass 1 offers no
    /// per-group sketch: pass-through, the exact `Count` and HydraCms. A
    /// PromQL count keeps Count-Min, Count Sketch and UnivMon.
    #[test]
    fn sql_count_star_offers_no_per_group_sketch() {
        let approximate = AccuracyTarget::EpsilonDelta {
            epsilon: 0.1,
            delta: 0.01,
        };
        let offered = |root: Rc<OperatorNode>| {
            let inventory = enumerate_local_logical_candidates(
                vec![(0, QueryRoot::Operator(root))],
                &BTreeMap::new(),
            )
            .unwrap();
            let target = &inventory.targets[0];
            target
                .alternatives
                .iter()
                .zip(&target.groupings)
                .map(|(a, g)| {
                    let family = match a {
                        Realization::PassThrough => "PassThrough".to_string(),
                        Realization::ExactAggregate { kind, .. } => format!("{kind:?}"),
                        Realization::Sketch(kind) => format!("{:?}", kind.algorithm()),
                        other => format!("{other:?}"),
                    };
                    match *g == GroupingStrategy::default() {
                        true => family,
                        false => format!("Hydra{family}"),
                    }
                })
                .collect::<Vec<_>>()
        };
        let mut schema = Schema::new(vec![
            asap_types::ir::schema::Field::plain("ts", DataType::Timestamp, false),
            asap_types::ir::schema::Field::plain("src_ip", DataType::Utf8, false),
        ]);
        schema.closed = true;
        let rows = crate::test_support::scan_from(
            Source::Table {
                table_ref: "flows".into(),
            },
            schema,
        );
        let count = AggIntent::Count {
            accuracy: approximate.clone(),
        };
        assert_eq!(
            offered(crate::test_support::agg(vec![1], count, rows)),
            ["PassThrough", "Count", "HydraCms"]
        );
        for query in ["count by (job) (m)", "count_over_time(m[1m])"] {
            let root = lower_promql(query, approximate.clone());
            let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
            let families = offered(root);
            for sketch in ["Cms", "CountSketch", "UnivMon"] {
                assert!(
                    families.iter().any(|f| f == sketch),
                    "{query}: {families:?}"
                );
            }
        }
    }

    /// Approximate requests must retain the exact execution alternative too.
    #[test]
    fn approximate_count_keeps_exact_and_universal_choices() {
        let choices = local_realizations_for_intent(&AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.05,
                delta: 0.01,
            },
        })
        .unwrap();
        assert!(choices
            .iter()
            .any(|choice| matches!(choice, Realization::PassThrough)));
        assert!(choices.iter().any(|choice| matches!(
            choice,
            Realization::ExactAggregate {
                kind: ExactKind::Count,
                ..
            }
        )));
        assert!(choices.iter().any(|choice| matches!(choice, Realization::Sketch(kind) if *kind.algorithm() == asap_types::ir::schema::SketchAlgorithm::UnivMon)));
    }
}
