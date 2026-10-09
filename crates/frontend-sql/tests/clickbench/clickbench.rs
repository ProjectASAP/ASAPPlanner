//! Real-world **SQL** conformance over the ClickBench corpus, parsed as
//! `SqlDialect::ClickhouseSQL`.
//!
//! Source: ClickBench's 43 queries (`tests/clickbench/data/clickbench.sql`,
//! copied verbatim; provenance and license in that file's header). Every query
//! reads one wide clickstream table, `hits`, and most are a filtered `COUNT`
//! or `COUNT(DISTINCT ...)`, grouped and ranked top-k, which is the shape ASAP
//! summaries target.
//!
//! The columns are mixed case (`UserID`, `SearchPhrase`), and ClickHouse
//! identifiers are case-sensitive, so the corpus also pins that `ClickhouseSQL`
//! does not fold unquoted identifiers to lowercase.
//!
//! Two guarantees:
//!   1. **Totality**: every query returns `Ok` or a clean `Err`, never panics.
//!   2. **Coverage ratchet**: all 43 lower today.

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog, SqlError as LoweringError};
use asap_types::pre_asap::schema::{ColumnId, DataType, Field, Schema};
use asap_types::pre_asap::{AggIntent, GroupKeys, QueryExpr};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;

const CORPUS: &str = include_str!("data/clickbench.sql");

/// `hits`, in ClickBench's `clickhouse/create.sql` column order. Integer
/// widths collapse to `Int64` and `TEXT`/`VARCHAR`/`CHAR` to `Utf8`. No
/// `unique_keys`: ClickBench's `PRIMARY KEY` is a ClickHouse sort key, not a
/// uniqueness constraint.
const COLUMNS: &[(&str, DataType)] = &[
    ("WatchID", DataType::Int64),
    ("JavaEnable", DataType::Int64),
    ("Title", DataType::Utf8),
    ("GoodEvent", DataType::Int64),
    ("EventTime", DataType::Timestamp),
    ("EventDate", DataType::Date),
    ("CounterID", DataType::Int64),
    ("ClientIP", DataType::Int64),
    ("RegionID", DataType::Int64),
    ("UserID", DataType::Int64),
    ("CounterClass", DataType::Int64),
    ("OS", DataType::Int64),
    ("UserAgent", DataType::Int64),
    ("URL", DataType::Utf8),
    ("Referer", DataType::Utf8),
    ("IsRefresh", DataType::Int64),
    ("RefererCategoryID", DataType::Int64),
    ("RefererRegionID", DataType::Int64),
    ("URLCategoryID", DataType::Int64),
    ("URLRegionID", DataType::Int64),
    ("ResolutionWidth", DataType::Int64),
    ("ResolutionHeight", DataType::Int64),
    ("ResolutionDepth", DataType::Int64),
    ("FlashMajor", DataType::Int64),
    ("FlashMinor", DataType::Int64),
    ("FlashMinor2", DataType::Utf8),
    ("NetMajor", DataType::Int64),
    ("NetMinor", DataType::Int64),
    ("UserAgentMajor", DataType::Int64),
    ("UserAgentMinor", DataType::Utf8),
    ("CookieEnable", DataType::Int64),
    ("JavascriptEnable", DataType::Int64),
    ("IsMobile", DataType::Int64),
    ("MobilePhone", DataType::Int64),
    ("MobilePhoneModel", DataType::Utf8),
    ("Params", DataType::Utf8),
    ("IPNetworkID", DataType::Int64),
    ("TraficSourceID", DataType::Int64),
    ("SearchEngineID", DataType::Int64),
    ("SearchPhrase", DataType::Utf8),
    ("AdvEngineID", DataType::Int64),
    ("IsArtifical", DataType::Int64),
    ("WindowClientWidth", DataType::Int64),
    ("WindowClientHeight", DataType::Int64),
    ("ClientTimeZone", DataType::Int64),
    ("ClientEventTime", DataType::Timestamp),
    ("SilverlightVersion1", DataType::Int64),
    ("SilverlightVersion2", DataType::Int64),
    ("SilverlightVersion3", DataType::Int64),
    ("SilverlightVersion4", DataType::Int64),
    ("PageCharset", DataType::Utf8),
    ("CodeVersion", DataType::Int64),
    ("IsLink", DataType::Int64),
    ("IsDownload", DataType::Int64),
    ("IsNotBounce", DataType::Int64),
    ("FUniqID", DataType::Int64),
    ("OriginalURL", DataType::Utf8),
    ("HID", DataType::Int64),
    ("IsOldCounter", DataType::Int64),
    ("IsEvent", DataType::Int64),
    ("IsParameter", DataType::Int64),
    ("DontCountHits", DataType::Int64),
    ("WithHash", DataType::Int64),
    ("HitColor", DataType::Utf8),
    ("LocalEventTime", DataType::Timestamp),
    ("Age", DataType::Int64),
    ("Sex", DataType::Int64),
    ("Income", DataType::Int64),
    ("Interests", DataType::Int64),
    ("Robotness", DataType::Int64),
    ("RemoteIP", DataType::Int64),
    ("WindowName", DataType::Int64),
    ("OpenerName", DataType::Int64),
    ("HistoryLength", DataType::Int64),
    ("BrowserLanguage", DataType::Utf8),
    ("BrowserCountry", DataType::Utf8),
    ("SocialNetwork", DataType::Utf8),
    ("SocialAction", DataType::Utf8),
    ("HTTPError", DataType::Int64),
    ("SendTiming", DataType::Int64),
    ("DNSTiming", DataType::Int64),
    ("ConnectTiming", DataType::Int64),
    ("ResponseStartTiming", DataType::Int64),
    ("ResponseEndTiming", DataType::Int64),
    ("FetchTiming", DataType::Int64),
    ("SocialSourceNetworkID", DataType::Int64),
    ("SocialSourcePage", DataType::Utf8),
    ("ParamPrice", DataType::Int64),
    ("ParamOrderID", DataType::Utf8),
    ("ParamCurrency", DataType::Utf8),
    ("ParamCurrencyID", DataType::Int64),
    ("OpenstatServiceName", DataType::Utf8),
    ("OpenstatCampaignID", DataType::Utf8),
    ("OpenstatAdID", DataType::Utf8),
    ("OpenstatSourceID", DataType::Utf8),
    ("UTMSource", DataType::Utf8),
    ("UTMMedium", DataType::Utf8),
    ("UTMCampaign", DataType::Utf8),
    ("UTMContent", DataType::Utf8),
    ("UTMTerm", DataType::Utf8),
    ("FromTag", DataType::Utf8),
    ("HasGCLID", DataType::Int64),
    ("RefererHash", DataType::Int64),
    ("URLHash", DataType::Int64),
    ("CLID", DataType::Int64),
];

