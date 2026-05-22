//! End-to-end tests for PromQL → L2 → canonical L3 lowering.

use std::time::Duration;

use asap_control_core::intent_algebra::{
    AggIntent, BinaryOpKind, ColumnRef, CompareOp, L3Expr, L3Scalar, PartitionKeys, QueryExpr,
    Source, WindowKind,
};
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::{
    BatchEntry, Query, QueryLanguage, QueryRequirements, QueryWorkload,
};

use asap_control_lower::{lower_promql, lower_promql_batch, LoweringError};

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
        .all(|p| matches!(&p.0, L3Expr::Compare { .. })));
}

#[test]
fn regex_matcher_lowers_to_regex_compareop() {
    let qe = lower(r#"http_requests_total{path=~"/api/.*"}"#);
    let QueryExpr::Scan { predicates, .. } = &qe else {
        panic!("expected Scan, got {qe:?}");
    };
    let L3Expr::Compare { left, op, right } = &predicates[0].0 else {
        panic!("expected Compare, got {:?}", predicates[0].0);
    };
    assert_eq!(*op, CompareOp::Regex);
    assert!(matches!(left.as_ref(), L3Expr::Column(ColumnRef::Named(n)) if n == "path"));
    assert!(matches!(right.as_ref(), L3Expr::Literal(L3Scalar::Utf8(v)) if v == "/api/.*"));
}

// ── *_over_time → Window over Aggregate ─────────────────────────────────────────

#[test]
fn quantile_over_time_is_window_over_aggregate() {
    let qe = lower(r#"quantile_over_time(0.99, http_request_duration{env="prod"}[5m])"#);
    let QueryExpr::Window {
        kind, size, child, ..
    } = &qe
    else {
        panic!("expected Window, got {qe:?}");
    };
    assert_eq!(*kind, WindowKind::Tumbling);
    assert_eq!(*size, Duration::from_secs(300));
    let QueryExpr::Aggregate {
        by, aggs, child, ..
    } = child.as_ref()
    else {
        panic!("expected Aggregate under Window, got {child:?}");
    };
    assert!(by.is_empty());
    assert!(matches!(aggs.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.99).abs() < 1e-9));
    // The label matcher folded onto the Scan.
    assert!(matches!(child.as_ref(), QueryExpr::Scan { predicates, .. } if predicates.len() == 1));
}

#[test]
fn outer_sum_by_wraps_in_partition() {
    // `sum by (host) (quantile_over_time(...))` is a two-level reduction: an
    // inner per-series quantile-over-time, then an outer cross-series sum.
    // Grouping rides on a `Partition` wrapping the outer Sum (backend model).
    let qe = lower(r#"sum by (host) (quantile_over_time(0.99, latency{service="web"}[5m]))"#);
    let QueryExpr::Partition { keys, child } = &qe else {
        panic!("expected Partition, got {qe:?}");
    };
    assert_eq!(keys, &PartitionKeys::By(vec!["host".into()]));
    // Outer cross-series Sum.
    let QueryExpr::Aggregate { aggs, child, .. } = child.as_ref() else {
        panic!("expected outer Aggregate{{Sum}} under Partition, got {child:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum]));
    // Inner: Window over Aggregate{Quantile}.
    let QueryExpr::Window { child, .. } = child.as_ref() else {
        panic!("expected Window under the outer Sum, got {child:?}");
    };
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Quantile { .. }])
    ));
}

#[test]
fn avg_over_time_maps_to_avg_intent() {
    let qe = lower("avg_over_time(cpu_seconds_total[10m])");
    let QueryExpr::Window { size, child, .. } = &qe else {
        panic!("expected Window, got {qe:?}");
    };
    assert_eq!(*size, Duration::from_secs(600));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Avg])
    ));
}

#[test]
fn stddev_and_stdvar_over_time() {
    let qe = lower("stddev_over_time(m[5m])");
    let QueryExpr::Window { child, .. } = &qe else {
        panic!("expected Window");
    };
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::StdDev { population: false }])
    ));

    let qe = lower("stdvar_over_time(m[5m])");
    let QueryExpr::Window { child, .. } = &qe else {
        panic!("expected Window");
    };
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Variance { population: false }])
    ));
}

#[test]
fn histogram_quantile_wraps_inner_in_quantile() {
    // The argument's structure (here `rate`) is preserved *under* the Quantile,
    // not squashed away — `Aggregate{Quantile}` over `Aggregate{Rate}` over Scan.
    let qe = lower(r#"histogram_quantile(0.95, rate(http_duration_seconds_bucket{le="0.5"}[5m]))"#);
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate{{Quantile}}, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.95).abs() < 1e-9));
    let QueryExpr::Aggregate { aggs, child, .. } = child.as_ref() else {
        panic!("expected inner Aggregate{{Rate}}, got {child:?}");
    };
    assert!(
        matches!(aggs.as_slice(), [AggIntent::Rate { window }] if *window == Duration::from_secs(300))
    );
    assert!(matches!(child.as_ref(), QueryExpr::Scan { predicates, .. } if predicates.len() == 1));
}

