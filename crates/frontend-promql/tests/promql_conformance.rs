//! PromQL **semantic conformance** for the parse-to-canonical-tree lowering.
//!
//! We *lower* PromQL to the intent algebra; we do not *execute* it. So "same
//! semantic job as Prometheus" here means: for each canonical query, does the
//! canonical tree encode the **documented PromQL meaning** — and where we knowingly
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
//! - (no suffix)  — we lower it and the canonical intent matches PromQL.
//! - `__GAP`      — a PromQL capability we don't *yet* support. It is **cleanly
//!   rejected** (never silently mislowered), and pinned here so adding support
//!   later flips the assertion deliberately.
//!
//! NOTE: the formerly-silent divergences (`group`→sum, dropped `offset`/`@`,
//! `changes`/`resets`→count) are now rejected rather than mislowered — see the
//! equivalence suite (`promql_equivalence.rs`) and section L below.

// `__GAP`-suffixed test names intentionally SHOUT the documented divergences.
#![allow(non_snake_case)]

use std::rc::Rc;
use std::time::Duration;

use asap_frontend_promql::PromqlError as LoweringError;
mod support;
use asap_types::ir::{
    BinaryOperator, ExprSemantics, NonASAPOp, OperatorNode, ScalarExpr, TimeRangeKind,
};
use asap_types::pre_asap::schema::DataType;
use asap_types::pre_asap::{
    AggIntent, ArithmeticOpKind, AtModifier, BinaryOpKind, CompareOpKind, MathFunc,
    PromQLVectorSetOpKind, Reduction, SampleKind, ScalarValue, Source, TimeFunc,
};
use asap_types::types::AccuracyTarget;
use support::{lower_promql, promql_scalar};

// ── harness helpers ─────────────────────────────────────────────────────────────

/// Lower, expecting success.
fn ok(q: &str) -> Rc<OperatorNode> {
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
fn intents(e: &OperatorNode) -> Vec<AggIntent> {
    let mut out = Vec::new();
    collect(e, &mut out);
    out
}

/// `AggIntent` only ever lives in `Aggregate.measures`, never in a scalar
/// position (issue #205); `children()` also descends into the operators a
/// scalar position reads (`scalar(v)`).
fn collect(e: &OperatorNode, out: &mut Vec<AggIntent>) {
    if let Some(NonASAPOp::Aggregate { measures, .. }) = e.non_asap() {
        out.extend(measures.iter().cloned());
    }
    for child in e.children() {
        collect(child, out);
    }
}

/// The first `Scan` reached by descending single-child nodes, with its metric
/// name and predicate count.
fn first_scan(e: &OperatorNode) -> (String, usize) {
    match e.expect_non_asap() {
        NonASAPOp::Scan {
            source, predicates, ..
        } => {
            let name = match source {
                Source::TimeSeries { metric } => metric.clone(),
                Source::Table { table_ref } => table_ref.clone(),
            };
            (name, predicates.len())
        }
        NonASAPOp::TimeRange { child, .. }
        | NonASAPOp::TimeShift { child, .. }
        | NonASAPOp::Aggregate { child, .. }
        | NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. }
        | NonASAPOp::PromqlSubquery { child, .. } => first_scan(child),
        other => panic!("no Scan reachable from {other:?}"),
    }
}

fn has<F: Fn(&AggIntent) -> bool>(e: &OperatorNode, pred: F) -> bool {
    intents(e).iter().any(pred)
}

/// Whether the tree contains a `Mul`-by-`ScalarExpr(-1)` anywhere — the shape unary
/// negation lowers to (issue #36).
fn negates_via_scalar(e: &OperatorNode) -> bool {
    fn negative(expr: &ScalarExpr) -> bool {
        matches!(expr, ScalarExpr::Arithmetic { op: ArithmeticOpKind::Mul, right, .. } if promql_scalar(right) == Some(-1.0))
            || expr.children().iter().any(|e| negative(e))
    }
    e.expect_non_asap()
        .scalar_exprs()
        .iter()
        .any(|e| negative(e))
        || e.children().iter().any(|e| negates_via_scalar(e))
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
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected a TimeRange for a bare selector, got {qe:?}");
    };
    let NonASAPOp::Scan { schema, .. } = child.expect_non_asap() else {
        panic!("expected a Scan inside the TimeRange, got {qe:?}");
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
    // SEMANTICS: `[5m]` turns an instant vector into a range vector,
    // represented in the canonical tree as a dedicated `TimeRange` node.
    let qe = ok("node_cpu_seconds_total[5m]");
    let NonASAPOp::TimeRange { range, .. } = qe.expect_non_asap() else {
        panic!("expected TimeRange for a range-vector selector, got {qe:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
}

// ─────────────────────────────────────────────────────────────────────────────
// B. Counters: rate / irate / increase     (cheat sheet "Rates of Increase";
//    functions.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn selector_time_ranges_carry_their_kind() {
    // SEMANTICS: an instant selector reads the latest sample within the
    // ingestion interval (`Instant`); `m[5m]` is a range selection (`Range`).
    // Same length is not the same shape: `m` and `m[1s]` stay distinct.
    assert!(matches!(
        ok("node_cpu_seconds_total").expect_non_asap(),
        NonASAPOp::TimeRange {
            kind: TimeRangeKind::Instant,
            ..
        }
    ));
    assert!(matches!(
        ok("node_cpu_seconds_total[5m]").expect_non_asap(),
        NonASAPOp::TimeRange {
            kind: TimeRangeKind::Range,
            ..
        }
    ));
    assert_ne!(
        ok("node_cpu_seconds_total"),
        ok("node_cpu_seconds_total[1s]")
    );
}

#[test]
fn rate_range_lives_in_time_range_node() {
    // SEMANTICS: per-second average rate; the temporal range lives on the
    // enclosing `TimeRange` node, not inside the intent.
    let qe = ok("rate(http_requests_total[5m])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let NonASAPOp::TimeRange { range, .. } = child.expect_non_asap() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
}

#[test]
fn irate_maps_to_its_own_intent() {
    assert!(has(&ok("irate(http_requests_total[1m])"), |i| matches!(
        i,
        AggIntent::IRate
    )));
}

#[test]
fn increase_range_lives_in_time_range_node() {
    let qe = ok("increase(http_requests_total[1h])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Increase]));
    let NonASAPOp::TimeRange { range, .. } = child.expect_non_asap() else {
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
    assert!(matches!(qe.expect_non_asap(), NonASAPOp::Aggregate { .. }));
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum { .. })));
}

#[test]
fn sum_by_groups_via_positional_aggregate() {
    // SEMANTICS: `by(job,instance)` keeps those labels; the grouping lives on a
    // positional `Aggregate.by` — the same shape SQL `GROUP BY` produces (not a
    // name-based Partition). SchemaResolver leaf = [ts, value, instance, job] (referenced
    // keys appended sorted), so the keys resolve to columns [2, 3].
    let qe = ok("sum by(job, instance) (node_filesystem_size_bytes)");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected positional Aggregate for `by(...)`, got {qe:?}");
    };
    assert_eq!(
        reduction,
        &Reduction::by(vec![2, 3]),
        "group keys resolve to positional ColumnIds"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::TimeRange { child, .. } if matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }))
    );
}

#[test]
fn count_is_row_count() {
    assert!(has(&ok("count(up)"), |i| matches!(
        i,
        AggIntent::Count { .. }
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
fn sum_without_groups_by_the_complement() {
    // SEMANTICS (issue #39): `without(instance)` = group by all labels EXCEPT
    // instance. The complement can't be enumerated under the open usage-derived
    // schema, so the excluded label is stored and the kept set is deferred to
    // the runtime: the grouping is the exclusion form and the output schema
    // stays OPEN (unlike `by`, which freezes to closed).
    let qe = ok("sum without(instance) (node_filesystem_size_bytes)");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    let by = reduction.expect_reduce();
    assert!(
        by.is_without(),
        "the grouping is the `without` exclusion form"
    );
    assert_eq!(by.keys().len(), 1, "the one excluded label (instance)");
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(
        !qe.schema.clone().closed,
        "a `without` result keeps an open schema (kept label set is runtime-only)"
    );
}

#[test]
fn without_on_topk_is_rejected() {
    // `without(...)` is modelled only for reducing aggregations; on topk/bottomk
    // (a ranking, not a reduction) it would need without-partitioning, so it is
    // rejected rather than silently lowered as a `by` (issue #39).
    let e = rejected("topk without (job) (3, http_requests_total)");
    assert!(format!("{e}").contains("without"), "got {e}");
}

#[test]
fn group_aggregator_lowers_to_a_distinct_intent() {
    // SEMANTICS (PromQL): `group(v)` returns a constant 1 per group (presence),
    // NOT a sum. It now lowers to a distinct `Group` intent (never folded onto
    // `Sum`) — see §S. Regression guard that it is not a `Sum`.
    let qe = ok("group by (job) (up)");
    assert!(has(&qe, |i| *i == AggIntent::Group));
    assert!(!has(&qe, |i| matches!(i, AggIntent::Sum { .. })));
}

// ─────────────────────────────────────────────────────────────────────────────
// D. Two-level: outer aggregation OVER an inner counter  (the canonical
//    `sum(rate(...))` shape; aggregators.test + functions.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sum_of_rate_is_two_levels() {
    // SEMANTICS: per-series rate, THEN cross-series sum. Both must survive.
    let qe = ok("sum(rate(http_requests_total[5m]))");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate{{Sum}}, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate])
    ));
}

