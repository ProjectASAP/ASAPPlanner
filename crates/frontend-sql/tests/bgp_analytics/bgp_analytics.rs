//! Real-world **SQL** conformance over a BGP-update / RIB-snapshot analytics
//! corpus, parsed as `SqlDialect::ClickhouseSQL`.
//!
//! Source: 15 real ClickHouse queries against a `bgp_updates` /
//! `bgp_rib_state` dataset (`tests/bgp_analytics/data/bgp_analytics.sql`) --
//! prefix update/withdrawal top-k, AS-path parsing for origin-ASN
//! attribution, MOAS detection, prefix deaggregation, RIB visibility/churn.
//!
//! Unlike the DQC and netflow SQL corpora, this one does **not** pin full
//! coverage: only queries 1-2 (plain `COUNT`/`GROUP BY`/`ORDER BY`/`LIMIT`)
//! lower end to end. The other 13 fail for two reasons, both clean errors
//! (never panics) surfaced as `SqlError::DataFusion`:
//!   - 11 use ClickHouse-only builtins (`uniqExact`, `countIf`,
//!     `toStartOfInterval`, `lagInFrame`, `isIPAddressInRange`, `arrayJoin`,
//!     `arrayFilter`, ...) that parse fine under the ClickHouse dialect but
//!     have no DataFusion planner equivalent registered.
//!   - 2 (queries 14, 15) use ClickHouse grammar the vendored sqlparser
//!     doesn't implement at all: a scalar/tuple `WITH <expr> AS <alias>`
//!     binding, and the parenthesis-free `USING <col>` shorthand.
//!
//! The totality guarantee still holds: every query returns `Ok` or a clean
//! `LoweringError`, never panics. The pinned counts document today's real
//! coverage so a regression (or a future improvement) is visible, not silent.

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog, SqlError as LoweringError};
use asap_ir::intent_algebra::schema::{Column, DataType, Schema};
use asap_ir::intent_algebra::{AggIntent, GroupKeys, QueryExpr};
use asap_ir::types::AccuracyTarget;
use asap_ir::workload::SqlDialect;

const CORPUS: &str = include_str!("data/bgp_analytics.sql");

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

/// `bgp_updates(timestamp, collector, peer_ip, peer_asn, prefix, operation,
/// as_path)` and `bgp_rib_state(snapshot_ts, collector, peer_ip, peer_asn,
/// prefix)`, each registered both bare and under a `bgp.` schema qualifier --
/// the corpus references both forms verbatim.
fn catalog() -> SqlCatalog {
    let updates = Schema::new(vec![
        col("timestamp", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("peer_ip", DataType::Utf8),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
        col("operation", DataType::Utf8),
        col("as_path", DataType::Utf8),
    ]);
    let rib = Schema::new(vec![
        col("snapshot_ts", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("peer_ip", DataType::Utf8),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
    ]);
    SqlCatalog::new()
        .with_table("bgp_updates", updates.clone())
        .with_table("bgp.bgp_updates", updates)
        .with_table("bgp_rib_state", rib.clone())
        .with_table("bgp.bgp_rib_state", rib)
}

/// Corpus statements. Comment (`#` / `--`) and blank lines are stripped
/// **before** splitting on `;` -- comments here don't contain semicolons, but
/// this keeps the same shape as the DQC/netflow corpus parsers.
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

async fn lower(q: &str) -> Result<QueryExpr, LoweringError> {
    lower_sql_dialect(
        q,
        &catalog(),
        SqlDialect::ClickhouseSQL,
        AccuracyTarget::Exact,
    )
    .await
}

#[derive(Default, Debug)]
struct Tally {
    lowered: usize,
    rejected: usize,
    unparseable: usize,
}

impl Tally {
    fn total(&self) -> usize {
        self.lowered + self.rejected + self.unparseable
    }
}

#[tokio::test]
async fn corpus_lowering_is_total_over_the_bgp_analytics_corpus() {
    let mut t = Tally::default();
    for q in queries() {
        // A panic here (not an `Err`) fails the test -- the totality guarantee.
        match lower(&q).await {
            Ok(_) => t.lowered += 1,
            Err(LoweringError::DataFusion(_)) => t.unparseable += 1,
            Err(_) => t.rejected += 1,
        }
    }
    eprintln!("bgp-analytics SQL corpus: {t:?}");

    assert_eq!(
        t.total(),
        15,
        "expected 15 BGP analytics queries, got {t:?}"
    );

    // Coverage ratchet -- today only queries 1-2 lower; ClickHouse-builtin
    // UDF support or vendored-grammar fixes would raise this deliberately.
    assert_eq!(
        t.lowered, 2,
        "BGP analytics SQL coverage changed -- update this assertion \
         if support for a ClickHouse builtin or grammar gap was added: {t:?}"
    );
    assert_eq!(
        t.unparseable, 13,
        "BGP analytics SQL coverage changed: {t:?}"
    );
    assert_eq!(t.rejected, 0, "unexpected non-DataFusion rejection: {t:?}");
}

fn first_aggregate(qe: &QueryExpr) -> Option<(&GroupKeys, &Vec<AggIntent>)> {
    match qe {
        QueryExpr::Aggregate { by, aggs, .. } => Some((by, aggs)),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Subquery { child, .. } => first_aggregate(child),
        _ => None,
    }
}

#[tokio::test]
async fn update_count_top_k_is_count_grouped_by_prefix() {
    let qs = queries();
    let qe = lower(&qs[0])
        .await
        .unwrap_or_else(|e| panic!("q1 failed: {e}"));
    let (by, aggs) = first_aggregate(&qe).expect("expected an Aggregate");
    assert_eq!(*by, GroupKeys::by(vec![4]), "grouped by prefix (column 4)");
    assert!(matches!(aggs.as_slice(), [AggIntent::Count { .. }]));
    assert!(
        matches!(qe, QueryExpr::Limit { .. }),
        "top-k shape keeps the LIMIT at the root: {qe:?}"
    );
}

#[tokio::test]
async fn withdrawal_count_top_k_is_count_grouped_by_prefix() {
    let qs = queries();
    let qe = lower(&qs[1])
        .await
        .unwrap_or_else(|e| panic!("q2 failed: {e}"));
    let (by, aggs) = first_aggregate(&qe).expect("expected an Aggregate");
    assert_eq!(*by, GroupKeys::by(vec![4]), "grouped by prefix (column 4)");
    assert!(matches!(aggs.as_slice(), [AggIntent::Count { .. }]));
    assert!(
        matches!(qe, QueryExpr::Limit { .. }),
        "top-k shape keeps the LIMIT at the root: {qe:?}"
    );
}
