// cargo run -p asap-devtools --bin sketch_coverage [-- --epsilon <f64>]
//
// Lowers every query in every corpus we have (mirrors `variant_coverage`'s
// corpus list exactly, so the two reports are directly comparable) with an
// *approximate* `AccuracyTarget`, runs Stage 1's Pass 1
// (`enumerate_local_logical_candidates`) over each query, and reports which
// sketch alternatives Pass 1 offers for it, plus the fraction of lowered
// queries with at least one.
//
// `--epsilon <f64>` (default 0.01) sets the `AccuracyTarget` every query in
// every corpus lowers with. Without an approximate target, Pass 1 offers no
// sketch alternative.

use asap_devtools::lower_promql_with_data_ingestion_interval;
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates;
use asap_logical_optimizer::Realization;
use asap_types::ir::schema::{DataType, Field, GroupingStrategy, Schema};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Line-based `#`/`--` comment stripping, then split on `;` — the shape every
/// SQL corpus test in this repo already uses (copied from `variant_coverage`
/// so both tools walk the exact same corpus text).
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

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
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

struct CorpusCoverage {
    name: &'static str,
    lowered: usize,
    failed: usize,
    /// Lowered queries Pass 1 rejected.
    rejected: usize,
    /// Per lowered query Pass 1 accepted, the sketch alternatives it offers.
    sketches: Vec<(String, BTreeSet<String>)>,
}

fn pct(n: usize, total: usize) -> String {
    if total == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", 100.0 * n as f64 / total as f64)
    }
}

/// Run Pass 1 over each of one corpus's already-lowered queries and collect
/// the sketch alternatives it offers for any of the query's targets. A Hydra
/// alternative is listed as `Hydra(<algorithm>)`.
fn analyze_corpus(
    name: &'static str,
    roots: Vec<(String, Rc<OperatorNode>)>,
    failed: usize,
) -> CorpusCoverage {
    let lowered = roots.len();
    let mut rejected = 0;
    let mut sketches = Vec::new();
    for (id, root) in roots {
        let Ok(inventory) = enumerate_local_logical_candidates(
            vec![(id.clone(), QueryRoot::Operator(root))],
            &BTreeMap::new(),
        ) else {
            rejected += 1;
            continue;
        };
        let offered = inventory
            .targets
            .iter()
            .flat_map(|target| target.alternatives.iter().zip(&target.groupings))
            .filter_map(|(alternative, grouping)| match alternative {
                Realization::Sketch(kind) if *grouping == GroupingStrategy::default() => {
                    Some(format!("{:?}", kind.algorithm()))
                }
                Realization::Sketch(kind) => Some(format!("Hydra({:?})", kind.algorithm())),
                _ => None,
            })
            .collect();
        sketches.push((id, offered));
    }
    CorpusCoverage {
        name,
        lowered,
        failed,
        rejected,
        sketches,
    }
}

fn covered(r: &CorpusCoverage) -> usize {
    r.sketches.iter().filter(|(_, s)| !s.is_empty()).count()
}

fn report(r: &CorpusCoverage) {
    println!("--- {} ---", r.name);
    println!(
        "lowered: {}, failed: {}, rejected by Pass 1: {}",
        r.lowered, r.failed, r.rejected
    );
    for (id, offered) in &r.sketches {
        if !offered.is_empty() {
            let offered: Vec<_> = offered.iter().map(String::as_str).collect();
            println!("  {id}: {}", offered.join(", "));
        }
    }
    println!(
        "sketch-approximable: {}/{} ({})",
        covered(r),
        r.lowered,
        pct(covered(r), r.lowered)
    );
    println!();
}

fn parse_epsilon() -> f64 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--epsilon" {
            let raw = args.next().expect("--epsilon requires a value");
            return raw
                .parse()
                .unwrap_or_else(|_| panic!("--epsilon must be a float, got {raw:?}"));
        }
    }
    0.01
}

fn parse_ingestion_interval() -> u64 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data-ingestion-interval-ms" {
            return args
                .next()
                .expect("--data-ingestion-interval-ms requires a value")
                .parse()
                .expect("--data-ingestion-interval-ms must be an unsigned integer");
        }
    }
    panic!("--data-ingestion-interval-ms is required")
}

#[tokio::main]
async fn main() {
    let epsilon = parse_epsilon();
    let interval_ms = parse_ingestion_interval();
    let accuracy = AccuracyTarget::Epsilon(epsilon);
    let mut results = Vec::new();

    // ── PromQL corpora — same set as `variant_coverage` ──
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
        let mut roots = Vec::new();
        let mut failed = 0;
        for (i, q) in promql_lines(corpus).enumerate() {
            match lower_promql_with_data_ingestion_interval(q, accuracy.clone(), interval_ms) {
                Ok(qe) => roots.push((format!("q{i}"), qe)),
                Err(_) => failed += 1,
            }
        }
        results.push(analyze_corpus(name, roots, failed));
    }

    // ── SQL corpora ──
    #[allow(clippy::type_complexity)]
    let sql_corpora: &[(&str, &str, fn() -> SqlCatalog, SqlDialect)] = &[
        (
            "sql/dqc_packet_trace",
            include_str!("../../../frontend-sql/tests/data_quality_check/data/synthetic_packet_trace_queries.sql"),
            dqc_catalog,
            SqlDialect::DataFusionSQL,
        ),
        (
            "sql/netflow",
            include_str!("../../../frontend-sql/tests/netflow/data/netflow.sql"),
            netflow_catalog,
            SqlDialect::DataFusionSQL,
        ),
        (
            "sql/bgp_analytics",
            include_str!("../../../frontend-sql/tests/bgp_analytics/data/bgp_analytics.sql"),
            bgp_catalog,
            SqlDialect::ClickhouseSQL,
        ),
    ];
    for (name, corpus, catalog_fn, dialect) in sql_corpora {
        let catalog = catalog_fn();
        let mut roots = Vec::new();
        let mut failed = 0;
        for (i, q) in sql_stmts(corpus).into_iter().enumerate() {
            match lower_sql_dialect(&q, &catalog, dialect.clone(), accuracy.clone()).await {
                Ok(qe) => roots.push((format!("q{i}"), qe)),
                Err(_) => failed += 1,
            }
        }
        results.push(analyze_corpus(name, roots, failed));
    }

    for r in &results {
        report(r);
    }

    let total_lowered: usize = results.iter().map(|r| r.lowered).sum();
    let total_failed: usize = results.iter().map(|r| r.failed).sum();
    let total_rejected: usize = results.iter().map(|r| r.rejected).sum();
    let total_sketch: usize = results.iter().map(covered).sum();

    println!("=== global (epsilon = {epsilon}) ===");
    println!(
        "total lowered: {total_lowered}, total failed: {total_failed}, \
         rejected by Pass 1: {total_rejected}"
    );
    println!(
        "sketch-approximable: {total_sketch}/{total_lowered} ({})",
        pct(total_sketch, total_lowered)
    );
}