#[test]
fn sum_by_of_rate_groups_outer_level() {
    // Outer cross-series Sum grouped on positional `Aggregate.by` over the
    // label-preserving inner Rate. Leaf = [ts, value, instance] → by = [2].
    let qe = ok("sum by(instance) (rate(node_network_receive_bytes_total[5m]))");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate grouped by instance, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    // child is the inner per-series Rate aggregate.
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate])
    ));
}

#[test]
fn sum_by_of_over_time_groups_outer_level() {
    // Outer cross-series Sum grouped on positional `Aggregate.by` over an inner
    // *per-series* `avg_over_time` — `Window { Aggregate{Avg} }` is label-
    // preserving, so the key resolves positionally just like the rate case (no
    // name-based Partition). Leaf = [ts, value, instance] → by = [2].
    let qe = ok("sum by(instance) (avg_over_time(node_cpu_seconds_total[5m]))");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate grouped by instance, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    // child is the inner per-series reduction: Aggregate{Avg} over TimeRange.
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected Aggregate (per-series avg_over_time) under the Sum, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Avg { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
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
            matches!(qe.expect_non_asap(), NonASAPOp::Aggregate { .. }),
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
    assert!(matches!(qe.expect_non_asap(), NonASAPOp::Aggregate { .. }));
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
    // φ-quantile from bucket rates. The `_bucket` metric marks the classic
    // cumulative-bucket form → `HistogramQuantile` (even without `sum by (le)`).
    let qe = ok("histogram_quantile(0.9, rate(demo_api_request_duration_seconds_bucket[5m]))");
    let NonASAPOp::Aggregate { measures, .. } = qe.expect_non_asap() else {
        panic!("expected Aggregate{{HistogramQuantile}}, got {qe:?}");
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::HistogramQuantile { q, .. }] if (*q - 0.9).abs() < 1e-9)
    );
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate)));
}

#[test]
fn histogram_quantile_over_sum_by_le_preserves_le_grouping() {
    // SEMANTICS: the standard pattern — bucket rates summed by `le`, then the
    // quantile. The `sum by (le)` aggregation must survive into the
    // canonical tree.
    let qe = ok(
        "histogram_quantile(0.99, sum by(le) (rate(demo_api_request_duration_seconds_bucket[5m])))",
    );
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate{{HistogramQuantile}}, got {qe:?}");
    };
    // `by (le)` marks the classic cumulative-bucket form → `HistogramQuantile`.
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::HistogramQuantile { .. }]
    ));
    // `sum by(le)` now survives as a positional Aggregate (by = [2], `le`), over
    // the inner Rate — no name-based Partition.
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected `sum by(le)` as a positional Aggregate, got {child:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
}

// ─────────────────────────────────────────────────────────────────────────────
// G. Binary ops: math, matching, comparison  (cheat sheet "Math Between
//    Series" / "Filtering Series by Value"; operators.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn vector_arithmetic() {
    let qe = ok("node_memory_MemFree_bytes + node_memory_Cached_bytes");
    let NonASAPOp::BinaryOp {
        operator: BinaryOperator { kind: op, .. },
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Arithmetic(ArithmeticOpKind::Add));
}

#[test]
fn on_matching_with_group_left() {
    // SEMANTICS: many-to-one matching on a label subset.
    let qe =
        ok("rate(demo_cpu_usage_seconds_total[1m]) / on(instance, job) group_left demo_num_cpus");
    let NonASAPOp::BinaryOp { operator, .. } = qe.expect_non_asap() else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(
        operator.kind,
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Div)
    );
    let vm = operator
        .vector_match
        .as_ref()
        .expect("on(...) group_left present");
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
        matches!(qe.expect_non_asap(), NonASAPOp::BinaryOp { operator: BinaryOperator { kind: op, .. }, .. } if *op == BinaryOpKind::Compare(CompareOpKind::Gt))
    );
}

#[test]
fn comparison_bool_modifier_returns_zero_or_one() {
    // SEMANTICS (operators.test): `bool` turns a filtering comparison into a
    // 0/1-valued one. On a vector operand it is `return_bool` on the
    // `BinaryOp`; between two scalars it is a `Case(Compare → 1, else 0)`
    // scalar expression under PromQL numeric rules — and a scalar comparison
    // without `bool` is not a PromQL expression at all.
    let bool_flag = |q: &str| match ok(q).expect_non_asap() {
        NonASAPOp::BinaryOp { return_bool, .. } => *return_bool,
        NonASAPOp::Project { .. } => true,
        NonASAPOp::Filter { .. } => false,
        other => panic!("expected BinaryOp for {q}, got {other:?}"),
    };
    assert!(bool_flag("go_goroutines > bool go_threads"));
    assert!(bool_flag("go_goroutines > bool 0"));
    assert!(!bool_flag("go_goroutines > go_threads"));
    assert!(!bool_flag("go_goroutines > 0"));

    let qe = support::scalar_root("1 < bool 2");
    let ScalarExpr::Case { branches, .. } = &qe else {
        panic!("expected a scalar Case, got {qe:?}");
    };
    assert!(matches!(
        branches.as_slice(),
        [(
            ScalarExpr::Compare {
                op: CompareOpKind::Lt,
                semantics: ExprSemantics::Promql,
                ..
            },
            _
        )]
    ));
    rejected("1 < 2");
}

#[test]
fn unary_negation_lowers_as_multiply_by_minus_one() {
    // SEMANTICS (PromQL, issue #36): `-expr` flips the sign of every sample.
    // Now that a scalar operand exists (#35), it lowers as `expr * -1` — a `Mul`
    // BinaryOp of the (label-preserving) vector against `ScalarExpr(-1)`. These are
    // the five cases the old `__GAP` test pinned as rejected.
    for q in [
        "-rate(http_errors_total[5m])",
        "-some_metric",
        "-metric_a or -metric_b",
        "http_requests_total - -http_errors_total",
        "sum(-node_cpu_seconds_total)",
    ] {
        let qe = ok(q);
        // A `Mul`-by-`-1` against a `ScalarExpr(-1)` appears somewhere in every tree.
        assert!(
            negates_via_scalar(&qe),
            "no `* -1` negation found in {q}: {qe:?}"
        );
    }

    let negated = ok("-some_metric");
    assert!(negates_via_scalar(&negated));
    assert!(negated.schema.has_promql_series_identity());
    assert!(negated.schema.time_index.is_some());
    let summed = ok("sum(-node_cpu_seconds_total)");
    assert!(has(&summed, |i| matches!(i, AggIntent::Sum { .. })));
    assert!(negates_via_scalar(&summed));
}

#[test]
fn unary_negation_of_constant_folds_to_scalar() {
    // `-(10*1024*1024)` — the operand is constant-foldable, so negation collapses
    // to a single negated `ScalarExpr` leaf (no `BinaryOp`), just like a bare literal.
    assert!(promql_scalar(&support::scalar_root("-(10*1024*1024)"))
        .is_some_and(|v| (v + 10_485_760.0).abs() < 1e-6));
}

#[test]
fn double_unary_negation_nests() {
    let qe = ok("- -some_metric");
    let NonASAPOp::Project { child, .. } = qe.expect_non_asap() else {
        panic!()
    };
    assert!(matches!(child.expect_non_asap(), NonASAPOp::Project { .. }));
    assert!(negates_via_scalar(child));
}

#[test]
fn count_maps_to_count_and_inherits_accuracy() {
    // Counts preserve the workload accuracy target without counting distinct values.
    let exact = lower_promql("count by (job) (up)", AccuracyTarget::Exact).unwrap();
    assert!(
        has(&exact, |i| matches!(
            i,
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact
            }
        )),
        "Count must stay Exact under AccuracyTarget::Exact, got {:?}",
        intents(&exact)
    );

    let approx = lower_promql("count by (job) (up)", AccuracyTarget::Epsilon(0.01)).unwrap();
    assert!(
        has(&approx, |i| matches!(
            i,
            AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(e)
            } if (*e - 0.01).abs() < 1e-9
        )),
        "Count must carry the approximate target, got {:?}",
        intents(&approx)
    );
}

#[test]
fn scalar_literal_operand_lowers_as_binaryop_scalar() {
    let qe = ok("node_filesystem_avail_bytes > 10*1024*1024");
    let ScalarExpr::Compare { op, right, .. } = support::sample_expression(&qe) else {
        panic!()
    };
    assert_eq!(*op, CompareOpKind::Gt);
    assert_eq!(promql_scalar(right), Some(10_485_760.0));
}

