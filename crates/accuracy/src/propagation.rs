//! Guarantee propagation: the theoretical guarantee of every node of a
//! composed DAG, composed with the built-in analytical rules
//! (`crate::composition::propagate`) through operators, and the helpers
//! Pass 2 and plan selection (Stage 3) use to read, bind and recompose
//! those guarantees and to resize the summaries they depend on.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::operator_properties::{BinaryOpKind, JoinKind, Reduction, Source};
use asap_types::ir::operator::pane_chain::{merged_chain, merged_window_build};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::guarantee::{
    AccuracyGuarantee, AccuracyMetric, BoundExpr, CompositionOperator, GuaranteeSource,
    ProbabilityExpr,
};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Schema};
use asap_types::ir::schema::{
    ExactKind, FamilyCapability, FieldDataType, GroupingStrategy, MergedError,
    NonNegativeWeightProof, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic,
    SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, NonASAPOp, Operator, OperatorNode, ProjectItem, QueryRoot, ScalarExpr,
    SchemaDerivationError,
};

use crate::composition::{
    additive_as_absolute, propagate, raw_data_samples_sign_statistic, StreamNorm, GROUP_ROW_COUNT,
    PRODUCT_OPERAND_DOMAINS, QUOTIENT_OPERAND_DOMAINS,
};
use crate::estimators::local_guarantee;
use crate::estimators::{hll, univmon};
use crate::function_rules::approximate_input_composition;
use crate::PropagationStats;

/// A rebuilt node per original node, keyed by the original's address.
type NodeToRewrittenNodeCache = HashMap<*const OperatorNode, Rc<OperatorNode>>;

/// The metric whose raw data samples, or plain sums of them, are every value
/// `node` outputs, or `None`. Only a time-series scan, a time range over it,
/// an `offset`-only time shift (it moves the window, not the samples) and a
/// plain `sum` keep that; any other operator (rate and arithmetic included)
/// ends it. The metric's type is not read here: whether its
/// samples are non-negative is plan selection's question
/// ([`WeightDomain::RawDataSamplesOfMetric`]).
pub fn raw_data_samples_of_metric(node: &OperatorNode) -> Option<String> {
    match node.non_asap()? {
        NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            ..
        } => Some(metric.clone()),
        NonASAPOp::TimeRange { child, .. } => raw_data_samples_of_metric(child),
        NonASAPOp::TimeShift { shift, child } if shift.at.is_none() => {
            raw_data_samples_of_metric(child)
        }
        NonASAPOp::Aggregate {
            measures, child, ..
        } if matches!(measures.as_slice(), [AggIntent::Sum { col: None }]) => {
            raw_data_samples_of_metric(child)
        }
        _ => None,
    }
}

/// The approximate inputs a query's value composes: every sketch summary
/// state reachable from `root`, each once, when the value carries a
/// guarantee that is not exact ([`root_guarantee`]), including compositions
/// conditional on source-sign facts supplied later. Empty when neither the
/// current nor conditional contract composes the inputs, or the result is
/// exact. Plan selection sizes each for its share of failure probability too.
pub fn approximate_inputs(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
    // Sizing may depend on a source-domain proof supplied only at Stage 3.
    // Count the inputs of that conditional composition too, without attaching
    // the hypothetical proof to the candidate or accepting its accuracy.
    let guarantee = root_guarantee(root).or_else(|| {
        bind_guarantees(root, &|guarantee| {
            guarantee.bind(&|statistic| {
                crate::composition::parse_raw_data_samples_sign_statistic(statistic).map(|_| 1.0)
            })
        })
    });
    if guarantee.is_none_or(|g| g.is_exact()) {
        return Vec::new();
    }
    let operators = match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    };
    let mut seen = HashSet::new();
    operators
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .filter(|node| {
            matches!(
                node.asap(),
                Some(
                    ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(..),
                        ..
                    } | ASAPOp::PaneChain {
                        family: FieldDataType::Sketch(..),
                        ..
                    }
                )
            )
        })
        .filter(|node| seen.insert(Rc::as_ptr(node)))
        .collect()
}

/// The theoretical guarantee of a query's output, as composed: the root
/// node's own, or a scalar root's composed over the operators it reads
/// ([`scalar_guarantee`]).
pub fn root_guarantee(root: &QueryRoot) -> Option<AccuracyGuarantee> {
    match root {
        QueryRoot::Operator(node) => node.guarantee.clone(),
        QueryRoot::Scalar(expr) => scalar_guarantee(expr, &|_| None),
    }
}

/// Bind known statistics in existing contracts, then recover compositions
/// that previously required constant child bounds. Physical state contracts,
/// schemas, timing, materialization, and sharing are preserved. This does not
/// infer facts: `bind` must leave unsupported statistics unknown.
pub fn bind_guarantees(
    root: &QueryRoot,
    bind: &dyn Fn(&AccuracyGuarantee) -> AccuracyGuarantee,
) -> Option<AccuracyGuarantee> {
    fn walk(
        node: &Rc<OperatorNode>,
        bind: &dyn Fn(&AccuracyGuarantee) -> AccuracyGuarantee,
        cache: &mut NodeToRewrittenNodeCache,
    ) -> Rc<OperatorNode> {
        if let Some(done) = cache.get(&Rc::as_ptr(node)) {
            return done.clone();
        }
        let operator = node.operator.map_children(|child| walk(child, bind, cache));
        let mut rebuilt = OperatorNode::with_schema(operator, node.schema.clone())
            .with_timing(node.timing)
            .with_materialization(node.materialization);
        rebuilt.guarantee = node
            .guarantee
            .as_ref()
            .map(bind)
            .or_else(|| composed_guarantee(&rebuilt).as_ref().map(bind));
        let rebuilt = Rc::new(rebuilt);
        cache.insert(Rc::as_ptr(node), rebuilt.clone());
        rebuilt
    }
    let mut cache = HashMap::new();
    let bound = match root {
        QueryRoot::Operator(node) => QueryRoot::Operator(walk(node, bind, &mut cache)),
        QueryRoot::Scalar(expr) => {
            QueryRoot::Scalar(expr.map_operator_refs(&mut |node| walk(node, bind, &mut cache)))
        }
    };
    root_guarantee(&bound)
}

