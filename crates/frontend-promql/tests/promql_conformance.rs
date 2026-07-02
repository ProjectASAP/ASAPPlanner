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
//! - `__GAP`      — a PromQL capability we don't *yet* support. It is **cleanly
//!   rejected** (never silently mislowered), and pinned here so adding support
//!   later flips the assertion deliberately.
//!
//! NOTE: the formerly-silent divergences (`group`→sum, dropped `offset`/`@`,
//! `changes`/`resets`→count) are now rejected rather than mislowered — see the
//! equivalence suite (`promql_equivalence.rs`) and section L below.

// `__GAP`-suffixed test names intentionally SHOUT the documented divergences.
#![allow(non_snake_case)]

use std::time::Duration;

use asap_ir::intent_algebra::schema::DataType;
use asap_ir::intent_algebra::{
    AggIntent, ArithOp, BinaryOpKind, CompareOp, QueryExpr, Source,
};
use asap_ir::types::AccuracyTarget;
use asap_frontend_promql::{lower_promql, PromqlError as LoweringError};

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
        | QueryExpr::TimeRange { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::WindowFunc { child, .. }
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
        QueryExpr::Scan { .. } | QueryExpr::Scalar(_) | QueryExpr::Ref { .. } => {}
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
        | QueryExpr::TimeRange { child, .. }
        | QueryExpr::Aggregate { child, .. }
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
fn promql_scan_schema_is_open() {
    // A schemaless PromQL leaf is *open*: the metric's full label set is
    // runtime-only, so the binding schema lists only the (ts, value) floor +
    // referenced labels and may be a subset of the runtime row.
    let qe = ok("node_cpu_seconds_total");
    let QueryExpr::Scan { schema, .. } = &qe else {
        panic!("expected a Scan for a bare selector, got {qe:?}");
    };
    assert!(
        !schema.closed,
        "a schemaless PromQL scan has an open schema"
    );
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
fn name_regex_matcher_is_rejected__GAP() {
    // A `__name__=~` / `!~` / `!=` matcher selects *across* metric names, which
    // the single-metric `Source::TimeSeries { metric }` can't represent. It is
    // rejected (issue #67) rather than silently mislowered to a literal metric
    // named after the pattern (`{__name__=~"node_.*"}` → `Source("node_.*")`).
    // Full support needs a wildcard/regex `Source` in the IR.
    let _ = rejected(r#"{__name__=~"node_.*"}"#);
    let _ = rejected(r#"{__name__!~"x", job="y"}"#);
    // Equality still names the metric (regression guard for the fix).
    let (metric, _) = first_scan(&ok(r#"{__name__="up"}"#));
    assert_eq!(metric, "up");
}

#[test]
fn range_vector_selector_is_time_range() {
    // SEMANTICS: `[5m]` turns an instant vector into a range vector.
    // In L3 this is a dedicated `TimeRange` node (not a streaming `Window`).
    let qe = ok("node_cpu_seconds_total[5m]");
    let QueryExpr::TimeRange { range, .. } = &qe else {
        panic!("expected TimeRange for a range-vector selector, got {qe:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
}

// ─────────────────────────────────────────────────────────────────────────────
// B. Counters: rate / irate / increase     (cheat sheet "Rates of Increase";
//    functions.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rate_range_lives_in_time_range_node() {
    // SEMANTICS: per-second average rate; the temporal range lives on the
    // enclosing `TimeRange` node, not inside the intent.
    let qe = ok("rate(http_requests_total[5m])");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Rate]));
    let QueryExpr::TimeRange { range, .. } = child.as_ref() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
}

#[test]
fn irate_maps_to_rate_intent() {
    // SEMANTICS: instant rate from the last two samples; same intent vocabulary.
    assert!(has(&ok("irate(http_requests_total[1m])"), |i| matches!(
        i,
        AggIntent::Rate
    )));
}

#[test]
fn increase_range_lives_in_time_range_node() {
    let qe = ok("increase(http_requests_total[1h])");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Increase]));
    let QueryExpr::TimeRange { range, .. } = child.as_ref() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(3600));
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
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum { .. })));
}

