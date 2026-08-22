//! Real-world **SQL** conformance over a BGP-update / RIB-snapshot analytics
//! corpus, parsed as `SqlDialect::ClickhouseSQL`.
//!
//! Source: 15 real ClickHouse queries against a `bgp_updates` /
//! `bgp_rib_state` dataset (`tests/bgp_analytics/data/bgp_analytics.sql`) --
//! prefix update/withdrawal top-k, AS-path parsing for origin-ASN
//! attribution, MOAS detection, prefix deaggregation, RIB visibility/churn.
//!
//! Unlike the DQC and netflow SQL corpora, this one does **not** pin full
//! coverage: only queries 1, 2, 6, 12 (plain `COUNT`/`GROUP BY`/`ORDER
//! BY`/`LIMIT`, plus `uniqExact` -- rewritten to `COUNT(DISTINCT ...)` by
//! DataFusion's own `Analyzer` before `lower_plan` runs, see
//! `UniqExactRewrite` in `sql/mod.rs`) lower end to end. The other 11 fail
//! for two distinct reasons -- distinct `DataFusionError` variants, not just
//! "some `SqlError::DataFusion`" -- and `EXPECTED` below pins each query to
//! its specific variant *and* a snippet of the error message, so a query
//! silently drifting from one failure mode to the other (e.g. a grammar gap
//! getting "fixed" into an unknown-function error, or vice versa) fails the
//! test even though the aggregate 4/9/2 split wouldn't otherwise move:
//!   - 9 queries hit `DataFusionError::Plan` ("unknown function"): they use
//!     ClickHouse-only builtins (`toIntervalMinute`, `lagInFrame`,
//!     `isIPAddressInRange`, `arrayJoin`, `arrayFilter`, ...) that parse fine
//!     under the ClickHouse dialect but have no DataFusion planner equivalent
//!     registered. (`toStartOfInterval` itself is registered -- issue #230 --
//!     but query 7 nests an unregistered `toIntervalMinute(...)` call inside
//!     it, so it still lands here, just one function name deeper.)
//!   - 2 queries (14, 15) hit `DataFusionError::SQL` (a `ParserError`): they
//!     use ClickHouse grammar the vendored sqlparser doesn't implement at
//!     all -- a scalar/tuple `WITH <expr> AS <alias>` binding, and the
//!     parenthesis-free `USING <col>` shorthand.
//!
//! The totality guarantee still holds: every query returns `Ok` or a clean
//! `Err`, never panics. The pinned per-query outcomes document today's real
//! coverage so a regression (or a future improvement) is visible, not silent.

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog, SqlError as LoweringError};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::{AggIntent, GroupKeys, QueryExpr};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;
use datafusion::error::DataFusionError;

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