#[test]
fn scalar_arithmetic_scales_the_vector() {
    let qe = ok("rate(m[5m]) * 100");
    let ScalarExpr::Arithmetic { op, right, .. } = support::sample_expression(&qe) else {
        panic!()
    };
    assert_eq!(*op, ArithmeticOpKind::Mul);
    assert_eq!(promql_scalar(right), Some(100.0));
}

// ─────────────────────────────────────────────────────────────────────────────
// H. Set operations                        (cheat sheet "Set Operations";
//    operators.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn set_ops_lower_to_binaryop() {
    // SEMANTICS: or = union of label sets; and = intersection; unless = difference.
    let set_op = |q: &str| match ok(q).expect_non_asap() {
        NonASAPOp::BinaryOp { operator, .. } => operator.kind.clone(),
        other => panic!("expected BinaryOp for {q}, got {other:?}"),
    };
    assert_eq!(
        set_op("up{job=\"a\"} or up{job=\"b\"}"),
        BinaryOpKind::Set(PromQLVectorSetOpKind::Or)
    );
    assert_eq!(
        set_op("node_network_mtu_bytes and node_up"),
        BinaryOpKind::Set(PromQLVectorSetOpKind::And)
    );
    assert_eq!(
        set_op("node_network_mtu_bytes unless node_down"),
        BinaryOpKind::Set(PromQLVectorSetOpKind::Unless)
    );
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
    assert!(matches!(qe.expect_non_asap(), NonASAPOp::Limit { .. }));
}

#[test]
fn topk_over_nested_sum_preserves_weighted_topk_accuracy() {
    // SEMANTICS (PromQL): `topk(3, sum by(x)(rate(...)))` is extremely common.
    // The final rates are query-time values. Their ordering does not establish
    // frequency-sketch membership semantics.
    let qe = ok("topk(3, sum by(instance) (rate(node_cpu_seconds_total[5m])))");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected weighted TopK aggregate, got {qe:?}");
    };
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::TopK { k: 3, .. }]
    ));
    // The inner `sum by (instance)` survives as a cross-series Aggregate over the
    // per-series rate — the nesting the old two-level template could not express.
    assert!(
        has(child, |i| matches!(i, AggIntent::Sum { .. }))
            && has(child, |i| matches!(i, AggIntent::Rate)),
        "inner sum-over-rate preserved, got {:?}",
        intents(child)
    );
    assert!(has(&qe, |i| matches!(i, AggIntent::TopK { .. })));
}

#[test]
fn outer_aggregate_over_nested_aggregate_nests() {
    // `max(sum by (job) (rate(m[5m])))` — an outer cross-series reduction over a
    // nested per-group reduction over a per-series rate: three stacked levels the
    // flat two-level template rejected. Each level survives into the
    // canonical tree (issue #27).
    let qe = ok("max(sum by (job) (rate(http_requests_total[5m])))");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Max { .. }]));
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected inner `sum by (job)` Aggregate, got {child:?}");
    };
    assert_eq!(
        reduction,
        &Reduction::by(vec![2]),
        "job grouping survives on the inner aggregate"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate)), "rate preserved");
}

#[test]
fn outer_group_key_absent_from_nested_aggregate_is_dropped() {
    // SEMANTICS (PromQL, issue #53): aggregating `by` a label that no input
    // series carries is valid — every series lands in one group and the
    // (empty) label is omitted from the output. Here the inner `sum by (group)`
    // collapses `job` away (its closed output schema is `[group, sum]`), so the
    // outer `by (job)` groups everything into a single global partition:
    // the query lowers with the provably-absent key dropped, exactly
    // `sum(sum by (group)(…))`.
    let qe = ok(r#"sum(sum by (group)(http_requests{job="api-server"})) by (job)"#);
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert_eq!(
        reduction,
        &Reduction::by(vec![]),
        "absent `job` key dropped → global aggregate"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    let NonASAPOp::Aggregate { reduction, .. } = child.expect_non_asap() else {
        panic!("expected inner `sum by (group)` Aggregate, got {child:?}");
    };
    assert_eq!(
        reduction,
        &Reduction::by(vec![2]),
        "inner grouping on `group` survives"
    );
}

#[test]
fn outer_group_key_present_after_inner_aggregate_still_resolves() {
    // The counterpart guard for #53: when the outer key IS in the inner
    // aggregate's output (`by (job)` over `sum by (job, group)`), it must keep
    // resolving positionally — the absent-key drop only fires on provable
    // absence, never on a resolvable key.
    let qe = ok("sum(sum by (job, group)(http_requests)) by (job)");
    let NonASAPOp::Aggregate {
        reduction, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    let NonASAPOp::Aggregate {
        reduction: inner_reduction,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected inner Aggregate, got {child:?}");
    };
    // Inner output schema is [group, job, sum] (keys in label-column order,
    // labels alphabetical on the scan) → job = col 1.
    assert_eq!(
        reduction,
        &Reduction::by(vec![1]),
        "outer `job` resolves against the inner output"
    );
    assert_eq!(inner_reduction.expect_reduce().len(), 2);
}

#[test]
fn outer_group_key_over_binary_op_resolves_on_both_sides() {
    // Issue #52: an outer aggregate's group key that appears in *neither* side of
    // a binary op — the metric-name label `__name__`, or a plain `job` — must
    // still resolve. Each `or` side is bound independently against its own
    // sub-tree, so the key is seeded as an inherited column on both sides.
    let qe = ok(r#"sum by (__name__)(metric_a{env="1"} or metric_b{env="2"})"#);
    let NonASAPOp::Aggregate {
        reduction, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    // `__name__` resolves to a single positional id against the binary op output.
    assert_eq!(
        reduction.expect_reduce().len(),
        1,
        "grouped by the one `__name__` key"
    );
    let NonASAPOp::BinaryOp { lhs, rhs, .. } = child.expect_non_asap() else {
        panic!("expected a BinaryOp child, got {child:?}");
    };
    // Both independently-bound sides carry `__name__` at the same position, so
    // the outer group key is consistent across the union.
    let (ls, rs) = (lhs.schema.clone(), rhs.schema.clone());
    assert_eq!(ls.column_id("__name__"), rs.column_id("__name__"));
    assert_eq!(
        ls.column_id("__name__"),
        Some(reduction.expect_reduce().keys()[0])
    );

    // The general case (a plain label, not just `__name__`) also lowers.
    assert!(matches!(
        ok("sum by (job)(metric_a or metric_b)").expect_non_asap(),
        NonASAPOp::Aggregate { .. }
    ));
}

#[test]
fn aggregate_over_binary_op_nests() {
    // `sum(rate(a[5m]) + rate(b[5m]))` — an aggregate whose argument is a binary
    // op over two range vectors. The old template only accepted a single inner
    // selector/call; now the binary op lowers and the outer sum wraps it.
    let qe = ok("sum(rate(a[5m]) + rate(b[5m]))");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::BinaryOp { .. }),
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
    assert!(matches!(
        qe.expect_non_asap(),
        NonASAPOp::PromqlSubquery { .. }
    ));
    assert!(has(&qe, |i| matches!(i, AggIntent::Rate)));
}

#[test]
fn over_time_of_subquery_reduces_per_series() {
    // SEMANTICS (PromQL): `max_over_time(rate(...)[1h:])` chains a sub-query into
    // a range-vector function — the sub-query evaluates `rate` across a 1h range,
    // then `max_over_time` takes the max of those samples *per series*. It lowers
    // to a per-series `Max` reduction over a `PromqlSubquery` (issue #27).
    let qe = ok("max_over_time(rate(demo_api_request_duration_seconds_count[5m])[1h:])");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected an Aggregate at the root, got {qe:?}");
    };
    assert_eq!(
        reduction,
        &Reduction::PerEntity,
        "`*_over_time` has no grouping — reduces per series"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Max { .. }]));
    // The reduction rides directly on the sub-query (the structural range marker
    // that keeps it label-preserving), which wraps the inner `rate`.
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::PromqlSubquery { .. }),
        "the `Max` reduces over a PromqlSubquery, got {child:?}"
    );
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Rate)));
}

#[test]
fn quantile_over_time_of_subquery_carries_phi() {
    // The `quantile_over_time` φ parameter is read from arg 0; the sub-query is
    // arg 1. It lowers to a per-series `Quantile(φ)` over the `PromqlSubquery`.
    let qe = ok("quantile_over_time(0.9, rate(demo[5m])[1h:])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.9).abs() < 1e-9)
    );
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::PromqlSubquery { .. }
    ));
}

