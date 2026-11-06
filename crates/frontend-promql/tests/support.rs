use std::rc::Rc;

use asap_frontend_promql::{lower_promql_query_workload, PromqlError};
use asap_types::ir::properties::AccuracyTarget;
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