/// `roots` with every sketch summary state rebuilt with the parameters
/// `resize` gives it (same algorithm; `None` keeps them), and every node
/// above one rebuilt over its new inputs with its guarantee composed again
/// ([`composed_guarantee`]; a node that had none keeps none). Timing,
/// materialization and sharing are kept. Plan selection enlarges summaries
/// for a failure probability this way.
pub fn with_resized_summaries(
    roots: &[QueryRoot],
    resize: &dyn Fn(&OperatorNode, &SketchParams) -> Option<SketchParams>,
) -> Result<Vec<QueryRoot>, SchemaDerivationError> {
    fn walk(
        node: &Rc<OperatorNode>,
        resize: &dyn Fn(&OperatorNode, &SketchParams) -> Option<SketchParams>,
        cache: &mut NodeToRewrittenNodeCache,
    ) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
        if let Some(done) = cache.get(&Rc::as_ptr(node)) {
            return Ok(done.clone());
        }
        for child in node.children() {
            walk(child, resize, cache)?;
        }
        let mut changed = node
            .children()
            .iter()
            .any(|child| !Rc::ptr_eq(child, &cache[&Rc::as_ptr(child)]));
        let mut operator = node.operator.clone();
        if let Operator::ASAP(
            ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }
            | ASAPOp::PaneChain {
                family: FieldDataType::Sketch(kind, _),
                ..
            },
        ) = &mut operator
        {
            if let Some(params) = resize(node, kind.params()).filter(|p| p != kind.params()) {
                *kind = SketchKind::new(*kind.algorithm(), params);
                changed = true;
            }
        }
        let rebuilt = if changed {
            let template = OperatorNode {
                operator,
                ..(**node).clone()
            };
            let mut rebuilt =
                template.with_new_children(|child| cache[&Rc::as_ptr(child)].clone())?;
            rebuilt.guarantee = node
                .guarantee
                .as_ref()
                .and_then(|_| composed_guarantee(&rebuilt));
            Rc::new(
                rebuilt
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        } else {
            node.clone()
        };
        cache.insert(Rc::as_ptr(node), rebuilt.clone());
        Ok(rebuilt)
    }
    let mut cache = HashMap::new();
    roots
        .iter()
        .map(|root| {
            Ok(match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(walk(node, resize, &mut cache)?),
                QueryRoot::Scalar(expr) => {
                    let mut rebuilt = HashMap::new();
                    for node in expr.operator_refs() {
                        rebuilt.insert(Rc::as_ptr(node), walk(node, resize, &mut cache)?);
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| rebuilt[&Rc::as_ptr(node)].clone()),
                    )
                }
            })
        })
        .collect()
}

/// The one source a summary state counts the distinct items of its own
/// rows of, as [`source_distinct_guarantee`] reads it: the source whose
/// distinct bound gives its failure probability. `None` for a state over
/// computed rows or several sources.
pub fn distinct_bound_source(state: &OperatorNode) -> Option<String> {
    let Some(ASAPOp::SummaryAgg { child, .. } | ASAPOp::PaneChain { child, .. }) = state.asap()
    else {
        return None;
    };
    source_scan(child)?;
    source(child)
}

/// `roots` with every node's guarantee composed again
/// ([`composed_guarantee`]), children first; a sub-DAG shared between roots
/// stays shared. Pass 2 calls it after re-sizing a shared summary, so each
/// consumer's guarantee is the shared state's.
pub fn recompose_guarantees(roots: &[QueryRoot]) -> Result<Vec<QueryRoot>, SchemaDerivationError> {
    let mut cache = HashMap::new();
    roots
        .iter()
        .map(|root| {
            Ok(match root {
                QueryRoot::Operator(node) => {
                    QueryRoot::Operator(with_guarantees(node, &mut cache)?)
                }
                QueryRoot::Scalar(expr) => {
                    for node in expr.operator_refs() {
                        with_guarantees(node, &mut cache)?;
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| cache[&Rc::as_ptr(node)].clone()),
                    )
                }
            })
        })
        .collect()
}

/// `node` and everything below it, each node with its [`composed_guarantee`].
pub fn with_guarantees(
    node: &Rc<OperatorNode>,
    cache: &mut NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    if let Some(done) = cache.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        with_guarantees(child, cache)?;
    }
    let mut rebuilt = node.with_new_children(|child| cache[&Rc::as_ptr(child)].clone())?;
    rebuilt.guarantee = composed_guarantee(&rebuilt);
    let rebuilt = Rc::new(rebuilt);
    cache.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// An extremum over at most one row per group is an identity on its value:
/// an aggregate whose measures are all `MIN`/`MAX` over a (possibly
/// filtered) grouped estimate, where every column of a unique key of the
/// estimate is a group key or pinned to one value by the filter
/// (Pass 2's finer-aggregate same-level read). Unlike a general
/// aggregate over rank estimates, this does not change the population to
/// which the input's error bound refers.
fn selected_group_guarantee(node: &OperatorNode) -> Option<AccuracyGuarantee> {
    let NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        child,
        ..
    } = node.non_asap()?
    else {
        return None;
    };
    if by.is_without()
        || !filters.iter().all(Option::is_none)
        || measures.is_empty()
        || !measures.iter().all(|measure| {
            matches!(
                measure,
                AggIntent::Min { col: Some(_) } | AggIntent::Max { col: Some(_) }
            )
        })
    {
        return None;
    }
    let (pred, grouped) = match child.non_asap()? {
        NonASAPOp::Filter { pred, child } => (Some(&pred.0), child),
        _ => (None, child),
    };
    let pinned = pred.map_or_else(Vec::new, pinned_columns);
    // The filter may read only key columns: a predicate on an estimate
    // would decide by an approximate value which groups are kept.
    let reads = pred.map_or_else(Vec::new, ScalarExpr::columns_referenced);
    if !grouped.schema.unique_keys.iter().any(|key| {
        key.iter()
            .all(|column| by.keys().contains(column) || pinned.contains(column))
            && reads.iter().all(|column| key.contains(column))
    }) {
        return None;
    }
    let mut source = grouped;
    while let Some(NonASAPOp::Project { cols, child, .. }) = source.non_asap() {
        if !cols
            .iter()
            .enumerate()
            .all(|(index, item)| matches!(item.expr, ScalarExpr::Column(column) if column == index))
        {
            return None;
        }
        source = child;
    }
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = source.asap() else {
        return None;
    };
    let Some(ASAPOp::SummaryAgg {
        reduction: Reduction::Reduce(keys),
        child: raw,
        ..
    }) = summary_input.asap()
    else {
        return None;
    };
    if keys.is_without()
        || !raw
            .guarantee
            .as_ref()
            .is_some_and(AccuracyGuarantee::is_exact)
    {
        return None;
    }
    grouped.guarantee.clone()
}

