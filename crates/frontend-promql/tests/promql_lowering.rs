//! End-to-end tests for PromQL → unresolved → canonical DAG lowering.

use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::{AggIntent, BinaryOpKind, Reduction, Source};
use asap_types::ir::scalar::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::{
    BinaryOperator, ExprSemantics, NonASAPOp, OperatorNode, ScalarExpr, TimeRangeKind,
};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
    Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, TimeSelection,
};

use asap_frontend_promql::{lower_promql_workload, PromqlError as LoweringError};
mod support;
use support::lower_promql;

fn lower(q: &str) -> Rc<OperatorNode> {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

#[test]
fn frequency_extensions_lower_to_explicit_frequency_statistics() {
    // ProjectASAP extensions reduce a frequency vector; numeric sample norms
    // have different semantics and must never be silently aliased here.
    for (query, expected) in [
        (
            "entropy_over_time(cpu_usage[5m])",
            AggIntent::FrequencyEntropy {
                col: None,
                accuracy: AccuracyTarget::Exact,
            },
        ),
        (
            "l2_over_time(cpu_usage[5m])",
            AggIntent::FrequencyL2 {
                col: None,
                accuracy: AccuracyTarget::Exact,
            },
        ),
    ] {
        assert!(all_intents(&lower(query))
            .iter()
            .any(|intent| std::mem::discriminant(intent) == std::mem::discriminant(&expected)));
    }
}

#[test]
fn distinct_over_time_preserves_cardinality_accuracy_and_nested_windows() {
    // Distinct counts sample values, not samples or series; all lowering routes
    // retain the caller's accuracy requirement, including subquery arguments.
    for query in [
        "distinct_over_time(cpu_usage{job=\"worker\"}[5m] offset 1h)",
        "distinct_over_time((cpu_usage + 1)[5m:1m])",
        "sum by(job)(distinct_over_time(cpu_usage[5m]))",
    ] {
        for accuracy in [AccuracyTarget::Exact, AccuracyTarget::Epsilon(0.02)] {
            let dag = lower_promql(query, accuracy.clone()).unwrap();
            let mut intents = Vec::new();
            collect_intents(&dag, &mut intents);
            assert!(
                intents.iter().any(|intent| matches!(
                    intent, AggIntent::Cardinality { accuracy: actual, .. } if actual == &accuracy
                )),
                "{query}: {dag:?}"
            );
            assert!(!intents
                .iter()
                .any(|intent| matches!(intent, AggIntent::Count { .. })));
        }
    }
}

// ── Bare selectors & label matchers (folded onto Scan.predicates) ───────────────

#[test]
fn bare_selector_is_scan_with_predicates() {
    let qe = lower(r#"http_requests_total{env="prod",status!="500"}"#);
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected TimeRange, got {qe:?}");
    };
    let NonASAPOp::Scan {
        source, predicates, ..
    } = child.expect_non_asap()
    else {
        panic!("expected Scan, got {qe:?}");
    };
    assert!(matches!(source, Source::TimeSeries { metric } if metric == "http_requests_total"));
    // The converter splits the matcher conjunction into one predicate per
    // conjunct on the Scan.
    assert_eq!(predicates.len(), 2);
    assert!(predicates
        .iter()
        .all(|p| matches!(&p.0, ScalarExpr::Compare { .. })));
}

#[test]
fn regex_matcher_lowers_to_regex_compareop() {
    let qe = lower(r#"http_requests_total{path=~"/api/.*"}"#);
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected TimeRange, got {qe:?}");
    };
    let NonASAPOp::Scan {
        predicates, schema, ..
    } = child.expect_non_asap()
    else {
        panic!("expected Scan, got {qe:?}");
    };
    let ScalarExpr::Compare {
        left, op, right, ..
    } = &predicates[0].0
    else {
        panic!("expected Compare, got {:?}", predicates[0].0);
    };
    assert_eq!(*op, CompareOpKind::Regex);
    // The label matcher's column is resolved positionally against the scan schema.
    let path_id = schema.column_id("path").expect("path in scan schema");
    assert!(matches!(left.as_ref(), ScalarExpr::Column(id) if *id == path_id));
    assert!(matches!(right.as_ref(), ScalarExpr::Literal(ScalarValue::Utf8(v)) if v == "/api/.*"));
}

// ── *_over_time → Aggregate over TimeRange ──────────────────────────────────────

#[test]
fn quantile_over_time_is_time_range_aggregate() {
    let qe = lower(r#"quantile_over_time(0.99, http_request_duration{env="prod"}[5m])"#);
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::PerEntity);
    assert!(
        matches!(measures.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.99).abs() < 1e-9)
    );
    let NonASAPOp::TimeRange { range, child, .. } = child.expect_non_asap() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
    // The label matcher folded onto the Scan.
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::Scan { predicates, .. } if predicates.len() == 1)
    );
}

#[test]
fn outer_sum_by_over_quantile_over_time_groups_positionally() {
    // `sum by (host) (quantile_over_time(...))`: inner per-series
    // quantile-over-time (label-preserving), then an outer cross-series sum
    // grouped on a positional `Aggregate.by` — the same shape SQL produces, not
    // a name-based Partition. Leaf = [ts, value, host, service] (referenced
    // names appended sorted) → host = col 2.
    let qe = lower(r#"sum by (host) (quantile_over_time(0.99, latency{service="web"}[5m]))"#);
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate grouped by host, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    // Inner: Aggregate{Quantile} over TimeRange (per-series over_time reduction).
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected Aggregate (quantile_over_time) under the outer Sum, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Quantile { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

#[test]
fn avg_over_time_maps_to_avg_intent() {
    let qe = lower("avg_over_time(cpu_seconds_total[10m])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Avg { .. }]));
    let NonASAPOp::TimeRange { range, .. } = child.expect_non_asap() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(600));
}

#[test]
fn stddev_and_stdvar_over_time() {
    let qe = lower("stddev_over_time(m[5m])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate");
    };
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::StdDev {
            population: true,
            ..
        }]
    ));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));

    let qe = lower("stdvar_over_time(m[5m])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate");
    };
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::Variance {
            population: true,
            ..
        }]
    ));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

