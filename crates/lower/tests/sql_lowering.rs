use std::collections::HashMap;

use asap_control_core::intent_algebra::expr::{
    AggIntent, Predicate, ProjectItem, QueryExpr, SortKey, Source,
};
use asap_control_core::intent_algebra::schema::{
    ColumnDef, L3DataType, SchemaCatalog, TableSchema,
};
use asap_control_core::intent_algebra::{CompareOp, L3Expr, SetOpKind};
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::{BatchEntry, Query, QueryLanguage, QueryWorkload, SqlDialect};
use asap_control_lower::{lower_batch, populate_schemas, LoweringError, SqlLowerer};

// ── Catalog helpers ───────────────────────────────────────────────────────────

fn metrics_catalog() -> SchemaCatalog {
    let mut tables = HashMap::new();
    tables.insert(
        "metrics".to_string(),
        TableSchema {
            columns: vec![
                ColumnDef {
                    name: "ts".to_string(),
                    data_type: L3DataType::Int64,
                    nullable: false,
                },
                ColumnDef {
                    name: "value".to_string(),
                    data_type: L3DataType::Float64,
                    nullable: true,
                },
                ColumnDef {
                    name: "region".to_string(),
                    data_type: L3DataType::Utf8,
                    nullable: true,
                },
                ColumnDef {
                    name: "host".to_string(),
                    data_type: L3DataType::Utf8,
                    nullable: true,
                },
            ],
            time_column: Some("ts".to_string()),
        },
    );
    SchemaCatalog { tables }
}

fn no_time_catalog() -> SchemaCatalog {
    let mut tables = HashMap::new();
    tables.insert(
        "events".to_string(),
        TableSchema {
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: L3DataType::Int64,
                    nullable: false,
                },
                ColumnDef {
                    name: "value".to_string(),
                    data_type: L3DataType::Float64,
                    nullable: true,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: L3DataType::Utf8,
                    nullable: true,
                },
            ],
            time_column: None,
        },
    );
    SchemaCatalog { tables }
}

// ── Tree-walking helpers ──────────────────────────────────────────────────────

/// Walk through Project/Filter/Sort/Limit wrappers to find the first Aggregate.
fn find_aggregate(
    expr: &QueryExpr,
) -> Option<(
    &Vec<asap_control_core::intent_algebra::expr::GroupKey>,
    &Vec<AggIntent>,
)> {
    match expr {
        QueryExpr::Aggregate { by, aggs, .. } => Some((by, aggs)),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. } => find_aggregate(&child.expr),
        _ => None,
    }
}

/// Walk through wrappers to find the predicate of the first Filter node.
fn find_predicate(expr: &QueryExpr) -> Option<&L3Expr> {
    match expr {
        QueryExpr::Filter {
            pred: Predicate(e), ..
        } => Some(e),
        QueryExpr::Project { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. } => find_predicate(&child.expr),
        _ => None,
    }
}

/// Walk through wrappers to find the cols of the first Project node.
fn find_project_items(expr: &QueryExpr) -> Option<&[ProjectItem]> {
    match expr {
        QueryExpr::Project { cols, .. } => Some(cols),
        QueryExpr::Sort { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Limit { child, .. } => find_project_items(&child.expr),
        _ => None,
    }
}

/// Walk through wrappers to find the keys of the first Sort node.
fn find_sort_keys(expr: &QueryExpr) -> Option<&[SortKey]> {
    match expr {
        QueryExpr::Sort { keys, .. } => Some(keys),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Limit { child, .. } => find_sort_keys(&child.expr),
        _ => None,
    }
}

/// Walk through wrappers to find the first Scan source.
fn find_source(expr: &QueryExpr) -> Option<&Source> {
    match expr {
        QueryExpr::Scan { source, .. } => Some(source),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. } => find_source(&child.expr),
        _ => None,
    }
}

// ── Tests: Scan / Projection ──────────────────────────────────────────────────

