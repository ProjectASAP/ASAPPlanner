//! Rule 6, rollup: a coarse grouped summary state read from a finer one's
//! states, merged into the coarse groups.

use crate::memo::rules::{add_candidate, expression, Pass2RuleError};
use asap_types::ir::operator::operator_properties::Source;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::schema::{FamilyCapability, GroupingStrategy};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// Rule 6: to each grouped summary state group (a `SummaryAgg` by keys,
/// exact accumulator or sketch, whose family merges across groups,
/// `FamilyCapability::merges_groups`), for each finer state group of the
/// same family, update and filter over the same rows ([`same_rows`]) whose
/// keys strictly include its keys (by name), the candidate
/// `SummaryMerge(group_by: its keys as the finer state's columns)` over the
/// finer group. The groups of one state partition its rows, so the merged
/// state is the coarse build's: a sum, count, minimum or maximum stays
/// exact; a sketch keeps its family's merged guarantee, which the readout's
/// edge checks (`FamilyCapability::merged_error`). The finer group's
/// windowed builds (rule 1) serve the coarse state through it, so a coarse
/// and a finer query over the same range share one tumbling-window chain.
pub(crate) fn add_rollup_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let states: Vec<MemoGroupId> = (0..memo.groups().len())
        .map(|index| MemoGroupId(index as u32))
        .filter(|&group| grouped_state(memo, group).is_some())
        .collect();
    for &coarse in &states {
        for &fine in &states {
            if coarse == fine {
                continue;
            }
            let Some(group_by) = rollup_keys(memo, coarse, fine) else {
                continue;
            };
            add_candidate(
                memo,
                coarse,
                Operator::ASAP(ASAPOp::SummaryMerge {
                    children: vec![fine],
                    newest_tumbling_windows: None,
                    group_by: Some(group_by),
                }),
                None,
                None,
                None,
                OriginOfCandidateForMemoGroup::Rollup,
            )?;
        }
    }
    Ok(())
}

/// The names of a grouped state's keys, when `group` is a `SummaryAgg` by
/// keys (not `without`), one instance per group, whose family merges
/// across groups.
fn grouped_state(memo: &Memo, group: MemoGroupId) -> Option<Vec<String>> {
    let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        reduction: Reduction::Reduce(by),
        grouping: GroupingStrategy::PerSubpopulationInstance,
        ..
    }) = expression(memo, group)
    else {
        return None;
    };
    if by.is_without() || !FamilyCapability::merges_groups(family) {
        return None;
    }
    let fields = &memo.groups()[child.0 as usize]
        .group_properties
        .schema
        .fields;
    by.keys()
        .iter()
        .map(|&key| fields.get(key).map(|field| field.name.clone()))
        .collect()
}

/// The finer state's output columns that hold `coarse`'s keys, in
/// `coarse`'s key order, when `fine` is a finer build of the same state:
/// the same family, update, grouping and filter over the same rows, with
/// keys that strictly include `coarse`'s (by name). `None` otherwise.
fn rollup_keys(memo: &Memo, coarse: MemoGroupId, fine: MemoGroupId) -> Option<Vec<usize>> {
    let (
        Operator::ASAP(ASAPOp::SummaryAgg {
            child,
            family,
            input,
            grouping,
            filter,
            ..
        }),
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: fine_child,
            family: fine_family,
            input: fine_input,
            grouping: fine_grouping,
            filter: fine_filter,
            ..
        }),
    ) = (expression(memo, coarse), expression(memo, fine))
    else {
        return None;
    };
    let (coarse_keys, fine_keys) = (grouped_state(memo, coarse)?, grouped_state(memo, fine)?);
    if family != fine_family
        || input != fine_input
        || grouping != fine_grouping
        || filter != fine_filter
        // A filter's columns are its own child's: over two children it
        // compares only by position.
        || (filter.is_some() && child != fine_child)
        || fine_keys.len() <= coarse_keys.len()
        || !coarse_keys.iter().all(|key| fine_keys.contains(key))
        || !same_rows(memo, *child, *fine_child)
    {
        return None;
    }
    let state = &memo.groups()[fine.0 as usize]
        .group_properties
        .schema
        .fields;
    coarse_keys
        .iter()
        .map(|key| state.iter().position(|field| &field.name == key))
        .collect()
}

/// Whether two groups hold the same rows up to the labels they list: one
/// group; two scans of one metric without predicates; equal time ranges or
/// time shifts over the same rows; or equal per-series reductions of one
/// `avg`, `sum`, `min` or `max` of the sample value over the same rows.
fn same_rows(memo: &Memo, left: MemoGroupId, right: MemoGroupId) -> bool {
    if left == right {
        return true;
    }
    match (expression(memo, left), expression(memo, right)) {
        (
            Operator::NonASAP(NonASAPOp::Scan {
                source: source @ Source::TimeSeries { .. },
                predicates,
                ..
            }),
            Operator::NonASAP(NonASAPOp::Scan {
                source: other_source,
                predicates: other_predicates,
                ..
            }),
        ) => source == other_source && predicates.is_empty() && other_predicates.is_empty(),
        (
            Operator::NonASAP(NonASAPOp::TimeRange { range, kind, child }),
            Operator::NonASAP(NonASAPOp::TimeRange {
                range: other_range,
                kind: other_kind,
                child: other_child,
            }),
        ) => range == other_range && kind == other_kind && same_rows(memo, *child, *other_child),
        (
            Operator::NonASAP(NonASAPOp::TimeShift { shift, child }),
            Operator::NonASAP(NonASAPOp::TimeShift {
                shift: other_shift,
                child: other_child,
            }),
        ) => shift == other_shift && same_rows(memo, *child, *other_child),
        (
            Operator::NonASAP(NonASAPOp::Aggregate {
                child,
                reduction: Reduction::PerEntity,
                measures,
                filters,
                having: None,
                ..
            }),
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: other_child,
                reduction: Reduction::PerEntity,
                measures: other_measures,
                filters: other_filters,
                having: None,
                ..
            }),
        ) => {
            matches!(
                measures.as_slice(),
                [AggIntent::Avg { col: None }
                    | AggIntent::Sum { col: None }
                    | AggIntent::Min { col: None }
                    | AggIntent::Max { col: None }]
            ) && measures == other_measures
                && filters.iter().all(Option::is_none)
                && other_filters.iter().all(Option::is_none)
                && same_rows(memo, *child, *other_child)
        }
        _ => false,
    }
}
