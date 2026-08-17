//! End-to-end SQL → L2 → canonical L3 lowering tests (positional IR).
//!
//! Validates the DataFusion front end: SQL parses + plans, lowers directly to
//! the canonical L2 shape (`QueryExpr<ColumnRef>`, issue #179), and the shared
//! `resolve_root` produces positional canonical L3 (the same resolver the
//! PromQL path uses).

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::{
    AggIntent, CompareOp, GroupKeys, JoinKind, L3Expr, L3Scalar, QueryExpr, Reduction, Source,
    WindowFuncKind,
};
use asap_types::types::AccuracyTarget;

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
fn find_aggregate(qe: &QueryExpr) -> Option<(&GroupKeys, &Vec<AggIntent>)> {
    match qe {
        QueryExpr::Aggregate {
            reduction,
            measures,
            ..
        } => Some((reduction.expect_reduce(), measures)),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => find_aggregate(child),
        _ => None,
    }
}

/// The first `Aggregate` node itself, for tests that need its child.
fn find_aggregate_node(qe: &QueryExpr) -> Option<&QueryExpr> {
    match qe {
        QueryExpr::Aggregate { .. } => Some(qe),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. } => find_aggregate_node(child),
        _ => None,
    }
}

/// The names of the columns the first `Aggregate`'s reducers read, resolved
/// against its child's schema, plus whether that child is a materializing
/// `Project` (issue #110).
fn reducer_input_names(qe: &QueryExpr) -> (Vec<String>, bool) {
    let QueryExpr::Aggregate {
        measures, child, ..
    } = find_aggregate_node(qe).expect("expected an Aggregate")
    else {
        unreachable!()
    };
    let schema = child.output_schema().expect("child schema");
    let names = measures
        .iter()
        .filter_map(|a| a.input_col())
        .map(|id| schema.columns[id].name.clone())
        .collect();
    (names, matches!(**child, QueryExpr::Project { .. }))
}

/// Find the first `Join` node along the single-child spine.
fn find_join(qe: &QueryExpr) -> Option<&QueryExpr> {
    match qe {
        QueryExpr::Join { .. } => Some(qe),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => find_join(child),
        _ => None,
    }
}

/// The first `Filter` node along the single-child spine.
fn find_filter(qe: &QueryExpr) -> Option<&QueryExpr> {
    match qe {
        QueryExpr::Filter { .. } => Some(qe),
        QueryExpr::Project { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => find_filter(child),
        _ => None,
    }
}

#[tokio::test]
async fn select_star_with_where_folds_predicate_onto_scan() {
    // SELECT * elides the projection; WHERE folds onto the Scan predicates.
    let qe = lower("SELECT * FROM metrics WHERE service = 'api'").await;
    let QueryExpr::Scan {
        source,
        predicates,
        schema,
    } = &qe
    else {
        panic!("expected Scan at root, got {qe:?}");
    };
    assert!(matches!(source, Source::Table { table_ref } if table_ref == "metrics"));
    assert_eq!(predicates.len(), 1, "WHERE clause folded onto the scan");
    assert!(
        schema.closed,
        "a catalog-backed SQL scan has a closed schema"
    );
}

#[tokio::test]
async fn multi_aggregate_group_by_binds_columns_positionally() {
    // SUM(bytes)=col 3, AVG(latency)=col 2, GROUP BY service=col 1.
    let qe = lower("SELECT service, SUM(bytes), AVG(latency) FROM metrics GROUP BY service").await;
    let (by, measures) = find_aggregate(&qe).expect("expected an Aggregate in the tree");
    assert_eq!(by, &vec![1], "GROUP BY service → column 1");
    assert!(
        measures.contains(&AggIntent::Sum { col: Some(3) }),
        "SUM(bytes) → Sum{{col:3}}, got {measures:?}"
    );
    assert!(
        measures.contains(&AggIntent::Avg { col: Some(2) }),
        "AVG(latency) → Avg{{col:2}}, got {measures:?}"
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
    let (by, measures) = find_aggregate(&qe).expect("expected an Aggregate (not a Partition)");
    assert_eq!(by, &vec![1], "GROUP BY service → Aggregate.by column 1");
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::Sum { col: Some(3) }]
    ));

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
    // case the heavy-hitter (frequency) sketch is correct for. The shared L3
    // `canonicalize` pass (issue #34) promotes it to the canonical two-level
    // form: an outer global `TopK` (by: []) over the explicit inner `Count`
    // grouped by `service`.
    let qe = lower(
        "SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) DESC LIMIT 10",
    )
    .await;
    let (by, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        by.is_empty(),
        "outer TopK is a global ranking (by: []), got {by:?}"
    );
    assert!(
        matches!(measures.as_slice(), [AggIntent::TopK { k: 10, .. }]),
        "count-ranked topk → heavy-hitter TopK, got {measures:?}"
    );
    // The inner child is the explicit Count, grouped by service (col 1).
    let QueryExpr::Aggregate { child, .. } = &qe else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    let (inner_by, inner_measures) = find_aggregate(child).expect("expected inner Count aggregate");
    assert_eq!(inner_by, &vec![1], "inner Count grouped by service → col 1");
    assert!(
        matches!(inner_measures.as_slice(), [AggIntent::Count { .. }]),
        "inner aggregate is the explicit Count, got {inner_measures:?}"
    );
}