#[tokio::test]
async fn test_scan_table_name() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT ts, value FROM metrics")
        .await
        .unwrap();

    let source = find_source(&result).expect("expected a Scan node");
    let Source::Table { table_ref, .. } = source else {
        panic!("expected Source::Table, got {source:?}");
    };
    assert_eq!(table_ref.0, "metrics");
}

#[tokio::test]
async fn test_projection_wraps_scan() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT ts, value FROM metrics")
        .await
        .unwrap();

    // DataFusion always emits a Projection for explicit SELECT lists.
    let QueryExpr::Project { cols, child } = result else {
        panic!("expected Project at root, got {:?}", result);
    };
    assert_eq!(cols.len(), 2);
    assert!(matches!(child.expr, QueryExpr::Scan { .. }));
}

// ── Tests: Filter / time extraction ──────────────────────────────────────────

#[tokio::test]
async fn test_non_time_filter_stays_as_filter() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    // region is not the time column → stays as Filter
    let result = lowerer
        .lower("SELECT ts FROM metrics WHERE region = 'us-east'")
        .await
        .unwrap();

    fn has_filter(e: &QueryExpr) -> bool {
        match e {
            QueryExpr::Filter { .. } => true,
            QueryExpr::Project { child, .. } => has_filter(&child.expr),
            _ => false,
        }
    }
    assert!(has_filter(&result), "expected a Filter node");
}

#[tokio::test]
async fn test_time_predicate_extracted_to_source() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    // ts is the time column → extracted into Source::Table.time_range
    let result = lowerer
        .lower("SELECT value FROM metrics WHERE ts > 1000 AND ts < 2000")
        .await
        .unwrap();

    let source = find_source(&result).unwrap();
    let Source::Table { time_range, .. } = source else {
        panic!("expected Source::Table");
    };
    let tr = time_range.as_ref().expect("expected time_range to be Some");
    assert_eq!(tr.start_ms, Some(1000));
    assert_eq!(tr.end_ms, Some(2000));
}

#[tokio::test]
async fn test_time_predicate_no_filter_wrapper() {
    // When ALL predicates are time bounds, no Filter node should appear.
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT value FROM metrics WHERE ts > 1000 AND ts < 2000")
        .await
        .unwrap();

    fn has_filter(e: &QueryExpr) -> bool {
        match e {
            QueryExpr::Filter { .. } => true,
            QueryExpr::Project { child, .. } => has_filter(&child.expr),
            _ => false,
        }
    }
    assert!(
        !has_filter(&result),
        "expected no Filter node when only time predicates"
    );
}

#[tokio::test]
async fn test_mixed_filter_keeps_filter_and_time_range() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT value FROM metrics WHERE ts > 1000 AND region = 'eu'")
        .await
        .unwrap();

    // The Source should have a time_range...
    let source = find_source(&result).unwrap();
    let Source::Table { time_range, .. } = source else {
        panic!("expected Source::Table");
    };
    assert!(time_range.is_some(), "expected time_range extracted");

    // ...and there should also be a Filter node for the non-time predicate.
    fn has_filter(e: &QueryExpr) -> bool {
        match e {
            QueryExpr::Filter { .. } => true,
            QueryExpr::Project { child, .. } => has_filter(&child.expr),
            _ => false,
        }
    }
    assert!(
        has_filter(&result),
        "expected Filter node for non-time predicate"
    );
}

// ── Tests: Aggregates ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_count_star_exact() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer.lower("SELECT COUNT(*) FROM metrics").await.unwrap();

    let (by, aggs) = find_aggregate(&result).expect("expected Aggregate");
    assert!(by.is_empty(), "no GROUP BY expected");
    assert_eq!(aggs.len(), 1);
    assert!(matches!(
        aggs[0],
        AggIntent::Count {
            accuracy: AccuracyTarget::Exact
        }
    ));
}

#[tokio::test]
async fn test_count_inherits_accuracy() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Epsilon(0.01));
    let result = lowerer.lower("SELECT COUNT(*) FROM metrics").await.unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert!(
        matches!(aggs[0], AggIntent::Count { accuracy: AccuracyTarget::Epsilon(e) } if (e - 0.01).abs() < 1e-12)
    );
}

