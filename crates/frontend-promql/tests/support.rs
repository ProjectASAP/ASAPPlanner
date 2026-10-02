use std::rc::Rc;

use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, HistogramCatalog, PromqlError,
};
use asap_types::ir::{NonASAPOp, OperatorNode, ScalarExpr};
use asap_types::pre_asap::ScalarValue;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
    Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, TimeSelection,
};

fn workload(query: &str, accuracy: AccuracyTarget) -> PlanningWorkload {
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
/// operator position (`ScalarBridge(Literal(Float64(v)))`); `None` for any
/// other shape.
#[allow(dead_code)]
pub fn promql_scalar(node: &OperatorNode) -> Option<f64> {
    match node.non_asap()? {
        NonASAPOp::ScalarBridge(ScalarExpr::Literal(ScalarValue::Float64(v))) => Some(*v),
        _ => None,
    }
}

/// Time `root` under the default (every summary maintained) lifecycle
/// assignment and export the post-ASAP DAG — the wire-6 export needs every
/// node timed first.
#[allow(dead_code)]
pub fn post_asap_dag(root: &Rc<OperatorNode>) -> asap_types::ir::export::PostAsapDag {
    use asap_types::ir::{apply_lifecycle_timings, LifecycleAssignment, TimingMemo};
    let timed = apply_lifecycle_timings(
        root,
        &LifecycleAssignment::default_maintained(),
        &mut TimingMemo::new(),
    )
    .expect("default lifecycle timings");
    asap_types::ir::export::compile_post_asap_dag(&timed).expect("post-ASAP DAG export")
}
