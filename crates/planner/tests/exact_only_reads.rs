//! A query reads the groups Pass 2 lists as exact-only for it
//! (`LogicalASAPMemoCandidates::exact_only_groups_of_query`) only through
//! exact candidates, while another query of the same workload may still
//! read a sketch of those groups: Stage 3's constraint (X), end to end.

use std::collections::BTreeSet;

use asap_planner::{plan_installable, FrontendInput, PlanningModels, UserInput};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::workload::PlanningWorkload;

/// PromQL `queries`, each with its requirements' JSON (`""`: none), every
/// 60 s at phase 0, over 1,000 gauge series of `m` sampled every second.
fn workload(queries: &[(&str, &str)]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|(query, requirements)| {
            format!(
                r#"{{"query": {query:?},
                    "demand": {{"fixed_interval_at": {{"interval": 60000, "evaluation_phase": 0}}}},
                    "requirements": {{{requirements}}},
                    "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"m": {{"series_count": 1000, "metric_type": "gauge",
                                  "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The plan of `queries`.
async fn plan(queries: &[(&str, &str)]) -> PhysicalASAPDAG {
    let workload = workload(queries);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql,
        PlanningModels::builtin(),
    ))
    .await
    .expect("the workload plans")
    .0
    .dag
}

/// The payloads of the nodes at or below query `query`'s root in `dag`.
fn payloads_read_by(dag: &PhysicalASAPDAG, query: usize) -> Vec<String> {
    let mut below = BTreeSet::new();
    let mut pending: Vec<PhysicalASAPNodeId> = vec![dag.roots[query]];
    while let Some(id) = pending.pop() {
        if below.insert(id) {
            pending.extend(
                dag.edges
                    .iter()
                    .filter(|edge| edge.consumer == id)
                    .map(|edge| edge.producer),
            );
        }
    }
    dag.nodes
        .iter()
        .filter(|node| below.contains(&node.id))
        .map(|node| format!("{:?}", node.payload))
        .collect()
}

/// The same p90 asked at rank error 0.01 (Q0) and at relative error 0.05
/// (Q1): KLL's rank error does not meet Q1's relative target, so Pass 2
/// lists the KLL readout and every group below it as exact-only for Q1.
/// Q1 reads no KLL sketch, even where Q0's plan has one to share.
#[tokio::test]
async fn a_relative_query_reads_no_rank_sketch_of_a_query_sharing_its_groups() {
    let dag = plan(&[
        (
            "quantile_over_time(0.9, m[5m])",
            r#""accuracy": {"max_error": 0.01, "accuracy_metric": "rank"}"#,
        ),
        (
            "quantile_over_time(0.9, m[5m])",
            r#""accuracy": {"max_error": 0.05, "accuracy_metric": "relative"}"#,
        ),
    ])
    .await;
    let read = payloads_read_by(&dag, 1);
    assert!(
        !read.iter().any(|payload| payload.contains("Sketch(Kll)")),
        "the relative query reads a KLL sketch: {read:?}"
    );
}

/// An exact p90 (no accuracy target) and the same p90 at rank error 0.01:
/// the exact query reads no sketch, whichever plan the other query gets.
#[tokio::test]
async fn an_exact_query_sharing_groups_with_an_approximate_one_reads_no_sketch() {
    let dag = plan(&[
        ("quantile_over_time(0.9, m[5m])", ""),
        (
            "quantile_over_time(0.9, m[5m])",
            r#""accuracy": {"max_error": 0.01, "accuracy_metric": "rank"}"#,
        ),
    ])
    .await;
    let read = payloads_read_by(&dag, 0);
    assert!(
        !read.iter().any(|payload| payload.contains("Sketch(")),
        "the exact query reads a sketch: {read:?}"
    );
}