#[test]
fn histogram_quantile_wraps_inner_in_quantile() {
    // The argument's structure (here `rate`) is preserved *under* the quantile,
    // not squashed away. The `_bucket` metric + `le` matcher mark the classic
    // form → `HistogramQuantile` over `Aggregate{Rate}` over Scan.
    let qe = lower(r#"histogram_quantile(0.95, rate(http_duration_seconds_bucket{le="0.5"}[5m]))"#);
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate{{HistogramQuantile}}, got {qe:?}");
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::HistogramQuantile { q, .. }] if (*q - 0.95).abs() < 1e-9)
    );
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected inner Aggregate{{Rate}}, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let NonASAPOp::TimeRange {
        range,
        child: tr_child,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected TimeRange under Rate, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
    assert!(
        matches!(tr_child.expect_non_asap(), NonASAPOp::Scan { predicates, .. } if predicates.len() == 1)
    );
}

#[test]
fn histogram_quantile_over_sum_by_le_preserves_grouping() {
    // The canonical Prometheus histogram pattern. Previously returned
    // UnsupportedFeature because `extract_matrix` couldn't see through the
    // `sum by (le)` aggregate; now the `le` grouping survives into the
    // canonical DAG.
    let qe = lower(r#"histogram_quantile(0.99, sum by (le) (rate(http_requests_bucket[5m])))"#);
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate{{HistogramQuantile}}, got {qe:?}");
    };
    // The `by (le)` grouping marks the classic cumulative-bucket form.
    assert!(
        matches!(measures.as_slice(), [AggIntent::HistogramQuantile { q, .. }] if (*q - 0.99).abs() < 1e-9)
    );
    // `sum by (le)` survives as a positional Aggregate (by = [2], `le`) over the
    // inner Rate — no name-based Partition.
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected `sum by (le)` as a positional Aggregate, got {child:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
}

/// The classic `histogram_quantile` aggregate: its `without` keys, `le`
/// column, and output column names.
fn classic_histogram(qe: &OperatorNode) -> (Vec<usize>, usize, Vec<String>) {
    let NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        measures,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected a reducing Aggregate, got {qe:?}");
    };
    let [AggIntent::HistogramQuantile { le, .. }] = measures.as_slice() else {
        panic!("expected HistogramQuantile, got {measures:?}");
    };
    assert!(by.is_without(), "histogram_quantile groups without (le)");
    let names = qe.schema.fields.iter().map(|c| c.name.clone()).collect();
    (by.keys().to_vec(), *le, names)
}

// A classic histogram_quantile groups `without (le)` and names the child's
// `le` column, even when no matcher or grouping mentions `le`.
#[test]
fn classic_histogram_quantile_groups_without_le() {
    let qe = lower("histogram_quantile(0.9, rate(http_duration_seconds_bucket[5m]))");
    let (keys, le, names) = classic_histogram(&qe);
    let NonASAPOp::Aggregate { child, .. } = qe.expect_non_asap() else {
        unreachable!()
    };
    let child = &child.schema;
    assert_eq!(child.fields[le].name, "le");
    assert_eq!(keys, vec![le]);
    assert_eq!(names, vec!["histogram_quantile"]);
}

// An explicit `sum by (le, job)` argument keeps `job` and drops `le` and the
// renamed sample value from the output labels.
#[test]
fn classic_histogram_quantile_over_sum_by_keeps_other_labels() {
    let qe =
        lower("histogram_quantile(0.9, sum by (le, job) (rate(http_duration_seconds_bucket[5m])))");
    let (keys, le, names) = classic_histogram(&qe);
    // `sum by (le, job)` outputs `[job, le, sum]`.
    assert_eq!((keys, le), (vec![1], 1));
    assert_eq!(names, vec!["job", "histogram_quantile"]);
}

// Out-of-range and NaN quantiles lower unchanged; execution returns -Inf/+Inf/NaN.
#[test]
fn classic_histogram_quantile_keeps_out_of_range_quantiles() {
    for (query, expected) in [
        ("histogram_quantile(-1, x_bucket)", -1.),
        ("histogram_quantile(2, x_bucket)", 2.),
    ] {
        let root = lower(query);
        let NonASAPOp::Aggregate { measures, .. } = root.expect_non_asap() else {
            panic!("{query}");
        };
        assert!(
            matches!(measures.as_slice(), [AggIntent::HistogramQuantile { q, .. }] if *q == expected)
        );
    }
    let root = lower("histogram_quantile(NaN, x_bucket)");
    let NonASAPOp::Aggregate { measures, .. } = root.expect_non_asap() else {
        panic!("NaN");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::HistogramQuantile { q, .. }] if q.is_nan()));
}

// An argument whose closed output lacks `le` has no buckets. Prometheus
// returns an empty vector; lowering rejects it rather than guess a column.
#[test]
fn classic_histogram_quantile_rejects_an_argument_without_le() {
    let error = lower_promql(
        "histogram_quantile(0.9, sum by (job) (rate(x_bucket[5m])))",
        AccuracyTarget::Exact,
    )
    .unwrap_err();
    assert!(error.to_string().contains("le"), "{error}");
}

// ── rate / increase carry their own window (no Window node) ─────────────────────

#[test]
fn rate_has_time_range_child_not_window() {
    let qe = lower("rate(http_requests_total[5m])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate for rate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let NonASAPOp::TimeRange { range, .. } = child.expect_non_asap() else {
        panic!("expected TimeRange child (not Window), got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
}

#[test]
fn increase_maps_to_increase_intent() {
    let qe = lower("increase(errors_total[1h])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate for increase, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Increase]));
    let NonASAPOp::TimeRange { range, .. } = child.expect_non_asap() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(3600));
}

// ── outer aggregation over an inner range-vector func is two levels ─────────────

#[test]
fn sum_over_rate_keeps_both_levels() {
    // Regression: `sum(rate(m[w]))` — the most common PromQL shape — must keep
    // the cross-series Sum, not collapse to a bare per-series Rate.
    let qe = lower("sum(rate(http_requests_total[5m]))");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate{{Sum}}, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected inner Aggregate{{Rate}}, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

#[test]
fn sum_by_over_rate_groups_the_outer_sum() {
    // `sum by (job) (rate(...))`: the grouping belongs to the OUTER sum and lands
    // on a positional `Aggregate.by` (the same shape SQL produces) over the
    // label-preserving inner Rate. Leaf = [ts, value, job] → by = [2].
    let qe = lower("sum by (job) (rate(http_requests_total[5m]))");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate grouped by job, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate])
    ));
}