#[tokio::test]
async fn count_ranked_topk_via_alias_is_also_heavy_hitter() {
    // Regression for #20: aliasing `COUNT(*)` in the ORDER BY used to defeat the
    // SQL front-end gate. The positional `canonicalize` pass now promotes it too,
    // so the aliased and inline forms produce identical L3.
    let inline = lower(
        "SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) DESC LIMIT 10",
    )
    .await;
    let aliased = lower(
        "SELECT service, COUNT(*) AS cnt FROM metrics GROUP BY service ORDER BY cnt DESC LIMIT 10",
    )
    .await;
    assert_eq!(
        inline, aliased,
        "aliased count-ranked topk must match the inline form"
    );
    let (_, measures) = find_aggregate(&aliased).expect("expected an Aggregate");
    assert!(
        matches!(measures.as_slice(), [AggIntent::TopK { k: 10, .. }]),
        "aliased count-ranked topk → heavy-hitter TopK, got {measures:?}"
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
    let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        measures.iter().any(|a| matches!(a, AggIntent::Avg { .. })),
        "AVG must be preserved, got {measures:?}"
    );
    assert!(
        !measures.iter().any(|a| matches!(a, AggIntent::TopK { .. })),
        "AVG ranking must not become a frequency heavy-hitter, got {measures:?}"
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
async fn aggregate_over_an_expression_reduces_a_derived_column() {
    // L3 reduces a column, not an arbitrary expression. `SUM(bytes + 1)` used to
    // be rejected for that reason; since #110 the expression is materialized as
    // a derived column in a `Project` beneath the aggregate, and reduced there.
    let qe = lower("SELECT SUM(bytes + 1) FROM metrics").await;
    let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        matches!(measures.as_slice(), [AggIntent::Sum { col: Some(_) }]),
        "expected Sum bound to the derived column, got {measures:?}"
    );
    let (names, materialized) = reducer_input_names(&qe);
    assert!(materialized, "expected a materializing Project");
    assert!(
        names[0].contains("bytes") && names[0].contains('1'),
        "the reduced column should be the projected `bytes + 1`, got {names:?}"
    );
}

#[tokio::test]
async fn count_star_is_count_intent() {
    let qe = lower("SELECT COUNT(*) FROM metrics").await;
    let (by, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(by.is_empty());
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
}

#[tokio::test]
async fn count_distinct_is_cardinality() {
    let qe = lower("SELECT COUNT(DISTINCT service) FROM metrics").await;
    let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::Cardinality { .. }]
    ));
}