/// Pinned outcome for one corpus query.
#[derive(Clone, Copy, Debug)]
enum Expected {
    /// Lowers end to end.
    Lowered,
    /// `DataFusionError::Plan` -- parses fine, no planner equivalent for a
    /// ClickHouse-only builtin. Payload is a (lowercased) substring of the
    /// "Invalid function '...'" message, i.e. which builtin tripped it.
    UnknownFunction(&'static str),
    /// `DataFusionError::SQL` -- the vendored sqlparser grammar doesn't
    /// implement the construct at all. Payload is a substring of the
    /// `ParserError` message.
    UnsupportedGrammar(&'static str),
}

/// Expected outcome for corpus queries 1-15, in order. See the module doc
/// comment for why each query lands where it does.
const EXPECTED: &[Expected] = &[
    Expected::Lowered,                        // 1
    Expected::Lowered,                        // 2
    Expected::UnknownFunction("arrayfilter"), // 3
    Expected::UnknownFunction("arrayfilter"), // 4
    Expected::UnknownFunction("arrayfilter"), // 5
    Expected::Lowered,                        // 6
    // `toStartOfInterval` itself is registered (issue #230), so planning
    // proceeds into its nested `toIntervalMinute(5)` argument -- an
    // unregistered ClickHouse builtin outside this issue's 10-function
    // scope -- and fails there instead.
    Expected::UnknownFunction("tointervalminute"), // 7
    Expected::UnknownFunction("arrayfilter"),      // 8
    Expected::UnknownFunction("isipaddressinrange"), // 9
    Expected::UnknownFunction("arrayfilter"),      // 10
    Expected::UnknownFunction("laginframe"),       // 11
    Expected::Lowered,                             // 12
    Expected::UnknownFunction("arrayjoin"),        // 13
    Expected::UnsupportedGrammar("Expected: identifier, found: ("), // 14
    Expected::UnsupportedGrammar("Expected: a list of columns in parentheses"), // 15
];

#[derive(Default, Debug)]
struct Tally {
    lowered: usize,
    unknown_function: usize,
    unsupported_grammar: usize,
}

#[tokio::test]
async fn corpus_lowering_matches_the_pinned_per_query_outcome() {
    let qs = queries();
    assert_eq!(
        qs.len(),
        EXPECTED.len(),
        "corpus fixture and expectation table drifted"
    );

    let mut t = Tally::default();
    for (i, (q, expected)) in qs.iter().zip(EXPECTED).enumerate() {
        let case = i + 1;
        // A panic here (not an `Err`) fails the test -- the totality guarantee.
        match (lower(q).await, expected) {
            (Ok(_), Expected::Lowered) => t.lowered += 1,
            (
                Err(LoweringError::DataFusion(DataFusionError::Plan(msg))),
                Expected::UnknownFunction(needle),
            ) => {
                assert!(
                    msg.to_lowercase().contains(needle),
                    "q{case} expected the planning error to mention '{needle}', got: {msg}"
                );
                t.unknown_function += 1;
            }
            (
                Err(LoweringError::DataFusion(DataFusionError::SQL(parse_err, _))),
                Expected::UnsupportedGrammar(needle),
            ) => {
                let msg = parse_err.to_string();
                assert!(
                    msg.contains(needle),
                    "q{case} expected the parser error to mention '{needle}', got: {msg}"
                );
                t.unsupported_grammar += 1;
            }
            (Ok(_), expected) => {
                panic!("q{case} was pinned to fail as {expected:?}, but it lowered successfully")
            }
            (Err(e), expected) => {
                panic!("q{case} expected {expected:?}, got a different outcome: {e}")
            }
        }
    }
    eprintln!("bgp-analytics SQL corpus: {t:?}");

    // Coverage ratchet -- queries 1, 2, 6, 12 lower (6 and 12's `uniqExact`
    // calls are rewritten to `COUNT(DISTINCT ...)` before `lower_plan` ever
    // sees them, see `UniqExactRewrite` in `sql/mod.rs`); further
    // ClickHouse-builtin UDF support or vendored-grammar fixes should move the
    // affected query's entry in `EXPECTED` to `Lowered` (raising `t.lowered`
    // here) rather than just this summary.
    assert_eq!(
        t.lowered, 4,
        "BGP analytics SQL coverage changed -- update EXPECTED if support for \
         a ClickHouse builtin or grammar gap was added: {t:?}"
    );
    assert_eq!(
        t.unknown_function, 9,
        "BGP analytics SQL coverage changed: {t:?}"
    );
    assert_eq!(
        t.unsupported_grammar, 2,
        "BGP analytics SQL coverage changed: {t:?}"
    );
}

fn first_aggregate(qe: &QueryExpr) -> Option<(&GroupKeys, &Vec<AggIntent>)> {
    match qe {
        QueryExpr::Aggregate {
            reduction,
            measures,
            ..
        } => Some((reduction.expect_reduce(), measures)),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Dedup { child, .. }
        | QueryExpr::PromqlSubquery { child, .. } => first_aggregate(child),
        _ => None,
    }
}

/// Both top-k queries (1: updates, 2: withdrawals) share the same shape:
/// `Count` grouped by `prefix` (column 4), with the top-k `LIMIT` at the root.
const TOP_K_CASES: &[(usize, &str)] = &[(0, "update-count"), (1, "withdrawal-count")];

#[tokio::test]
async fn top_k_queries_are_count_grouped_by_prefix() {
    let qs = queries();
    for &(idx, label) in TOP_K_CASES {
        let qe = lower(&qs[idx])
            .await
            .unwrap_or_else(|e| panic!("q{} ({label}) failed: {e}", idx + 1));
        let (by, measures) = first_aggregate(&qe)
            .unwrap_or_else(|| panic!("q{} ({label}) expected an Aggregate", idx + 1));
        assert_eq!(
            *by,
            GroupKeys::by(vec![4]),
            "q{} ({label}) should be grouped by prefix (column 4)",
            idx + 1
        );
        assert!(
            matches!(measures.as_slice(), [AggIntent::Count { .. }]),
            "q{} ({label}) expected a single Count aggregate, got {measures:?}",
            idx + 1
        );
        assert!(
            matches!(qe, QueryExpr::Limit { .. }),
            "q{} ({label}) top-k shape keeps the LIMIT at the root: {qe:?}",
            idx + 1
        );
    }
}