/// The theoretical guarantee of a composed node, given its children's,
/// composed with the built-in analytical rules ([`propagate`]);
/// Stage 1 reads no data statistics or deployment evidence. Exact over exact
/// inputs; a summary estimate has its sized family's bound when
/// [`estimate_has_bound`], composed with its input's when that is
/// approximate; an exact aggregation or accumulator over approximate values
/// uses its function's rule ([`approximate_input_composition`]); arithmetic composes as
/// [`arithmetic_guarantee`]. A rule that needs a statistic leaves it
/// symbolic (`BoundExpr::Unknown`), and plan selection (Stage 3) binds it.
/// `None` where no rule applies: a comparison, a value `HAVING` or any other
/// predicate over an approximate value, a rank error through any operator (a
/// rank interval counts as none in the MVP). A projection of columns only
/// keeps its child's guarantee. A summary state carries none itself.
///
/// A guarantee holds per output point (per series or group), with its
/// failure probability per point ([`AccuracyScope::PerValue`]). Plan
/// selection states it over a whole result of `G` points as `G·δ`.
///
/// [`AccuracyScope::PerValue`]: asap_types::ir::properties::AccuracyScope::PerValue
pub(crate) fn composed_guarantee(node: &OperatorNode) -> Option<AccuracyGuarantee> {
    if let Some(guarantee) = selected_group_guarantee(node) {
        return Some(guarantee);
    }
    if let Some(guarantee) = rolled_up_guarantee(node) {
        return guarantee;
    }
    if let Some(guarantee) = merged_panes_guarantee(node) {
        return guarantee;
    }
    let exact = |node: &OperatorNode| {
        node.guarantee
            .as_ref()
            .is_some_and(AccuracyGuarantee::is_exact)
    };
    // A summary state's input rows and any subquery in its filter.
    let exact_inputs = |state: &OperatorNode| state.children().into_iter().all(|c| exact(c));
    // Over approximate input rows, a summary's filter must not decide which
    // rows count by an estimate: it may read no input column (guarantees are
    // per node, so any column may carry the estimate; column-level tracking
    // is #689), and its subqueries must be exact.
    let exact_filter = |state: &OperatorNode| {
        let Some(ASAPOp::SummaryAgg { filter, .. }) = state.asap() else {
            return false;
        };
        filter
            .as_ref()
            .is_none_or(|filter| !reads_columns(&filter.0))
            && state.children().into_iter().skip(1).all(|c| exact(c))
    };
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match child.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(..),
                ..
            }) if exact_inputs(child) => Some(AccuracyGuarantee::exact(
                "exact accumulator over exact input",
            )),
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(kind, _),
                child: input,
                reduction,
                ..
            }) if exact_filter(child) && !groups_by_estimate(reduction, input) => {
                let stats = fold_stats(reduction, input)?;
                let input = operand_guarantee(input)?;
                let op = match kind {
                    // Counts rows, not values.
                    ExactKind::Count => return Some(AccuracyGuarantee::exact("count of rows")),
                    ExactKind::Sum => CompositionOperator::ExactSum,
                    ExactKind::Min | ExactKind::Max => CompositionOperator::ExactExtremum,
                    ExactKind::Rate => CompositionOperator::CounterRate,
                    ExactKind::IRate => CompositionOperator::InstantCounterRate,
                    ExactKind::Increase => CompositionOperator::CounterIncrease,
                };
                propagate(&op, &[input], None, &stats).ok()
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
                reduction,
                ..
            }) if estimate_has_bound(family, update, query)
                || estimate_bound_over_raw_data_samples(family, update, query).is_some() =>
            {
                let local = match estimate_bound_over_raw_data_samples(family, update, query) {
                    Some(gated) => gated,
                    None => source_distinct_guarantee(family, query, input)
                        .or_else(|| local_guarantee(family, query))?,
                };
                let local = interpolation_guarantee(family, update, query, input, local)?;
                if exact_inputs(summary_input) {
                    Some(local)
                } else if exact_filter(summary_input) && !groups_by_estimate(reduction, input) {
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
        // Each step re-evaluates the child: every sample keeps its guarantee.
        Operator::NonASAP(NonASAPOp::PromqlSubquery { child, .. }) => child.guarantee.clone(),
        Operator::NonASAP(_) if node.children().into_iter().all(|child| exact(child)) => {
            Some(AccuracyGuarantee::exact("exact over exact inputs"))
        }
        // Over a join, the child's sample value may be an exact input's
        // column, so each column's own guarantee is read instead.
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) if derives_from_join(child) => {
            relational_project_guarantee(cols, child)
        }
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) => project_guarantee(cols, child),
        Operator::NonASAP(NonASAPOp::Join { .. }) => join_guarantee(node),
        // A measure filter or `HAVING` over approximate values changes
        // which rows or groups count: no rule.
        Operator::NonASAP(NonASAPOp::Aggregate {
            measures,
            filters,
            having: None,
            reduction,
            child,
            ..
        }) if filters.iter().all(Option::is_none) && !groups_by_estimate(reduction, child) => {
            match measures.as_slice() {
                [intent] => propagate(
                    &approximate_input_composition(intent)?,
                    &[operand_guarantee(child)?],
                    None,
                    &fold_stats(reduction, child)?,
                )
                .ok(),
                _ => None,
            }
        }
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
            (operand_guarantee(lhs), None),
            (operand_guarantee(rhs), None),
            Rc::ptr_eq(lhs, rhs),
        ),
        Operator::NonASAP(_) => None,
    }
}

/// The guarantee of an estimate or finalized value read from a rollup (a
/// regrouping `SummaryMerge` of one input): the same read of the input
/// states, when the family's merged error for that read is exact or the
/// same as one build's over the union (the family-capability table). The
/// groups of one build partition its rows, so a rolled-up state is a merge
/// of disjoint parts, and a family whose bound holds for one build over
/// the union keeps it: KMV, HLL and DDSketch merge into the state a direct
/// coarse build would have, KLL into a state with the same rank bound, and
/// linear sketches into the direct build's counters, whose error is
/// relative to the coarse group's total. Each guarantee holds per output
/// point, so the number of groups does not change it. None when the merged
/// readout is approximate (heap top-k, entropy) or the family has no merge.
/// `None` for any node that does not read a rollup.
fn rolled_up_guarantee(node: &OperatorNode) -> Option<Option<AccuracyGuarantee>> {
    let state = match node.asap()? {
        ASAPOp::SummaryEstimate { summary_input, .. } => summary_input,
        ASAPOp::FinalizeExactAccumulator { child } => child,
        _ => return None,
    };
    let Some(ASAPOp::SummaryMerge {
        children,
        group_by: Some(_),
        ..
    }) = state.asap()
    else {
        return None;
    };
    let [input] = children.as_slice() else {
        return Some(None);
    };
    // Kept whole, a selected state keeps the guarantee it was built with.
    let input = asap_types::ir::operator::asap::selected_from(input).unwrap_or(input);
    let family = state.asap()?.produced_state()?;
    let merged = match node.asap()? {
        ASAPOp::SummaryEstimate { query, .. } => FamilyCapability::of(family).merged_error(query),
        // An exact accumulator merged across groups is exact.
        _ => FamilyCapability::merges_groups(family).then_some(MergedError::Exact),
    };
    if !matches!(
        merged,
        Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
    ) {
        return Some(None);
    }
    let reader = OperatorNode::with_schema(
        node.operator.map_children(|child| {
            if Rc::ptr_eq(child, state) {
                Rc::clone(input)
            } else {
                Rc::clone(child)
            }
        }),
        node.schema.clone(),
    );
    Some(composed_guarantee(&reader))
}

