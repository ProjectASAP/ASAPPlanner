//! PromQL **semantic conformance** for the L1→L3 lowering.
//!
//! We *lower* PromQL to the intent algebra; we do not *execute* it. So "same
//! semantic job as Prometheus" here means: for each canonical query, does the
//! L3 tree encode the **documented PromQL meaning** — and where we knowingly
//! diverge (reject, approximate, or drop a modifier), is that pinned by a test
//! so it stays visible?
//!
//! Sources for the queries + their semantics:
//! - PromQL basics (data types, selectors, offset/@/subquery):
//!   <https://prometheus.io/docs/prometheus/latest/querying/basics/>
//! - PromLabs PromQL cheat sheet (common real-world queries by category):
//!   <https://promlabs.com/promql-cheat-sheet/>
//! - Prometheus' own engine test corpus (these are *execution* tests —
//!   load → eval → expect values — so they define semantics we mirror as
//!   *structure*): <https://github.com/prometheus/prometheus/tree/main/promql/promqltest/testdata>
//!   Relevant files, mapped to the sections below: selectors.test,
//!   aggregators.test, functions.test, histograms.test, operators.test,
//!   subquery.test, at_modifier.test, literals.test, limit.test
//!
//! Legend used in test names:
//! - (no suffix)  — we lower it and the L3 intent matches PromQL.
//! - `__rejected` — we cleanly return an error (capability not implemented).
//! - `__GAP`      — we lower it but the result **diverges** from PromQL
//!   semantics (silent loss or over-aggregation). Pinned so a future fix
//!   flips the assertion deliberately.

// `__GAP`-suffixed test names intentionally SHOUT the documented divergences.
#![allow(non_snake_case)]

use std::time::Duration;

use asap_control_core::intent_algebra::{
    AggIntent, BinaryOpKind, PartitionKeys, QueryExpr, Source,
};
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::{lower_promql, LoweringError};

// ── harness helpers ─────────────────────────────────────────────────────────────

/// Lower, expecting success.
fn ok(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact)
        .unwrap_or_else(|e| panic!("expected {q:?} to lower, got error: {e}"))
}

/// Lower, expecting a clean `LoweringError` (an unsupported capability).
fn rejected(q: &str) -> LoweringError {
    match lower_promql(q, AccuracyTarget::Exact) {
        Err(e) => e,
        Ok(tree) => panic!("expected {q:?} to be rejected, but it lowered to: {tree:?}"),
    }
}

/// Every `AggIntent` anywhere in the tree, root-to-leaf.
fn intents(e: &QueryExpr) -> Vec<AggIntent> {
    let mut out = Vec::new();
    collect(e, &mut out);
    out
}