#[tokio::test]
async fn test_sum_aggregate() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT SUM(value) FROM metrics")
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert_eq!(aggs.len(), 1);
    assert!(matches!(aggs[0], AggIntent::Sum));
}

#[tokio::test]
async fn test_min_max_aggregates() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT MIN(value), MAX(value) FROM metrics")
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert_eq!(aggs.len(), 2);
    assert!(aggs.iter().any(|a| matches!(a, AggIntent::Min)));
    assert!(aggs.iter().any(|a| matches!(a, AggIntent::Max)));
}

#[tokio::test]
async fn test_avg_aggregate() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT AVG(value) FROM metrics")
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert!(aggs.iter().any(|a| matches!(a, AggIntent::Avg)));
}

#[tokio::test]
async fn test_stddev_sample() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT STDDEV(value) FROM metrics")
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert!(aggs
        .iter()
        .any(|a| matches!(a, AggIntent::Stddev { population: false })));
}

#[tokio::test]
async fn test_group_by_extracted() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT region, COUNT(*) FROM metrics GROUP BY region")
        .await
        .unwrap();

    let (by, aggs) = find_aggregate(&result).unwrap();
    assert_eq!(by.len(), 1);
    assert_eq!(by[0].0, "region");
    assert!(matches!(aggs[0], AggIntent::Count { .. }));
}

#[tokio::test]
async fn test_multiple_aggregates_in_one_node() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT COUNT(*), SUM(value), MIN(value), MAX(value) FROM metrics")
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert_eq!(aggs.len(), 4);
}

// ── Tests: Approximate intents ────────────────────────────────────────────────

#[tokio::test]
async fn test_count_distinct_becomes_cardinality() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT COUNT(DISTINCT host) FROM metrics")
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert!(aggs
        .iter()
        .any(|a| matches!(a, AggIntent::Cardinality { .. })));
}

#[tokio::test]
async fn test_approx_percentile_becomes_quantile() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(
        &catalog,
        AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        },
    );
    let result = lowerer
        .lower("SELECT approx_percentile_cont(value, 0.99) FROM metrics")
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).unwrap();
    assert!(aggs
        .iter()
        .any(|a| matches!(a, AggIntent::Quantile { q, .. } if (*q - 0.99).abs() < 1e-12)));
}

// ── Test: TopK heavy-hitter pattern ──────────────────────────────────────────

#[tokio::test]
async fn test_order_by_desc_limit_becomes_topk() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower(
            "SELECT host, COUNT(*) AS cnt FROM metrics \
             GROUP BY host ORDER BY cnt DESC LIMIT 10",
        )
        .await
        .unwrap();

    let (_, aggs) = find_aggregate(&result).expect("expected Aggregate");
    assert_eq!(aggs.len(), 1, "TopK should produce exactly one AggIntent");
    let AggIntent::TopK { k, by, .. } = &aggs[0] else {
        panic!("expected TopK, got {:?}", aggs[0]);
    };
    assert_eq!(*k, 10);
    assert!(by.iter().any(|c| c.0 == "host"));
}

// ── Test: Window functions ────────────────────────────────────────────────────

#[tokio::test]
async fn test_window_function_produces_window_func_node() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower(
            "SELECT region, AVG(value) OVER (PARTITION BY region ORDER BY ts) \
             FROM metrics",
        )
        .await
        .unwrap();

    fn has_window_func(e: &QueryExpr) -> bool {
        match e {
            QueryExpr::WindowFunc { .. } => true,
            QueryExpr::Project { child, .. } => has_window_func(&child.expr),
            _ => false,
        }
    }
    assert!(has_window_func(&result), "expected a WindowFunc node");
}

