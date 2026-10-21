//! Correlation retains both arguments through SQL lowering and exact planning.
use std::rc::Rc;

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{
    apply_materialization_timings, physical_export::compile_physical_asap_dag,
    MaterializationAssignment, NonASAPOp, OperatorNode, ScalarExpr, TimingMemo,
};

fn catalog() -> SqlCatalog {
    let schema = Schema::new(vec![
        Field::plain("x", DataType::Float64, true),
        Field::plain("y", DataType::Float64, true),
        Field::plain("g", DataType::Int64, false),
    ]);
    SqlCatalog::new()
        .with_table("a", schema.clone())
        .with_table("b", schema)
}

async fn lower(sql: &str) -> Rc<OperatorNode> {
    lower_sql(sql, &catalog(), AccuracyTarget::exact())
        .await
        .unwrap()
}

fn aggregate(query: &OperatorNode) -> (&[AggIntent], &OperatorNode) {
    match query.expect_non_asap() {
        NonASAPOp::Aggregate {
            measures, child, ..
        } => (measures, child),
        NonASAPOp::Project { child, .. }
        | NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. } => aggregate(child),
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
        assert_eq!(measures, &[AggIntent::PearsonCorr { left: 0, right: 1 }]);
        let NonASAPOp::Project { cols, .. } = child.expect_non_asap() else {
            panic!("derived inputs")
        };
        assert_eq!(cols.len(), 2);
        assert!(cols
            .iter()
            .any(|col| !matches!(col.expr, ScalarExpr::Column(_))));
        assert_eq!(query.schema.fields[0].dtype, DataType::Float64);
    }
}

// Identically named join columns remain distinct even when projected beneath the aggregate.
#[tokio::test]
async fn corr_preserves_qualified_join_inputs() {
    let query = lower("SELECT corr(a.x, b.x) FROM a JOIN b ON a.g = b.g").await;
    let (measures, child) = aggregate(&query);
    assert_eq!(measures[0].input_cols(), vec![0, 1]);
    let NonASAPOp::Project { cols, .. } = child.expect_non_asap() else {
        panic!("paired projection")
    };
    assert_eq!(cols[0].expr, ScalarExpr::Column(0));
    assert_eq!(cols[1].expr, ScalarExpr::Column(3));
}

// Grouping and sibling reducers cannot drop either correlation argument.
#[tokio::test]
async fn corr_coexists_with_grouping_having_and_other_measures() {
    let query = lower("SELECT g, corr(x, y) AS r, sum(x * y) AS s FROM a GROUP BY g HAVING corr(x, y) > 0 ORDER BY r").await;
    let (measures, child) = aggregate(&query);
    let pair = measures
        .iter()
        .find(|m| matches!(m, AggIntent::PearsonCorr { .. }))
        .unwrap();
    let schema = &child.schema;
    for id in pair.input_cols() {
        assert!(id < schema.fields.len());
    }
    assert!(measures.iter().any(|m| matches!(m, AggIntent::Sum { .. })));
    let output = &query.schema;
    assert_eq!(output.fields[1].name, "r");
    assert_eq!(output.fields[1].dtype, DataType::Float64);
    assert!(output.fields[1].nullable);
}

// Repeated inputs reuse their value while retaining two argument positions.
#[tokio::test]
async fn corr_repeated_input_and_serialization() {
    let query = lower("SELECT corr(x, x) FROM a").await;
    assert_eq!(aggregate(&query).0[0].input_cols(), vec![0, 0]);
    let encoded = serde_json::to_string(&query).unwrap();
    let decoded: OperatorNode = serde_json::from_str(&encoded).unwrap();
    assert_eq!(*query, decoded);
}

// Unsupported modifiers and window calls fail instead of silently changing semantics.
#[tokio::test]
async fn corr_rejects_unrepresented_forms() {
    for sql in [
        "SELECT corr(DISTINCT x, y) FROM a",
        "SELECT corr(x, y ORDER BY g) FROM a",
        "SELECT corr(x, y) OVER () FROM a",
        "SELECT corr(x) FROM a",
    ] {
        assert!(
            lower_sql(sql, &catalog(), AccuracyTarget::exact())
                .await
                .is_err(),
            "{sql}"
        );
    }
}

// A `FILTER` clause becomes the measure's own predicate (#466), leaving the
// two inputs untouched.
#[tokio::test]
async fn corr_filter_is_a_measure_filter() {
    let query = lower("SELECT corr(x, y) FILTER (WHERE g > 0) FROM a").await;
    let (measures, _) = aggregate(&query);
    assert_eq!(measures[0].input_cols(), vec![0, 1]);
    fn filters(query: &OperatorNode) -> &[Option<asap_types::ir::Predicate>] {
        match query.expect_non_asap() {
            NonASAPOp::Aggregate { filters, .. } => filters,
            NonASAPOp::Project { child, .. } | NonASAPOp::Filter { child, .. } => filters(child),
            other => panic!("expected aggregate, got {other:?}"),
        }
    }
    assert!(
        matches!(filters(&query), [Some(_)]),
        "{:?}",
        filters(&query)
    );
}

// The plan the planner pipeline selects retains the complete typed query and
// compiles to a physical ASAP DAG.
#[tokio::test]
async fn corr_survives_exact_plan_compilation() {
    let query = lower("SELECT corr(x, y) AS r FROM a").await;
    let run = asap_plan_selection::run_stage1_to_stage3_with(
        vec![(0, asap_types::ir::QueryRoot::Operator(query.clone()))],
        &[asap_types::workload::QueryWorkloadEntry {
            query: Default::default(),
            requirements: asap_types::workload::QueryRequirements {
                accuracy: asap_types::ir::properties::AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Default::default(),
            recurrence: Default::default(),
            range_evaluation: None,
        }],
        &Default::default(),
        asap_plan_selection::PlanningModels::builtin(),
        asap_plan_selection::Stage1ToStage3Search::exhaustive(),
    )
    .unwrap();
    let [asap_types::ir::QueryRoot::Operator(plan)] = run.selected_logical().unwrap() else {
        panic!("one operator root");
    };
    assert!(plan.guarantee.as_ref().unwrap().is_exact());
    // Pass 1 has no candidate for `corr`: the plan is the query's own
    // operator DAG, no ASAP node added.
    assert!(!plan.contains_asap(), "expected the exact plan");
    assert_eq!(aggregate(plan).0, aggregate(&query).0);
    let timed = apply_materialization_timings(
        plan,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    compile_physical_asap_dag(&timed).unwrap();
}
