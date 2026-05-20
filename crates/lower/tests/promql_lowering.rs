//! End-to-end tests for PromQL → L3 intent-algebra lowering.

use std::time::Duration;

use asap_control_core::intent_algebra::expr::{
    AggIntent, BinaryOpKind, QueryExpr, Source, TimeWindowKind,
};
use asap_control_core::intent_algebra::schema::{L3DataType, MetricSchema, SchemaCatalog};
use asap_control_core::intent_algebra::{CompareOp, L3Expr, L3Scalar};
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::{
    BatchEntry, Query, QueryLanguage, QueryRequirements, QueryWorkload,
};

use asap_control_lower::{lower_promql, lower_promql_batch, populate_schemas};

// ── Fixtures ───────────────────────────────────────────────────────────────────

fn empty_catalog() -> SchemaCatalog {
    SchemaCatalog::default()
}

fn catalog_with(metric: &str, labels: &[&str]) -> SchemaCatalog {
    let mut c = SchemaCatalog::default();
    c.metrics.insert(
        metric.to_string(),
        MetricSchema {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            value_type: L3DataType::Float64,
        },
    );
    c
}

fn lower(q: &str) -> QueryExpr {
    lower_promql(q, &empty_catalog(), AccuracyTarget::Exact)
        .unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

fn lower_eps(q: &str) -> QueryExpr {
    lower_promql(q, &empty_catalog(), AccuracyTarget::Epsilon(0.01))
        .unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

// ── Bare selectors & label matchers ─────────────────────────────────────────────

#[test]
fn bare_selector_is_scan_with_predicates() {
    let qe = lower(r#"http_requests_total{env="prod",status!="500"}"#);
    let QueryExpr::Scan { source, predicates } = &qe else {
        panic!("expected Scan, got {qe:?}");
    };
    match source {
        Source::TimeSeries { metric, time } => {
            assert_eq!(metric.0, "http_requests_total");
            assert!(time.is_none(), "PromQL string carries no absolute range");
        }
        other => panic!("expected TimeSeries source, got {other:?}"),
    }
    assert_eq!(predicates.len(), 2);
}

#[test]
fn regex_matcher_lowers_to_regex_compareop() {
    let qe = lower(r#"http_requests_total{path=~"/api/.*"}"#);
    let QueryExpr::Scan { predicates, .. } = &qe else {
        panic!("expected Scan, got {qe:?}");
    };
    let L3Expr::Compare { left, op, right } = &predicates[0].0 else {
        panic!("expected Compare predicate, got {:?}", predicates[0].0);
    };
    assert_eq!(*op, CompareOp::Regex);
    assert!(matches!(left.as_ref(), L3Expr::Column(c) if c.0 == "path"));
    assert!(matches!(right.as_ref(), L3Expr::Literal(L3Scalar::Utf8(v)) if v == "/api/.*"));
}

// ── *_over_time → Window → Aggregate ─────────────────────────────────────────────

#[test]
fn quantile_over_time_is_window_over_aggregate() {
    let qe = lower(r#"quantile_over_time(0.99, http_request_duration{env="prod"}[5m])"#);
    let QueryExpr::TimeWindow {
        kind, size, child, ..
    } = &qe
    else {
        panic!("expected TimeWindow, got {qe:?}");
    };
    assert_eq!(*kind, TimeWindowKind::Tumbling);
    assert_eq!(*size, Duration::from_secs(300));
    match &child.expr {
        QueryExpr::Aggregate {
            by, aggs, child, ..
        } => {
            assert!(by.is_empty());
            assert!(matches!(
                aggs.as_slice(),
                [AggIntent::Quantile { q, .. }] if (*q - 0.99).abs() < 1e-9
            ));
            // The label matcher rides on the Scan, not a separate Filter node.
            assert!(
                matches!(&child.expr, QueryExpr::Scan { predicates, .. } if predicates.len() == 1)
            );
        }
        other => panic!("expected Aggregate under TimeWindow, got {other:?}"),
    }
}

#[test]
fn outer_sum_by_pushes_group_keys_onto_inner_aggregate() {
    let qe = lower(r#"sum by (host) (quantile_over_time(0.99, latency{service="web"}[5m]))"#);
    let QueryExpr::TimeWindow { child, .. } = &qe else {
        panic!("expected TimeWindow, got {qe:?}");
    };
    let QueryExpr::Aggregate { by, aggs, .. } = &child.expr else {
        panic!("expected Aggregate, got {:?}", child.expr);
    };
    assert_eq!(by.len(), 1);
    assert_eq!(by[0].0, "host");
    // The inner quantile_over_time supplies the intent; the outer `sum` only groups.
    assert!(matches!(aggs.as_slice(), [AggIntent::Quantile { .. }]));
}

#[test]
fn avg_over_time_maps_to_avg_intent() {
    let qe = lower("avg_over_time(cpu_seconds_total[10m])");
    let QueryExpr::TimeWindow { size, child, .. } = &qe else {
        panic!("expected TimeWindow, got {qe:?}");
    };
    assert_eq!(*size, Duration::from_secs(600));
    assert!(matches!(
        &child.expr,
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Avg])
    ));
}

#[test]
fn min_max_sum_over_time_map_directly() {
    for (q, want) in [
        ("min_over_time(m[5m])", "min"),
        ("max_over_time(m[5m])", "max"),
        ("sum_over_time(m[5m])", "sum"),
    ] {
        let qe = lower(q);
        let QueryExpr::TimeWindow { child, .. } = &qe else {
            panic!("expected TimeWindow for {q}");
        };
        let QueryExpr::Aggregate { aggs, .. } = &child.expr else {
            panic!("expected Aggregate for {q}");
        };
        let ok = matches!(
            (want, &aggs[0]),
            ("min", AggIntent::Min) | ("max", AggIntent::Max) | ("sum", AggIntent::Sum)
        );
        assert!(ok, "{q} produced {:?}", aggs[0]);
    }
}

#[test]
fn stddev_and_stdvar_over_time() {
    let qe = lower("stddev_over_time(m[5m])");
    let QueryExpr::TimeWindow { child, .. } = &qe else {
        panic!("expected TimeWindow");
    };
    assert!(matches!(
        &child.expr,
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::StdDev { population: false }])
    ));

    let qe = lower("stdvar_over_time(m[5m])");
    let QueryExpr::TimeWindow { child, .. } = &qe else {
        panic!("expected TimeWindow");
    };
    assert!(matches!(
        &child.expr,
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Variance { population: false }])
    ));
}