// ── Tests: Error cases ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_join_returns_error() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let err = lowerer
        .lower("SELECT a.ts FROM metrics a JOIN metrics b ON a.ts = b.ts")
        .await
        .unwrap_err();
    assert!(
        matches!(err, LoweringError::UnsupportedFeature(ref msg) if msg.contains("JOIN")),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_subquery_returns_error() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let err = lowerer
        .lower("SELECT * FROM (SELECT value FROM metrics) sub")
        .await
        .unwrap_err();
    assert!(
        matches!(err, LoweringError::UnsupportedFeature(ref msg) if msg.to_lowercase().contains("subquery")),
        "unexpected error: {err}"
    );
}

// ── Tests: Predicate / ProjectItem / SortKey content ─────────────────────────

#[tokio::test]
async fn test_filter_predicate_references_filtered_column() {
    // WHERE value > 5.0 should produce a Predicate that references "value".
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT ts FROM metrics WHERE value > 5.0")
        .await
        .unwrap();

    let pred = find_predicate(&result).expect("expected Filter predicate");
    let refs = pred.columns_referenced();
    assert!(
        refs.iter().any(|r| r.0 == "value"),
        "expected 'value' column ref in predicate"
    );
}

#[tokio::test]
async fn test_filter_two_non_time_predicates_is_bool_and() {
    // WHERE value > 0 AND region = 'us' — both non-time → BoolAnd with 2 conjuncts.
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT ts FROM metrics WHERE value > 0 AND region = 'us'")
        .await
        .unwrap();

    let pred = find_predicate(&result).expect("expected Filter predicate");
    assert_eq!(
        pred.conjuncts().len(),
        2,
        "expected BoolAnd with 2 conjuncts, got: {pred:?}"
    );
}

#[tokio::test]
async fn test_project_items_carry_column_refs() {
    // SELECT id, value FROM events → two ProjectItems, each with a Column expr.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer.lower("SELECT id, value FROM events").await.unwrap();

    let items = find_project_items(&result).expect("expected Project node");
    assert_eq!(items.len(), 2, "expected 2 ProjectItems");

    let col_names: Vec<&str> = items
        .iter()
        .filter_map(|pi| {
            if let L3Expr::Column(c) = &pi.expr {
                Some(c.0.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(col_names.contains(&"id"), "expected 'id' in project items");
    assert!(
        col_names.contains(&"value"),
        "expected 'value' in project items"
    );
}

#[tokio::test]
async fn test_sort_key_ascending_flag() {
    // ORDER BY id ASC → SortKey with ascending = true.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events ORDER BY id ASC")
        .await
        .unwrap();

    let keys = find_sort_keys(&result).expect("expected Sort node");
    assert_eq!(keys.len(), 1);
    assert!(keys[0].ascending, "expected ascending sort key");
}

#[tokio::test]
async fn test_sort_key_descending_flag() {
    // ORDER BY value DESC → SortKey with ascending = false.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events ORDER BY value DESC")
        .await
        .unwrap();

    let keys = find_sort_keys(&result).expect("expected Sort node");
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].ascending, "expected descending sort key");
}

// ── Tests: df_expr_to_l3 edge cases ──────────────────────────────────────────

#[tokio::test]
async fn test_filter_is_null_predicate() {
    // WHERE value IS NULL → IsNull(Column("value"))
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE value IS NULL")
        .await
        .unwrap();

    let pred = find_predicate(&result).expect("expected Filter predicate");
    assert!(
        matches!(pred, L3Expr::IsNull(inner) if matches!(inner.as_ref(), L3Expr::Column(c) if c.0 == "value")),
        "expected IsNull(Column(\"value\")), got: {pred:?}"
    );
}

#[tokio::test]
async fn test_filter_in_list_predicate() {
    // WHERE id IN (1, 2, 3) → InList { expr: Column("id"), list: [..], negated: false }
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE id IN (1, 2, 3)")
        .await
        .unwrap();

    let pred = find_predicate(&result).expect("expected Filter predicate");
    let L3Expr::InList {
        expr,
        list,
        negated,
    } = pred
    else {
        panic!("expected InList, got: {pred:?}");
    };
    assert!(matches!(expr.as_ref(), L3Expr::Column(c) if c.0 == "id"));
    assert_eq!(list.len(), 3);
    assert!(!negated);
}

#[tokio::test]
async fn test_filter_between_normalizes_to_bool_and() {
    // WHERE value BETWEEN 0 AND 100 → BoolAnd([Compare(Ge, 0), Compare(Le, 100)])
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE value BETWEEN 0.0 AND 100.0")
        .await
        .unwrap();

    let pred = find_predicate(&result).expect("expected Filter predicate");
    let conjuncts = pred.conjuncts();
    assert_eq!(
        conjuncts.len(),
        2,
        "BETWEEN should produce 2 conjuncts, got: {pred:?}"
    );
    // First conjunct: value >= 0, second: value <= 100
    assert!(matches!(
        &conjuncts[0],
        L3Expr::Compare {
            op: CompareOp::Ge,
            ..
        }
    ));
    assert!(matches!(
        &conjuncts[1],
        L3Expr::Compare {
            op: CompareOp::Le,
            ..
        }
    ));
}

// ── Tests: lower_batch ────────────────────────────────────────────────────────

fn make_workload(queries: Vec<&str>) -> QueryWorkload {
    QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        query_batch: Some(
            queries
                .into_iter()
                .map(|q| BatchEntry {
                    query: Query(q.to_string()),
                    requirements: None,
                })
                .collect(),
        ),
        repeating_queries: None,
        data_characteristics: None,
    }
}

#[tokio::test]
async fn test_lower_batch_empty_returns_empty_vec() {
    let catalog = metrics_catalog();
    let workload = QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        query_batch: None,
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_batch(&workload, &catalog).await;
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_lower_batch_two_valid_queries() {
    let catalog = metrics_catalog();
    let workload = make_workload(vec![
        "SELECT COUNT(*) FROM metrics",
        "SELECT SUM(value) FROM metrics",
    ]);
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok(), "first query should succeed");
    assert!(results[1].is_ok(), "second query should succeed");
}

#[tokio::test]
async fn test_lower_batch_error_is_per_query() {
    // A bad query in the batch should not prevent the good ones from being lowered.
    let catalog = metrics_catalog();
    let workload = make_workload(vec!["SELECT COUNT(*) FROM metrics", "NOT VALID SQL !!!"]);
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok(), "first (valid) query should succeed");
    assert!(results[1].is_err(), "second (invalid) query should fail");
}

