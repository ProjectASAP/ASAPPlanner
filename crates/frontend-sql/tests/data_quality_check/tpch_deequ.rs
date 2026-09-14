//! Real-world **SQL** conformance over the DQC warehouse-ingestion check set.
//!
//! Source: sidra's `workload/tpch_deequ.yaml` — 50 data-quality checks over
//! TPC-H `lineitem` (completeness, uniqueness, validity, distributional, volume,
//! freshness), each fused into one query whose outer SELECT *is* the check's
//! assertion (`tests/data/tpch_deequ_queries.sql`). Here we only lower them.
//!
//! This is the second SQL corpus beside `synthetic_packet_trace.rs`, and it
//! pins a different shape: where that one is grouped counts and window
//! functions over a packet trace, these are ungrouped `avg(CASE WHEN …)` folds
//! with date and interval arithmetic in the predicate — the shape a check set
//! has when every cell answers one yes/no question about one table.
//!
//! Two guarantees:
//!   1. **Totality** — every query returns `Ok` or a clean `LoweringError`,
//!      never panics.
//!   2. **Coverage ratchet** — 48 of 50 lower today. The two that do not are
//!      `corr(x, y)`, which has no `AggIntent` variant: every aggregate intent
//!      is unary and correlation would be the first binary one. A change that
//!      drops coverage below 48 trips the ratchet.
//!
//! Schema: `lineitem`, the 16 TPC-H columns. The four `DECIMAL(15,2)` columns
//! are declared `Float64` — the canonical `DataType` has no fixed-point type,
//! which is the same narrowing sidra's own catalog file makes.

use asap_frontend_sql::{lower_sql, SqlCatalog, SqlError as LoweringError};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;

const CORPUS: &str = include_str!("data/tpch_deequ_queries.sql");

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

/// No `time_index` and no `unique_keys`: the checks do not slice by time, and
/// declaring `(l_orderkey, l_linenumber)` as a key would let a planner fold
/// U-P2b's `pk_distinctness = 1.0` to a constant — a check that reads no rows
/// is no longer the check the corpus is measuring.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            col("l_orderkey", DataType::Int64),
            col("l_partkey", DataType::Int64),
            col("l_suppkey", DataType::Int64),
            col("l_linenumber", DataType::Int64),
            col("l_quantity", DataType::Float64),
            col("l_extendedprice", DataType::Float64),
            col("l_discount", DataType::Float64),
            col("l_tax", DataType::Float64),
            col("l_returnflag", DataType::Utf8),
            col("l_linestatus", DataType::Utf8),
            col("l_shipdate", DataType::Timestamp),
            col("l_commitdate", DataType::Timestamp),
            col("l_receiptdate", DataType::Timestamp),
            col("l_shipinstruct", DataType::Utf8),
            col("l_shipmode", DataType::Utf8),
            col("l_comment", DataType::Utf8),
        ]),
    )
}

/// One query per line. Not a `;` split: U-P3p's regex literal
/// `'^[a-zA-Z ,.:;!?-]+$'` contains a semicolon, and the generator emits
/// single-line SQL, so the line is the exact statement boundary.
fn queries() -> Vec<String> {
    CORPUS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Default)]
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

/// The front end lowers 48 of the 50 warehouse-ingestion checks, and never
/// panics on any of them.
#[tokio::test]
async fn lowers_the_warehouse_ingestion_check_set() {
    let cat = catalog();
    let mut t = Tally::default();
    for q in queries() {
        match lower_sql(&q, &cat, AccuracyTarget::Exact).await {
            Ok(_) => t.lowered += 1,
            // DataFusion surfaces parse/plan failures as `DataFusion(_)`.
            Err(LoweringError::DataFusion(_)) => t.unparseable += 1,
            Err(_) => t.rejected += 1,
        }
    }
    eprintln!("tpch_deequ SQL corpus: {t:?}");

    assert_eq!(t.total(), 50, "expected 50 DQC checks, got {t:?}");

    assert_eq!(
        t.unparseable, 0,
        "some DQC checks failed to parse/plan: {t:?}"
    );

    // Coverage ratchet. The 2 that do not lower are the `corr` cells; adding a
    // binary `AggIntent` would raise this to 50.
    assert_eq!(
        t.lowered, 48,
        "SQL lowering coverage moved off the DQC ratchet: {t:?}"
    );
}