#[tokio::test]
async fn select_distinct_lowers_to_distinct_with_positional_cols() {
    // SELECT DISTINCT → a `Distinct` node whose `cols` are positional ColumnIds
    // (not name-based ColumnRefs). DataFusion's `Distinct::All` dedups on every
    // column, so `cols` is empty here — but the field type is now `Vec<ColumnId>`.
    let qe = lower("SELECT DISTINCT service FROM metrics").await;
    let QueryExpr::Distinct { cols, .. } = &qe else {
        panic!("expected a Distinct at the root, got {qe:?}");
    };
    let _: &Vec<usize> = cols; // compile-time: positional ids, not ColumnRefs
    assert!(cols.is_empty(), "DISTINCT * dedups on all columns");
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
async fn derived_table_join_disambiguates_via_alias() {
    // Issue #66: a join over two *derived tables* must bind its keys to distinct
    // positions. Before the fix the derived output columns lost their qualifier,
    // so `a.service` and `b.service` both fell back to the first bare `service`
    // (col 0) — `service = service`, always true → a silent cross product.
    // Concatenated: a[service,region] ++ b[service,region] → a.service=0, b.service=2.
    let qe = lower(
        "SELECT a.region, b.region \
         FROM (SELECT service, region FROM hosts) a \
         JOIN (SELECT service, region FROM hosts) b ON a.service = b.service",
    )
    .await;
    let join = find_join(&qe).expect("expected a Join in the tree");
    assert_eq!(
        join_eq_columns(join),
        [0, 2],
        "derived-table join keys must bind to distinct positions, not both to the first `service`"
    );
}

#[tokio::test]
async fn derived_table_select_star_join_disambiguates_via_alias() {
    // Same as above but `SELECT *` derived tables (the non-Projection path that
    // wraps the inner plan in an identity re-qualifying projection).
    let qe = lower(
        "SELECT a.region, b.region \
         FROM (SELECT * FROM hosts) a JOIN (SELECT * FROM hosts) b \
         ON a.service = b.service",
    )
    .await;
    let join = find_join(&qe).expect("expected a Join in the tree");
    let [l, r] = join_eq_columns(join);
    assert_ne!(
        l, r,
        "SELECT * derived-table join keys must not collapse to one column"
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
async fn qualified_where_over_join_resolves_to_right_side() {
    // Issue #7 beyond the join key: a WHERE on the *duplicated* column name
    // (`service` exists on both sides) must bind to the qualified side, not the
    // first match. metrics.service = col 1, hosts.service = col 4 → `hosts.service`
    // must resolve to 4. (Unoptimized plan keeps the Filter above the Join — no
    // predicate pushdown — so it binds against the concatenated schema.)
    let qe = lower(
        "SELECT metrics.bytes FROM metrics JOIN hosts ON metrics.service = hosts.service \
         WHERE hosts.service = 'api'",
    )
    .await;
    let filter = find_filter(&qe).expect("expected a Filter over the join");
    let QueryExpr::Filter { pred, .. } = filter else {
        unreachable!("find_filter only returns Filter");
    };
    assert!(
        matches!(&pred.0, L3Expr::Compare { left, op: CompareOp::Eq, .. }
            if matches!(left.as_ref(), L3Expr::Column(4))),
        "hosts.service must bind to concatenated position 4 (not the first `service`), got {:?}",
        pred.0
    );
}

#[tokio::test]
async fn self_join_group_by_disambiguates_via_qualifier() {
    // L2 group-key qualifier fix: GROUP BY on the *duplicated* column over a
    // self-join must bind to the qualified side, not first-match. metrics ⋈
    // metrics → a.service = col 1, b.service = col 5. (Without qualified keys,
    // both `GROUP BY a.service` and `GROUP BY b.service` collapsed to col 1.)
    let qe_b = lower(
        "SELECT b.service, COUNT(*) FROM metrics a JOIN metrics b \
         ON a.service = b.service GROUP BY b.service",
    )
    .await;
    let (by, _) = find_aggregate(&qe_b).expect("expected an Aggregate over the self-join");
    assert_eq!(
        by,
        &vec![5],
        "GROUP BY b.service binds to the b side (col 5)"
    );

    let qe_a = lower(
        "SELECT a.service, COUNT(*) FROM metrics a JOIN metrics b \
         ON a.service = b.service GROUP BY a.service",
    )
    .await;
    let (by, _) = find_aggregate(&qe_a).expect("expected an Aggregate over the self-join");
    assert_eq!(
        by,
        &vec![1],
        "GROUP BY a.service binds to the a side (col 1)"
    );
}

#[tokio::test]
async fn aggregate_over_join_binds_against_concatenated_schema() {
    // GROUP BY a right-table column over a join: the key must resolve against
    // the concatenated schema, exercising the bottom-up converter end to end.
    // Two aggregates → the multi-agg path, which carries GROUP BY keys as
    // positional `Aggregate.by` (as does every reducing GROUP BY).
    let qe = lower(
        "SELECT hosts.region, SUM(metrics.bytes), COUNT(*) \
         FROM metrics JOIN hosts ON metrics.service = hosts.service \
         GROUP BY hosts.region",
    )
    .await;
    let (by, measures) = find_aggregate(&qe).expect("expected an Aggregate over the join");
    // metrics(ts,service,latency,bytes) ++ hosts(service,region) →
    // region is column 5, bytes is column 3 of the concatenated schema.
    assert_eq!(
        by,
        &vec![5],
        "GROUP BY hosts.region → concatenated column 5"
    );
    assert!(
        measures.contains(&AggIntent::Sum { col: Some(3) }),
        "SUM(metrics.bytes) → Sum{{col:3}}, got {measures:?}"
    );
}

// ── Issue #111: IN / EXISTS subquery predicates become semi / anti joins ────

/// The first `Join` node's `(kind, predicate, left column count)`.
fn join_parts(qe: &QueryExpr) -> (&JoinKind, &L3Expr, usize) {
    let QueryExpr::Join {
        kind,
        pred,
        left,
        right: _,
    } = find_join(qe).expect("expected a Join")
    else {
        unreachable!()
    };
    let left_len = left.output_schema().expect("left schema").columns.len();
    (kind, &pred.0, left_len)
}

#[tokio::test]
async fn in_subquery_lowers_to_a_semi_join() {
    // `metrics(ts, service, latency, bytes)` — service is column 1.
    let qe =
        lower("SELECT service FROM metrics WHERE service IN (SELECT service FROM hosts)").await;
    let (kind, pred, left_len) = join_parts(&qe);
    assert_eq!(kind, &JoinKind::Semi);
    assert_eq!(left_len, 4);

    // The predicate resolves against `left ++ right`. Both relations have a
    // `service` column, so a name-based lookup would bind *both* sides to the
    // left's — silently making this `service = service`, always true. The key is
    // projected under a synthetic name to make that impossible.
    let L3Expr::Compare { left, right, .. } = pred else {
        panic!("expected a comparison, got {pred:?}");
    };
    assert_eq!(**left, L3Expr::Column(1), "outer service");
    assert_eq!(
        **right,
        L3Expr::Column(left_len),
        "the subquery key, not the outer column again"
    );
}

#[tokio::test]
async fn a_semi_join_outputs_only_the_left_schema() {
    // The right side is a filter, not a source of columns.
    let qe =
        lower("SELECT service FROM metrics WHERE service IN (SELECT service FROM hosts)").await;
    let join = find_join(&qe).expect("expected a Join");
    let names: Vec<_> = join
        .output_schema()
        .expect("join schema")
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(names, ["ts", "service", "latency", "bytes"]);
}

#[tokio::test]
async fn a_subquery_key_that_is_an_expression_still_binds() {
    // `SELECT bytes + 1 …` has no column name of its own; it is projected under
    // the synthetic key rather than becoming an unreferenceable `col_0`.
    let qe =
        lower("SELECT service FROM metrics WHERE bytes IN (SELECT bytes + 1 FROM metrics)").await;
    assert_eq!(join_parts(&qe).0, &JoinKind::Semi);
}

#[tokio::test]
async fn a_multi_column_in_subquery_is_rejected() {
    let err = lower_sql(
        "SELECT service FROM metrics WHERE service IN (SELECT service, region FROM hosts)",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await
    .expect_err("IN must select one column");
    assert!(format!("{err}").contains("exactly one column"), "got {err}");
}

#[tokio::test]
async fn an_ordinary_conjunct_still_folds_onto_the_scan() {
    // The residual filter stays *below* the semi-join, where the converter can
    // still fold it onto the Scan. A semi-join only drops left rows, so the
    // orders agree.
    let qe = lower(
        "SELECT service FROM metrics WHERE bytes > 10 \
         AND service IN (SELECT service FROM hosts)",
    )
    .await;
    fn scan_has_predicate(qe: &QueryExpr) -> bool {
        match qe {
            QueryExpr::Scan { predicates, .. } => !predicates.is_empty(),
            QueryExpr::Project { child, .. }
            | QueryExpr::Filter { child, .. }
            | QueryExpr::Aggregate { child, .. } => scan_has_predicate(child),
            QueryExpr::Join { left, right, .. } => {
                scan_has_predicate(left) || scan_has_predicate(right)
            }
            _ => false,
        }
    }
    assert_eq!(join_parts(&qe).0, &JoinKind::Semi);
    assert!(
        scan_has_predicate(&qe),
        "WHERE bytes > 10 should reach the Scan"
    );
}

/// Find the first `WindowFunc` node along the single-child spine.
fn find_windowfunc(qe: &QueryExpr) -> Option<&QueryExpr> {
    match qe {
        QueryExpr::WindowFunc { .. } => Some(qe),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Aggregate { child, .. }
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

// ── Nested query functions: derived tables / inline views (issue #27) ───────────

/// Collect every `AggIntent` in the tree, root-to-leaf.
fn all_intents(qe: &QueryExpr) -> Vec<AggIntent> {
    let mut out = Vec::new();
    fn go(qe: &QueryExpr, out: &mut Vec<AggIntent>) {
        match qe {
            QueryExpr::Aggregate {
                measures, child, ..
            } => {
                out.extend(measures.iter().cloned());
                go(child, out);
            }
            QueryExpr::Project { child, .. }
            | QueryExpr::Filter { child, .. }
            | QueryExpr::Distinct { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::WindowFunc { child, .. }
            | QueryExpr::Subquery { child, .. } => go(child, out),
            QueryExpr::BinaryOp { lhs, rhs, .. }
            | QueryExpr::Join {
                left: lhs,
                right: rhs,
                ..
            }
            | QueryExpr::SetOp {
                left: lhs,
                right: rhs,
                ..
            } => {
                go(lhs, out);
                go(rhs, out);
            }
            _ => {}
        }
    }
    go(qe, &mut out);
    out
}

#[tokio::test]
async fn derived_table_aggregate_over_aggregate_nests() {
    // `MAX(s)` over a derived table `(SELECT service, SUM(bytes) AS s … GROUP BY
    // service)` — the SQL counterpart of PromQL function nesting (issue #27).
    // Both reductions survive into L3: an outer `Max` over the inner `Sum`.
    let qe = lower(
        "SELECT MAX(s) FROM \
         (SELECT service, SUM(bytes) AS s FROM metrics GROUP BY service) t",
    )
    .await;
    let intents = all_intents(&qe);
    assert!(
        intents.iter().any(|i| matches!(i, AggIntent::Max { .. })),
        "outer MAX survives, got {intents:?}"
    );
    assert!(
        intents.iter().any(|i| matches!(i, AggIntent::Sum { .. })),
        "inner SUM survives, got {intents:?}"
    );
    // The whole nested tree's output schema derives without error (positional
    // resolution is total across the derived-table boundary).
    assert_eq!(qe.output_schema().unwrap().columns.len(), 1);
}

#[tokio::test]
async fn derived_table_outer_avg_over_inner_percentile() {
    // Outer exact `AVG` over an inner approximate `Quantile` — each layer keeps
    // its own intent (the per-node sketch-vs-exact choice is an L4 decision).
    let qe = lower(
        "SELECT AVG(p) FROM \
         (SELECT service, approx_percentile_cont(latency, 0.9) AS p \
          FROM metrics GROUP BY service) t",
    )
    .await;
    let intents = all_intents(&qe);
    assert!(intents.iter().any(|i| matches!(i, AggIntent::Avg { .. })));
    assert!(intents
        .iter()
        .any(|i| matches!(i, AggIntent::Quantile { q, .. } if (*q - 0.9).abs() < 1e-9)));
}

#[tokio::test]
async fn filter_over_derived_aggregate_resolves_alias_column() {
    // `WHERE t.s > 100` over a derived aggregate — the qualified ref `t.s`
    // resolves by bare name against the derived output schema, and the Filter
    // sits above the inner Aggregate.
    let qe = lower(
        "SELECT t.service, t.s FROM \
         (SELECT service, SUM(bytes) AS s FROM metrics GROUP BY service) t \
         WHERE t.s > 100",
    )
    .await;
    assert!(
        find_filter(&qe).is_some(),
        "the outer WHERE lowers to a Filter, got {qe:?}"
    );
    assert!(all_intents(&qe)
        .iter()
        .any(|i| matches!(i, AggIntent::Sum { .. })));
    // Schema derivation is total across the boundary.
    let _ = qe.output_schema().expect("nested schema derivation");
}

#[tokio::test]
async fn scalar_subquery_in_predicate_is_rejected() {
    // A subquery-*valued* expression (`x > (SELECT …)`) needs a subquery node in
    // the L2 expression IR (and a correlated/uncorrelated decision); rejected
    // cleanly until that lands. Derived tables in FROM (the common nesting
    // shape) ARE supported — see the tests above.
    let res = lower_sql(
        "SELECT service FROM metrics WHERE bytes > (SELECT AVG(bytes) FROM metrics)",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await;
    assert!(
        res.is_err(),
        "scalar subquery in predicate should be rejected"
    );
}

#[tokio::test]
async fn correlated_exists_lifts_its_correlation_into_the_join() {
    // `EXISTS (SELECT 1 FROM hosts h WHERE h.service = m.service)` → a semi-join
    // on `h.service = m.service`. The `SELECT 1` projection is dropped: a
    // semi-join keeps no right columns, and it would have projected away the
    // very column the correlation needs.
    let qe = lower(
        "SELECT service FROM metrics m WHERE EXISTS \
         (SELECT 1 FROM hosts h WHERE h.service = m.service)",
    )
    .await;
    let (kind, pred, left_len) = join_parts(&qe);
    assert_eq!(kind, &JoinKind::Semi);
    let L3Expr::Compare { left, right, .. } = pred else {
        panic!("expected the correlation as a comparison, got {pred:?}");
    };
    assert_eq!(**left, L3Expr::Column(left_len), "h.service (right side)");
    assert_eq!(**right, L3Expr::Column(1), "m.service (left side)");
}

#[tokio::test]
async fn not_exists_lowers_to_an_anti_join() {
    let qe = lower(
        "SELECT service FROM metrics m WHERE NOT EXISTS \
         (SELECT 1 FROM hosts h WHERE h.service = m.service)",
    )
    .await;
    assert_eq!(join_parts(&qe).0, &JoinKind::Anti);
}

#[tokio::test]
async fn an_uncorrelated_exists_is_an_unconditional_semi_join() {
    // No correlation → keep every left row iff the right side has any row.
    let qe = lower("SELECT service FROM metrics WHERE EXISTS (SELECT 1 FROM hosts)").await;
    let (kind, pred, _) = join_parts(&qe);
    assert_eq!(kind, &JoinKind::Semi);
    assert_eq!(*pred, L3Expr::Literal(L3Scalar::Boolean(true)));
}

#[tokio::test]
async fn not_in_subquery_is_rejected_rather_than_mislowered_as_an_anti_join() {
    // `NOT IN` is *not* an anti-join. Under three-valued logic a single NULL
    // among the subquery's rows makes `c NOT IN (…)` UNKNOWN for every `c`, so
    // the query returns nothing — while an anti-join returns every unmatched
    // left row. Rejecting is the only correct option until the nullability is
    // proven, and `NOT EXISTS` is the safe spelling.
    let err = lower_sql(
        "SELECT service FROM metrics WHERE service NOT IN (SELECT service FROM hosts)",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await
    .expect_err("NOT IN must not lower to an anti-join");
    assert!(format!("{err}").contains("NOT IN"), "got {err}");
}

#[tokio::test]
async fn a_correlated_in_subquery_is_rejected() {
    let err = lower_sql(
        "SELECT service FROM metrics m WHERE service IN \
         (SELECT h.service FROM hosts h WHERE h.region = m.service)",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await
    .expect_err("correlated IN needs both a key match and a correlation");
    assert!(format!("{err}").contains("correlated IN"), "got {err}");
}

// ── Issue #115: Quantile / Cardinality carry their input column ─────────────

#[tokio::test]
async fn quantile_carries_its_input_column() {
    // `metrics(ts=0, service=1, latency=2, bytes=3)`. Two quantiles over
    // different columns must not compare equal — a workload-level dedupe pass
    // would compare on `AggIntent` equality, so a col-less intent would
    // collapse them.
    let qe = lower(
        "SELECT approx_percentile_cont(latency, 0.5), \
                approx_percentile_cont(bytes, 0.5) FROM metrics",
    )
    .await;
    let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        matches!(
            measures.as_slice(),
            [
                AggIntent::Quantile { col: Some(2), .. },
                AggIntent::Quantile { col: Some(3), .. }
            ]
        ),
        "quantiles must bind their own column, got {measures:?}"
    );
    assert_ne!(
        measures[0], measures[1],
        "distinct-column quantiles must not compare equal"
    );
}

#[tokio::test]
async fn count_distinct_carries_its_input_column() {
    let qe = lower("SELECT COUNT(DISTINCT service), COUNT(DISTINCT bytes) FROM metrics").await;
    let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        matches!(
            measures.as_slice(),
            [
                AggIntent::Cardinality { col: Some(1), .. },
                AggIntent::Cardinality { col: Some(3), .. }
            ]
        ),
        "cardinalities must bind their own column, got {measures:?}"
    );
    assert_ne!(
        measures[0], measures[1],
        "distinct-column cardinalities must not compare equal"
    );
}

#[tokio::test]
async fn quantile_and_count_distinct_over_an_expression_bind_the_derived_column() {
    // A SQL aggregate has no "sample value" to fall back on, so an expression
    // argument must never reach L3 as `col: None` (#115). Since #110 it reaches
    // L3 as `col: Some(derived)` instead of being rejected.
    for q in [
        "SELECT approx_percentile_cont(bytes * 8, 0.95) FROM metrics",
        "SELECT COUNT(DISTINCT bytes * 8) FROM metrics",
        "SELECT approx_distinct(bytes * 8) FROM metrics",
    ] {
        let qe = lower(q).await;
        let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
        assert!(
            measures[0].input_col().is_some(),
            "{q} must bind a column, never `col: None`, got {measures:?}"
        );
        let (names, materialized) = reducer_input_names(&qe);
        assert!(materialized, "{q} expected a materializing Project");
        assert!(
            names[0].contains("bytes"),
            "{q} should reduce the projected `bytes * 8`, got {names:?}"
        );
    }
}

// ── Issue #111: median / approx_median → the φ=0.5 quantile ─────────────────

#[tokio::test]
async fn median_lowers_to_the_half_quantile() {
    // `metrics(ts=0, service=1, latency=2, bytes=3)`.
    for sql in [
        "SELECT median(latency) FROM metrics",
        "SELECT approx_median(latency) FROM metrics",
    ] {
        let qe = lower(sql).await;
        let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
        assert!(
            matches!(
                measures.as_slice(),
                [AggIntent::Quantile { col: Some(2), q, .. }] if (*q - 0.5).abs() < 1e-9
            ),
            "{sql} should lower to Quantile(0.5) over latency, got {measures:?}"
        );
    }
}

#[tokio::test]
async fn median_is_the_same_intent_as_an_explicit_half_percentile() {
    // Two spellings of one intent: CSE should be able to merge them.
    let m = lower("SELECT median(latency) FROM metrics").await;
    let p = lower("SELECT approx_percentile_cont(latency, 0.5) FROM metrics").await;
    let (_, m_measures) = find_aggregate(&m).expect("expected an Aggregate");
    let (_, p_measures) = find_aggregate(&p).expect("expected an Aggregate");
    assert_eq!(m_measures, p_measures);
}

#[tokio::test]
async fn median_threads_the_accuracy_target() {
    // The `approx_` prefix does not decide: the AccuracyTarget does.
    let qe = lower_sql(
        "SELECT approx_median(latency) FROM metrics",
        &catalog(),
        AccuracyTarget::Epsilon(0.01),
    )
    .await
    .expect("approx_median should lower");
    let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        matches!(
            measures.as_slice(),
            [AggIntent::Quantile { accuracy: AccuracyTarget::Epsilon(e), .. }]
                if (*e - 0.01).abs() < 1e-12
        ),
        "median must carry the workload's accuracy target, got {measures:?}"
    );
}

#[tokio::test]
async fn median_over_an_expression_binds_the_derived_column() {
    // Was rejected when filed (#111); supported since #110 materialized the
    // expression. What must still hold is the #115 rule: never `col: None`.
    let qe = lower("SELECT median(bytes * 8) FROM metrics").await;
    let (_, measures) = find_aggregate(&qe).expect("expected an Aggregate");
    assert!(
        matches!(measures.as_slice(), [AggIntent::Quantile { col: Some(_), q, .. }] if (*q - 0.5).abs() < 1e-9),
        "expected Quantile(0.5) bound to the derived column, got {measures:?}"
    );
}

// ── Issue #110: expression GROUP BY (time bucketing) ────────────────────────

#[tokio::test]
async fn time_bucketing_group_by_lowers_to_a_derived_key() {
    // The canonical time-series shape: `GROUP BY date_trunc(...)`. The bucket
    // expression is materialized beneath the aggregate and grouped on.
    let qe =
        lower("SELECT date_trunc('minute', ts) AS m, SUM(bytes) FROM metrics GROUP BY m").await;
    let node = find_aggregate_node(&qe).expect("expected an Aggregate");
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = node
    else {
        unreachable!()
    };
    assert!(
        matches!(**child, QueryExpr::Project { .. }),
        "expected a materializing Project beneath the Aggregate"
    );
    let schema = child.output_schema().expect("child schema");
    assert_eq!(reduction, &Reduction::by(vec![0]));
    assert!(
        schema.columns[0].name.contains("date_trunc"),
        "group key should be the projected bucket, got {:?}",
        schema.columns[0].name
    );
    // The reducer still binds its own column, not the bucket.
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::Sum { col: Some(1) }]
    ));
}

#[tokio::test]
async fn time_bucketing_keeps_the_scan_predicate() {
    // The projection is inserted above the scan, so a WHERE clause still folds
    // onto the Scan rather than being stranded.
    let qe = lower(
        "SELECT date_trunc('minute', ts) AS m, SUM(bytes) FROM metrics \
         WHERE bytes > 10 GROUP BY m",
    )
    .await;
    fn scan_has_predicate(qe: &QueryExpr) -> bool {
        match qe {
            QueryExpr::Scan { predicates, .. } => !predicates.is_empty(),
            QueryExpr::Project { child, .. }
            | QueryExpr::Filter { child, .. }
            | QueryExpr::Aggregate { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. } => scan_has_predicate(child),
            _ => false,
        }
    }
    assert!(scan_has_predicate(&qe), "WHERE should stay on the Scan");
}

#[tokio::test]
async fn a_plain_group_by_inserts_no_projection() {
    // Queries that lowered before #110 must keep their exact tree shape — the
    // projection appears only when something actually needs materializing.
    for q in [
        "SELECT service, SUM(bytes) FROM metrics GROUP BY service",
        "SELECT SUM(bytes) FROM metrics",
        "SELECT COUNT(*) FROM metrics",
    ] {
        let qe = lower(q).await;
        let QueryExpr::Aggregate { child, .. } =
            find_aggregate_node(&qe).expect("expected an Aggregate")
        else {
            unreachable!()
        };
        assert!(
            !matches!(**child, QueryExpr::Project { .. }),
            "{q} should not gain a projection"
        );
    }
}

#[tokio::test]
async fn a_shared_expression_is_materialized_once() {
    let qe = lower("SELECT SUM(bytes * 2), MIN(bytes * 2) FROM metrics").await;
    let QueryExpr::Aggregate {
        measures, child, ..
    } = find_aggregate_node(&qe).expect("expected an Aggregate")
    else {
        unreachable!()
    };
    assert_eq!(
        child.output_schema().expect("child schema").columns.len(),
        1,
        "the two reducers should share one derived column"
    );
    assert_eq!(measures[0].input_col(), measures[1].input_col());
}

// ── Issue #118: multi-level grouping expands into one Aggregate per level ───

/// The branches of the first `Merge` along the single-child spine.
fn merge_branches(qe: &QueryExpr) -> &Vec<QueryExpr> {
    fn find(qe: &QueryExpr) -> Option<&Vec<QueryExpr>> {
        match qe {
            QueryExpr::Merge { children } => Some(children),
            QueryExpr::Project { child, .. }
            | QueryExpr::Filter { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. } => find(child),
            _ => None,
        }
    }
    find(qe).expect("expected a Merge")
}

/// `(group keys, column names)` of each merged grouping level.
fn grouping_levels(qe: &QueryExpr) -> Vec<(GroupKeys, Vec<String>)> {
    merge_branches(qe)
        .iter()
        .map(|b| {
            let QueryExpr::Project { child, .. } = b else {
                panic!("expected a Project per level, got {b:?}");
            };
            let QueryExpr::Aggregate { reduction, .. } = child.as_ref() else {
                panic!("expected an Aggregate under the Project, got {child:?}");
            };
            let names = b
                .output_schema()
                .expect("level schema")
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect();
            (reduction.expect_reduce().clone(), names)
        })
        .collect()
}

#[tokio::test]
async fn rollup_expands_to_one_aggregate_per_prefix() {
    // ROLLUP(a, b) → (a,b), (a), () — three levels, widest first.
    let qe =
        lower("SELECT service, bytes, SUM(latency) FROM metrics GROUP BY ROLLUP(service, bytes)")
            .await;
    let levels = grouping_levels(&qe);
    let keys: Vec<_> = levels.iter().map(|(by, _)| by.clone()).collect();
    assert_eq!(
        keys,
        vec![
            GroupKeys::by(vec![1, 3]),
            GroupKeys::by(vec![1]),
            GroupKeys::none(),
        ]
    );
}

#[tokio::test]
async fn cube_expands_to_the_power_set() {
    // CUBE(a, b) → (a,b), (a), (b), () — four levels.
    let qe =
        lower("SELECT service, bytes, SUM(latency) FROM metrics GROUP BY CUBE(service, bytes)")
            .await;
    assert_eq!(grouping_levels(&qe).len(), 4);
}

#[tokio::test]
async fn a_mixed_grouping_set_is_normalized_by_datafusion() {
    // `GROUP BY g, ROLLUP(d)` arrives as one GroupingSets, not a plain key
    // alongside a grouping set — so there is only one shape to handle.
    let qe =
        lower("SELECT service, bytes, SUM(latency) FROM metrics GROUP BY service, ROLLUP(bytes)")
            .await;
    assert_eq!(grouping_levels(&qe).len(), 2);
}

#[tokio::test]
async fn omitted_grouping_keys_become_typed_nulls() {
    // Every level must emit every key — as NULL where the level omits it — or
    // `Merge` (which takes the first child's schema) would misdescribe the rest.
    // The null is *cast*: a bare Null literal infers as Float64.
    let qe = lower("SELECT service, SUM(bytes) FROM metrics GROUP BY ROLLUP(service)").await;
    let levels = grouping_levels(&qe);
    assert_eq!(levels.len(), 2);
    for (_, names) in &levels {
        assert_eq!(
            names,
            &["service".to_string(), "sum(metrics.bytes)".to_string()]
        );
    }

    // The `()` level projects `service` as a Utf8 null, not a Float64 one.
    let schema = merge_branches(&qe)[1]
        .output_schema()
        .expect("level schema");
    assert_eq!(schema.columns[0].name, "service");
    assert_eq!(
        schema.columns[0].dtype,
        DataType::Utf8,
        "the omitted key must keep its declared type"
    );
}

#[tokio::test]
async fn grouping_levels_are_union_compatible() {
    let qe = lower(
        "SELECT service, bytes, SUM(latency) FROM metrics GROUP BY GROUPING SETS ((service),(bytes),())",
    )
    .await;
    let shapes: Vec<_> = merge_branches(&qe)
        .iter()
        .map(|b| {
            b.output_schema()
                .expect("level schema")
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.dtype.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        shapes.windows(2).all(|w| w[0] == w[1]),
        "levels disagree: {shapes:?}"
    );
}

#[tokio::test]
async fn grouping_function_is_rejected() {
    // `__grouping_id` is dropped when the levels are expanded. It is observable
    // only through `GROUPING(col)`, so dropping it loses nothing representable —
    // this test is what makes that true.
    let err = lower_sql(
        "SELECT service, SUM(bytes), GROUPING(service) FROM metrics GROUP BY ROLLUP(service)",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await
    .expect_err("GROUPING() must be rejected while __grouping_id is dropped");
    assert!(format!("{err}").contains("grouping"), "got {err}");
}

#[tokio::test]
async fn a_non_column_key_inside_a_grouping_set_is_rejected() {
    // The #110 derived-column machinery covers plain `GROUP BY <expr>`; inside a
    // grouping set the key also has to be reinstatable as a typed null.
    let err = lower_sql(
        "SELECT date_trunc('minute', ts) AS m, SUM(bytes) FROM metrics GROUP BY ROLLUP(m)",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await
    .expect_err("expression key inside ROLLUP must be rejected");
    assert!(
        format!("{err}").contains("non-column key inside a multi-level grouping"),
        "got {err}"
    );
}

#[tokio::test]
async fn multi_level_grouping_composes_with_a_derived_reducer_argument() {
    // #110's materializing Project sits beneath every level's Aggregate.
    let qe = lower("SELECT service, SUM(bytes * 8) FROM metrics GROUP BY ROLLUP(service)").await;
    for b in merge_branches(&qe) {
        let QueryExpr::Project { child, .. } = b else {
            panic!("expected a Project per level");
        };
        let QueryExpr::Aggregate {
            measures, child, ..
        } = child.as_ref()
        else {
            panic!("expected an Aggregate");
        };
        assert!(matches!(
            measures.as_slice(),
            [AggIntent::Sum { col: Some(_) }]
        ));
        assert!(
            matches!(**child, QueryExpr::Project { .. }),
            "the derived-column projection should sit under each level"
        );
    }
}

#[tokio::test]
async fn an_ambiguous_passthrough_column_is_rejected_only_when_projecting() {
    // A `Project` carries one relation qualifier for all its columns, so `a.k`
    // and `b.k` cannot both survive it. That only matters once a projection is
    // inserted: without a derived column the join keys resolve as before.
    let ok = lower_sql(
        "SELECT m.service, h.service, SUM(m.bytes) FROM metrics m \
         JOIN hosts h ON m.service = h.service GROUP BY m.service, h.service",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await;
    assert!(
        ok.is_ok(),
        "no derived column ⇒ no projection ⇒ no ambiguity"
    );

    let err = lower_sql(
        "SELECT m.service, h.service, SUM(m.bytes * 2) FROM metrics m \
         JOIN hosts h ON m.service = h.service GROUP BY m.service, h.service",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await
    .expect_err("ambiguous passthrough must be rejected, not silently resolved");
    assert!(format!("{err}").contains("ambiguous column"), "got {err}");
}

// ── Issue #111: array_agg is deliberately not an intent (WONTFIX) ───────────

#[tokio::test]
async fn array_agg_is_deliberately_rejected() {
    // Not a coverage gap. `AggIntent` exists so the planner can bind a sketch or
    // a mergeable accumulator per node; `array_agg` pre-aggregates nothing (its
    // output is O(input rows)), has no bounded-memory approximate form, and its
    // partial state *is* the data. An `AggIntent::ArrayAgg` would force every
    // arm of `plan::boundary::realize` — an exhaustive match — to answer
    // `PassThrough`. Contrast `median`, which is `Quantile { q: 0.5 }` and does
    // feed the sketch path.
    //
    // This test exists so the rejection reads as a decision rather than a gap.
    let err = lower_sql(
        "SELECT array_agg(service) FROM metrics",
        &catalog(),
        AccuracyTarget::Exact,
    )
    .await
    .expect_err("array_agg must not lower to an intent");
    assert!(
        format!("{err}").contains("unsupported aggregate: array_agg"),
        "expected a clean UnsupportedAggregate, got {err}"
    );
}
