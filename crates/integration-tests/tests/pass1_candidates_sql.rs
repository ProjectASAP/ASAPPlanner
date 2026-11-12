//! SQL → Pass 1 memo candidates: where each `HAVING` is applied and what a
//! Top-K heap sketch ranks.

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_stage1_pass1_per_computation_candidates::pass1_memo;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, Predicate, QueryRoot, ScalarExpr};
use asap_types::stages::Memo;
use asap_types::summary_type::{SketchAlgorithm, SummaryInputExpr};

/// Pass 1's memo of `sql` over `flows(src, bytes)`.
async fn pass1(sql: &str) -> Memo {
    pass1_over(
        vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ],
        sql,
    )
    .await
}

/// Pass 1's memo of `sql` over `flows(fields)`.
async fn pass1_over(fields: Vec<Field>, sql: &str) -> Memo {
    let catalog = SqlCatalog::new().with_table("flows", Schema::new(fields));
    let root = lower_sql(sql, &catalog)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
    let memo = asap_stage0_memo::memo_of_queries(
        &[QueryRoot::Operator(root)],
        &[None],
        &Default::default(),
    )
    .unwrap();
    pass1_memo(memo, None).unwrap_or_else(|e| panic!("Pass 1 failed for {sql:?}: {e}"))
}

/// Whether some candidate applies a `HAVING` below the aggregate: a
/// `Filter` group over the aggregate's input, which a state build reads.
fn having_pushed_below(memo: &Memo) -> bool {
    let groups = memo.groups();
    groups.iter().any(|group| {
        let Operator::NonASAP(NonASAPOp::Aggregate { child, .. }) = &group.expression else {
            return false;
        };
        groups.iter().any(|filter| {
            matches!(&filter.expression, Operator::NonASAP(NonASAPOp::Filter { child: input, .. }) if input == child)
        })
    })
}

/// A key-only `HAVING` is pushed below the aggregate.
#[tokio::test]
async fn key_only_having_is_pushed_below() {
    let memo = pass1("SELECT src, count(*) FROM flows GROUP BY src HAVING src = 'a'").await;
    assert!(having_pushed_below(&memo));
}

/// A `HAVING` over an aggregate value stays above the output.
#[tokio::test]
async fn value_having_stays_above() {
    let memo = pass1("SELECT src, count(*) FROM flows GROUP BY src HAVING count(*) > 10").await;
    assert!(!having_pushed_below(&memo));
}

/// A global aggregate's `HAVING`, even a constant one, stays above the output.
#[tokio::test]
async fn global_constant_having_stays_above() {
    let memo = pass1("SELECT count(*) FROM flows HAVING false").await;
    assert!(!having_pushed_below(&memo));
}

/// A `HAVING` that reads a key and a value pushes its key conjunct below
/// the aggregate and keeps its value conjunct above: some candidate of the
/// `HAVING`'s `Filter` group filters by `count(*) > 10` alone.
#[tokio::test]
async fn mixed_having_pushes_its_key_conjunct_below() {
    let memo =
        pass1("SELECT src, count(*) FROM flows GROUP BY src HAVING src = 'a' AND count(*) > 10")
            .await;
    assert!(having_pushed_below(&memo));
    let (filter, value_conjunct) = memo
        .groups()
        .iter()
        .find_map(|group| match &group.expression {
            Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::BoolAnd(conjuncts)),
                ..
            }) => Some((group, conjuncts[1].clone())),
            _ => None,
        })
        .expect("the HAVING's Filter group");
    assert!(filter.candidates.iter().any(|candidate| matches!(
        &candidate.expression.operator,
        Operator::NonASAP(NonASAPOp::Filter { pred, .. }) if pred.0 == value_conjunct
    )));
}

/// The item and weight columns of every heap sketch state group in Pass 1's
/// memo of `sql` over `flows(key: key_type, bytes)`.
async fn heap_updates(key: &str, key_type: DataType, sql: &str) -> Vec<(String, String)> {
    heap_updates_over(
        vec![
            Field::plain(key, key_type, false),
            Field::plain("bytes", DataType::Float64, false),
        ],
        sql,
    )
    .await
}

/// [`heap_updates`] over `flows(fields)`.
async fn heap_updates_over(fields: Vec<Field>, sql: &str) -> Vec<(String, String)> {
    let memo = pass1_over(fields, sql).await;
    let name = |expr: &SummaryInputExpr| match expr {
        SummaryInputExpr::Column(ColumnRef::Named(name)) => name.clone(),
        // The ranked value when it is the child's one sample value.
        SummaryInputExpr::Column(ColumnRef::SampleValue) => "<sample value>".into(),
        SummaryInputExpr::Tuple(items) => match items.as_slice() {
            [SummaryInputExpr::Column(ColumnRef::Named(name))] => name.clone(),
            other => panic!("one item column: {other:?}"),
        },
        SummaryInputExpr::Constant(value) => value.to_string(),
        other => panic!("a named column or constant: {other:?}"),
    };
    memo.groups()
        .iter()
        .filter_map(|group| match &group.expression {
            Operator::ASAP(ASAPOp::SummaryAgg {
                summary_type:
                    FieldDataType::Sketch(
                        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap,
                    ),
                input,
                ..
            }) => Some((name(input.item.as_ref().unwrap()), name(&input.weight))),
            _ => None,
        })
        .collect()
}

const COUNT_RANKED_TOPK: &str =
    "SELECT KEY, count(*) FROM flows GROUP BY KEY ORDER BY count(*) DESC LIMIT 10";

/// An Int64 key is not mistaken for the ranked value: both heap sketches
/// are built over raw rows, keyed by the key with unit weight.
#[tokio::test]
async fn topk_heap_over_an_int64_key_counts_raw_rows() {
    let updates = heap_updates(
        "port",
        DataType::Int64,
        &COUNT_RANKED_TOPK.replace("KEY", "port"),
    )
    .await;
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates
            .iter()
            .all(|(item, weight)| item == "port" && weight == "1"),
        "{updates:?}"
    );
}

/// A key named `value` is not the ranked value: the heap sketches are
/// fed raw rows with unit weight.
#[tokio::test]
async fn topk_heap_over_a_key_named_value_counts_raw_rows() {
    let updates = heap_updates(
        "value",
        DataType::Utf8,
        &COUNT_RANKED_TOPK.replace("KEY", "value"),
    )
    .await;
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates
            .iter()
            .all(|(item, weight)| item == "value" && weight == "1"),
        "{updates:?}"
    );
}

/// SQL's per-group top-k idiom (`ROW_NUMBER() OVER (PARTITION BY city
/// ORDER BY COUNT(*) DESC) … WHERE rn <= 5`) is a heavy-hitter top-k, as
/// PromQL `topk by (city)` is: both heap sketches are offered, keyed by the
/// host and fed raw rows with unit weight.
#[tokio::test]
async fn rownumber_per_group_topk_offers_heap_sketches() {
    let updates = heap_updates_over(
        vec![
            Field::plain("city", DataType::Utf8, false),
            Field::plain("host", DataType::Utf8, false),
        ],
        "SELECT city, host, c FROM (SELECT city, host, COUNT(*) AS c, ROW_NUMBER() OVER \
         (PARTITION BY city ORDER BY COUNT(*) DESC) AS rn FROM flows GROUP BY city, host) \
         WHERE rn <= 5",
    )
    .await;
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates
            .iter()
            .all(|(item, weight)| item == "host" && weight == "1"),
        "{updates:?}"
    );
}
