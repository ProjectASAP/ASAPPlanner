//! Where an aggregate's `HAVING` sits in the memo: in its own `having` or in
//! a consumer `Filter` group directly above it.

use asap_types::ir::{NonASAPOp, Operator, Predicate};
use asap_types::stages::{Memo, MemoGroupId};

use crate::expression;

/// The consumer `Filter` groups directly above `aggregate_group` (its
/// `HAVING` as the SQL frontend lowers it), with their predicates. A
/// `Filter` that reads the aggregate from a scalar expression stays a plain
/// consumer.
pub(super) fn having_filter_groups(
    memo: &Memo,
    aggregate_group: MemoGroupId,
) -> impl Iterator<Item = (MemoGroupId, &Predicate<MemoGroupId>)> {
    memo.groups()[aggregate_group.0 as usize]
        .parent_groups()
        .iter()
        .filter_map(move |&parent| match expression(memo, parent) {
            Operator::NonASAP(NonASAPOp::Filter { child, pred }) if *child == aggregate_group => {
                Some((parent, pred))
            }
            _ => None,
        })
}

/// Whether `aggregate_group` has a `HAVING`: its own `having` or a consumer
/// `Filter` group directly above it ([`having_filter_groups`]).
pub(crate) fn has_having(memo: &Memo, aggregate_group: MemoGroupId) -> bool {
    matches!(
        expression(memo, aggregate_group),
        Operator::NonASAP(NonASAPOp::Aggregate {
            having: Some(_),
            ..
        })
    ) || having_filter_groups(memo, aggregate_group).next().is_some()
}