/// The guarantee of an estimate or finalized value read from merged
/// tumbling panes ([`merged_chain`]): the guarantee of the same read of one
/// build over the merged panes' window ([`merged_window_build`]: the whole
/// chain's, or its newest panes') when the
/// family's merged error for that read is exact or the same as that build's
/// (the family-capability table); none when it is approximate (evidence
/// only, for plan selection to judge) or the family has no merge. `None` for
/// any node that does not read merged panes.
fn merged_panes_guarantee(node: &OperatorNode) -> Option<Option<AccuracyGuarantee>> {
    let (state, merged) = match node.asap()? {
        ASAPOp::SummaryEstimate {
            summary_input,
            query,
        } => {
            let family = merged_chain(summary_input)?.asap()?.produced_state()?;
            (
                summary_input,
                FamilyCapability::of(family).merged_error(query),
            )
        }
        ASAPOp::FinalizeExactAccumulator { child } => {
            let family = merged_chain(child)?.asap()?.produced_state()?;
            // An exact accumulator's merged state is exact.
            let merged = FamilyCapability::of(family).merge_inputs;
            (child, merged.map(|_| MergedError::Exact))
        }
        _ => return None,
    };
    if !matches!(
        merged,
        Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
    ) {
        return Some(None);
    }
    let chain = merged_chain(state)?;
    let Ok(mut window_build) = merged_window_build(state) else {
        return Some(None);
    };
    // The merged panes' window range over the chain's input carries the
    // guarantee composed over that input.
    let ASAPOp::PaneChain { child: input, .. } = chain.asap()? else {
        return Some(None);
    };
    if let Operator::ASAP(ASAPOp::SummaryAgg { child: range, .. }) = &mut window_build.operator {
        let mut timed = (**range).clone();
        timed.guarantee = composed_guarantee(&timed);
        debug_assert!(Rc::ptr_eq(timed.children()[0], input));
        *range = Rc::new(timed);
    }
    let window_build = Rc::new(window_build);
    let reader = OperatorNode::with_schema(
        node.operator.map_children(|child| {
            if Rc::ptr_eq(child, state) {
                Rc::clone(&window_build)
            } else {
                Rc::clone(child)
            }
        }),
        node.schema.clone(),
    );
    Some(composed_guarantee(&reader))
}

/// The guarantee of a distinct count of one instance per group over a
/// source's own rows, whose failure probability is left symbolic in the
/// source's distinct bound, which Stage 1 does not read:
/// - classic HLL's ([`hll::classic_guarantee`]);
/// - UnivMon's, sized for a distinct count ([`univmon::distinct_guarantee`]).
///
/// `None` for any other summary, or one over computed rows, whose distinct
/// items the source's bound does not bound.
pub(crate) fn source_distinct_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
    input: &Rc<OperatorNode>,
) -> Option<AccuracyGuarantee> {
    let FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) = family else {
        return None;
    };
    if *query != SketchStatistic::Cardinality {
        return None;
    }
    source_scan(input)?;
    match kind.params() {
        SketchParams::Hll {
            precision,
            relative_error,
        } => hll::classic_guarantee(*precision, *relative_error, &source(input)?),
        params @ SketchParams::UnivMon { .. } => {
            univmon::distinct_guarantee(params, &source(input)?)
        }
        _ => None,
    }
}

/// The rows one group of an exact fold over approximate `input` counts.
/// Across PromQL series (`Reduce`) they are at most the input series, an
/// input-row-count statistic left symbolic for plan selection; across SQL
/// groups they are a group-row-count statistic nothing binds; over time
/// (`PerEntity`) they are one series' samples in the window: the steps of a
/// subquery with an explicit resolution, `⌈range / resolution⌉`. `None`
/// when that count is not in the query.
pub(crate) fn fold_stats(
    reduction: &Reduction,
    input: &Rc<OperatorNode>,
) -> Option<PropagationStats> {
    let sign_statistic =
        estimates_raw_data_samples_of(input).map(|metric| raw_data_samples_sign_statistic(&metric));
    let stats = match reduction {
        // The series count of the sources read bounds a PromQL fold's rows
        // (series), not the rows of a SQL group.
        Reduction::Reduce(_) if reads_table(input) => Some(PropagationStats {
            row_count_statistic: Some(GROUP_ROW_COUNT),
            ..PropagationStats::default()
        }),
        Reduction::Reduce(_) => Some(PropagationStats::default()),
        Reduction::PerEntity => match input.non_asap() {
            Some(NonASAPOp::PromqlSubquery {
                range,
                resolution: Some(resolution),
                ..
            }) if !resolution.is_zero() => Some(PropagationStats {
                input_row_count: Some(range.as_nanos().div_ceil(resolution.as_nanos()) as u64),
                ..PropagationStats::default()
            }),
            _ => None,
        },
    }?;
    Some(PropagationStats {
        sign_statistic,
        ..stats
    })
}

/// The metric whose raw data samples, or plain sums of them, a composed node's
/// values are or estimate, or `None`: [`raw_data_samples_of_metric`]'s strict rule
/// over a composed candidate. An exact sum accumulator, a projection of
/// columns and a
/// heap estimate whose update weights carry the
/// [`WeightDomain::RawDataSamplesOfMetric`] requirement keep it, and so does a
/// quantile estimate of those values, built over the raw window or merged
/// from its tumbling panes: it is one of them, or between two of them, so it
/// shares their sign. Anything else (rate, arithmetic, other
/// functions) ends it. Only the sign of the values is read from it.
pub(crate) fn estimates_raw_data_samples_of(node: &OperatorNode) -> Option<String> {
    match &node.operator {
        Operator::NonASAP(NonASAPOp::TimeRange { child, .. }) => {
            estimates_raw_data_samples_of(child)
        }
        Operator::NonASAP(NonASAPOp::TimeShift { child, shift }) if shift.at.is_none() => {
            estimates_raw_data_samples_of(child)
        }
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match child.asap()? {
            ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
                child,
                filter: None,
                ..
            } => estimates_raw_data_samples_of(child),
            _ => None,
        },
        // Only a φ in [0, 1] picks one of the values; outside it the answer
        // is -Inf or +Inf, whatever their sign. Merged tumbling panes of one
        // definition ([`merged_chain`]) are read through their `PaneChain`:
        // a quantile of the merged state is still one of the panes'
        // samples, or between two of them.
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query: SketchStatistic::Quantile { q },
        }) if (0.0..=1.0).contains(q) => match merged_chain(summary_input)
            .unwrap_or(summary_input)
            .asap()?
        {
            ASAPOp::SummaryAgg {
                input:
                    SummaryUpdate {
                        item: None,
                        weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                        ..
                    },
                filter: None,
                child,
                ..
            }
            | ASAPOp::PaneChain {
                input:
                    SummaryUpdate {
                        item: None,
                        weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                        ..
                    },
                filter: None,
                child,
                ..
            } => estimates_raw_data_samples_of(child),
            _ => None,
        },
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
            match summary_input.asap()? {
                ASAPOp::SummaryAgg {
                    input:
                        SummaryUpdate {
                            weight_domain: WeightDomain::RawDataSamplesOfMetric { metric },
                            ..
                        },
                    filter: None,
                    ..
                } => Some(metric.clone()),
                _ => None,
            }
        }
        // Renaming, reordering or dropping columns keeps every value.
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
            if cols
                .iter()
                .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
        {
            estimates_raw_data_samples_of(child)
        }
        _ => raw_data_samples_of_metric(node),
    }
}

/// Whether `node` reads a SQL table.
fn reads_table(node: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(node).iter().any(|node| {
        matches!(
            node.non_asap(),
            Some(NonASAPOp::Scan {
                source: Source::Table { .. },
                ..
            })
        )
    })
}