#[test]
fn sum_by_groups_via_positional_aggregate() {
    // SEMANTICS: `by(job,instance)` keeps those labels; the grouping lives on a
    // positional `Aggregate.by` — the same shape SQL `GROUP BY` produces (not a
    // name-based Partition). Binder leaf = [ts, value, instance, job] (referenced
    // keys appended sorted), so the keys resolve to columns [2, 3].
    let qe = ok("sum by(job, instance) (node_filesystem_size_bytes)");
    let QueryExpr::Aggregate {
        by, aggs, child, ..
    } = &qe
    else {
        panic!("expected positional Aggregate for `by(...)`, got {qe:?}");
    };
    assert_eq!(
        by,
        &vec![2, 3],
        "group keys resolve to positional ColumnIds"
    );
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(child.as_ref(), QueryExpr::Scan { .. }));
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
    assert!(has(&ok("avg(up)"), |i| matches!(i, AggIntent::Avg { .. })));
    assert!(has(&ok("min(up)"), |i| matches!(i, AggIntent::Min { .. })));
    assert!(has(&ok("max(up)"), |i| matches!(i, AggIntent::Max { .. })));
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
fn group_aggregator_is_rejected() {
    // SEMANTICS (PromQL): `group(v)` returns a constant 1 per group (presence),
    // NOT a sum. Rather than fold it onto `Sum` (wrong value) we reject it.
    let _ = rejected("group by (job) (up)");
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
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate])
    ));
}

#[test]
fn sum_by_of_rate_groups_outer_level() {
    // Outer cross-series Sum grouped on positional `Aggregate.by` over the
    // label-preserving inner Rate. Leaf = [ts, value, instance] → by = [2].
    let qe = ok("sum by(instance) (rate(node_network_receive_bytes_total[5m]))");
    let QueryExpr::Aggregate {
        by, aggs, child, ..
    } = &qe
    else {
        panic!("expected outer Aggregate grouped by instance, got {qe:?}");
    };
    assert_eq!(by, &vec![2]);
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    // child is the inner per-series Rate aggregate.
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate])
    ));
}

#[test]
fn sum_by_of_over_time_groups_outer_level() {
    // Outer cross-series Sum grouped on positional `Aggregate.by` over an inner
    // *per-series* `avg_over_time` — `Window { Aggregate{Avg} }` is label-
    // preserving, so the key resolves positionally just like the rate case (no
    // name-based Partition). Leaf = [ts, value, instance] → by = [2].
    let qe = ok("sum by(instance) (avg_over_time(node_cpu_seconds_total[5m]))");
    let QueryExpr::Aggregate {
        by, aggs, child, ..
    } = &qe
    else {
        panic!("expected outer Aggregate grouped by instance, got {qe:?}");
    };
    assert_eq!(by, &vec![2]);
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    // child is the inner per-series reduction: Aggregate{Avg} over TimeRange.
    let QueryExpr::Aggregate { aggs, child, .. } = child.as_ref() else {
        panic!("expected Aggregate (per-series avg_over_time) under the Sum, got {child:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Avg { .. }]));
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

// ─────────────────────────────────────────────────────────────────────────────
// E. Aggregation over time (per-series)    (cheat sheet "Aggregating Over
//    Time"; functions.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn over_time_functions_reduce_over_time_range() {
    // SEMANTICS: reduce the samples WITHIN each series over the range →
    // Aggregate over TimeRange (per-series, label-preserving).
    for (q, want) in [
        ("avg_over_time(go_goroutines[5m])", "avg"),
        ("max_over_time(process_resident_memory_bytes[1d])", "max"),
        ("min_over_time(go_goroutines[5m])", "min"),
        ("sum_over_time(go_goroutines[5m])", "sum"),
        ("count_over_time(go_goroutines[5m])", "count"),
    ] {
        let qe = ok(q);
        assert!(
            matches!(&qe, QueryExpr::Aggregate { .. }),
            "{q}: expected Aggregate"
        );
        let matched = intents(&qe).iter().any(|i| match want {
            "avg" => matches!(i, AggIntent::Avg { .. }),
            "max" => matches!(i, AggIntent::Max { .. }),
            "min" => matches!(i, AggIntent::Min { .. }),
            "sum" => matches!(i, AggIntent::Sum { .. }),
            "count" => matches!(i, AggIntent::Count { .. }),
            _ => unreachable!(),
        });
        assert!(matched, "{q}: missing {want} intent");
    }
}

#[test]
fn quantile_over_time_is_aggregate_over_time_range() {
    let qe = ok("quantile_over_time(0.9, request_latency_seconds[5m])");
    assert!(matches!(&qe, QueryExpr::Aggregate { .. }));
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
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate)));
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
    // `sum by(le)` now survives as a positional Aggregate (by = [2], `le`), over
    // the inner Rate — no name-based Partition.
    let QueryExpr::Aggregate { by, aggs, .. } = child.as_ref() else {
        panic!("expected `sum by(le)` as a positional Aggregate, got {child:?}");
    };
    assert_eq!(by, &vec![2]);
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
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
    assert_eq!(*op, BinaryOpKind::Arith(ArithOp::Add));
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
    assert_eq!(*op, BinaryOpKind::Arith(ArithOp::Div));
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
    assert!(
        matches!(&qe, QueryExpr::BinaryOp { op, .. } if *op == BinaryOpKind::Compare(CompareOp::Gt))
    );
}

