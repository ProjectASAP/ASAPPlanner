//! Cross-language L3 equivalence (issue #34).
//!
//! Semantically equivalent SQL and PromQL queries must lower to the **same
//! canonical intent algebra**, so an L4 rule matching on `AggIntent` sees one
//! spelling regardless of source language. These tests are the executable spec
//! for the shared [`canonicalize`](asap_l2::canonicalize) pass: they pin the
//! canonical heavy-hitter shape and assert both front ends reach it.
//!
//! A literal `lower_sql(S) == lower_promql(P)` cannot hold — the two count
//! *different* things (SQL rows vs. time-series samples over a window) and read
//! from different sources, so their leaves differ by design (#25). What must
//! match is the **shape above the leaf**: an outer `Aggregate([TopK{k}])` over an
//! explicit inner `Aggregate([Count])`.

use asap_devtools::{lower_promql, lower_sql, SqlCatalog};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::{AggIntent, GroupKeys, QueryExpr};
use asap_types::types::AccuracyTarget;

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "metrics",
        Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("region", DataType::Utf8),
                col("latency", DataType::Float64),
                col("bytes", DataType::Int64),
            ],
            0,
            vec![],
        ),
    )
}

async fn sql(q: &str) -> QueryExpr {
    lower_sql(q, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap_or_else(|e| panic!("SQL {q:?} failed to lower: {e:?}"))
}

fn promql(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact)
        .unwrap_or_else(|e| panic!("PromQL {q:?} failed to lower: {e:?}"))
}

/// The canonical heavy-hitter shape: an outer `Aggregate([TopK{k}])` (grouped by
/// `by`) over an inner `Aggregate([Count])`. Returns `(k, outer_by)`.
fn heavy_hitter(qe: &QueryExpr) -> Option<(usize, GroupKeys)> {
    let QueryExpr::Aggregate {
        reduction,
        aggs,
        child,
        ..
    } = qe
    else {
        return None;
    };
    let [AggIntent::TopK { k, .. }] = aggs.as_slice() else {
        return None;
    };
    // The child must be the explicit inner Count (not a raw Scan) — this is the
    // structural unification #25 asked for.
    let QueryExpr::Aggregate { aggs: inner, .. } = child.as_ref() else {
        return None;
    };
    matches!(inner.as_slice(), [AggIntent::Count { .. }])
        .then(|| (*k, reduction.expect_reduce().clone()))
}

#[tokio::test]
async fn sql_and_promql_heavy_hitter_share_the_canonical_shape() {
    // S2 (SQL, global count-topk) and P1 (PromQL, global count-topk) both express
    // "top-5 by count". They must reach the same canonical shape: outer global
    // TopK{5} (by: []) over an explicit inner Count.
    let s2 = sql(
        "SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) DESC LIMIT 5",
    )
    .await;
    let p1 = promql("topk(5, count_over_time(http_requests_total[5m]))");

    let (sk, sby) = heavy_hitter(&s2).expect("SQL S2 is a canonical heavy-hitter");
    let (pk, pby) = heavy_hitter(&p1).expect("PromQL P1 is a canonical heavy-hitter");
    assert_eq!(sk, 5);
    assert_eq!(pk, 5);
    assert!(sby.is_empty(), "S2 is a global topk");
    assert!(pby.is_empty(), "P1 is a global topk");
}

#[tokio::test]
async fn sql_aliased_and_inline_count_topk_are_identical() {
    // #20: aliasing the COUNT in the ORDER BY must not change the L3.
    let inline = sql(
        "SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) DESC LIMIT 5",
    )
    .await;
    let aliased =
        sql("SELECT service, COUNT(*) AS c FROM metrics GROUP BY service ORDER BY c DESC LIMIT 5")
            .await;
    assert_eq!(inline, aliased);
}

#[tokio::test]
async fn non_count_ranked_topk_stays_generic_in_both_languages() {
    // Ranking by a non-count (SUM / a raw value) is NOT a frequency heavy-hitter
    // in either language — it must stay a generic Sort+Limit, never a TopK.
    let s = sql(
        "SELECT service, SUM(bytes) FROM metrics GROUP BY service ORDER BY SUM(bytes) DESC LIMIT 5",
    )
    .await;
    let p = promql("topk(5, http_requests_total)");
    assert!(
        heavy_hitter(&s).is_none(),
        "SUM-ranked is not a heavy-hitter: {s:?}"
    );
    assert!(
        !matches!(&s, QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::TopK { .. }])),
    );
    assert!(
        heavy_hitter(&p).is_none(),
        "value-ranked topk is not a count heavy-hitter: {p:?}"
    );
}

