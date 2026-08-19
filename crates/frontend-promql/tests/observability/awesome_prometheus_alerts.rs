//! Real-world PromQL conformance over the **awesome-prometheus-alerts** corpus.
//!
//! Source: <https://samber.github.io/awesome-prometheus-alerts/> (MIT) — 949
//! deduplicated alerting `expr` strings spanning Prometheus self-monitoring,
//! host/hardware, node-exporter, databases, message brokers, Kubernetes, and
//! more (`tests/data/awesome_prometheus_alerts.txt`).
//!
//! We *lower* (parse → the canonical tree), we do not execute. Two guarantees:
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
//! rejected *only* because a bare numeric threshold operand has no canonical
//! scalar-comparison node yet (see `scalar_threshold_*__GAP`). The query bodies underneath
//! (`rate`, `*_over_time`, `histogram_quantile`, `sum by (…)`) lower fine on
//! their own — see the `*_core_lowers` tests.

// `__GAP`-suffixed names intentionally SHOUT the documented divergences.
#![allow(non_snake_case)]

use asap_frontend_promql::{lower_promql, PromqlError as LoweringError};
use asap_types::pre_asap::{AggIntent, BinaryOpKind, CompareOp, QueryExpr, Reduction};
use asap_types::types::AccuracyTarget;

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

/// Every `AggIntent` in the tree.
fn intents(e: &QueryExpr) -> Vec<AggIntent> {
    let mut out = Vec::new();
    fn go(e: &QueryExpr, out: &mut Vec<AggIntent>) {
        match e {
            QueryExpr::Aggregate {
                measures, child, ..
            } => {
                out.extend(measures.iter().cloned());
                go(child, out);
            }
            QueryExpr::TimeRange { child, .. }
            | QueryExpr::TimeShift { child, .. }
            | QueryExpr::Filter { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::Subquery { child, .. }
            | QueryExpr::Distinct { child, .. }
            | QueryExpr::WindowFunc { child, .. }
            | QueryExpr::Project { child, .. }
            | QueryExpr::Relabel { child, .. }
            | QueryExpr::Sample { child, .. }
            | QueryExpr::InfoJoin { child, .. } => go(child, out),
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
            QueryExpr::VectorFromScalar(inner) | QueryExpr::ScalarFromVector(inner) => {
                go(inner, out)
            }
            // `AggIntent` only ever lives in `Aggregate.measures`, never in a
            // scalar position (issue #205) — nothing to collect there.
            QueryExpr::Scan { .. } | QueryExpr::Scalar(_) | QueryExpr::EvalTime => {}
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
            | QueryExpr::Arith { .. }
            | QueryExpr::Case { .. } => {}
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

    // Coverage floor (ratchet, not an exact count). The scalar-threshold operand
    // (#35) unblocked the dominant `<vector> <cmp> <scalar>` shape, taking
    // coverage from ~13 to ~863/949. Remaining gaps are un-implemented functions
    // (histogram_*, absent, changes-derivatives-with-scalar, without(), …).
    assert!(
        t.lowered >= 800,
        "real-world lowering coverage regressed: {t:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Patterns we lower (verbatim corpus queries) — pin the canonical shape
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
    // Prometheus latency-percentile pattern). The `by (le)` grouping marks the
    // classic bucket form, so it lowers to `HistogramQuantile` (bucket
    // interpolation) over `sum by (le)` over rate over a TimeRange scan.
    let qe =
        ok("histogram_quantile(0.95, sum(rate(http_request_duration_seconds_bucket[5m])) by (le))");
    assert!(has(
        &qe,
        |i| matches!(i, AggIntent::HistogramQuantile { q } if (*q - 0.95).abs() < 1e-9)
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
    let QueryExpr::Aggregate {
        reduction,
        measures,
        ..
    } = &qe
    else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert_eq!(
        reduction,
        &Reduction::by(vec![2]),
        "job grouping → col 2 in [ts, value, job]"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
}

// ─────────────────────────────────────────────────────────────────────────────
// Dominant gaps in real-world alerting (pinned rejections)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn scalar_threshold_comparisons_lower_to_binaryop_scalar() {
    // ~822/949 corpus queries are `<vector> <cmp> <scalar>`. The numeric
    // threshold is now a `Scalar` operand of the `BinaryOp` (issue #35) — the
    // single biggest unblock for real alerts.
    for q in [
        "prometheus_config_last_reload_successful != 1",
        "increase(prometheus_tsdb_compactions_failed_total[1m]) > 0",
        "rate(alertmanager_notifications_failed_total[3m]) > 0.05",
    ] {
        let QueryExpr::BinaryOp { rhs, .. } = ok(q) else {
            panic!("expected a BinaryOp for {q:?}");
        };
        assert!(
            matches!(rhs.as_ref(), QueryExpr::Scalar(_)),
            "scalar threshold operand for {q:?}, got {rhs:?}"
        );
    }
}

#[test]
fn absent_function_lowers_to_absent_intent() {
    // `absent(up{job="prometheus"})` — "job missing" alerts. Lowers to the
    // `Absent` intent (issue #47); the empty→synthesized-sample logic is a
    // post-ASAP concern.
    let qe = ok(r#"absent(up{job="prometheus"})"#);
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Absent)));
}

#[test]
fn changes_function_body_lowers_to_changes_intent() {
    // `changes(process_start_time_seconds{…}[15m])` — restart-detection. Lowers
    // to the `Changes` intent (issue #44), NOT aliased to a sample count. The
    // full alert `changes(...) > 2` still needs the scalar-threshold operand
    // (#35) — pinned by `scalar_threshold_comparisons_are_rejected__GAP`.
    let qe = ok(
        r#"changes(process_start_time_seconds{job=~"prometheus|pushgateway|alertmanager"}[15m])"#,
    );
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Changes)));
}

#[test]
fn delta_function_body_lowers_to_delta_intent() {
    // `delta(systemd_socket_refused_connections_total[5m])` — host alerts.
    let qe = ok("delta(systemd_socket_refused_connections_total[5m])");
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Delta)));
}

