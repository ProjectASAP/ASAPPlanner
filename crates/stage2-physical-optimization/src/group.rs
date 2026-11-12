//! Per-group facts the decision tree's levels 3 and 4 read: whether a
//! group's candidates are offered `IngestionTime`, and whether they are
//! offered `AcrossEvaluations`. Each is computed once per group, over the
//! candidates Pass 2 kept (an eliminated candidate reads nothing).

use std::collections::HashSet;

use asap_types::ir::operator::arguments::{AtModifier, TimeShift};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoGroupId,
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
/// `AcrossEvaluations`. It does for a chain, for a group evaluated on a
/// known grid (`evaluation_grid_of_group`) and for a time-independent
/// group (its one result is stored as is and readable at any time), unless
/// the group is read under an absolute `@ T`
/// ([`read_at_absolute_time_per_group`]).
pub(super) fn across_evaluations_offered_per_group(
    logical: &LogicalASAPMemoCandidates,
    eliminated: &HashSet<CandidateForMemoGroupId>,
) -> Vec<bool> {
    let read_at_absolute_time = read_at_absolute_time_per_group(logical, eliminated);
    logical
        .memo
        .groups()
        .iter()
        .zip(&logical.evaluation_grid_of_group)
        .zip(read_at_absolute_time)
        .map(|((group, evaluation_grid), read_at_absolute_time)| {
            let chain = matches!(
                group.expression,
                Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
            );
            (chain
                || evaluation_grid.is_some()
                || !group.group_properties.depends_on_evaluation_time)
                && !read_at_absolute_time
        })
        .collect()
}

/// Per group, by `MemoGroupId`: whether it is read under an absolute
/// `@ T`, that is below a `TimeShift` anchored at a timestamp, through any
/// operators except another `@` (which sets its own evaluation time). With
/// no known end of the horizon, how far back such a reader reaches (the
/// horizon end − T + range) is unbounded, so the group is not materialized
/// across evaluations (concepts doc, section 2): in
/// `max_over_time(rate(c[1m])[5m:1m] @ 90)` neither the subquery nor the
/// `rate`, its range or the scan is. A group also read on a relative path
/// counts as read under the anchor.
fn read_at_absolute_time_per_group(
    logical: &LogicalASAPMemoCandidates,
    eliminated: &HashSet<CandidateForMemoGroupId>,
) -> Vec<bool> {
    let anchor = |operator: &Operator<MemoGroupId>| match operator {
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift { at: Some(at), .. },
            ..
        }) => Some(matches!(at, AtModifier::Timestamp(_))),
        _ => None,
    };
    let group_count = logical.memo.groups().len();
    let mut read = vec![false; group_count];
    let mut pending = Vec::new();
    fn mark(input: &MemoGroupId, read: &mut [bool], pending: &mut Vec<usize>) {
        let input = input.0 as usize;
        if !read[input] {
            read[input] = true;
            pending.push(input);
        }
    }
    for group in 0..group_count {
        for candidate in kept_candidates(logical, eliminated, group) {
            if anchor(&candidate.expression.operator) == Some(true) {
                for input in candidate.expression.operator.children() {
                    mark(input, &mut read, &mut pending);
                }
            }
        }
    }
    while let Some(group) = pending.pop() {
        for candidate in kept_candidates(logical, eliminated, group) {
            if anchor(&candidate.expression.operator).is_none() {
                for input in candidate.expression.operator.children() {
                    mark(input, &mut read, &mut pending);
                }
            }
        }
    }
    read
}
