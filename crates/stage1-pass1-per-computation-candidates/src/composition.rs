//! Composing one whole-workload candidate DAG from one per-computation
//! candidate per target (Algorithm 1's `SUBSTITUTE`): test support only.
//! Planning adds each way to compute to the memo instead (#1207 PR 3
//! deletes this).
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::operator_properties::{BinaryOpKind, JoinKind, Reduction};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::column_resolution::ResolveError;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::topk::ranked_column;
use asap_types::ir::schema::{ColumnId, Field};
use asap_types::ir::schema::{DataType, Schema};
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, FieldDataType, NonNegativeWeightProof, SketchAlgorithm,
    SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, OperatorResultKind,
    Predicate, ProjectItem, QueryRoot, ScalarExpr,
};

use asap_accuracy::propagation::{raw_data_samples_of_metric, with_guarantees};
use asap_accuracy::sizing::sketch_statistic;

use asap_types::stages::{ComputationTarget, HavingPlacement, HowToComputeAggregateMeasure};

use super::per_computation_candidates::*;
/// `update` with the open requirement that its weights are samples of
/// `metric` ([`WeightDomain::RawDataSamplesOfMetric`]), when it is a keyed heap
/// update whose weights are not otherwise proven.
fn with_raw_data_samples_requirement(
    mut update: SummaryUpdate,
    metric: Option<&str>,
) -> SummaryUpdate {
    if let Some(metric) = metric {
        if update.item.is_some() && update.weight_domain == WeightDomain::UnknownOrSigned {
            update.weight_domain = WeightDomain::RawDataSamplesOfMetric {
                metric: metric.to_string(),
            };
        }
    }
    update
}

/// Build one whole-workload candidate (end of Pass 1): `choice[i]` picks
/// `inventory.targets[i]`'s candidate, or past its own candidates its fused
/// computation's ([`ComputationTarget::computation_for`]). An aggregate with
/// a chosen candidate other than `Exact` is replaced measure by measure: an
/// exact accumulator by `SummaryAgg` → `FinalizeExactAccumulator`, a summary
/// by `SummaryAgg` → `SummaryEstimate`, `avg` by its `sum` and `count`
/// accumulators divided, a fused computation by one state over the absorbed
/// target's input, and any other measure by its own single-measure
/// aggregate; several measures are joined back on the group keys. A summary
/// is sized here for its measure's accuracy requirement. A measure filter
/// becomes `SummaryAgg.filter`;
/// `HAVING` is applied as its target records. A sub-DAG shared between roots
/// stays shared. Every node carries the guarantee `asap_accuracy::propagation::composed_guarantee`
/// gives it.
pub fn compose_logical_candidate(
    inventory: &PerComputationCandidates,
    choice: &[usize],
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    compose(inventory, &inventory.roots, choice)
}

/// [`compose_logical_candidate`] of `roots` (some of `inventory.roots`),
/// each summary sized for its measure's whole budget.
fn compose(
    inventory: &PerComputationCandidates,
    roots: &[QueryRoot],
    choice: &[usize],
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    if choice.len() != inventory.targets.len() {
        return Err(PerComputationCandidateError::InvalidChoice);
    }
    let mut chosen: Chosen = HashMap::new();
    for (target, &index) in inventory.targets.iter().zip(choice) {
        let (candidate, absorbs) = target
            .computation_for(index)
            .ok_or(PerComputationCandidateError::InvalidChoice)?;
        chosen
            .entry(Rc::as_ptr(inventory.aggregate(target)))
            .or_default()
            .push(ChosenComputation {
                target,
                candidate,
                fused: absorbs.is_some(),
            });
    }
    // Consumer `Filter`s whose key-only `HAVING` moves below a substituted
    // aggregate: each is replaced by its input.
    let pushed_below: HashSet<_> = chosen
        .values()
        .filter(|targets| substituted(targets))
        .flat_map(|targets| &targets[0].target.having_filters)
        .filter(|entry| entry.placement == HavingPlacement::BelowAggregate)
        .filter_map(|entry| entry.filter.as_ref().map(Rc::as_ptr))
        .collect();
    compose_roots(roots, &chosen, &pushed_below, &Sizing::default())
}

