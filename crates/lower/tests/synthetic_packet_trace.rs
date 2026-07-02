//! Real-world **SQL** conformance over the DQC synthetic-packet-trace
//! evaluation queries.
//!
//! Source: the `metrics.py` fidelity benchmark for synthetic network-trace
//! generation — every registered evaluation query (`eval_metrics`), grouped as
//! packet level (29), flow level stateless (21), flow level stateful (20) = 70
//! queries (`tests/data/synthetic_packet_trace_queries.sql`). Each is run on a
//! real trace and a synthetic trace and the two result sets are compared to
//! score fidelity; here we only *lower* them.
//!
//! This is the SQL counterpart of `awesome_prometheus_alerts.rs`: a real-world
//! query corpus that pins the front end against regressions. Two guarantees:
//!   1. **Totality** — every query returns `Ok` or a clean `LoweringError`,
//!      never panics.
//!   2. **Full coverage** — the DataFusion SQL front end lowers **all 70**
//!      (CTEs, `LAG` window functions, multi-argument `COUNT(DISTINCT …)`,
//!      `STDDEV_POP`, `HAVING`, `CASE`). A regression that drops any query below
//!      full coverage trips the ratchet.
//!
//! Schema: `packets(srcip, dstip, srcport, dstport, proto, time, pkt_len)`;
//! flow / 5-tuple = `(srcip, dstip, srcport, dstport, proto)`.

use asap_ir::intent_algebra::schema::{Column, DataType, Schema};
use asap_ir::intent_algebra::{AggIntent, GroupKeys, QueryExpr};
use asap_ir::types::AccuracyTarget;
use asap_control_lower::{lower_sql, LoweringError, SqlCatalog};

const CORPUS: &str = include_str!("data/synthetic_packet_trace_queries.sql");

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

/// `packets(srcip, dstip, srcport, dstport, proto, time, pkt_len)`. IPs and
/// proto are strings; ports and length are integers; `time` is a float epoch
/// (the stateful queries do `MAX(time) - MIN(time)` / `time - LAG(time)` and
/// divide bytes by it, so it must be numeric, not a `Timestamp`).
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "packets",
        Schema::new(vec![
            col("srcip", DataType::Utf8),
            col("dstip", DataType::Utf8),
            col("srcport", DataType::Int64),
            col("dstport", DataType::Int64),
            col("proto", DataType::Utf8),
            col("time", DataType::Float64),
            col("pkt_len", DataType::Int64),
        ]),
    )
}

