use std::rc::Rc;

use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, HistogramCatalog, PromqlError,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::{NonASAPOp, OperatorNode, ScalarExpr};
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
                range_evaluation: None,
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
pub fn physical_asap_dag(
    root: &Rc<OperatorNode>,
) -> asap_types::ir::physical_export::PhysicalASAPDAG {
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

/// The data these tests plan over, declared: 1,000 rows arriving per
/// second, and each metric `root` reads holding 1,000 series.
#[allow(dead_code)]
pub fn declared_data(root: &Rc<OperatorNode>) -> DataWorkload {
    let sources = OperatorNode::reachable(root)
        .into_iter()
        .filter_map(|node| match node.non_asap() {
            Some(NonASAPOp::Scan {
                source: asap_types::ir::operator::Source::TimeSeries { metric },
                ..
            }) => Some((
                metric.clone(),
                asap_types::workload::SourceFacts {
                    series_count: Some(1_000),
                    ..Default::default()
                },
            )),
            _ => None,
        })
        .collect();
    DataWorkload {
        arrival: asap_types::workload::DataArrival::Ingesting {
            rows_per_second: 1_000.0,
        },
        sources,
    }
}

/// Every planning stage's output for the single query `root` at `accuracy`,
/// with the built-in models.
#[allow(dead_code)]
pub fn run_stage1_to_stage3(
    root: Rc<OperatorNode>,
    accuracy: AccuracyTarget,
) -> asap_plan_selection::Stage1ToStage3Run<usize> {
    let data = declared_data(&root);
    asap_plan_selection::run_stage1_to_stage3_with(
        vec![(0, asap_types::ir::QueryRoot::Operator(root))],
        &[asap_types::workload::QueryWorkloadEntry {
            query: Default::default(),
            requirements: asap_types::workload::QueryRequirements {
                accuracy,
                max_latency_ms: None,
            },
            predictability: Default::default(),
            recurrence: Default::default(),
            range_evaluation: None,
        }],
        &data,
        asap_plan_selection::PlanningModels::builtin(),
        asap_plan_selection::Stage1ToStage3Search::default(),
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

/// Local computation candidates for frontend binding tests. Other computations
/// keep their default choices; a timed MILP archive is not a candidate catalog.
#[allow(dead_code)]
pub fn logical_candidates(root: Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    local_computation_candidates(root).expect("generated computation candidates compose")
}

fn local_computation_candidates(root: Rc<OperatorNode>) -> Result<Vec<Rc<OperatorNode>>, String> {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    use asap_types::ir::QueryRoot;
    let inventory = generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))])
        .map_err(|error| error.to_string())?;
    let mut choices = vec![vec![0; inventory.targets.len()]];
    for (index, target) in inventory.targets.iter().enumerate() {
        for choice in 1..target.choice_count() {
            let mut selected = vec![0; inventory.targets.len()];
            selected[index] = choice;
            choices.push(selected);
        }
    }
    choices
        .into_iter()
        .map(|choice| {
            let mut roots = compose_logical_candidate(&inventory, &choice)
                .map_err(|error| error.to_string())?;
            match roots.remove(0).1 {
                QueryRoot::Operator(root) => Ok(root),
                QueryRoot::Scalar(_) => Err("expected an operator root".into()),
            }
        })
        .collect()
}

/// Fail on a rejected candidate rather than silently dropping it from assertions.
#[allow(dead_code)]
pub fn require_composed_operator_candidates(
    candidates: Vec<asap_plan_selection::search::LogicalCandidate>,
) -> Vec<Rc<OperatorNode>> {
    candidates
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

/// One generated ASAP candidate, or the exact computation if no local rule
/// applies. Corpus coverage tests exercise binding, not cost-based selection.
#[allow(dead_code)]
pub fn logical_plan(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, String> {
    let candidates = local_computation_candidates(Rc::clone(root))?;
    Ok(candidates
        .iter()
        .find(|node| node.contains_asap())
        .unwrap_or(&candidates[0])
        .clone())
}
