//! Cross-language pre-ASAP-IR equivalence (issue #34).
//!
//! Semantically equivalent SQL and PromQL queries must lower to the **same
//! canonical intent algebra**, so a post-ASAP binding rule matching on
//! `AggIntent` sees one spelling regardless of source language. These tests
//! are the executable spec
//! for the shared `canonicalize` (`asap-frontend-common`) pass: they pin the
//! canonical heavy-hitter shape and assert both front ends reach it.
//!
//! A literal `lower_sql(S) == lower_promql(P)` cannot hold — the two count
//! *different* things (SQL rows vs. time-series samples over a window) and read
//! from different sources, so their leaves differ by design (#25). What must
//! match is the **shape above the leaf**: an outer `Aggregate([TopK{k}])` over an
//! explicit inner `Aggregate([Count])`.

use asap_devtools::{lower_promql_with_sample_interval, lower_sql, SqlCatalog};
use asap_types::ir::operator::{AggIntent, GroupKeys};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, OperatorNode};
use std::rc::Rc;

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
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

async fn sql(q: &str) -> Rc<OperatorNode> {
    lower_sql(q, &catalog(), AccuracyTarget::exact())
        .await
        .unwrap_or_else(|e| panic!("SQL {q:?} failed to lower: {e:?}"))
}

fn promql(q: &str) -> Rc<OperatorNode> {
    lower_promql_with_sample_interval(q, AccuracyTarget::exact(), 1_000)
        .unwrap_or_else(|e| panic!("PromQL {q:?} failed to lower: {e:?}"))
}

/// The canonical heavy-hitter shape: an outer `Aggregate([TopK{k}])` (grouped by
/// `by`) over an inner `Aggregate([Count])`. Returns `(k, outer_by)`.
fn heavy_hitter(qe: &OperatorNode) -> Option<(usize, GroupKeys)> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    }) = qe.non_asap()
    else {
        return None;
    };
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return None;
    };
    // The child must be the explicit inner Count (not a raw Scan) — this is the
    // structural unification #25 asked for.
    let Some(NonASAPOp::Aggregate {
        measures: inner, ..
    }) = child.non_asap()
    else {
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
    // #20: aliasing the COUNT in the ORDER BY must not change the canonical shape.
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
async fn value_ranked_sum_and_raw_values_stay_generic() {
    // SUM and a bare PromQL value are both value rankings. Keep their explicit
    // Sort + Limit shape rather than assigning frequency-sketch semantics.
    let s = sql(
        "SELECT service, SUM(bytes) FROM metrics GROUP BY service ORDER BY SUM(bytes) DESC LIMIT 5",
    )
    .await;
    let p = promql("topk(5, http_requests_total)");
    assert!(
        heavy_hitter(&s).is_none(),
        "SUM ranking is value-ranked: {s:?}"
    );
    assert!(
        heavy_hitter(&p).is_none(),
        "a raw value has no additive heavy-hitter input: {p:?}"
    );
}

#[tokio::test]
async fn ascending_count_ranked_topk_stays_generic_in_both_languages() {
    // The symmetric bottom-k case (issue #38): ranking by a count but taking the
    // *bottom* k is NOT a frequency heavy-hitter — the shared decision rule
    // The Top-K operator's additive-ranking rule requires descending. Both front ends must
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
        matches!(s.non_asap(), Some(NonASAPOp::Limit { .. })),
        "SQL stays a Limit: {s:?}"
    );
    assert!(
        matches!(p.non_asap(), Some(NonASAPOp::Limit { .. })),
        "PromQL stays a Limit: {p:?}"
    );
}

/// The partitioned heavy-hitter of `qe`: its `k` and the names of the
/// columns it partitions by, if `qe` is an outer `Aggregate([TopK{k}])`.
fn partitioned_top_k(qe: &OperatorNode) -> Option<(usize, Vec<String>)> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    }) = qe.non_asap()
    else {
        return None;
    };
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return None;
    };
    let partition = reduction
        .expect_reduce()
        .iter()
        .map(|&column| child.schema.fields[column].name.clone())
        .collect();
    Some((*k, partition))
}