/// One measure of an aggregate: the aggregate and the measure's index.
type MeasureKey = (*const OperatorNode, usize);

/// How [`compose_logical_candidate`] sizes summaries in one composition.
#[derive(Default)]
struct Sizing {
    /// Every sketch summary state built, with the measure it sizes.
    built: std::cell::RefCell<Vec<(Rc<OperatorNode>, MeasureKey)>>,
}

/// Every root of `roots` with the chosen candidates substituted, each
/// summary sized as `sizing` says, and every node with its guarantee.
fn compose_roots(
    roots: &[QueryRoot],
    chosen: &Chosen<'_>,
    pushed_below: &HashSet<*const OperatorNode>,
    sizing: &Sizing,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    let (mut cache, mut guaranteed) = (HashMap::new(), HashMap::new());
    let mut rewrite = |node: &Rc<OperatorNode>| {
        let node = rewrite(node, chosen, pushed_below, sizing, &mut cache)?;
        with_guarantees(&node, &mut guaranteed).map_err(PerComputationCandidateError::from)
    };
    let roots = roots
        .iter()
        .map(|root| {
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
            Ok(root)
        })
        .collect::<Result<Vec<_>, PerComputationCandidateError>>()?;
    // The built states, as the roots hold them: with their guarantees.
    for (state, _) in sizing.built.borrow_mut().iter_mut() {
        if let Some(with_guarantee) = guaranteed.get(&Rc::as_ptr(state)) {
            *state = with_guarantee.clone();
        }
    }
    Ok(roots)
}

/// One target's chosen computation in a whole-workload choice.
struct ChosenComputation<'a> {
    /// The target whose measure it computes.
    target: &'a ComputationTarget,
    /// How it is computed.
    candidate: &'a HowToComputeAggregateMeasure,
    /// Whether the candidate is the target's fused computation's, which
    /// absorbs the targets beneath; `false` for one of its own candidates.
    fused: bool,
}

/// Each aggregate's targets with their chosen computations.
type Chosen<'a> = HashMap<*const OperatorNode, Vec<ChosenComputation<'a>>>;
type NodeToRewrittenNodeCache = HashMap<*const OperatorNode, Rc<OperatorNode>>;

/// Whether any measure of the aggregate is substituted by something other than
/// its plain exact computation.
fn substituted(targets: &[ChosenComputation<'_>]) -> bool {
    targets
        .iter()
        .any(|chosen| *chosen.candidate != HowToComputeAggregateMeasure::Exact)
}

fn rewrite(
    node: &Rc<OperatorNode>,
    chosen: &Chosen<'_>,
    pushed_below: &HashSet<*const OperatorNode>,
    sizing: &Sizing,
    cache: &mut NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = cache.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        rewrite(child, chosen, pushed_below, sizing, cache)?;
    }
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &cache[&Rc::as_ptr(child)]));
    let rebuilt = match (chosen.get(&Rc::as_ptr(node)), node.non_asap()) {
        (Some(targets), _) if substituted(targets) => substitute(node, targets, sizing, cache)?,
        (_, Some(NonASAPOp::Filter { child, .. })) if pushed_below.contains(&Rc::as_ptr(node)) => {
            cache[&Rc::as_ptr(child)].clone()
        }
        _ if changed => Rc::new(node.with_new_children(|child| cache[&Rc::as_ptr(child)].clone())?),
        _ => node.clone(),
    };
    cache.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// Replace an aggregate whose measures are `targets`' (or exact, for a