// ── histogram_quantile ───────────────────────────────────────────────────────────

#[test]
fn histogram_quantile_substitutes_to_quantile() {
    let qe = lower(r#"histogram_quantile(0.95, rate(http_duration_seconds_bucket{le="0.5"}[5m]))"#);
    let QueryExpr::TimeWindow { size, child, .. } = &qe else {
        panic!("expected TimeWindow, got {qe:?}");
    };
    assert_eq!(*size, Duration::from_secs(300));
    let QueryExpr::Aggregate { aggs, child, .. } = &child.expr else {
        panic!("expected Aggregate");
    };
    assert!(matches!(
        aggs.as_slice(),
        [AggIntent::Quantile { q, .. }] if (*q - 0.95).abs() < 1e-9
    ));
    // The `le` matcher is preserved on the bucket scan.
    assert!(matches!(&child.expr, QueryExpr::Scan { predicates, .. } if predicates.len() == 1));
}

// ── rate / increase carry their own window ───────────────────────────────────────

#[test]
fn rate_has_no_timewindow_node() {
    let qe = lower("rate(http_requests_total[5m])");
    let QueryExpr::Aggregate { aggs, child, .. } = &qe else {
        panic!("expected Aggregate (no TimeWindow) for rate, got {qe:?}");
    };
    assert!(matches!(
        aggs.as_slice(),
        [AggIntent::Rate { window }] if *window == Duration::from_secs(300)
    ));
    assert!(matches!(&child.expr, QueryExpr::Scan { .. }));
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

// ── count / cardinality ───────────────────────────────────────────────────────────

#[test]
fn count_over_time_is_count_intent() {
    let qe = lower("count_over_time(m[5m])");
    let QueryExpr::TimeWindow { child, .. } = &qe else {
        panic!("expected TimeWindow");
    };
    assert!(matches!(
        &child.expr,
        QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Count { .. }])
    ));
}

