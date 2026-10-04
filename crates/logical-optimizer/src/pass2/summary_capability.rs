//! Pass 2's summary-capability rule (#509): computations with the same
//! summary input data (source, filters, update expression, grouping) and the
//! same window can share one summary that supports every requested estimate,
//! sized for the strictest consumer.
//!
//! The rule is all-or-nothing per key (#580 decision W5): every approximate
//! target of a key is re-sized for the key's strictest requirement, so their
//! summary producers are identical and the shared variant's merge after
//! composition reaches one state. Sizing reuses the legacy reconciliation's
//! argument ([`super::reconciliation`]): requirements resolve through
//! [`accuracy_budget`] and every shipped sizing formula is monotonic in
//! `(ε, δ)`, so a summary sized for a requirement that dominates every
//! consumer's meets each of them. Stage 3 still checks each query against its
//! own target.
//!
//! A shared state changes cost non-additively, so this is a Stage 1 variant
//! next to the independent one, and Stage 3 chooses.

use std::rc::Rc;

use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::schema::ColumnId;
use asap_types::ir::{NonASAPOp, OperatorNode};
use asap_types::types::AccuracyTarget;

use super::reconciliation::dominates;
use crate::pass1::logical_candidates::{
    local_realizations_for_intent, LocalLogicalCandidates, LogicalCandidateError,
};
use crate::pass1::replacement::{accuracy_budget, accuracy_target};

/// The estimates one summary serves over one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability {
    /// Any quantile, from one KLL or DDSketch.
    Quantile,
    /// Distinct count, L2 norm and entropy of the value frequencies, from one
    /// UnivMon (#509 Example 2).
    FrequencyMoments,
}

/// What a summary for a target would ingest and which estimates it would
/// serve. Targets with equal keys can share one summary.
struct Key<'a> {
    capability: Capability,
    column: Option<ColumnId>,
    reduction: &'a Reduction,
    /// The input, window included (`TimeRange`, `WHERE`, ...).
    child: &'a Rc<OperatorNode>,
}

impl Key<'_> {
    fn matches(&self, other: &Key<'_>) -> bool {
        self.capability == other.capability
            && self.column == other.column
            && self.reduction == other.reduction
            && (Rc::ptr_eq(self.child, other.child) || self.child == other.child)
    }
}

/// The key of a single-measure, unfiltered aggregate with an approximate
/// requirement, or `None` when the rule does not apply to it.
fn key(node: &OperatorNode) -> Option<Key<'_>> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        having: None,
        child,
        ..
    }) = node.non_asap()
    else {
        return None;
    };
    let ([intent], true) = (measures.as_slice(), filters.iter().all(Option::is_none)) else {
        return None;
    };
    if matches!(accuracy_target(intent)?, AccuracyTarget::Exact) {
        return None;
    }
    let (capability, column) = match intent {
        AggIntent::Quantile { col, .. } => (Capability::Quantile, *col),
        // A distinct-tuple count has no UnivMon alternative.
        AggIntent::Cardinality { cols, .. } if cols.len() <= 1 => {
            (Capability::FrequencyMoments, cols.first().copied())
        }
        AggIntent::FrequencyL2 { col, .. } | AggIntent::FrequencyEntropy { col, .. } => {
            (Capability::FrequencyMoments, *col)
        }
        _ => return None,
    };
    Some(Key {
        capability,
        column,
        reduction,
        child,
    })
}

/// `intent` with its accuracy requirement replaced.
fn with_accuracy(intent: &AggIntent, target: AccuracyTarget) -> AggIntent {
    let mut intent = intent.clone();
    match &mut intent {
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::FrequencyL2 { accuracy, .. }
        | AggIntent::FrequencyEntropy { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => *accuracy = target,
        _ => {}
    }
    intent
}

/// The requirement that dominates every one in `targets`: the smallest ε
/// and the smallest δ.
fn strictest<'a>(targets: impl IntoIterator<Item = &'a AccuracyTarget>) -> AccuracyTarget {
    let (epsilon, delta) = targets
        .into_iter()
        .map(accuracy_budget)
        .fold((f64::INFINITY, f64::INFINITY), |(e, d), (e2, d2)| {
            (e.min(e2), d.min(d2))
        });
    AccuracyTarget::EpsilonDelta { epsilon, delta }
}

