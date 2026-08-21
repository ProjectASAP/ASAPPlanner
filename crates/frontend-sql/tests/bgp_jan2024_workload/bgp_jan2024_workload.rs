//! Real-world **SQL** conformance over a 200-query BGP analyst workload,
//! parsed as `SqlDialect::ClickhouseSQL`.
//!
//! Source: `data/bgp_jan2024_rrc00_200_query_workload.yaml`, copied verbatim
//! (byte-for-byte) from ASAPQuery PR #561
//! (`local_experiments/bgp_jan2024_rrc00_200_query_workload.yaml`) -- an
//! LLM-authored analyst workload against a `bgp.bgp_updates` table, each
//! entry carrying an `id`/`title`/`analyst_question`/`window` alongside the
//! `sql`. Parsed here as YAML (not hand-copied into the flat `.sql` shape
//! the other SQL corpora use) so the query text is never retyped.
//!
//! Unlike `bgp_analytics` (15 hand-picked production queries, pinned
//! per-query), this corpus is 200 queries wide and only pins an **aggregate
//! tally** by outcome category -- see the module doc on [`Category`] for why.

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog, SqlError};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;
use datafusion::error::DataFusionError;
use serde::Deserialize;
use std::collections::BTreeMap;

const WORKLOAD_YAML: &str = include_str!("data/bgp_jan2024_rrc00_200_query_workload.yaml");

#[derive(Deserialize)]
struct Workload {
    queries: Vec<QueryCase>,
}

#[derive(Deserialize)]
struct QueryCase {
    id: String,
    sql: String,
}

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

/// `bgp.bgp_updates`, widened past the 7-column `bgp_analytics` schema with
/// the extra BGP-attribute columns this workload references directly
/// (`origin`, `next_hop`, `local_pref`, `med`, `communities`, `atomic`,
/// `aggr_asn`, `aggr_ip`, `source_file`) -- every query in the corpus wraps
/// the numeric-looking ones (`local_pref`, `med`, `origin`, `aggr_asn`) in
/// `toString`/`toInt64OrZero`/`toFloat64OrZero` before use, and treats `''`
/// as their "missing" sentinel, so they're modeled as `Utf8` like the rest
/// of the free-text columns rather than a numeric type.
fn catalog() -> SqlCatalog {
    let updates = Schema::new(vec![
        col("timestamp", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("peer_ip", DataType::Utf8),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
        col("operation", DataType::Utf8),
        col("as_path", DataType::Utf8),
        col("origin", DataType::Utf8),
        col("next_hop", DataType::Utf8),
        col("local_pref", DataType::Utf8),
        col("med", DataType::Utf8),
        col("communities", DataType::Utf8),
        col("atomic", DataType::Utf8),
        col("aggr_asn", DataType::Utf8),
        col("aggr_ip", DataType::Utf8),
        col("source_file", DataType::Utf8),
    ]);
    SqlCatalog::new()
        .with_table("bgp_updates", updates.clone())
        .with_table("bgp.bgp_updates", updates)
}

async fn lower(q: &str) -> Result<asap_types::pre_asap::QueryExpr, SqlError> {
    lower_sql_dialect(
        q,
        &catalog(),
        SqlDialect::ClickhouseSQL,
        AccuracyTarget::Exact,
    )
    .await
}

/// Coarse outcome bucket for a corpus query. Deliberately coarser than
/// `bgp_analytics`'s per-query `Expected` enum: at 200 queries, pinning an
/// exact error-message needle per query would mean 200 hand-maintained
/// entries, and DataFusion's "Did you mean 'x'?" spelling suggestion on
/// `Plan` errors is **not deterministic** across process runs (confirmed by
/// re-running the same corpus twice and seeing the suggested function name
/// change) -- so a message-snippet needle on that text would be flaky. The
/// stable, useful signal is which *kind* of failure a query hits; this tally
/// is that signal, ratcheted so a category shifting size is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    Lowered,
    /// `DataFusionError::Plan` -- almost entirely "unknown function" for a
    /// ClickHouse-only builtin (`uniqExact`, `countIf`, `splitByChar`, ...).
    Plan,
    /// `DataFusionError::SchemaError` -- column genuinely absent even from
    /// the widened catalog above.
    Schema,
    /// `DataFusionError::SQL` -- vendored sqlparser can't parse the
    /// construct at all.
    Parse,
    /// `DataFusionError::NotImplemented` -- parses and plans far enough to
    /// hit an explicitly-unimplemented DataFusion code path (e.g. map/array
    /// index access).
    NotImplemented,
    /// `SqlError::UnsupportedFeature` -- recognized by our own L1→L2 step
    /// but not yet lowered (e.g. `NOT IN (subquery)`).
    UnsupportedFeature,
    /// Every other `SqlError`/`DataFusionError` variant.
    Other,
}

fn categorize(err: &SqlError) -> Category {
    match err {
        SqlError::DataFusion(DataFusionError::Plan(_)) => Category::Plan,
        SqlError::DataFusion(DataFusionError::SchemaError(_, _)) => Category::Schema,
        SqlError::DataFusion(DataFusionError::SQL(_, _)) => Category::Parse,
        SqlError::DataFusion(DataFusionError::NotImplemented(_)) => Category::NotImplemented,
        SqlError::UnsupportedFeature(_) => Category::UnsupportedFeature,
        _ => Category::Other,
    }
}

#[tokio::test]
async fn corpus_lowering_matches_the_pinned_aggregate_tally() {
    let workload: Workload =
        serde_yaml::from_str(WORKLOAD_YAML).expect("corpus YAML failed to parse");
    assert_eq!(
        workload.queries.len(),
        200,
        "corpus fixture drifted from the 200 queries this test pins"
    );

    let mut tally: BTreeMap<Category, usize> = BTreeMap::new();
    for case in &workload.queries {
        // A panic here (not an `Err`) fails the test -- the totality guarantee.
        let category = match lower(&case.sql).await {
            Ok(_) => Category::Lowered,
            Err(e) => categorize(&e),
        };
        *tally.entry(category).or_default() += 1;
        if category == Category::Other {
            // Not pinned by construction (see `Category::Other` doc) --
            // surface which query and error so a new failure mode is
            // diagnosable instead of just silently counted.
            let err = lower(&case.sql).await.unwrap_err();
            eprintln!("{} landed in Category::Other: {err}", case.id);
        }
    }
    eprintln!("bgp_jan2024_workload SQL corpus tally: {tally:?}");

    let expect = |c: Category, n: usize| {
        assert_eq!(
            tally.get(&c).copied().unwrap_or(0),
            n,
            "bgp_jan2024_workload coverage changed for {c:?} -- update the pinned \
             tally if support for a ClickHouse builtin, schema column, or grammar \
             gap was added/removed: {tally:?}"
        );
    };
    expect(Category::Lowered, 64);
    expect(Category::Plan, 127);
    expect(Category::Schema, 0);
    expect(Category::Parse, 0);
    expect(Category::NotImplemented, 3);
    expect(Category::UnsupportedFeature, 6);
    expect(Category::Other, 0);
}
