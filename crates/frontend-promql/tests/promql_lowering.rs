//! End-to-end tests for PromQL → unresolved → canonical tree lowering.

use std::time::Duration;

use asap_types::pre_asap::{
    AggIntent, ArithmeticOpKind, BinaryOpKind, CompareOpKind, QueryExpr, Reduction, ScalarValue,
    Source,
};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{BatchEntry, Query, QueryLanguage, QueryRequirements, QueryWorkload};

use asap_frontend_promql::{lower_promql, lower_promql_batch, PromqlError as LoweringError};

fn lower(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

// ── Bare selectors & label matchers (folded onto Scan.predicates) ───────────────

#[test]
fn bare_selector_is_scan_with_predicates() {
    let qe = lower(r#"http_requests_total{env="prod",status!="500"}"#);
    let QueryExpr::Scan {
        source, predicates, ..
    } = &qe
    else {
        panic!("expected Scan, got {qe:?}");
    };
    assert!(matches!(source, Source::TimeSeries { metric } if metric == "http_requests_total"));
    // The converter splits the matcher conjunction into one predicate per
    // conjunct on the Scan.
    assert_eq!(predicates.len(), 2);
    assert!(predicates
        .iter()
        .all(|p| matches!(p.0.as_ref(), QueryExpr::Compare { .. })));
}

#[test]
fn regex_matcher_lowers_to_regex_compareop() {
    let qe = lower(r#"http_requests_total{path=~"/api/.*"}"#);
    let QueryExpr::Scan {
        predicates, schema, ..
    } = &qe
    else {
        panic!("expected Scan, got {qe:?}");
    };
    let QueryExpr::Compare { left, op, right } = predicates[0].0.as_ref() else {
        panic!("expected Compare, got {:?}", predicates[0].0);
    };
    assert_eq!(*op, CompareOpKind::Regex);
    // The label matcher's column is resolved positionally against the scan schema.
    let path_id = schema.column_id("path").expect("path in scan schema");
    assert!(matches!(left.as_ref(), QueryExpr::Column(id) if *id == path_id));
    assert!(matches!(right.as_ref(), QueryExpr::Literal(ScalarValue::Utf8(v)) if v == "/api/.*"));
}

// ── *_over_time → Aggregate over TimeRange ──────────────────────────────────────

#[test]
fn quantile_over_time_is_time_range_aggregate() {
    let qe = lower(r#"quantile_over_time(0.99, http_request_duration{env="prod"}[5m])"#);
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &qe
    else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::PerEntity);
    assert!(
        matches!(measures.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.99).abs() < 1e-9)
    );
    let QueryExpr::TimeRange { range, child } = child.as_ref() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
    // The label matcher folded onto the Scan.
    assert!(matches!(child.as_ref(), QueryExpr::Scan { predicates, .. } if predicates.len() == 1));
}

#[test]
fn outer_sum_by_over_quantile_over_time_groups_positionally() {
    // `sum by (host) (quantile_over_time(...))`: inner per-series
    // quantile-over-time (label-preserving), then an outer cross-series sum
    // grouped on a positional `Aggregate.by` — the same shape SQL produces, not
    // a name-based Partition. Leaf = [ts, value, host, service] (referenced
    // names appended sorted) → host = col 2.
    let qe = lower(r#"sum by (host) (quantile_over_time(0.99, latency{service="web"}[5m]))"#);
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &qe
    else {
        panic!("expected outer Aggregate grouped by host, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    // Inner: Aggregate{Quantile} over TimeRange (per-series over_time reduction).
    let QueryExpr::Aggregate {
        measures, child, ..
    } = child.as_ref()
    else {
        panic!("expected Aggregate (quantile_over_time) under the outer Sum, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Quantile { .. }]));
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

#[test]
fn avg_over_time_maps_to_avg_intent() {
    let qe = lower("avg_over_time(cpu_seconds_total[10m])");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected Aggregate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Avg { .. }]));
    let QueryExpr::TimeRange { range, .. } = child.as_ref() else {
        panic!("expected TimeRange child, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(600));
}

#[test]
fn stddev_and_stdvar_over_time() {
    let qe = lower("stddev_over_time(m[5m])");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
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
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));

    let qe = lower("stdvar_over_time(m[5m])");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
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
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

#[test]
fn histogram_quantile_wraps_inner_in_quantile() {
    // The argument's structure (here `rate`) is preserved *under* the quantile,
    // not squashed away. The `_bucket` metric + `le` matcher mark the classic
    // form → `HistogramQuantile` over `Aggregate{Rate}` over Scan.
    let qe = lower(r#"histogram_quantile(0.95, rate(http_duration_seconds_bucket{le="0.5"}[5m]))"#);
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected outer Aggregate{{HistogramQuantile}}, got {qe:?}");
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::HistogramQuantile { q }] if (*q - 0.95).abs() < 1e-9)
    );
    let QueryExpr::Aggregate {
        measures, child, ..
    } = child.as_ref()
    else {
        panic!("expected inner Aggregate{{Rate}}, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let QueryExpr::TimeRange {
        range,
        child: tr_child,
    } = child.as_ref()
    else {
        panic!("expected TimeRange under Rate, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
    assert!(
        matches!(tr_child.as_ref(), QueryExpr::Scan { predicates, .. } if predicates.len() == 1)
    );
}

#[test]
fn histogram_quantile_over_sum_by_le_preserves_grouping() {
    // The canonical Prometheus histogram pattern. Previously returned
    // UnsupportedFeature because `extract_matrix` couldn't see through the
    // `sum by (le)` aggregate; now the `le` grouping survives into the
    // canonical tree.
    let qe = lower(r#"histogram_quantile(0.99, sum by (le) (rate(http_requests_bucket[5m])))"#);
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected outer Aggregate{{HistogramQuantile}}, got {qe:?}");
    };
    // The `by (le)` grouping marks the classic cumulative-bucket form.
    assert!(
        matches!(measures.as_slice(), [AggIntent::HistogramQuantile { q }] if (*q - 0.99).abs() < 1e-9)
    );
    // `sum by (le)` survives as a positional Aggregate (by = [2], `le`) over the
    // inner Rate — no name-based Partition.
    let QueryExpr::Aggregate {
        reduction,
        measures,
        ..
    } = child.as_ref()
    else {
        panic!("expected `sum by (le)` as a positional Aggregate, got {child:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
}

// ── rate / increase carry their own window (no Window node) ─────────────────────

#[test]
fn rate_has_time_range_child_not_window() {
    let qe = lower("rate(http_requests_total[5m])");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected Aggregate for rate, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let QueryExpr::TimeRange { range, .. } = child.as_ref() else {
        panic!("expected TimeRange child (not Window), got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(300));
}

#[test]
fn increase_maps_to_increase_intent() {
    let qe = lower("increase(errors_total[1h])");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected Aggregate for increase, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Increase]));
    let QueryExpr::TimeRange { range, .. } = child.as_ref() else {
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
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected outer Aggregate{{Sum}}, got {qe:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    let QueryExpr::Aggregate {
        measures, child, ..
    } = child.as_ref()
    else {
        panic!("expected inner Aggregate{{Rate}}, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

#[test]
fn sum_by_over_rate_groups_the_outer_sum() {
    // `sum by (job) (rate(...))`: the grouping belongs to the OUTER sum and lands
    // on a positional `Aggregate.by` (the same shape SQL produces) over the
    // label-preserving inner Rate. Leaf = [ts, value, job] → by = [2].
    let qe = lower("sum by (job) (rate(http_requests_total[5m]))");
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &qe
    else {
        panic!("expected outer Aggregate grouped by job, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate])
    ));
}

#[test]
fn count_over_rate_keeps_both_levels() {
    // The `Outer::Count` sibling of the `sum(rate(...))` bug.
    let qe = lower("count(rate(http_requests_total[5m]))");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected outer Aggregate{{Cardinality}}, got {qe:?}");
    };
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::Cardinality { .. }]
    ));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate])
    ));
}

// ── count / cardinality ───────────────────────────────────────────────────────

#[test]
fn count_over_time_is_count_intent() {
    let qe = lower("count_over_time(m[5m])");
    let QueryExpr::Aggregate {
        measures, child, ..
    } = &qe
    else {
        panic!("expected Aggregate");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

#[test]
fn outer_count_is_cardinality() {
    // `count by (symbol) (count_over_time(...))`: inner per-series sample count
    // over the window (label-preserving), outer cross-series cardinality grouped
    // on a positional `Aggregate.by`. Leaf = [ts, value, symbol] → symbol = col 2.
    let qe = lower("count by (symbol) (count_over_time(financial_last_trade_price[5m]))");
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &qe
    else {
        panic!("expected outer Aggregate grouped by symbol, got {qe:?}");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::Cardinality { .. }]
    ));
    // Inner: Aggregate{Count} over TimeRange (per-series count_over_time).
    let QueryExpr::Aggregate {
        measures, child, ..
    } = child.as_ref()
    else {
        panic!("expected Aggregate (count_over_time) under the cardinality, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

// ── topk / bottomk ────────────────────────────────────────────────────────────

#[test]
fn topk_over_count_is_heavy_hitter_topk() {
    let qe = lower(r#"topk by (service) (10, count_over_time(requests{env="prod"}[1m]))"#);
    // Heavy-hitter: Aggregate{TopK} with grouping resolved to positional ids.
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &qe
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
    let QueryExpr::Aggregate {
        measures, child, ..
    } = child.as_ref()
    else {
        panic!("expected Aggregate (count_over_time) under TopK, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    let QueryExpr::TimeRange { range, child } = child.as_ref() else {
        panic!("expected TimeRange under Count aggregate, got {child:?}");
    };
    assert_eq!(*range, Duration::from_secs(60));
    assert!(matches!(child.as_ref(), QueryExpr::Scan { .. }));
}

#[test]
fn topk_over_avg_is_generic_sort_limit() {
    let qe = lower("topk by (host) (5, avg_over_time(cpu[5m]))");
    let QueryExpr::Limit { n, offset, child } = &qe else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 5);
    assert_eq!(*offset, 0);
    let QueryExpr::Sort {
        keys,
        partition_by,
        child,
    } = child.as_ref()
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
        matches!(child.as_ref(), QueryExpr::Aggregate { reduction, measures, .. }
            if reduction == &Reduction::PerEntity && matches!(measures.as_slice(), [AggIntent::Avg { .. }])),
        "expected bare per-series Avg aggregate under Sort, got {child:?}"
    );
}

#[test]
fn topk_over_sum_is_generic_sort_limit() {
    // Only `count_over_time` triggers the heavy-hitter path; `sum_over_time`
    // falls back to generic sort + limit.
    let qe = lower("topk(5, sum_over_time(m[5m]))");
    assert!(
        matches!(&qe, QueryExpr::Limit { .. }),
        "expected Limit (generic sort), got {qe:?}"
    );
    assert!(has_intent(&qe, |i| matches!(i, AggIntent::Sum { .. })));
    assert!(!has_intent(&qe, |i| matches!(i, AggIntent::TopK { .. })));
}

#[test]
fn bottomk_over_count_is_generic_sort_ascending() {
    // `bottomk` is never a heavy-hitter (descending=false), even over count.
    let qe = lower("bottomk(3, count_over_time(m[5m]))");
    let QueryExpr::Limit { n, child, .. } = &qe else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 3);
    let QueryExpr::Sort { keys, .. } = child.as_ref() else {
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
    let QueryExpr::Limit { n, child, .. } = &qe else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, 3);
    let QueryExpr::Sort { keys, .. } = child.as_ref() else {
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
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &qe
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
    let QueryExpr::Aggregate {
        measures, child, ..
    } = child.as_ref()
    else {
        panic!("expected inner Aggregate{{Count}}, got {child:?}");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
    assert!(matches!(child.as_ref(), QueryExpr::TimeRange { .. }));
}

// ── binary ops ────────────────────────────────────────────────────────────────

#[test]
fn binary_op_division() {
    let qe = lower("rate(a[5m]) / rate(b[5m])");
    let QueryExpr::BinaryOp { op, lhs, rhs, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Arithmetic(ArithmeticOpKind::Div));
    assert!(
        matches!(lhs.as_ref(), QueryExpr::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate]))
    );
    assert!(
        matches!(rhs.as_ref(), QueryExpr::Aggregate { measures, .. } if matches!(measures.as_slice(), [AggIntent::Rate]))
    );
}

#[test]
fn binary_op_with_on_grouping() {
    let qe = lower("a / on(host) b");
    let QueryExpr::BinaryOp { vector_match, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    let vm = vector_match.as_ref().expect("vector_match present");
    use asap_types::pre_asap::VectorMatchKind;
    assert_eq!(vm.kind, VectorMatchKind::On);
    assert_eq!(vm.labels, vec!["host".to_string()]);
}

#[test]
fn binary_op_binds_each_branch_against_its_own_schema() {
    // Each side scans a different metric and groups by a different label. With a
    // single root schema threaded to both branches, the left scan would leak the
    // right's group key (and vice-versa). Per-branch binding keeps them separate.
    let qe = lower("count by (job) (a) / count by (region) (b)");
    let QueryExpr::BinaryOp { lhs, rhs, .. } = &qe else {
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

/// Collect every `AggIntent` in the tree, root-to-leaf.
fn all_intents(e: &QueryExpr) -> Vec<AggIntent> {
    let mut out = Vec::new();
    collect_intents(e, &mut out);
    out
}

fn collect_intents(e: &QueryExpr, out: &mut Vec<AggIntent>) {
    match e {
        QueryExpr::Aggregate {
            measures, child, ..
        } => {
            out.extend(measures.iter().cloned());
            collect_intents(child, out);
        }
        QueryExpr::TimeRange { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. } => collect_intents(child, out),
        QueryExpr::BinaryOp { lhs, rhs, .. } => {
            collect_intents(lhs, out);
            collect_intents(rhs, out);
        }
        _ => {}
    }
}

/// True if any `AggIntent` anywhere in the tree satisfies `pred`.
fn has_intent<F: Fn(&AggIntent) -> bool>(e: &QueryExpr, pred: F) -> bool {
    all_intents(e).iter().any(pred)
}

/// Column names on the first `Scan` reachable by descending single-child nodes.
fn scan_columns(e: &QueryExpr) -> Vec<String> {
    match e {
        QueryExpr::Scan { schema, .. } => schema.columns.iter().map(|c| c.name.clone()).collect(),
        QueryExpr::Aggregate { child, .. }
        | QueryExpr::TimeRange { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. } => scan_columns(child),
        _ => vec![],
    }
}

// ── without(...) grouping (issue #39) ───────────────────────────────────────────

#[test]
fn without_grouping_lowers_to_the_exclusion_form() {
    // `sum without (instance) (rate(m[5m]))` — a cross-series reduction over the
    // per-series rate, grouped by every label except `instance`. The excluded
    // label is stored positionally (the Binder seeds it), the grouping is the
    // `without` form, and the output schema stays open.
    let qe = lower("sum without (instance) (rate(m[5m]))");
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &qe
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
        matches!(child.as_ref(), QueryExpr::Aggregate { measures, .. }
        if matches!(measures.as_slice(), [AggIntent::Rate]))
    );
    assert!(!qe.output_schema().unwrap().closed);
}

// ── parameter validation (reject rather than silently truncate/garble) ──────────

#[test]
fn fractional_or_negative_topk_k_is_rejected() {
    // `as u64` would silently truncate 2.7→2 / saturate -1→0.
    assert!(lower_promql("topk(2.7, count_over_time(m[1m]))", AccuracyTarget::Exact).is_err());
    assert!(lower_promql("bottomk(2.5, sum_over_time(m[1m]))", AccuracyTarget::Exact).is_err());
}

#[test]
fn out_of_range_quantile_phi_is_rejected() {
    // φ outside [0,1] would otherwise yield a bogus `quantile_1_5` column.
    assert!(lower_promql("quantile(1.5, up)", AccuracyTarget::Exact).is_err());
    assert!(lower_promql("quantile_over_time(1.5, m[5m])", AccuracyTarget::Exact).is_err());
    assert!(lower_promql(
        "histogram_quantile(2.0, rate(b[5m]))",
        AccuracyTarget::Exact
    )
    .is_err());
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

// ── accuracy propagation ──────────────────────────────────────────────────────

#[test]
fn accuracy_target_flows_into_quantile_intent() {
    let qe = lower_promql(
        "quantile_over_time(0.9, m[5m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let QueryExpr::Aggregate { measures, .. } = &qe else {
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
    // The Binder adds all referenced label names (group keys AND filter
    // predicate columns) to the scan schema, so `env` appears as a column
    // even though it is only used as a filter.
    // per_series_reduction_schema preserves the time axis and all label columns.
    let QueryExpr::Aggregate { .. } = &qe else {
        panic!("expected Aggregate, got {qe:?}");
    };
    let schema = qe.output_schema().expect("aggregate schema");
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["ts", "value", "env"]);
    assert_eq!(
        schema.time_index,
        Some(0),
        "per-series over_time preserves the time axis"
    );
}

#[test]
fn scan_schema_carries_ts_value_and_group_keys() {
    // `service` is a group key → the Binder lands it in the self-contained
    // Scan schema (positional). `env` is only a filter, so it is not a column.
    let qe = lower("count by (service) (count_over_time(requests[1m]))");
    fn find_scan(n: &QueryExpr) -> &QueryExpr {
        match n {
            QueryExpr::Scan { .. } => n,
            QueryExpr::TimeRange { child, .. }
            | QueryExpr::Aggregate { child, .. }
            | QueryExpr::Filter { child, .. } => find_scan(child),
            other => panic!("unexpected node {other:?}"),
        }
    }
    let QueryExpr::Scan { schema, .. } = find_scan(&qe) else {
        unreachable!()
    };
    let mut names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["service", "ts", "value"]);
    assert_eq!(schema.time_index, Some(0)); // ts
}

// ── batch entry point ─────────────────────────────────────────────────────────

#[test]
fn batch_lowers_each_entry_and_reads_per_query_accuracy() {
    let workload = QueryWorkload {
        language: QueryLanguage::PromQL,
        query_batch: Some(vec![
            BatchEntry {
                query: Query("rate(a[5m])".into()),
                requirements: None,
            },
            BatchEntry {
                query: Query("quantile_over_time(0.9, b[5m])".into()),
                requirements: Some(QueryRequirements {
                    accuracy: Some(AccuracyTarget::Epsilon(0.02)),
                    latency_ms: None,
                }),
            },
        ]),
        repeating_queries: None,
        data_workload: None,
    };
    let results = lower_promql_batch(&workload);
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok());
    assert!(results[1].is_ok());
}

#[test]
fn batch_rejects_non_promql_language() {
    use asap_types::workload::SqlDialect;
    let workload = QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        query_batch: Some(vec![BatchEntry {
            query: Query("SELECT 1".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_workload: None,
    };
    let results = lower_promql_batch(&workload);
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0], Err(LoweringError::WrongLanguage(_))));
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
        matches!(q, QueryExpr::Aggregate { ref reduction, .. } if reduction == &Reduction::by(vec![]))
    );

    // Cross-series reduce grouped by a label → `Aggregate.reduction`.
    let q = lower("sum by (job) (http_requests_total)");
    assert!(matches!(q, QueryExpr::Aggregate { ref reduction, .. }
        if reduction.expect_reduce().len() == 1));

    // Reduce over a label-preserving `rate` grouped by a label → still
    // `Aggregate.reduction` (the keys resolve against rate's preserved schema).
    let q = lower("sum by (job) (rate(http_requests_total[5m]))");
    assert!(matches!(q, QueryExpr::Aggregate { ref reduction, .. }
        if reduction.expect_reduce().len() == 1));
}

#[test]
fn generic_topk_grouping_lowers_to_sort_partition_by() {
    // Per-group ranking (`topk by (host)`, non-heavy-hitter) groups *without*
    // reducing → the grouping rides on `Sort.partition_by`, and the windowed
    // reduction beneath stays label-preserving (`by: []`). No `Partition` node.
    let q = lower("topk by (host) (5, avg_over_time(cpu[5m]))");
    let QueryExpr::Limit { child, .. } = &q else {
        panic!("expected Limit, got {q:?}");
    };
    let QueryExpr::Sort {
        partition_by,
        child,
        ..
    } = child.as_ref()
    else {
        panic!("expected Sort, got {child:?}");
    };
    assert_eq!(partition_by, &vec![2], "host is col 2 in [ts, value, host]");
    assert!(
        matches!(child.as_ref(), QueryExpr::Aggregate { reduction, .. } if reduction == &Reduction::PerEntity)
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
    let QueryExpr::Limit { n, child, .. } = &q else {
        panic!("expected Limit, got {q:?}");
    };
    assert_eq!(*n, 3);
    let QueryExpr::Sort {
        keys,
        partition_by,
        child,
    } = child.as_ref()
    else {
        panic!("expected Sort, got {child:?}");
    };
    assert!(!keys[0].ascending, "topk ranks descending");
    assert_eq!(partition_by, &vec![2], "job is col 2 in [ts, value, job]");
    // No implicit reducing aggregate — the selector is label-preserving, so the
    // sort is directly over the Scan (the `job` label survives to partition by).
    assert!(
        matches!(child.as_ref(), QueryExpr::Scan { .. }),
        "ranking is over the bare Scan, not a reducing Aggregate, got {child:?}"
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
    let QueryExpr::Limit { child, .. } = &q else {
        panic!("expected Limit, got {q:?}");
    };
    let QueryExpr::Sort {
        partition_by,
        child,
        ..
    } = child.as_ref()
    else {
        panic!("expected Sort, got {child:?}");
    };
    assert!(partition_by.is_empty(), "no `by` → global ranking");
    assert!(matches!(child.as_ref(), QueryExpr::Scan { .. }));
    assert!(!has_intent(&q, |i| matches!(i, AggIntent::Sum { .. })));
}

// ── Issue #109: histogram_quantiles fans out into one branch per φ ──────────

/// The `(label value, intent)` of each `histogram_quantiles` branch.
fn quantile_branches(q: &QueryExpr) -> Vec<(String, AggIntent)> {
    let QueryExpr::Concat { children } = q else {
        panic!("expected a Concat at the root, got {q:?}");
    };
    children
        .iter()
        .map(|c| {
            let QueryExpr::PromqlRelabel { value, child, .. } = c else {
                panic!("expected PromqlRelabel per branch, got {c:?}");
            };
            let QueryExpr::Literal(ScalarValue::Utf8(v)) = value.as_ref() else {
                panic!("expected a literal label value, got {value:?}");
            };
            let QueryExpr::Aggregate { measures, .. } = child.as_ref() else {
                panic!("expected an Aggregate under the PromqlRelabel, got {child:?}");
            };
            (v.clone(), measures[0].clone())
        })
        .collect()
}

#[test]
fn histogram_quantiles_fans_out_over_native_histograms() {
    // Raw / native-histogram argument → the sketch-able `Quantile` intent,
    // exactly as the single-quantile `histogram_quantile` would choose.
    let q = lower(r#"histogram_quantiles(testhistogram3, "q", 0, 0.25, 1)"#);
    let branches = quantile_branches(&q);
    assert_eq!(branches.len(), 3);
    let labels: Vec<_> = branches.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        ["0.0", "0.25", "1.0"],
        "OpenMetrics float formatting"
    );
    for (_, intent) in &branches {
        assert!(
            matches!(intent, AggIntent::Quantile { .. }),
            "native histogram → sketch-able Quantile, got {intent:?}"
        );
    }
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
    let q = lower(r#"histogram_quantiles(testhistogram3, "q", 0.5, 0.9)"#);
    let QueryExpr::Concat { children } = &q else {
        panic!("expected Concat");
    };
    let shapes: Vec<Vec<String>> = children
        .iter()
        .map(|c| {
            c.output_schema()
                .expect("branch schema")
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect()
        })
        .collect();
    assert_eq!(shapes[0], shapes[1], "branches must be union-compatible");
    assert_eq!(shapes[0], vec!["value".to_string(), "q".to_string()]);
    assert_eq!(
        q.output_schema().expect("merged schema").columns.len(),
        2,
        "the merged schema describes every branch"
    );
}

#[test]
fn histogram_quantiles_uses_the_given_label_name() {
    let q = lower(r#"histogram_quantiles(h, "phi", 0.5)"#);
    let QueryExpr::Concat { children } = &q else {
        panic!("expected Concat");
    };
    let QueryExpr::PromqlRelabel { dst, .. } = &children[0] else {
        panic!("expected PromqlRelabel");
    };
    assert_eq!(dst, "phi");
}

#[test]
fn histogram_quantiles_formats_small_quantiles_like_prometheus() {
    // `labels.FormatOpenMetricsFloat`: Go's %g, so exponent form below 1e-4.
    let q = lower(r#"histogram_quantiles(h, "q", 0.00001)"#);
    assert_eq!(quantile_branches(&q)[0].0, "1e-05");
}

#[test]
fn histogram_quantiles_rejects_an_out_of_range_quantile() {
    // Same rule as `histogram_quantile(φ, …)` — one bad φ fails the whole call.
    for q in [
        r#"histogram_quantiles(h, "q", -0.1)"#,
        r#"histogram_quantiles(h, "q", 1.01)"#,
        r#"histogram_quantiles(h, "q", 0.5, NaN)"#,
    ] {
        assert!(
            lower_promql(q, AccuracyTarget::Exact).is_err(),
            "{q} should be rejected"
        );
    }
}
