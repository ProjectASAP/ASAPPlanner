//! Real-world **SQL** conformance over sidra's six generated TPC-H
//! data-quality workloads (`workload/tpch_{gate,ml}_*_rich.yaml`): 541 checks
//! in all, each fused into one query whose outer SELECT is the check's
//! assertion (`data/tpch_*_rich_queries.sql`). Here we only lower them.
//!
//! Beside `tpch_deequ.rs` (50 hand-written checks), these are generated from
//! the column profile, so they are wider: per-column statistics, column pairs,
//! slices, and, in the `ml` workloads, distances between two tables.
//!
//! sidra's own functions (`entropy`, `cramers_v`, `tvd_similarity`, ...;
//! sidra's `workload/udf/`) are left as calls, not expanded into the SQL
//! sidra runs them as. None is a DataFusion built-in, so each such query is
//! rejected as an unknown function; the pins below record which function.
//!
//! Two guarantees per workload:
//!   1. **Totality**: every query returns `Ok` or a clean `SqlError`, never
//!      panics.
//!   2. **Coverage ratchet**: the rejected queries are pinned by test id and by
//!      the reason they are rejected, so a query that starts or stops lowering,
//!      or fails for a different reason, fails the test.
//!
//! Two rejections are not sidra functions: `covar_pop` has no aggregate
//! intent, and `variance_pop` is how sidra's exporter spells the file's
//! `var_pop`, a name DataFusion does not have.
//!
//! Schema: the 16 TPC-H `lineitem` columns, `DECIMAL(15,2)` declared `Float64`
//! as in `tpch_deequ.rs`. The `ml` workloads read two tables of that schema:
//! `online`, the data under test, and `training`, the reference.

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::schema::{DataType, Field, Schema};

fn lineitem() -> Schema {
    let col = |name: &str, dtype| Field::plain(name, dtype, false);
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
    ])
}

/// `(test id, query)` pairs. A query is one line, preceded by a `-- <id>`
/// line; header comments have spaces after the `-- ` and an id has none.
fn queries(corpus: &'static str) -> Vec<(&'static str, &'static str)> {
    let mut id = None;
    let mut queries = Vec::new();
    for line in corpus.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("-- ") {
            if !rest.is_empty() && !rest.contains(char::is_whitespace) {
                id = Some(rest);
            }
        } else if !line.is_empty() && !line.starts_with("--") {
            queries.push((id.take().expect("each query has an id"), line));
        }
    }
    queries
}