#[test]
fn unary_negation_is_rejected__GAP() {
    // SEMANTICS (PromQL): `-expr` flips the sign of every sample (and `-rate(…)`
    // negates the rate). With no negate/scalar node in the L2 PromQL path we
    // can't model that, so it's rejected rather than silently lowered as `+expr`
    // (which would compute the wrong result). `-<literal>` folds into the
    // literal at parse time and is caught by the bare-scalar rejection instead.
    let _ = rejected("-rate(http_errors_total[5m])");
    let _ = rejected("-some_metric");
    let _ = rejected("-metric_a or -metric_b");
    // Negation nested inside a larger expression propagates the rejection,
    // rather than lowering the rest with the inner sign silently dropped.
    let _ = rejected("http_requests_total - -http_errors_total");
    let _ = rejected("sum(-node_cpu_seconds_total)");
}

#[test]
fn count_maps_to_cardinality_and_inherits_accuracy() {
    // SEMANTICS (review #2): PromQL `count by (...)` counts distinct series → the
    // `Cardinality` intent. The workload's AccuracyTarget threads onto it:
    // `Exact` stays exact (no silent HLL substitution); an approximate target is
    // carried through for L4 to honor. This pins the intentional count→Cardinality
    // mapping and its accuracy gating.
    let exact = lower_promql("count by (job) (up)", AccuracyTarget::Exact).unwrap();
    assert!(
        has(&exact, |i| matches!(
            i,
            AggIntent::Cardinality {
                accuracy: AccuracyTarget::Exact
            }
        )),
        "count→Cardinality must stay Exact under AccuracyTarget::Exact, got {:?}",
        intents(&exact)
    );

    let approx = lower_promql("count by (job) (up)", AccuracyTarget::Epsilon(0.01)).unwrap();
    assert!(
        has(&approx, |i| matches!(
            i,
            AggIntent::Cardinality {
                accuracy: AccuracyTarget::Epsilon(e)
            } if (*e - 0.01).abs() < 1e-9
        )),
        "count→Cardinality must carry the approximate target, got {:?}",
        intents(&approx)
    );
}

