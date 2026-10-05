//! The stage pipeline's selection never commits a heap that needs the PromQL
//! series identity; a deployment prices such heaps.
mod common;
use common::{compile_physical_asap_dag, selected_dag};
use planner_types::ir::OperatorNode;

use asap_executor::physical_planner::promql_rows::SERIES_IDENTITY_COLUMN;
use planner_types::{
    types::AccuracyTarget,
    workload::{
        AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence as WorkloadEvidence,
        PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload,
        TimeSelection,
    },
};
use std::rc::Rc;

fn lower(query: &str, accuracy: &AccuracyTarget) -> Rc<OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy.clone()),
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
            data_ingestion_interval: WorkloadEvidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    asap_frontend_promql::lower_promql_workload(&workload, 0)
        .unwrap()
        .remove(0)
}

/// Whether an operator below the root carries series identity. A top-k root
/// returns its selected series' identity whatever realizes it.
fn carries_identity(root: &Rc<OperatorNode>) -> bool {
    let dag = compile_physical_asap_dag(root).unwrap();
    dag.nodes
        .iter()
        .filter(|node| !dag.roots.contains(&node.id))
        .any(|node| {
            node.output_schema
                .fields
                .iter()
                .any(|field| field.name == SERIES_IDENTITY_COLUMN)
        })
}

const CURRENT_SERIES_TOPK: &str = "topk by(job)(1, m)";

// The stage pipeline's selection keeps the logical plan; deployment prices heaps.
#[test]
fn selection_never_commits_a_series_identity_heap() {
    let accuracy = AccuracyTarget::Epsilon(0.1);
    let root = lower(CURRENT_SERIES_TOPK, &accuracy);
    let selected = selected_dag(root, accuracy);
    assert!(!carries_identity(&selected));
}