#[test]
fn outer_count_is_cardinality() {
    let qe = lower("count by (symbol) (count_over_time(financial_last_trade_price[5m]))");
    let QueryExpr::TimeWindow { child, .. } = &qe else {
        panic!("expected TimeWindow, got {qe:?}");
    };
    let QueryExpr::Aggregate { by, aggs, .. } = &child.expr else {
        panic!("expected Aggregate");
    };
    assert_eq!(by.len(), 1);
    assert_eq!(by[0].0, "symbol");
    assert!(matches!(aggs.as_slice(), [AggIntent::Cardinality { .. }]));
}

// ── topk / bottomk ────────────────────────────────────────────────────────────────

#[test]
fn topk_over_count_is_heavy_hitter_topk() {
    let qe = lower(r#"topk by (service) (10, count_over_time(requests{env="prod"}[1m]))"#);
    // Window over Aggregate: the heavy-hitter top-k is computed per 1m window.
    let QueryExpr::TimeWindow { size, child, .. } = &qe else {
        panic!("expected TimeWindow, got {qe:?}");
    };
    assert_eq!(*size, Duration::from_secs(60));
    let QueryExpr::Aggregate {
        by, aggs, child, ..
    } = &child.expr
    else {
        panic!("expected Aggregate with TopK, got {:?}", child.expr);
    };
    assert!(by.is_empty(), "heavy-hitter keys live in the TopK intent");
    match aggs.as_slice() {
        [AggIntent::TopK { k, by, .. }] => {
            assert_eq!(*k, 10);
            assert_eq!(by.len(), 1);
            assert_eq!(by[0].0, "service");
        }
        other => panic!("expected TopK intent, got {other:?}"),
    }
    // The heavy-hitter sketch counts directly off the scan in one pass.
    assert!(matches!(&child.expr, QueryExpr::Scan { .. }));
}

#[test]
fn topk_over_avg_is_generic_sort_limit() {
    // Ranking by avg value has no heavy-hitter sketch → generic Sort + Limit.
    let qe = lower("topk by (host) (5, avg_over_time(cpu[5m]))");
    let QueryExpr::Limit { n, offset, child } = &qe else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, Some(5));
    assert_eq!(*offset, 0);
    let QueryExpr::Sort { keys, child } = &child.expr else {
        panic!("expected Sort under Limit, got {:?}", child.expr);
    };
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].ascending, "topk ranks descending");
    // Underneath: the windowed avg aggregate grouped by host.
    let QueryExpr::TimeWindow { child, .. } = &child.expr else {
        panic!("expected TimeWindow under Sort");
    };
    assert!(matches!(
        &child.expr,
        QueryExpr::Aggregate { by, aggs, .. }
            if by.len() == 1 && by[0].0 == "host" && matches!(aggs.as_slice(), [AggIntent::Avg])
    ));
}

#[test]
fn bottomk_is_always_generic_sort_ascending() {
    let qe = lower("bottomk(3, count_over_time(m[5m]))");
    let QueryExpr::Limit { n, child, .. } = &qe else {
        panic!("expected Limit, got {qe:?}");
    };
    assert_eq!(*n, Some(3));
    let QueryExpr::Sort { keys, .. } = &child.expr else {
        panic!("expected Sort");
    };
    assert!(keys[0].ascending, "bottomk ranks ascending");
}

// ── binary ops ────────────────────────────────────────────────────────────────────

#[test]
fn binary_op_division() {
    let qe = lower("rate(a[5m]) / rate(b[5m])");
    let QueryExpr::BinaryOp { op, lhs, rhs, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    assert_eq!(*op, BinaryOpKind::Div);
    assert!(
        matches!(&lhs.expr, QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate { .. }]))
    );
    assert!(
        matches!(&rhs.expr, QueryExpr::Aggregate { aggs, .. } if matches!(aggs.as_slice(), [AggIntent::Rate { .. }]))
    );
}

#[test]
fn binary_op_with_on_grouping() {
    let qe = lower("a / on(host) b");
    let QueryExpr::BinaryOp { vector_match, .. } = &qe else {
        panic!("expected BinaryOp, got {qe:?}");
    };
    let vm = vector_match.as_ref().expect("vector_match present");
    assert!(vm.on);
    assert_eq!(vm.labels, vec!["host".to_string()]);
}

