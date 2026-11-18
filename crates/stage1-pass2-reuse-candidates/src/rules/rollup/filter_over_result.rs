//! The candidate computing a group from a group of the same keys over more
//! rows: a `Filter` on those keys over its result, or over the same
//! per-group readout of it.

use crate::rules::candidates::add_candidates_per_input_parameters;
use crate::Pass2RuleError;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

use super::member::Member;
use super::rewrite::{compensation, conjunction, selecting_columns};

/// Adds to `narrow` the candidate `Filter(compensation, wide)` when the two
/// have the same keys, in the same order, and `narrow`'s selection lies
/// inside `wide`'s, narrower only on those keys; then the same one level
/// up, to each group computed from `narrow` by a per-group operator
/// ([`per_group`], as its expression or a candidate's) from the group
/// computed from `wide` by the same operator, and so on. For W = `sum by (pod) (rate(x[5m]))` and q' = `sum by (pod)
/// (rate(x{pod="a"}[5m]))`, q' gets `Filter(pod = "a", W)`; for a quantile
/// per city and the same for `city = 'NYC'`, both the state and the
/// estimate of the second get theirs.
///
/// A filter on group keys keeps or drops whole groups, so it commutes with
/// the aggregation (exact or a summary's) and with any per-group operator
/// that copies the keys, and it keeps each kept value's error. The memo
/// checks each candidate's output is its group's.
pub(super) fn filter_from(
    memo: &mut Memo,
    narrow: &Member,
    wide: &Member,
) -> Result<(), Pass2RuleError> {
    if narrow.keys != wide.keys {
        return Ok(());
    }
    let Some(compensation) = compensation(narrow, wide) else {
        return Ok(());
    };
    if compensation.is_empty() {
        return Ok(());
    }
    let mut pairs = vec![(narrow.group, wide.group)];
    while let Some((narrow, wide)) = pairs.pop() {
        let groups = memo.groups();
        let Some(conjuncts) = selecting_columns(
            &groups[wide.0 as usize].group_properties.column_lineage,
            &compensation,
        ) else {
            continue;
        };
        for &parent in groups[narrow.0 as usize].parent_groups() {
            let over_wide = operators_of(memo, parent)
                .filter(|operator| per_group(operator) && operator.children().contains(&&narrow))
                .map(|operator| {
                    operator.map_children(|&child| if child == narrow { wide } else { child })
                })
                .collect::<Vec<_>>();
            if let Some(&wide_parent) = groups[wide.0 as usize]
                .parent_groups()
                .iter()
                .find(|&&known| operators_of(memo, known).any(|known| over_wide.contains(known)))
            {
                pairs.push((parent, wide_parent));
            }
        }
        let filter = Operator::NonASAP(NonASAPOp::Filter {
            pred: conjunction(conjuncts),
            child: wide,
        });
        add_candidates_per_input_parameters(
            memo,
            narrow,
            filter,
            OriginOfCandidateForMemoGroup::Rollup,
        )?;
    }
    Ok(())
}

/// The operators computing `group`: its expression and its candidates'.
fn operators_of(memo: &Memo, group: MemoGroupId) -> impl Iterator<Item = &Operator<MemoGroupId>> {
    let group = &memo.groups()[group.0 as usize];
    std::iter::once(&group.expression).chain(
        group
            .candidates
            .iter()
            .map(|candidate| &candidate.expression.operator),
    )
}

/// Whether `expression` computes each output row from one row of its input
/// group, the group's keys with it: a readout, a finalize, a `Project` or a
/// `Filter`.
fn per_group(expression: &Operator<MemoGroupId>) -> bool {
    matches!(
        expression,
        Operator::ASAP(ASAPOp::SummaryEstimate { .. } | ASAPOp::FinalizeExactAccumulator { .. })
            | Operator::NonASAP(NonASAPOp::Project { .. } | NonASAPOp::Filter { .. })
    )
}
