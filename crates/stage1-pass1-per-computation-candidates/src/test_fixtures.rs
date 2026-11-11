//! Fixtures this crate's own tests share. The memo builders add Stage 0's
//! groups through `Memo::find_or_add_group` and `Memo::add_query`, as
//! Stage 0 does, so no test calls another stage crate. The expressions
//! come from `asap_types::test_support::memo_fixtures`; its scans list Utf8
//! labels.

use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::series_identity::with_promql_series_identity;
use asap_types::ir::schema::DataType;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, Predicate, QueryRoot};
use asap_types::stages::{Memo, MemoGroupId, MemoQuery};
use asap_types::test_support::memo_fixtures::{aggregate_with, scan};

/// The group of `operator`, added as Stage 0 adds it.
pub fn group(memo: &mut Memo, operator: Operator<MemoGroupId>) -> MemoGroupId {
    memo.find_or_add_group(operator).unwrap()
}

/// The group of a `TimeSeries(metric)` scan listing `labels`, its series
/// identity realized as the planner realizes it before Stage 0
/// (`with_promql_series_identity`): a closed schema whose last column is
/// `$promql_series_identity`, keyed by it and the timestamp.
pub fn scan_group(memo: &mut Memo, metric: &str, labels: &[&str]) -> MemoGroupId {
    let labels: Vec<(&str, DataType)> = labels.iter().map(|l| (*l, DataType::Utf8)).collect();
    let mut operator = scan(metric, &labels, vec![]);
    let open =
        OperatorNode::new_shared(operator.map_children(|_| unreachable!("a scan has no input")))
            .unwrap();
    let realized = with_promql_series_identity(&open).unwrap().schema.clone();
    if let Operator::NonASAP(NonASAPOp::Scan { schema, .. }) = &mut operator {
        *schema = realized;
    }
    group(memo, operator)
}

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

/// Adds a query whose root is group `root`.
pub fn query(memo: &mut Memo, root: MemoGroupId) -> usize {
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(root),
        accuracy_target: None,
        number_of_values_per_query_evaluation: None,
    })
}
