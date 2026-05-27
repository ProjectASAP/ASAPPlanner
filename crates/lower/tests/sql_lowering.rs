//! End-to-end SQL → L2 → canonical L3 lowering tests (positional IR).
//!
//! Validates the re-targeted DataFusion front end: SQL parses + plans, lowers to
//! the relational L2 algebra, and the shared `convert_root` produces positional
//! canonical L3 (the same converter the PromQL path uses).

use asap_control_core::intent_algebra::schema::{Column, DataType, Schema};
use asap_control_core::intent_algebra::{
    AggIntent, CompareOp, JoinKind, L3Expr, QueryExpr, Source, WindowFuncKind,
};
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::{lower_sql, SqlCatalog};

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

/// `metrics(ts, service, latency, bytes)` + `hosts(service, region)`.
fn catalog() -> SqlCatalog {
    SqlCatalog::new()
        .with_table(
            "metrics",
            Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp),
                    col("service", DataType::Utf8),
                    col("latency", DataType::Float64),
                    col("bytes", DataType::Int64),
                ],
                0,
                vec![],
            ),
        )
        .with_table(
            "hosts",
            Schema::new(vec![
                col("service", DataType::Utf8),
                col("region", DataType::Utf8),
            ]),
        )
}

async fn lower(sql: &str) -> QueryExpr {
    lower_sql(sql, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"))
}

/// Find the first `Aggregate` node along the single-child spine.
fn find_aggregate(qe: &QueryExpr) -> Option<(&Vec<usize>, &Vec<AggIntent>)> {
    match qe {
        QueryExpr::Aggregate { by, aggs, .. } => Some((by, aggs)),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Window { child, .. }
        | QueryExpr::Partition { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => find_aggregate(child),
        _ => None,
    }
}

/// Find the first `Join` node along the single-child spine.
fn find_join(qe: &QueryExpr) -> Option<&QueryExpr> {
    match qe {
        QueryExpr::Join { .. } => Some(qe),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Window { child, .. }
        | QueryExpr::Partition { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => find_join(child),
        _ => None,
    }
}

#[tokio::test]
async fn select_star_with_where_folds_predicate_onto_scan() {
    // SELECT * elides the projection; WHERE folds onto the Scan predicates.
    let qe = lower("SELECT * FROM metrics WHERE service = 'api'").await;
    let QueryExpr::Scan {
        source, predicates, ..
    } = &qe
    else {
        panic!("expected Scan at root, got {qe:?}");
    };
    assert!(matches!(source, Source::Table { table_ref } if table_ref == "metrics"));
    assert_eq!(predicates.len(), 1, "WHERE clause folded onto the scan");
}

#[tokio::test]
async fn multi_aggregate_group_by_binds_columns_positionally() {
    // SUM(bytes)=col 3, AVG(latency)=col 2, GROUP BY service=col 1.
    let qe = lower("SELECT service, SUM(bytes), AVG(latency) FROM metrics GROUP BY service").await;
    let (by, aggs) = find_aggregate(&qe).expect("expected an Aggregate in the tree");
    assert_eq!(by, &vec![1], "GROUP BY service → column 1");
    assert!(
        aggs.contains(&AggIntent::Sum { col: Some(3) }),
        "SUM(bytes) → Sum{{col:3}}, got {aggs:?}"
    );
    assert!(
        aggs.contains(&AggIntent::Avg { col: Some(2) }),
        "AVG(latency) → Avg{{col:2}}, got {aggs:?}"
    );
}

#[tokio::test]
async fn projection_over_aggregate_resolves_output_types_via_output_names() {
    // The enclosing Projection references the aggregates by DataFusion's
    // generated names (e.g. "sum(metrics.bytes)"); output_names threads those
    // onto the L3 Aggregate so the Project resolves real types — not the Utf8
    // fallback that an unresolved column would get.
    let qe = lower("SELECT SUM(bytes), AVG(latency) FROM metrics").await;
    let schema = qe
        .output_schema()
        .expect("root projection schema derivation");
    assert_eq!(schema.columns.len(), 2);
    assert_eq!(
        schema.columns[0].dtype,
        DataType::Int64,
        "SUM(bytes:Int64) resolves to Int64, not the Utf8 fallback"
    );
    assert_eq!(
        schema.columns[1].dtype,
        DataType::Float64,
        "AVG(latency) resolves to Float64"
    );
}

#[tokio::test]
async fn single_agg_group_by_keeps_key_in_output_schema() {
    // A tabular single-aggregate GROUP BY routes through the positional
    // Aggregate.by path (not the PromQL fused-Partition shape), so the group
    // key is a real output column the enclosing SELECT projection resolves.
    let qe = lower("SELECT service, SUM(bytes) FROM metrics GROUP BY service").await;
    let (by, aggs) = find_aggregate(&qe).expect("expected an Aggregate (not a Partition)");
    assert_eq!(by, &vec![1], "GROUP BY service → Aggregate.by column 1");
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { col: Some(3) }]));

    // Both the group key and the aggregate resolve in the root projection schema.
    let schema = qe.output_schema().expect("root projection schema");
    assert_eq!(schema.columns.len(), 2);
    assert_eq!(
        schema.columns[0].dtype,
        DataType::Utf8,
        "service is in the output"
    );
    assert_eq!(schema.columns[1].dtype, DataType::Int64, "SUM(bytes)");
}

