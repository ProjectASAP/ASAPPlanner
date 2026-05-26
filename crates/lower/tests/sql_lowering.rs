//! End-to-end SQL → L2 → canonical L3 lowering tests (positional IR).
//!
//! Validates the re-targeted DataFusion front end: SQL parses + plans, lowers to
//! the relational L2 algebra, and the shared `convert_root` produces positional
//! canonical L3 (the same converter the PromQL path uses).

use asap_control_core::intent_algebra::schema::{Column, DataType, Schema};
use asap_control_core::intent_algebra::{AggIntent, QueryExpr, Source};
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::{lower_sql, SqlCatalog};

fn col(name: &str, dtype: DataType) -> Column {
    Column {
        name: name.into(),
        dtype,
        nullable: false,
    }
}

/// `metrics(ts, service, latency, bytes)` — column positions 0..3.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
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
}

async fn lower(sql: &str) -> QueryExpr {
    lower_sql(sql, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"))
}

/// Find the first `Aggregate` node anywhere in the tree.
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
async fn unsupported_join_is_rejected_not_mislowered() {
    // The front end declines JOIN rather than silently dropping a side.
    let res = lower_sql(
        "SELECT a.service FROM metrics a JOIN metrics b ON a.service = b.service",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await;
    assert!(res.is_err(), "JOIN should be rejected in v1");
}