#[test]
fn scalar_literal_operand_lowers_as_binaryop_scalar() {
    // Issue #35: `<vector> op <scalar>` — the numeric threshold is a `Scalar`
    // operand of the `BinaryOp`, and constant arithmetic (`10*1024*1024`) is
    // folded. The output schema is the vector side's.
    let qe = ok("node_filesystem_avail_bytes > 10*1024*1024");
    let QueryExpr::BinaryOp { op, lhs, rhs, .. } = &qe else {
        panic!("expected a BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Compare(CompareOp::Gt));
    assert!(matches!(lhs.as_ref(), QueryExpr::Scan { .. }), "vector on the left");
    assert!(
        matches!(rhs.as_ref(), QueryExpr::Scalar(v) if (*v - 10_485_760.0).abs() < 1e-6),
        "folded scalar threshold on the right, got {rhs:?}"
    );
    // Schema derivation follows the vector side (a scalar contributes no labels).
    assert!(qe.output_schema().is_ok());
}

#[test]
fn scalar_arithmetic_scales_the_vector() {
    // `rate(m[5m]) * 100` — a unit conversion. Arithmetic BinaryOp of the vector
    // with a `Scalar(100)`.
    let qe = ok("rate(m[5m]) * 100");
    let QueryExpr::BinaryOp { op, rhs, .. } = &qe else {
        panic!("expected a BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Arith(ArithOp::Mul));
    assert!(matches!(rhs.as_ref(), QueryExpr::Scalar(v) if (*v - 100.0).abs() < 1e-9));
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
    // SEMANTICS: top-k by frequency → first-class heavy-hitter `TopK` intent.
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
fn topk_over_nested_aggregate_is_generic_sort_limit() {
    // SEMANTICS (PromQL): `topk(3, sum by(x)(rate(...)))` is extremely common.
    // The argument is a nested aggregate (not the heavy-hitter `count` shape),
    // so it ranks by sample value → generic `Sort{value desc} → Limit{k}` over
    // the fully-lowered inner aggregate (issue #27: arbitrary function nesting).
    let qe = ok("topk(3, sum by(instance) (rate(node_cpu_seconds_total[5m])))");
    let QueryExpr::Limit { n, child, .. } = &qe else {
        panic!("expected outer Limit, got {qe:?}");
    };
    assert_eq!(*n, 3);
    let QueryExpr::Sort { keys, child, .. } = child.as_ref() else {
        panic!("expected Sort under Limit, got {child:?}");
    };
    assert!(!keys[0].ascending, "topk ranks descending by value");
    // The inner `sum by (instance)` survives as a cross-series Aggregate over the
    // per-series rate — the nesting the old two-level template could not express.
    assert!(
        has(child, |i| matches!(i, AggIntent::Sum { .. }))
            && has(child, |i| matches!(i, AggIntent::Rate)),
        "inner sum-over-rate preserved, got {:?}",
        intents(child)
    );
}

#[test]
fn outer_aggregate_over_nested_aggregate_nests() {
    // `max(sum by (job) (rate(m[5m])))` — an outer cross-series reduction over a
    // nested per-group reduction over a per-series rate: three stacked levels the
    // flat two-level template rejected. Each level survives into L3 (issue #27).
    let qe = ok("max(sum by (job) (rate(http_requests_total[5m])))");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Max { .. }]));
    let QueryExpr::Aggregate { by, aggs, .. } = child.as_ref() else {
        panic!("expected inner `sum by (job)` Aggregate, got {child:?}");
    };
    assert_eq!(by, &vec![2], "job grouping survives on the inner aggregate");
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate)), "rate preserved");
}

#[test]
fn aggregate_over_binary_op_nests() {
    // `sum(rate(a[5m]) + rate(b[5m]))` — an aggregate whose argument is a binary
    // op over two range vectors. The old template only accepted a single inner
    // selector/call; now the binary op lowers and the outer sum wraps it.
    let qe = ok("sum(rate(a[5m]) + rate(b[5m]))");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    assert!(
        matches!(child.as_ref(), QueryExpr::BinaryOp { .. }),
        "argument lowers as a BinaryOp, got {child:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// J. Subqueries                            (basics §Subqueries; subquery.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn subquery_wraps_inner_query() {
    // SEMANTICS: `<inst>[range:res]` evaluates the inner query across a range.
    let qe = ok("rate(demo_api_request_duration_seconds_count[5m])[1h:]");
    assert!(matches!(&qe, QueryExpr::Subquery { .. }));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate)));
}

