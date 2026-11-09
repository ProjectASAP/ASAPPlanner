//! Fixtures this crate's own tests share. The memo builders add Stage 0's
//! groups through `Memo::find_or_add_group` and `Memo::add_query`, as
//! Stage 0 does, so no test calls another stage crate.

use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::schema::{ColumnId, DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, Predicate, QueryRoot};
use asap_types::stages::{Memo, MemoGroupId, MemoQuery};

/// `[ts(0), value(1), labels...]`, time index 0, with `unique_keys`.
pub fn metric_schema(labels: &[&str], unique_keys: Vec<Vec<ColumnId>>) -> Schema {
    let mut columns = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    columns.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
    Schema::with_time_index(columns, 0, unique_keys)
}

/// A predicate-free `TimeSeries(metric)` scan with `schema`.
pub fn scan_operator<C>(metric: &str, schema: Schema) -> Operator<C> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema,
    })
}

/// The group of `operator`, added as Stage 0 adds it.
pub fn group(memo: &mut Memo, operator: Operator<MemoGroupId>) -> MemoGroupId {
    memo.find_or_add_group(operator).unwrap()
}

/// The group of a `TimeSeries(metric)` scan over [`metric_schema`] with its
/// series identity realized as the planner realizes it before Stage 0
/// (`with_promql_series_identity`): a closed schema whose last column is
/// `$promql_series_identity`, keyed by it and the timestamp.
pub fn scan_group(memo: &mut Memo, metric: &str, labels: &[&str]) -> MemoGroupId {
    let open =
        OperatorNode::new_shared(scan_operator(metric, metric_schema(labels, vec![]))).unwrap();
    let realized = with_promql_series_identity(&open).unwrap();
    group(memo, scan_operator(metric, realized.schema.clone()))
}

/// The group of an `Aggregate` of `measures` (named by their intents) over
/// `child`, grouped by `reduction`, unfiltered, with `having`.
pub fn aggregate_group(
    memo: &mut Memo,
    child: MemoGroupId,
    reduction: Reduction,
    measures: Vec<AggIntent>,
    having: Option<Predicate<MemoGroupId>>,
) -> MemoGroupId {
    let count = measures.len();
    group(
        memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child,
            reduction,
            measures,
            output_names: vec![String::new(); count],
            filters: vec![None; count],
            having,
        }),
    )
}

/// Adds a query whose root is group `root`.
pub fn query(memo: &mut Memo, root: MemoGroupId) -> usize {
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(root),
        accuracy_target: None,
        values_per_evaluation: None,
    })
}