fn collect(e: &QueryExpr, out: &mut Vec<AggIntent>) {
    match e {
        QueryExpr::Aggregate { aggs, child, .. } => {
            out.extend(aggs.iter().cloned());
            collect(child, out);
        }
        QueryExpr::Window { child, .. }
        | QueryExpr::Partition { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Project { child, .. } => collect(child, out),
        QueryExpr::BinaryOp { lhs, rhs, .. } => {
            collect(lhs, out);
            collect(rhs, out);
        }
        QueryExpr::Join { left, right, .. } | QueryExpr::SetOp { left, right, .. } => {
            collect(left, out);
            collect(right, out);
        }
        QueryExpr::Merge { children } => children.iter().for_each(|c| collect(c, out)),
        QueryExpr::LetBinding { expr, child, .. } => {
            collect(expr, out);
            collect(child, out);
        }
        QueryExpr::Scan { .. } | QueryExpr::Ref { .. } => {}
    }
}

/// The first `Scan` reached by descending single-child nodes, with its metric
/// name and predicate count.
fn first_scan(e: &QueryExpr) -> (String, usize) {
    match e {
        QueryExpr::Scan {
            source, predicates, ..
        } => {
            let name = match source {
                Source::TimeSeries { metric } => metric.clone(),
                Source::Table { table_ref } => table_ref.clone(),
            };
            (name, predicates.len())
        }
        QueryExpr::Window { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Partition { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => first_scan(child),
        other => panic!("no Scan reachable from {other:?}"),
    }
}

fn has<F: Fn(&AggIntent) -> bool>(e: &QueryExpr, pred: F) -> bool {
    intents(e).iter().any(pred)
}

// ─────────────────────────────────────────────────────────────────────────────
// A. Selectors & label matchers           (basics §"Instant/Range Vector
//    Selectors"; selectors.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn instant_vector_selector() {
    // SEMANTICS: bare metric → instant vector (latest sample per series).
    let (metric, preds) = first_scan(&ok("node_cpu_seconds_total"));
    assert_eq!(metric, "node_cpu_seconds_total");
    assert_eq!(preds, 0, "no label matchers → no predicates");
}

#[test]
fn label_matchers_become_scan_predicates() {
    // SEMANTICS: `=`, `!=`, `=~`, `!~` filter series; one conjunct per matcher.
    let (_, preds) = first_scan(&ok(
        r#"http_requests_total{job!="x",path=~"/api/.*",env!~"dev"}"#,
    ));
    assert_eq!(preds, 3, "three matchers → three Scan predicates");
}

#[test]
fn name_label_selects_the_metric() {
    // SEMANTICS: the metric name is the internal `__name__` label.
    let (metric, preds) = first_scan(&ok(r#"{__name__="up"}"#));
    assert_eq!(metric, "up");
    assert_eq!(preds, 0, "__name__ is the metric, not a residual predicate");
}

#[test]
fn range_vector_selector_is_a_window() {
    // SEMANTICS: `[5m]` turns an instant vector into a range vector.
    let qe = ok("node_cpu_seconds_total[5m]");
    let QueryExpr::Window { size, .. } = &qe else {
        panic!("expected Window for a range-vector selector, got {qe:?}");
    };
    assert_eq!(*size, Duration::from_secs(300));
}

// ─────────────────────────────────────────────────────────────────────────────
// B. Counters: rate / irate / increase     (cheat sheet "Rates of Increase";
//    functions.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rate_carries_its_window_in_the_intent() {
    // SEMANTICS: per-second average rate over the range; the window IS the rate
    // parameter, so no separate Window node.
    let qe = ok("rate(http_requests_total[5m])");
    assert!(matches!(&qe, QueryExpr::Aggregate { .. }));
    assert!(has(
        &qe,
        |i| matches!(i, AggIntent::Rate { window } if *window == Duration::from_secs(300))
    ));
}

#[test]
fn irate_maps_to_rate_intent() {
    // SEMANTICS: instant rate from the last two samples; same intent vocabulary.
    assert!(has(&ok("irate(http_requests_total[1m])"), |i| matches!(
        i,
        AggIntent::Rate { .. }
    )));
}

#[test]
fn increase_maps_to_increase_intent() {
    assert!(has(&ok("increase(http_requests_total[1h])"), |i| matches!(
        i,
        AggIntent::Increase { window } if *window == Duration::from_secs(3600)
    )));
}

// ─────────────────────────────────────────────────────────────────────────────
// C. Aggregation across series             (cheat sheet "Aggregating Over
//    Multiple Series"; aggregators.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sum_collapses_all_series() {
    // SEMANTICS: `sum(v)` → one output series. No grouping → no Partition.
    let qe = ok("sum(node_filesystem_size_bytes)");
    assert!(matches!(&qe, QueryExpr::Aggregate { .. }));
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum)));
}

#[test]
fn sum_by_preserves_dimensions_as_partition() {
    // SEMANTICS: `by(job,instance)` keeps those labels; grouping rides on Partition.
    let qe = ok("sum by(job, instance) (node_filesystem_size_bytes)");
    let QueryExpr::Partition { keys, .. } = &qe else {
        panic!("expected Partition for `by(...)`, got {qe:?}");
    };
    assert_eq!(
        *keys,
        PartitionKeys::By(vec!["job".into(), "instance".into()])
    );
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum)));
}

