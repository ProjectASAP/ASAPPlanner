use std::rc::Rc;

use asap_frontend_promql::{lower_promql_query_workload, HistogramCatalog, PromqlError};
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
    lower_promql_with_histograms(query, accuracy, HistogramCatalog::default())
}

#[allow(dead_code)]
pub fn lower_promql_with_histograms(
    query: &str,
    accuracy: AccuracyTarget,
    histograms: HistogramCatalog,
) -> Result<Rc<OperatorNode>, PromqlError> {
    lower_promql_query_workload(&workload(query, accuracy), &histograms)?
        .remove(0)
        .as_operator()
        .cloned()
        .ok_or_else(|| {
            PromqlError::UnsupportedFeature("scalar root: use lower_promql_query_workload".into())
        })
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
    use asap_types::ir::{
        apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache,
    };
    let timed = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::new(),
    )
    .expect("default materialization timings");
    asap_types::ir::physical_export::compile_physical_asap_dag(&timed)
        .expect("post-ASAP DAG export")
}

#[allow(dead_code)]
pub fn scalar_root(query: &str) -> ScalarExpr {
    match lower_promql_query_workload(
        &workload(query, AccuracyTarget::exact()),
        &HistogramCatalog::default(),
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
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[(
            0,
            QueryRoot::Operator(root),
        )]),
    )
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
            match roots.remove(0) {
                QueryRoot::Operator(root) => Ok(root),
                QueryRoot::Scalar(_) => Err("expected an operator root".into()),
            }
        })
        .collect()
}

/// A composed logical candidate, or why it does not compose.
#[allow(dead_code)]
pub struct LogicalCandidate {
    pub id: String,
    pub roots: Result<Vec<asap_types::ir::QueryRoot>, String>,
}

/// Fail on a rejected candidate rather than silently dropping it from assertions.
#[allow(dead_code)]
pub fn require_composed_operator_candidates(
    candidates: Vec<LogicalCandidate>,
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