#[test]
fn aggregation_over_over_time_of_subquery_keeps_labels() {
    // `sum by (job) (max_over_time(rate(m[5m])[1h:]))` — the inner
    // `max_over_time` is per-series (label-preserving), so the `job` label
    // survives for the OUTER cross-series `sum by (job)` to group on. If the
    // inner `Max` collapsed labels, `job` would not resolve here.
    let qe = ok("sum by (job) (max_over_time(rate(demo{job=\"api\"}[5m])[1h:]))");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(
        matches!(reduction, Reduction::Reduce(by) if !by.is_empty()),
        "outer `sum by (job)` groups on a label"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    // Inner node is the per-series `max_over_time` reduction over the subquery.
    let NonASAPOp::Aggregate {
        reduction: inner_reduction,
        measures: inner_measures,
        child: inner_child,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected inner Aggregate, got {child:?}");
    };
    assert_eq!(inner_reduction, &Reduction::PerEntity);
    assert!(matches!(inner_measures.as_slice(), [AggIntent::Max { .. }]));
    assert!(matches!(
        inner_child.expect_non_asap(),
        NonASAPOp::PromqlSubquery { .. }
    ));
}

#[test]
fn nested_subquery_from_prometheus_docs() {
    // SEMANTICS (PromQL): the *nested sub-query* example from the official docs
    // (<https://prometheus.io/docs/prometheus/latest/querying/examples/>):
    //
    //   max_over_time(deriv(rate(distance_covered_total[5s])[30s:5s])[10m:])
    //
    // Two stacked sub-queries, each feeding a range-vector function; the outer
    // `[10m:]` uses the **default resolution** (no explicit step). Each level
    // lowers to its own node, so the whole spine pins as:
    //
    //   Max ∘ PromqlSubquery{10m, res: None} ∘ Deriv ∘ PromqlSubquery{30s, res: 5s}
    //       ∘ Rate ∘ TimeRange{5s} ∘ Scan
    //
    // Every reduction is per-series (no grouping), so the output schema stays
    // the label-preserving `[ts, value]`.
    let qe = ok("max_over_time(deriv(rate(distance_covered_total[5s])[30s:5s])[10m:])");

    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected `max_over_time` Aggregate at the root, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::PerEntity);
    assert!(matches!(measures.as_slice(), [AggIntent::Max { .. }]));

    let NonASAPOp::PromqlSubquery {
        range,
        resolution,
        child,
    } = child.expect_non_asap()
    else {
        panic!("expected the outer `[10m:]` PromqlSubquery, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(600));
    assert_eq!(*resolution, None, "`[10m:]` keeps the default resolution");

    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected the `deriv` Aggregate, got {child:?}");
    };
    assert_eq!(reduction, &Reduction::PerEntity);
    assert!(matches!(measures.as_slice(), [AggIntent::Deriv]));

    let NonASAPOp::PromqlSubquery {
        range,
        resolution,
        child,
    } = child.expect_non_asap()
    else {
        panic!("expected the inner `[30s:5s]` PromqlSubquery, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(30));
    assert_eq!(*resolution, Some(Duration::from_secs(5)));

    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected the `rate` Aggregate, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let NonASAPOp::TimeRange { range, .. } = child.expect_non_asap() else {
        panic!("expected the `[5s]` TimeRange under rate, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(5));

    // Per-series end to end: the schema keeps the (ts, value) floor and stays open.
    let schema = qe.schema.clone();
    assert_eq!(
        schema
            .fields
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["ts", "value"],
    );
    assert!(!schema.closed, "per-series chain never freezes the schema");
}

// ─────────────────────────────────────────────────────────────────────────────
// K. Time-shift modifiers                  (basics §Offset/@; at_modifier.test)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn offset_modifier_lowers_to_a_time_shift() {
    // SEMANTICS (PromQL, issue #40): `offset 5m` shifts the lookback 5m into the
    // past — a `TimeShift` wrapper over the selector (signed ms; a negative
    // offset shifts forward). Schema is unchanged (the shift only moves *when*).
    let qe = ok("http_requests_total offset 5m");
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected an ingestion TimeRange, got {qe:?}");
    };
    let NonASAPOp::TimeShift { shift, child } = child.expect_non_asap() else {
        panic!("expected a TimeShift, got {qe:?}");
    };
    assert_eq!(shift.offset_ms, 300_000);
    assert!(shift.at.is_none());
    assert!(matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }));

    // `offset -5m` shifts forward → negative ms.
    let qe = ok("http_requests_total offset -5m");
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected an ingestion TimeRange");
    };
    let NonASAPOp::TimeShift { shift, .. } = child.expect_non_asap() else {
        panic!("expected a TimeShift");
    };
    assert_eq!(shift.offset_ms, -300_000);
}

#[test]
fn at_modifier_lowers_to_a_time_shift() {
    // SEMANTICS (PromQL, issue #40): `@ <ts>` pins the evaluation to an absolute
    // instant (PromQL seconds → IR milliseconds); `@ start()` / `@ end()` anchor
    // to the query range bounds.
    let qe = ok("http_requests_total @ 1609746000");
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected an ingestion TimeRange");
    };
    let NonASAPOp::TimeShift { shift, .. } = child.expect_non_asap() else {
        panic!("expected a TimeShift for `@ <ts>`");
    };
    assert_eq!(shift.at, Some(AtModifier::Timestamp(1_609_746_000_000)));
    assert_eq!(shift.offset_ms, 0);

    let qe = ok("http_requests_total @ start()");
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected an ingestion TimeRange");
    };
    let NonASAPOp::TimeShift { shift, .. } = child.expect_non_asap() else {
        panic!("expected a TimeShift for `@ start()`");
    };
    assert_eq!(shift.at, Some(AtModifier::Start));

    // Offset and `@` compose: `@ end() offset 5m` carries both.
    let qe = ok("http_requests_total @ end() offset 5m");
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected an ingestion TimeRange, got {qe:?}");
    };
    let NonASAPOp::TimeShift { shift, .. } = child.expect_non_asap() else {
        panic!("expected a TimeShift, got {qe:?}");
    };
    assert_eq!(shift.at, Some(AtModifier::End));
    assert_eq!(shift.offset_ms, 300_000);
}

#[test]
fn offset_on_a_ranged_selector_wraps_inside_the_time_range() {
    // `rate(m[5m] offset 1h)` — the offset is on the ranged selector, so the
    // `TimeShift` sits *under* the `TimeRange` (the 5m window is taken at the
    // shifted time), and the whole thing under the per-series `Rate` (#40).
    let qe = ok("rate(http_requests_total[5m] offset 1h)");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected the rate Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let NonASAPOp::TimeRange { child, .. } = child.expect_non_asap() else {
        panic!("expected a TimeRange under rate, got {child:?}");
    };
    let NonASAPOp::TimeShift { shift, child } = child.expect_non_asap() else {
        panic!("expected a TimeShift under the TimeRange, got {child:?}");
    };
    assert_eq!(shift.offset_ms, 3_600_000);
    assert!(matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }));
}

// ─────────────────────────────────────────────────────────────────────────────
// L. Unsupported functions                 (functions.test) — clean rejection
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn unsupported_functions_are_rejected() {
    // These parse fine but have no intent-algebra lowering yet. Each must return
    // a clean LoweringError rather than mislower.
    for q in [
        "step()",
        "range()",
        r#"histogram_quantiles("le", 0.5, 0.9, x)"#,
        // NOTE: counter-derivatives (#44), math/trig (#45, §O), presence (#47,
        // §P), time/calendar (#46, §Q), vector/scalar (#48, §R),
        // label_replace/label_join (#50, §T) and the extra range reducers +
        // sort family (#51, §U) now lower — see those sections. `info` (#84),
        // `min_of`/`max_of` (#89) are pinned in §R / §U.
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
    let schema = ok("count_over_time(m[5m])").schema.clone();
    let value = schema
        .fields
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
        let NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        } = qe.expect_non_asap()
        else {
            panic!("expected an Aggregate for {q:?}, got {qe:?}");
        };
        assert_eq!(
            reduction,
            &Reduction::PerEntity,
            "{q}: per-series, no grouping"
        );
        assert_eq!(
            measures.as_slice(),
            std::slice::from_ref(&want),
            "{q}: wrong intent"
        );
        assert!(
            matches!(child.expect_non_asap(), NonASAPOp::TimeRange { .. }),
            "{q}: reduction rides on a TimeRange, got {child:?}"
        );
    }
}

