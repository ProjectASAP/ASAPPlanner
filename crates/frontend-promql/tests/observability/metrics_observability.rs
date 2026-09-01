//! Corpus totality checks over the metrics-observability benchmark exports.
//!
//! The source dump mixes Prometheus query logs, Grafana dashboard JSON, and
//! Prometheus rule YAML. The preprocessing script produces the line-oriented
//! fixtures consumed here.

use std::rc::Rc;

use asap_aware_mapping::replacement::{keep_pre_asap, ImplementError};
use asap_aware_mapping::{
    Replacement, ReplacementStrategy, ReplacementSubDAG, SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_frontend_promql::{lower_promql, PromqlError};
use asap_types::post_asap::{SummaryExpr, SummaryNode};
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::types::AccuracyTarget;

const CORPORA: &[(&str, &str)] = &[
    (
        "CMU query logs",
        include_str!("data/metrics_observability/cmu_chad_logs.txt"),
    ),
    (
        "CMU dashboards",
        include_str!("data/metrics_observability/cmu_chad_dashboards.txt"),
    ),
    (
        "CMU rules",
        include_str!("data/metrics_observability/cmu_chad_rules.txt"),
    ),
    (
        "Claude query logs",
        include_str!("data/metrics_observability/from_claude_logs.txt"),
    ),
    (
        "Grafana dashboards",
        include_str!("data/metrics_observability/grafana_dashboards.txt"),
    ),
    (
        "Kubernetes mixin dashboards",
        include_str!("data/metrics_observability/kubernetes_mixin_dashboards.txt"),
    ),
    (
        "Kubernetes mixin alerts",
        include_str!("data/metrics_observability/kubernetes_mixin_alerts.txt"),
    ),
    (
        "Kubernetes mixin rules",
        include_str!("data/metrics_observability/kubernetes_mixin_rules.txt"),
    ),
    (
        "Promset",
        include_str!("data/metrics_observability/promset.txt"),
    ),
];

fn queries(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

fn post_asap_candidate(expr: &QueryExpr) -> Result<Rc<SummaryNode>, ImplementError> {
    let root = Rc::new(expr.clone());
    let target = TargetSubDAG::new(&root);
    match SketchAlgorithmStrategy::default_cost_model()
        .replacements(&target)
        .into_iter()
        .next()
    {
        Some(ReplacementSubDAG {
            replacement: Replacement::Summary(node),
            ..
        }) => Ok(node),
        _ => keep_pre_asap(&root),
    }
}

#[test]
fn benchmark_corpora_are_total_and_report_coverage() {
    for (name, corpus) in CORPORA {
        let mut total = 0;
        let mut lowered = 0;
        let mut parse_failures = 0;
        let mut rejected = 0;
        let mut post_asap_candidates = 0;
        let mut post_asap_unchanged = 0;
        let mut post_asap_errors = 0;

        for query in queries(corpus) {
            total += 1;
            // Approximate accuracy exercises the sketch-replacement boundary
            // used by the existing post-ASAP corpus binding test.
            match lower_promql(query, AccuracyTarget::Epsilon(0.01)) {
                Ok(expr) => {
                    lowered += 1;
                    match post_asap_candidate(&expr) {
                        Ok(node) if !matches!(node.expr, SummaryExpr::KeepPreAsap(_)) => {
                            post_asap_candidates += 1
                        }
                        Ok(_) => {
                            post_asap_unchanged += 1;
                            if std::env::var_os("METRICS_OBSERVABILITY_REPORT").is_some() {
                                eprintln!("POST_ASAP_MISS\t{name}\t{query}");
                            }
                        }
                        Err(_) => post_asap_errors += 1,
                    }
                }
                Err(PromqlError::Parse(_)) => parse_failures += 1,
                Err(_) => rejected += 1,
            }
        }

        eprintln!("{name}: total={total}, parse_errors={parse_failures}, lowering_errors={rejected}, pre_asap={lowered}, post_asap_candidates={post_asap_candidates}, post_asap_unchanged={post_asap_unchanged}, post_asap_errors={post_asap_errors}");
        assert!(total > 0, "{name} fixture is empty");
        // Dashboard variables and panel expressions can contain Grafana
        // template syntax rather than standalone PromQL. They are still
        // counted above so extraction drift is visible, but are not expected
        // to parse as PromQL.
        assert_eq!(total, lowered + rejected + parse_failures);
    }
}
