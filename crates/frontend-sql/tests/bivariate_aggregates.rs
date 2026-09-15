//! Two-input aggregates retain paired arguments through SQL lowering and exact planning.
use std::rc::Rc;

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::pre_asap::{AggIntent, BivariateAggOp, Column, DataType, QueryExpr, Schema};
use asap_types::types::AccuracyTarget;

fn catalog() -> SqlCatalog {
    let schema = Schema::new(vec![
        Column::new("x", DataType::Float64, true),
        Column::new("y", DataType::Float64, true),
        Column::new("g", DataType::Int64, false),
    ]);
    SqlCatalog::new()
        .with_table("a", schema.clone())
        .with_table("b", schema)
}

async fn lower(sql: &str) -> QueryExpr {
    lower_sql(sql, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap()
}

fn aggregate(query: &QueryExpr) -> (&[AggIntent], &QueryExpr) {
    match query {
        QueryExpr::Aggregate {
            measures, child, ..
        } => (measures, child),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Sort { child, .. } => aggregate(child),
        other => panic!("expected aggregate, got {other:?}"),
    }
}

// Expressions in either argument, including casts, survive as projected values.
#[tokio::test]
async fn corr_materializes_both_arguments() {
    for sql in [
        "SELECT corr(x + 1, y * 2) FROM a",
        "SELECT corr(x, y * 2) FROM a",
        "SELECT corr(CAST(g AS DOUBLE), y) FROM a",
        "SELECT corr(x, 1.0) FROM a",
    ] {
        let query = lower(sql).await;
        let (measures, child) = aggregate(&query);
        assert_eq!(
            measures,
            &[AggIntent::Bivariate {
                op: BivariateAggOp::Correlation,
                left: 0,
                right: 1,
            }]
        );
        let QueryExpr::Project { cols, .. } = child else {
            panic!("derived inputs")
        };
        assert_eq!(cols.len(), 2);
        assert!(cols
            .iter()
            .any(|col| !matches!(col.expr, QueryExpr::Column(_))));
        assert_eq!(
            query.output_schema().unwrap().columns[0].dtype,
            DataType::Float64
        );
    }
}

// Identically named join columns remain distinct even when projected beneath the aggregate.
#[tokio::test]
async fn corr_preserves_qualified_join_inputs() {
    let query = lower("SELECT corr(a.x, b.x) FROM a JOIN b ON a.g = b.g").await;
    let (measures, child) = aggregate(&query);
    assert_eq!(measures[0].input_cols(), vec![0, 1]);
    let QueryExpr::Project { cols, .. } = child else {
        panic!("paired projection")
    };
    assert_eq!(cols[0].expr, QueryExpr::Column(0));
    assert_eq!(cols[1].expr, QueryExpr::Column(3));
}

// Grouping and sibling reducers cannot drop either correlation argument.
#[tokio::test]
async fn corr_coexists_with_grouping_having_and_other_measures() {
    let query = lower("SELECT g, corr(x, y) AS r, sum(x * y) AS s FROM a GROUP BY g HAVING corr(x, y) > 0 ORDER BY r").await;
    let (measures, child) = aggregate(&query);
    let pair = measures
        .iter()
        .find(|m| matches!(m, AggIntent::Bivariate { .. }))
        .unwrap();
    let schema = child.output_schema().unwrap();
    for id in pair.input_cols() {
        assert!(id < schema.columns.len());
    }
    assert!(measures.iter().any(|m| matches!(m, AggIntent::Sum { .. })));
    let output = query.output_schema().unwrap();
    assert_eq!(output.columns[1].name, "r");
    assert_eq!(output.columns[1].dtype, DataType::Float64);
    assert!(output.columns[1].nullable);
}

// Repeated inputs reuse their value while retaining two argument positions.
#[tokio::test]
async fn corr_repeated_input_and_serialization() {
    let query = lower("SELECT corr(x, x) FROM a").await;
    assert_eq!(aggregate(&query).0[0].input_cols(), vec![0, 0]);
    let encoded = serde_json::to_string(&query).unwrap();
    let decoded: QueryExpr = serde_json::from_str(&encoded).unwrap();
    assert_eq!(query, decoded);
}

// Unsupported modifiers and window calls fail instead of silently changing semantics.
#[tokio::test]
async fn corr_rejects_unrepresented_forms() {
    for sql in [
        "SELECT corr(DISTINCT x, y) FROM a",
        "SELECT corr(x, y) FILTER (WHERE g > 0) FROM a",
        "SELECT corr(x, y ORDER BY g) FROM a",
        "SELECT corr(x, y) OVER () FROM a",
        "SELECT corr(x) FROM a",
    ] {
        assert!(
            lower_sql(sql, &catalog(), AccuracyTarget::Exact)
                .await
                .is_err(),
            "{sql}"
        );
    }
}

// Exact fallback retains the complete typed query and compiles to an executable DAG.
#[tokio::test]
async fn corr_survives_exact_plan_compilation() {
    let query = Rc::new(lower("SELECT corr(x, y) AS r FROM a").await);
    let plan = asap_aware_mapping::replacement::keep_pre_asap(&query).unwrap();
    assert!(plan.guarantee.as_ref().unwrap().is_exact());
    let asap_types::post_asap::SummaryExpr::KeepPreAsap(retained) = &plan.expr else {
        panic!("expected exact fallback");
    };
    assert_eq!(aggregate(retained).0, aggregate(&query).0);
    asap_types::post_asap::compile_executable_dag(&plan).unwrap();
}