/// The summary-capability rule over a Pass 1 inventory.
#[derive(Debug, Clone)]
pub struct CapabilitySharing<Id> {
    /// `inventory` with every target of a shared key re-sized for the key's
    /// strictest requirement.
    pub inventory: LocalLogicalCandidates<Id>,
    /// Whether any target was re-sized. When none was, the rule adds only
    /// the merge of identical producers after composition.
    pub resized: bool,
}

/// Apply the rule to `inventory`, or `None` when no two targets share a key.
pub fn share_summary_capability<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
) -> Result<Option<CapabilitySharing<Id>>, LogicalCandidateError> {
    let keys: Vec<_> = inventory.targets.iter().map(|t| key(&t.target)).collect();
    let mut group = vec![None::<usize>; keys.len()];
    for t in 0..keys.len() {
        let Some(key) = &keys[t] else { continue };
        group[t] = Some(
            (0..t)
                .find(|&u| keys[u].as_ref().is_some_and(|other| key.matches(other)))
                .map_or(t, |u| group[u].expect("keyed")),
        );
    }
    let mut out = inventory.clone();
    let mut shared = false;
    let mut resized = false;
    for leader in 0..keys.len() {
        let members: Vec<usize> = (0..keys.len())
            .filter(|&t| group[t] == Some(leader))
            .collect();
        if members.len() < 2 {
            continue;
        }
        shared = true;
        let intents: Vec<&AggIntent> = members
            .iter()
            .map(|&t| single_intent(inventory, t))
            .collect();
        let requirements: Vec<&AccuracyTarget> = intents
            .iter()
            .map(|intent| accuracy_target(intent).expect("keyed targets are approximate"))
            .collect();
        let target = strictest(requirements.iter().copied());
        debug_assert!(requirements.iter().all(|r| dominates(&target, r)));
        for ((&t, intent), requirement) in members.iter().zip(intents).zip(requirements) {
            if accuracy_budget(requirement) == accuracy_budget(&target) {
                continue;
            }
            let alternatives =
                local_realizations_for_intent(&with_accuracy(intent, target.clone()))?;
            // Keyed targets never absorb the target beneath them (only a
            // whole-expression top-k does), so only the sizes change.
            debug_assert!(out.targets[t].absorbs.iter().all(Option::is_none));
            out.targets[t].absorbs = vec![None; alternatives.len()];
            out.targets[t].windows = vec![Default::default(); alternatives.len()];
            // Count targets, the only ones with Hydra, are not keyed.
            out.targets[t].groupings = vec![Default::default(); alternatives.len()];
            out.targets[t].alternatives = alternatives;
            resized = true;
        }
    }
    Ok(shared.then_some(CapabilitySharing {
        inventory: out,
        resized,
    }))
}