#[test]
fn predict_linear_carries_horizon_seconds() {
    // `predict_linear(v[w], t)` — the 2nd (scalar) arg is the prediction horizon
    // in seconds; it must be carried in the intent (it changes the result).
    let qe = ok("predict_linear(node_filesystem_avail_bytes[3h], 86400)");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    assert_eq!(
        measures.as_slice(),
        &[AggIntent::PredictLinear { seconds: 86400.0 }]
    );
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

#[test]
fn double_exponential_smoothing_carries_factors() {
    let want = AggIntent::DoubleExpSmoothing {
        smoothing: 0.5,
        trend: 0.3,
    };
    let a = ok("double_exponential_smoothing(m[10m], 0.5, 0.3)");
    assert_eq!(intents(&a).as_slice(), std::slice::from_ref(&want));
}

#[test]
fn aggregation_over_counter_derivative_keeps_labels() {
    // A counter-derivative is per-series (label-preserving), so an outer
    // `sum by (job)` can group on a label the inner `changes` preserved.
    let qe = ok(r#"sum by (job) (changes(m{job="api"}[15m]))"#);
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(
        matches!(reduction, Reduction::Reduce(by) if !by.is_empty()),
        "outer sum groups on job"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
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
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate, got {qe:?}");
    };
    assert!(
        matches!(reduction, Reduction::Reduce(by) if !by.is_empty()),
        "outer `avg by (dc)` groups on a label"
    );
    assert!(matches!(measures.as_slice(), [AggIntent::Avg { .. }]));
    let NonASAPOp::Aggregate {
        reduction: inner_reduction,
        measures: inner_measures,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected inner per-series Aggregate, got {child:?}");
    };
    assert_eq!(
        inner_reduction,
        &Reduction::PerEntity,
        "inner derivative stays per-series"
    );
    assert_eq!(
        inner_measures.as_slice(),
        std::slice::from_ref(&AggIntent::PredictLinear { seconds: 3600.0 })
    );
}

#[test]
fn topk_over_counter_derivative_is_generic_sort_limit() {
    // `topk(k, deriv(...))` ranks the per-series derivative values — a generic
    // `Sort + Limit`, NOT a heavy-hitter `TopK` (that's only `count_over_time`).
    let qe = ok("topk(3, deriv(m[5m]))");
    let NonASAPOp::Limit {
        n: Some(n), child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 3);
    assert!(matches!(child.expect_non_asap(), NonASAPOp::Sort { .. }));
    assert!(intents(&qe).iter().any(|i| matches!(i, AggIntent::Deriv)));
    assert!(
        !intents(&qe)
            .iter()
            .any(|i| matches!(i, AggIntent::TopK { .. })),
        "counter-derivative topk is generic ranking, not a heavy-hitter sketch"
    );
}

#[test]
fn counter_derivative_composes_in_binary_ops() {
    // As a vector operand: `delta(a[5m]) / delta(b[5m])` is a BinaryOp of two
    // per-series Delta reductions.
    let ratio = ok("delta(a[5m]) / delta(b[5m])");
    let NonASAPOp::BinaryOp {
        operator: BinaryOperator { kind: op, .. },
        lhs,
        rhs,
        ..
    } = ratio.expect_non_asap()
    else {
        panic!("expected BinaryOp, got {ratio:?}");
    };
    assert_eq!(*op, BinaryOpKind::Arithmetic(ArithmeticOpKind::Div));
    assert!(
        matches!(lhs.expect_non_asap(), NonASAPOp::Aggregate { measures, .. } if measures.as_slice() == [AggIntent::Delta])
    );
    assert!(
        matches!(rhs.expect_non_asap(), NonASAPOp::Aggregate { measures, .. } if measures.as_slice() == [AggIntent::Delta])
    );

    // Under an aggregate over a binary op mixing a counter-derivative with
    // another per-series function: `sum(rate(m[5m]) + changes(m[5m]))`.
    let mixed = ok("sum(rate(m[5m]) + changes(m[5m]))");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = mixed.expect_non_asap()
    else {
        panic!("expected Aggregate, got {mixed:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::BinaryOp { .. }
    ));
    assert!(intents(&mixed).iter().any(|i| matches!(i, AggIntent::Rate)));
    assert!(intents(&mixed)
        .iter()
        .any(|i| matches!(i, AggIntent::Changes)));
}

#[test]
fn range_functions_over_a_subquery_reduce_per_series() {
    // Issue #55 — the whole range-vector family accepts a sub-query argument
    // (generalizing `*_over_time`, #42): `rate`/`increase`/`irate` and the
    // counter-derivatives. Each lowers to a per-series `Aggregate{[f]}` directly
    // over the `PromqlSubquery` — the sub-query is the range context, so there is NO
    // separate `TimeRange` (that would double the range).
    for (q, want) in [
        ("rate(sum(m)[5m:])", AggIntent::Rate),
        ("increase(sum(m)[5m:])", AggIntent::Increase),
        ("irate(sum(m)[5m:])", AggIntent::IRate),
        ("changes(rate(m[5m])[1h:])", AggIntent::Changes),
        ("delta(sum(m)[5m:])", AggIntent::Delta),
        ("deriv(sum(m)[10m:])", AggIntent::Deriv),
        ("resets(sum(m)[5m:])", AggIntent::Resets),
    ] {
        let qe = ok(q);
        let NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        } = qe.expect_non_asap()
        else {
            panic!("{q}: expected an Aggregate, got {qe:?}");
        };
        assert_eq!(
            reduction,
            &Reduction::PerEntity,
            "{q}: per-series, no grouping"
        );
        assert_eq!(
            measures.as_slice(),
            std::slice::from_ref(&want),
            "{q}: wrong intent"
        );
        assert!(
            matches!(child.expect_non_asap(), NonASAPOp::PromqlSubquery { .. }),
            "{q}: reduces directly over the PromqlSubquery (no TimeRange), got {child:?}"
        );
    }
}

#[test]
fn predict_linear_and_double_exp_over_a_subquery_carry_params() {
    // The scalar params survive the sub-query path.
    let pl = ok("predict_linear(sum(m)[1h:], 3600)");
    assert!(intents(&pl).iter().any(
        |i| matches!(i, AggIntent::PredictLinear { seconds } if (*seconds - 3600.0).abs() < 1e-9)
    ));
    let de = ok("double_exponential_smoothing(sum(m)[10m:], 0.5, 0.3)");
    assert!(intents(&de).iter().any(|i| matches!(
        i,
        AggIntent::DoubleExpSmoothing { smoothing, trend }
            if (*smoothing - 0.5).abs() < 1e-9 && (*trend - 0.3).abs() < 1e-9
    )));
}

// ─────────────────────────────────────────────────────────────────────────────
// N. Native-histogram accessors            (functions.test; issue #43)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn histogram_quantile_classic_bucket_vs_native() {
    // Two lowerings of `histogram_quantile(φ, …)`: the classic cumulative-bucket
    // form → exact `HistogramQuantile`; a native-histogram / raw-samples argument
    // → the generic (sketch-able) `Quantile`. The classic form is recognised by
    // `by (le)`, a `_bucket` metric, or an `le` matcher (issue #43).
    for classic in [
        "histogram_quantile(0.9, sum by (le) (rate(x_bucket[5m])))",
        "histogram_quantile(0.9, rate(x_bucket[5m]))", // bare _bucket metric
        r#"histogram_quantile(0.9, rate(x{le="0.5"}[5m]))"#, // le matcher
    ] {
        let qe = ok(classic);
        assert!(
            has(
                &qe,
                |i| matches!(i, AggIntent::HistogramQuantile { q, .. } if (*q - 0.9).abs() < 1e-9)
            ),
            "classic bucket form → HistogramQuantile: {classic}"
        );
        assert!(
            !has(&qe, |i| matches!(i, AggIntent::Quantile { .. })),
            "{classic}"
        );
    }
    for native in [
        "histogram_quantile(0.9, my_native_histogram)",
        "histogram_quantile(0.9, request_duration_seconds)", // raw samples (your extension)
    ] {
        let qe = ok(native);
        assert!(
            has(
                &qe,
                |i| matches!(i, AggIntent::Quantile { q, .. } if (*q - 0.9).abs() < 1e-9)
            ),
            "native/raw form → generic Quantile: {native}"
        );
        assert!(
            !has(&qe, |i| matches!(i, AggIntent::HistogramQuantile { .. })),
            "{native}"
        );
    }
}

#[test]
fn histogram_accessors_lower_to_per_series_intents() {
    // `histogram_<accessor>(v)` extracts a float per series from a native
    // histogram — a per-series `Aggregate{[accessor]}` directly over the
    // (instant) argument, no grouping. (`histogram_quantile` has its own two
    // lowerings — see `histogram_quantile_classic_bucket_vs_native`.)
    for (q, want) in [
        ("histogram_count(v)", AggIntent::HistogramCount),
        ("histogram_sum(v)", AggIntent::HistogramSum),
        ("histogram_avg(v)", AggIntent::HistogramAvg),
        ("histogram_stddev(v)", AggIntent::HistogramStdDev),
        ("histogram_stdvar(v)", AggIntent::HistogramStdVar),
    ] {
        let qe = ok(q);
        let NonASAPOp::Aggregate {
            reduction,
            measures,
            ..
        } = qe.expect_non_asap()
        else {
            panic!("{q}: expected an Aggregate, got {qe:?}");
        };
        assert_eq!(
            reduction,
            &Reduction::PerEntity,
            "{q}: per-series, no grouping"
        );
        assert_eq!(
            measures.as_slice(),
            std::slice::from_ref(&want),
            "{q}: wrong intent"
        );
    }
}