/// Lowers every query of `corpus` against `tables` (each a `lineitem`) and
/// checks that exactly `rejected` fail, each with its pinned reason.
async fn check(corpus: &'static str, tables: &[&str], total: usize, rejected: &[(&str, &str)]) {
    let catalog = tables
        .iter()
        .fold(SqlCatalog::new(), |c, t| c.with_table(*t, lineitem()));
    let queries = queries(corpus);
    assert_eq!(queries.len(), total);

    let mut actual = Vec::new();
    for (id, query) in queries {
        if let Err(error) = lower_sql(query, &catalog).await {
            actual.push((id, error.to_string()));
        }
    }
    let unexpected: Vec<_> = actual
        .iter()
        .filter(|(id, msg)| {
            !rejected
                .iter()
                .any(|(want, why)| want == id && msg.contains(why))
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected rejection(s): {unexpected:#?}"
    );
    let now_lowering: Vec<_> = rejected
        .iter()
        .filter(|(want, _)| !actual.iter().any(|(id, _)| id == want))
        .collect();
    assert!(
        now_lowering.is_empty(),
        "pinned as rejected but now lower; unpin them: {now_lowering:#?}"
    );
}

/// `tpch_gate_basic_rich`: 44 of 45 lower; the rest call a sidra UDF.
#[tokio::test]
async fn gate_basic() {
    check(
        include_str!("data/tpch_gate_basic_rich_queries.sql"),
        &["lineitem"],
        45,
        &[("UG-Distribution-013", "Invalid function 'value_range'")],
    )
    .await;
}

/// `tpch_gate_complex_rich`: 92 of 124 lower; the rest call a sidra UDF or `covar_pop`.
#[tokio::test]
async fn gate_complex() {
    check(
        include_str!("data/tpch_gate_complex_rich_queries.sql"),
        &["lineitem"],
        124,
        &[
            ("UG-Distribution-044", "unsupported aggregate: covar_pop"),
            ("UG-Distribution-042", "Invalid function 'cramers_v'"),
            ("UG-Distribution-045", "Invalid function 'cramers_v'"),
            ("UG-Distribution-019", "Invalid function 'entropy'"),
            ("UG-Distribution-023", "Invalid function 'entropy'"),
            ("UG-Distribution-029", "Invalid function 'entropy'"),
            ("UG-Distribution-032", "Invalid function 'entropy'"),
            ("US-Distribution-005", "Invalid function 'entropy'"),
            ("US-Distribution-006", "Invalid function 'entropy'"),
            ("US-Distribution-007", "Invalid function 'entropy'"),
            ("US-Distribution-009", "Invalid function 'entropy'"),
            ("US-Distribution-010", "Invalid function 'entropy'"),
            ("UG-Distribution-014", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-022", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-031", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-017", "Invalid function 'iqr'"),
            ("UG-Distribution-018", "Invalid function 'iqr'"),
            ("UG-Distribution-020", "Invalid function 'iqr'"),
            ("UG-Distribution-021", "Invalid function 'iqr'"),
            ("UG-Distribution-026", "Invalid function 'kurtosis'"),
            ("UG-Distribution-013", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-015", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-034", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-036", "Invalid function 'mode_frequency'"),
            ("US-Distribution-004", "Invalid function 'mode_frequency'"),
            ("US-Distribution-008", "Invalid function 'mode_frequency'"),
            ("US-Distribution-011", "Invalid function 'mode_frequency'"),
            (
                "UG-Distribution-041",
                "Invalid function 'mutual_information'",
            ),
            ("UG-Distribution-016", "Invalid function 'skewness'"),
            ("UG-Distribution-037", "Invalid function 'skewness'"),
            ("UG-Distribution-040", "Invalid function 'spearman'"),
            ("UG-Distribution-033", "Invalid function 'top_k_share'"),
        ],
    )
    .await;
}

/// `tpch_gate_distribution_rich`: 51 of 78 lower; the rest call a sidra UDF.
#[tokio::test]
async fn gate_distribution() {
    check(
        include_str!("data/tpch_gate_distribution_rich_queries.sql"),
        &["lineitem"],
        78,
        &[
            ("UG-Distribution-051", "Invalid function 'cramers_v'"),
            ("UG-Distribution-021", "Invalid function 'entropy'"),
            ("UG-Distribution-027", "Invalid function 'entropy'"),
            ("UG-Distribution-036", "Invalid function 'entropy'"),
            ("UG-Distribution-040", "Invalid function 'entropy'"),
            ("UG-Distribution-016", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-026", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-029", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-039", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-019", "Invalid function 'iqr'"),
            ("UG-Distribution-020", "Invalid function 'iqr'"),
            ("UG-Distribution-022", "Invalid function 'iqr'"),
            ("UG-Distribution-025", "Invalid function 'iqr'"),
            ("UG-Distribution-031", "Invalid function 'kurtosis'"),
            ("UG-Distribution-038", "Invalid function 'kurtosis'"),
            ("UG-Distribution-015", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-017", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-042", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-044", "Invalid function 'mode_frequency'"),
            (
                "UG-Distribution-050",
                "Invalid function 'mutual_information'",
            ),
            ("UG-Distribution-035", "Invalid function 'outlier_ratio'"),
            ("UG-Distribution-018", "Invalid function 'skewness'"),
            ("UG-Distribution-023", "Invalid function 'skewness'"),
            ("UG-Distribution-028", "Invalid function 'skewness'"),
            ("UG-Distribution-045", "Invalid function 'skewness'"),
            ("UG-Distribution-049", "Invalid function 'spearman'"),
            ("UG-Distribution-041", "Invalid function 'top_k_share'"),
        ],
    )
    .await;
}

/// `tpch_gate_slice_rich`: 77 of 77 lower.
#[tokio::test]
async fn gate_slice() {
    check(
        include_str!("data/tpch_gate_slice_rich_queries.sql"),
        &["lineitem"],
        77,
        &[],
    )
    .await;
}

/// `tpch_ml_basic_rich`: 56 of 70 lower; the rest call a sidra UDF or `variance_pop`.
#[tokio::test]
async fn ml_basic() {
    check(
        include_str!("data/tpch_ml_basic_rich_queries.sql"),
        &["online", "training"],
        70,
        &[
            ("UG-Distribution-034", "Invalid function 'cramers_v'"),
            ("UG-Distribution-036", "Invalid function 'cramers_v'"),
            ("UG-Distribution-020", "Invalid function 'entropy'"),
            ("UG-Distribution-022", "Invalid function 'entropy'"),
            ("UG-Distribution-031", "Invalid function 'entropy'"),
            ("UG-Distribution-017", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-015", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-018", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-032", "Invalid function 'mode_frequency'"),
            (
                "UG-Distribution-035",
                "Invalid function 'mutual_information'",
            ),
            (
                "UG-Distribution-037",
                "Invalid function 'mutual_information'",
            ),
            ("UG-Distribution-038", "Invalid function 'spearman'"),
            ("UG-Distribution-004", "Invalid function 'variance_pop'"),
            ("UG-Distribution-007", "Invalid function 'variance_pop'"),
        ],
    )
    .await;
}

/// `tpch_ml_complex_rich`: 75 of 147 lower; the rest call a sidra UDF or `variance_pop`.
#[tokio::test]
async fn ml_complex() {
    check(
        include_str!("data/tpch_ml_complex_rich_queries.sql"),
        &["online", "training"],
        147,
        &[
            ("BG-Distribution-029", "Invalid function 'cohens_d'"),
            ("BG-Distribution-031", "Invalid function 'cohens_d'"),
            ("BG-Distribution-032", "Invalid function 'cohens_d'"),
            ("BG-Distribution-023", "Invalid function 'cramers_v'"),
            ("BG-Distribution-027", "Invalid function 'cramers_v'"),
            ("UG-Distribution-027", "Invalid function 'cramers_v'"),
            ("UG-Distribution-029", "Invalid function 'cramers_v'"),
            ("BG-Distribution-012", "Invalid function 'cv'"),
            ("BG-Distribution-001", "Invalid function 'entropy'"),
            ("BG-Distribution-010", "Invalid function 'entropy'"),
            ("BG-Distribution-013", "Invalid function 'entropy'"),
            ("BG-Distribution-018", "Invalid function 'entropy'"),
            ("BS-Distribution-001", "Invalid function 'entropy'"),
            ("UG-Distribution-015", "Invalid function 'entropy'"),
            ("UG-Distribution-017", "Invalid function 'entropy'"),
            ("UG-Distribution-025", "Invalid function 'entropy'"),
            ("US-Distribution-001", "Invalid function 'entropy'"),
            ("US-Distribution-002", "Invalid function 'entropy'"),
            ("US-Distribution-005", "Invalid function 'entropy'"),
            (
                "BG-Distribution-045",
                "Invalid function 'generalized_jaccard'",
            ),
            (
                "BG-Distribution-046",
                "Invalid function 'generalized_jaccard'",
            ),
            (
                "BG-Distribution-047",
                "Invalid function 'generalized_jaccard'",
            ),
            (
                "BG-Distribution-048",
                "Invalid function 'generalized_jaccard'",
            ),
            ("BG-Distribution-016", "Invalid function 'gini_impurity'"),
            ("UG-Distribution-014", "Invalid function 'gini_impurity'"),
            ("US-Distribution-004", "Invalid function 'gini_impurity'"),
            ("US-Distribution-006", "Invalid function 'gini_impurity'"),
            (
                "BG-Distribution-037",
                "Invalid function 'hellinger_similarity'",
            ),
            (
                "BS-Distribution-008",
                "Invalid function 'hellinger_similarity'",
            ),
            (
                "BS-Distribution-009",
                "Invalid function 'hellinger_similarity'",
            ),
            (
                "BS-Distribution-010",
                "Invalid function 'hellinger_similarity'",
            ),
            (
                "BS-Distribution-011",
                "Invalid function 'hellinger_similarity'",
            ),
            ("BG-Distribution-005", "Invalid function 'iqr'"),
            ("BG-Distribution-019", "Invalid function 'iqr'"),
            ("BG-Distribution-040", "Invalid function 'js_similarity'"),
            ("BG-Distribution-043", "Invalid function 'js_similarity'"),
            ("BS-Distribution-007", "Invalid function 'js_similarity'"),
            ("BS-Distribution-014", "Invalid function 'js_similarity'"),
            (
                "BG-Distribution-034",
                "Invalid function 'js_similarity_binned'",
            ),
            (
                "BS-Distribution-002",
                "Invalid function 'js_similarity_binned'",
            ),
            (
                "BS-Distribution-005",
                "Invalid function 'js_similarity_binned'",
            ),
            ("BG-Distribution-041", "Invalid function 'linf_similarity'"),
            ("BG-Distribution-044", "Invalid function 'linf_similarity'"),
            ("BS-Distribution-006", "Invalid function 'linf_similarity'"),
            ("BG-Distribution-002", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-013", "Invalid function 'mode_frequency'"),
            ("UG-Distribution-026", "Invalid function 'mode_frequency'"),
            ("US-Distribution-003", "Invalid function 'mode_frequency'"),
            (
                "BG-Distribution-021",
                "Invalid function 'mutual_information'",
            ),
            (
                "UG-Distribution-028",
                "Invalid function 'mutual_information'",
            ),
            (
                "UG-Distribution-030",
                "Invalid function 'mutual_information'",
            ),
            ("BG-Distribution-003", "Invalid function 'skewness'"),
            ("BG-Distribution-017", "Invalid function 'skewness'"),
            ("BG-Distribution-022", "Invalid function 'spearman'"),
            ("BG-Distribution-024", "Invalid function 'spearman'"),
            ("UG-Distribution-031", "Invalid function 'spearman'"),
            ("BG-Distribution-011", "Invalid function 'top_k_share'"),
            ("BG-Distribution-038", "Invalid function 'tvd_similarity'"),
            ("BG-Distribution-039", "Invalid function 'tvd_similarity'"),
            ("BG-Distribution-042", "Invalid function 'tvd_similarity'"),
            ("BS-Distribution-012", "Invalid function 'tvd_similarity'"),
            ("BS-Distribution-013", "Invalid function 'tvd_similarity'"),
            ("BS-Distribution-015", "Invalid function 'tvd_similarity'"),
            (
                "BG-Distribution-028",
                "Invalid function 'tvd_similarity_binned'",
            ),
            (
                "BG-Distribution-035",
                "Invalid function 'tvd_similarity_binned'",
            ),
            (
                "BG-Distribution-036",
                "Invalid function 'tvd_similarity_binned'",
            ),
            (
                "BS-Distribution-003",
                "Invalid function 'tvd_similarity_binned'",
            ),
            (
                "BS-Distribution-004",
                "Invalid function 'tvd_similarity_binned'",
            ),
            ("UG-Distribution-004", "Invalid function 'variance_pop'"),
            ("UG-Distribution-007", "Invalid function 'variance_pop'"),
            ("BG-Distribution-030", "Invalid function 'welch_t'"),
            ("BG-Distribution-033", "Invalid function 'welch_t'"),
        ],
    )
    .await;
}
