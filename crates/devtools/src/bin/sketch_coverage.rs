// cargo run -p asap-devtools --bin sketch_coverage -- --sample-interval-ms <ms> [--max-error <f64>]
//
// Lowers every query in every corpus we have (mirrors `variant_coverage`'s
// corpus list exactly, so the two reports are directly comparable) with an
// *approximate* `AccuracyTarget`, runs Pass 1 of logical ASAP-aware
// optimization (`generate_per_computation_logical_memo_candidates`, every
// summary supported, no accuracy pruning) over each query, and
// reports which summary candidates Pass 1 offers for it, plus the fraction of
// lowered queries with at least one.
//
// `--sample-interval-ms <ms>` is the sample interval declared for every
// metric a query reads, which its bare instant selectors look back.
// `--max-error <f64>` (ε, default 0.01) sets the `AccuracyTarget` every query in
// every corpus lowers with. Without an approximate target, Pass 1 offers no
// summary candidate.

use asap_accuracy::{IgnoreAccuracyGuarantees, NoAccuracyEvidence};
use asap_devtools::corpora::{
    bgp_catalog, dqc_catalog, netflow_catalog, parse_sample_interval, promql_lines, sql_statements,
};
use asap_devtools::{lower_promql_with_sample_interval, SqlCatalog};
use asap_frontend_sql::lower_sql_dialect;
use asap_logical_asap_optimization::generate_dag_of_logical_memo_groups;
use asap_logical_asap_optimization::generate_per_computation_logical_memo_candidates;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::HowToComputeAggregateMeasure;
use asap_types::workload::{DataWorkload, SqlDialect};
use std::collections::BTreeSet;
use std::rc::Rc;

struct CorpusCoverage {
    name: &'static str,
    lowered: usize,
    failed: usize,
    /// Lowered queries Pass 1 rejected.
    rejected: usize,
    /// Per lowered query Pass 1 accepted, the summary candidates it offers.
    summaries: Vec<(String, BTreeSet<String>)>,
}

fn pct(n: usize, total: usize) -> String {
    if total == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", 100.0 * n as f64 / total as f64)
    }
}

/// Run Pass 1 over each of one corpus's already-lowered queries (each
/// lowered with `accuracy`) and collect the summary candidates it offers
/// for any of the query's targets.
fn analyze_corpus(
    name: &'static str,
    roots: Vec<(String, Rc<OperatorNode>)>,
    failed: usize,
    accuracy: &AccuracyTarget,
) -> CorpusCoverage {
    let lowered = roots.len();
    let mut rejected = 0;
    let mut summaries = Vec::new();
    for (id, root) in roots {
        let Ok(pass1) = generate_per_computation_logical_memo_candidates(
            generate_dag_of_logical_memo_groups(&[QueryRoot::Operator(root)]),
            std::slice::from_ref(accuracy),
            &IgnoreAccuracyGuarantees,
            &NoAccuracyEvidence,
            &DataWorkload::default(),
            None,
            None,
        ) else {
            rejected += 1;
            continue;
        };
        let offered = pass1
            .computation_targets
            .iter()
            .flat_map(|target| &target.ways_to_compute)
            .filter_map(|candidate| match candidate {
                HowToComputeAggregateMeasure::Summary(algorithm) => Some(format!("{algorithm:?}")),
                _ => None,
            })
            .collect();
        summaries.push((id, offered));
    }
    CorpusCoverage {
        name,
        lowered,
        failed,
        rejected,
        summaries,
    }
}

fn covered(r: &CorpusCoverage) -> usize {
    r.summaries.iter().filter(|(_, s)| !s.is_empty()).count()
}

fn report(r: &CorpusCoverage) {
    println!("--- {} ---", r.name);
    println!(
        "lowered: {}, failed: {}, rejected by Pass 1: {}",
        r.lowered, r.failed, r.rejected
    );
    for (id, offered) in &r.summaries {
        if !offered.is_empty() {
            let offered: Vec<_> = offered.iter().map(String::as_str).collect();
            println!("  {id}: {}", offered.join(", "));
        }
    }
    println!(
        "summary-approximable: {}/{} ({})",
        covered(r),
        r.lowered,
        pct(covered(r), r.lowered)
    );
    println!();
}

fn parse_max_error() -> f64 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--max-error" {
            let raw = args.next().expect("--max-error requires a value");
            return raw
                .parse()
                .unwrap_or_else(|_| panic!("--max-error must be a float, got {raw:?}"));
        }
    }
    0.01
}

#[tokio::main]
async fn main() {
    let max_error = parse_max_error();
    let interval_ms = parse_sample_interval();
    let accuracy = AccuracyTarget::max_error(max_error);
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
            match lower_promql_with_sample_interval(q, accuracy.clone(), interval_ms) {
                Ok(qe) => roots.push((format!("q{i}"), qe)),
                Err(_) => failed += 1,
            }
        }
        results.push(analyze_corpus(name, roots, failed, &accuracy));
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
        for (i, q) in sql_statements(corpus).into_iter().enumerate() {
            match lower_sql_dialect(&q, &catalog, dialect.clone(), accuracy.clone()).await {
                Ok(qe) => roots.push((format!("q{i}"), qe)),
                Err(_) => failed += 1,
            }
        }
        results.push(analyze_corpus(name, roots, failed, &accuracy));
    }

    for r in &results {
        report(r);
    }

    let total_lowered: usize = results.iter().map(|r| r.lowered).sum();
    let total_failed: usize = results.iter().map(|r| r.failed).sum();
    let total_rejected: usize = results.iter().map(|r| r.rejected).sum();
    let total_covered: usize = results.iter().map(covered).sum();

    println!("=== global (max_error = {max_error}) ===");
    println!(
        "total lowered: {total_lowered}, total failed: {total_failed}, rejected by Pass 1: {total_rejected}"
    );
    println!(
        "summary-approximable: {total_covered}/{total_lowered} ({})",
        pct(total_covered, total_lowered)
    );
}