#[test]
fn count_over_rate_keeps_both_levels() {
    // The `Outer::Count` sibling of the `sum(rate(...))` bug.
    let qe = lower("count(rate(http_requests_total[5m]))");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate{{Count}}, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate])
    ));
}

#[test]
fn count_over_distinct_over_time_preserves_both_aggregates() {
    // One series with window samples [1, 2] produces one distinct-count
    // result (value 2). The outer count counts that one series, yielding 1.
    for (query, reduction) in [
        (
            "count(distinct_over_time(unique_users[5m]))",
            Reduction::by(vec![]),
        ),
        (
            "count by (job) (distinct_over_time(unique_users[5m]))",
            Reduction::by(vec![2]),
        ),
    ] {
        let dag = lower(query);
        let NonASAPOp::Aggregate {
            measures,
            reduction: actual,
            child,
            ..
        } = dag.expect_non_asap()
        else {
            panic!("expected outer Count: {dag:?}");
        };
        assert!(
            matches!(measures.as_slice(), [AggIntent::Count { .. }]),
            "{query}: {dag:?}"
        );
        assert_eq!(actual, &reduction, "{query}");
        let NonASAPOp::Aggregate {
            measures,
            reduction,
            child,
            ..
        } = child.expect_non_asap()
        else {
            panic!("expected inner per-series Cardinality: {dag:?}");
        };
        assert!(
            matches!(measures.as_slice(), [AggIntent::Cardinality { .. }]),
            "{query}: {dag:?}"
        );
        assert_eq!(reduction, &Reduction::PerEntity, "{query}");
        assert!(
            matches!(child.expect_non_asap(), NonASAPOp::TimeRange { range, .. } if range.as_secs() == 300)
        );
    }
}

// ── count / cardinality ───────────────────────────────────────────────────────

// Both selector fast paths and recursive vector expressions count rows, not values.
#[test]
fn count_never_lowers_to_distinct_sample_values() {
    for query in [
        "count(up)",
        "count by (job) (up)",
        "count without (instance) (up)",
        "count(up + 1)",
        "count(count_over_time(up[5m]))",
        "count_over_time(up[5m])",
    ] {
        let dag = lower(query);
        let intents = all_intents(&dag);
        assert!(
            intents.iter().any(|i| matches!(i, AggIntent::Count { .. })),
            "{query}: {dag:?}"
        );
        assert!(
            !intents
                .iter()
                .any(|i| matches!(i, AggIntent::Cardinality { .. })),
            "{query}: {dag:?}"
        );
    }
}

#[test]
fn count_over_time_is_count_intent() {
    let qe = lower("count_over_time(m[5m])");
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

#[test]
fn outer_count_counts_series() {
    // `count by (symbol) (count_over_time(...))`: inner per-series sample count
    // over the window (label-preserving), outer cross-series row count grouped
    // on a positional `Aggregate.by`. Leaf = [ts, value, symbol] → symbol = col 2.
    let qe = lower("count by (symbol) (count_over_time(financial_last_trade_price[5m]))");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected outer Aggregate grouped by symbol, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    // Inner: Aggregate{Count} over TimeRange (per-series count_over_time).
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected Aggregate (count_over_time) under the outer count, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

// ── topk / bottomk ────────────────────────────────────────────────────────────

#[test]
fn topk_over_count_is_heavy_hitter_topk() {
    let qe = lower(r#"topk by (service) (10, count_over_time(requests{env="prod"}[1m]))"#);
    // Heavy-hitter: Aggregate{TopK} with grouping resolved to positional ids.
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate with TopK, got {qe:?}");
    };
    // `service` is the only group key → resolved to a positional ColumnId.
    assert_eq!(reduction.expect_reduce().len(), 1);
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::TopK { k: 10, .. }]
    ));
    // The count_over_time under the TopK is a TimeRange-backed aggregate.
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected Aggregate (count_over_time) under TopK, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    let NonASAPOp::TimeRange { range, child, .. } = child.expect_non_asap() else {
        panic!("expected TimeRange under Count aggregate, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(60));
    assert!(matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }));
}

#[test]
fn topk_over_sum_is_value_weighted_heavy_hitter_topk() {
    let qe = lower(r#"topk by (service) (5, sum_over_time(requests{env="prod"}[1m]))"#);
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate with TopK, got {qe:?}");
    };
    assert_eq!(reduction.expect_reduce().len(), 1);
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::TopK { k: 5, .. }]
    ));
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected Aggregate (sum_over_time) under TopK, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

#[test]
fn topk_over_avg_is_generic_sort_limit() {
    let qe = lower("topk by (host) (5, avg_over_time(cpu[5m]))");
    let NonASAPOp::Limit {
        n: Some(n),
        offset,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 5);
    assert_eq!(*offset, 0);
    let NonASAPOp::Sort {
        keys,
        partition_by,
        child,
    } = child.expect_non_asap()
    else {
        panic!("expected Sort under Limit, got {child:?}");
    };
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].ascending, "topk ranks descending");
    // `by (host)` is per-group ranking → it rides on `Sort.partition_by`
    // (positional), not a `Partition` node (issue #12). `host` is col 2 in
    // the per-series avg schema [ts, value, host].
    assert_eq!(partition_by, &vec![2]);
    // Underneath: the label-preserving windowed avg aggregate (by: []), no
    // intervening Partition.
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::Aggregate { reduction, measures, .. }
            if reduction == &Reduction::PerEntity && matches!(measures.as_slice(), [AggIntent::Avg { .. }])),
        "expected bare per-series Avg aggregate under Sort, got {child:?}"
    );
}

#[test]
fn ungrouped_topk_over_sum_is_heavy_hitter() {
    let qe = lower("topk(5, sum_over_time(m[5m]))");
    assert!(matches!(qe.expect_non_asap(), NonASAPOp::Aggregate { .. }));
    assert!(has_intent(&qe, |i| matches!(i, AggIntent::Sum { .. })));
    assert!(has_intent(&qe, |i| matches!(
        i,
        AggIntent::TopK { k: 5, .. }
    )));
}