/// The corpus statements. Comment (`#` / `--`) and blank lines are stripped
/// **before** splitting on `;` — the descriptions contain semicolons
/// (e.g. "> 1 packet; zero duration"), so splitting first would fragment them.
fn queries() -> Vec<String> {
    let sql: String = CORPUS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--") && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    sql.split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ── tree helpers ──────────────────────────────────────────────────────────────

/// Every `AggIntent` in the tree, root-to-leaf.
fn intents(e: &QueryExpr) -> Vec<AggIntent> {
    let mut out = Vec::new();
    fn go(e: &QueryExpr, out: &mut Vec<AggIntent>) {
        match e {
            QueryExpr::Aggregate { aggs, child, .. } => {
                out.extend(aggs.iter().cloned());
                go(child, out);
            }
            QueryExpr::Window { child, .. }
            | QueryExpr::TimeRange { child, .. }
            | QueryExpr::Filter { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::Subquery { child, .. }
            | QueryExpr::Distinct { child, .. }
            | QueryExpr::WindowFunc { child, .. }
            | QueryExpr::Project { child, .. } => go(child, out),
            QueryExpr::BinaryOp { lhs, rhs, .. }
            | QueryExpr::Join { left: lhs, right: rhs, .. }
            | QueryExpr::SetOp { left: lhs, right: rhs, .. } => {
                go(lhs, out);
                go(rhs, out);
            }
            QueryExpr::Merge { children } => children.iter().for_each(|c| go(c, out)),
            QueryExpr::LetBinding { expr, child, .. } => {
                go(expr, out);
                go(child, out);
            }
            QueryExpr::Scan { .. } | QueryExpr::Ref { .. } => {}
        }
    }
    go(e, &mut out);
    out
}

/// The first `Aggregate`'s `(by, aggs)` along the single-child spine.
fn first_aggregate(qe: &QueryExpr) -> Option<(&GroupKeys, &Vec<AggIntent>)> {
    match qe {
        QueryExpr::Aggregate { by, aggs, .. } => Some((by, aggs)),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::WindowFunc { child, .. }
        | QueryExpr::Subquery { child, .. } => first_aggregate(child),
        _ => None,
    }
}

/// Whether a `WindowFunc` (analytic `OVER (…)`) node appears anywhere.
fn has_window_func(qe: &QueryExpr) -> bool {
    match qe {
        QueryExpr::WindowFunc { .. } => true,
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => has_window_func(child),
        QueryExpr::LetBinding { expr, child, .. } => {
            has_window_func(expr) || has_window_func(child)
        }
        _ => false,
    }
}

async fn lower(q: &str) -> QueryExpr {
    lower_sql(q, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap_or_else(|e| panic!("expected {q:?} to lower, got error: {e}"))
}

// ── corpus-wide invariant ─────────────────────────────────────────────────────

#[derive(Default, Debug)]
struct Tally {
    lowered: usize,
    rejected: usize,
    unparseable: usize,
}

#[tokio::test]
async fn corpus_lowering_is_total_and_fully_supported() {
    let cat = catalog();
    let mut t = Tally::default();
    for q in queries() {
        // A panic here (not an `Err`) fails the test — the totality guarantee.
        match lower_sql(&q, &cat, AccuracyTarget::Exact).await {
            Ok(_) => t.lowered += 1,
            // DataFusion surfaces parse/plan failures as `DataFusion(_)`.
            Err(LoweringError::DataFusion(_)) => t.unparseable += 1,
            Err(_) => t.rejected += 1,
        }
    }
    eprintln!("synthetic-packet-trace SQL corpus: {t:?}");

    // The document enumerates exactly 70 queries.
    assert_eq!(t.total(), 70, "expected 70 DQC queries, got {t:?}");

    // Parse/plan every one — the front end accepts 100% of the benchmark SQL.
    assert_eq!(t.unparseable, 0, "some DQC queries failed to parse/plan: {t:?}");

    // Full-coverage ratchet: today the SQL front end lowers ALL 70. A change
    // that can no longer lower some query trips this deliberately.
    assert_eq!(
        t.lowered, 70,
        "SQL lowering coverage regressed below full DQC coverage: {t:?}"
    );
}

impl Tally {
    fn total(&self) -> usize {
        self.lowered + self.rejected + self.unparseable
    }
}

// ── shape tests (verbatim corpus queries) ─────────────────────────────────────

#[tokio::test]
async fn packet_count_is_count_intent() {
    // P-COUNT — `SELECT COUNT(*) FROM packets`.
    let qe = lower("SELECT COUNT(*) AS total_packets FROM packets").await;
    let (by, aggs) = first_aggregate(&qe).expect("expected an Aggregate");
    assert!(by.is_empty(), "global count has no grouping");
    assert!(matches!(aggs.as_slice(), [AggIntent::Count { .. }]));
}

#[tokio::test]
async fn distinct_source_ips_is_cardinality() {
    // P-SRCIP-CD — `COUNT(DISTINCT srcip)` → the `Cardinality` intent.
    let qe = lower("SELECT COUNT(DISTINCT srcip) AS n_src_ips FROM packets").await;
    let (_, aggs) = first_aggregate(&qe).expect("expected an Aggregate");
    assert!(matches!(aggs.as_slice(), [AggIntent::Cardinality { .. }]));
}

#[tokio::test]
async fn multi_arg_count_distinct_flow_is_cardinality() {
    // SP-CD-FLOW — distinct 5-tuples per source port. The multi-column
    // `COUNT(DISTINCT srcip, dstip, srcport, dstport, proto)` is still one
    // `Cardinality` (of the tuple), grouped by srcport.
    let qe = lower(
        "SELECT srcport, COUNT(DISTINCT srcip, dstip, srcport, dstport, proto) AS n \
         FROM packets GROUP BY srcport ORDER BY n DESC",
    )
    .await;
    let (by, aggs) = first_aggregate(&qe).expect("expected an Aggregate");
    assert_eq!(by.len(), 1, "grouped by srcport");
    assert!(matches!(aggs.as_slice(), [AggIntent::Cardinality { .. }]));
}

#[tokio::test]
async fn five_tuple_flow_group_by_binds_five_keys() {
    // FT-PKT — packets per 5-tuple flow. GROUP BY of the whole flow → five
    // positional group keys with a plain `Count`.
    let qe = lower(
        "SELECT srcip, dstip, srcport, dstport, proto, COUNT(*) AS pkts \
         FROM packets GROUP BY srcip, dstip, srcport, dstport, proto ORDER BY pkts DESC",
    )
    .await;
    let (by, aggs) = first_aggregate(&qe).expect("expected an Aggregate");
    assert_eq!(by.len(), 5, "grouped by the 5-tuple");
    assert!(matches!(aggs.as_slice(), [AggIntent::Count { .. }]));
}

#[tokio::test]
async fn bytes_per_source_ip_is_sum() {
    // SI-BYTE — `SUM(pkt_len)` per source IP.
    let qe = lower("SELECT srcip, SUM(pkt_len) AS bytes FROM packets GROUP BY srcip").await;
    let (by, aggs) = first_aggregate(&qe).expect("expected an Aggregate");
    assert_eq!(by.len(), 1);
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
}

#[tokio::test]
async fn stateful_interarrival_uses_a_window_function() {
    // SI-AINT — the inter-arrival CTE uses `LAG(time) OVER (PARTITION BY srcip
    // ORDER BY time)`, which lowers to an analytic `WindowFunc` node.
    let qe = lower(
        "WITH gaps AS (\
            SELECT srcip, time - LAG(time) OVER (PARTITION BY srcip ORDER BY time) AS gap \
            FROM packets\
         ) \
         SELECT srcip, AVG(gap) AS avg_interval \
         FROM gaps GROUP BY srcip HAVING COUNT(*) > 10 ORDER BY avg_interval DESC",
    )
    .await;
    assert!(
        has_window_func(&qe),
        "the LAG(...) OVER (...) inter-arrival gap must lower to a WindowFunc, got {qe:?}"
    );
}

#[tokio::test]
async fn stddev_pop_of_gaps_is_population_stddev() {
    // SI-SINT — `STDDEV_POP(gap)` over the inter-arrival CTE → population stddev.
    let qe = lower(
        "WITH gaps AS (\
            SELECT srcip, time - LAG(time) OVER (PARTITION BY srcip ORDER BY time) AS gap \
            FROM packets\
         ) \
         SELECT srcip, STDDEV_POP(gap) AS std_iat FROM gaps WHERE gap IS NOT NULL GROUP BY srcip",
    )
    .await;
    assert!(
        intents(&qe)
            .iter()
            .any(|i| matches!(i, AggIntent::StdDev { population: true, .. })),
        "STDDEV_POP must lower to a population StdDev intent, got {:?}",
        intents(&qe)
    );
}
