//! Real-world PromQL conformance over the **awesome-prometheus-alerts** corpus.
//!
//! Source: <https://samber.github.io/awesome-prometheus-alerts/> (MIT) — 949
//! deduplicated alerting `expr` strings spanning Prometheus self-monitoring,
//! host/hardware, node-exporter, databases, message brokers, Kubernetes, and
//! more (`tests/data/awesome_prometheus_alerts.txt`).
//!
//! We *lower* (parse → L2 → L3), we do not execute. Two guarantees:
//!   1. **Totality** — every real-world query returns `Ok` or a clean
//!      `LoweringError` and never panics.
//!   2. **Parseability** — none of them fail at the *parse* stage; the private
//!      `promql-parser` `asap` branch accepts 100% of real-world alert PromQL.
//!
//! Plus targeted tests that pin the *shape* of the patterns we lower, and `__GAP`
//! tests that pin the dominant patterns we cleanly reject today — so adding
//! support later (scalar thresholds, `absent`/`changes`/`delta`/`predict_linear`,
//! `without`) flips the assertion deliberately instead of silently.
//!
//! NOTE: the headline finding is that real alerting PromQL is overwhelmingly
//! `<vector-expr> <cmp> <scalar>` (e.g. `… > 0`, `… != 1`): ~822/949 queries are
//! rejected *only* because a bare numeric threshold operand has no L2 scalar
//! node yet (see `scalar_threshold_*__GAP`). The query bodies underneath
//! (`rate`, `*_over_time`, `histogram_quantile`, `sum by (…)`) lower fine on
//! their own — see the `*_core_lowers` tests.

// `__GAP`-suffixed names intentionally SHOUT the documented divergences.
#![allow(non_snake_case)]

use asap_control_core::intent_algebra::{AggIntent, BinaryOpKind, CompareOp, QueryExpr};
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::{lower_promql, LoweringError};

const CORPUS: &str = include_str!("data/awesome_prometheus_alerts.txt");

/// Non-comment, non-blank query lines.
fn queries() -> impl Iterator<Item = &'static str> {
    CORPUS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// Lower, expecting success.
fn ok(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact)
        .unwrap_or_else(|e| panic!("expected {q:?} to lower, got error: {e}"))
}

/// Lower, expecting a clean (non-panicking) `LoweringError`.
fn rejected(q: &str) -> LoweringError {
    match lower_promql(q, AccuracyTarget::Exact) {
        Err(e) => e,
        Ok(tree) => panic!("expected {q:?} to be rejected, but it lowered to: {tree:?}"),
    }
}

/// Every `AggIntent` in the tree.
fn intents(e: &QueryExpr) -> Vec<AggIntent> {
    let mut out = Vec::new();
    fn go(e: &QueryExpr, out: &mut Vec<AggIntent>) {
        match e {
            QueryExpr::Aggregate { aggs, child, .. } => {
                out.extend(aggs.iter().cloned());
                go(child, out);
            }
            QueryExpr::Window { child, .. }
            | QueryExpr::TimeRange { child, .. }
            | QueryExpr::Filter { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::Subquery { child, .. }
            | QueryExpr::Distinct { child, .. }
            | QueryExpr::WindowFunc { child, .. }
            | QueryExpr::Project { child, .. } => go(child, out),
            QueryExpr::BinaryOp { lhs, rhs, .. }
            | QueryExpr::Join {
                left: lhs,
                right: rhs,
                ..
            }
            | QueryExpr::SetOp {
                left: lhs,
                right: rhs,
                ..
            } => {
                go(lhs, out);
                go(rhs, out);
            }
            QueryExpr::Merge { children } => children.iter().for_each(|c| go(c, out)),
            QueryExpr::LetBinding { expr, child, .. } => {
                go(expr, out);
                go(child, out);
            }
            QueryExpr::Scan { .. } | QueryExpr::Ref { .. } => {}
        }
    }
    go(e, &mut out);
    out
}

fn has<F: Fn(&AggIntent) -> bool>(e: &QueryExpr, p: F) -> bool {
    intents(e).iter().any(p)
}

// ─────────────────────────────────────────────────────────────────────────────
// Corpus-wide invariants
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Default, Debug)]
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

#[test]
fn corpus_lowering_is_total_and_fully_parseable() {
    let mut t = Tally::default();
    for q in queries() {
        // A panic here (not an `Err`) fails the test — that is the totality
        // guarantee over real-world input.
        match lower_promql(q, AccuracyTarget::Exact) {
            Ok(_) => t.lowered += 1,
            Err(LoweringError::Parse(_)) => t.unparseable += 1,
            Err(_) => t.rejected += 1,
        }
    }
    eprintln!("awesome-prometheus-alerts corpus: {t:?}");

    assert!(t.total() >= 900, "corpus unexpectedly small: {t:?}");

    // Parseability: the parser accepts 100% of real-world alert PromQL. A
    // regression that breaks parsing for any real query trips this.
    assert_eq!(
        t.unparseable, 0,
        "{} real-world alert queries failed to PARSE — the parser should accept them all",
        t.unparseable
    );

    // Coverage floor (ratchet, not an exact count): at minimum the
    // vector-vs-vector comparisons lower. Lifting the scalar-threshold or
    // nested-function gaps should raise this deliberately.
    assert!(
        t.lowered >= 12,
        "real-world lowering coverage regressed: {t:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Patterns we lower (verbatim corpus queries) — pin the L3 shape
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn vector_vs_vector_comparison_lowers_to_binaryop() {
    // node-exporter: `node_hwmon_temp_celsius > node_hwmon_temp_max_celsius`.
    // Both operands are instant vectors → a `BinaryOp{Compare}` of two scans.
    let qe = ok("node_hwmon_temp_celsius > node_hwmon_temp_max_celsius");
    let QueryExpr::BinaryOp { op, lhs, rhs, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Compare(CompareOp::Gt));
    assert!(matches!(lhs.as_ref(), QueryExpr::Scan { .. }));
    assert!(matches!(rhs.as_ref(), QueryExpr::Scan { .. }));
}

#[test]
fn kube_replica_mismatch_comparison_lowers() {
    // Kubernetes: `kube_replicaset_spec_replicas != kube_replicaset_status_ready_replicas`.
    let qe = ok("kube_replicaset_spec_replicas != kube_replicaset_status_ready_replicas");
    assert!(matches!(
        &qe,
        QueryExpr::BinaryOp { op, .. } if *op == BinaryOpKind::Compare(CompareOp::Ne)
    ));
}

#[test]
fn histogram_quantile_core_lowers() {
    // GitLab/Sidekiq latency SLOs: `histogram_quantile(0.95, sum(rate(<…>_bucket[5m])) by (le))`
    // (the corpus query is this `> N` thresholded; the body is the canonical
    // Prometheus latency-percentile pattern). It lowers to the full nested shape:
    // Quantile over `sum by (le)` over rate over a TimeRange scan.
    let qe =
        ok("histogram_quantile(0.95, sum(rate(http_request_duration_seconds_bucket[5m])) by (le))");
    assert!(has(
        &qe,
        |i| matches!(i, AggIntent::Quantile { q, .. } if (*q - 0.95).abs() < 1e-9)
    ));
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum { .. })));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate)));
}