#[test]
fn bottomk_over_count_is_generic_sort_ascending() {
    // `bottomk` is never a heavy-hitter (descending=false), even over count.
    let qe = lower("bottomk(3, count_over_time(m[5m]))");
    let NonASAPOp::Limit {
        n: Some(n), child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 3);
    let NonASAPOp::Sort { keys, .. } = child.expect_non_asap() else {
        panic!("expected Sort");
    };
    assert!(keys[0].ascending, "bottomk ranks ascending");
    // Count intent is still present (as the inner aggregate), no TopK.
    assert!(has_intent(&qe, |i| matches!(i, AggIntent::Count { .. })));
    assert!(!has_intent(&qe, |i| matches!(i, AggIntent::TopK { .. })));
}

#[test]
fn bottomk_is_always_generic_sort_ascending() {
    let qe = lower("bottomk(3, count_over_time(m[5m]))");
    let NonASAPOp::Limit {
        n: Some(n), child, ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 3);
    let NonASAPOp::Sort { keys, .. } = child.expect_non_asap() else {
        panic!("expected Sort");
    };
    assert!(keys[0].ascending, "bottomk ranks ascending");
}

#[test]
fn topk_count_output_schema_carries_group_key() {
    // The inner Count is per-series (label-preserving), so the group-by key
    // (`service`) flows through to the outer TopK's `by` column. Leaf schema =
    // [ts, value, service] → TopK groups on service (col 2).
    let qe = lower("topk by (service) (5, count_over_time(m[1m]))");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected Aggregate{{TopK}}, got {qe:?}");
    };
    assert_eq!(
        reduction,
        &Reduction::by(vec![2]),
        "service is col 2 in [ts, value, service]"
    );
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::TopK { k: 5, .. }]
    ));
    // Inner Count aggregate is visible with its TimeRange child.
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected inner Aggregate{{Count}}, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::TimeRange { .. }
    ));
}

// ── binary ops ────────────────────────────────────────────────────────────────

#[test]
fn binary_op_division() {
    let qe = lower("rate(a[5m]) / rate(b[5m])");
    let NonASAPOp::BinaryOp {
        operator: BinaryOperator { kind: op, .. },
        lhs,
        rhs,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Arithmetic(ArithmeticOpKind::Div));
    assert!(
        matches!(lhs.expect_non_asap(), NonASAPOp::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate]))
    );
    assert!(
        matches!(rhs.expect_non_asap(), NonASAPOp::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate]))
    );
}

#[test]
fn binary_op_with_on_grouping() {
    let qe = lower("a / on(host) b");
    let NonASAPOp::BinaryOp {
        operator: BinaryOperator { vector_match, .. },
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    let vm = vector_match.as_ref().expect("vector_match present");
    use asap_types::ir::operator::VectorMatchKind;
    assert_eq!(vm.kind, VectorMatchKind::On);
    assert_eq!(vm.labels, vec!["host".to_string()]);
}

// `bool` changes a comparison from a filter to a 0/1 result, so the IR must
// carry it.
#[test]
fn bool_comparisons_are_distinct() {
    let op = |q: &str| match lower(q).expect_non_asap() {
        NonASAPOp::BinaryOp {
            operator,
            return_bool,
            ..
        } => (operator.kind.clone(), *return_bool),
        NonASAPOp::Filter {
            pred: asap_types::ir::Predicate(ScalarExpr::Compare { op, .. }),
            ..
        } => (BinaryOpKind::Compare(op.clone()), false),
        NonASAPOp::Project { cols, .. } => {
            let ScalarExpr::Case { branches, .. } = &cols[1].expr else {
                panic!()
            };
            let ScalarExpr::Compare { op, .. } = &branches[0].0 else {
                panic!()
            };
            (BinaryOpKind::Compare(op.clone()), true)
        }
        other => panic!("expected BinaryOp, got {other:?}"),
    };
    assert_eq!(
        op("a > 1"),
        (BinaryOpKind::Compare(CompareOpKind::Gt), false)
    );
    assert_eq!(
        op("a > bool 1"),
        (BinaryOpKind::Compare(CompareOpKind::Gt), true)
    );
    assert_eq!(
        op("a == bool on(job) b"),
        (BinaryOpKind::Compare(CompareOpKind::Eq), true)
    );
}

#[test]
fn binary_op_binds_each_branch_against_its_own_schema() {
    // Each side scans a different metric and groups by a different label. With a
    // single root schema threaded to both branches, the left scan would leak the
    // right's group key (and vice-versa). Per-branch binding keeps them separate.
    let qe = lower("count by (job) (a) / count by (region) (b)");
    let NonASAPOp::BinaryOp { lhs, rhs, .. } = qe.expect_non_asap() else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    let lcols = scan_columns(lhs);
    let rcols = scan_columns(rhs);
    assert!(
        lcols.iter().any(|c| c == "job") && !lcols.iter().any(|c| c == "region"),
        "lhs scan schema leaked the rhs key: {lcols:?}"
    );
    assert!(
        rcols.iter().any(|c| c == "region") && !rcols.iter().any(|c| c == "job"),
        "rhs scan schema leaked the lhs key: {rcols:?}"
    );
}

/// Collect every `AggIntent` in the dag, root-to-leaf.
fn all_intents(e: &OperatorNode) -> Vec<AggIntent> {
    let mut out = Vec::new();
    collect_intents(e, &mut out);
    out
}

fn collect_intents(e: &OperatorNode, out: &mut Vec<AggIntent>) {
    match e.expect_non_asap() {
        NonASAPOp::Aggregate {
            measures, child, ..
        } => {
            out.extend(measures.iter().cloned());
            collect_intents(child, out);
        }
        NonASAPOp::TimeRange { child, .. }
        | NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. } => collect_intents(child, out),
        NonASAPOp::BinaryOp { lhs, rhs, .. } => {
            collect_intents(lhs, out);
            collect_intents(rhs, out);
        }
        _ => {}
    }
}

/// True if any `AggIntent` anywhere in the dag satisfies `pred`.
fn has_intent<F: Fn(&AggIntent) -> bool>(e: &OperatorNode, pred: F) -> bool {
    all_intents(e).iter().any(pred)
}

/// Field names on the first `Scan` reachable by descending single-child nodes.
fn scan_columns(e: &OperatorNode) -> Vec<String> {
    match e.expect_non_asap() {
        NonASAPOp::Scan { schema, .. } => schema.fields.iter().map(|c| c.name.clone()).collect(),
        NonASAPOp::Aggregate { child, .. }
        | NonASAPOp::TimeRange { child, .. }
        | NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. } => scan_columns(child),
        _ => vec![],
    }
}