#[test]
fn over_time_of_subquery_reduces_per_series() {
    // SEMANTICS (PromQL): `max_over_time(rate(...)[1h:])` chains a sub-query into
    // a range-vector function — the sub-query evaluates `rate` across a 1h range,
    // then `max_over_time` takes the max of those samples *per series*. It lowers
    // to a per-series `Max` reduction over a `Subquery` (issue #27).
    let qe = ok("max_over_time(rate(demo_api_request_duration_seconds_count[5m])[1h:])");
    let QueryExpr::Aggregate { by, aggs, child, .. } = &qe else {
        panic!("expected an Aggregate at the root, got {qe:?}");
    };
    assert!(by.is_empty(), "`*_over_time` has no grouping — reduces per series");
    assert!(matches!(aggs.as_slice(), [AggIntent::Max { .. }]));
    // The reduction rides directly on the sub-query (the structural range marker
    // that keeps it label-preserving), which wraps the inner `rate`.
    assert!(
        matches!(child.as_ref(), QueryExpr::Subquery { .. }),
        "the `Max` reduces over a Subquery, got {child:?}"
    );
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Rate)));
}

#[test]
fn quantile_over_time_of_subquery_carries_phi() {
    // The `quantile_over_time` φ parameter is read from arg 0; the sub-query is
    // arg 1. It lowers to a per-series `Quantile(φ)` over the `Subquery`.
    let qe = ok("quantile_over_time(0.9, rate(demo[5m])[1h:])");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.9).abs() < 1e-9));
    assert!(matches!(child.as_ref(), QueryExpr::Subquery { .. }));
}

#[test]
fn aggregation_over_over_time_of_subquery_keeps_labels() {
    // `sum by (job) (max_over_time(rate(m[5m])[1h:]))` — the inner
    // `max_over_time` is per-series (label-preserving), so the `job` label
    // survives for the OUTER cross-series `sum by (job)` to group on. If the
    // inner `Max` collapsed labels, `job` would not resolve here.
    let qe = ok("sum by (job) (max_over_time(rate(demo{job=\"api\"}[5m])[1h:]))");
    let QueryExpr::Aggregate { by, aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(!by.is_empty(), "outer `sum by (job)` groups on a label");
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    // Inner node is the per-series `max_over_time` reduction over the subquery.
    let QueryExpr::Aggregate { by: inner_by, aggs: inner_aggs, child: inner_child, .. } =
        child.as_ref()
    else {
        panic!("expected inner Aggregate, got {child:?}");
    };
    assert!(inner_by.is_empty());
    assert!(matches!(inner_aggs.as_slice(), [AggIntent::Max { .. }]));
    assert!(matches!(inner_child.as_ref(), QueryExpr::Subquery { .. }));
}

// ─────────────────────────────────────────────────────────────────────────────
// K. Time-shift modifiers                  (basics §Offset/@; at_modifier.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn offset_modifier_is_rejected() {
    // SEMANTICS (PromQL): `offset 5m` shifts the lookback 5m into the past. The
    // intent algebra can't represent it, so we reject rather than silently drop
    // it (which would change the query's meaning).
    let _ = rejected("http_requests_total offset 5m");
}

#[test]
fn at_modifier_is_rejected() {
    // SEMANTICS (PromQL): `@ <ts>` pins the evaluation time. Rejected for the
    // same reason as `offset`.
    let _ = rejected("http_requests_total @ 1609746000");
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
        r#"label_replace(up, "host", "$1", "instance", "(.+):.*")"#,
        "clamp_max(go_goroutines, 5)",
        // NOTE: changes / delta / deriv / resets / predict_linear /
        // double_exponential_smoothing now lower to distinct counter-derivative
        // intents (issue #44) — see section M below.
    ] {
        let _ = rejected(q);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// M. Counter-derivative range functions     (functions.test; issue #44)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn count_over_time_value_column_is_float64() {
    // #69: a per-series range reduction produces a PromQL sample value, which is
    // always float64. `count_over_time`'s `Count` intent types `Int64`, but the
    // derived `value` column must be `Float64` like every other range reducer.
    let schema = ok("count_over_time(m[5m])").output_schema().unwrap();
    let value = schema
        .columns
        .iter()
        .find(|c| c.name == "value")
        .expect("value column");
    assert_eq!(value.dtype, DataType::Float64);
}

