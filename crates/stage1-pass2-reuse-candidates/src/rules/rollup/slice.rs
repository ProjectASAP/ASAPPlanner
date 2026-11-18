//! Slicing: the candidate computing a group from a group of the same keys
//! over more rows, a `Filter` on those keys over its states or values, or
//! over the same per-group readout of it.

use std::collections::BTreeSet;

use crate::rules::candidates::add_candidates_per_input_parameters;
use crate::Pass2RuleError;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

use super::compensation::{compensation, conjunction, selecting_columns};
use super::member::Member;

/// Adds to `narrow` the candidate `Filter(compensation, wide)` when the two
/// have the same keys, in the same order, and `narrow`'s selection lies
/// inside `wide`'s, narrower only on those keys. Then the same one level
/// up: to each group computed from `narrow` by a row-wise operator
/// ([`is_row_wise`], as its expression or a candidate's that no rule
/// added), from the group computed from `wide` by the same operator, and so
/// on. Returns whether `narrow` got its candidate.
///
/// For W = `sum by (pod) (rate(x[5m]))` and q' = `sum by (pod)
/// (rate(x{pod="a"}[5m]))`, q' gets `Filter(pod = "a", W)`. For a quantile
/// per city and the same for `city = 'NYC'`, the second's state, estimate
/// and output each get a `Filter` over the first's.
///
/// A filter on group keys keeps or drops whole groups, so it commutes with
/// the aggregation (exact or a summary's) and with any row-wise operator
/// that copies the keys, and it keeps each kept value's error. The memo
/// checks each candidate's output is its group's.
pub(super) fn slice_from(
    memo: &mut Memo,
    narrow: &Member,
    wide: &Member,
) -> Result<bool, Pass2RuleError> {
    if narrow.keys != wide.keys {
        return Ok(false);
    }
    let Some(narrowing) = compensation(narrow, wide) else {
        return Ok(false);
    };
    if narrowing.is_empty() {
        return Ok(false);
    }
    let candidates_before = memo.groups()[narrow.group.0 as usize].candidates.len();
    let mut pairs = vec![(narrow.group, wide.group)];
    let mut visited = BTreeSet::new();
    while let Some((narrow_group, wide_group)) = pairs.pop() {
        if !visited.insert((narrow_group, wide_group)) {
            continue;
        }
        let groups = memo.groups();
        let Some(conjuncts) = selecting_columns(
            &groups[wide_group.0 as usize]
                .group_properties
                .column_lineage,
            &narrowing,
        ) else {
            continue;
        };
        for &parent in groups[narrow_group.0 as usize].parent_groups() {
            let over_wide = operators_of(memo, parent)
                .filter(|operator| is_row_wise(operator) && operator.children() == [&narrow_group])
                .map(|operator| operator.map_children(|_| wide_group))
                .collect::<Vec<_>>();
            if let Some(&wide_parent) =
                groups[wide_group.0 as usize]
                    .parent_groups()
                    .iter()
                    .find(|&&candidate_parent| {
                        candidate_parent != parent
                            && operators_of(memo, candidate_parent)
                                .any(|operator| over_wide.contains(operator))
                    })
            {
                pairs.push((parent, wide_parent));
            }
        }
        let filter = Operator::NonASAP(NonASAPOp::Filter {
            pred: conjunction(conjuncts),
            child: wide_group,
        });
        add_candidates_per_input_parameters(
            memo,
            narrow_group,
            filter,
            OriginOfCandidateForMemoGroup::Rollup,
        )?;
    }
    Ok(memo.groups()[narrow.group.0 as usize].candidates.len() > candidates_before)
}

/// The operators computing `group` before Pass 2's rules: its expression
/// and its candidates' that no rule added (not `Rollup`), so the walk never
/// reads a `Filter` a rule added.
fn operators_of(memo: &Memo, group: MemoGroupId) -> impl Iterator<Item = &Operator<MemoGroupId>> {
    let group = &memo.groups()[group.0 as usize];
    std::iter::once(&group.expression).chain(
        group
            .candidates
            .iter()
            .filter(|candidate| candidate.origin != OriginOfCandidateForMemoGroup::Rollup)
            .map(|candidate| &candidate.expression.operator),
    )
}

/// Whether `expression` computes each output row from one row of its input:
/// a readout, a finalize, a `Project` or a `Filter`. Whether it copies the
/// keys is checked one level up, by their lineage.
fn is_row_wise(expression: &Operator<MemoGroupId>) -> bool {
    matches!(
        expression,
        Operator::ASAP(ASAPOp::SummaryEstimate { .. } | ASAPOp::FinalizeExactAccumulator { .. })
            | Operator::NonASAP(NonASAPOp::Project { .. } | NonASAPOp::Filter { .. })
    )
}
