//! Fixtures this crate's own tests share, beside the memo builders of
//! `asap_types::test_support::memo_builders` (`group`, `scan_group`,
//! `query`), so no test calls another stage crate.

use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::Predicate;
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::test_support::memo_builders::group;
use asap_types::test_support::memo_fixtures::aggregate_with;

/// The group of an `Aggregate` of `measures` (unnamed, unfiltered) over
/// `child`, grouped by `reduction`, with `having`.
pub fn aggregate_group(
    memo: &mut Memo,
    child: MemoGroupId,
    reduction: Reduction,
    measures: Vec<AggIntent>,
    having: Option<Predicate<MemoGroupId>>,
) -> MemoGroupId {
    let measures = measures.into_iter().map(|m| (m, None)).collect();
    group(memo, aggregate_with(reduction, measures, having, child))
}
