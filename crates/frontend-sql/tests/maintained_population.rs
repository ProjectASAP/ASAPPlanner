//! SQL and PromQL use the same shared-state rule without sharing membership semantics.
use asap_aware_mapping::maintained_population::MaintainedPopulationStrategy;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::{
    ir::{
        apply_materialization_timings, cse::share_common_sub_dags,
        physical_export::compile_physical_asap_dag, ASAPOp, MaterializationAssignment, NonASAPOp, Operator,
        OperatorNode, TimingMemo},
    post_asap::maintained_population::{MaintainedPopulation, PopulationInput},
    pre_asap::{DataType, Field, Schema},
    types::AccuracyTarget};
use std::rc::Rc;

async fn aggregate(q: &str) -> Rc<OperatorNode> {
    let catalog = SqlCatalog::new().with_table(
        "samples",
        Schema::new(vec![
            Field::plain("latency", DataType::Float64, false),
            Field::plain("job", DataType::Utf8, false),
        ]),
    );
    lower_sql(q, &catalog, AccuracyTarget::Exact).await.unwrap()
}

/// The `MaintainPopulation` node a candidate's evaluation reads, and its spec.
fn population(mut node: &OperatorNode) -> (&Rc<OperatorNode>, &MaintainedPopulation) {
    while let Operator::NonASAP(NonASAPOp::Project { child, .. }) = &node.operator {
        node = child;
    }
    let Operator::ASAP(ASAPOp::EvaluatePopulation { child, .. }) = &node.operator else {
        panic!("evaluation")
    };
    let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &child.operator else {
        panic!("state")
    };
    (child, population)
}

/// Export `plan` the way the planner does: assign the default materialization
/// timings, then compile the timed DAG.
fn compile(plan: &Rc<OperatorNode>) -> Result<(), String> {
    let timed = apply_materialization_timings(
        plan,
        &MaterializationAssignment::all_query_time(),
        &mut TimingMemo::new(),
    )
    .map_err(|e| e.to_string())?;
    compile_physical_asap_dag(&timed)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// Quantile parameters are evaluation identity, while source, value column and grouping are state identity.
#[tokio::test]
async fn sql_quantiles_share_rows_without_promql_lookback() {
    let roots = vec![
        aggregate("SELECT median(latency) FROM samples").await,
        aggregate("SELECT approx_percentile_cont(latency, 0.99) FROM samples").await,
    ];
    let rule = MaintainedPopulationStrategy::new(&roots);
    let plans = share_common_sub_dags(
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| (i, rule.candidate(r).expect("table population")))
            .collect(),
    );
    for (_, plan) in &plans {
        compile(plan).unwrap();
    }
    let (a, spec) = population(&plans[0].1);
    let (b, _) = population(&plans[1].1);
    assert!(Rc::ptr_eq(a, b));
    assert!(matches!(
        spec.input,
        PopulationInput::Rows {
            value_column: 0,
            ..
        }
    ));
}

// Different GROUP BY populations must not be merged just because they read the same table.
#[tokio::test]
async fn sql_grouping_separates_populations() {
    let roots = vec![
        aggregate("SELECT median(latency) FROM samples").await,
        aggregate("SELECT job, median(latency) FROM samples GROUP BY job").await,
    ];
    let rule = MaintainedPopulationStrategy::new(&roots);
    let a = rule.candidate(&roots[0]).unwrap();
    let b = rule.candidate(&roots[1]).unwrap();
    assert_ne!(population(&a).1.input, population(&b).1.input);
}

// Input predicates and value expressions remain part of sharing identity.
#[tokio::test]
async fn sql_filters_separate_populations() {
    let roots = vec![
        aggregate("SELECT median(latency) FROM samples WHERE job = 'api'").await,
        aggregate("SELECT median(latency) FROM samples WHERE job = 'db'").await,
    ];
    let rule = MaintainedPopulationStrategy::new(&roots);
    let a = rule.candidate(&roots[0]).expect("filtered table input");
    let b = rule.candidate(&roots[1]).expect("filtered table input");
    assert_ne!(population(&a).1.input, population(&b).1.input);
}

// All four scalar evaluations can share the same non-null numeric SQL population.
#[tokio::test]
async fn sql_scalar_evaluations_share_membership() {
    let mut roots = Vec::new();
    for function in [
        "median(latency)",
        "sum(latency)",
        "avg(latency)",
        "count(*)",
    ] {
        roots.push(aggregate(&format!("SELECT {function} FROM samples")).await);
    }
    let rule = MaintainedPopulationStrategy::new(&roots);
    let plans = share_common_sub_dags(
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| (i, rule.candidate(r).expect("scalar population")))
            .collect(),
    );
    for (_, plan) in &plans {
        compile(plan).unwrap();
        assert!(Rc::ptr_eq(population(&plans[0].1).0, population(plan).0));
    }
}

// A evaluation cannot reinterpret a label column as its numeric population.
#[tokio::test]
async fn malformed_table_population_fails_validation() {
    let root = aggregate("SELECT median(latency) FROM samples").await;
    let rule = MaintainedPopulationStrategy::new(std::slice::from_ref(&root));
    let mut candidate = rule.candidate(&root).unwrap();
    let Operator::NonASAP(NonASAPOp::Project { child, .. }) =
        &mut Rc::make_mut(&mut candidate).operator
    else {
        unreachable!()
    };
    let Operator::ASAP(ASAPOp::EvaluatePopulation { child, .. }) =
        &mut Rc::make_mut(child).operator
    else {
        unreachable!()
    };
    let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) =
        &mut Rc::make_mut(child).operator
    else {
        unreachable!()
    };
    let PopulationInput::Rows { value_column, .. } = &mut population.input else {
        unreachable!()
    };
    *value_column = 1;
    assert!(compile(&candidate).is_err());
}

// SQL ORDER BY value DESC LIMIT k uses the same maximum-k state contract.
#[tokio::test]
async fn sql_topk_limits_share_maximum_k() {
    let roots = vec![
        aggregate("SELECT * FROM samples ORDER BY latency DESC LIMIT 1").await,
        aggregate("SELECT * FROM samples ORDER BY latency DESC LIMIT 5").await,
    ];
    let rule = MaintainedPopulationStrategy::new(&roots);
    let plans = share_common_sub_dags(
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| (i, rule.candidate(r).expect("SQL topk")))
            .collect(),
    );
    for (_, plan) in &plans {
        compile(plan).unwrap();
        assert_eq!(population(plan).1.max_k, 5);
        assert!(Rc::ptr_eq(population(&plans[0].1).0, population(plan).0));
    }
}

// A SELECT list that keeps the table's column order is the same top-k as `*`.
#[tokio::test]
async fn sql_topk_over_an_identity_select_list_is_recognized() {
    let root = aggregate("SELECT latency, job FROM samples ORDER BY latency DESC LIMIT 5").await;
    let rule = MaintainedPopulationStrategy::new(std::slice::from_ref(&root));
    let plan = rule.candidate(&root).expect("SQL topk");
    compile(&plan).unwrap();
    assert_eq!(population(&plan).1.max_k, 5);
}

// A reordering projection moves the Sort key's column, so it is not skipped.
#[tokio::test]
async fn sql_topk_over_a_reordering_select_list_is_not_recognized() {
    let root = aggregate("SELECT job, latency FROM samples ORDER BY latency DESC LIMIT 5").await;
    let rule = MaintainedPopulationStrategy::new(std::slice::from_ref(&root));
    assert!(rule.candidate(&root).is_none());
}