#[test]
fn count_is_cardinality() {
    assert!(has(&ok("count(up)"), |i| matches!(
        i,
        AggIntent::Cardinality { .. }
    )));
}

#[test]
fn avg_min_max_stddev_stdvar_quantile_aggregators() {
    assert!(has(&ok("avg(up)"), |i| matches!(i, AggIntent::Avg)));
    assert!(has(&ok("min(up)"), |i| matches!(i, AggIntent::Min)));
    assert!(has(&ok("max(up)"), |i| matches!(i, AggIntent::Max)));
    assert!(has(&ok("stddev(up)"), |i| matches!(
        i,
        AggIntent::StdDev { .. }
    )));
    assert!(has(&ok("stdvar(up)"), |i| matches!(
        i,
        AggIntent::Variance { .. }
    )));
    assert!(has(&ok("quantile(0.5, up)"), |i| matches!(
        i,
        AggIntent::Quantile { .. }
    )));
}

#[test]
fn sum_without_is_rejected() {
    // SEMANTICS: `without(instance)` = group by all labels EXCEPT instance.
    // We can't enumerate a metric's full label set (usage-derived schema), so
    // the complement is rejected rather than silently mis-grouped.
    let e = rejected("sum without(instance) (node_filesystem_size_bytes)");
    assert!(format!("{e}").contains("without"), "got {e}");
}

#[test]
fn group_aggregator_is_lowered_as_sum__GAP() {
    // SEMANTICS (PromQL): `group(v)` returns a constant 1 per group (presence),
    // NOT a sum. We currently fold it onto `Sum`. Pinned as a known divergence.
    assert!(has(&ok("group by (job) (up)"), |i| matches!(
        i,
        AggIntent::Sum
    )));
}

// ─────────────────────────────────────────────────────────────────────────────
// D. Two-level: outer aggregation OVER an inner counter  (the canonical
//    `sum(rate(...))` shape; aggregators.test + functions.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sum_of_rate_is_two_levels() {
    // SEMANTICS: per-series rate, THEN cross-series sum. Both must survive.
    let qe = ok("sum(rate(http_requests_total[5m]))");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate{{Sum}}, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum]));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate { .. }])
    ));
}

#[test]
fn sum_by_of_rate_groups_outer_level() {
    let qe = ok("sum by(instance) (rate(node_network_receive_bytes_total[5m]))");
    let QueryExpr::Partition { keys, child } = &qe else {
        panic!("expected Partition, got {qe:?}");
    };
    assert_eq!(*keys, PartitionKeys::By(vec!["instance".into()]));
    // Partition → Sum → Rate.
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Sum])
    ));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate { .. })));
}

// ─────────────────────────────────────────────────────────────────────────────
// E. Aggregation over time (per-series)    (cheat sheet "Aggregating Over
//    Time"; functions.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn over_time_functions_window_then_reduce() {
    // SEMANTICS: reduce the samples WITHIN each series over the range → Window
    // over the matching reduce intent.
    for (q, want) in [
        ("avg_over_time(go_goroutines[5m])", "avg"),
        ("max_over_time(process_resident_memory_bytes[1d])", "max"),
        ("min_over_time(go_goroutines[5m])", "min"),
        ("sum_over_time(go_goroutines[5m])", "sum"),
        ("count_over_time(go_goroutines[5m])", "count"),
    ] {
        let qe = ok(q);
        assert!(
            matches!(&qe, QueryExpr::Window { .. }),
            "{q}: expected Window"
        );
        let matched = intents(&qe).iter().any(|i| match want {
            "avg" => matches!(i, AggIntent::Avg),
            "max" => matches!(i, AggIntent::Max),
            "min" => matches!(i, AggIntent::Min),
            "sum" => matches!(i, AggIntent::Sum),
            "count" => matches!(i, AggIntent::Count { .. }),
            _ => unreachable!(),
        });
        assert!(matched, "{q}: missing {want} intent");
    }
}

