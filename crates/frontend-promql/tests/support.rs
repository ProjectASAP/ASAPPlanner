use std::rc::Rc;

use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, HistogramCatalog, PromqlError,
};
use asap_types::ir::{NonASAPOp, OperatorNode, ScalarExpr};
use asap_types::pre_asap::ScalarValue;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};

pub fn workload(query: &str, accuracy: AccuracyTarget) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy,
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(1_000),
        )),
    }
}

#[allow(dead_code)]
pub fn lower_promql(
    query: &str,
    accuracy: AccuracyTarget,
) -> Result<Rc<OperatorNode>, PromqlError> {
    let mut lowered = lower_promql_workload(&workload(query, accuracy))?;
    Ok(lowered.remove(0))
}

#[allow(dead_code)]
pub fn lower_promql_with_histograms(
    query: &str,
    accuracy: AccuracyTarget,
    histograms: HistogramCatalog,
) -> Result<Rc<OperatorNode>, PromqlError> {
    let mut lowered =
        lower_promql_workload_with_histograms(&workload(query, accuracy), histograms)?;
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
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .expect("default materialization timings");
    asap_types::ir::physical_export::compile_physical_asap_dag(&timed)
        .expect("post-ASAP DAG export")
}

#[allow(dead_code)]
pub fn scalar_root(query: &str) -> ScalarExpr {
    match asap_frontend_promql::lower_promql_query_workload(&workload(
        query,
        AccuracyTarget::exact(),
    ))
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

/// Every planning stage's output for the single query `root` at `accuracy`,
/// with the built-in models.
#[allow(dead_code)]
pub fn run_stage1_to_stage3(
    root: Rc<OperatorNode>,
    accuracy: AccuracyTarget,
) -> asap_aware_mapping::Stage1ToStage3Run<usize> {
    asap_aware_mapping::run_stage1_to_stage3(
        vec![(0, asap_types::ir::QueryRoot::Operator(root))],
        &[asap_types::workload::QueryWorkloadEntry {
            requirements: asap_types::workload::QueryRequirements {
                accuracy,
                max_latency_ms: None,
            },
            ..Default::default()
        }],
        &DataWorkload::default(),
        asap_aware_mapping::PlanningModels::builtin(),
    )
    .expect("Pass 1 accepts the query")
}

/// The logical DAG the planner pipeline selects for the single query `root`
/// at `accuracy`.
#[allow(dead_code)]
pub fn selected_dag(root: Rc<OperatorNode>, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let run = run_stage1_to_stage3(root, accuracy);
    match run.selected_logical().expect("a plan is selected") {
        [asap_types::ir::QueryRoot::Operator(node)] => Rc::clone(node),
        other => panic!("one operator root: {other:?}"),
    }
}

/// Every logical candidate (a composed CandidateLogicalASAPDAG) of the single
/// query `root` at `accuracy`, in enumeration order; a candidate that does not
/// compose panics, so none is silently skipped.
#[allow(dead_code)]
pub fn logical_candidates(
    root: Rc<OperatorNode>,
    accuracy: AccuracyTarget,
) -> Vec<Rc<OperatorNode>> {
    run_stage1_to_stage3(root, accuracy)
        .stage1_logical_asap_optimization
        .into_iter()
        .filter_map(|candidate| {
            match candidate
                .roots
                .unwrap_or_else(|e| panic!("{} does not compose: {e}", candidate.id))
                .remove(0)
            {
                asap_types::ir::QueryRoot::Operator(node) => Some(node),
                asap_types::ir::QueryRoot::Scalar(_) => None,
            }
        })
        .collect()
}

/// One logical candidate of the single query `root`: the first composed
/// candidate (in enumeration order) that reads a summary, else
/// the exact one (the first). `Err` when Pass 1 rejects the query or the
/// exact candidate does not compose; another candidate may legitimately not
/// compose (a candidate its input cannot feed), and selection skips it.
#[allow(dead_code)]
pub fn logical_plan(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, String> {
    let run = asap_aware_mapping::run_stage1_to_stage3(
        vec![(0, asap_types::ir::QueryRoot::Operator(Rc::clone(root)))],
        &[asap_types::workload::QueryWorkloadEntry {
            requirements: asap_types::workload::QueryRequirements {
                accuracy: asap_types::types::AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            ..Default::default()
        }],
        &DataWorkload::default(),
        asap_aware_mapping::PlanningModels::builtin(),
    )
    .map_err(|e| e.to_string())?;
    let operator = |roots: &[asap_types::ir::QueryRoot]| match &roots[0] {
        asap_types::ir::QueryRoot::Operator(node) => Rc::clone(node),
        asap_types::ir::QueryRoot::Scalar(_) => unreachable!("an operator root stays one"),
    };
    let summarized = run
        .stage1_logical_asap_optimization
        .iter()
        .filter_map(|candidate| candidate.roots.as_deref().ok().map(operator))
        .find(|node| node.contains_asap());
    match summarized {
        Some(node) => Ok(node),
        None => run.stage1_logical_asap_optimization[0]
            .roots
            .as_deref()
            .map(operator)
            .map_err(|e| e.to_string()),
    }
}