// ── without resolution ──────────────────────────────────────────────────────────

#[test]
fn without_resolves_kept_labels_from_catalog() {
    let catalog = catalog_with("m", &["host", "region", "instance"]);
    let qe = lower_promql(
        "sum without (instance) (rate(m[5m]))",
        &catalog,
        AccuracyTarget::Exact,
    )
    .expect("lower with catalog");
    let QueryExpr::Aggregate { by, .. } = &qe else {
        panic!("expected Aggregate, got {qe:?}");
    };
    let mut got: Vec<&str> = by.iter().map(|g| g.0.as_str()).collect();
    got.sort();
    assert_eq!(got, vec!["host", "region"]);
}

#[test]
fn without_without_catalog_is_unsupported() {
    let err = lower_promql(
        "sum without (instance) (rate(m[5m]))",
        &empty_catalog(),
        AccuracyTarget::Exact,
    )
    .unwrap_err();
    assert!(format!("{err}").contains("without"), "got {err}");
}

// ── accuracy propagation ──────────────────────────────────────────────────────────

#[test]
fn accuracy_target_flows_into_quantile_intent() {
    let qe = lower_eps("quantile_over_time(0.9, m[5m])");
    let QueryExpr::TimeWindow { child, .. } = &qe else {
        panic!("expected TimeWindow");
    };
    let QueryExpr::Aggregate { aggs, .. } = &child.expr else {
        panic!("expected Aggregate");
    };
    assert!(matches!(
        &aggs[0],
        AggIntent::Quantile { accuracy: AccuracyTarget::Epsilon(e), .. } if (*e - 0.01).abs() < 1e-12
    ));
}

// ── schema population ───────────────────────────────────────────────────────────

#[test]
fn schema_population_for_quantile_over_time() {
    let catalog = catalog_with("http_request_duration", &["env", "host"]);
    let qe = lower_promql(
        r#"quantile_over_time(0.99, http_request_duration{env="prod"}[5m])"#,
        &catalog,
        AccuracyTarget::Exact,
    )
    .unwrap();
    let typed = populate_schemas(qe, &catalog);
    // Root TimeWindow passes the aggregate's schema through: a single Float64
    // `value` column (group-by is empty).
    assert_eq!(typed.schema.fields.len(), 1);
    assert_eq!(typed.schema.fields[0].name, "value");
    assert_eq!(typed.schema.fields[0].dtype, L3DataType::Float64);

    // Walk to the Scan leaf: timestamp + value + the two catalog labels.
    fn scan_schema(n: &asap_control_core::intent_algebra::L3Node) -> Vec<String> {
        match &n.expr {
            QueryExpr::Scan { .. } => n.schema.fields.iter().map(|f| f.name.clone()).collect(),
            QueryExpr::TimeWindow { child, .. }
            | QueryExpr::Aggregate { child, .. }
            | QueryExpr::Filter { child, .. } => scan_schema(child),
            other => panic!("unexpected node {other:?}"),
        }
    }
    let mut leaf = scan_schema(&typed);
    leaf.sort();
    assert_eq!(leaf, vec!["env", "host", "timestamp", "value"]);
}

#[test]
fn schema_population_for_heavy_hitter_topk() {
    let catalog = catalog_with("requests", &["service", "env"]);
    let qe = lower_promql(
        "topk by (service) (10, count_over_time(requests[1m]))",
        &catalog,
        AccuracyTarget::Exact,
    )
    .unwrap();
    let typed = populate_schemas(qe, &catalog);
    // TopK output: the by-column(s) + a synthetic `count` column.
    let names: Vec<&str> = typed
        .schema
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(names, vec!["service", "count"]);
    assert_eq!(typed.schema.fields[1].dtype, L3DataType::Int64);
}

// ── batch entry point ─────────────────────────────────────────────────────────────

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
    let results = lower_promql_batch(&workload, &empty_catalog());
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
    let results = lower_promql_batch(&workload, &empty_catalog());
    assert_eq!(results.len(), 1);
    assert!(matches!(
        results[0],
        Err(asap_control_lower::LoweringError::WrongLanguage(_))
    ));
}
