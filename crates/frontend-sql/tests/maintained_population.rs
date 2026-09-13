//! SQL and PromQL use the same shared-state rule without sharing membership semantics.
use asap_aware_mapping::maintained_population::MaintainedPopulationStrategy;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::{
    post_asap::{
        compile_executable_dag,
        maintained_population::{MaintainedPopulation, PopulationInput},
        share_common_summary_subtrees, SummaryExpr, ValueOperation,
    },
    pre_asap::{Column, DataType, QueryExpr, Schema},
    types::AccuracyTarget,
};
use std::rc::Rc;

async fn aggregate(q: &str) -> Rc<QueryExpr> {
    let catalog = SqlCatalog::new().with_table(
        "samples",
        Schema::new(vec![
            Column::new("latency", DataType::Float64, false),
            Column::new("job", DataType::Utf8, false),
        ]),
    );
    let root = lower_sql(q, &catalog, AccuracyTarget::Exact).await.unwrap();
    Rc::new(root)
}

fn population(
    mut node: &asap_types::post_asap::SummaryNode,
) -> (
    &Rc<asap_types::post_asap::SummaryNode>,
    &MaintainedPopulation,
) {
    while let SummaryExpr::ValueOperation {
        child,
        operation: ValueOperation::Project { .. },
        ..
    } = &node.expr
    {
        node = child;
    }
    let SummaryExpr::ValueOperation { child, .. } = &node.expr else {
        panic!("readout")
    };
    let SummaryExpr::ValueOperation {
        operation: ValueOperation::MaintainPopulation { population },
        ..
    } = &child.expr
    else {
        panic!("state")
    };
    (child, population)
}

// Quantile parameters are readout identity, while source, value column and grouping are state identity.
#[tokio::test]
async fn sql_quantiles_share_rows_without_promql_lookback() {
    let roots = vec![
        aggregate("SELECT median(latency) FROM samples").await,
        aggregate("SELECT approx_percentile_cont(latency, 0.99) FROM samples").await,
    ];
    let rule = MaintainedPopulationStrategy::new(&roots);
    let plans = share_common_summary_subtrees(
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| (i, rule.candidate(r).expect("table population")))
            .collect(),
    );
    for (_, plan) in &plans {
        compile_executable_dag(plan).unwrap();
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

// All four scalar readouts can share the same non-null numeric SQL population.
#[tokio::test]
async fn sql_scalar_readouts_share_membership() {
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
    let plans = share_common_summary_subtrees(
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| (i, rule.candidate(r).expect("scalar population")))
            .collect(),
    );
    for (_, plan) in &plans {
        compile_executable_dag(plan).unwrap();
        assert!(Rc::ptr_eq(population(&plans[0].1).0, population(plan).0));
    }
}

// A readout cannot reinterpret a label column as its numeric population.
#[tokio::test]
async fn malformed_table_population_fails_validation() {
    let root = aggregate("SELECT median(latency) FROM samples").await;
    let rule = MaintainedPopulationStrategy::new(std::slice::from_ref(&root));
    let mut candidate = rule.candidate(&root).unwrap();
    let SummaryExpr::ValueOperation { child, .. } = &mut Rc::make_mut(&mut candidate).expr else {
        unreachable!()
    };
    let SummaryExpr::ValueOperation { child, .. } = &mut Rc::make_mut(child).expr else {
        unreachable!()
    };
    let SummaryExpr::ValueOperation {
        operation: ValueOperation::MaintainPopulation { population },
        ..
    } = &mut Rc::make_mut(child).expr
    else {
        unreachable!()
    };
    let PopulationInput::Rows { value_column, .. } = &mut population.input else {
        unreachable!()
    };
    *value_column = 1;
    assert!(compile_executable_dag(&candidate).is_err());
}
