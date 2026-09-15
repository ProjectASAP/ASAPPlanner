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
//!   2. **Coverage ratchet** — 47 of 50 lower today. The exact rejected set
//!      consists of one composite COUNT(DISTINCT) and two correlation checks,
//!      whose aggregate semantics the canonical IR cannot represent.
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
            col("l_shipdate", DataType::Date),
            col("l_commitdate", DataType::Date),
            col("l_receiptdate", DataType::Date),
            col("l_shipinstruct", DataType::Utf8),
            col("l_shipmode", DataType::Utf8),
            col("l_comment", DataType::Utf8),
        ]),
    )
}

/// One query per line. Not a `;` split: U-P3p's regex literal
/// `'^[a-zA-Z ,.:;!?-]+$'` contains a semicolon, and the generator emits
/// single-line SQL, so the line is the exact statement boundary.
fn queries() -> Vec<(&'static str, &'static str)> {
    let mut id = None;
    let mut queries = Vec::new();
    for line in CORPUS.lines().map(str::trim) {
        if let Some(query_id) = line.strip_prefix("-- U-") {
            id = Some(query_id);
        } else if !line.is_empty() && !line.starts_with("--") {
            queries.push((id.take().expect("each query has an ID"), line));
        }
    }
    queries
}

// Pin rejected IDs and error reasons so coverage swaps cannot pass the ratchet.
#[tokio::test]
async fn lowers_the_warehouse_ingestion_check_set() {
    let cat = catalog();
    let queries = queries();
    assert_eq!(queries.len(), 50);
    let mut rejected = Vec::new();
    let mut lowered = 0;
    for (id, query) in queries {
        match lower_sql(query, &cat, AccuracyTarget::Exact).await {
            Ok(_) => lowered += 1,
            Err(LoweringError::UnsupportedAggregate(reason)) => rejected.push((id, reason)),
            Err(error) => panic!("unexpected failure for U-{id}: {error}"),
        }
    }
    // P4d and P4l are Pearson correlation checks; they lower since `corr`
    // became `AggIntent::PearsonCorr`.
    assert_eq!(
        rejected,
        vec![("P2b", "multi-column COUNT(DISTINCT)".into())]
    );
    assert_eq!(lowered, 49);
}
