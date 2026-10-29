//! Shared, size-independent per-query proof used before joint search.
//! A proof considers every local Pass 2 and Stage 2 choice; unknown/unsupported
//! composition and any size-dependent failure remain eligible for joint planning.
use crate::{checked_candidate, PlanningModels, RejectionCause};
use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    HowToComputeAggregateMeasure, PerComputationCandidates,
};
use asap_logical_asap_optimization::pass2::workload_candidates::stage1_candidates_of_choice;
use asap_physical_asap_optimization::optimize_physical_asap;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::collections::HashSet;
use std::rc::Rc;

#[derive(Debug, Default)]
pub(crate) struct QueryCandidatePruningReport {
    pub choices_checked: usize,
    pub queries_skipped: usize,
    pub shared_producer_choices_retained: usize,
    pub non_summary_choices_retained: usize,
    /// No empty-group semantics are assumed: preserve the final own candidate.
    pub final_choices_retained: usize,
    pub removed: Vec<PrunedQueryChoice>,
}

#[derive(Debug)]
pub(crate) struct PrunedQueryChoice {
    pub target: usize,
    /// Original Pass 1 choice index, before this pruning pass.
    pub choice: usize,
    pub cause: RejectionCause,
    pub reason: String,
}

pub(crate) struct PrunedQueryCandidates<Id> {
    pub inventory: PerComputationCandidates<Id>,
    pub report: QueryCandidatePruningReport,
}

/// Bounded per-query checking, never a Cartesian product across queries.
/// A choice survives if any consumer context has no complete proof. Families
/// offered by another choice also survive: their state may serve a different
/// logical result even when this choice's original consumer is impossible.
pub(crate) fn prune_query_candidates<Id: Clone>(
    inventory: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    max_checks: usize,
) -> Result<PrunedQueryCandidates<Id>, String> {
    if inventory.roots.len() != entries.len() {
        return Err("query pruning requires one workload entry per query root".into());
    }
    if inventory
        .targets
        .iter()
        .any(|target| target.ways_to_compute.is_empty())
    {
        return Err("query pruning requires an own candidate for every target".into());
    }
    let mut report = QueryCandidatePruningReport::default();
    let targets = crate::accuracy_targets(entries);
    let mut retain: Vec<Vec<bool>> = inventory
        .targets
        .iter()
        .map(|target| vec![false; target.choice_count()])
        .collect();
    let mut failures: Vec<Vec<Option<(RejectionCause, String)>>> = inventory
        .targets
        .iter()
        .map(|target| vec![None; target.choice_count()])
        .collect();
    let potentially_removable: Vec<Vec<bool>> = inventory
        .targets
        .iter()
        .enumerate()
        .map(|(target, computation)| {
            (0..computation.choice_count())
                .map(|choice| {
                    matches!(
                        computation.computation_for(choice),
                        Some((HowToComputeAggregateMeasure::Summary(_), _))
                    ) && !has_other_summary_family_choice(inventory, target, choice)
                })
                .collect()
        })
        .collect();
    let mut referenced = vec![false; inventory.targets.len()];
    for (query, root) in inventory.roots.iter().enumerate() {
        let digits = query_digits(inventory, &root.1);
        if digits.is_empty() {
            continue;
        }
        for &digit in &digits {
            referenced[digit] = true;
        }
        let combinations = digits.iter().try_fold(1usize, |count, &digit| {
            count.checked_mul(inventory.targets[digit].choice_count())
        });
        // This pass bounds local combinations as well as total evaluations.
        // A repeated target can choose different suppliers at each use in the
        // connection model; legacy uniform-choice enumeration is incomplete.
        if !digits
            .iter()
            .any(|&digit| potentially_removable[digit].iter().any(|remove| *remove))
            || repeated_target_occurrence(inventory, &root.1)
            || combinations.is_none_or(|count| {
                count > 64 || count > max_checks.saturating_sub(report.choices_checked)
            })
        {
            report.queries_skipped += 1;
            for &digit in &digits {
                retain[digit].fill(true);
            }
            continue;
        }
        let alone = PerComputationCandidates {
            roots: vec![root.clone()],
            targets: inventory.targets.clone(),
            pruned_ways_to_compute: Vec::new(),
        };
        let mut choice = vec![0; inventory.targets.len()];
        loop {
            report.choices_checked += 1;
            match alone_violation(
                &alone,
                &choice,
                &entries[query..=query],
                &targets[query..=query],
                data,
                &models,
            ) {
                Some(failure) => {
                    for &digit in &digits {
                        failures[digit][choice[digit]].get_or_insert_with(|| failure.clone());
                    }
                }
                None => {
                    for &digit in &digits {
                        retain[digit][choice[digit]] = true;
                    }
                }
            }
            if !next_digits(inventory, &digits, &mut choice) {
                break;
            }
        }
    }
    let mut pruned = inventory.clone();
    for (target, computation) in pruned.targets.iter_mut().enumerate() {
        if !referenced[target] {
            continue;
        }
        // Removing a consumer also removes the producers generated for it.
        // Keep every summary family offered elsewhere, even across different
        // inputs: family equality deliberately over-approximates possible reuse.
        // Exact/fused accumulator producer shapes are not pruned by this pass.
        for (choice, keep) in retain[target].iter_mut().enumerate() {
            if *keep {
                continue;
            }
            let candidate = inventory.targets[target]
                .computation_for(choice)
                .ok_or("query pruning references a missing target choice")?
                .0;
            let HowToComputeAggregateMeasure::Summary(_) = candidate else {
                *keep = true;
                report.non_summary_choices_retained += 1;
                continue;
            };
            if has_other_summary_family_choice(inventory, target, choice) {
                *keep = true;
                report.shared_producer_choices_retained += 1;
            }
        }
        let own = computation.ways_to_compute.len();
        if !retain[target][..own].iter().any(|keep| *keep) {
            retain[target][0] = true;
            report.final_choices_retained += 1;
        }
        for (choice, keep) in retain[target].iter().enumerate() {
            if !keep {
                let (cause, reason) = failures[target][choice]
                    .as_ref()
                    .ok_or("query pruning is missing a complete failure proof")?;
                report.removed.push(PrunedQueryChoice {
                    target,
                    choice,
                    cause: *cause,
                    reason: reason.clone(),
                });
            }
        }
        computation.ways_to_compute = std::mem::take(&mut computation.ways_to_compute)
            .into_iter()
            .enumerate()
            .filter_map(|(choice, candidate)| retain[target][choice].then_some(candidate))
            .collect();
        if let Some(fused) = &mut computation.fused_computation {
            fused.ways_to_compute = std::mem::take(&mut fused.ways_to_compute)
                .into_iter()
                .enumerate()
                .filter_map(|(choice, candidate)| retain[target][own + choice].then_some(candidate))
                .collect();
        }
    }
    Ok(PrunedQueryCandidates {
        inventory: pruned,
        report,
    })
}

