//! Per-group facts the decision tree's levels 3 and 4 read: whether a
//! group's candidates are offered `IngestionTime`, and whether they are
//! offered `AcrossEvaluations`. Each is computed once per group, over the
//! candidates Pass 2 kept (an eliminated candidate reads nothing).

use std::collections::HashSet;

use asap_types::ir::operator::arguments::AtModifier;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId,
};

/// The candidates of group `group` that Pass 2 kept.
fn kept_candidates<'a>(
    logical: &'a LogicalASAPMemoCandidates,
    eliminated: &'a HashSet<CandidateForMemoGroupId>,
    group: usize,
) -> impl Iterator<Item = &'a CandidateForMemoGroup> + 'a {
    logical.memo.groups()[group]
        .candidates
        .iter()
        .enumerate()
        .filter(move |(index_in_group, _)| {
            !eliminated.contains(&CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group: *index_in_group,
            })
        })
        .map(|(_, candidate)| candidate)
}

/// Per group, by `MemoGroupId`: whether level 3 offers its candidates
/// `IngestionTime`. It does when the group is time-independent (it does
/// not depend on the evaluation time, as a chain does not), or when it is
/// an input, directly or through other offered groups, of a candidate of
/// an offered group: an ingestion-time candidate reads only ingestion-time
/// producers.
pub(super) fn ingestion_time_offered_per_group(
    logical: &LogicalASAPMemoCandidates,
    eliminated: &HashSet<CandidateForMemoGroupId>,
) -> Vec<bool> {
    let groups = logical.memo.groups();
    let mut offered: Vec<bool> = groups
        .iter()
        .map(|group| !group.group_properties.depends_on_evaluation_time)
        .collect();
    let mut pending: Vec<usize> = (0..groups.len()).filter(|&group| offered[group]).collect();
    while let Some(group) = pending.pop() {
        for candidate in kept_candidates(logical, eliminated, group) {
            for input in candidate.expression.operator.children() {
                let input = input.0 as usize;
                if !offered[input] {
                    offered[input] = true;
                    pending.push(input);
                }
            }
        }
    }
    offered
}

/// Per group, by `MemoGroupId`: whether level 4 offers its candidates
/// `AcrossEvaluations`. It does for a chain and for a group evaluated on a
/// known grid (`evaluation_grid_of_group`), unless the group is read under
/// an absolute `@ T` ([`read_at_absolute_time`]).
pub(super) fn across_evaluations_offered_per_group(
    logical: &LogicalASAPMemoCandidates,
    _eliminated: &HashSet<CandidateForMemoGroupId>,
) -> Vec<bool> {
    let memo = &logical.memo;
    memo.groups()
        .iter()
        .zip(&logical.evaluation_grid_of_group)
        .enumerate()
        .map(|(group_index, (group, evaluation_grid))| {
            let chain = matches!(
                group.expression,
                Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
            );
            (chain || evaluation_grid.is_some())
                && !read_at_absolute_time(memo, MemoGroupId(group_index as u32))
        })
        .collect()
}

/// Whether `group` is read under an absolute `@ T`: a `TimeShift` anchored
/// at a timestamp reads it, directly or through `TimeShift`s without `@`.
/// With no known end of the horizon, how far back that reader reaches (the
/// horizon end − T + range) is unbounded, so `group` is not materialized
/// across evaluations (concepts doc, section 5); `x @ 1700000000 [5m]`
/// reads `x` at query time.
fn read_at_absolute_time(memo: &Memo, group: MemoGroupId) -> bool {
    memo.groups()[group.0 as usize]
        .parent_groups()
        .iter()
        .any(
            |&parent| match &memo.groups()[parent.0 as usize].expression {
                Operator::NonASAP(NonASAPOp::TimeShift { shift, child }) if *child == group => {
                    match shift.at {
                        Some(AtModifier::Timestamp(_)) => true,
                        None => read_at_absolute_time(memo, parent),
                        Some(_) => false,
                    }
                }
                _ => false,
            },
        )
}