#[tokio::test]
async fn test_unknown_table_returns_error() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let err = lowerer
        .lower("SELECT x FROM ghost_table")
        .await
        .unwrap_err();
    // DataFusion will reject the unknown table during planning
    assert!(
        matches!(
            err,
            LoweringError::DataFusion(_) | LoweringError::TableNotFound(_)
        ),
        "unexpected error variant: {err}"
    );
}

// ── Tests: language guard ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_promql_workload_returns_wrong_language() {
    let catalog = metrics_catalog();
    let workload = QueryWorkload {
        language: QueryLanguage::PromQL,
        query_batch: Some(vec![BatchEntry {
            query: Query("some_metric".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], Err(LoweringError::WrongLanguage(_))),
        "expected WrongLanguage, got: {:?}",
        results[0]
    );
}

#[tokio::test]
async fn test_elastic_dsl_workload_returns_wrong_language() {
    let catalog = metrics_catalog();
    let workload = QueryWorkload {
        language: QueryLanguage::ElasticDSL,
        query_batch: Some(vec![BatchEntry {
            query: Query("{\"query\":{}}".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0], Err(LoweringError::WrongLanguage(_))));
}

#[tokio::test]
async fn test_datafusion_language_accepted() {
    let catalog = metrics_catalog();
    let workload = QueryWorkload {
        language: QueryLanguage::DataFusion,
        query_batch: Some(vec![BatchEntry {
            query: Query("SELECT COUNT(*) FROM metrics".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_ok(), "DataFusion language should be accepted");
}

// ── Tests: BETWEEN time extraction ───────────────────────────────────────────

#[tokio::test]
async fn test_time_between_extracted_to_source() {
    // WHERE ts BETWEEN 1000 AND 2000 should set start_ms=1000, end_ms=2000.
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT value FROM metrics WHERE ts BETWEEN 1000 AND 2000")
        .await
        .unwrap();

    let source = find_source(&result).unwrap();
    let Source::Table { time_range, .. } = source else {
        panic!("expected Source::Table");
    };
    let tr = time_range
        .as_ref()
        .expect("expected time_range from BETWEEN");
    assert_eq!(tr.start_ms, Some(1000));
    assert_eq!(tr.end_ms, Some(2000));
}

#[tokio::test]
async fn test_time_between_leaves_no_filter_wrapper() {
    // A BETWEEN-only predicate on the time column has no non-time residual,
    // so no Filter node should appear.
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT value FROM metrics WHERE ts BETWEEN 1000 AND 2000")
        .await
        .unwrap();

    fn has_filter(e: &QueryExpr) -> bool {
        match e {
            QueryExpr::Filter { .. } => true,
            QueryExpr::Project { child, .. } => has_filter(&child.expr),
            _ => false,
        }
    }
    assert!(
        !has_filter(&result),
        "BETWEEN on time col should leave no Filter wrapper"
    );
}

#[tokio::test]
async fn test_time_between_mixed_with_non_time_predicate() {
    // WHERE ts BETWEEN 1000 AND 2000 AND region = 'us':
    // time range should be extracted; Filter stays for the non-time conjunct.
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT value FROM metrics WHERE ts BETWEEN 1000 AND 2000 AND region = 'us'")
        .await
        .unwrap();

    let source = find_source(&result).unwrap();
    let Source::Table { time_range, .. } = source else {
        panic!("expected Source::Table");
    };
    let tr = time_range.as_ref().expect("expected time_range");
    assert_eq!(tr.start_ms, Some(1000));
    assert_eq!(tr.end_ms, Some(2000));

    fn has_filter(e: &QueryExpr) -> bool {
        match e {
            QueryExpr::Filter { .. } => true,
            QueryExpr::Project { child, .. } => has_filter(&child.expr),
            _ => false,
        }
    }
    assert!(
        has_filter(&result),
        "expected Filter for non-time predicate"
    );
}

// ── Tests: multi-dialect SQL ──────────────────────────────────────────────────

#[tokio::test]
async fn test_clickhouse_dialect_returns_unsupported_dialect() {
    let catalog = metrics_catalog();
    let workload = QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::ClickhouseSQL),
        query_batch: Some(vec![BatchEntry {
            query: Query("SELECT COUNT(*) FROM metrics".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], Err(LoweringError::UnsupportedDialect(_))),
        "expected UnsupportedDialect, got: {:?}",
        results[0]
    );
}

#[tokio::test]
async fn test_elastic_sql_dialect_returns_unsupported_dialect() {
    let catalog = metrics_catalog();
    let workload = QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::ElasticSQL),
        query_batch: Some(vec![BatchEntry {
            query: Query("{\"query\":{}}".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], Err(LoweringError::UnsupportedDialect(_))),
        "expected UnsupportedDialect, got: {:?}",
        results[0]
    );
}

#[tokio::test]
async fn test_datafusion_sql_dialect_accepted() {
    let catalog = metrics_catalog();
    let workload = QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        query_batch: Some(vec![BatchEntry {
            query: Query("SELECT COUNT(*) FROM metrics".into()),
            requirements: None,
        }]),
        repeating_queries: None,
        data_characteristics: None,
    };
    let results = lower_batch(&workload, &catalog).await;
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_ok(),
        "SQL(DataFusionSQL) should be accepted, got: {:?}",
        results[0]
    );
}

// ── Tests: Source::Table.columns ─────────────────────────────────────────────

#[tokio::test]
async fn test_scan_columns_populated_from_projection() {
    // SELECT ts, value FROM metrics — DataFusion should push the projection
    // into the TableScan; Source::Table.columns should name the two columns.
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT ts, value FROM metrics")
        .await
        .unwrap();

    let source = find_source(&result).expect("expected a Scan");
    let Source::Table { columns, .. } = source else {
        panic!("expected Source::Table");
    };
    // At minimum, each named column should appear in the list.
    let names: Vec<&str> = columns.iter().map(|c| c.0.as_str()).collect();
    assert!(
        names.contains(&"ts"),
        "expected 'ts' in columns, got: {names:?}"
    );
    assert!(
        names.contains(&"value"),
        "expected 'value' in columns, got: {names:?}"
    );
}

#[tokio::test]
async fn test_scan_columns_empty_for_select_star() {
    // SELECT * — no projection pushdown; Source::Table.columns stays empty
    // (meaning "all columns"; cost estimator treats empty as unconstrained).
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer.lower("SELECT * FROM metrics").await.unwrap();

    let source = find_source(&result).expect("expected a Scan");
    let Source::Table { columns, .. } = source else {
        panic!("expected Source::Table");
    };
    assert!(
        columns.is_empty(),
        "SELECT * should produce no column constraints, got: {columns:?}"
    );
}

// ── Tests: UNION / UNION ALL ──────────────────────────────────────────────────

#[tokio::test]
async fn test_union_all_produces_set_op_union_node() {
    // UNION ALL — no dedup — maps to SetOp { kind: Union, all: true }.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events UNION ALL SELECT id FROM events")
        .await
        .unwrap();

    fn find_set_op(e: &QueryExpr) -> Option<(&SetOpKind, bool)> {
        match e {
            QueryExpr::SetOp { kind, all, .. } => Some((kind, *all)),
            QueryExpr::Project { child, .. } => find_set_op(&child.expr),
            _ => None,
        }
    }
    let (kind, all) = find_set_op(&result).expect("expected SetOp node");
    assert!(matches!(kind, SetOpKind::Union));
    assert!(all, "UNION ALL should set all=true");
}

#[tokio::test]
async fn test_union_distinct_produces_distinct_over_set_op() {
    // UNION (without ALL) = UNION DISTINCT = Distinct wrapping SetOp.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events UNION SELECT id FROM events")
        .await
        .unwrap();

    fn has_distinct(e: &QueryExpr) -> bool {
        match e {
            QueryExpr::Distinct { .. } => true,
            QueryExpr::Project { child, .. } => has_distinct(&child.expr),
            _ => false,
        }
    }
    assert!(
        has_distinct(&result),
        "UNION DISTINCT should produce a Distinct node"
    );
}

// ── Tests: arithmetic / negative / LIKE / CASE in expressions ────────────────

#[tokio::test]
async fn test_arithmetic_in_projection_succeeds() {
    // SELECT value * 2 FROM events — should not return UnsupportedFeature.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer.lower("SELECT value * 2.0 FROM events").await;
    assert!(
        result.is_ok(),
        "arithmetic in projection should succeed, got: {result:?}"
    );
}

#[tokio::test]
async fn test_arithmetic_in_predicate_succeeds() {
    // WHERE value * 0.9 > 5.0 — should not return UnsupportedFeature.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE value * 0.9 > 5.0")
        .await;
    assert!(
        result.is_ok(),
        "arithmetic in predicate should succeed, got: {result:?}"
    );
}

#[tokio::test]
async fn test_arithmetic_predicate_references_column() {
    // Predicate from `value * 0.9 > 5` should still reference "value".
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE value * 0.9 > 5.0")
        .await
        .unwrap();
    let pred = find_predicate(&result).expect("expected Filter predicate");
    let refs = pred.columns_referenced();
    assert!(
        refs.iter().any(|r| r.0 == "value"),
        "expected 'value' in predicate refs"
    );
}

#[tokio::test]
async fn test_negative_literal_in_predicate_succeeds() {
    // WHERE value > -1 — unary minus should not return UnsupportedFeature.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE value > -1.0")
        .await;
    assert!(
        result.is_ok(),
        "unary minus in predicate should succeed, got: {result:?}"
    );
}

#[tokio::test]
async fn test_like_predicate_produces_compare_like() {
    // WHERE name LIKE 'a%' → Compare { op: Like, .. }
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE name LIKE 'a%'")
        .await
        .unwrap();
    let pred = find_predicate(&result).expect("expected Filter predicate");
    assert!(
        matches!(
            pred,
            L3Expr::Compare {
                op: CompareOp::Like,
                ..
            }
        ),
        "expected Compare(Like), got: {pred:?}"
    );
}

#[tokio::test]
async fn test_ilike_predicate_produces_compare_ilike() {
    // WHERE name ILIKE 'A%' → Compare { op: ILike, .. }
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT id FROM events WHERE name ILIKE 'A%'")
        .await
        .unwrap();
    let pred = find_predicate(&result).expect("expected Filter predicate");
    assert!(
        matches!(
            pred,
            L3Expr::Compare {
                op: CompareOp::ILike,
                ..
            }
        ),
        "expected Compare(ILike), got: {pred:?}"
    );
}

#[tokio::test]
async fn test_case_in_projection_succeeds() {
    // CASE WHEN value > 5 THEN 1 ELSE 0 END — should not return UnsupportedFeature.
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT CASE WHEN value > 5.0 THEN 1 ELSE 0 END AS tier FROM events")
        .await;
    assert!(
        result.is_ok(),
        "CASE WHEN in projection should succeed, got: {result:?}"
    );
}

#[tokio::test]
async fn test_case_projection_item_is_case_expr() {
    let catalog = no_time_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let result = lowerer
        .lower("SELECT CASE WHEN value > 5.0 THEN 1 ELSE 0 END AS tier FROM events")
        .await
        .unwrap();
    let items = find_project_items(&result).expect("expected Project node");
    let has_case = items
        .iter()
        .any(|pi| matches!(pi.expr, L3Expr::Case { .. }));
    assert!(
        has_case,
        "expected a Case expr in project items, got: {items:?}"
    );
}

// ── Tests: populate_schemas ───────────────────────────────────────────────────

#[tokio::test]
async fn test_populate_schemas_scan_gets_catalog_schema() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let expr = lowerer.lower("SELECT * FROM metrics").await.unwrap();

    let typed = populate_schemas(expr, &catalog);
    assert_eq!(
        typed.schema.fields.len(),
        4,
        "scan schema should have 4 fields from catalog"
    );
    assert_eq!(typed.schema.fields[0].name, "ts");
    assert_eq!(typed.schema.time_index, Some(0));
}

#[tokio::test]
async fn test_populate_schemas_project_gives_subset_schema() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let expr = lowerer
        .lower("SELECT ts, value FROM metrics")
        .await
        .unwrap();

    let typed = populate_schemas(expr, &catalog);
    assert_eq!(typed.schema.fields.len(), 2);
    assert_eq!(typed.schema.fields[0].name, "ts");
    assert_eq!(typed.schema.fields[1].name, "value");
    assert_eq!(typed.schema.time_index, Some(0));
}

#[tokio::test]
async fn test_populate_schemas_inner_nodes_are_typed() {
    // The child of the root Project (a Scan) should also have a non-empty schema.
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let expr = lowerer
        .lower("SELECT ts, value FROM metrics")
        .await
        .unwrap();

    let typed = populate_schemas(expr, &catalog);
    let QueryExpr::Project { child, .. } = &typed.expr else {
        panic!("expected Project at root");
    };
    assert!(
        !child.schema.fields.is_empty(),
        "child Scan should have a populated schema after populate_schemas"
    );
}

#[tokio::test]
async fn test_populate_schemas_aggregate() {
    let catalog = metrics_catalog();
    let lowerer = SqlLowerer::new(&catalog, AccuracyTarget::Exact);
    let expr = lowerer
        .lower("SELECT region, COUNT(*) FROM metrics GROUP BY region")
        .await
        .unwrap();

    let typed = populate_schemas(expr, &catalog);
    // Root is Project wrapping Aggregate; two fields: region + count col.
    assert_eq!(typed.schema.fields.len(), 2);
    assert_eq!(typed.schema.fields[0].name, "region");
}