#[test]
fn predict_linear_function_body_lowers_with_horizon() {
    // The canonical "disk will fill in 24h" alert body. `predict_linear` lowers
    // to `PredictLinear { seconds }` (issue #44); the full `... <= 0 and ...`
    // alert still needs the scalar operand (#35).
    let qe = ok(
        r#"predict_linear(node_filesystem_avail_bytes{fstype!~"^(fuse.*|tmpfs|cifs|nfs)"}[3h], 86400)"#,
    );
    assert!(intents(&qe).iter().any(
        |i| matches!(i, AggIntent::PredictLinear { seconds } if (*seconds - 86400.0).abs() < 1e-9)
    ));
}

#[test]
fn vector_literal_lowers_to_a_labelless_vector() {
    // `vector(1)` — used in dead-man's-switch ("always firing") alerts. Now
    // lowers to a `VectorFromScalar` over the scalar `1` (issue #48).
    let qe = ok("vector(1)");
    let QueryExpr::VectorFromScalar(inner) = &qe else {
        panic!("expected VectorFromScalar, got {qe:?}");
    };
    assert!(matches!(inner.as_ref(), QueryExpr::Scalar(v) if *v == 1.0));
    // The result is a vector: it carries a time index (unlike a bare scalar).
    assert!(qe.output_schema().unwrap().time_index.is_some());
}

#[test]
fn without_grouping_lowers_to_the_exclusion_form() {
    // `(min without (cpu) (rate(node_cpu_seconds_total{mode="idle"}[1h]))) > 0.8`
    // — `without(...)` groups by every label except the listed ones. The
    // complement isn't enumerated (open usage-derived schema); the excluded
    // labels are stored and the kept set is runtime-resolved (issue #39).
    let qe = ok(r#"(min without (cpu) (rate(node_cpu_seconds_total{mode="idle"}[1h]))) > 0.8"#);
    // Top level is the `> 0.8` comparison; the `min without (cpu)` is its LHS.
    let QueryExpr::BinaryOp { lhs, .. } = &qe else {
        panic!("expected a comparison BinaryOp, got {qe:?}");
    };
    let QueryExpr::Aggregate {
        reduction,
        measures,
        ..
    } = lhs.as_ref()
    else {
        panic!("expected a `min without` Aggregate on the LHS, got {lhs:?}");
    };
    assert!(
        reduction.expect_reduce().is_without(),
        "grouping is the exclusion form"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Min { .. }]));
}
