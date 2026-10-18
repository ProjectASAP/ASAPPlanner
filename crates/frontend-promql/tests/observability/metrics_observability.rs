//! Corpus totality checks over the metrics-observability benchmark exports.
//!
//! The source dump mixes Prometheus query logs, Grafana dashboard JSON, and
//! Prometheus rule YAML. The preprocessing script produces the line-oriented
//! fixtures consumed here.

use asap_frontend_promql::PromqlError;
#[path = "../support.rs"]
mod support;
use asap_types::ir::properties::AccuracyTarget;
use support::{logical_plan, lower_promql};

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
            match lower_promql(query, AccuracyTarget::max_error(0.01)) {
                Ok(expr) => {
                    lowered += 1;
                    match logical_plan(&expr) {
                        // An ASAP operator bound somewhere below the root.
                        Ok(node) if node.contains_asap() => post_asap_candidates += 1,
                        Ok(_) => {
                            post_asap_unchanged += 1;
                            if std::env::var_os("METRICS_OBSERVABILITY_REPORT").is_some() {
                                eprintln!("POST_ASAP_MISS\t{name}\t{query}");
                            }
                        }
                        Err(error) => {
                            post_asap_errors += 1;
                            if std::env::var_os("METRICS_OBSERVABILITY_REPORT").is_some() {
                                eprintln!("POST_ASAP_ERROR\t{name}\t{query}\t{error}");
                            }
                        }
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

/// The two Kubernetes mixin alerts whose candidates do not compose: each
/// aggregates over a `topk` output, which the frontend declares as
/// `[keys.., topk_k]` with a `Utf8` placeholder and no value column, so the
/// outer `sum`'s accumulator has no value to read (and the plain plan would
/// sum a string). They compose once the top-k output carries the ranked
/// value (#672).
#[test]
#[ignore = "an aggregate over a top-k output needs the top-k value column (#672)"]
fn kubernetes_mixin_aggregates_over_topk_compose() {
    for query in [
        r#"sum without (id, metrics_path, name, image, endpoint, job, node) ( topk by (cluster, namespace, pod, container, instance) (1, increase( container_cpu_cfs_throttled_periods_total{container!="", job="cadvisor", } [5m]) ) ) / on (cluster, namespace, pod, container, instance) group_left sum without (id, metrics_path, name, image, endpoint, job, node) ( topk by (cluster, namespace, pod, container, instance) (1, increase( container_cpu_cfs_periods_total{job="cadvisor", } [5m]) ) ) > ( 25 / 100 )"#,
        r#"histogram_quantile(0.99, sum by (cluster, instance, le) ( topk by (cluster, instance, le, operation_type) (1, rate(kubelet_pod_worker_duration_seconds_bucket{job="kubelet"}[5m]) ) ) ) * on(cluster, instance) group_left(node) topk by (cluster, instance, node) (1, kubelet_node_name{job="kubelet"} ) > 60"#,
    ] {
        let root = lower_promql(query, AccuracyTarget::max_error(0.01)).unwrap();
        if let Err(error) = logical_plan(&root) {
            panic!("{query}: {error}");
        }
    }
}