/// Whether a grouping by `reduction` over an approximate `input` may decide
/// a row's group by an estimate: a grouping column other than a label (a
/// string) or the time index. Guarantees are per node, so any such column
/// may carry the estimate (column-level tracking is #689); a PromQL `by`
/// or `without` groups by labels only. `false` over exact input.
fn groups_by_estimate(reduction: &Reduction, input: &OperatorNode) -> bool {
    let Some(by) = reduction.group_keys() else {
        return false;
    };
    if input
        .guarantee
        .as_ref()
        .is_some_and(AccuracyGuarantee::is_exact)
    {
        return false;
    }
    let schema = &input.schema;
    let columns: Vec<usize> = if by.is_without() {
        let value = resolve_column_ref(&ColumnRef::SampleValue, schema).ok();
        (0..schema.fields.len())
            .filter(|i| !by.keys().contains(i) && Some(*i) != value)
            .collect()
    } else {
        by.keys().to_vec()
    };
    columns.into_iter().any(|i| {
        Some(i) != schema.time_index
            && !matches!(
                schema.fields.get(i).map(|f| &f.dtype),
                Some(FieldDataType::Plain(DataType::Utf8))
            )
    })
}

/// Whether `expr` reads any input column.
fn reads_columns(expr: &ScalarExpr) -> bool {
    let mut reads = false;
    expr.map_columns(&mut |column| {
        reads = true;
        column
    });
    reads
}

/// The built-in rule for `op`, with every statistic unknown (Stage 1 reads
/// none); `None` when no rule applies.
fn compose(
    op: &CompositionOperator,
    inputs: Vec<AccuracyGuarantee>,
    local: Option<&AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    propagate(op, &inputs, local, &PropagationStats::default()).ok()
}

/// A projection that computes over its child's sample value: the guarantee
/// of the one item that reads it ([`project_column_guarantee`]). `None` when
/// no single item reads the value.
fn project_guarantee(cols: &[ProjectItem], child: &Rc<OperatorNode>) -> Option<AccuracyGuarantee> {
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
    scalar_guarantee(&item.expr, &|column| {
        project_column_guarantee(child, column)
    })
}

/// Whether `node` is a join, or reads one only through operators that keep
/// its columns (projections, filters, sorts, limits, deduplication). The
/// columns of such a node come from different inputs, so its sample value
/// says nothing about where the error lies.
fn derives_from_join(node: &OperatorNode) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Join { .. }) => true,
        Some(
            NonASAPOp::Project { child, .. }
            | NonASAPOp::Filter { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::Dedup { child, .. },
        ) => derives_from_join(child),
        _ => false,
    }
}

/// The guarantee of each output column of `node`, where it is known: every
/// column of an exact node is exact; a join beside exact inputs
/// ([`join_guarantee`]) gives the exact inputs' columns exact and the
/// approximate input's columns its guarantee; a projection of columns maps
/// its child's. `None` anywhere else, a computed projection included.
/// Guarantees are per node, so every column of an approximate input counts
/// as approximate; column-level tracking is #689.
///
/// This assumes every approximate node of Stage 1 has an exact row set:
/// its guarantee bounds each row's values, never which rows exist (a
/// top-k estimate, whose rows are approximate, has no bound;
/// `estimate_has_bound`). A guarantee on row membership would need these
/// rules revisited.
fn column_guarantees(node: &OperatorNode) -> Option<Vec<AccuracyGuarantee>> {
    let width = node.schema.fields.len();
    if let Some(guarantee) = node.guarantee.as_ref().filter(|g| g.is_exact()) {
        return Some(vec![guarantee.clone(); width]);
    }
    match &node.operator {
        Operator::NonASAP(NonASAPOp::Join {
            kind:
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Cross,
            left,
            right,
            ..
        }) => {
            let input = join_input_guarantees(left, right)?;
            (input.len() == width).then_some(input)
        }
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) => {
            let child = column_guarantees(child)?;
            cols.iter()
                .map(|item| match item.expr {
                    ScalarExpr::Column(id) => child.get(id).cloned(),
                    _ => None,
                })
                .collect()
        }
        _ => None,
    }
}

/// The guarantees of a join's input columns (`left ++ right`) when at most
/// one input is approximate and the others are exact: the exact input's
/// columns exact, the approximate input's columns its guarantee.
fn join_input_guarantees(
    left: &OperatorNode,
    right: &OperatorNode,
) -> Option<Vec<AccuracyGuarantee>> {
    let exact = |node: &OperatorNode| node.guarantee.as_ref().is_some_and(|g| g.is_exact());
    if !exact(left) && !exact(right) {
        return None;
    }
    let side = |node: &OperatorNode| {
        let guarantee = node.guarantee.clone()?;
        Some(vec![guarantee; node.schema.fields.len()])
    };
    let mut columns = side(left)?;
    columns.extend(side(right)?);
    Some(columns)
}

/// A join whose inputs are exact but one keeps that input's guarantee when
/// its predicate reads only exact columns (a cross join, or a key equality
/// on the exact inputs' columns): which rows match, and how often, then
/// does not depend on an estimate, and each output row carries one row of
/// the approximate input unchanged. Outer joins pad on the same exact
/// decision. A semi- or anti-join, a key on an approximate input's column
/// (per-node guarantees count all its columns approximate; #689), or two
/// approximate inputs have none. An exact join is composed before this.
/// Like [`column_guarantees`], this assumes the approximate input's row set
/// is exact: only its values carry error.
fn join_guarantee(node: &OperatorNode) -> Option<AccuracyGuarantee> {
    let Operator::NonASAP(NonASAPOp::Join {
        kind: JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Cross,
        pred,
        left,
        right,
    }) = &node.operator
    else {
        return None;
    };
    let columns = join_input_guarantees(left, right)?;
    let pred_exact =
        scalar_guarantee(&pred.0, &|id| columns.get(id).cloned()).is_some_and(|g| g.is_exact());
    if !pred_exact {
        return None;
    }
    [left, right]
        .into_iter()
        .find_map(|input| input.guarantee.clone().filter(|g| !g.is_exact()))
}

/// The guarantee of a projection over relational columns
/// ([`column_guarantees`]): each output column's as [`scalar_guarantee`]
/// (a `CASE` as [`case_guarantee`]), and the node's the weakest of them.
/// `None` when the child's column guarantees are unknown or any output
/// column has none.
fn relational_project_guarantee(
    cols: &[ProjectItem],
    child: &Rc<OperatorNode>,
) -> Option<AccuracyGuarantee> {
    let columns = column_guarantees(child)?;
    let column = |id: usize| columns.get(id).cloned();
    weakest(
        cols.iter()
            .map(|item| scalar_guarantee(&item.expr, &column))
            .collect::<Option<Vec<_>>>()?,
    )
}

/// The weakest of `guarantees`: exact when all are, the approximate one
/// when there is one (or all approximate ones are equal), and otherwise
/// as an extremum of them ([`lipschitz_guarantee`]).
fn weakest(guarantees: Vec<AccuracyGuarantee>) -> Option<AccuracyGuarantee> {
    let mut approximate: Vec<AccuracyGuarantee> = Vec::new();
    for guarantee in guarantees.into_iter().filter(|g| !g.is_exact()) {
        if !approximate.contains(&guarantee) {
            approximate.push(guarantee);
        }
    }
    match approximate.len() {
        0 => Some(AccuracyGuarantee::exact("exact over exact operands")),
        1 => approximate.pop(),
        _ => lipschitz_guarantee(approximate),
    }
}

