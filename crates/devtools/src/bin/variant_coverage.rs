// cargo run -p asap-lower --bin variant_coverage
//
// Lowers every query in every corpus we have (PromQL + SQL), walks the
// resulting QueryExpr trees, and reports which enum variants show up — per
// corpus, then rolled up globally. Used to find the minimal QueryExpr node set.

use asap_devtools::lower_promql;
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::QueryExpr;
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;
use std::collections::BTreeSet;

const ALL_VARIANTS: &[&str] = &[
    "Scan",
    "PromqlScalarBridge",
    "EvalTimestamp",
    "CurrentTimestamp",
    "PromqlVectorFromScalar",
    "PromqlScalarFromVector",
    "PromqlRelabel",
    "PromqlInfoEnrich",
    "PromqlSeriesSample",
    "Filter",
    "Project",
    "Aggregate",
    "Dedup",
    "Concat",
    "Join",
    "SetOp",
    "Sort",
    "Limit",
    "PromqlSubquery",
    "TimeRange",
    "TimeShift",
    "SQLWindowFunc",
    "BinaryOp",
];

fn walk(e: &QueryExpr, seen: &mut BTreeSet<&'static str>) {
    match e {
        QueryExpr::Scan { .. } => {
            seen.insert("Scan");
        }
        QueryExpr::PromqlScalarBridge(_) => {
            seen.insert("PromqlScalarBridge");
        }
        QueryExpr::EvalTimestamp => {
            seen.insert("EvalTimestamp");
        }
        QueryExpr::CurrentTimestamp => {
            seen.insert("CurrentTimestamp");
        }
        QueryExpr::PromqlVectorFromScalar(inner) => {
            seen.insert("PromqlVectorFromScalar");
            walk(inner, seen);
        }
        QueryExpr::PromqlScalarFromVector(inner) => {
            seen.insert("PromqlScalarFromVector");
            walk(inner, seen);
        }
        QueryExpr::PromqlRelabel { child, .. } => {
            seen.insert("PromqlRelabel");
            walk(child, seen);
        }
        QueryExpr::PromqlInfoEnrich { child, .. } => {
            seen.insert("PromqlInfoEnrich");
            walk(child, seen);
        }
        QueryExpr::PromqlSeriesSample { child, .. } => {
            seen.insert("PromqlSeriesSample");
            walk(child, seen);
        }
        QueryExpr::Filter { child, .. } => {
            seen.insert("Filter");
            walk(child, seen);
        }
        QueryExpr::Project { child, .. } => {
            seen.insert("Project");
            walk(child, seen);
        }
        QueryExpr::Aggregate { child, .. } => {
            seen.insert("Aggregate");
            walk(child, seen);
        }
        QueryExpr::Dedup { child, .. } => {
            seen.insert("Dedup");
            walk(child, seen);
        }
        QueryExpr::Concat { children } => {
            seen.insert("Concat");
            children.iter().for_each(|c| walk(c, seen));
        }
        QueryExpr::Join { left, right, .. } => {
            seen.insert("Join");
            walk(left, seen);
            walk(right, seen);
        }
        QueryExpr::SetOp { left, right, .. } => {
            seen.insert("SetOp");
            walk(left, seen);
            walk(right, seen);
        }
        QueryExpr::Sort { child, .. } => {
            seen.insert("Sort");
            walk(child, seen);
        }
        QueryExpr::Limit { child, .. } => {
            seen.insert("Limit");
            walk(child, seen);
        }
        QueryExpr::PromqlSubquery { child, .. } => {
            seen.insert("PromqlSubquery");
            walk(child, seen);
        }
        QueryExpr::TimeRange { child, .. } => {
            seen.insert("TimeRange");
            walk(child, seen);
        }
        QueryExpr::TimeShift { child, .. } => {
            seen.insert("TimeShift");
            walk(child, seen);
        }
        QueryExpr::SQLWindowFunc { child, .. } => {
            seen.insert("SQLWindowFunc");
            walk(child, seen);
        }
        QueryExpr::BinaryOp { lhs, rhs, .. } => {
            seen.insert("BinaryOp");
            walk(lhs, seen);
            walk(rhs, seen);
        }
        // Scalar expression variants (issue #205) aren't relational nodes;
        // this walk only reports on the relational skeleton, so stop here.
        QueryExpr::Column(_)
        | QueryExpr::Literal(_)
        | QueryExpr::Compare { .. }
        | QueryExpr::BoolAnd(_)
        | QueryExpr::BoolOr(_)
        | QueryExpr::Not(_)
        | QueryExpr::IsNull(_)
        | QueryExpr::IsNotNull(_)
        | QueryExpr::Cast { .. }
        | QueryExpr::InList { .. }
        | QueryExpr::FunctionCall { .. }
        | QueryExpr::Arithmetic { .. }
        | QueryExpr::Case { .. } => {}
    }
}

