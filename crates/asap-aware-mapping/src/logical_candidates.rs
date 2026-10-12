//! Pass 1 local alternatives over the unified logical IR.
//!
//! Alternatives are nominal realization descriptors attached to their original
//! target, not ranked plans or accuracy certificates. Summary alternatives are
//! unsized: Pass 2 may share a summary and tighten it to the strictest consumer,
//! so sizing happens later. Workload composition and physical planning consume
//! this inventory later; empirical models belong to selection. The legacy search
//! API remains until planner cutover.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator_properties::Reduction;
use asap_types::ir::{NonASAPOp, OperatorNode, Predicate, QueryRoot, SchemaDerivationError};
use asap_types::post_asap::{ExactKind, ExactParams, SketchAlgorithm};
use asap_types::pre_asap::{resolve_column_ref, AggIntent, ColumnRef};
use asap_types::types::AccuracyTarget;
use thiserror::Error;

use crate::replacement::{accuracy_budget, accuracy_target, summary_candidates};

/// One local realization of a target measure.
///
/// Each target has exactly one exact option: `ExactAccumulator` or
/// `AvgAsSumCount` when the intent has one and it can be built, otherwise
/// `Exact`. The plain form would duplicate "accumulator, not materialized";
/// Stage 2/3 decide materialization and sharing.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalCandidate {
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
pub struct LocalChoices {
    pub alternatives: Vec<LocalCandidate>,
    pub pruned: Vec<(LocalCandidate, PruneReason)>,
}

impl LocalChoices {
    /// Prune matching candidates; a pruned exact accumulator form falls back to
    /// plain `Exact`, so the target keeps one exact option.
    fn prune(&mut self, reason: PruneReason, applies: impl Fn(&LocalCandidate) -> bool) {
        let (pruned, kept): (Vec<_>, _) = std::mem::take(&mut self.alternatives)
            .into_iter()
            .partition(|choice| *choice != LocalCandidate::Exact && applies(choice));
        self.alternatives = kept;
        if pruned.iter().any(|choice| {
            matches!(
                choice,
                LocalCandidate::ExactAccumulator { .. } | LocalCandidate::AvgAsSumCount
            )
        }) {
            self.alternatives.insert(0, LocalCandidate::Exact);
        }
        self.pruned.extend(
            pruned
                .into_iter()
                .map(|choice: LocalCandidate| (choice, reason)),
        );
    }
}

/// Where a target's `HAVING` is applied. It covers the aggregate's own
/// `having` and a `Filter` directly above the aggregate (the form the SQL
/// frontend lowers `HAVING` to). Composition (#575) applies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HavingPlacement {
    /// The predicate reads only group keys, so it is pushed below the
    /// aggregate as a row predicate, like a measure filter.
    BelowAggregate,
    /// The predicate reads aggregate values, so it stays a `Filter` above the
    /// finalize/estimate output; approximate candidates of this target carry
    /// `guarantee = None`, since membership near the threshold may change.
    AboveOutput,
}

/// One measure of an aggregate with at least one realization besides exact
/// execution. The target retains source, grouping, filters, input expressions
/// and evaluation context.
#[derive(Debug, Clone)]
pub struct LocalLogicalTarget {
    pub target: Rc<OperatorNode>,
    /// Index into the aggregate's measures.
    pub measure: usize,
    pub choices: LocalChoices,
    /// The measure's own filter; only rows that pass it update the state
    /// (`SummaryAgg.filter`).
    pub filter: Option<Predicate>,
    pub having: Option<HavingPlacement>,
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
    #[error("logical candidate input already contains ASAP operators")]
    ContainsAsap,
    #[error("approximate accuracy requires finite positive epsilon and delta in (0, 1)")]
    InvalidAccuracy,
}