#[test]
fn quantile_over_time_is_window_over_quantile() {
    let qe = ok("quantile_over_time(0.9, request_latency_seconds[5m])");
    assert!(matches!(&qe, QueryExpr::Window { .. }));
    assert!(has(
        &qe,
        |i| matches!(i, AggIntent::Quantile { q, .. } if (*q - 0.9).abs() < 1e-9)
    ));
}

// ─────────────────────────────────────────────────────────────────────────────
// F. Histograms                            (cheat sheet "Quantiles from
//    Histograms"; histograms.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn histogram_quantile_over_rate() {
    // SEMANTICS: φ-quantile estimated from bucket rates.
    let qe = ok("histogram_quantile(0.9, rate(demo_api_request_duration_seconds_bucket[5m]))");
    let QueryExpr::Aggregate { aggs, .. } = &qe else {
        panic!("expected Aggregate{{Quantile}}, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.9).abs() < 1e-9));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate { .. })));
}

#[test]
fn histogram_quantile_over_sum_by_le_preserves_le_grouping() {
    // SEMANTICS: the standard pattern — bucket rates summed by `le`, then the
    // quantile. The `sum by (le)` aggregation must survive into L3.
    let qe = ok(
        "histogram_quantile(0.99, sum by(le) (rate(demo_api_request_duration_seconds_bucket[5m])))",
    );
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate{{Quantile}}, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Quantile { .. }]));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Partition { keys, .. } if *keys == PartitionKeys::By(vec!["le".into()])
    ));
}

// ─────────────────────────────────────────────────────────────────────────────
// G. Binary ops: math, matching, comparison  (cheat sheet "Math Between
//    Series" / "Filtering Series by Value"; operators.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn vector_arithmetic() {
    let qe = ok("node_memory_MemFree_bytes + node_memory_Cached_bytes");
    let QueryExpr::BinaryOp { op, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Add);
}

#[test]
fn on_matching_with_group_left() {
    // SEMANTICS: many-to-one matching on a label subset.
    let qe =
        ok("rate(demo_cpu_usage_seconds_total[1m]) / on(instance, job) group_left demo_num_cpus");
    let QueryExpr::BinaryOp {
        op, vector_match, ..
    } = &qe
    else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Div);
    let vm = vector_match.as_ref().expect("on(...) group_left present");
    assert_eq!(vm.labels, vec!["instance".to_string(), "job".to_string()]);
    assert!(
        vm.grouping.is_some(),
        "group_left should set the grouping side"
    );
}

#[test]
fn vector_comparison_filters() {
    // SEMANTICS: `>` between two vectors keeps the LHS series where it holds.
    let qe = ok("go_goroutines > go_threads");
    assert!(matches!(&qe, QueryExpr::BinaryOp { op, .. } if *op == BinaryOpKind::Gt));
}

#[test]
fn scalar_literal_operand_is_rejected__GAP() {
    // SEMANTICS (PromQL): `v > 10*1024*1024` filters by a scalar threshold.
    // We have no scalar/number-literal expression in L2, so a literal operand
    // is rejected. Common real-world thresholds therefore don't lower yet.
    let _ = rejected("node_filesystem_avail_bytes > 10*1024*1024");
}

// ─────────────────────────────────────────────────────────────────────────────
// H. Set operations                        (cheat sheet "Set Operations";
//    operators.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn set_ops_lower_to_binaryop() {
    // SEMANTICS: or = union of label sets; and = intersection; unless = difference.
    assert!(matches!(&ok("up{job=\"a\"} or up{job=\"b\"}"),
        QueryExpr::BinaryOp { op, .. } if *op == BinaryOpKind::Or));
    assert!(matches!(&ok("node_network_mtu_bytes and node_up"),
        QueryExpr::BinaryOp { op, .. } if *op == BinaryOpKind::And));
    assert!(matches!(&ok("node_network_mtu_bytes unless node_down"),
        QueryExpr::BinaryOp { op, .. } if *op == BinaryOpKind::Unless));
}