/// Line-based `#`/`--` comment stripping, then split on `;` — the shape every
/// SQL corpus test in this repo already uses.
fn sql_stmts(corpus: &str) -> Vec<String> {
    let sql: String = corpus
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

fn promql_lines(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

fn dqc_catalog() -> SqlCatalog {
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

fn netflow_catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "netflow_table",
        Schema::with_time_index(
            vec![
                col("time", DataType::Timestamp),
                col("srcip", DataType::Utf8),
                col("dstip", DataType::Utf8),
                col("srcport", DataType::Int64),
                col("dstport", DataType::Int64),
                col("proto", DataType::Utf8),
                col("pkt_len", DataType::Int64),
            ],
            0,
            vec![],
        ),
    )
}

fn bgp_catalog() -> SqlCatalog {
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

struct CorpusResult {
    name: &'static str,
    lowered: usize,
    failed: usize,
    variants: BTreeSet<&'static str>,
}

fn report(r: &CorpusResult) {
    println!("--- {} ---", r.name);
    println!("lowered: {}, failed: {}", r.lowered, r.failed);
    println!("variants ({}): {:?}", r.variants.len(), r.variants);
    println!();
}

#[tokio::main]
async fn main() {
    let mut results = Vec::new();

    // ── PromQL corpora ──
    let promql_corpora: &[(&str, &str)] = &[
        (
            "promql/docs",
            include_str!(
                "../../../frontend-promql/tests/observability/data/promql_corpus_docs.txt"
            ),
        ),
        (
            "promql/testdata",
            include_str!(
                "../../../frontend-promql/tests/observability/data/promql_corpus_testdata.txt"
            ),
        ),
        (
            "promql/o11y_bench",
            include_str!("../../../frontend-promql/tests/observability/data/o11y_bench_promql.txt"),
        ),
        (
            "promql/awesome_alerts",
            include_str!(
                "../../../frontend-promql/tests/observability/data/awesome_prometheus_alerts.txt"
            ),
        ),
    ];
    for (name, corpus) in promql_corpora {
        let mut variants = BTreeSet::new();
        let mut lowered = 0;
        let mut failed = 0;
        for q in promql_lines(corpus) {
            match lower_promql(q, AccuracyTarget::Exact) {
                Ok(qe) => {
                    walk(&qe, &mut variants);
                    lowered += 1;
                }
                Err(_) => failed += 1,
            }
        }
        results.push(CorpusResult {
            name,
            lowered,
            failed,
            variants,
        });
    }

    // ── SQL corpora ──
    #[allow(clippy::type_complexity)]
    let sql_corpora: &[(&str, &str, fn() -> SqlCatalog)] = &[
        ("sql/dqc_packet_trace", include_str!("../../../frontend-sql/tests/data_quality_check/data/synthetic_packet_trace_queries.sql"), dqc_catalog),
        ("sql/netflow", include_str!("../../../frontend-sql/tests/netflow/data/netflow.sql"), netflow_catalog),
    ];
    for (name, corpus, catalog_fn) in sql_corpora {
        let catalog = catalog_fn();
        let mut variants = BTreeSet::new();
        let mut lowered = 0;
        let mut failed = 0;
        for q in sql_stmts(corpus) {
            match lower_sql_dialect(
                &q,
                &catalog,
                SqlDialect::DataFusionSQL,
                AccuracyTarget::Exact,
            )
            .await
            {
                Ok(qe) => {
                    walk(&qe, &mut variants);
                    lowered += 1;
                }
                Err(_) => failed += 1,
            }
        }
        results.push(CorpusResult {
            name,
            lowered,
            failed,
            variants,
        });
    }

    // bgp_analytics: ClickHouse dialect, mostly-rejected corpus (documented in
    // its own test) — still worth walking whatever *does* lower.
    {
        let corpus =
            include_str!("../../../frontend-sql/tests/bgp_analytics/data/bgp_analytics.sql");
        let catalog = bgp_catalog();
        let mut variants = BTreeSet::new();
        let mut lowered = 0;
        let mut failed = 0;
        for q in sql_stmts(corpus) {
            match lower_sql_dialect(
                &q,
                &catalog,
                SqlDialect::ClickhouseSQL,
                AccuracyTarget::Exact,
            )
            .await
            {
                Ok(qe) => {
                    walk(&qe, &mut variants);
                    lowered += 1;
                }
                Err(_) => failed += 1,
            }
        }
        results.push(CorpusResult {
            name: "sql/bgp_analytics",
            lowered,
            failed,
            variants,
        });
    }

    for r in &results {
        report(r);
    }

    let mut global: BTreeSet<&'static str> = BTreeSet::new();
    let mut total_lowered = 0;
    let mut total_failed = 0;
    for r in &results {
        global.extend(r.variants.iter().copied());
        total_lowered += r.lowered;
        total_failed += r.failed;
    }

    println!("=== global ===");
    println!("total lowered: {total_lowered}, total failed: {total_failed}\n");
    println!("used variants ({}):", global.len());
    for v in &global {
        println!("  {v}");
    }
    println!("\nunused variants ({}):", ALL_VARIANTS.len() - global.len());
    for v in ALL_VARIANTS {
        if !global.contains(v) {
            println!("  {v}");
        }
    }
}
