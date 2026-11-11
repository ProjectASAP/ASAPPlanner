//! `asap-stage1-pass2-reuse-candidates` — Stage 1, Pass 2: the reuse
//! rules over memo groups, the readouts' required accuracy, dead-candidate
//! elimination and the logical edges. Its entry,
//! [`generate_logical_asap_memo_candidates`], extends Pass 1's memo into
//! the [`LogicalASAPMemoCandidates`](asap_types::stages::LogicalASAPMemoCandidates)
//! Stage 2 expands, e.g. `sum by (job)` and `sum by (job, instance)` of one
//! count reading one state.
//!
//! It depends on `asap-types`, where its input and output are, and on
//! `asap-accuracy` for $\alpha^{\log}$. It depends on no other stage crate,
//! and the planner facade calls it after Pass 1.

/// Step 4: dead-candidate elimination and its root causes.
mod dead_candidates;
/// Step 3: reach, caps and readout requirements.
mod requirements;
/// Steps 1, 2 and 5: the rules that add groups and candidates.
mod rules;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use asap_accuracy::{accepted_metrics, producer_satisfies_logical_requirements, AccuracyEvidence};
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{
    CandidateForMemoGroupId, EvaluationGrid, LogicalASAPMemoCandidates, LogicalEdge, Memo,
    MemoGroupId, PrunedLogicalCandidate, RootPruneReason,
};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};
use dead_candidates::eliminate_dead_candidates;
use requirements::set_readout_requirements;
pub use rules::Pass2RuleError;
use rules::{chains, rollup, topk};

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
    let before_chains = eliminate_dead_candidates(
        &memo,
        &candidate_edges(&memo, entries, &unknown_values, evidence),
    );
    chains::add_chain_groups(&mut memo, entries, &dead(&before_chains))?;
    let candidate_edges = candidate_edges(&memo, entries, &unknown_values, evidence);
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
/// `unknown_values` fails with `ValuesPerEvaluationUnknown`. A chain
/// `SummaryTumblingWindowChain[w]` has an edge only into a consumer group
/// whose result does not depend on the evaluation time or whose
/// evaluation grid ([`evaluation_grids`]) $w$ divides: a chain of another
/// reader's width does not end at this reader's evaluations.
fn candidate_edges(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    unknown_values: &BTreeMap<CandidateForMemoGroupId, usize>,
    evidence: &dyn AccuracyEvidence,
) -> Vec<CandidateEdge> {
    let accepted = accepted_metrics(memo);
    let evaluation_grid_of_group = evaluation_grids(memo, entries);
    let mut edges = Vec::new();
    for (group, memo_group) in memo.groups().iter().enumerate() {
        let ends_on_the_evaluations =
            |input_group: MemoGroupId| match memo.groups()[input_group.0 as usize].expression {
                Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                    tumbling_window_width_ms,
                    ..
                }) if memo_group.group_properties.depends_on_evaluation_time => {
                    evaluation_grid_of_group[group]
                        .is_some_and(|grid| grid.period_ms.is_multiple_of(tumbling_window_width_ms))
                }
                _ => true,
            };
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let consumer = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            for (input, &input_group) in candidate
                .expression
                .operator
                .children()
                .into_iter()
                .enumerate()
            {
                if !ends_on_the_evaluations(input_group) {
                    continue;
                }
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
