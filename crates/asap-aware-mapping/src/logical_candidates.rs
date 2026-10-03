//! Pass 1 local alternatives over the unified logical IR.
//!
//! Alternatives are nominal realization descriptors attached to their original
//! target, not ranked plans or accuracy certificates. Workload composition and
//! physical planning consume this inventory later; empirical models belong to
//! selection. The legacy search API remains until planner cutover.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::summary_coverage::{CoverageRegion, SummaryCoverage};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, SchemaDerivationError};
use asap_types::post_asap::{
    EntityIdentity, ExactKind, ExactParams, FieldDataType, GroupingStrategy,
    NonNegativeWeightProof, SketchAlgorithm, SketchKind, SketchStatistic, SummaryInputExpr,
    SummaryUpdate, WeightDomain,
};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::{AggIntent, Reduction, Schema};
use asap_types::types::AccuracyTarget;
use thiserror::Error;

use crate::replacement::{
    accuracy_budget, accuracy_target, default_size_params, summary_candidates, Realization,
};

/// All local realizations of one single-measure aggregate. The target retains
/// source, grouping, filters, input expressions and evaluation context.
#[derive(Debug, Clone)]
pub struct LocalLogicalTarget {
    pub target: Rc<OperatorNode>,
    pub alternatives: Vec<Realization>,
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
) -> Result<LocalLogicalCandidates<Id>, LogicalCandidateError> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for (_, root) in &roots {
        root.validate_structure()?;
        let operators = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for root in operators {
            for node in OperatorNode::reachable(root) {
                if !seen.insert(Rc::as_ptr(&node)) {
                    continue;
                }
                if node.timing.is_some() {
                    return Err(LogicalCandidateError::AssignedTiming);
                }
                if let Some(NonASAPOp::Aggregate { measures, .. }) = node.non_asap() {
                    if let [intent] = measures.as_slice() {
                        targets.push(LocalLogicalTarget {
                            alternatives: local_realizations_for_intent(intent)?,
                            target: node,
                        });
                    }
                }
            }
        }
    }
    Ok(LocalLogicalCandidates { roots, targets })
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
                .map(|alternative| (Rc::as_ptr(&target.target), alternative))
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

fn rewrite(
    node: &Rc<OperatorNode>,
    chosen: &HashMap<*const OperatorNode, &Realization>,
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
        Some(realization) if **realization != Realization::PassThrough => {
            realize(node, realization, memo)?
        }
        _ if changed => Rc::new(node.map_children(|child| memo[&Rc::as_ptr(child)].clone())?),
        _ => node.clone(),
    };
    memo.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

fn realize(
    target: &OperatorNode,
    realization: &Realization,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, LogicalCandidateError> {
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
    if !filters.is_empty() || having.is_some() {
        return Err(LogicalCandidateError::Unsupported(
            "filtered or HAVING aggregate",
        ));
    }
    let child = memo[&Rc::as_ptr(child)].clone();
    let (family, query) = match realization {
        Realization::ExactAggregate { kind, params } => (
            FieldDataType::ExactAggregate(kind.clone(), params.clone()),
            None,
        ),
        Realization::Sketch(kind) => (
            FieldDataType::Sketch(kind.clone(), GroupingStrategy::default()),
            Some(statistic(intent)?),
        ),
        _ => return Err(LogicalCandidateError::Unsupported("summary family")),
    };
    let input = summary_update(intent, &family, reduction, &child.schema)?;
    let state = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: child.clone(),
        family,
        input,
        reduction: reduction.clone(),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))?;
    // Whole-source coverage is declared, not proven: Pass 1 trusts that the
    // state holds every observation of its source that reaches it (#570).
    let coverage = SummaryCoverage {
        source: single_source(&child)?,
        regions: vec![CoverageRegion {
            time_ms: None,
            population: BTreeMap::new(),
        }],
    };
    let state = Rc::new(state.with_coverage(coverage)?);
    let evaluation = match query {
        Some(query) => ASAPOp::SummaryEstimate {
            summary_input: state,
            query,
        },
        None => ASAPOp::FinalizeExactAccumulator { child: state },
    };
    Ok(OperatorNode::new_shared(Operator::ASAP(evaluation))?)
}

fn single_source(
    node: &Rc<OperatorNode>,
) -> Result<asap_types::pre_asap::Source, LogicalCandidateError> {
    let mut sources = Vec::new();
    for node in OperatorNode::reachable(node) {
        if let Some(NonASAPOp::Scan { source, .. }) = node.non_asap() {
            if !sources.contains(source) {
                sources.push(source.clone());
            }
        }
    }
    match <[_; 1]>::try_from(sources) {
        Ok([source]) => Ok(source),
        Err(_) => Err(LogicalCandidateError::Unsupported(
            "summary coverage needs exactly one source",
        )),
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
    let weight = crate::replacement::summarised_input(intent, child)
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
                .map(crate::replacement::column_ref)
                .collect();
            // Rows that carry the full series identity rank it as a column,
            // the item form the runtime builds keyed summaries from.
            let item = if child.has_promql_series_identity() {
                SummaryInputExpr::Column(ColumnRef::Named(
                    asap_types::pre_asap::schema::PROMQL_SERIES_IDENTITY.into(),
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
        assert!(choices.iter().any(|choice| matches!(choice, Realization::Sketch(kind) if *kind.algorithm() == asap_types::post_asap::SketchAlgorithm::UnivMon)));
    }
}
