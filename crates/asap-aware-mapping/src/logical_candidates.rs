//! Pass 1 local alternatives over the unified logical IR.
//!
//! Alternatives are nominal realization descriptors attached to their original
//! target, not ranked plans or accuracy certificates. Workload composition and
//! physical planning consume this inventory later; empirical models belong to
//! selection. The legacy search API remains until planner cutover.
use std::collections::HashSet;
use std::rc::Rc;

use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot, SchemaDerivationError};
use asap_types::post_asap::{ExactKind, ExactParams, SketchKind};
use asap_types::pre_asap::AggIntent;
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