/// A `CASE` whose operand and conditions are exact picks its branch by an
/// exact decision, so it keeps the weakest branch's guarantee (a literal or
/// NULL branch, or a missing `ELSE`, is exact). A condition over an
/// approximate value has none.
fn case_guarantee(
    operand: Option<&ScalarExpr>,
    branches: &[(ScalarExpr, ScalarExpr)],
    else_expr: Option<&ScalarExpr>,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    let exact = |expr: &ScalarExpr| scalar_guarantee(expr, column).is_some_and(|g| g.is_exact());
    if !operand.is_none_or(exact) || !branches.iter().all(|(when, _)| exact(when)) {
        return None;
    }
    weakest(
        branches
            .iter()
            .map(|(_, then)| then)
            .chain(else_expr)
            .map(|expr| scalar_guarantee(expr, column))
            .collect::<Option<Vec<_>>>()?,
    )
}

/// The guarantee of `column` of a projection's `child`: over a join, the
/// column's own ([`column_guarantees`]); otherwise the child's sample
/// value has the child's as an operand ([`operand_guarantee`]) and other
/// columns (labels, timestamps) are exact. `None` when the child has no
/// sample value or the column's guarantee is unknown.
pub fn project_column_guarantee(
    child: &Rc<OperatorNode>,
    column: usize,
) -> Option<AccuracyGuarantee> {
    // Over a join, each column's own guarantee ([`column_guarantees`]).
    if derives_from_join(child) {
        return column_guarantees(child)?.get(column).cloned();
    }
    let value = resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok()?;
    if column == value {
        operand_guarantee(child)
    } else {
        Some(AccuracyGuarantee::exact(
            "column other than the sample value",
        ))
    }
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
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    let literal = |expr: &ScalarExpr| match expr {
        ScalarExpr::Literal(ScalarValue::Float64(value)) => Some(*value),
        ScalarExpr::Literal(ScalarValue::Int64(value)) => Some(*value as f64),
        _ => None,
    };
    let operand = |expr: &ScalarExpr| (operand_of(expr, column), literal(expr));
    match expr {
        ScalarExpr::Column(id) => column(*id),
        ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
            node.guarantee.clone()
        }
        ScalarExpr::Literal(_) | ScalarExpr::CurrentTimestamp | ScalarExpr::EvalTimestamp => {
            Some(AccuracyGuarantee::exact("constant"))
        }
        ScalarExpr::Negative { expr, .. } => {
            lipschitz_guarantee(vec![scalar_guarantee(expr, column)?])
        }
        ScalarExpr::Case {
            operand,
            branches,
            else_expr,
        } => case_guarantee(operand.as_deref(), branches, else_expr.as_deref(), column),
        ScalarExpr::Arithmetic {
            op, left, right, ..
        } => arithmetic_guarantee(
            op.clone(),
            operand(left),
            operand(right),
            same_value(left, right),
        ),
        ScalarExpr::FunctionCall { name, args } if is_monotone_lipschitz(name) => {
            lipschitz_guarantee(
                args.iter()
                    .map(|arg| operand_of(arg, column))
                    .collect::<Option<_>>()?,
            )
        }
        _ => {
            let mut columns = Vec::new();
            expr.map_columns(&mut |id| {
                columns.push(id);
                id
            });
            let exact = |g: Option<AccuracyGuarantee>| g.is_some_and(|g| g.is_exact());
            (columns.into_iter().all(|id| exact(column(id)))
                && expr
                    .operator_refs()
                    .into_iter()
                    .all(|node| exact(node.guarantee.clone())))
            .then(|| AccuracyGuarantee::exact("exact over exact operands"))
        }
    }
}

/// The guarantee of a scalar operand as an input to arithmetic or a
/// function: a planned operator's as [`operand_guarantee`], anything else
/// as [`scalar_guarantee`].
pub fn operand_of(
    expr: &ScalarExpr,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    match expr {
        ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
            operand_guarantee(node)
        }
        _ => scalar_guarantee(expr, column),
    }
}

/// `node`'s guarantee as an input to a computation over its values: a
/// Count-Min or Count Sketch bound restated as an absolute error over the
/// stream norm its summary's update weight allows ([`stream_norm`],
/// `additive_as_absolute`).
pub fn operand_guarantee(node: &Rc<OperatorNode>) -> Option<AccuracyGuarantee> {
    let guarantee = node.guarantee.clone()?;
    Some(additive_as_absolute(guarantee, stream_norm(node).as_ref()))
}

/// The stream norm that bounds the one Count-Min or Count Sketch summary
/// below `node`, by its update weight: the source's own norm when the
/// weight is the source's raw value column (the summary's input is the
/// source's scan, at most windowed or filtered); `|c|` times the source's
/// row count for a constant weight `c`. `None` for any other weight (a
/// computed value such as `m * 1000` or `rate(…)`), several summaries or
/// sources, or none.
pub(crate) fn stream_norm(node: &Rc<OperatorNode>) -> Option<StreamNorm> {
    let summaries: Vec<_> = OperatorNode::reachable(node)
        .into_iter()
        .filter(|node| {
            matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if matches!(
                    kind.algorithm(),
                    SketchAlgorithm::Cms
                        | SketchAlgorithm::CmsWithHeap
                        | SketchAlgorithm::CountSketch
                        | SketchAlgorithm::CountSketchWithHeap
                )
            )
        })
        .collect();
    let [summary] = summaries.as_slice() else {
        return None;
    };
    let Some(ASAPOp::SummaryAgg {
        input: update,
        child,
        ..
    }) = summary.asap()
    else {
        return None;
    };
    let source = source(child)?;
    match &update.weight {
        SummaryInputExpr::Constant(weight) => Some(StreamNorm::ConstantWeight {
            weight: *weight,
            source,
        }),
        SummaryInputExpr::Column(column) if raw_value_column(child, column) => {
            Some(StreamNorm::Source(source))
        }
        _ => None,
    }
}

/// Whether `column` of `node` is its source's raw data sample value: `node` is
/// the source's scan, at most behind filters and windows that keep its
/// schema, and `column` resolves to the scan's sample value.
fn raw_value_column(node: &Rc<OperatorNode>, column: &ColumnRef) -> bool {
    let Some(schema) = source_scan(node) else {
        return false;
    };
    let resolved = resolve_column_ref(column, schema).ok();
    resolved.is_some() && resolved == resolve_column_ref(&ColumnRef::SampleValue, schema).ok()
}

/// The scan's schema when `node` is a source's scan, at most behind filters
/// and windows that keep its schema: its rows are the source's own.
fn source_scan(node: &Rc<OperatorNode>) -> Option<&Schema> {
    let mut current = node;
    loop {
        if current.schema != node.schema {
            return None;
        }
        match current.non_asap() {
            Some(NonASAPOp::Filter { child, .. } | NonASAPOp::TimeRange { child, .. }) => {
                current = child
            }
            Some(NonASAPOp::Scan { schema, .. }) => return Some(schema),
            _ => return None,
        }
    }
}