// ── without(...) grouping (issue #39) ───────────────────────────────────────────

#[test]
fn without_grouping_lowers_to_the_exclusion_form() {
    // `sum without (instance) (rate(m[5m]))` — a cross-series reduction over the
    // per-series rate, grouped by every label except `instance`. The excluded
    // label is stored positionally (the SchemaResolver seeds it), the grouping is the
    // `without` form, and the output schema stays open.
    let qe = lower("sum without (instance) (rate(m[5m]))");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected an Aggregate, got {qe:?}");
    };
    let by = reduction.expect_reduce();
    assert!(by.is_without());
    assert_eq!(by.keys().len(), 1, "excluded `instance`");
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    // The inner per-series rate is preserved (label-preserving) under the outer
    // cross-series `without` reduction.
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::Aggregate { measures, .. }
        if matches!(measures.as_slice(), [AggIntent::Rate]))
    );
    assert!(!qe.schema.clone().closed);
}

// ── parameter validation (reject rather than silently truncate/garble) ──────────

#[test]
fn fractional_or_negative_topk_k_is_rejected() {
    // `as u64` would silently truncate 2.7→2 / saturate -1→0.
    assert!(lower_promql("topk(2.7, count_over_time(m[1m]))", AccuracyTarget::Exact).is_err());
    assert!(lower_promql("bottomk(2.5, sum_over_time(m[1m]))", AccuracyTarget::Exact).is_err());
}

#[test]
fn out_of_range_quantile_phi_is_accepted() {
    // Prometheus defines out-of-range phi results; lowering must preserve it.
    for query in [
        "quantile(1.5, up)",
        "quantile_over_time(1.5, m[5m])",
        "histogram_quantile(2.0, rate(b_bucket[5m]))",
    ] {
        assert!(
            lower_promql(query, AccuracyTarget::Exact).is_ok(),
            "{query}"
        );
    }
}

#[test]
fn function_wrapped_range_vector_is_rejected_not_stripped() {
    // `rate(abs(m[5m]))` must NOT silently lower as `rate(m[5m])` — the wrapper
    // is rejected (here, at parse or in extract_matrix), never stripped.
    assert!(
        lower_promql("rate(abs(http_requests_total[5m]))", AccuracyTarget::Exact).is_err(),
        "function-wrapped range vector should be rejected"
    );
}

#[test]
fn pathologically_nested_query_is_rejected_not_stack_overflow() {
    // 300 nested parens parse fine but exceed the walker's depth limit (256);
    // it must return an error, not overflow the stack.
    let q = format!("{}m{}", "(".repeat(300), ")".repeat(300));
    let err = lower_promql(&q, AccuracyTarget::Exact).unwrap_err();
    assert!(format!("{err}").contains("nesting"), "got {err}");
}

// Behavior: every parser-accepted `fill` modifier form is rejected with a
// fill-specific lowering error rather than silently dropped.
#[test]
fn fill_modifiers_are_rejected_not_ignored() {
    for q in [
        "a + fill(0) b",
        "a + fill_left(1) b",
        "a + fill_right(2) b",
        "a + fill_left(1) fill_right(2) b",
        "a + fill_right(2) fill_left(1) b",
        "a + on(job) fill(0) b",
        "a * ignoring(instance) group_left(env) fill_right(0) b",
        "a > bool on(job) fill(0) b",
        "sum(a - on(job) group_right fill_left(0) b)",
    ] {
        match lower_promql(q, AccuracyTarget::Exact) {
            Err(LoweringError::UnsupportedFeature(m)) if m.contains("`fill`") => {}
            other => panic!("expected fill rejection for {q:?}, got {other:?}"),
        }
    }
}

// ── accuracy propagation ──────────────────────────────────────────────────────

#[test]
fn accuracy_target_flows_into_quantile_intent() {
    let qe = lower_promql(
        "quantile_over_time(0.9, m[5m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let NonASAPOp::Aggregate { measures, .. } = qe.expect_non_asap() else {
        panic!("expected Aggregate");
    };
    assert!(matches!(
        &measures[0],
        AggIntent::Quantile { accuracy: AccuracyTarget::Epsilon(e), .. } if (*e - 0.01).abs() < 1e-12
    ));
}

// ── schema flow (positional, carried on Scan; derived on demand) ─────────────────

#[test]
fn aggregate_output_schema_preserves_time_axis_and_labels() {
    let qe = lower(r#"quantile_over_time(0.99, http_request_duration{env="prod"}[5m])"#);
    // Per-series reduction: the root is Aggregate { TimeRange { Scan } }.
    // The SchemaResolver adds all referenced label names (group keys AND filter
    // predicate columns) to the scan schema, so `env` appears as a column
    // even though it is only used as a filter.
    // per_series_reduction_schema preserves the time axis and all label columns.
    let NonASAPOp::Aggregate { .. } = qe.expect_non_asap() else {
        panic!("expected Aggregate, got {qe:?}");
    };
    let schema = &qe.schema;
    let names: Vec<&str> = schema.fields.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["ts", "value", "env"]);
    assert_eq!(
        schema.time_index,
        Some(0),
        "per-series over_time preserves the time axis"
    );
}

#[test]
fn scan_schema_carries_ts_value_and_group_keys() {
    // `service` is a group key → the SchemaResolver lands it in the self-contained
    // Scan schema (positional). `env` is only a filter, so it is not a column.
    let qe = lower("count by (service) (count_over_time(requests[1m]))");
    fn find_scan(n: &OperatorNode) -> &OperatorNode {
        match n.expect_non_asap() {
            NonASAPOp::Scan { .. } => n,
            NonASAPOp::TimeRange { child, .. }
            | NonASAPOp::Aggregate { child, .. }
            | NonASAPOp::Filter { child, .. } => find_scan(child),
            other => panic!("unexpected node {other:?}"),
        }
    }
    let NonASAPOp::Scan { schema, .. } = find_scan(&qe).expect_non_asap() else {
        unreachable!()
    };
    let mut names: Vec<&str> = schema.fields.iter().map(|c| c.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["service", "ts", "value"]);
    assert_eq!(schema.time_index, Some(0)); // ts
}