#[test]
fn error_ratio_core_lowers() {
    // The classic error-rate ratio (LiteLLM/HTTP 5xx panels), minus the `> 0.05`
    // threshold: `sum(rate(failed[5m])) / sum(rate(total[5m]))` → a `BinaryOp(Div)`
    // of two cross-series sums over per-series rates.
    let qe = ok("sum(rate(litellm_proxy_failed_requests_metric_total[5m])) / sum(rate(litellm_proxy_total_requests_metric_total[5m]))");
    let QueryExpr::BinaryOp { op, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert!(matches!(op, BinaryOpKind::Arith(_)));
    assert_eq!(
        intents(&qe)
            .iter()
            .filter(|i| matches!(i, AggIntent::Sum { .. }))
            .count(),
        2
    );
    assert_eq!(
        intents(&qe)
            .iter()
            .filter(|i| matches!(i, AggIntent::Rate))
            .count(),
        2
    );
}

#[test]
fn all_targets_missing_core_lowers() {
    // Prometheus self-monitoring `sum by (job) (up)` (the corpus query is
    // `… == 0`). Cross-series sum grouped positionally on `job`.
    let qe = ok("sum by (job) (up)");
    let QueryExpr::Aggregate { by, aggs, .. } = &qe else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert_eq!(by, &vec![2], "job grouping → col 2 in [ts, value, job]");
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
}

// ─────────────────────────────────────────────────────────────────────────────
// Dominant gaps in real-world alerting (pinned rejections)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn scalar_threshold_comparisons_are_rejected__GAP() {
    // ~822/949 corpus queries are `<vector> <cmp> <scalar>`. The numeric
    // threshold operand has no L2 scalar node, so the whole alert is rejected
    // (cleanly) — this is the single biggest blocker to lowering real alerts.
    // All three reject via the bare-scalar-operand path (UnsupportedFeature).
    for q in [
        "prometheus_config_last_reload_successful != 1",
        "increase(prometheus_tsdb_compactions_failed_total[1m]) > 0",
        "rate(alertmanager_notifications_failed_total[3m]) > 0.05",
    ] {
        assert!(
            matches!(rejected(q), LoweringError::UnsupportedFeature(_)),
            "expected a clean UnsupportedFeature rejection for {q:?}"
        );
    }
}

#[test]
fn absent_function_is_rejected__GAP() {
    // `absent(up{job="prometheus"})` — "job missing" alerts. `absent` has no
    // intent-algebra representation yet.
    let _ = rejected(r#"absent(up{job="prometheus"})"#);
}

#[test]
fn changes_function_is_rejected__GAP() {
    // `changes(process_start_time_seconds{…}[15m]) > 2` — restart-detection.
    // `changes` is not a sample count, so it is rejected (not aliased to count).
    let _ = rejected(
        r#"changes(process_start_time_seconds{job=~"prometheus|pushgateway|alertmanager"}[15m]) > 2"#,
    );
}

#[test]
fn delta_function_is_rejected__GAP() {
    // `delta(systemd_socket_refused_connections_total[5m]) > 3` — host alerts.
    let _ = rejected("delta(systemd_socket_refused_connections_total[5m]) > 3");
}

#[test]
fn predict_linear_function_is_rejected__GAP() {
    // The canonical "disk will fill in 24h" alert. `predict_linear` has no
    // intent representation yet.
    let _ = rejected(
        r#"predict_linear(node_filesystem_avail_bytes{fstype!~"^(fuse.*|tmpfs|cifs|nfs)"}[3h], 86400) <= 0 and node_filesystem_avail_bytes > 0"#,
    );
}

#[test]
fn vector_literal_is_rejected__GAP() {
    // `vector(1)` — used in dead-man's-switch ("always firing") alerts.
    let _ = rejected("vector(1)");
}

#[test]
fn without_grouping_is_rejected__GAP() {
    // `(min without (cpu) (rate(node_cpu_seconds_total{mode="idle"}[1h]))) > 0.8`
    // — `without(...)` needs the metric's full label set, which the usage-derived
    // schema can't enumerate.
    let _ =
        rejected(r#"(min without (cpu) (rate(node_cpu_seconds_total{mode="idle"}[1h]))) > 0.8"#);
}
