//! Per-query validity before composition (joint-planning-search design,
//! step 2): each query's own candidates are checked with the query planned
//! alone, and a whole-workload choice that picks a discarded one is never
//! composed.
//!
//! A query's own candidate is one choice of a Pass 1 candidate for every
//! target the query reads. It is rejected only when no remaining choice can
//! repair it (design method 2): planned alone, its composed candidate and
//! every Pass 2 candidate of it, in every Stage 2 materialization, fail a
//! check that no summary size changes ([`checked_candidate`],
//! [`CheckFailure::size_independent`](crate::CheckFailure::size_independent)): the executor has no kernel for a
//! per-series aggregate it reads, a Count-Min reads weights not proven
//! non-negative, or its output has no guarantee or one whose bound stays
//! unknown, with a deployment accuracy model and evidence that judge only
//! the guarantee. Joint planning repairs nothing else: composition with
//! other queries and the cross-query Pass 2 rules share nodes, size a
//! shared summary for its strictest reader, or answer an exact computation
//! from another exact one. A different family or a different
//! materialization is another choice, still searched, and every completed
//! plan is still checked by Stage 3.

use std::collections::HashSet;
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_logical_asap_optimization::workload_candidates::stage1_candidates_of_choice;
use asap_physical_asap_optimization::optimize_physical_asap;
use asap_types::ir::{OperatorNode, QueryRoot};

use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::{
    accuracy_targets, checked_candidate, DiscardedQueryCandidate, PlanningModels,
    QuerySearchReport, RejectionCause,
};

/// Each query's own candidates, checked: which whole-workload choices
/// compose only valid ones.
pub(crate) struct QueryCandidates {
    queries: Vec<QueryChoices>,
}

/// One query's own candidates.
struct QueryChoices {
    /// The digits of a whole-workload choice that decide its plan, in
    /// increasing order: the targets it reads, and any target whose fused
    /// computation absorbs one of them.
    digits: Vec<usize>,
    /// Its own candidates that failed a check, by their values of `digits`.
    discarded: HashSet<Vec<usize>>,
    report: QuerySearchReport,
}

impl QueryCandidates {
    /// Check every own candidate of every query of `per_computation`
    /// (already pruned for the deployment's capabilities), each planned
    /// alone through Stage 2 and the Stage 3 checks. `entries[q]` is query
    /// `q`'s workload entry.
    pub(crate) fn check<Id: Clone>(
        per_computation: &PerComputationCandidates<Id>,
        entries: &[QueryWorkloadEntry],
        data: &DataWorkload,
        models: PlanningModels<'_>,
    ) -> Self {
        let targets = accuracy_targets(entries);
        let queries = per_computation
            .roots
            .iter()
            .enumerate()
            .map(|(query, root)| {
                let alone = PerComputationCandidates {
                    roots: vec![root.clone()],
                    targets: per_computation.targets.clone(),
                };
                let digits = query_digits(per_computation, &root.1);
                let mut choices = QueryChoices {
                    digits,
                    discarded: HashSet::new(),
                    report: QuerySearchReport::default(),
                };
                let mut choice = vec![0; per_computation.targets.len()];
                loop {
                    if !per_computation.absorbed_choice_varies(&choice) {
                        choices.report.candidates_checked += 1;
                        let failed = alone_violation(
                            &alone,
                            &choice,
                            &entries[query..=query],
                            &targets[query..=query],
                            data,
                            &models,
                        );
                        if let Some((cause, reason)) = failed {
                            // Planned alone, it is the workload's first query.
                            let reason = reason.strip_prefix("q1: ").unwrap_or(&reason).to_string();
                            let values: Vec<usize> =
                                choices.digits.iter().map(|&d| choice[d]).collect();
                            choices.report.discarded.push(DiscardedQueryCandidate {
                                choice: choices
                                    .digits
                                    .iter()
                                    .copied()
                                    .zip(values.clone())
                                    .collect(),
                                cause,
                                reason,
                            });
                            choices.discarded.insert(values);
                        }
                    }
                    if !next_digits(per_computation, &choices.digits, &mut choice) {
                        break;
                    }
                }
                choices
            })
            .collect();
        Self { queries }
    }

    /// `Some(n)` when every whole-workload choice that agrees with `choice`
    /// on its first `n` digits picks a discarded candidate of some query:
    /// the smallest such `n` over the queries whose candidate in `choice` is
    /// discarded. `None` when `choice` picks only valid ones.
    pub(crate) fn invalid_prefix(&self, choice: &[usize]) -> Option<usize> {
        self.queries
            .iter()
            .filter(|query| !query.discarded.is_empty())
            .filter(|query| {
                let values: Vec<usize> = query.digits.iter().map(|&d| choice[d]).collect();
                query.discarded.contains(&values)
            })
            .map(|query| query.digits.last().map_or(0, |&last| last + 1))
            .min()
    }

    /// Each query's report, in workload order.
    pub(crate) fn reports(&self) -> Vec<QuerySearchReport> {
        self.queries.iter().map(|q| q.report.clone()).collect()
    }
}

/// The digits of a whole-workload choice that decide `root`'s plan: the
/// targets whose aggregate it reads, and the targets whose fused
/// computation absorbs one of those, in increasing order.
fn query_digits<Id>(
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
                    .fused
                    .as_ref()
                    .is_some_and(|fused| read[fused.absorbs])
        })
        .collect()
}

/// Mixed-radix increment of `choice` over `digits` only (the last varying
/// fastest); `false` after the last combination.
fn next_digits<Id>(
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
fn alone_violation<Id: Clone>(
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
            models.deployment_capabilities,
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