#[tokio::test]
async fn count_ranked_topk_is_heavy_hitter() {
    // `ORDER BY COUNT(*) DESC LIMIT k` over a single COUNT aggregate is the one
    // case the heavy-hitter (frequency) sketch is correct for. (The key must
    // reference the count output directly; an alias would safely fall back to a
    // generic Sort+Limit.)
    let qe = lower(
        "SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) DESC LIMIT 10",
    )
    .await;
    let (by, aggs) = find_aggregate(&qe).expect("expected an Aggregate");
    assert_eq!(by, &vec![1], "GROUP BY service → col 1");
    assert!(
        matches!(aggs.as_slice(), [AggIntent::TopK { k: 10, .. }]),
        "count-ranked topk → heavy-hitter TopK, got {aggs:?}"
    );
}

#[tokio::test]
async fn non_count_ranked_limit_keeps_the_aggregate() {
    // Ranking by AVG (not a count) must NOT become a frequency heavy-hitter —
    // the AVG aggregate has to survive as a generic Sort+Limit.
    let qe = lower(
        "SELECT service, AVG(latency) AS a FROM metrics GROUP BY service ORDER BY a DESC LIMIT 10",
    )
    .await;
    let (_, aggs) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        aggs.iter().any(|a| matches!(a, AggIntent::Avg { .. })),
        "AVG must be preserved, got {aggs:?}"
    );
    assert!(
        !aggs.iter().any(|a| matches!(a, AggIntent::TopK { .. })),
        "AVG ranking must not become a frequency heavy-hitter, got {aggs:?}"
    );
}