/// SQL's per-partition top-k idiom (`ROW_NUMBER() OVER (PARTITION BY
/// region ORDER BY COUNT(*) DESC) … WHERE rn <= 5`) is the heavy-hitter
/// PromQL `topk by (region)` lowers to: the top 5 per region by count, an
/// outer `TopK{5}` grouped by `region` over the explicit inner Count.
#[tokio::test]
async fn sql_rownumber_count_topk_is_promqls_topk_by() {
    let s = sql(
        "SELECT service, region, v FROM (SELECT service, region, COUNT(*) AS v, \
         ROW_NUMBER() OVER (PARTITION BY region ORDER BY COUNT(*) DESC) AS rn \
         FROM metrics GROUP BY service, region) t WHERE rn <= 5",
    )
    .await;
    let p = promql(
        "topk by (region) (5, sum by (service, region) (count_over_time(http_requests_total[5m])))",
    );
    s.validate_structure().unwrap();
    let region = vec!["region".to_string()];
    assert_eq!(partitioned_top_k(&s), Some((5, region.clone())), "{s:?}");
    assert_eq!(partitioned_top_k(&p), Some((5, region)), "{p:?}");
    assert!(heavy_hitter(&s).is_some(), "over the explicit inner Count");
    assert!(!OperatorNode::reachable(&s)
        .iter()
        .any(|node| matches!(node.non_asap(), Some(NonASAPOp::SQLWindowFunc { .. }))));
}

/// An avg ranking is no heavy-hitter: the generic per-partition
/// `Limit{5}{Sort}` that PromQL `topk by (region)` over an average lowers
/// to, with the query's three columns.
#[tokio::test]
async fn sql_rownumber_avg_topk_is_a_partitioned_sort_limit() {
    let query = sql(
        "SELECT service, region, v FROM (SELECT service, region, AVG(latency) AS v, \
         ROW_NUMBER() OVER (PARTITION BY region ORDER BY AVG(latency) DESC) AS rn \
         FROM metrics GROUP BY service, region) t WHERE rn <= 5",
    )
    .await;
    query.validate_structure().unwrap();
    assert_eq!(query.schema.fields.len(), 3);
    let Some(NonASAPOp::Limit {
        n: Some(5),
        partition_by,
        child,
        ..
    }) = query.non_asap()
    else {
        panic!("a per-partition Limit: {query:?}");
    };
    assert_eq!(child.schema.fields[partition_by[0]].name, "region");
    assert!(!OperatorNode::reachable(&query)
        .iter()
        .any(|node| matches!(node.non_asap(), Some(NonASAPOp::SQLWindowFunc { .. }))));
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

/// Heavy-hitter promotion changes a node's columns, so it applies only to
/// a ranking nothing reads by position. `topk(5, sum by (job) (m))` at the
/// root is promoted; under `count(...)`, which reads its columns, it stays
/// the order-by-value `Limit{Sort}`.
#[tokio::test]
async fn a_promql_ranking_read_by_position_stays_a_sort_limit() {
    let root = promql("topk(5, sum by (job) (http_requests_total))");
    assert!(
        matches!(root.non_asap(), Some(NonASAPOp::Aggregate { measures, .. })
            if matches!(measures.as_slice(), [AggIntent::TopK { .. }])),
        "{root:?}"
    );
    let nested = promql("count(topk(5, sum by (job) (http_requests_total)))");
    let Some(NonASAPOp::Aggregate { child, .. }) = nested.non_asap() else {
        panic!("count at the root: {nested:?}");
    };
    assert!(
        matches!(child.non_asap(), Some(NonASAPOp::Limit { .. })),
        "{child:?}"
    );
}