/// measure that is no target) by its substitution, with the same output
/// columns.
fn substitute(
    target: &Rc<OperatorNode>,
    targets: &[ChosenComputation<'_>],
    sizing: &Sizing,
    cache: &NodeToRewrittenNodeCache,
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
    let having = &targets[0].target.having_filters;
    // Operator refs in a predicate (a subquery) are this aggregate's
    // children, already rewritten.
    let rewritten = |pred: &Predicate| {
        Predicate(
            pred.0
                .map_operator_refs(&mut |node| cache[&Rc::as_ptr(node)].clone()),
        )
    };
    // The metric whose raw data samples, or plain sums of them, are every
    // value the aggregate reads: a heap-sketch weight read from them carries
    // the open requirement `WeightDomain::RawDataSamplesOfMetric`, which
    // plan selection confirms.
    let raw_data_samples = raw_data_samples_of_metric(child);
    let mut input = cache[&Rc::as_ptr(child)].clone();
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
            // `fused`: the candidate is the target's fused computation's,
            // which absorbs the target beneath.
            let (candidate, raw_data_samples_of, fused) = targets
                .iter()
                .find(|chosen| chosen.target.measure_index == measure)
                .map_or(
                    (&HowToComputeAggregateMeasure::Exact, None, false),
                    |chosen| (chosen.candidate, raw_data_samples.as_deref(), chosen.fused),
                );
            let filter = filters.get(measure).and_then(Option::as_ref).map(rewritten);
            let key = (Rc::as_ptr(target), measure);
            if fused {
                return match candidate {
                    HowToComputeAggregateMeasure::ExactAccumulator(_) => nested_sum(target, cache),
                    HowToComputeAggregateMeasure::Summary(algorithm) => whole_expression(
                        target,
                        intent,
                        algorithm,
                        raw_data_samples_of,
                        (sizing, key),
                        cache,
                    ),
                    HowToComputeAggregateMeasure::Exact => {
                        Err(PerComputationCandidateError::Unsupported(
                            "plain exact execution of a fused computation",
                        ))
                    }
                };
            }
            substitute_measure(
                target,
                intent,
                candidate,
                filter,
                &input,
                raw_data_samples_of,
                (sizing, key),
            )
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
pub(crate) fn with_names_of(
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
    // A top-k's ranked rows may carry their item columns after the
    // aggregate's ([`ranked_as_topk`]).
    if names(&output).starts_with(&names(target)) {
        return Ok(with_aggregate_qualifiers(target, output));
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
    Ok(with_aggregate_qualifiers(
        target,
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols,
            qualifier: None,
            child: output,
        }))?,
    ))
}

/// An equivalent aggregate replacement must retain SQL field identity. These
/// explicit qualifiers survive child rebinding; types and keys remain derived
/// from executable operators rather than copied from the requested output.
fn with_aggregate_qualifiers(target: &OperatorNode, output: Rc<OperatorNode>) -> Rc<OperatorNode> {
    if output
        .schema
        .fields
        .iter()
        .zip(&target.schema.fields)
        .all(|(field, expected)| field.table == expected.table)
    {
        return output;
    }
    let mut output = output.as_ref().clone();
    for (field, expected) in output.schema.fields.iter_mut().zip(&target.schema.fields) {
        field.table = expected.table.clone();
    }
    Rc::new(output)
}

/// A whole-expression top-k for `target` ([`whole_expression_input`]): one
/// heap summary of `algorithm` over the absorbed aggregate's rewritten
/// input, partitioned as `target`.
fn whole_expression(
    target: &OperatorNode,
    intent: &AggIntent,
    algorithm: &SketchAlgorithm,
    raw_data_samples_of: Option<&str>,
    (sizing, key): (&Sizing, MeasureKey),
    cache: &NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (input, update, partitions) = whole_expression_input(target).ok_or(
        PerComputationCandidateError::Unsupported("whole-expression top-k input"),
    )?;
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: cache[&Rc::as_ptr(&input)].clone(),
        input: with_raw_data_samples_requirement(update, raw_data_samples_of),
        summary_type: FieldDataType::Sketch(*algorithm),
        reduction: partitions,
        filter: None,
    }))?;
    sizing.built.borrow_mut().push((state.clone(), key));
    let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query: sketch_statistic(intent).ok_or(PerComputationCandidateError::Unsupported(
            "sketch evaluation",
        ))?,
    }))?;
    ranked_as_topk(target, estimate)
}