// ─────────────────────────────────────────────────────────────────────────────
// I. Sorting / top-k                        (cheat sheet "Sorting"/topk;
//    functions.test, limit.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn topk_over_count_is_heavy_hitter() {
    // SEMANTICS: top-k by frequency → single-pass heavy-hitter sketch.
    let qe = ok("topk(10, count_over_time(http_requests_total[1m]))");
    assert!(has(
        &qe,
        |i| matches!(i, AggIntent::TopK { k, .. } if *k == 10)
    ));
}

#[test]
fn bottomk_is_generic_sort_limit() {
    // SEMANTICS: bottom-k → generic ascending order + limit (no sketch).
    let qe = ok("bottomk(3, count_over_time(http_requests_total[5m]))");
    assert!(matches!(&qe, QueryExpr::Limit { .. }));
}

#[test]
fn topk_over_aggregate_arg_is_rejected__GAP() {
    // SEMANTICS (PromQL): `topk(3, sum by(x)(rate(...)))` is extremely common.
    // Our aggregate-argument lowering only accepts selectors/calls, not a
    // nested aggregate, so this is rejected today.
    let _ = rejected("topk(3, sum by(instance) (rate(node_cpu_seconds_total[5m])))");
}

// ─────────────────────────────────────────────────────────────────────────────
// J. Subqueries                            (basics §Subqueries; subquery.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn subquery_wraps_inner_query() {
    // SEMANTICS: `<inst>[range:res]` evaluates the inner query across a range.
    let qe = ok("rate(demo_api_request_duration_seconds_count[5m])[1h:]");
    assert!(matches!(&qe, QueryExpr::Subquery { .. }));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate { .. })));
}

#[test]
fn over_time_of_subquery_is_rejected__GAP() {
    // SEMANTICS (PromQL): `max_over_time(rate(...)[1h:])` chains a subquery into
    // a range-vector function. `extract_matrix` doesn't accept a subquery arg,
    // so this canonical pattern is rejected today.
    let _ = rejected("max_over_time(rate(demo_api_request_duration_seconds_count[5m])[1h:])");
}

// ─────────────────────────────────────────────────────────────────────────────
// K. Time-shift modifiers                  (basics §Offset/@; at_modifier.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn offset_modifier_is_silently_dropped__GAP() {
    // SEMANTICS (PromQL): `offset 5m` shifts the lookback 5m into the past.
    // `vs_parts` reads only name + matchers, so the offset is lost: the query
    // lowers as if there were no offset. Pinned so the silent loss is visible.
    let (metric, _) = first_scan(&ok("http_requests_total offset 5m"));
    assert_eq!(metric, "http_requests_total");
}

#[test]
fn at_modifier_is_silently_dropped__GAP() {
    // SEMANTICS (PromQL): `@ <ts>` pins the evaluation time. Also dropped today.
    let (metric, _) = first_scan(&ok("http_requests_total @ 1609746000"));
    assert_eq!(metric, "http_requests_total");
}

// ─────────────────────────────────────────────────────────────────────────────
// L. Unsupported functions                 (functions.test) — clean rejection
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn unsupported_functions_are_rejected() {
    // These parse fine but have no intent-algebra lowering yet. Each must return
    // a clean LoweringError rather than mislower.
    for q in [
        "time()",
        "timestamp(up)",
        "absent(up)",
        "absent_over_time(up[5m])",
        "deriv(demo_disk_usage_bytes[1h])",
        "delta(demo_disk_usage_bytes[1h])",
        "predict_linear(demo_disk_usage_bytes[4h], 3600)",
        r#"label_replace(up, "host", "$1", "instance", "(.+):.*")"#,
        "clamp_max(go_goroutines, 5)",
    ] {
        let _ = rejected(q);
    }
}
