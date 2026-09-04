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
    EvaluationSchedule, OutputRepresentation, ResultGuarantee, SummaryExpr,
    SummaryMaintenanceLifecycle, SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode,
    SummaryNode, SummaryWindowFramework,
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
    pub selected_physical_plan_id: Option<String>,
    pub summary_total_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_accuracy_guarantee: Option<ResultGuarantee>,
    pub raw_recompute_total_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryMaintenanceDeploymentExport {
    pub summary_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_window_framework: Option<SummaryWindowFramework>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<SummaryMaintenanceLifecycleGuaranteeExport>,
    pub alternatives: Vec<SummaryMaintenanceLifecycleAlternativeExport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryMaintenanceLifecycleAlternativeExport {
    pub lifecycle: SummaryMaintenanceLifecycleExport,
    pub total_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection: Option<SummaryMaintenanceLifecycleRejectionExport>,
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryMaintenanceLifecycleGuaranteeExport {
    pub lifecycle: SummaryMaintenanceLifecycleExport,
    pub maintenance_mode: SummaryMaintenanceModeExport,
    pub evaluation_schedule: EvaluationScheduleExport,
    pub output_representation: OutputRepresentationExport,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryMaintenanceModeExport {
    DirectBuild,
    Incremental,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SummaryMaintenanceLifecycleExport {
    Ephemeral,
    Prepared {
        activate_at_ms: u64,
        retire_at_ms: u64,
    },
    Shared {
        retention_ms: u64,
    },
    ContinuouslyMaintained,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationScheduleExport {
    OneShot,
    PerUpdate,
    OnRead,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputRepresentationExport {
    PlainRows,
    SummaryState,
    FinalizedValue,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryMaintenanceLifecycleRejectionExport {
    UnsupportedByRuntime,
    RequiresPredictableOneTimeQuery,
    RequiresMultipleReads,
    RequiresHorizon,
    RequiresContinuousData,
    MissingOrStaleIngestionRate,
    SummaryDoesNotSupportIncrementalUpdates,
    SummaryDoesNotSupportDeletion,
    MissingCostEvidence,
}

pub fn export_summary_maintenance_plan(
    plan: &SummaryMaintenanceLifecyclePlan,
) -> SummaryMaintenanceDagExport {
    let deployments: Vec<_> = plan
        .deployments
        .iter()
        .map(|deployment| SummaryMaintenanceDeploymentExport {
            summary_index: deployment.summary_index,
            selected_window_framework: deployment.selected_window_framework.clone(),
            selected: deployment
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .map(export_guarantee),
            alternatives: deployment
                .alternatives
                .iter()
                .map(|alternative| SummaryMaintenanceLifecycleAlternativeExport {
                    lifecycle: export_lifecycle(&alternative.summary_maintenance_lifecycle),
                    total_cost: alternative.total_cost.map(|cost| cost.0),
                    rejection: alternative.rejection.as_ref().map(export_rejection),
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
        selected_physical_plan_id: plan.selected_physical_plan_id.clone(),
        summary_total_cost: plan.summary_total_cost.map(|cost| cost.0),
        window_accuracy_guarantee: plan.window_accuracy_guarantee.clone(),
        raw_recompute_total_cost: plan.raw_recompute_total_cost.map(|cost| cost.0),
    }
}

/// Walk in the same post-order as `dag_export::export_summary` and attach a
/// deployment directly to every flattened occurrence of its `SummaryAgg`.
/// This makes the decision visible to graph consumers without asking them to
/// reconstruct pointer identity from `summary_index` or graph position.
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
        SummaryExpr::ExactBinary { lhs, rhs, .. } => vec![lhs, rhs],
        SummaryExpr::SummaryAgg { child, .. } => vec![child],
        SummaryExpr::SummaryJoin { outer, inner, .. }
        | SummaryExpr::SummarySubtract {
            left: outer,
            right: inner,
        } => vec![outer, inner],
        SummaryExpr::SummaryDelete { summary_input, .. }
        | SummaryExpr::SummaryEstimate { summary_input, .. } => vec![summary_input],
        SummaryExpr::SummaryMerge { children } => children.iter().collect(),
    }
}

fn export_guarantee(
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
) -> SummaryMaintenanceLifecycleGuaranteeExport {
    SummaryMaintenanceLifecycleGuaranteeExport {
        lifecycle: export_lifecycle(&guarantee.summary_maintenance_lifecycle),
        maintenance_mode: match guarantee.summary_maintenance_mode {
            SummaryMaintenanceMode::DirectBuild => SummaryMaintenanceModeExport::DirectBuild,
            SummaryMaintenanceMode::Incremental => SummaryMaintenanceModeExport::Incremental,
        },
        evaluation_schedule: match guarantee.evaluation_schedule {
            EvaluationSchedule::OneShot => EvaluationScheduleExport::OneShot,
            EvaluationSchedule::PerUpdate => EvaluationScheduleExport::PerUpdate,
            EvaluationSchedule::OnRead => EvaluationScheduleExport::OnRead,
        },
        output_representation: match guarantee.output_representation {
            OutputRepresentation::PlainRows => OutputRepresentationExport::PlainRows,
            OutputRepresentation::SummaryState => OutputRepresentationExport::SummaryState,
            OutputRepresentation::FinalizedValue => OutputRepresentationExport::FinalizedValue,
        },
    }
}

fn export_lifecycle(lifecycle: &SummaryMaintenanceLifecycle) -> SummaryMaintenanceLifecycleExport {
    match lifecycle {
        SummaryMaintenanceLifecycle::Ephemeral => SummaryMaintenanceLifecycleExport::Ephemeral,
        SummaryMaintenanceLifecycle::Prepared {
            activate_at,
            retire_at,
        } => SummaryMaintenanceLifecycleExport::Prepared {
            activate_at_ms: activate_at.0,
            retire_at_ms: retire_at.0,
        },
        SummaryMaintenanceLifecycle::Shared { retention } => {
            SummaryMaintenanceLifecycleExport::Shared {
                retention_ms: retention.0,
            }
        }
        SummaryMaintenanceLifecycle::ContinuouslyMaintained => {
            SummaryMaintenanceLifecycleExport::ContinuouslyMaintained
        }
    }
}

fn export_rejection(
    rejection: &SummaryMaintenanceLifecycleRejection,
) -> SummaryMaintenanceLifecycleRejectionExport {
    match rejection {
        SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime => {
            SummaryMaintenanceLifecycleRejectionExport::UnsupportedByRuntime
        }
        SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery => {
            SummaryMaintenanceLifecycleRejectionExport::RequiresPredictableOneTimeQuery
        }
        SummaryMaintenanceLifecycleRejection::RequiresMultipleReads => {
            SummaryMaintenanceLifecycleRejectionExport::RequiresMultipleReads
        }
        SummaryMaintenanceLifecycleRejection::RequiresHorizon => {
            SummaryMaintenanceLifecycleRejectionExport::RequiresHorizon
        }
        SummaryMaintenanceLifecycleRejection::RequiresContinuousData => {
            SummaryMaintenanceLifecycleRejectionExport::RequiresContinuousData
        }
        SummaryMaintenanceLifecycleRejection::MissingOrStaleIngestionRate => {
            SummaryMaintenanceLifecycleRejectionExport::MissingOrStaleIngestionRate
        }
        SummaryMaintenanceLifecycleRejection::SummaryDoesNotSupportIncrementalUpdates => {
            SummaryMaintenanceLifecycleRejectionExport::SummaryDoesNotSupportIncrementalUpdates
        }
        SummaryMaintenanceLifecycleRejection::SummaryDoesNotSupportDeletion => {
            SummaryMaintenanceLifecycleRejectionExport::SummaryDoesNotSupportDeletion
        }
        SummaryMaintenanceLifecycleRejection::MissingCostEvidence => {
            SummaryMaintenanceLifecycleRejectionExport::MissingCostEvidence
        }
    }
}