#[test]
fn counter_derivative_functions_lower_to_distinct_intents() {
    // Each range function reduces one series' window to one value per series
    // (label-preserving), riding on a `TimeRange`, and carries its OWN intent —
    // deliberately not aliased to rate/increase/count.
    for (q, want) in [
        ("changes(m[15m])", AggIntent::Changes),
        ("delta(m[5m])", AggIntent::Delta),
        ("idelta(m[5m])", AggIntent::IDelta),
        ("deriv(m[1h])", AggIntent::Deriv),
        ("resets(m[1h])", AggIntent::Resets),
    ] {
        let qe = ok(q);
        let QueryExpr::Aggregate { by, aggs, child, .. } = &qe else {
            panic!("expected an Aggregate for {q:?}, got {qe:?}");
        };
        assert!(by.is_empty(), "{q}: per-series, no grouping");
        assert_eq!(aggs.as_slice(), std::slice::from_ref(&want), "{q}: wrong intent");
        assert!(
            matches!(child.as_ref(), QueryExpr::TimeRange { .. }),
            "{q}: reduction rides on a TimeRange, got {child:?}"
        );
    }
}

#[test]
fn predict_linear_carries_horizon_seconds() {
    // `predict_linear(v[w], t)` — the 2nd (scalar) arg is the prediction horizon
    // in seconds; it must be carried in the intent (it changes the result).
    let qe = ok("predict_linear(node_filesystem_avail_bytes[3h], 86400)");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    assert_eq!(aggs.as_slice(), &[AggIntent::PredictLinear { seconds: 86400.0 }]);
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

#[test]
fn double_exponential_smoothing_carries_factors_and_holt_winters_is_an_alias() {
    // Both spellings lower to the same intent carrying the two smoothing factors.
    let want = AggIntent::DoubleExpSmoothing {
        smoothing: 0.5,
        trend: 0.3,
    };
    let a = ok("double_exponential_smoothing(m[10m], 0.5, 0.3)");
    let b = ok("holt_winters(m[10m], 0.5, 0.3)");
    assert_eq!(intents(&a).as_slice(), std::slice::from_ref(&want));
    assert_eq!(a, b, "holt_winters is the legacy alias of double_exponential_smoothing");
}

#[test]
fn aggregation_over_counter_derivative_keeps_labels() {
    // A counter-derivative is per-series (label-preserving), so an outer
    // `sum by (job)` can group on a label the inner `changes` preserved.
    let qe = ok(r#"sum by (job) (changes(m{job="api"}[15m]))"#);
    let QueryExpr::Aggregate { by, aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(!by.is_empty(), "outer sum groups on job");
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Changes)));
    let _ = child;
}

#[test]
fn outer_stat_over_counter_derivative_nests_two_levels() {
    // A cross-series stat over a counter-derivative is a genuine two-level
    // reduction: the derivative runs per series (inner), the stat aggregates
    // across series (outer). They must not collapse into one node — and a
    // grouped outer (`avg by (dc)`) must resolve its key against the labels the
    // inner reduction preserved, threading any scalar param (predict horizon).
    let qe = ok("avg by (dc) (predict_linear(m[3h], 3600))");
    let QueryExpr::Aggregate { by, aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(!by.is_empty(), "outer `avg by (dc)` groups on a label");
    assert!(matches!(aggs.as_slice(), [AggIntent::Avg { .. }]));
    let QueryExpr::Aggregate { by: inner_by, aggs: inner_aggs, .. } = child.as_ref() else {
        panic!("expected inner per-series Aggregate, got {child:?}");
    };
    assert!(inner_by.is_empty(), "inner derivative stays per-series");
    assert_eq!(
        inner_aggs.as_slice(),
        std::slice::from_ref(&AggIntent::PredictLinear { seconds: 3600.0 })
    );
}

#[test]
fn topk_over_counter_derivative_is_generic_sort_limit() {
    // `topk(k, deriv(...))` ranks the per-series derivative values — a generic
    // `Sort + Limit`, NOT a heavy-hitter `TopK` (that's only `count_over_time`).
    let qe = ok("topk(3, deriv(m[5m]))");
    let QueryExpr::Limit { n, child, .. } = &qe else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 3);
    assert!(matches!(child.as_ref(), QueryExpr::Sort { .. }));
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Deriv)));
    assert!(
        !intents(&qe).iter().any(|i| matches!(i, AggIntent::TopK { .. })),
        "counter-derivative topk is generic ranking, not a heavy-hitter sketch"
    );
}