#[test]
fn histogram_quantile_over_sum_by_le_preserves_grouping() {
    // The canonical Prometheus histogram pattern. Previously returned
    // UnsupportedFeature because `extract_matrix` couldn't see through the
    // `sum by (le)` aggregate; now the `le` grouping survives into L3.
    let qe = lower(r#"histogram_quantile(0.99, sum by (le) (rate(http_requests_bucket[5m])))"#);
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate{{Quantile}}, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Quantile { q, .. }] if (*q - 0.99).abs() < 1e-9));
    assert!(
        matches!(child.as_ref(), QueryExpr::Partition { keys, .. } if *keys == PartitionKeys::By(vec!["le".into()])),
        "expected `sum by (le)` to survive as a Partition under the quantile, got {child:?}"
    );
}

// ── rate / increase carry their own window (no Window node) ─────────────────────

#[test]
fn rate_has_no_window_node() {
    let qe = lower("rate(http_requests_total[5m])");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected Aggregate (no Window) for rate, got {qe:?}");
    };
    assert!(matches!(
        aggs.as_slice(),
        [AggIntent::Rate { window }] if *window == Duration::from_secs(300)
    ));
    assert!(matches!(child.as_ref(), QueryExpr::Scan { .. }));
}

#[test]
fn increase_maps_to_increase_intent() {
    let qe = lower("increase(errors_total[1h])");
    let QueryExpr::Aggregate { aggs, .. } = &qe else {
        panic!("expected Aggregate for increase, got {qe:?}");
    };
    assert!(matches!(
        aggs.as_slice(),
        [AggIntent::Increase { window }] if *window == Duration::from_secs(3600)
    ));
}

// ── outer aggregation over an inner range-vector func is two levels ─────────────

#[test]
fn sum_over_rate_keeps_both_levels() {
    // Regression: `sum(rate(m[w]))` — the most common PromQL shape — must keep
    // the cross-series Sum, not collapse to a bare per-series Rate.
    let qe = lower("sum(rate(http_requests_total[5m]))");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate{{Sum}}, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum]));
    let QueryExpr::Aggregate { aggs, child, .. } = child.as_ref() else {
        panic!("expected inner Aggregate{{Rate}}, got {child:?}");
    };
    assert!(
        matches!(aggs.as_slice(), [AggIntent::Rate { window }] if *window == Duration::from_secs(300))
    );
    assert!(matches!(child.as_ref(), QueryExpr::Scan { .. }));
}

#[test]
fn sum_by_over_rate_groups_the_outer_sum() {
    // `sum by (job) (rate(...))`: the grouping belongs to the OUTER sum, landing
    // on a Partition that wraps the two-level aggregate.
    let qe = lower("sum by (job) (rate(http_requests_total[5m]))");
    let QueryExpr::Partition { keys, child } = &qe else {
        panic!("expected Partition by job, got {qe:?}");
    };
    assert_eq!(*keys, PartitionKeys::By(vec!["job".into()]));
    let QueryExpr::Aggregate { aggs, child, .. } = child.as_ref() else {
        panic!("expected Aggregate{{Sum}} under Partition, got {child:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Sum]));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate { .. }])
    ));
}

#[test]
fn count_over_rate_keeps_both_levels() {
    // The `Outer::Count` sibling of the `sum(rate(...))` bug.
    let qe = lower("count(rate(http_requests_total[5m]))");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected outer Aggregate{{Cardinality}}, got {qe:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Cardinality { .. }]));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate { .. }])
    ));
}

// ── count / cardinality ───────────────────────────────────────────────────────

#[test]
fn count_over_time_is_count_intent() {
    let qe = lower("count_over_time(m[5m])");
    let QueryExpr::Window { child, .. } = &qe else {
        panic!("expected Window");
    };
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Count { .. }])
    ));
}

#[test]
fn outer_count_is_cardinality() {
    // `count by (symbol) (count_over_time(...))`: inner per-series sample count
    // over the window, outer cross-series cardinality grouped by symbol.
    let qe = lower("count by (symbol) (count_over_time(financial_last_trade_price[5m]))");
    let QueryExpr::Partition { keys, child } = &qe else {
        panic!("expected Partition, got {qe:?}");
    };
    assert_eq!(keys, &PartitionKeys::By(vec!["symbol".into()]));
    // Outer cardinality (count of series).
    let QueryExpr::Aggregate { aggs, child, .. } = child.as_ref() else {
        panic!("expected outer Aggregate{{Cardinality}} under Partition, got {child:?}");
    };
    assert!(matches!(aggs.as_slice(), [AggIntent::Cardinality { .. }]));
    // Inner: Window over Aggregate{Count} (count_over_time).
    let QueryExpr::Window { child, .. } = child.as_ref() else {
        panic!("expected Window under the outer cardinality, got {child:?}");
    };
    assert!(matches!(
        child.as_ref(),
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Count { .. }])
    ));
}

