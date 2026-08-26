// cargo run -p asap-devtools --bin sketch_coverage [-- --epsilon <f64>]
//
// Lowers every query in every corpus we have (mirrors `variant_coverage`'s
// corpus list exactly, so the two reports are directly comparable) with an
// *approximate* `AccuracyTarget`, runs `asap_aware_mapping::explain_replacements`
// over each corpus as one workload, and reports the MVP demo's query-coverage
// metric: of the queries that lowered successfully, what fraction got
//
//   - a `SketchApproximation` candidate (a genuine sketch alternative was
//     found for at least one aggregate in the query — the KLL-vs-DDSketch
//     kind of degree of freedom), and/or
//   - a `CommonSubexpressionReuse` candidate (the query shares a subtree,
//     inside itself or with another query in the same corpus, that a
//     build-once-and-share candidate was found for).
//
// `--epsilon <f64>` (default 0.01) sets the `AccuracyTarget` every query in
// every corpus lowers with. Without an approximate target,
// `SketchAlgorithmStrategy` never has a genuine sketch alternative to
// report — see `dag_export`'s own `--epsilon` doc comment for the same
// point, made there per-query instead of per-run.
//
// This is a workload-level count (`explain_replacements` runs
// `search_workload` once per corpus, over every query in it together), not
// just a per-query re-run of the single-target path — so cross-query CSE
// reuse inside one corpus shows up here the same way it would in the
// dag-viewer's Union mode.

use asap_aware_mapping::{explain_replacements, ExplanationKind};
use asap_devtools::lower_promql;
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::QueryExpr;
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;
use std::collections::BTreeSet;

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

struct CorpusCoverage {
    name: &'static str,
    lowered: usize,
    failed: usize,
    sketch_covered: usize,
    cse_covered: usize,
    either_covered: usize,
}

fn pct(n: usize, total: usize) -> String {
    if total == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", 100.0 * n as f64 / total as f64)
    }
}

/// `explain_replacements`' `location` is a comma-joined list of breadcrumbs
/// (`collect_locations` in `explanation.rs`), one per path from a workload
/// root to the target — e.g. `root "q3"` or `root "q3" > lhs`. A location
/// "covers" `label` (a bare `root "qN"` breadcrumb) if `label` is exactly one
/// of those comma-separated entries or the prefix of one that goes deeper —
/// i.e. the explanation's target is reachable from that query's root at all.
fn covers(location: &str, label: &str) -> bool {
    location
        .split(", ")
        .any(|loc| loc == label || loc.starts_with(&format!("{label} > ")))
}

fn root_label(id: &str) -> String {
    format!("root {id:?}")
}

/// Run `explain_replacements` over one corpus's already-lowered roots as one
/// workload, then attribute each finding back to the query root(s) it's
/// reachable from.
fn analyze_corpus(
    name: &'static str,
    roots: Vec<(String, QueryExpr)>,
    failed: usize,
) -> CorpusCoverage {
    let lowered = roots.len();
    let labels: Vec<String> = roots.iter().map(|(id, _)| root_label(id)).collect();
    let explanations = explain_replacements(roots);

    let mut sketch_covered: BTreeSet<usize> = BTreeSet::new();
    let mut cse_covered: BTreeSet<usize> = BTreeSet::new();
    for explanation in &explanations {
        for (i, label) in labels.iter().enumerate() {
            if !covers(&explanation.location, label) {
                continue;
            }
            match explanation.kind {
                ExplanationKind::SketchApproximation => {
                    sketch_covered.insert(i);
                }
                ExplanationKind::CommonSubexpressionReuse => {
                    cse_covered.insert(i);
                }
                // `#[non_exhaustive]`: a future kind just doesn't count
                // toward either bucket here until this tool is taught about it.
                _ => {}
            }
        }
    }
    let either_covered = sketch_covered.union(&cse_covered).count();

    CorpusCoverage {
        name,
        lowered,
        failed,
        sketch_covered: sketch_covered.len(),
        cse_covered: cse_covered.len(),
        either_covered,
    }
}

fn report(r: &CorpusCoverage) {
    println!("--- {} ---", r.name);
    println!("lowered: {}, failed: {}", r.lowered, r.failed);
    println!(
        "sketch-approximable: {}/{} ({})",
        r.sketch_covered,
        r.lowered,
        pct(r.sketch_covered, r.lowered)
    );
    println!(
        "CSE-shareable:       {}/{} ({})",
        r.cse_covered,
        r.lowered,
        pct(r.cse_covered, r.lowered)
    );
    println!(
        "either (coverage):   {}/{} ({})",
        r.either_covered,
        r.lowered,
        pct(r.either_covered, r.lowered)
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

#[tokio::main]
async fn main() {
    let epsilon = parse_epsilon();
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
            match lower_promql(q, accuracy.clone()) {
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
    let total_sketch: usize = results.iter().map(|r| r.sketch_covered).sum();
    let total_cse: usize = results.iter().map(|r| r.cse_covered).sum();
    let total_either: usize = results.iter().map(|r| r.either_covered).sum();

    println!("=== global (epsilon = {epsilon}) ===");
    println!("total lowered: {total_lowered}, total failed: {total_failed}");
    println!(
        "sketch-approximable: {total_sketch}/{total_lowered} ({})",
        pct(total_sketch, total_lowered)
    );
    println!(
        "CSE-shareable:       {total_cse}/{total_lowered} ({})",
        pct(total_cse, total_lowered)
    );
    println!(
        "either (coverage):   {total_either}/{total_lowered} ({})",
        pct(total_either, total_lowered)
    );
}