/// The one source (PromQL metric or SQL table) `node` reads; `None` when it
/// reads several or none.
fn source(node: &Rc<OperatorNode>) -> Option<String> {
    let sources: HashSet<String> = OperatorNode::reachable(node)
        .iter()
        .filter_map(|node| match node.non_asap() {
            Some(NonASAPOp::Scan { source, .. }) => Some(match source {
                Source::TimeSeries { metric } => metric.clone(),
                Source::Table { table_ref } => table_ref.clone(),
            }),
            _ => None,
        })
        .collect();
    let mut sources = sources.into_iter();
    match (sources.next(), sources.next()) {
        (Some(source), None) => Some(source),
        _ => None,
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
fn lipschitz_guarantee(args: Vec<AccuracyGuarantee>) -> Option<AccuracyGuarantee> {
    let approximate: Vec<_> = args
        .into_iter()
        .filter(|g| !g.is_exact())
        .map(|g| additive_as_absolute(g, None))
        .collect();
    if approximate.is_empty() {
        return Some(AccuracyGuarantee::exact(
            "Lipschitz function of exact operands",
        ));
    }
    propagate(
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
type Operand = (Option<AccuracyGuarantee>, Option<f64>);

/// The guarantee of `left op right` (#669, MVP). Count-Min and Count Sketch
/// bounds count as absolute errors (normalized bound times stream norm).
/// - `x ± y`: absolute errors add (union bound on the failure
///   probabilities); `x ± c` (`c` exact) keeps an absolute error.
/// - `c · x`, `x / c` (`c` a nonzero literal): an absolute error scales by
///   `|c|` (or `1/|c|`); a relative error is kept, as it is by `x · y` or
///   `x / y` with `y` exact.
/// - `x · y` of two relative errors: relative `a + b + a·b`.
/// - `x / y` with `y` a relative error and `x` a relative error or exact:
///   relative `(a + b)/(1 − b)` (`a = 0` for an exact `x`), none when
///   `b ≥ 1`.
/// - `x · y` of two other estimates, `x / y` with any other approximate
///   `y`: a bound that needs enforced operand domains, left symbolic
///   ([`operand_domain_statistic`]) for plan selection to bind.
/// - Everything else (a relative error shifted by a constant or added to
///   another, a rank error, `%`, `^`, `atan2`) has none.
///
/// `same_operand`: both sides are one estimate (the same planned operator),
/// which fails or holds as one event, so its failure probability counts
/// once. Two estimates count once each, even over one shared summary: the
/// built-in guarantees hold per estimate, not simultaneously.
fn arithmetic_guarantee(
    op: ArithmeticOpKind,
    left: Operand,
    right: Operand,
    same_operand: bool,
) -> Option<AccuracyGuarantee> {
    let (l, r) = (
        additive_as_absolute(left.0?, None),
        additive_as_absolute(right.0?, None),
    );
    let once = same_operand.then(|| l.spec.failure_probability.clone());
    let statistic = operand_domain_statistic(&op, &l, &r);
    let composed = match (op, l.is_exact(), r.is_exact()) {
        (_, true, true) => return Some(AccuracyGuarantee::exact("arithmetic over exact operands")),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, true, false) => shifted(r),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, false, true) => shifted(l),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, false, false) => compose(
            &CompositionOperator::Lipschitz { constant: 1.0 },
            vec![l],
            Some(&r),
        ),
        (ArithmeticOpKind::Mul, true, false) => scaled(r, left.1.map(f64::abs)),
        (ArithmeticOpKind::Mul, false, true) => scaled(l, right.1.map(f64::abs)),
        (ArithmeticOpKind::Mul, false, false) if statistic.is_none() => relative_product(l, r),
        (ArithmeticOpKind::Div, false, true) => match right.1 {
            Some(0.0) => None,
            c => scaled(l, c.map(|c| 1.0 / c.abs())),
        },
        (ArithmeticOpKind::Div, _, false) if statistic.is_none() => propagate(
            &CompositionOperator::ExactDivision,
            &[l, r],
            None,
            &PropagationStats::default(),
        )
        .ok(),
        (ArithmeticOpKind::Mul | ArithmeticOpKind::Div, _, _) => symbolic(l, r, statistic?),
        _ => None,
    }?;
    let mut composed = composed;
    if let Some(failure_probability) = once {
        composed.spec.failure_probability = failure_probability;
    }
    Some(composed)
}

/// The statistic a product or quotient of `l` and `r` leaves symbolic on
/// its operands' domains, when it is one whose bound needs them: a product
/// of two estimates, other than of two relative errors
/// ([`PRODUCT_OPERAND_DOMAINS`]); a quotient with an approximate
/// denominator, other than of relative or exact over relative
/// (`QUOTIENT_OPERAND_DOMAINS`). `None` for any other operation (a
/// product or quotient of relative errors has a closed form), or when an
/// operand's error has no value form (a rank error). Plan selection
/// (Stage 3) finds the operations to bind with this same rule.
pub fn operand_domain_statistic(
    op: &ArithmeticOpKind,
    l: &AccuracyGuarantee,
    r: &AccuracyGuarantee,
) -> Option<&'static str> {
    let valued = |g: &AccuracyGuarantee| {
        g.is_exact()
            || matches!(
                g.spec.accuracy_metric,
                AccuracyMetric::ABSOLUTE | AccuracyMetric::Relative
            )
    };
    let relative =
        |g: &AccuracyGuarantee| g.is_exact() || g.spec.accuracy_metric == AccuracyMetric::Relative;
    if !(valued(l) && valued(r)) {
        return None;
    }
    match op {
        ArithmeticOpKind::Mul
            if !l.is_exact()
                && !r.is_exact()
                && !(l.spec.accuracy_metric == AccuracyMetric::Relative
                    && r.spec.accuracy_metric == AccuracyMetric::Relative) =>
        {
            Some(PRODUCT_OPERAND_DOMAINS)
        }
        ArithmeticOpKind::Div if !r.is_exact() && !(relative(l) && relative(r)) => {
            Some(QUOTIENT_OPERAND_DOMAINS)
        }
        _ => None,
    }
}

/// A product of two relative-error estimates: within relative
/// `(1 + a)(1 + b) − 1 = a + b + a·b`, whatever the signs. `None` for any
/// other pair.
fn relative_product(l: AccuracyGuarantee, r: AccuracyGuarantee) -> Option<AccuracyGuarantee> {
    if l.spec.accuracy_metric != AccuracyMetric::Relative
        || r.spec.accuracy_metric != AccuracyMetric::Relative
    {
        return None;
    }
    let bound = BoundExpr::Sum {
        terms: vec![
            l.spec.error.clone(),
            r.spec.error.clone(),
            BoundExpr::Product {
                factors: vec![l.spec.error.clone(), r.spec.error.clone()],
            },
        ],
    };
    Some(binary_guarantee(
        AccuracyMetric::Relative,
        bound,
        l,
        r,
        None,
        CompositionOperator::ExactProduct,
        "relative_product_union_bound",
    ))
}