/// A heap's top-k `estimate` (`[partitions.., items.., topk]`, one row per
/// ranked item) in `target`'s columns: its `[keys.., topk_k]`, with
/// `topk_k` the estimated Float64 weight, then the item columns. This is
/// the exact top-k's physical shape (its ranked rows' value, then the
/// columns naming each ranked item: PromQL labels, SQL group keys), so
/// consumers of `[keys.., topk_k]` find those columns where they were.
fn ranked_as_topk(
    target: &OperatorNode,
    estimate: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate { reduction, .. }) = target.non_asap() else {
        return Err(PerComputationCandidateError::Unsupported(
            "top-k target is not an aggregate",
        ));
    };
    let ranked_index = reduction.group_keys().map_or(0, |keys| keys.len());
    let fields = &estimate.schema.fields;
    let estimated = fields.len() - 1;
    let cols = target
        .schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let source = if index == ranked_index {
                estimated
            } else {
                fields[..estimated]
                    .iter()
                    .position(|input| input.name == field.name)
                    .ok_or(PerComputationCandidateError::Unsupported(
                        "top-k estimate without a ranked-row identity column",
                    ))?
            };
            Ok(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(source),
            })
        })
        .collect::<Result<Vec<_>, PerComputationCandidateError>>()?;
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: estimate,
        },
    ))?)
}

/// A nested-sum merge for `target` ([`nested_sum_input`]): one exact `Sum`
/// accumulator over the absorbed sum's rewritten input, grouped as
/// `target`, finalized.
fn nested_sum(
    target: &OperatorNode,
    cache: &NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (rows, intent, reduction) = nested_sum_input(target).ok_or(
        PerComputationCandidateError::Unsupported("nested-sum merge input"),
    )?;
    let state = exact_accumulator(
        ExactKind::Sum,
        &intent,
        &cache[&Rc::as_ptr(&rows)],
        reduction,
    )?;
    Ok(OperatorNode::new_shared(Operator::ASAP(
        ASAPOp::FinalizeExactAccumulator { child: state },
    ))?)
}

/// An unfiltered exact accumulator of `kind` for `intent` over `input`,
/// grouped by `reduction`.
pub(crate) fn exact_accumulator(
    kind: ExactKind,
    intent: &AggIntent,
    input: &Rc<OperatorNode>,
    reduction: Reduction,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let family = FieldDataType::exact(kind);
    let read = measure_input(intent, input, None)?;
    Ok(OperatorNode::new_shared(Operator::ASAP(
        ASAPOp::SummaryAgg {
            input: summary_update(intent, &family, &reduction, &input.schema, read)?,
            child: Rc::clone(input),
            summary_type: family,
            reduction,
            filter: None,
        },
    ))?)
}

