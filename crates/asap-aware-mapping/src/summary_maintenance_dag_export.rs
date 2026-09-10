//! Serializable DAG export for a materialized summary-maintenance plan.
//!
//! `asap-types::dag_export` owns the crate-neutral post-ASAP graph shape. This
//! adapter lives in the mapping layer, where summary-maintenance lifecycle
//! alternatives and their typed rejection reasons are available, and emits
//! both views together.

use std::collections::HashMap;
use std::rc::Rc;

use serde::Serialize;

use asap_types::dag_export::{self, SummaryDagGraph};
use asap_types::post_asap::{
    PostAsapNodeId, ResultGuarantee, SummaryExpr, SummaryMaintenanceLifecycle,
    SummaryMaintenanceLifecycleGuarantee, SummaryNode, SummaryWindowFramework,
};

use crate::summary_maintenance_lifecycle::{
    SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecycleRejection,
};

#[derive(Debug, Clone, Serialize)]
pub struct SummaryMaintenanceDagExport {
    pub graph: SummaryDagGraph,
    pub deployments: Vec<SummaryMaintenanceDeploymentExport>,
    pub horizon_seconds: Option<f64>,
    pub evaluation_rate_per_second: Option<f64>,
    pub update_rate_per_second: Option<f64>,
    pub expected_reads: Option<f64>,
    pub selected_raw_recompute: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Provider implementation key. The legacy JSON field name is retained
    /// until the surrounding export receives its own schema-version bump.
    #[serde(rename = "selected_physical_plan_id")]
    pub selected_window_implementation_id: Option<String>,
    pub summary_total_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_accuracy_guarantee: Option<ResultGuarantee>,
    pub raw_recompute_total_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryMaintenanceDeploymentExport {
    pub post_asap_node_id: PostAsapNodeId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_window_framework: Option<SummaryWindowFramework>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<SummaryMaintenanceLifecycleGuaranteeExport>,
    pub alternatives: Vec<SummaryMaintenanceLifecycleAlternativeExport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryMaintenanceLifecycleAlternativeExport {
    pub lifecycle: SummaryMaintenanceLifecycle,
    pub total_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection: Option<SummaryMaintenanceLifecycleRejection>,
    pub assumptions: Vec<String>,
}

pub type SummaryMaintenanceLifecycleGuaranteeExport = SummaryMaintenanceLifecycleGuarantee;

pub fn export_summary_maintenance_plan(
    plan: &SummaryMaintenanceLifecyclePlan,
) -> SummaryMaintenanceDagExport {
    let deployments: Vec<_> = plan
        .deployments
        .iter()
        .map(|deployment| SummaryMaintenanceDeploymentExport {
            post_asap_node_id: deployment.post_asap_node_id,
            selected_window_framework: deployment.selected_window_framework.clone(),
            selected: deployment
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .cloned(),
            alternatives: deployment
                .alternatives
                .iter()
                .map(|alternative| SummaryMaintenanceLifecycleAlternativeExport {
                    lifecycle: alternative.summary_maintenance_lifecycle.clone(),
                    total_cost: alternative.total_cost.map(|cost| cost.0),
                    rejection: alternative.rejection.clone(),
                    assumptions: alternative.assumptions.clone(),
                })
                .collect(),
        })
        .collect();
    let mut graph = dag_export::export_summary(&plan.root);
    let deployment_by_summary: HashMap<_, _> = plan
        .deployments
        .iter()
        .zip(&deployments)
        .map(|(deployment, export)| (Rc::as_ptr(&deployment.summary), export))
        .collect();
    let mut next_node_id = 0;
    annotate_lifecycle_deployments(
        &plan.root,
        &mut graph,
        &deployment_by_summary,
        &mut next_node_id,
    );

    SummaryMaintenanceDagExport {
        graph,
        deployments,
        horizon_seconds: plan.horizon.map(|horizon| horizon.0),
        evaluation_rate_per_second: plan.evaluation_rate.map(|rate| rate.0),
        update_rate_per_second: plan.update_rate.map(|rate| rate.0),
        expected_reads: plan.expected_reads,
        selected_raw_recompute: plan.selected_raw_recompute,
        selected_window_implementation_id: plan.selected_window_implementation_id.clone(),
        summary_total_cost: plan.summary_total_cost.map(|cost| cost.0),
        window_accuracy_guarantee: plan.window_accuracy_guarantee.clone(),
        raw_recompute_total_cost: plan.raw_recompute_total_cost.map(|cost| cost.0),
    }
}

/// Walk in the same post-order as `dag_export::export_summary` and attach a
/// deployment directly to every flattened occurrence of its `SummaryAgg`.
/// This makes the decision visible to graph consumers without asking them to
/// reconstruct pointer identity from graph position.
fn annotate_lifecycle_deployments(
    node: &SummaryNode,
    graph: &mut SummaryDagGraph,
    deployments: &HashMap<*const SummaryNode, &SummaryMaintenanceDeploymentExport>,
    next_node_id: &mut usize,
) {
    if !matches!(node.expr, SummaryExpr::KeepPreAsap(_)) {
        for child in summary_children(&node.expr) {
            annotate_lifecycle_deployments(child, graph, deployments, next_node_id);
        }
    }
    let graph_node = &mut graph.nodes[*next_node_id];
    if let Some(deployment) = deployments.get(&(node as *const SummaryNode)) {
        graph_node.detail["summary_maintenance"] =
            serde_json::to_value(deployment).expect("lifecycle export is serializable");
    }
    *next_node_id += 1;
}

fn summary_children(expr: &SummaryExpr) -> Vec<&Rc<SummaryNode>> {
    match expr {
        SummaryExpr::KeepPreAsap(_) => vec![],
        SummaryExpr::BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
        SummaryExpr::SummaryAgg { child, .. } => vec![child],
        SummaryExpr::ValueOperation { child, .. } => vec![child],
        SummaryExpr::SummaryJoin { outer, inner, .. }
        | SummaryExpr::SummarySubtract {
            left: outer,
            right: inner,
        }
        | SummaryExpr::CandidateTopK {
            candidates: outer,
            values: inner,
            ..
        } => vec![outer, inner],
        SummaryExpr::SummaryDelete { summary_input, .. }
        | SummaryExpr::SummaryEstimate { summary_input, .. } => vec![summary_input],
        SummaryExpr::SummaryMerge { children } => children.iter().collect(),
    }
}
