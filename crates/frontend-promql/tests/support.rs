use std::rc::Rc;

use asap_frontend_promql::{lower_promql_query_workload, PromqlError};
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::{NonASAPOp, OperatorNode, ScalarExpr};
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, PrometheusMetricType,
    Query, QueryLanguage, QueryRequirements, QueryWorkload,
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
    lower_promql_with_metric_types(query, accuracy, &[])
}

/// [`lower_promql`] over data that also declares each of `metric_types`'
/// sources of its type, sampled every second like the query's metrics.
#[allow(dead_code)]
pub fn lower_promql_with_metric_types(
    query: &str,
    accuracy: AccuracyTarget,
    metric_types: &[(&str, PrometheusMetricType)],
) -> Result<Rc<OperatorNode>, PromqlError> {
    let mut workload = workload(query, accuracy);
    let data = workload.data_workload.as_mut().expect("declared data");
    for (source, metric_type) in metric_types {
        let facts = data.sources.entry(source.to_string()).or_default();
        facts.metric_type = Some(*metric_type);
        facts.sample_interval = Some(DurationMs(1_000));
    }
    lower_promql_query_workload(&workload)?
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

/// Time `root` under the default physical properties assignment (every summary
/// at query time) and export the physical DAG — export needs every node
/// timed first.
#[allow(dead_code)]
pub fn physical_asap_dag(
    root: &Rc<OperatorNode>,
) -> asap_types::plan::physical_asap_dag::PhysicalASAPDAG {
    asap_types::test_support::physical_asap_dag_at_query_time(root).expect("post-ASAP DAG export")
}

#[allow(dead_code)]
pub fn scalar_root(query: &str) -> ScalarExpr {
    match lower_promql_query_workload(&workload(query, AccuracyTarget::exact()))
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
            max_out_of_order_ms: None,
        },
        sources,
    }
}

/// A composed logical candidate, or why it does not compose.
#[allow(dead_code)]
pub struct LogicalCandidate {
    pub id: String,
    pub roots: Result<Vec<asap_types::ir::QueryRoot>, String>,
}