/// One measure of `target` over `input`: its group keys, then its value.
fn substitute_measure(
    target: &OperatorNode,
    intent: &AggIntent,
    candidate: &HowToComputeAggregateMeasure,
    filter: Option<Predicate>,
    input: &Rc<OperatorNode>,
    raw_data_samples_of: Option<&str>,
    (sizing, key): (&Sizing, MeasureKey),
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        output_names,
        ..
    }) = target.non_asap()
    else {
        unreachable!("substitute checked the aggregate")
    };
    let state = |family: FieldDataType, intent: &AggIntent, read: SummaryInputExpr| {
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::SummaryAgg {
                child: input.clone(),
                input: with_raw_data_samples_requirement(
                    summary_update(intent, &family, reduction, &input.schema, read)?,
                    raw_data_samples_of,
                ),
                summary_type: family,
                reduction: reduction.clone(),
                filter: filter.clone(),
            },
        ))?)
    };
    let finalized = |kind: &ExactKind, intent: &AggIntent, read| {
        let state = state(FieldDataType::exact(*kind), intent, read)?;
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator { child: state },
        ))?)
    };
    Ok(match candidate {
        HowToComputeAggregateMeasure::Exact => {
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                child: input.clone(),
                reduction: reduction.clone(),
                measures: vec![intent.clone()],
                // One name per measure, as the target names this one (an
                // empty name is the intent's synthetic one).
                output_names: vec![output_names.get(key.1).cloned().unwrap_or_default()],
                filters: vec![filter.clone()],
                having: None,
            }))?
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) if kinds.len() == 1 => {
            let read = measure_input(intent, input, filter.as_ref())?;
            finalized(&kinds[0], intent, read)?
        }
        HowToComputeAggregateMeasure::Summary(algorithm) => {
            let family = FieldDataType::Sketch(*algorithm);
            let summary_input = state(
                family,
                intent,
                measure_input(intent, input, filter.as_ref())?,
            )?;
            sizing.built.borrow_mut().push((summary_input.clone(), key));
            let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query: sketch_statistic(intent).ok_or(
                    PerComputationCandidateError::Unsupported("sketch evaluation"),
                )?,
            }))?;
            match intent {
                AggIntent::TopK { .. } => ranked_as_topk(target, estimate)?,
                _ => estimate,
            }
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) => {
            let (AggIntent::Avg { col }, [ExactKind::Sum, ExactKind::Count]) =
                (intent, kinds.as_slice())
            else {
                return Err(PerComputationCandidateError::Unsupported(
                    "several exact states other than avg's sum then count",
                ));
            };
            let sum_intent = AggIntent::Sum { col: *col };
            // The count counts the averaged column, as `avg` does: it is
            // non-null (a nullable input prunes this candidate), so it counts
            // the rows the sum reads.
            let read = measure_input(&sum_intent, input, filter.as_ref())?;
            let sum = finalized(&ExactKind::Sum, &sum_intent, read.clone())?;
            let count = finalized(&ExactKind::Count, &AggIntent::Count, read)?;
            if input.result_kind == OperatorResultKind::Relation {
                return sql_avg(reduction, filter.is_some(), sum, count);
            }
            // `avg` is Float64 whatever the summed type, as `Avg`'s output is,
            // and a PromQL binary operator reads Float64 sample values. A
            // grouped sum or count ends with its value, cast when not Float64
            // (a count is Int64). A per-series sum keeps the input's labels,
            // so its value is the sample-value column; a per-series count is
            // Float64.
            let (sum, count) = match reduction {
                Reduction::Reduce(by) => (
                    last_as_float64(by.len(), sum)?,
                    last_as_float64(by.len(), count)?,
                ),
                Reduction::PerEntity => {
                    let value = resolve_column_ref(&ColumnRef::SampleValue, &sum.schema)
                        .ok()
                        .and_then(|column| sum.schema.fields.get(column));
                    if value.map(|field| &field.dtype)
                        != Some(&FieldDataType::Plain(DataType::Float64))
                    {
                        return Err(PerComputationCandidateError::Unsupported(
                            "non-Float64 per-series avg",
                        ));
                    }
                    (sum, count)
                }
            };
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

/// `node`, a grouped aggregate's `keys` group keys and then its value, with
/// the value cast to Float64 unless it already is one.
fn last_as_float64(
    keys: usize,
    node: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if node.schema.fields.last().map(|field| &field.dtype)
        == Some(&FieldDataType::Plain(DataType::Float64))
    {
        return Ok(node);
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(keys)),
            to: DataType::Float64,
            try_cast: false,
        },
    });
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: node,
        },
    ))?)
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

