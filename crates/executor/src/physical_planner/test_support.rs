//! Helpers for the Planner's own tests (`test-support`).

use super::*;
use planner_types::stages::{Lifetime, StorageMedium};
use std::collections::HashMap;

/// Set θ on every sketch build among `nodes` (`SummaryAgg`,
/// `SummaryTumblingWindowChain` of a sketch family): `parameters` when
/// given, else the first θ its algorithm lists (`parameter_list`).
/// Exact builds keep `None`. For tests of hand-built plans, whose nodes
/// carry no θ: pass `&mut dag.nodes`, or `[&mut node]`.
pub fn set_build_parameters<'a>(
    nodes: impl IntoIterator<Item = &'a mut PhysicalASAPDAGNode>,
    parameters: Option<&SketchParams>,
) {
    for node in nodes {
        if let Payload::ASAP(
            ASAPOp::SummaryAgg {
                summary_type: FieldDataType::Sketch(algorithm),
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                summary_type: FieldDataType::Sketch(algorithm),
                ..
            },
        ) = &node.payload
        {
            node.parameters = parameters.cloned().or_else(|| {
                planner_types::summary_type::parameter_list(*algorithm)
                    .first()
                    .cloned()
            });
        }
    }
}

/// The materialized views at `nodes`, in that order: each node that is
/// not a view is marked materialized at ingestion time, and it and
/// every node below it run at ingestion time (their outputs and
/// outgoing edges); every view without a retention or coverage is
/// retained for ever with a coverage of its own. For tests that bind
/// states at those nodes, as a deployment binds its views.
pub fn views_at(
    dag: &mut PhysicalASAPDAG,
    nodes: &[NodeId],
) -> Result<Vec<MaterializedView>, Error> {
    use planner_types::ir::properties::ExecutionTiming;
    let mut producers: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for edge in &dag.edges {
        producers
            .entry(edge.consumer as NodeId)
            .or_default()
            .push(edge.producer as NodeId);
    }
    let marked: Vec<NodeId> = dag
        .nodes
        .iter()
        .filter(|node| {
            nodes.contains(&(node.id as NodeId))
                && !matches!(
                    node.lifetime,
                    planner_types::stages::Lifetime::AcrossEvaluations(_)
                )
        })
        .map(|node| node.id as NodeId)
        .collect();
    let mut at_ingestion = BTreeSet::new();
    let mut pending = marked.clone();
    while let Some(id) = pending.pop() {
        if at_ingestion.insert(id) {
            pending.extend(producers.get(&id).into_iter().flatten());
        }
    }
    for node in &mut dag.nodes {
        let id = node.id as NodeId;
        if marked.contains(&id) {
            node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
        }
        if matches!(node.lifetime, Lifetime::AcrossEvaluations(_)) {
            node.retention_ms.get_or_insert(i64::MAX as u64);
            node.view_coverage.get_or_insert_with(|| {
                planner_types::plan::physical_asap_dag::ViewCoverage::new(
                    format!("node {id}"),
                    vec![],
                )
            });
        }
        if at_ingestion.contains(&id) {
            node.output_state.timing = ExecutionTiming::IngestionTime;
        }
    }
    for edge in &mut dag.edges {
        if at_ingestion.contains(&(edge.producer as NodeId)) {
            edge.data_state_mut().timing = ExecutionTiming::IngestionTime;
        }
    }
    let views = dag
        .materialized_views()
        .map_err(|error| invalid(error.to_string()))?;
    nodes
        .iter()
        .map(|&id| {
            views
                .iter()
                .find(|view| view.node_id as NodeId == id)
                .cloned()
                .ok_or_else(|| invalid(format!("node {id} is not a materialized view")))
        })
        .collect()
}