// ── batch entry point ─────────────────────────────────────────────────────────

#[test]
fn batch_lowers_each_entry_and_reads_per_query_accuracy() {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![
                BatchEntry {
                    query: Query("rate(a[5m])".into()),
                    requirements: QueryRequirements::default(),
                    predictability: Predictability::Unknown,
                    invocations: 1,
                    execute_at: None,
                    time_selection: TimeSelection::default(),
                },
                BatchEntry {
                    query: Query("quantile_over_time(0.9, b[5m])".into()),
                    requirements: QueryRequirements {
                        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(0.02)),
                        ..Default::default()
                    },
                    predictability: Predictability::Unknown,
                    invocations: 1,
                    execute_at: None,
                    time_selection: TimeSelection::default(),
                },
            ]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let results = lower_promql_workload(&workload, 0).expect("valid workload");
    assert_eq!(results.len(), 2);
}

#[test]
fn batch_rejects_non_promql_language() {
    use asap_types::workload::SqlDialect;
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(vec![BatchEntry {
                query: Query("SELECT 1".into()),
                requirements: QueryRequirements::default(),
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: None,
    };
    assert!(matches!(
        lower_promql_workload(&workload, 0),
        Err(LoweringError::WrongLanguage(_))
    ));
}

// ── #12: one home per grouping concept (the canonical `Partition` node is removed) ──
//
// `Partition` and `Aggregate.by` were two ways to express grouping. #12 collapses
// them: a reducing GROUP BY → `Aggregate.by`; per-group *ranking* (split without
// reduce) → `Sort.partition_by`; parallel sharding → a deployment's own
// physical stage. There is no longer a canonical `Partition` node. These
// tests pin both surviving canonical homes.

#[test]
fn reducing_group_by_lowers_to_aggregate_by() {
    // Cross-series reduce, no keys → bare `Aggregate { reduction: Reduce([]) }`.
    let q = lower("sum(http_requests_total)");
    assert!(
        matches!(q.expect_non_asap(), NonASAPOp::Aggregate { reduction, .. } if reduction == &Reduction::by(vec![]))
    );

    // Cross-series reduce grouped by a label → `Aggregate.reduction`.
    let q = lower("sum by (job) (http_requests_total)");
    assert!(
        matches!(q.expect_non_asap(), NonASAPOp::Aggregate { reduction, .. }
        if reduction.expect_reduce().len() == 1)
    );

    // Reduce over a label-preserving `rate` grouped by a label → still
    // `Aggregate.reduction` (the keys resolve against rate's preserved schema).
    let q = lower("sum by (job) (rate(http_requests_total[5m]))");
    assert!(
        matches!(q.expect_non_asap(), NonASAPOp::Aggregate { reduction, .. }
        if reduction.expect_reduce().len() == 1)
    );
}

#[test]
fn generic_topk_grouping_lowers_to_sort_partition_by() {
    // Per-group ranking (`topk by (host)`, non-heavy-hitter) groups *without*
    // reducing → the grouping rides on `Sort.partition_by`, and the windowed
    // reduction beneath stays label-preserving (`by: []`). No `Partition` node.
    let q = lower("topk by (host) (5, avg_over_time(cpu[5m]))");
    let NonASAPOp::Limit { child, .. } = q.expect_non_asap() else {
        panic!("expected Limit, got {q:?}");
    };
    let NonASAPOp::Sort {
        partition_by,
        child,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected Sort, got {child:?}");
    };
    assert_eq!(partition_by, &vec![2], "host is col 2 in [ts, value, host]");
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::Aggregate { reduction, .. } if reduction == &Reduction::PerEntity)
    );
}

#[test]
fn topk_over_bare_selector_by_label_ranks_per_group() {
    // `topk(3, http_requests_total) by (job)` — top-3 series per `job`. A bare
    // instant selector ranks its OWN samples; it must not be wrapped in an
    // implicit cross-series `Sum`, which would collapse the `job` partition
    // label before `Sort.partition_by` resolves it (issue #30 — follow-up to the
    // Partition→Sort.partition_by reframe in #12). Expected:
    //   Limit{3} → Sort{value desc, partition_by:[job]} → Scan
    let q = lower("topk(3, http_requests_total) by (job)");
    let NonASAPOp::Limit {
        n: Some(n), child, ..
    } = q.expect_non_asap()
    else {
        panic!("expected Limit, got {q:?}");
    };
    assert_eq!(*n, 3);
    let NonASAPOp::Sort {
        keys,
        partition_by,
        child,
    } = child.expect_non_asap()
    else {
        panic!("expected Sort, got {child:?}");
    };
    assert!(!keys[0].ascending, "topk ranks descending");
    assert_eq!(partition_by, &vec![2], "job is col 2 in [ts, value, job]");
    // No implicit reducing aggregate — the selector is label-preserving, so the
    // sort is directly over the selector horizon (the `job` label survives to partition by).
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::TimeRange { child, .. } if matches!(child.expect_non_asap(), NonASAPOp::Scan { .. })),
        "ranking is over the bare selector horizon, not a reducing Aggregate, got {child:?}"
    );
    assert!(
        !has_intent(&q, |i| matches!(i, AggIntent::Sum { .. })),
        "no implicit Sum is introduced over a bare selector"
    );
}

#[test]
fn topk_over_bare_selector_ranks_raw_samples() {
    // Even without `by`, `topk(3, m)` ranks the raw instant-vector samples — it
    // does not sum them. The sort sits directly over the Scan, partition empty.
    let q = lower("topk(3, http_requests_total)");
    let NonASAPOp::Limit { child, .. } = q.expect_non_asap() else {
        panic!("expected Limit, got {q:?}");
    };
    let NonASAPOp::Sort {
        partition_by,
        child,
        ..
    } = child.expect_non_asap()
    else {
        panic!("expected Sort, got {child:?}");
    };
    assert!(partition_by.is_empty(), "no `by` → global ranking");
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::TimeRange { child, .. } if matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }))
    );
    assert!(!has_intent(&q, |i| matches!(i, AggIntent::Sum { .. })));
}

// ── Issue #109: histogram_quantiles fans out into one branch per φ ──────────