#[tokio::test]
async fn ascending_count_ranked_topk_stays_generic_in_both_languages() {
    // The symmetric bottom-k case (issue #38): ranking by a count but taking the
    // *bottom* k is NOT a frequency heavy-hitter — the shared decision rule
    // (`is_frequency_heavy_hitter`) requires descending. Both front ends must
    // make the same call: SQL `ORDER BY COUNT(*) ASC LIMIT k` and PromQL
    // `bottomk(k, count_over_time(…))` both stay a generic Sort+Limit, never a
    // TopK. This pins the two count-ranked detectors to agree on direction.
    let s =
        sql("SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) ASC LIMIT 5")
            .await;
    let p = promql("bottomk(5, count_over_time(http_requests_total[5m]))");

    assert!(
        heavy_hitter(&s).is_none(),
        "SQL ASC count-limit is not a heavy-hitter: {s:?}"
    );
    assert!(
        heavy_hitter(&p).is_none(),
        "PromQL bottomk-count is not a heavy-hitter: {p:?}"
    );
    // Both are the generic order-by-value + limit shape.
    assert!(
        matches!(&s, QueryExpr::Limit { .. }),
        "SQL stays a Limit: {s:?}"
    );
    assert!(
        matches!(&p, QueryExpr::Limit { .. }),
        "PromQL stays a Limit: {p:?}"
    );
}

/// Descend through a leading `Project` (the derived-table SELECT list).
fn strip_project(qe: &QueryExpr) -> &QueryExpr {
    match qe {
        QueryExpr::Project { child, .. } => strip_project(child),
        other => other,
    }
}

#[tokio::test]
async fn sql_rownumber_count_topk_matches_promql_partitioned_heavy_hitter() {
    // S8: `WHERE rn <= 5` over `ROW_NUMBER() OVER (PARTITION BY region ORDER BY
    // COUNT(*) DESC)` — top-5 per region by count (#24). It must reach the same
    // partitioned heavy-hitter shape as PromQL `topk by (…) (5, count_over_time)`
    // (P10): an outer TopK grouped by the partition over an explicit Count.
    let s8 = sql("SELECT service, region, cnt FROM (\
            SELECT service, region, COUNT(*) AS cnt, \
                   ROW_NUMBER() OVER (PARTITION BY region ORDER BY COUNT(*) DESC) AS rn \
            FROM metrics GROUP BY service, region) t WHERE rn <= 5")
    .await;
    let (k, by) = heavy_hitter(strip_project(&s8)).expect("S8 is a partitioned heavy-hitter");
    assert_eq!(k, 5);
    assert!(!by.is_empty(), "partitioned by region, not a global topk");

    let p10 = promql("topk by (service) (5, count_over_time(http_requests_total[5m]))");
    let (pk, pby) = heavy_hitter(&p10).expect("P10 is a partitioned heavy-hitter");
    assert_eq!(pk, 5);
    assert!(!pby.is_empty(), "PromQL topk-by is also partitioned");
}

#[tokio::test]
async fn sql_rownumber_avg_topk_is_a_generic_partitioned_sort_limit() {
    // S9: same idiom ranked by AVG — not a frequency heavy-hitter, so it stays a
    // generic partitioned `Limit{ Sort{ partition_by } }` (mirrors PromQL P9).
    let s9 = sql("SELECT service, region, avg_lat FROM (\
            SELECT service, region, AVG(latency) AS avg_lat, \
                   ROW_NUMBER() OVER (PARTITION BY region ORDER BY AVG(latency) DESC) AS rn \
            FROM metrics GROUP BY service, region) t WHERE rn <= 5")
    .await;
    assert!(
        heavy_hitter(strip_project(&s9)).is_none(),
        "AVG-ranked is not a heavy-hitter"
    );
    let QueryExpr::Limit { child, .. } = strip_project(&s9) else {
        panic!("expected a Limit, got {:?}", strip_project(&s9));
    };
    let QueryExpr::Sort { partition_by, .. } = child.as_ref() else {
        panic!("expected a Sort under the Limit");
    };
    assert!(!partition_by.is_empty(), "partitioned by region");
}

#[tokio::test]
async fn offset_defeats_heavy_hitter_promotion() {
    // `LIMIT k OFFSET n` is not "the top k" — it must stay a Sort+Limit.
    let s = sql(
        "SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) DESC LIMIT 5 OFFSET 3",
    )
    .await;
    assert!(
        heavy_hitter(&s).is_none(),
        "OFFSET must not promote to TopK: {s:?}"
    );
}