#[test]
fn histogram_fraction_carries_its_bounds() {
    // `histogram_fraction(lower, upper, v)` — bounds from args 0/1, vector arg 2.
    let qe = ok("histogram_fraction(0, 0.2, v)");
    assert!(intents(&qe).iter().any(|i| matches!(
        i,
        AggIntent::HistogramFraction { lower, upper }
            if *lower == 0.0 && (*upper - 0.2).abs() < 1e-9
    )));
}

// ─────────────────────────────────────────────────────────────────────────────
// O. Math / trig scalar-transform functions (functions.test; issue #45)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn math_functions_lower_to_per_series_math_intents() {
    // Each `f(v)` is a per-series element-wise value transform — a per-series
    // `Aggregate{[Math(f)]}` over the (instant) argument, no grouping.
    for (q, want) in [
        ("abs(v)", MathFunc::Abs),
        ("ceil(v)", MathFunc::Ceil),
        ("floor(v)", MathFunc::Floor),
        ("sqrt(v)", MathFunc::Sqrt),
        ("ln(v)", MathFunc::Ln),
        ("log2(v)", MathFunc::Log2),
        ("sgn(v)", MathFunc::Sgn),
        ("sin(v)", MathFunc::Sin),
        ("atanh(v)", MathFunc::Atanh),
        ("deg(v)", MathFunc::Deg),
        ("rad(v)", MathFunc::Rad),
    ] {
        let qe = ok(q);
        let NonASAPOp::Aggregate {
            reduction,
            measures,
            ..
        } = qe.expect_non_asap()
        else {
            panic!("{q}: expected an Aggregate, got {qe:?}");
        };
        assert_eq!(
            reduction,
            &Reduction::PerEntity,
            "{q}: per-series, no grouping"
        );
        assert!(
            matches!(measures.as_slice(), [AggIntent::Math(m)] if *m == want),
            "{q}: wrong intent, got {measures:?}"
        );
    }
}

#[test]
fn clamp_and_round_carry_their_params() {
    assert!(intents(&ok("clamp(v, 0, 100)")).iter().any(
        |i| matches!(i, AggIntent::Math(MathFunc::Clamp { min, max }) if *min == 0.0 && *max == 100.0)
    ));
    assert!(intents(&ok("clamp_min(v, 1)"))
        .iter()
        .any(|i| matches!(i, AggIntent::Math(MathFunc::ClampMin { min }) if *min == 1.0)));
    assert!(intents(&ok("clamp_max(v, 5)"))
        .iter()
        .any(|i| matches!(i, AggIntent::Math(MathFunc::ClampMax { max }) if *max == 5.0)));
    // `round(v)` defaults the step to 1; `round(v, 5)` reads it.
    assert!(intents(&ok("round(v)")).iter().any(
        |i| matches!(i, AggIntent::Math(MathFunc::Round { to_nearest }) if *to_nearest == 1.0)
    ));
    assert!(intents(&ok("round(v, 5)")).iter().any(
        |i| matches!(i, AggIntent::Math(MathFunc::Round { to_nearest }) if *to_nearest == 5.0)
    ));
}

#[test]
fn pi_lowers_to_a_scalar_constant() {
    // `pi()` is the constant π — a `ScalarExpr` leaf, not a `Math` intent.
    assert!(promql_scalar(&support::scalar_root("pi()"))
        .is_some_and(|v| (v - std::f64::consts::PI).abs() < 1e-12));
}

// ─────────────────────────────────────────────────────────────────────────────
// P. Presence functions                     (functions.test; issue #47)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn presence_functions_lower_to_presence_intents() {
    for (q, want) in [
        (r#"absent(up{job="x"})"#, AggIntent::Absent),
        ("absent_over_time(m[1h])", AggIntent::AbsentOverTime),
        ("present_over_time(m[5m])", AggIntent::PresentOverTime),
    ] {
        let qe = ok(q);
        assert!(intents(&qe).contains(&want), "{q}: got {:?}", intents(&qe));
    }
}

#[test]
fn absent_keeps_matcher_labels_for_the_synthesized_output() {
    // `absent(v)` synthesizes its output labels from `v`'s equality matchers, so
    // those labels must survive into the schema — here `job` from `{job="x"}`.
    let qe = ok(r#"absent(up{job="x"})"#);
    let cols = qe.schema.clone();
    assert!(
        cols.fields.iter().any(|c| c.name == "job"),
        "matcher label `job` kept, got {:?}",
        cols.fields.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Q. Time / calendar functions            (functions.test; issue #46)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn time_lowers_to_the_eval_time_scalar() {
    assert!(matches!(
        support::scalar_root("time()"),
        ScalarExpr::EvalTimestamp
    ));
}

#[test]
fn time_minus_vector_is_the_uptime_pattern() {
    let qe = ok("time() - process_start_time_seconds");
    assert!(
        matches!(support::sample_expression(&qe), ScalarExpr::Arithmetic { op: ArithmeticOpKind::Sub, left, .. } if matches!(left.as_ref(), ScalarExpr::EvalTimestamp))
    );
    assert!(qe.schema.time_index.is_some());
}

#[test]
fn calendar_functions_lower_to_time_fn_intents() {
    // SEMANTICS: each of these is a per-series float transform of its argument's
    // timestamp (or, for `timestamp`, the sample's own time). functions.test.
    for (q, want) in [
        ("timestamp(up)", TimeFunc::Timestamp),
        ("minute(v)", TimeFunc::Minute),
        ("hour(v)", TimeFunc::Hour),
        ("day_of_week(v)", TimeFunc::DayOfWeek),
        ("day_of_month(v)", TimeFunc::DayOfMonth),
        ("day_of_year(v)", TimeFunc::DayOfYear),
        ("month(v)", TimeFunc::Month),
        ("year(v)", TimeFunc::Year),
        ("days_in_month(v)", TimeFunc::DaysInMonth),
    ] {
        let qe = ok(q);
        assert!(
            has(&qe, |i| *i == AggIntent::TimeFn(want)),
            "{q} → TimeFn({want:?}), got {:?}",
            intents(&qe)
        );
    }
}

#[test]
fn no_arg_calendar_function_reads_the_eval_time() {
    // `day_of_week()` with no argument computes over the evaluation time itself,
    // so it is a `TimeFn` aggregate whose child is the `EvalTimestamp` scalar.
    let qe = ok("day_of_week()");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::TimeFn(TimeFunc::DayOfWeek)]
    ));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::PromqlVectorFromScalar(ScalarExpr::EvalTimestamp)
    ));
}

#[test]
fn timestamp_composes_under_an_outer_aggregation() {
    // `sum by (job) (timestamp(up))` — the per-series `timestamp` transform sits
    // below an ordinary grouped sum. Both intents must appear in the tree.
    let qe = ok("sum by (job) (timestamp(up))");
    assert!(has(&qe, |i| *i == AggIntent::TimeFn(TimeFunc::Timestamp)));
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum { .. })));
}

// ─────────────────────────────────────────────────────────────────────────────
// R. Type-conversion functions: vector() / scalar()   (functions.test; issue #48)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn vector_promotes_a_scalar_to_a_vector() {
    // SEMANTICS: `vector(s)` is the scalar→instant-vector bridge — a label-less
    // single series carrying the scalar's value.
    let qe = ok("vector(1)");
    let NonASAPOp::PromqlVectorFromScalar(inner) = qe.expect_non_asap() else {
        panic!("expected PromqlVectorFromScalar, got {qe:?}");
    };
    assert!(matches!(inner, ScalarExpr::Literal(ScalarValue::Float64(v)) if *v == 1.0));
    // Vector-typed: schema has a time index (a scalar leaf has none).
    let sch = qe.schema.clone();
    assert!(sch.time_index.is_some());
    assert!(sch.fields.iter().any(|c| c.name == "value"));
}

#[test]
fn scalar_collapses_a_vector_to_a_scalar() {
    let qe = support::scalar_root("scalar(node_load1)");
    let ScalarExpr::PromqlScalarFromVector(inner) = &qe else {
        panic!()
    };
    assert_eq!(first_scan(inner).0, "node_load1");
}

#[test]
fn vector_zero_is_a_vector_operand_of_a_set_op() {
    // `up or vector(0)` — the dead-man's-switch. `or` is a set op between two
    // vectors, so `vector(0)` must be a vector (a `PromqlVectorFromScalar`), never a
    // folded scalar operand.
    let qe = ok("up or vector(0)");
    let NonASAPOp::BinaryOp {
        operator: BinaryOperator { kind: op, .. },
        rhs,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected a BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Set(PromQLVectorSetOpKind::Or));
    assert!(matches!(
        rhs.expect_non_asap(),
        NonASAPOp::PromqlVectorFromScalar(_)
    ));
}

#[test]
fn scalar_of_a_vector_feeds_a_threshold_comparison() {
    let qe = ok("node_load1 > scalar(node_cpu_count)");
    let ScalarExpr::Compare { right, .. } = support::sample_expression(&qe) else {
        panic!()
    };
    assert!(matches!(
        right.as_ref(),
        ScalarExpr::PromqlScalarFromVector(_)
    ));
    assert!(qe.schema.time_index.is_some());
}