fn single_intent<Id>(inventory: &LocalLogicalCandidates<Id>, t: usize) -> &AggIntent {
    match inventory.targets[t].target.non_asap() {
        Some(NonASAPOp::Aggregate { measures, .. }) => &measures[0],
        _ => unreachable!("keyed targets are single-measure aggregates"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::logical_candidates::enumerate_local_logical_candidates;
    use crate::pass1::replacement::Realization;
    use crate::test_support::lower_promql;
    use asap_types::ir::schema::{SketchAlgorithm, SketchParams};
    use asap_types::ir::QueryRoot;

    fn inventory(queries: &[(&str, f64)]) -> LocalLogicalCandidates<usize> {
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, (q, epsilon))| {
                let root = lower_promql(q, AccuracyTarget::Epsilon(*epsilon));
                let root =
                    asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
                (i, QueryRoot::Operator(root))
            })
            .collect();
        enumerate_local_logical_candidates(roots, &Default::default()).unwrap()
    }

    fn kll_k(inventory: &LocalLogicalCandidates<usize>, t: usize) -> u32 {
        inventory.targets[t]
            .alternatives
            .iter()
            .find_map(|a| match a {
                Realization::Sketch(kind) => match kind.params() {
                    SketchParams::Kll { k } => Some(*k),
                    _ => None,
                },
                _ => None,
            })
            .expect("a KLL alternative")
    }

    /// p50 at ε = 0.01 and p99 at ε = 0.001 over one input: both targets'
    /// summaries are sized for ε = 0.001, so their KLLs are identical.
    #[test]
    fn quantiles_are_sized_for_the_strictest_consumer() {
        let base = inventory(&[
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[5m])", 0.001),
        ]);
        assert!(kll_k(&base, 0) < kll_k(&base, 1));
        let shared = share_summary_capability(&base).unwrap().expect("one key");
        assert!(shared.resized);
        assert_eq!(kll_k(&shared.inventory, 0), kll_k(&base, 1));
        assert_eq!(kll_k(&shared.inventory, 1), kll_k(&base, 1));
        let ddsketch = |inv: &LocalLogicalCandidates<usize>, t: usize| {
            inv.targets[t].alternatives.iter().find_map(|a| match a {
                Realization::Sketch(kind) if *kind.algorithm() == SketchAlgorithm::DDSketch => {
                    Some(kind.clone())
                }
                _ => None,
            })
        };
        assert_eq!(ddsketch(&shared.inventory, 0), ddsketch(&base, 1));
    }

    /// Equal requirements need no re-sizing; the key still groups them.
    #[test]
    fn equal_requirements_are_grouped_without_resizing() {
        let base = inventory(&[
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[5m])", 0.01),
        ]);
        let shared = share_summary_capability(&base).unwrap().expect("one key");
        assert!(!shared.resized);
    }

    /// Distinct count, entropy and L2 over one input form one key. UnivMon's
    /// shape does not depend on the requirement, so all three alternatives
    /// are the same state; the distinct count's other summaries are re-sized
    /// for the strictest ε.
    #[test]
    fn frequency_moments_share_one_univmon() {
        let base = inventory(&[
            ("distinct_over_time(src[1m])", 0.02),
            ("entropy_over_time(src[1m])", 0.05),
            ("l2_over_time(src[1m])", 0.01),
        ]);
        let shared = share_summary_capability(&base).unwrap().expect("one key");
        assert!(shared.resized);
        let univmon: Vec<_> = shared
            .inventory
            .targets
            .iter()
            .map(|t| {
                t.alternatives
                    .iter()
                    .find(|a| matches!(a, Realization::Sketch(kind) if *kind.algorithm() == SketchAlgorithm::UnivMon))
                    .cloned()
                    .expect("a UnivMon alternative")
            })
            .collect();
        assert!(univmon.iter().all(|u| *u == univmon[0]));
        let hll = |inv: &LocalLogicalCandidates<usize>| {
            inv.targets[0].alternatives.iter().find_map(|a| match a {
                Realization::Sketch(kind) if *kind.algorithm() == SketchAlgorithm::Hll => {
                    Some(kind.clone())
                }
                _ => None,
            })
        };
        assert_ne!(hll(&shared.inventory), hll(&base));
    }

    /// A different window, selector or estimate family is a different key.
    #[test]
    fn different_summary_input_or_window_is_not_shared() {
        for queries in [
            [
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[10m])", 0.001),
            ],
            [
                ("quantile_over_time(0.5, lat{job=\"a\"}[5m])", 0.01),
                ("quantile_over_time(0.99, lat{job=\"b\"}[5m])", 0.001),
            ],
            [
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("count_over_time(lat[5m])", 0.001),
            ],
        ] {
            let base = inventory(&queries);
            assert!(
                share_summary_capability(&base).unwrap().is_none(),
                "{queries:?}"
            );
        }
    }
}