fn has_other_summary_family_choice<Id>(
    inventory: &PerComputationCandidates<Id>,
    target: usize,
    choice: usize,
) -> bool {
    let Some((HowToComputeAggregateMeasure::Summary(family), _)) =
        inventory.targets[target].computation_for(choice)
    else {
        return false;
    };
    inventory.targets.iter().enumerate().any(|(other_target, other)| {
        (0..other.choice_count()).any(|other_choice| {
            (other_target != target || other_choice != choice)
                && matches!(other.computation_for(other_choice), Some((HowToComputeAggregateMeasure::Summary(other_family), _)) if other_family == family)
        })
    })
}

fn repeated_target_occurrence<Id>(
    inventory: &PerComputationCandidates<Id>,
    root: &QueryRoot,
) -> bool {
    let target_nodes: HashSet<_> = inventory
        .targets
        .iter()
        .map(|target| Rc::as_ptr(&target.aggregate))
        .collect();
    let mut pending: Vec<_> = match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expression) => expression.operator_refs(),
    };
    let mut seen_targets = HashSet::new();
    let mut visits = 0;
    while let Some(node) = pending.pop() {
        visits += 1;
        if visits > 4096 {
            return true;
        }
        let pointer = Rc::as_ptr(node);
        if target_nodes.contains(&pointer) && !seen_targets.insert(pointer) {
            return true;
        }
        pending.extend(node.children());
    }
    false
}

/// The digits of a whole-workload choice that decide `root`'s plan: the
/// targets whose aggregate it reads, and the targets whose fused
/// computation absorbs one of those, in increasing order.
pub(crate) fn query_digits<Id>(
    per_computation: &PerComputationCandidates<Id>,
    root: &QueryRoot,
) -> Vec<usize> {
    let starts: Vec<&Rc<OperatorNode>> = match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    };
    let reachable: HashSet<*const OperatorNode> = starts
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .map(|node| Rc::as_ptr(&node))
        .collect();
    let read: Vec<bool> = per_computation
        .targets
        .iter()
        .map(|target| reachable.contains(&Rc::as_ptr(&target.aggregate)))
        .collect();
    (0..per_computation.targets.len())
        .filter(|&t| {
            read[t]
                || per_computation.targets[t]
                    .fused_computation
                    .as_ref()
                    .is_some_and(|fused| fused.absorbed_targets.iter().any(|&a| read[a]))
        })
        .collect()
}