fn catalog() -> SqlCatalog {
    let fields = COLUMNS
        .iter()
        .map(|(name, dtype)| Field::plain(*name, dtype.clone(), false))
        .collect();
    SqlCatalog::new().with_table("hits", Schema::new(fields))
}

/// Position of `name` in `hits`, so assertions name columns instead of
/// hard-coding indices into a 105-column table.
fn column(name: &str) -> ColumnId {
    COLUMNS
        .iter()
        .position(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no column {name} in hits"))
}

/// One query per line, trailing `;` stripped.
fn queries() -> Vec<&'static str> {
    CORPUS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .map(|l| l.strip_suffix(';').unwrap_or(l))
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

/// Every ClickBench query lowers end to end. A query that stops lowering
/// fails here by number with its error, rather than as a moved total.
#[tokio::test]
async fn every_clickbench_query_lowers() {
    let qs = queries();
    assert_eq!(qs.len(), 43, "corpus fixture drifted");
    let mut failed = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        // A panic here (not an `Err`) fails the test: the totality guarantee.
        if let Err(e) = lower(q).await {
            failed.push(format!("q{}: {e}", i + 1));
        }
    }
    assert!(
        failed.is_empty(),
        "ClickBench queries failed to lower:\n{}",
        failed.join("\n")
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
        | QueryExpr::Limit { child, .. } => first_aggregate(child),
        _ => None,
    }
}

/// q5 `COUNT(DISTINCT UserID)` and q9 (distinct users per region) lower to a
/// `Cardinality` over `UserID`, ungrouped and grouped by `RegionID`.
#[tokio::test]
async fn distinct_user_counts_are_cardinality_over_user_id() {
    let qs = queries();
    for (case, by) in [(5, vec![]), (9, vec![column("RegionID")])] {
        let qe = lower(qs[case - 1])
            .await
            .unwrap_or_else(|e| panic!("q{case} failed: {e}"));
        let (keys, measures) =
            first_aggregate(&qe).unwrap_or_else(|| panic!("q{case} expected an Aggregate"));
        assert_eq!(*keys, GroupKeys::by(by), "q{case} grouping");
        assert!(
            matches!(measures.as_slice(), [AggIntent::Cardinality { cols, .. }]
                if cols == &[column("UserID")]),
            "q{case} expected one Cardinality over UserID, got {measures:?}"
        );
    }
}

/// q13 ranks search phrases by `COUNT(*)` with `ORDER BY c DESC LIMIT 10`;
/// canonicalization turns that into a `TopK { k: 10 }` over the per-phrase
/// `Count`, the same shape as PromQL's `topk`.
#[tokio::test]
async fn top_search_phrases_canonicalize_to_top_k() {
    let qe = lower(queries()[12]).await.expect("q13 lowers");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("q13 expected a TopK Aggregate at the root, got {qe:?}");
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::TopK { k: 10, .. }]),
        "q13 expected TopK(10), got {measures:?}"
    );
    let (keys, inner) = first_aggregate(child).expect("q13 expected a per-phrase Aggregate");
    assert_eq!(*keys, GroupKeys::by(vec![column("SearchPhrase")]));
    assert!(
        matches!(inner.as_slice(), [AggIntent::Count { .. }]),
        "q13 expected a per-phrase Count, got {inner:?}"
    );
}