/// The `(label value, intent)` of each `histogram_quantiles` branch.
fn quantile_branches(q: &OperatorNode) -> Vec<(String, AggIntent)> {
    let NonASAPOp::Concat { children, .. } = q.expect_non_asap() else {
        panic!("expected a Concat at the root, got {q:?}");
    };
    children
        .iter()
        .map(|c| {
            let NonASAPOp::PromqlRelabel { value, child, .. } = c.expect_non_asap() else {
                panic!("expected PromqlRelabel per branch, got {c:?}");
            };
            let ScalarExpr::Literal(ScalarValue::Utf8(v)) = value else {
                panic!("expected a literal label value, got {value:?}");
            };
            let NonASAPOp::Aggregate { measures, .. } = child.expect_non_asap() else {
                panic!("expected an Aggregate under the PromqlRelabel, got {child:?}");
            };
            (v.clone(), measures[0].clone())
        })
        .collect()
}

#[test]
fn histogram_quantiles_rejects_unrepresented_native_histograms() {
    assert!(lower_promql(
        r#"histogram_quantiles(testhistogram3, "q", 0, 0.25, 1)"#,
        AccuracyTarget::Exact
    )
    .is_err());
}

#[test]
fn histogram_quantiles_over_classic_buckets_interpolates() {
    // `_bucket` argument → exact cumulative-bucket interpolation, never a sketch.
    let q = lower(r#"histogram_quantiles(request_duration_seconds_bucket, "q", 0.5, 0.9)"#);
    for (_, intent) in quantile_branches(&q) {
        assert!(
            matches!(intent, AggIntent::HistogramQuantile { .. }),
            "classic buckets → HistogramQuantile, got {intent:?}"
        );
    }
}

#[test]
fn histogram_quantiles_branches_are_union_compatible() {
    // `Concat` derives its schema from the first child, so every branch must
    // agree on column names — the φ lives in the label, not the column name.
    let q = lower(r#"histogram_quantiles(testhistogram3_bucket, "q", 0.5, 0.9)"#);
    let NonASAPOp::Concat { children, .. } = q.expect_non_asap() else {
        panic!("expected Concat");
    };
    let shapes: Vec<Vec<String>> = children
        .iter()
        .map(|c| {
            c.schema
                .clone()
                .fields
                .iter()
                .map(|c| c.name.clone())
                .collect()
        })
        .collect();
    assert_eq!(shapes[0], shapes[1], "branches must be union-compatible");
    assert_eq!(shapes[0], vec!["value".to_string(), "q".to_string()]);
    assert_eq!(
        q.schema.fields.len(),
        2,
        "the merged schema describes every branch"
    );
}

#[test]
fn histogram_quantiles_uses_the_given_label_name() {
    let q = lower(r#"histogram_quantiles(h_bucket, "phi", 0.5)"#);
    let NonASAPOp::Concat { children, .. } = q.expect_non_asap() else {
        panic!("expected Concat");
    };
    let NonASAPOp::PromqlRelabel { dst, .. } = children[0].expect_non_asap() else {
        panic!("expected PromqlRelabel");
    };
    assert_eq!(dst, "phi");
}

#[test]
fn histogram_quantiles_formats_small_quantiles_like_prometheus() {
    // `labels.FormatOpenMetricsFloat`: Go's %g, so exponent form below 1e-4.
    let q = lower(r#"histogram_quantiles(h_bucket, "q", 0.00001)"#);
    assert_eq!(quantile_branches(&q)[0].0, "1e-05");
}

#[test]
fn histogram_quantiles_rejects_an_out_of_range_quantile() {
    // Same rule as `histogram_quantile(φ, …)` — one bad φ fails the whole call.
    for q in [
        r#"histogram_quantiles(h_bucket, "q", -0.1)"#,
        r#"histogram_quantiles(h_bucket, "q", 1.01)"#,
        r#"histogram_quantiles(h_bucket, "q", 0.5, NaN)"#,
    ] {
        assert!(
            lower_promql(q, AccuracyTarget::Exact).is_err(),
            "{q} should be rejected"
        );
    }
}

// ── TimeRange.kind: instant vs range selectors ──────────────────────────────────

#[test]
fn bare_instant_selector_is_an_instant_time_range() {
    // `up` reads the latest sample per series within the workload's ingestion
    // interval (1s in `support::workload`): an `Instant` lookback of that length.
    let qe = lower("up");
    let NonASAPOp::TimeRange { range, kind, child } = qe.expect_non_asap() else {
        panic!("expected TimeRange, got {qe:?}");
    };
    assert_eq!(*kind, TimeRangeKind::Instant);
    assert_eq!(*range, Duration::from_secs(1));
    assert!(matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }));
}

#[test]
fn explicit_range_selector_is_a_range_time_range() {
    // `m[5m]` keeps its own window and is a `Range` selection — both under a
    // range function and as a bare matrix selector.
    let qe = lower("rate(m[5m])");
    let NonASAPOp::Aggregate { child, .. } = qe.expect_non_asap() else {
        panic!("expected Aggregate, got {qe:?}");
    };
    let NonASAPOp::TimeRange { range, kind, .. } = child.expect_non_asap() else {
        panic!("expected TimeRange, got {child:?}");
    };
    assert_eq!(*kind, TimeRangeKind::Range);
    assert_eq!(*range, Duration::from_secs(300));

    let qe = lower("m[5m]");
    assert!(matches!(
        qe.expect_non_asap(),
        NonASAPOp::TimeRange {
            kind: TimeRangeKind::Range,
            ..
        }
    ));
}

#[test]
fn instant_and_range_selectors_of_equal_length_stay_distinct() {
    // The kind is part of the shape: a 1s range selector is not the same dag as
    // the 1s instant lookback injected around a bare selector.
    assert_ne!(lower("up"), lower("up[1s]"));
}

// ── the `bool` modifier → `return_bool` ─────────────────────────────────────────

#[test]
fn vector_scalar_comparison_without_bool_filters() {
    let qe = lower("up > 0");
    assert!(matches!(qe.expect_non_asap(), NonASAPOp::Filter { .. }));
    assert!(matches!(
        support::sample_expression(&qe),
        ScalarExpr::Compare {
            op: CompareOpKind::Gt,
            ..
        }
    ));
}

#[test]
fn vector_scalar_comparison_with_bool_sets_return_bool() {
    let qe = lower("up > bool 0");
    assert!(matches!(
        support::sample_expression(&qe),
        ScalarExpr::Case { .. }
    ));
    assert_ne!(qe, lower("up > 0"));
}

#[test]
fn vector_vector_comparison_with_bool_sets_return_bool() {
    // `a > bool b` — the modifier lands on the vector/vector op itself, with
    // the default (ignoring nothing) match.
    let qe = lower("a > bool b");
    let NonASAPOp::BinaryOp {
        operator,
        return_bool,
        lhs,
        rhs,
    } = qe.expect_non_asap()
    else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert!(*return_bool);
    assert_eq!(operator.kind, BinaryOpKind::Compare(CompareOpKind::Gt));
    assert!(matches!(lhs.expect_non_asap(), NonASAPOp::TimeRange { .. }));
    assert!(matches!(rhs.expect_non_asap(), NonASAPOp::TimeRange { .. }));
    assert!(!lower("a > b").expect_non_asap().children().is_empty());
    assert_ne!(qe, lower("a > b"));
}

#[test]
fn bool_modifier_composes_with_vector_matching() {
    let qe = lower("a > bool on(job) b");
    let NonASAPOp::BinaryOp {
        operator,
        return_bool,
        ..
    } = qe.expect_non_asap()
    else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert!(*return_bool);
    let vm = operator.vector_match.as_ref().expect("on(job) present");
    assert_eq!(vm.labels, vec!["job".to_string()]);
}

// ── scalar expressions: negation, arithmetic, comparison ────────────────────────

#[test]
fn scalar_negation_of_time_is_a_negative_expression() {
    // `-time()` is a scalar expression; its negation stays structural (the
    // operand is not a constant to fold) and follows PromQL numeric rules.
    let qe = support::scalar_root("-time()");
    let ScalarExpr::Negative { expr, semantics } = &qe else {
        panic!("expected ScalarExpr(Negative), got {qe:?}");
    };
    assert_eq!(*semantics, ExprSemantics::Promql);
    assert!(matches!(expr.as_ref(), ScalarExpr::EvalTimestamp));
    // Scalar-shaped: no time index.
}

#[test]
fn scalar_negation_of_a_constant_still_folds() {
    // `-(2)` is constant: it folds to one literal rather than a `Negative`.
    assert_eq!(
        support::promql_scalar(&support::scalar_root("-(2)")),
        Some(-2.0)
    );
}

#[test]
fn scalar_arithmetic_carries_promql_semantics() {
    let qe = support::scalar_root("time() - 1");
    let ScalarExpr::Arithmetic {
        op,
        left,
        right,
        semantics,
    } = &qe
    else {
        panic!("expected scalar(Arithmetic), got {qe:?}");
    };
    assert_eq!(*op, ArithmeticOpKind::Sub);
    assert_eq!(*semantics, ExprSemantics::Promql);
    assert!(matches!(left.as_ref(), ScalarExpr::EvalTimestamp));
    assert!(matches!(
        right.as_ref(),
        ScalarExpr::Literal(ScalarValue::Float64(v)) if *v == 1.0
    ));
}

#[test]
fn scalar_bool_comparison_is_a_zero_one_case_with_promql_semantics() {
    // `1 < bool 2` → `Case(Compare(1 < 2) → 1.0, else 0.0)`: PromQL yields 0/1.
    let qe = support::scalar_root("1 < bool 2");
    let ScalarExpr::Case {
        operand,
        branches,
        else_expr,
    } = &qe
    else {
        panic!("expected scalar(Case), got {qe:?}");
    };
    assert!(operand.is_none());
    let [(when, then)] = branches.as_slice() else {
        panic!("expected one branch, got {branches:?}");
    };
    let ScalarExpr::Compare {
        left,
        op,
        right,
        semantics,
    } = when
    else {
        panic!("expected a Compare condition, got {when:?}");
    };
    assert_eq!(*op, CompareOpKind::Lt);
    assert_eq!(*semantics, ExprSemantics::Promql);
    assert!(matches!(left.as_ref(), ScalarExpr::Literal(ScalarValue::Float64(v)) if *v == 1.0));
    assert!(matches!(right.as_ref(), ScalarExpr::Literal(ScalarValue::Float64(v)) if *v == 2.0));
    assert!(matches!(then, ScalarExpr::Literal(ScalarValue::Float64(v)) if *v == 1.0));
    assert!(matches!(
        else_expr.as_deref(),
        Some(ScalarExpr::Literal(ScalarValue::Float64(v))) if *v == 0.0
    ));
}

#[test]
fn scalar_comparison_without_bool_is_rejected() {
    // PromQL has no scalar filter: a scalar/scalar comparison needs `bool`.
    for q in ["1 < 2", "time() > 0", "(1 + 1) == 2"] {
        assert!(
            lower_promql(q, AccuracyTarget::Exact).is_err(),
            "{q} must be rejected without `bool`"
        );
    }
}

#[test]
fn label_matcher_predicates_carry_promql_semantics() {
    let qe = lower(r#"up{job="api"}"#);
    let NonASAPOp::TimeRange { child, .. } = qe.expect_non_asap() else {
        panic!("expected TimeRange, got {qe:?}");
    };
    let NonASAPOp::Scan { predicates, .. } = child.expect_non_asap() else {
        panic!("expected Scan, got {child:?}");
    };
    assert!(matches!(
        &predicates[0].0,
        ScalarExpr::Compare {
            semantics: ExprSemantics::Promql,
            ..
        }
    ));
}

// A subquery's `offset` / `@` modifier stays a `TimeShift` over the subquery.
#[test]
fn subquery_time_shift_is_retained() {
    let root = lower("max_over_time(m[5m:1m] offset 1m)");
    let NonASAPOp::Aggregate { child, .. } = root.expect_non_asap() else {
        panic!("expected a range function, got {root:?}");
    };
    let NonASAPOp::TimeShift { shift, child } = child.expect_non_asap() else {
        panic!("subquery offset was dropped: {child:?}");
    };
    assert_eq!(shift.offset_ms, 60_000);
    assert!(matches!(
        child.expect_non_asap(),
        NonASAPOp::PromqlSubquery { .. }
    ));
    let root = lower("max_over_time(m[5m:1m] @ 100)");
    assert!(matches!(
        root.expect_non_asap(),
        NonASAPOp::Aggregate { child, .. }
            if matches!(child.expect_non_asap(), NonASAPOp::TimeShift { .. })
    ));
}
