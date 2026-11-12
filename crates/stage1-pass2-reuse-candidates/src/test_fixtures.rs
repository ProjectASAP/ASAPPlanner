//! Fixtures this crate's own tests share, beside the memo builders of
//! `asap_types::test_support::memo_builders` (groups with the candidates
//! Pass 1 would add, queries), so no test calls another stage crate: the
//! group builders Pass 2's tests alone use, Pass 2's run and readers of
//! its output.
use std::collections::BTreeSet;

use asap_accuracy::NoMeasuredGuarantees;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId,
    PrunedLogicalCandidate, RequiredAccuracy,
};
use asap_types::summary_type::ExactKind;
use asap_types::test_support::memo_builders::group;
use asap_types::test_support::memo_fixtures;
use asap_types::workload::{
    Predictability, Query, QueryRecurrence, QueryRequirements, QueryWorkloadEntry, RepeatedDemand,
    RepetitionInterval, TimestampMs,
};

use crate::generate_logical_asap_memo_candidates;

/// An exact accumulator state of `kind` over `window` grouped by
/// `reduction`, read by a `FinalizeExactAccumulator` group (the query
/// root): what Pass 1 offers for `sum_over_time` (per series) or for a
/// `sum by (keys)` of `count_over_time` (the count by keys).
pub fn exact_state(
    memo: &mut Memo,
    window: MemoGroupId,
    kind: ExactKind,
    reduction: Reduction,
) -> (MemoGroupId, MemoGroupId) {
    let state = group(
        memo,
        memo_fixtures::state_grouped_by(FieldDataType::exact(kind), reduction, window),
    );
    let finalized = group(
        memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state }),
    );
    (state, finalized)
}

/// A query repeating every `interval_ms` at phase 0 (`FixedIntervalAt`).
pub fn entry(interval_ms: u32) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Query("q".into()),
        requirements: QueryRequirements::default(),
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(0),
        }),
        range_evaluation: None,
    }
}

pub fn pass2(memo: Memo, entries: &[QueryWorkloadEntry]) -> LogicalASAPMemoCandidates {
    generate_logical_asap_memo_candidates(memo, entries, &NoMeasuredGuarantees).unwrap()
}

pub fn candidates_of(memo: &Memo, group: MemoGroupId) -> &[CandidateForMemoGroup] {
    &memo.groups()[group.0 as usize].candidates
}

/// The edges into input `input` of `consumer`: each producer and α^log.
pub fn edges_into(
    logical: &LogicalASAPMemoCandidates,
    consumer: CandidateForMemoGroupId,
    input: usize,
) -> Vec<(CandidateForMemoGroupId, bool)> {
    logical
        .edges
        .iter()
        .filter(|edge| {
            edge.consumer_candidate_id == consumer && edge.input_index_of_consumer == input
        })
        .map(|edge| {
            (
                edge.producer_candidate_id,
                edge.satisfies_logical_requirement,
            )
        })
        .collect()
}

/// The pruned entry of `candidate`, if Pass 2 eliminated it.
pub fn pruned_entry(
    logical: &LogicalASAPMemoCandidates,
    candidate: CandidateForMemoGroupId,
) -> Option<&PrunedLogicalCandidate> {
    logical
        .pruned
        .iter()
        .find(|pruned| pruned.candidate == candidate)
}

/// The required accuracy of each readout candidate of `group` (its only
/// input's).
pub fn requirements_of(memo: &Memo, group: MemoGroupId) -> Vec<Option<RequiredAccuracy>> {
    candidates_of(memo, group)
        .iter()
        .filter(|c| {
            matches!(
                c.expression.operator,
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            )
        })
        .map(|c| c.input_requirements[0].required_accuracy)
        .collect()
}

/// The tumbling-window widths of the chains whose merges state group
/// `state` has as candidates.
pub fn chain_widths_serving(memo: &Memo, state: MemoGroupId) -> BTreeSet<u64> {
    candidates_of(memo, state)
        .iter()
        .filter_map(|c| match &c.expression.operator {
            Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                group_by: None,
            }) => chain_width_below(memo, children[0]),
            _ => None,
        })
        .collect()
}

/// The width of the tumbling-window chain `group` reads through
/// `TimeRange` and `TimeShift` groups, if any.
pub fn chain_width_below(memo: &Memo, mut group: MemoGroupId) -> Option<u64> {
    loop {
        match &memo.groups()[group.0 as usize].expression {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => return Some(*tumbling_window_width_ms),
            Operator::NonASAP(
                NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
            ) => group = *child,
            _ => return None,
        }
    }
}

/// The groups whose expression matches `pattern`.
pub fn groups_where(
    memo: &Memo,
    pattern: impl Fn(&Operator<MemoGroupId>) -> bool,
) -> Vec<MemoGroupId> {
    memo.groups()
        .iter()
        .enumerate()
        .filter(|(_, group)| pattern(&group.expression))
        .map(|(index, _)| MemoGroupId(index as u32))
        .collect()
}

/// The group whose expression is `operator`, if the memo has one.
pub fn group_of(memo: &Memo, operator: &Operator<MemoGroupId>) -> Option<MemoGroupId> {
    groups_where(memo, |expression| expression == operator)
        .first()
        .copied()
}