#[test]
fn info_lowers_to_a_label_enrichment_join() {
    // `info(v, [selector])` is a label-enrichment *join* against the info
    // metric(s) — it lowers to an `PromqlInfoEnrich` over the (unchanged) input vector
    // (issue #84). The value/time axis pass through; the enriched labels are
    // runtime, so the schema stays the child's.
    let qe = ok("info(rate(http_requests_total[5m]))");
    let NonASAPOp::PromqlInfoEnrich { selector, child } = qe.expect_non_asap() else {
        panic!("expected an PromqlInfoEnrich, got {qe:?}");
    };
    assert!(selector.is_empty(), "no selector → default target_info");
    // The child is the untouched input (a per-series rate reduction here).
    assert!(has(child, |i| *i == AggIntent::Rate));
    assert!(qe.schema.clone().time_index.is_some());
}

#[test]
fn info_selector_carries_the_info_side_matchers() {
    // `info(v, {__name__=~".+_info", data=~".+"})` — the selector picks the info
    // metric(s) via `__name__` and constrains the data labels. Regex / `__name__`
    // matchers are kept symbolically (not run through the single-metric selector
    // path).
    let qe = ok(r#"info(build_info, {__name__=~".+_info", another_data=~".+"})"#);
    let NonASAPOp::PromqlInfoEnrich { selector, .. } = qe.expect_non_asap() else {
        panic!("expected an PromqlInfoEnrich, got {qe:?}");
    };
    assert_eq!(
        selector.len(),
        2,
        "both selector matchers kept: {selector:?}"
    );
    assert!(selector
        .iter()
        .any(|m| m.label == "__name__" && m.op == CompareOpKind::Regex));
    assert!(selector.iter().any(|m| m.label == "another_data"));
}

#[test]
fn info_composes_under_an_aggregation_and_over_a_time_shift() {
    // `sum(info(m))` — enrichment first, then a cross-series sum over it.
    assert!(has(&ok("sum(info(node_uname_info))"), |i| matches!(
        i,
        AggIntent::Sum { .. }
    )));
    // `offset` / `@` on the input now lower to a `TimeShift` under the info-join
    // (issue #40) — the enrichment composes over the shifted selector.
    assert!(matches!(
        ok("info(metric @ 60)").expect_non_asap(),
        NonASAPOp::PromqlInfoEnrich { .. }
    ));
    assert!(matches!(
        ok("info(metric offset 1m)").expect_non_asap(),
        NonASAPOp::PromqlInfoEnrich { .. }
    ));
}

// ─────────────────────────────────────────────────────────────────────────────
// S. Extended aggregation operators: group / count_values (aggregators.test; #49)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn group_lowers_to_a_constant_group_intent() {
    // SEMANTICS: `group(v)` yields a constant 1 per group — a distinct intent,
    // NOT folded onto `sum` (which would return the value sum instead of 1).
    let qe = ok("group(up)");
    let NonASAPOp::Aggregate { measures, .. } = qe.expect_non_asap() else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Group]));
    // Output column is the constant-1 `group` value.
    let sch = qe.schema.clone();
    assert!(sch.fields.iter().any(|c| c.name == "group"));
}

#[test]
fn group_by_keeps_the_grouping_keys() {
    // `group by (job) (up)` — the grouping keys ride on `Aggregate.by`.
    let qe = ok("group by (job) (up)");
    let sch = qe.schema.clone();
    assert!(sch.fields.iter().any(|c| c.name == "job"));
    assert!(has(&qe, |i| *i == AggIntent::Group));
}

#[test]
fn count_values_groups_by_value_and_synthesizes_a_label() {
    // SEMANTICS: `count_values("l", v)` groups the input series by their sample
    // value, counts each distinct value, and emits that value as a new label
    // `l`. The intent carries the label; schema gains a `Utf8` `l` column.
    let qe = ok(r#"count_values("version", build_version)"#);
    let NonASAPOp::Aggregate { measures, .. } = qe.expect_non_asap() else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::CountValues { label }] if label == "version")
    );
    let sch = qe.schema.clone();
    let version = sch
        .fields
        .iter()
        .find(|c| c.name == "version")
        .expect("synthesized `version` label column");
    assert_eq!(
        version.dtype,
        DataType::Utf8,
        "the value becomes a string label"
    );
    assert!(
        sch.fields.iter().any(|c| c.name == "count"),
        "and a count column"
    );
}