#[tokio::test]
async fn distinct_value_reducer_is_rejected_not_dropped() {
    // L3 has no distinct-Sum; SUM(DISTINCT x) must be rejected, not silently
    // lowered as SUM(x).
    let res = lower_sql(
        "SELECT SUM(DISTINCT bytes) FROM metrics",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await;
    assert!(res.is_err(), "SUM(DISTINCT ...) should be rejected");
}

#[tokio::test]
async fn aggregate_over_non_column_expression_is_rejected() {
    // L3 reduces a column, not an arbitrary expression — SUM(bytes + 1) must be
    // rejected rather than silently reducing a probe column.
    let res = lower_sql(
        "SELECT SUM(bytes + 1) FROM metrics",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await;
    assert!(res.is_err(), "SUM(<expr>) should be rejected");
}

#[tokio::test]
async fn count_star_is_count_intent() {
    let qe = lower("SELECT COUNT(*) FROM metrics").await;
    let (by, aggs) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(by.is_empty());
    assert!(matches!(aggs.as_slice(), [AggIntent::Count { .. }]));
}

#[tokio::test]
async fn count_distinct_is_cardinality() {
    let qe = lower("SELECT COUNT(DISTINCT service) FROM metrics").await;
    let (_, aggs) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(matches!(aggs.as_slice(), [AggIntent::Cardinality { .. }]));
}

#[tokio::test]
async fn inner_join_lowers_to_join_over_two_scans() {
    // INNER JOIN over two distinct tables → L3 Join with both leaves as Scans.
    let qe = lower(
        "SELECT metrics.bytes, hosts.region \
         FROM metrics JOIN hosts ON metrics.service = hosts.service",
    )
    .await;
    let join = find_join(&qe).expect("expected a Join in the tree");
    let QueryExpr::Join {
        kind, left, right, ..
    } = join
    else {
        unreachable!("find_join only returns Join");
    };
    assert_eq!(*kind, JoinKind::Inner);
    assert!(matches!(left.as_ref(), QueryExpr::Scan { .. }));
    assert!(matches!(right.as_ref(), QueryExpr::Scan { .. }));
}

/// The two `ColumnId`s an equijoin predicate `Column(l) = Column(r)` binds to,
/// returned sorted so the assertion is independent of left/right ordering.
fn join_eq_columns(join: &QueryExpr) -> [usize; 2] {
    let QueryExpr::Join { pred, .. } = join else {
        unreachable!("expected a Join");
    };
    let L3Expr::Compare {
        left,
        op: CompareOp::Eq,
        right,
    } = &pred.0
    else {
        panic!("expected an equijoin Compare, got {:?}", pred.0);
    };
    match (left.as_ref(), right.as_ref()) {
        (L3Expr::Column(l), L3Expr::Column(r)) => {
            let mut cols = [*l, *r];
            cols.sort_unstable();
            cols
        }
        other => panic!("expected Column = Column, got {other:?}"),
    }
}

#[tokio::test]
async fn join_predicate_disambiguates_shared_column_name() {
    // Issue #7: `metrics.service = hosts.service` shares a column name across the
    // join. The qualified refs must bind to two *distinct* positions in the
    // concatenated schema, not collapse onto the first `service`.
    // metrics(ts,service,latency,bytes) ++ hosts(service,region)
    // → metrics.service = col 1, hosts.service = col 4.
    let qe = lower(
        "SELECT metrics.bytes, hosts.region \
         FROM metrics JOIN hosts ON metrics.service = hosts.service",
    )
    .await;
    let join = find_join(&qe).expect("expected a Join in the tree");
    assert_eq!(
        join_eq_columns(join),
        [1, 4],
        "join key must bind to distinct positions, not the same `service`"
    );
}

#[tokio::test]
async fn self_join_disambiguates_via_aliases() {
    // A self-join shares *every* column name; the alias qualifiers (`a`/`b`) are
    // the only way to tell the two `service` columns apart.
    // metrics ++ metrics → a.service = col 1, b.service = col 5 (4 cols/side).
    let qe = lower(
        "SELECT a.bytes, b.latency \
         FROM metrics a JOIN metrics b ON a.service = b.service",
    )
    .await;
    let join = find_join(&qe).expect("expected a self-Join in the tree");
    assert_eq!(
        join_eq_columns(join),
        [1, 5],
        "self-join keys must bind to distinct sides"
    );
}

#[tokio::test]
async fn aggregate_over_join_binds_against_concatenated_schema() {
    // GROUP BY a right-table column over a join: the key must resolve against
    // the concatenated schema, exercising the bottom-up converter end to end.
    // Two aggregates → the multi-agg path, which carries GROUP BY keys as
    // positional `Aggregate.by` (the single-agg path folds them into Partition).
    let qe = lower(
        "SELECT hosts.region, SUM(metrics.bytes), COUNT(*) \
         FROM metrics JOIN hosts ON metrics.service = hosts.service \
         GROUP BY hosts.region",
    )
    .await;
    let (by, aggs) = find_aggregate(&qe).expect("expected an Aggregate over the join");
    // metrics(ts,service,latency,bytes) ++ hosts(service,region) →
    // region is column 5, bytes is column 3 of the concatenated schema.
    assert_eq!(
        by,
        &vec![5],
        "GROUP BY hosts.region → concatenated column 5"
    );
    assert!(
        aggs.contains(&AggIntent::Sum { col: Some(3) }),
        "SUM(metrics.bytes) → Sum{{col:3}}, got {aggs:?}"
    );
}

#[tokio::test]
async fn semi_join_is_rejected_not_mislowered() {
    // No L3 counterpart for semi/anti joins yet → reject rather than mislower.
    let res = lower_sql(
        "SELECT service FROM metrics WHERE service IN (SELECT service FROM hosts)",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await;
    assert!(
        res.is_err(),
        "semi-join / IN-subquery should be rejected in v1"
    );
}

/// Find the first `WindowFunc` node along the single-child spine.
fn find_windowfunc(qe: &QueryExpr) -> Option<&QueryExpr> {
    match qe {
        QueryExpr::WindowFunc { .. } => Some(qe),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Window { child, .. }
        | QueryExpr::Partition { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => find_windowfunc(child),
        _ => None,
    }
}

#[tokio::test]
async fn window_function_lowers_to_positional_windowfunc() {
    // ROW_NUMBER() OVER (PARTITION BY service ORDER BY bytes DESC).
    let qe = lower(
        "SELECT service, ROW_NUMBER() OVER (PARTITION BY service ORDER BY bytes DESC) \
         FROM metrics",
    )
    .await;
    let win = find_windowfunc(&qe).expect("expected a WindowFunc node");
    let QueryExpr::WindowFunc {
        func,
        partition_by,
        order_by,
        ..
    } = win
    else {
        unreachable!("find_windowfunc only returns WindowFunc");
    };
    assert_eq!(*func, WindowFuncKind::RowNumber);
    assert_eq!(partition_by, &vec![1], "PARTITION BY service → col 1");
    assert_eq!(order_by.len(), 1);
    assert_eq!(
        order_by[0].expr,
        L3Expr::Column(3),
        "ORDER BY bytes → col 3"
    );
    assert!(!order_by[0].ascending, "DESC");

    // The window output column is appended to the schema (Int64 for ROW_NUMBER),
    // and the enclosing projection resolves it (output_name threading).
    let schema = qe.output_schema().expect("root schema");
    assert!(
        schema.columns.iter().any(|c| c.dtype == DataType::Int64),
        "row_number output column present, got {:?}",
        schema.columns
    );
}

#[tokio::test]
async fn window_aggregate_lowers_to_windowfunc() {
    let qe = lower("SELECT service, SUM(bytes) OVER (PARTITION BY service) FROM metrics").await;
    let win = find_windowfunc(&qe).expect("expected a WindowFunc node");
    let QueryExpr::WindowFunc { func, args, .. } = win else {
        unreachable!();
    };
    assert_eq!(*func, WindowFuncKind::Sum);
    assert_eq!(args, &vec![L3Expr::Column(3)], "SUM(bytes) → arg col 3");
}