// ── topk / bottomk ────────────────────────────────────────────────────────────

#[test]
fn topk_over_count_is_heavy_hitter_topk() {
    let qe = lower(r#"topk by (service) (10, count_over_time(requests{env="prod"}[1m]))"#);
    // Heavy-hitter: Aggregate{TopK} with grouping resolved to positional ids.
    let QueryExpr::Aggregate {
        by, aggs, child, ..
    } = &qe
    else {
        panic!("expected Aggregate with TopK, got {qe:?}");
    };
    // `service` is the only group key → resolved to a positional ColumnId.
    assert_eq!(by.len(), 1);
    assert!(matches!(aggs.as_slice(), [AggIntent::TopK { k: 10, .. }]));
    // The heavy-hitter sketch counts directly off the windowed scan.
    let QueryExpr::Window { size, child, .. } = child.as_ref() else {
        panic!("expected Window under TopK Aggregate, got {child:?}");
    };
    assert_eq!(*size, Duration::from_secs(60));
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
    let QueryExpr::Sort { keys, child } = child.as_ref() else {
        panic!("expected Sort under Limit, got {child:?}");
    };
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].ascending, "topk ranks descending");
    // Underneath: the windowed avg aggregate, grouped via Partition.
    assert!(
        matches!(child.as_ref(), QueryExpr::Partition { keys, .. } if *keys == PartitionKeys::By(vec!["host".into()]))
    );
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

// ── binary ops ────────────────────────────────────────────────────────────────

#[test]
fn binary_op_division() {
    let qe = lower("rate(a[5m]) / rate(b[5m])");
    let QueryExpr::BinaryOp { op, lhs, rhs, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Div);
    assert!(
        matches!(lhs.as_ref(), QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate { .. }]))
    );
    assert!(
        matches!(rhs.as_ref(), QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate { .. }]))
    );
}

#[test]
fn binary_op_with_on_grouping() {
    let qe = lower("a / on(host) b");
    let QueryExpr::BinaryOp { vector_match, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    let vm = vector_match.as_ref().expect("vector_match present");
    use asap_control_core::intent_algebra::VectorMatchKind;
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

/// Column names on the first `Scan` reachable by descending single-child nodes.
fn scan_columns(e: &QueryExpr) -> Vec<String> {
    match e {
        QueryExpr::Scan { schema, .. } => schema.columns.iter().map(|c| c.name.clone()).collect(),
        QueryExpr::Partition { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Window { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. } => scan_columns(child),
        _ => vec![],
    }
}

// ── without is unsupported (no label registry) ──────────────────────────────────

#[test]
fn without_grouping_is_unsupported() {
    let err = lower_promql(
        "sum without (instance) (rate(m[5m]))",
        AccuracyTarget::Exact,
    )
    .unwrap_err();
    assert!(format!("{err}").contains("without"), "got {err}");
}

// ── accuracy propagation ──────────────────────────────────────────────────────

#[test]
fn accuracy_target_flows_into_quantile_intent() {
    let qe = lower_promql(
        "quantile_over_time(0.9, m[5m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let QueryExpr::Window { child, .. } = &qe else {
        panic!("expected Window");
    };
    let QueryExpr::Aggregate { aggs, .. } = child.as_ref() else {
        panic!("expected Aggregate");
    };
    assert!(matches!(
        &aggs[0],
        AggIntent::Quantile { accuracy: AccuracyTarget::Epsilon(e), .. } if (*e - 0.01).abs() < 1e-12
    ));
}

// ── schema flow (positional, carried on Scan; derived on demand) ─────────────────

#[test]
fn aggregate_output_schema_is_single_quantile_column() {
    let qe = lower(r#"quantile_over_time(0.99, http_request_duration{env="prod"}[5m])"#);
    // Window requires its child to carry a time axis; the Aggregate beneath it
    // strips it, so derive the schema at the Aggregate node.
    let QueryExpr::Window { child, .. } = &qe else {
        panic!("expected Window");
    };
    let schema = child.output_schema().expect("aggregate schema");
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["quantile_0_99"]);
    assert!(
        schema.time_index.is_none(),
        "aggregate strips the time axis"
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
            QueryExpr::Partition { child, .. }
            | QueryExpr::Window { child, .. }
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
        data_characteristics: None,
    };
    let results = lower_promql_batch(&workload);
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok());
    assert!(results[1].is_ok());
}

#[test]
fn batch_rejects_non_promql_language() {
    use asap_control_core::workload::SqlDialect;
    let workload = QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        query_batch: Some(vec![BatchEntry {
            query: Query("SELECT 1".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_promql_batch(&workload);
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0], Err(LoweringError::WrongLanguage(_))));
}
