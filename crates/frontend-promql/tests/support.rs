use std::rc::Rc;

use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, HistogramCatalog, PromqlError,
};
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::{NonASAPOp, OperatorNode, ScalarExpr};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
    Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, TimeSelection,
};

pub fn workload(query: &str, accuracy: AccuracyTarget) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    }
}

#[allow(dead_code)]
pub fn lower_promql(
    query: &str,
    accuracy: AccuracyTarget,
) -> Result<Rc<OperatorNode>, PromqlError> {
    let mut lowered = lower_promql_workload(&workload(query, accuracy), 0)?;
    Ok(lowered.remove(0))
}

#[allow(dead_code)]
pub fn lower_promql_with_histograms(
    query: &str,
    accuracy: AccuracyTarget,
    histograms: HistogramCatalog,
) -> Result<Rc<OperatorNode>, PromqlError> {
    let mut lowered =
        lower_promql_workload_with_histograms(&workload(query, accuracy), histograms, 0)?;
    Ok(lowered.remove(0))
}

/// The value of a bare PromQL numeric literal / folded constant at an
/// scalar position (`Literal(Float64(v))`); `None` for any
/// other shape.
#[allow(dead_code)]
pub fn promql_scalar(node: &ScalarExpr) -> Option<f64> {
    match node {
        ScalarExpr::Literal(ScalarValue::Float64(v)) => Some(*v),
        _ => None,
    }
}

/// Time `root` under the default materialization assignment (every summary
/// at query time) and export the physical DAG — export needs every node
/// timed first.
#[allow(dead_code)]
pub fn post_asap_dag(root: &Rc<OperatorNode>) -> asap_types::ir::physical_export::PhysicalASAPDAG {
    use asap_types::ir::{apply_materialization_timings, MaterializationAssignment, TimingMemo};
    let timed = apply_materialization_timings(
        root,
        &MaterializationAssignment::all_query_time(),
        &mut TimingMemo::new(),
    )
    .expect("default materialization timings");
    asap_types::ir::physical_export::compile_physical_asap_dag(&timed)
        .expect("post-ASAP DAG export")
}

#[allow(dead_code)]
pub fn scalar_root(query: &str) -> ScalarExpr {
    match asap_frontend_promql::lower_promql_query_workload(
        &workload(query, AccuracyTarget::Exact),
        0,
    )
    .unwrap()
    .remove(0)
    {
        asap_types::ir::QueryRoot::Scalar(expr) => expr,
        _ => panic!("expected scalar root: {query}"),
    }
}

#[allow(dead_code)]
pub fn sample_expression(node: &OperatorNode) -> &ScalarExpr {
    match node.expect_non_asap() {
        NonASAPOp::Project { cols, .. } => {
            &cols[node.schema.column_id("value").unwrap_or(cols.len() - 1)].expr
        }
        NonASAPOp::Filter { pred, .. } => &pred.0,
        other => panic!("expected sample expression, got {other:?}"),
    }
}

/// The logical DAG the #509 stage pipeline selects for the single query
/// `root` at `accuracy`, run once, with the built-in models.
#[allow(dead_code)]
pub fn selected_dag(root: Rc<OperatorNode>, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    use asap_types::ir::QueryRoot;
    use asap_types::workload::{QueryRecurrence, RootDemand};
    let demand = [RootDemand {
        accuracy: Some(accuracy),
        recurrence: QueryRecurrence::OneTime {
            invocations: 1,
            execute_at: None,
        },
        predictability: Predictability::default(),
        latency_ms: None,
    }];
    let run = asap_plan_selection::plan_stages(
        vec![(0, QueryRoot::Operator(root))],
        &demand,
        &DataWorkload::default(),
        asap_plan_selection::PlanningModels::builtin(),
        0,
    )
    .unwrap();
    let QueryRoot::Operator(root) = &run.plan.logical[0].1 else {
        panic!("operator root")
    };
    root.clone()
}

/// Every whole-query candidate Stage 1's Pass 1 lists for `root` (up to
/// 4096), composed; choices that do not compose are skipped.
#[allow(dead_code)]
pub fn stage1_candidates(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    use asap_logical_optimizer::pass1::logical_candidates::{
        compose_logical_candidate, enumerate_choices, enumerate_local_logical_candidates,
    };
    use asap_types::ir::QueryRoot;
    let inventory = enumerate_local_logical_candidates(
        vec![(0, QueryRoot::Operator(Rc::clone(root)))],
        &Default::default(),
    )
    .unwrap();
    enumerate_choices(&inventory, 4096)
        .iter()
        .filter_map(|choice| {
            match compose_logical_candidate(&inventory, choice)
                .ok()?
                .remove(0)
                .1
            {
                QueryRoot::Operator(node) => Some(node),
                QueryRoot::Scalar(_) => None,
            }
        })
        .collect()
}

/// A candidate Stage 1 composes for `root`: the first of its (up to 64)
/// whole-query choices that composes and builds a summary, else the
/// pass-through choice. `Err` when Pass 1 rejects the query or the
/// pass-through choice does not compose. A choice may legitimately not
/// compose (an alternative its input cannot feed); selection skips it.
#[allow(dead_code)]
pub fn stage1_plan(
    root: &Rc<OperatorNode>,
) -> Result<
    Rc<OperatorNode>,
    asap_logical_optimizer::pass1::logical_candidates::LogicalCandidateError,
> {
    use asap_logical_optimizer::pass1::logical_candidates::{
        compose_logical_candidate, enumerate_choices, enumerate_local_logical_candidates,
    };
    use asap_types::ir::QueryRoot;
    let inventory = enumerate_local_logical_candidates(
        vec![(0, QueryRoot::Operator(Rc::clone(root)))],
        &Default::default(),
    )?;
    let compose = |choice: &[usize]| {
        compose_logical_candidate(&inventory, choice).map(|mut roots| match roots.remove(0).1 {
            QueryRoot::Operator(node) => node,
            QueryRoot::Scalar(_) => unreachable!("an operator root composes to an operator root"),
        })
    };
    let summarized = enumerate_choices(&inventory, 64)
        .iter()
        .filter_map(|choice| compose(choice).ok())
        .find(|node| node.contains_asap());
    match summarized {
        Some(node) => Ok(node),
        None => compose(&vec![0; inventory.targets.len()]),
    }
}