/// `x` times an exact factor (`Some(|c|)` when it is a literal): a
/// relative error is kept; an absolute error scales by `|c|`.
fn scaled(input: AccuracyGuarantee, factor: Option<f64>) -> Option<AccuracyGuarantee> {
    match (input.spec.accuracy_metric, factor) {
        (AccuracyMetric::Relative, _) => Some(input),
        (_, Some(constant)) => compose(
            &CompositionOperator::Lipschitz { constant },
            vec![input],
            None,
        ),
        _ => None,
    }
}

/// `x` plus an exact value: an absolute error is kept.
fn shifted(input: AccuracyGuarantee) -> Option<AccuracyGuarantee> {
    compose(
        &CompositionOperator::Lipschitz { constant: 1.0 },
        vec![input],
        None,
    )
}

/// A product or quotient whose bound needs its operands' domains, which
/// Stage 1 does not read: the bound is the symbolic `statistic`
/// ([`operand_domain_statistic`]), absolute, with the union bound on the
/// failure probability.
fn symbolic(
    l: AccuracyGuarantee,
    r: AccuracyGuarantee,
    statistic: &'static str,
) -> Option<AccuracyGuarantee> {
    let (operator, rule) = match statistic {
        QUOTIENT_OPERAND_DOMAINS => (
            CompositionOperator::ExactDivision,
            "absolute_division_needs_operand_domains",
        ),
        _ => (
            CompositionOperator::ExactProduct,
            "absolute_product_needs_operand_domains",
        ),
    };
    Some(binary_guarantee(
        AccuracyMetric::ABSOLUTE,
        BoundExpr::Unknown {
            statistic: statistic.into(),
        },
        l,
        r,
        Some(statistic),
        operator,
        rule,
    ))
}

/// A guarantee over two operands with the union bound on their failure
/// probabilities, and its provenance.
fn binary_guarantee(
    metric: AccuracyMetric,
    bound: BoundExpr,
    l: AccuracyGuarantee,
    r: AccuracyGuarantee,
    unavailable: Option<&str>,
    operator: CompositionOperator,
    rule: &str,
) -> AccuracyGuarantee {
    let failure_probability = ProbabilityExpr::UnionBound {
        terms: vec![
            l.spec.failure_probability.clone(),
            r.spec.failure_probability.clone(),
        ],
    };
    let provenance = [l, r]
        .into_iter()
        .enumerate()
        .map(|(input_index, g)| GuaranteeSource::ChildGuarantee {
            input_index,
            guarantee: Box::new(g),
        })
        .chain(
            unavailable.map(|statistic| GuaranteeSource::UnavailableStatistic {
                statistic: statistic.into(),
            }),
        )
        .chain([GuaranteeSource::CompositionStep {
            operator,
            rule: rule.into(),
        }])
        .collect();
    AccuracyGuarantee::new(metric, bound, failure_probability, provenance)
}

/// Whether the default accuracy model's bound for `family` answers `query`
/// over `update`. A heap sketch's bound is per item frequency, not Top-K
/// membership; a Count-Min bound assumes non-negative weights.
pub(crate) fn estimate_has_bound(
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
        && !(count_min && !matches!(update.weight_domain, WeightDomain::NonNegative { .. }))
}

/// DDS quantiles use linear interpolation of neighbouring representatives.
/// Relative error survives that convex combination only without cancellation.
/// Endpoints select an extremum; interior quantiles need a sign proof. Keep a
/// raw metric's requirement symbolic so Stage 3 can bind declared source facts.
fn interpolation_guarantee(
    family: &FieldDataType,
    update: &SummaryUpdate,
    query: &SketchStatistic,
    input: &OperatorNode,
    mut guarantee: AccuracyGuarantee,
) -> Option<AccuracyGuarantee> {
    let interpolates = matches!(family, FieldDataType::Sketch(kind, _)
        if *kind.algorithm() == SketchAlgorithm::DDSketch)
        && matches!(query, SketchStatistic::Quantile { q } if *q > 0.0 && *q < 1.0);
    if !interpolates || matches!(update.weight_domain, WeightDomain::NonNegative { .. }) {
        return Some(guarantee);
    }
    let metric = match &update.weight_domain {
        WeightDomain::RawDataSamplesOfMetric { metric } => metric.clone(),
        _ if matches!(
            update.weight,
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        ) =>
        {
            estimates_raw_data_samples_of(input)?
        }
        _ => return None,
    };
    guarantee.spec.error = asap_types::ir::properties::BoundExpr::Product {
        factors: vec![
            guarantee.spec.error,
            asap_types::ir::properties::BoundExpr::Unknown {
                statistic: raw_data_samples_sign_statistic(&metric),
            },
        ],
    };
    Some(guarantee)
}

/// A Count-Min estimate whose weights carry the open requirement that they
/// are raw data samples of one metric ([`WeightDomain::RawDataSamplesOfMetric`]): its local
/// guarantee with the bound gated on that metric's sign
/// ([`raw_data_samples_sign_statistic`]), which plan selection binds to 1 when the
/// metric is declared a counter. `None` for any other estimate.
pub(crate) fn estimate_bound_over_raw_data_samples(
    family: &FieldDataType,
    update: &SummaryUpdate,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    let WeightDomain::RawDataSamplesOfMetric { metric } = &update.weight_domain else {
        return None;
    };
    let proven = SummaryUpdate {
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::CounterRawDataSamples,
        },
        ..update.clone()
    };
    if !estimate_has_bound(family, &proven, query) {
        return None;
    }
    let mut local = local_guarantee(family, query)?;
    local.spec.error = asap_types::ir::properties::BoundExpr::Product {
        factors: vec![
            local.spec.error,
            asap_types::ir::properties::BoundExpr::Unknown {
                statistic: raw_data_samples_sign_statistic(metric),
            },
        ],
    };
    Some(local)
}

pub fn split_and(expr: &ScalarExpr, out: &mut Vec<ScalarExpr>) {
    match expr {
        ScalarExpr::BoolAnd(items) => items.iter().for_each(|item| split_and(item, out)),
        other => out.push(other.clone()),
    }
}

/// The columns `pred` pins to one literal value each: its conjuncts
/// `c = v` and `c IN (v)`.
fn pinned_columns(pred: &ScalarExpr) -> Vec<usize> {
    let mut conjuncts = Vec::new();
    split_and(pred, &mut conjuncts);
    conjuncts.iter().filter_map(pinned_column).collect()
}

/// The column `conjunct` pins to one literal value (`c = v`, `c IN (v)`).
pub fn pinned_column(conjunct: &ScalarExpr) -> Option<usize> {
    match conjunct {
        ScalarExpr::Compare {
            left,
            op: CompareOpKind::Eq,
            right,
            ..
        } => match (&**left, &**right) {
            (ScalarExpr::Column(column), ScalarExpr::Literal(_))
            | (ScalarExpr::Literal(_), ScalarExpr::Column(column)) => Some(*column),
            _ => None,
        },
        ScalarExpr::InList {
            expr,
            list,
            negated: false,
        } => match (&**expr, list.as_slice()) {
            (ScalarExpr::Column(column), [ScalarExpr::Literal(_)]) => Some(*column),
            _ => None,
        },
        _ => None,
    }
}
