//! Stage 1, Pass 2's entry and output: the Pass 1 memo extended by the
//! rules over groups, the readouts' required accuracy, the dead candidates
//! and the logical edges ([`asap_types::stages::LogicalASAPMemoCandidates`]),
//! which Stage 2 expands.

use std::collections::{BTreeMap, BTreeSet};

use crate::dead_candidates::eliminate_dead_candidates;
use crate::requirements::set_readout_requirements;
use crate::rules::{chains, rollup, topk, Pass2RuleError};
use asap_accuracy::{accepted_metrics, producer_satisfies_logical_requirements, AccuracyEvidence};
use asap_types::stages::{
    CandidateForMemoGroupId, EvaluationGrid, LogicalASAPMemoCandidates, LogicalEdge, Memo,
    MemoGroupId, PrunedLogicalCandidate, RootPruneReason,
};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

/// Stage 1, Pass 2 (concepts doc, section 3, "Stage 1, Pass 2"), in order:
/// 1. rollup from the finest common group;
/// 2. top-k reuse;
/// 3. reach, caps and readout requirements;
/// 4. dead-candidate elimination;
/// 5. chain groups for the surviving state groups, then dead-candidate
///    elimination again over the memo with them (a merge whose θ serves no
///    consumer);
/// 6. the logical edges between the surviving candidates, with
///    $\alpha^{\log}$ from `evidence` or the theoretical guarantee.
///
/// `entries[i]` is query `i`'s workload entry (its evaluation grid).
pub fn generate_logical_asap_memo_candidates(
    pass1: Memo,
    entries: &[QueryWorkloadEntry],
    evidence: &dyn AccuracyEvidence,
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    let mut memo = pass1;
    rollup::add_rollup_candidates(&mut memo)?;
    topk::add_topk_candidates(&mut memo)?;
    let unknown_values = set_readout_requirements(&mut memo)?;
    let dead = |pruned: &[PrunedLogicalCandidate]| -> BTreeSet<CandidateForMemoGroupId> {
        pruned.iter().map(|pruned| pruned.candidate).collect()
    };
    let before_chains =
        eliminate_dead_candidates(&memo, &candidate_edges(&memo, &unknown_values, evidence));
    chains::add_chain_groups(&mut memo, entries, &dead(&before_chains))?;
    let candidate_edges = candidate_edges(&memo, &unknown_values, evidence);
    let pruned = eliminate_dead_candidates(&memo, &candidate_edges);
    let dead = dead(&pruned);
    let edges = candidate_edges
        .into_iter()
        .filter(|edge| !dead.contains(&edge.producer) && !dead.contains(&edge.consumer))
        .map(|edge| LogicalEdge {
            producer_candidate_id: edge.producer,
            consumer_candidate_id: edge.consumer,
            input_index_of_consumer: edge.input,
            satisfies_logical_requirement: edge.satisfies.is_ok(),
        })
        .collect();
    let evaluation_grid_of_group = evaluation_grids(&memo, entries);
    Ok(LogicalASAPMemoCandidates {
        memo,
        edges,
        pruned,
        evaluation_grid_of_group,
    })
}

/// One possible $(u, c, i)$: `producer` is a candidate of the group input
/// `input` of `consumer` reads, and whether it satisfies that input's
/// logical requirement (`producer_satisfies_logical_requirements`).
pub(crate) struct CandidateEdge {
    pub(crate) producer: CandidateForMemoGroupId,
    pub(crate) consumer: CandidateForMemoGroupId,
    pub(crate) input: usize,
    pub(crate) satisfies: Result<(), RootPruneReason>,
}

/// Every $(u, c, i)$ of `memo` (`Memo::producers`), grouped by consumer and
/// input, producers in group order. An edge into a readout of
/// `unknown_values` fails with `ValuesPerEvaluationUnknown`.
fn candidate_edges(
    memo: &Memo,
    unknown_values: &BTreeMap<CandidateForMemoGroupId, usize>,
    evidence: &dyn AccuracyEvidence,
) -> Vec<CandidateEdge> {
    let accepted = accepted_metrics(memo);
    let mut edges = Vec::new();
    for (group, memo_group) in memo.groups().iter().enumerate() {
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let consumer = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            for input in 0..candidate.expression.operator.children().len() {
                for producer in memo.producers(consumer, input) {
                    edges.push(CandidateEdge {
                        producer,
                        consumer,
                        input,
                        satisfies: match unknown_values.get(&consumer) {
                            Some(&query) => {
                                Err(RootPruneReason::ValuesPerEvaluationUnknown { consumer, query })
                            }
                            None => producer_satisfies_logical_requirements(
                                memo, &accepted, producer, consumer, input, evidence,
                            ),
                        },
                    });
                }
            }
        }
    }
    edges
}

/// Per group: the grid its result is evaluated on, the gcd of the
/// evaluation grids of the queries reaching it
/// (`QueryRecurrence::evaluation_grid_ms`) at phase 0; `None` when no query
/// reaches it or one has no known grid.
fn evaluation_grids(memo: &Memo, entries: &[QueryWorkloadEntry]) -> Vec<Option<EvaluationGrid>> {
    memo.groups()
        .iter()
        .map(|group| {
            let mut period_ms = 0;
            for &query in group.reached_by_queries() {
                let entry = entries.get(query)?;
                period_ms = greatest_common_divisor(
                    period_ms,
                    entry
                        .recurrence
                        .evaluation_grid_ms(entry.range_evaluation)?,
                );
            }
            (period_ms > 0).then_some(EvaluationGrid {
                period_ms,
                phase_ms: 0,
            })
        })
        .collect()
}