#[test]
fn count_values_accepts_a_parenthesised_label_and_by_grouping() {
    // `count_values by (job) ((("v")), m)` — nested parens around the string
    // param, plus `by` grouping. Both survive.
    let qe = ok(r#"count_values by (job) ((("v")), m)"#);
    assert!(has(
        &qe,
        |i| matches!(i, AggIntent::CountValues { label } if label == "v")
    ));
    let sch = qe.schema.clone();
    assert!(sch.fields.iter().any(|c| c.name == "job"));
    assert!(sch.fields.iter().any(|c| c.name == "v"));
}

#[test]
fn count_values_label_colliding_with_a_group_key_is_not_duplicated() {
    // `count_values by (job)("job", v)` — the synthesized label name collides
    // with a group-by key. PromQL's synthesized label takes precedence; the
    // output must carry a single `job` column, never two.
    let qe = ok(r#"count_values by (job) ("job", version)"#);
    let sch = qe.schema.clone();
    let jobs = sch.fields.iter().filter(|c| c.name == "job").count();
    assert_eq!(jobs, 1, "collision deduped, got {:?}", sch.fields);
    assert!(sch.fields.iter().any(|c| c.name == "count"));
}

#[test]
fn limitk_and_limit_ratio_lower_to_series_sampling() {
    // `limitk`/`limit_ratio` are series-*sampling* selection — a subset of whole
    // series kept unchanged (NOT a ranking), so they lower to the dedicated
    // `PromqlSeriesSample` node, never `topk`'s `Sort → Limit` (issue #86).
    assert!(matches!(
        ok("limitk(2, http_requests)").expect_non_asap(),
        NonASAPOp::PromqlSeriesSample {
            kind: SampleKind::LimitK(2),
            ..
        }
    ));
    assert!(matches!(
        ok("limit_ratio(0.1, http_requests)").expect_non_asap(),
        NonASAPOp::PromqlSeriesSample { kind: SampleKind::LimitRatio(r), .. } if (r - 0.1).abs() < 1e-9
    ));
    // Series-preserving: the output schema equals the input's (ts, value).
    let sch = ok("limitk(2, http_requests)").schema.clone();
    assert!(sch.fields.iter().any(|c| c.name == "value"));
    assert!(sch.time_index.is_some());
}

#[test]
fn limit_ratio_keeps_a_negative_ratio_and_clamps_out_of_range() {
    // A negative ratio selects the complementary fraction — it must survive, not
    // be normalised away. Out-of-range magnitudes clamp to [-1, 1] (Prometheus).
    assert!(matches!(
        ok("limit_ratio(-0.5, http_requests)").expect_non_asap(),
        NonASAPOp::PromqlSeriesSample { kind: SampleKind::LimitRatio(r), .. } if (r + 0.5).abs() < 1e-9
    ));
    assert!(matches!(
        ok("limit_ratio(1.1, http_requests)").expect_non_asap(),
        NonASAPOp::PromqlSeriesSample { kind: SampleKind::LimitRatio(r), .. } if (r - 1.0).abs() < 1e-9
    ));
}

#[test]
fn limitk_by_carries_the_grouping_and_composes_in_a_set_op() {
    // `limitk by (group)` samples per group; the grouping label is seeded.
    let qe = ok("limitk by (group) (2, http_requests)");
    let NonASAPOp::PromqlSeriesSample { by, .. } = qe.expect_non_asap() else {
        panic!("expected a PromqlSeriesSample, got {qe:?}");
    };
    assert!(!by.is_empty(), "grouped sampling keeps its `by` keys");
    // `count(limitk(2, v) and v)` — the surviving series' identity matters, so
    // the PromqlSeriesSample must be preserved under the set op (it must lower, not reject).
    assert!(has(
        &ok("count(limitk(2, http_requests) and http_requests)"),
        |i| matches!(i, AggIntent::Count { .. })
    ));
}

#[test]
fn dynamic_and_non_finite_sample_params_are_rejected() {
    // A dynamic k/ratio (not a compile-time constant) or a NaN can't be a static
    // `PromqlSeriesSample` param — rejected rather than mislowered.
    let _ = rejected("limitk(NaN, http_requests)");
    let _ = rejected("limitk(scalar(foo), http_requests)");
    let _ = rejected("limit_ratio(time() % 17 / 17, http_requests)");
}

// ─────────────────────────────────────────────────────────────────────────────
// T. Label-rewrite functions: label_replace / label_join   (functions.test; #50)
// ─────────────────────────────────────────────────────────────────────────────

/// Descend single-child nodes to the first `PromqlRelabel`.
fn first_relabel(e: &OperatorNode) -> &OperatorNode {
    match e.expect_non_asap() {
        NonASAPOp::PromqlRelabel { .. } => e,
        NonASAPOp::Aggregate { child, .. }
        | NonASAPOp::Filter { child, .. }
        | NonASAPOp::TimeRange { child, .. }
        | NonASAPOp::TimeShift { child, .. } => first_relabel(child),
        other => panic!("no PromqlRelabel reachable from {other:?}"),
    }
}

/// True when `value` is a `FunctionCall` with the given name.
fn is_fn_named(value: &ScalarExpr, name: &str) -> bool {
    matches!(value, ScalarExpr::FunctionCall { name: n, .. } if n == name)
}

#[test]
fn label_replace_is_a_relabel_over_the_vector() {
    // SEMANTICS: `label_replace(v, dst, repl, src, regex)` rewrites the `dst`
    // label per series from a regex over `src`; the sample value is untouched.
    let qe = ok(r#"label_replace(up, "host", "$1", "instance", "(.+):.*")"#);
    let NonASAPOp::PromqlRelabel { dst, value, child } = qe.expect_non_asap() else {
        panic!("expected a PromqlRelabel, got {qe:?}");
    };
    assert_eq!(dst, "host");
    // The child is the untouched vector.
    let (metric, _) = first_scan(child);
    assert_eq!(metric, "up");
    // The value expression is a `label_replace` fn reading the `src` label.
    assert!(is_fn_named(value, "label_replace"));
    // Output: the child's columns + the synthesized `host` label; value & ts kept.
    let sch = qe.schema.clone();
    assert!(sch.fields.iter().any(|c| c.name == "host"));
    assert!(sch.fields.iter().any(|c| c.name == "value"));
    assert!(sch.time_index.is_some(), "the vector's time axis survives");
}

#[test]
fn label_join_concatenates_source_labels() {
    // SEMANTICS: `label_join(v, dst, sep, src…)` joins the source labels with
    // `sep` into `dst`.
    let qe = ok(r#"label_join(up, "combined", "-", "job", "instance")"#);
    let NonASAPOp::PromqlRelabel { dst, value, .. } = qe.expect_non_asap() else {
        panic!("expected a PromqlRelabel, got {qe:?}");
    };
    assert_eq!(dst, "combined");
    assert!(is_fn_named(value, "label_join"));
    let sch = qe.schema.clone();
    assert!(sch.fields.iter().any(|c| c.name == "combined"));
}

#[test]
fn label_replace_composes_under_an_aggregation() {
    // `sum by (host) (label_replace(up, "host", "$1", "instance", "(.+):.*"))` —
    // relabel first, then group by the synthesized label.
    let qe = ok(r#"sum by (host) (label_replace(up, "host", "$1", "instance", "(.+):.*"))"#);
    // A PromqlRelabel sits below the outer Sum.
    let relabel = first_relabel(&qe);
    assert!(
        matches!(relabel.expect_non_asap(), NonASAPOp::PromqlRelabel { dst, .. } if dst == "host")
    );
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum { .. })));
    let sch = qe.schema.clone();
    assert!(sch.fields.iter().any(|c| c.name == "host"));
}

// ─────────────────────────────────────────────────────────────────────────────
// U. Long-tail: extra range reducers + the sort family     (functions.test; #51)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn extra_over_time_reducers_lower_to_per_series_intents() {
    // SEMANTICS: each is a per-series reduction of one series' range window to a
    // single value — a `TimeRange`-wrapped `Aggregate` with the matching intent.
    for (q, want) in [
        ("last_over_time(m[5m])", AggIntent::LastOverTime),
        ("first_over_time(m[5m])", AggIntent::FirstOverTime),
        ("mad_over_time(m[5m])", AggIntent::MadOverTime),
        ("ts_of_min_over_time(m[5m])", AggIntent::TsOfMinOverTime),
        ("ts_of_max_over_time(m[5m])", AggIntent::TsOfMaxOverTime),
        ("ts_of_first_over_time(m[5m])", AggIntent::TsOfFirstOverTime),
        ("ts_of_last_over_time(m[5m])", AggIntent::TsOfLastOverTime),
    ] {
        let qe = ok(q);
        assert!(has(&qe, |i| *i == want), "{q}: {:?}", intents(&qe));
        // Per-series: the range window survives as a `TimeRange`.
        assert!(
            matches!(qe.expect_non_asap(), NonASAPOp::Aggregate { child, .. } if matches!(child.expect_non_asap(), NonASAPOp::TimeRange { .. })),
            "{q} keeps its range as a TimeRange"
        );
    }
}

#[test]
fn last_over_time_composes_under_an_outer_aggregation() {
    // `sum by (job) (last_over_time(m[5m]))` — per-series last, THEN cross-series
    // sum. Both intents survive (issue #27's arbitrary nesting).
    let qe = ok("sum by (job) (last_over_time(m[5m]))");
    assert!(has(&qe, |i| *i == AggIntent::LastOverTime));
    assert!(has(&qe, |i| matches!(i, AggIntent::Sum { .. })));
}

#[test]
fn sort_and_sort_desc_reorder_by_value_without_a_limit() {
    // SEMANTICS: `sort`/`sort_desc` reorder an instant vector by sample value.
    // Row-preserving → a bare `Sort` (no `Limit`), ascending / descending.
    for (q, ascending) in [
        ("sort(http_requests)", true),
        ("sort_desc(http_requests)", false),
    ] {
        let qe = ok(q);
        let NonASAPOp::Sort { keys, child, .. } = qe.expect_non_asap() else {
            panic!("{q}: expected a Sort, got {qe:?}");
        };
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ascending, ascending, "{q}");
        // No Limit above the Sort — every series is preserved.
        assert!(!matches!(qe.expect_non_asap(), NonASAPOp::Limit { .. }));
        // The value column is what it ranks on: descend to the scan.
        let (metric, _) = first_scan(child);
        assert_eq!(metric, "http_requests");
    }
}

#[test]
fn sort_by_label_orders_on_each_label_in_turn() {
    // `sort_by_label(v, "group", "instance", "job")` — one ascending sort key per
    // label, in argument order; the labels are seeded into the schema.
    let qe = ok(r#"sort_by_label(http_requests, "group", "instance", "job")"#);
    let NonASAPOp::Sort { keys, .. } = qe.expect_non_asap() else {
        panic!("expected a Sort, got {qe:?}");
    };
    assert_eq!(keys.len(), 3, "one key per label");
    assert!(keys.iter().all(|k| k.ascending));
    let sch = qe.schema.clone();
    for label in ["group", "instance", "job"] {
        assert!(sch.fields.iter().any(|c| c.name == label), "{label} seeded");
    }
}

#[test]
fn sort_by_label_desc_is_descending() {
    let qe = ok(r#"sort_by_label_desc(http_requests, "instance")"#);
    let NonASAPOp::Sort { keys, .. } = qe.expect_non_asap() else {
        panic!("expected a Sort, got {qe:?}");
    };
    assert!(keys.iter().all(|k| !k.ascending));
}

#[test]
fn min_of_max_of_fold_constant_scalars() {
    // `min_of`/`max_of` are n-ary scalar reducers. When every argument is a
    // constant they constant-fold to a `ScalarExpr` leaf, just like scalar
    // arithmetic (#35) — the only form the intent algebra can hold (#89).
    assert_eq!(
        promql_scalar(&support::scalar_root("min_of(3, 5)")),
        Some(3.0)
    );
    assert_eq!(
        promql_scalar(&support::scalar_root("max_of(3, 5)")),
        Some(5.0)
    );
    assert_eq!(
        promql_scalar(&support::scalar_root("min_of(-2, -5)")),
        Some(-5.0)
    );
    // Nested folds and use as a threshold operand.
    assert_eq!(
        promql_scalar(&support::scalar_root("max_of(min_of(2, 3), 10)")),
        Some(10.0)
    );
    let qe = ok("up > max_of(1, 2)");
    let ScalarExpr::Compare { right: rhs, .. } = support::sample_expression(&qe) else {
        panic!("{qe:?}")
    };
    assert_eq!(promql_scalar(rhs), Some(2.0));
}

#[test]
fn min_of_max_of_ignore_nan_like_the_min_max_aggregators() {
    // A NaN argument is skipped (Prometheus `min`/`max` NaN semantics).
    assert_eq!(
        promql_scalar(&support::scalar_root("max_of(3, NaN)")),
        Some(3.0)
    );
    assert_eq!(
        promql_scalar(&support::scalar_root("min_of(NaN, 3)")),
        Some(3.0)
    );
}

#[test]
fn non_constant_min_of_max_of_is_rejected__GAP() {
    // A dynamic argument (`step()` — itself unsupported, #89) can't be folded to
    // a constant and there is no scalar min/max node, so it stays rejected
    // rather than mislowered. These forms also only appear inside unsupported
    // dynamic range / offset positions in the corpus.
    let _ = rejected("min_of(step(), 1s)");
    let _ = rejected("max_of(min_of(step() + 1, 1h), 1ms)");
}