/// What a measure's state reads from each row of `input`, resolvable there.
/// A SQL `count` reads [`counted_column`]; any other measure, a PromQL count
/// included, reads its input column (the sample value when it names none).
fn measure_input(
    intent: &AggIntent,
    input: &Rc<OperatorNode>,
    filter: Option<&Predicate>,
) -> Result<SummaryInputExpr, PerComputationCandidateError> {
    let schema = &input.schema;
    let read = match intent {
        AggIntent::Count if input.result_kind == OperatorResultKind::Relation => {
            SummaryInputExpr::Column(counted_column(schema, filter)?)
        }
        // A top-k ranks its child's aggregate output, not a column of its own.
        AggIntent::TopK { .. } => SummaryInputExpr::Column(
            ranked_value(input)
                .map_err(|_| {
                    PerComputationCandidateError::Unsupported("Top-K has no ranked column")
                })?
                .1,
        ),
        _ => summarised_input(intent, schema).ok_or(PerComputationCandidateError::Unsupported(
            "input column outside child schema",
        ))?,
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
    let reference = |id: usize| {
        schema
            .fields
            .get(id)
            .map(asap_types::ir::schema::Field::column_ref)
    };
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
        FieldDataType::Sketch(kind) => Some(kind),
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
                asap_types::ir::schema::topk::series_identity_item(child, &groups)
            } else if child.closed {
                // A closed child (an aggregate's output, say) lists every
                // column: the item is its columns outside the partition and
                // the ranked value, as the legacy current-series and counter
                // rules rank them.
                let SummaryInputExpr::Column(ranked) = &weight else {
                    return Err(PerComputationCandidateError::Unsupported(
                        "Top-K ranked value is not a column",
                    ));
                };
                let value = resolve_column_ref(ranked, child).map_err(|_| {
                    PerComputationCandidateError::Unsupported("Top-K ranked value does not resolve")
                })?;
                let items: Vec<_> = child
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != value && !groups.contains(index))
                    .map(|(_, field)| SummaryInputExpr::Column(field.column_ref()))
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
                    .map(asap_types::ir::schema::Field::column_ref)
                    .collect();
                SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { excluding })
            };
            SummaryUpdate {
                item: Some(item),
                // The ranked value ([`measure_input`]).
                weight,
                // Not proven non-negative; selection decides whether CMS is admissible.
                weight_domain: WeightDomain::UnknownOrSigned,
            }
        }
        (_, Some(SketchAlgorithm::UnivMon)) | (AggIntent::Count, Some(SketchAlgorithm::Cms)) => {
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

/// The column fed into a *single-column* summary: the intent's leading
/// positional input resolved to a name against the child schema, or the PromQL
/// sample value when it reads none. Callers are responsible for only reaching
/// here with a one-column intent — [`summarised_input`] is the general form.
pub(crate) fn summarised_column(intent: &AggIntent, child_schema: &Schema) -> ColumnRef {
    match intent
        .input_cols()
        .first()
        .and_then(|id| child_schema.fields.get(*id))
    {
        Some(c) => c.column_ref(),
        None => ColumnRef::SampleValue,
    }
}

/// [`ranked_column`] as a reference: the sample value when it is that
/// column, else the column by name.
pub(crate) fn ranked_value(
    child: &Rc<OperatorNode>,
) -> Result<(ColumnId, ColumnRef), ResolveError> {
    let ranked = ranked_column(child)?;
    let column = match resolve_column_ref(&ColumnRef::SampleValue, &child.schema) {
        Ok(value) if value == ranked => ColumnRef::SampleValue,
        _ => child.schema.fields[ranked].column_ref(),
    };
    Ok((ranked, column))
}

/// What the summary consumes per input row. An intent that reads one column (or
/// none) feeds that column; `COUNT(DISTINCT a, b)` feeds the whole tuple as one
/// item, so the distinct-count sketch hashes `(a, b)` rather than `a` — the
/// difference between tuple cardinality and single-column cardinality.
///
/// A tuple leg outside the child schema is an error rather than
/// [`summarised_column`]'s sample-value fallback: a leg has no sample-value
/// reading, and silently dropping one would under-count: `None` then.
pub(crate) fn summarised_input(
    intent: &AggIntent,
    child_schema: &Schema,
) -> Option<SummaryInputExpr> {
    let cols = intent.input_cols();
    if cols.len() < 2 {
        return Some(SummaryInputExpr::Column(summarised_column(
            intent,
            child_schema,
        )));
    }
    let legs = cols
        .iter()
        .map(|id| child_schema.fields.get(*id).map(Field::column_ref))
        .collect::<Option<Vec<_>>>()?;
    Some(SummaryInputExpr::Tuple(
        legs.into_iter().map(SummaryInputExpr::Column).collect(),
    ))
}