/// Mixed-radix increment of `choice` over `digits` only (the last varying
/// fastest); `false` after the last combination.
pub(crate) fn next_digits<Id>(
    per_computation: &PerComputationCandidates<Id>,
    digits: &[usize],
    choice: &mut [usize],
) -> bool {
    for &digit in digits.iter().rev() {
        choice[digit] += 1;
        if choice[digit] < per_computation.targets[digit].choice_count() {
            return true;
        }
        choice[digit] = 0;
    }
    false
}

/// The check the one-query workload `alone` fails with `choice` whatever
/// joint planning does: its composed candidate and every Pass 2 candidate
/// of it (the rules a single query can apply, such as window composition),
/// each in every Stage 2 materialization, fail the checks, each failure
/// independent of summary sizes ([`CheckFailure::size_independent`](crate::CheckFailure::size_independent)), which
/// sharing with another query can change. `None` when one passes, one
/// fails a check a larger summary might pass, or none composes or is
/// implemented (the whole-workload candidates are then checked as usual).
pub(crate) fn alone_violation<Id: Clone>(
    alone: &PerComputationCandidates<Id>,
    choice: &[usize],
    entries: &[QueryWorkloadEntry],
    targets: &[asap_types::ir::properties::AccuracyTarget],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<(RejectionCause, String)> {
    let mut first = None;
    for candidate in stage1_candidates_of_choice(alone, entries, choice, &|_| true) {
        let roots = candidate.roots.ok()?;
        let physical = optimize_physical_asap(
            "alone",
            &roots,
            entries,
            data,
            models.deployment_capabilities.query_time_results_retained,
        )
        .ok()?;
        for candidate in &physical {
            match checked_candidate(candidate, targets, data, models) {
                Err(failed) if failed.size_independent => {
                    first.get_or_insert((failed.cause, failed.reason));
                }
                _ => return None,
            }
        }
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        generate_per_computation_candidates, HowToComputeAggregateMeasure,
    };
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::SketchAlgorithm;
    use asap_types::workload::{DataArrival, QueryRecurrence, SourceFacts};

    fn fixture() -> (
        PerComputationCandidates<usize>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let target = AccuracyTarget::max_error(0.05).failure_probability(0.3);
        let root = lower_promql("avg(quantile_over_time(0.95, latency[5m]))", target.clone());
        let inventory =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
        let entries = vec![entry_with(
            target,
            QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
        )];
        let data = DataWorkload {
            arrival: DataArrival::AtRest,
            sources: [(
                "latency".into(),
                SourceFacts {
                    series_count: Some(10),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        (inventory, entries, data)
    }

    fn kll_target(inventory: &PerComputationCandidates<usize>) -> usize {
        inventory
            .targets
            .iter()
            .position(|target| {
                target
                    .ways_to_compute
                    .contains(&HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll))
            })
            .unwrap()
    }

    // Rank-only KLL cannot feed value averaging, regardless of its size.
    #[test]
    fn removes_query_impossible_kll_before_candidate_expansion() {
        let (inventory, entries, data) = fixture();
        let target = kll_target(&inventory);
        let result =
            prune_query_candidates(&inventory, &entries, &data, PlanningModels::builtin(), 128)
                .unwrap();
        assert!(!result.inventory.targets[target]
            .ways_to_compute
            .contains(&HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)));
        assert!(!result.report.removed.is_empty());
    }

    // An invalid consumer must not delete the state a valid quantile can use.
    #[test]
    fn keeps_shared_kll_for_valid_quantile_consumer() {
        let (mut inventory, mut entries, data) = fixture();
        let target = kll_target(&inventory);
        inventory.roots.push((
            1,
            QueryRoot::Operator(Rc::clone(&inventory.targets[target].aggregate)),
        ));
        entries.push(entries[0].clone());
        let result =
            prune_query_candidates(&inventory, &entries, &data, PlanningModels::builtin(), 128)
                .unwrap();
        assert!(result.inventory.targets[target]
            .ways_to_compute
            .contains(&HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)));
    }

    // A different logical quantile may read a producer generated for P95.
    #[test]
    fn keeps_producer_configuration_for_another_logical_target() {
        let (inventory, mut entries, data) = fixture();
        let mut roots = inventory.roots;
        roots.push((
            1,
            QueryRoot::Operator(lower_promql(
                "quantile_over_time(0.99, latency[5m])",
                AccuracyTarget::max_error(0.05).failure_probability(0.3),
            )),
        ));
        let inventory = generate_per_computation_candidates(roots).unwrap();
        entries.push(entries[0].clone());
        let target = kll_target(&inventory);
        let result =
            prune_query_candidates(&inventory, &entries, &data, PlanningModels::builtin(), 128)
                .unwrap();
        assert!(result.inventory.targets[target]
            .ways_to_compute
            .contains(&HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)));
    }

    // Shared-only families cannot be deleted, so avoid all physical proof work.
    #[test]
    fn shared_families_skip_unproductive_query_enumeration() {
        let (inventory, mut entries, data) = fixture();
        let mut roots = inventory.roots;
        roots.push((
            1,
            QueryRoot::Operator(lower_promql(
                "avg(quantile_over_time(0.99, latency[5m]))",
                AccuracyTarget::max_error(0.05).failure_probability(0.3),
            )),
        ));
        let inventory = generate_per_computation_candidates(roots).unwrap();
        entries.push(entries[0].clone());
        let result =
            prune_query_candidates(&inventory, &entries, &data, PlanningModels::builtin(), 128)
                .unwrap();
        assert_eq!(result.report.choices_checked, 0);
        assert_eq!(result.report.queries_skipped, 2);
        assert!(result.report.removed.is_empty());
    }

    // No conclusion follows from a truncated local-choice enumeration.
    #[test]
    fn incomplete_enumeration_keeps_candidates() {
        let (inventory, entries, data) = fixture();
        let result =
            prune_query_candidates(&inventory, &entries, &data, PlanningModels::builtin(), 0)
                .unwrap();
        assert!(result.report.removed.is_empty());
        assert_eq!(
            result
                .inventory
                .targets
                .iter()
                .map(|t| t.choice_count())
                .collect::<Vec<_>>(),
            inventory
                .targets
                .iter()
                .map(|t| t.choice_count())
                .collect::<Vec<_>>()
        );
    }

    // Two references can select different candidates for one logical group.
    #[test]
    fn repeated_target_references_are_not_pruned_by_uniform_choices() {
        use asap_types::ir::scalar::ArithmeticOpKind;
        use asap_types::ir::{ExprSemantics, ScalarExpr};
        let (mut inventory, entries, data) = fixture();
        let QueryRoot::Operator(root) = &inventory.roots[0].1 else {
            unreachable!()
        };
        let reference = ScalarExpr::PromqlScalarFromVector(Rc::clone(root));
        inventory.roots[0].1 = QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Add,
            left: Box::new(reference.clone()),
            right: Box::new(reference),
            semantics: ExprSemantics::Promql,
        });
        let result =
            prune_query_candidates(&inventory, &entries, &data, PlanningModels::builtin(), 128)
                .unwrap();
        assert_eq!(result.report.queries_skipped, 1);
        assert_eq!(result.report.choices_checked, 0);
        assert!(result.report.removed.is_empty());
    }

    // A larger shared summary may repair a finite error-bound miss.
    #[test]
    fn finite_accuracy_miss_remains_eligible_for_shared_resizing() {
        let (mut inventory, mut entries, data) = fixture();
        let target = kll_target(&inventory);
        inventory.roots[0].1 = QueryRoot::Operator(Rc::clone(&inventory.targets[target].aggregate));
        entries[0].requirements.accuracy = AccuracyTarget::max_error(1e-8).failure_probability(0.3);
        let result =
            prune_query_candidates(&inventory, &entries, &data, PlanningModels::builtin(), 128)
                .unwrap();
        assert!(result.inventory.targets[target]
            .ways_to_compute
            .contains(&HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)));
    }

    // Deployment evidence can depend on a size introduced by future sharing.
    #[test]
    fn possible_evidence_preserves_missing_guarantee_candidates() {
        struct PotentialEvidence;
        impl asap_accuracy::AccuracyEvidenceProvider for PotentialEvidence {}
        let (inventory, entries, data) = fixture();
        let evidence = PotentialEvidence;
        let models = PlanningModels {
            evidence: &evidence,
            ..PlanningModels::builtin()
        };
        let result = prune_query_candidates(&inventory, &entries, &data, models, 128).unwrap();
        assert!(result.report.removed.is_empty());
    }
}