/// Enumerate exact, rewrite and summary choices of one measure in stable catalog
/// order, without ranking or empirical assessment. A summary is not a claim that
/// a deployment meets the measure's accuracy requirement. The one exact option
/// comes first: the exact accumulator form if the intent has one, else `Exact`.
pub fn local_candidates_for_intent(
    intent: &AggIntent,
) -> Result<LocalChoices, LogicalCandidateError> {
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
        Some((kind, params)) => LocalCandidate::ExactAccumulator { kind, params },
        None if matches!(intent, AggIntent::Avg { .. }) => LocalCandidate::AvgAsSumCount,
        None => LocalCandidate::Exact,
    };
    let mut choices = LocalChoices {
        alternatives: vec![exact],
        pruned: vec![],
    };
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
        }
        let summaries = summary_candidates(intent).iter().cloned();
        if *target == AccuracyTarget::Exact {
            choices.pruned.extend(summaries.map(|algorithm| {
                (
                    LocalCandidate::Summary(algorithm),
                    PruneReason::RequiresExact,
                )
            }));
        } else {
            choices
                .alternatives
                .extend(summaries.map(LocalCandidate::Summary));
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

/// Where `having` over `aggregate`'s output is applied: below the aggregate
/// when it reads only `by(...)` group keys, otherwise above the output.
fn having_placement(aggregate: &OperatorNode, having: &Predicate) -> HavingPlacement {
    let keys = match aggregate.non_asap() {
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(by),
            ..
        }) if !by.is_without() => by.len(),
        _ => 0,
    };
    if having.0.operator_refs().is_empty()
        && having.0.columns_referenced().iter().all(|id| *id < keys)
    {
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
pub fn enumerate_local_logical_candidates<Id>(
    roots: Vec<(Id, QueryRoot)>,
) -> Result<LocalLogicalCandidates<Id>, LogicalCandidateError> {
    let mut seen = HashSet::new();
    let mut aggregates = Vec::new();
    // `HAVING` lowered as a `Filter` directly above an aggregate; a shared
    // aggregate keeps the placement that is safe for every such filter.
    let mut havings: HashMap<*const OperatorNode, HavingPlacement> = HashMap::new();
    for (_, root) in &roots {
        root.validate_structure()?;
        let operators = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for root in operators {
            if root.contains_asap() {
                return Err(LogicalCandidateError::ContainsAsap);
            }
            for node in OperatorNode::reachable(root) {
                if !seen.insert(Rc::as_ptr(&node)) {
                    continue;
                }
                if node.timing.is_some() {
                    return Err(LogicalCandidateError::AssignedTiming);
                }
                match node.non_asap() {
                    Some(NonASAPOp::Aggregate { .. }) => aggregates.push(node),
                    Some(NonASAPOp::Filter { pred, child })
                        if matches!(child.non_asap(), Some(NonASAPOp::Aggregate { .. })) =>
                    {
                        let placement = having_placement(child, pred);
                        havings
                            .entry(Rc::as_ptr(child))
                            .and_modify(|having| *having = (*having).max(placement))
                            .or_insert(placement);
                    }
                    _ => {}
                }
            }
        }
    }
    let mut targets = Vec::new();
    for node in aggregates {
        let Some(NonASAPOp::Aggregate {
            measures,
            filters,
            having,
            ..
        }) = node.non_asap()
        else {
            continue;
        };
        let having = having
            .as_ref()
            .map(|having| having_placement(&node, having))
            .max(havings.get(&Rc::as_ptr(&node)).copied());
        for (measure, intent) in measures.iter().enumerate() {
            let mut choices = local_candidates_for_intent(intent)?;
            if choices.alternatives == [LocalCandidate::Exact] && choices.pruned.is_empty() {
                continue;
            }
            if nullable_avg_input(&node, intent) {
                choices.prune(PruneReason::NullableAvgInput, |choice| {
                    *choice == LocalCandidate::AvgAsSumCount
                });
            }
            targets.push(LocalLogicalTarget {
                target: node.clone(),
                measure,
                choices,
                filter: filters.get(measure).cloned().flatten(),
                having,
            });
        }
    }
    Ok(LocalLogicalCandidates { roots, targets })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Approximate count keeps one exact option, its accumulator, beside the
    /// universal summary; plain exact execution is not offered.
    #[test]
    fn approximate_count_keeps_one_exact_accumulator_and_universal_choices() {
        let choices = local_candidates_for_intent(&AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.05,
                delta: 0.01,
            },
        })
        .unwrap()
        .alternatives;
        assert!(!choices.contains(&LocalCandidate::Exact));
        assert_eq!(
            choices[0],
            LocalCandidate::ExactAccumulator {
                kind: ExactKind::Count,
                params: ExactParams::Count
            }
        );
        assert!(choices.contains(&LocalCandidate::Summary(SketchAlgorithm::UnivMon)));
    }
}