#[test]
fn counter_derivative_composes_in_binary_ops() {
    // As a vector operand: `delta(a[5m]) / delta(b[5m])` is a BinaryOp of two
    // per-series Delta reductions.
    let ratio = ok("delta(a[5m]) / delta(b[5m])");
    let QueryExpr::BinaryOp { op, lhs, rhs, .. } = &ratio else {
        panic!("expected BinaryOp, got {ratio:?}");
    };
    assert_eq!(*op, BinaryOpKind::Arith(ArithOp::Div));
    assert!(matches!(lhs.as_ref(), QueryExpr::Aggregate { aggs, .. } if aggs.as_slice() == [AggIntent::Delta]));
    assert!(matches!(rhs.as_ref(), QueryExpr::Aggregate { aggs, .. } if aggs.as_slice() == [AggIntent::Delta]));

    // Under an aggregate over a binary op mixing a counter-derivative with
    // another per-series function: `sum(rate(m[5m]) + changes(m[5m]))`.
    let mixed = ok("sum(rate(m[5m]) + changes(m[5m]))");
    let QueryExpr::Aggregate { aggs, child, .. } = &mixed else {
        panic!("expected Aggregate, got {mixed:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(child.as_ref(), QueryExpr::BinaryOp { .. }));
    assert!(intents(&mixed).iter().any(|i| matches!(i, AggIntent::Rate)));
    assert!(intents(&mixed).iter().any(|i| matches!(i, AggIntent::Changes)));
}

#[test]
fn range_functions_over_a_subquery_reduce_per_series() {
    // Issue #55 — the whole range-vector family accepts a sub-query argument
    // (generalizing `*_over_time`, #42): `rate`/`increase`/`irate` and the
    // counter-derivatives. Each lowers to a per-series `Aggregate{[f]}` directly
    // over the `Subquery` — the sub-query is the range context, so there is NO
    // separate `TimeRange` (that would double the range).
    for (q, want) in [
        ("rate(sum(m)[5m:])", AggIntent::Rate),
        ("increase(sum(m)[5m:])", AggIntent::Increase),
        ("irate(sum(m)[5m:])", AggIntent::Rate), // irate shares the Rate intent
        ("changes(rate(m[5m])[1h:])", AggIntent::Changes),
        ("delta(sum(m)[5m:])", AggIntent::Delta),
        ("deriv(sum(m)[10m:])", AggIntent::Deriv),
        ("resets(sum(m)[5m:])", AggIntent::Resets),
    ] {
        let qe = ok(q);
        let QueryExpr::Aggregate { by, aggs, child, .. } = &qe else {
            panic!("{q}: expected an Aggregate, got {qe:?}");
        };
        assert!(by.is_empty(), "{q}: per-series, no grouping");
        assert_eq!(aggs.as_slice(), std::slice::from_ref(&want), "{q}: wrong intent");
        assert!(
            matches!(child.as_ref(), QueryExpr::Subquery { .. }),
            "{q}: reduces directly over the Subquery (no TimeRange), got {child:?}"
        );
    }
}

#[test]
fn predict_linear_and_double_exp_over_a_subquery_carry_params() {
    // The scalar params survive the sub-query path.
    let pl = ok("predict_linear(sum(m)[1h:], 3600)");
    assert!(intents(&pl)
        .iter()
        .any(|i| matches!(i, AggIntent::PredictLinear { seconds } if (*seconds - 3600.0).abs() < 1e-9)));
    let de = ok("double_exponential_smoothing(sum(m)[10m:], 0.5, 0.3)");
    assert!(intents(&de).iter().any(|i| matches!(
        i,
        AggIntent::DoubleExpSmoothing { smoothing, trend }
            if (*smoothing - 0.5).abs() < 1e-9 && (*trend - 0.3).abs() < 1e-9
    )));
}
