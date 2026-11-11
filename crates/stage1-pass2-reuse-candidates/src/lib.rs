//! `asap-stage1-pass2-reuse-candidates` — Stage 1, Pass 2: the reuse
//! rules over memo groups, the readouts' required accuracy, dead-candidate
//! elimination and the logical edges. Its entry,
//! [`generate_logical_asap_memo_candidates`], extends Pass 1's memo into
//! the [`LogicalASAPMemoCandidates`](asap_types::stages::LogicalASAPMemoCandidates)
//! Stage 2 expands, e.g. `sum by (job)` and `sum by (job, instance)` of one
//! count reading one state.
//!
//! It depends on `asap-types`, where its input and output are, and on
//! `asap-accuracy` for the readout levels and $\alpha^{\log}$. It depends on
//! no other stage crate, and the planner facade calls it after Pass 1.

/// Step 4: dead-candidate elimination and its root causes.
mod dead_candidates;
/// Step 6: the possible edges and their $\alpha^{\log}$.
mod edges;
/// Step 3: readout requirements.
mod readout_requirements;
/// Steps 1, 2 and 5: the rules that add groups and candidates.
mod rules;
#[cfg(test)]
mod test_fixtures;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use asap_accuracy::AccuracyEvidence;
use asap_types::stages::{
    CandidateError, CandidateForMemoGroupId, EvaluationGrid, LogicalASAPMemoCandidates,
    LogicalEdge, Memo, MemoGroupError, PrunedLogicalCandidate,
};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};
use dead_candidates::eliminate_dead_candidates;
use edges::candidate_edges;
use readout_requirements::set_readout_requirements;
use rules::{chains, rollup, topk};

/// Why Pass 2 failed on a memo: a rule built an expression the node rules
/// reject (a bug in the rule), or the memo's reads form a cycle.
#[derive(Debug, thiserror::Error)]
pub enum Pass2RuleError {
    /// A group, or one of its candidates, reads itself through other groups.
    #[error("memo dependencies contain a cycle")]
    CyclicDependencies,
    /// A rule built a group expression the node rules reject.
    #[error("a rule built a group that breaks the node rules: {0}")]
    Group(#[from] MemoGroupError),
    /// A rule built a candidate expression the node rules reject, or gave
    /// it θ its group's output cannot have.
    #[error("a rule built a candidate that breaks the node rules: {0}")]
    Candidate(CandidateError),
}

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

/// Per group: the grid its result is evaluated on, the gcd of the
/// evaluation grids of the queries reaching it
/// ([`query_evaluation_grid_ms`]) at phase 0; `None` when no query reaches
/// it or one has no known grid.
fn evaluation_grids(memo: &Memo, entries: &[QueryWorkloadEntry]) -> Vec<Option<EvaluationGrid>> {
    memo.groups()
        .iter()
        .map(|group| {
            let mut period_ms = 0;
            for &query in group.reached_by_queries() {
                period_ms =
                    greatest_common_divisor(period_ms, query_evaluation_grid_ms(entries, query)?);
            }
            (period_ms > 0).then_some(EvaluationGrid {
                period_ms,
                phase_ms: 0,
            })
        })
        .collect()
}

/// The evaluation grid of query `query` (`entries[query]`'s
/// `QueryRecurrence::evaluation_grid_ms`); `None` when it has no known
/// grid.
fn query_evaluation_grid_ms(entries: &[QueryWorkloadEntry], query: usize) -> Option<u64> {
    let entry = entries.get(query)?;
    entry.recurrence.evaluation_grid_ms(entry.range_evaluation)
}
