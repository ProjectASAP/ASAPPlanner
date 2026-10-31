//! A SQL trailing window, `WHERE ts >= now() - INTERVAL 'w'` on the table's
//! time index, plans like a PromQL range selector `m[w]`: Pass 2 builds its
//! summaries in tumbling panes, Stage 2 maintains them at ingestion time,
//! a deployment that keeps no raw data still gets a plan, and Stage 3 prices
//! each scan by its window.

use crate::planner_layering_common;

use asap_cost::CostInputs;
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, Operator, QueryRoot};
use asap_types::workload::{
    DataArrival, DataWorkload, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
    SourceFacts, SqlDialect,
};
use planner_layering_common::*;

/// The 5 and 15 minute distinct counts of W-RollupHeavy's template 9.
const QUERIES: [&str; 2] = [
    "SELECT dst_subnet, COUNT(DISTINCT src_addr) AS value FROM flows \
     WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY dst_subnet",
    "SELECT dst_subnet, COUNT(DISTINCT src_addr) AS value FROM flows \
     WHERE ts >= now() - INTERVAL '15 minutes' GROUP BY dst_subnet",
];
const WINDOWS_MS: [u64; 2] = [300_000, 900_000];

/// `flows(ts, dst_subnet, src_addr)`, time index `ts`.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("dst_subnet", DataType::Utf8, false),
                Field::plain("src_addr", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    )
}

/// Both queries every 60 s, relative ε = 0.05 per value, over 1,000 rows/s.
fn workload() -> PlanningWorkload {
    let entry = |query: &str| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget {
                accuracy_metric: Some(AccuracyMetric::Relative),
                accuracy_scope: AccuracyScope::PerValue,
                ..AccuracyTarget::max_error(0.05).failure_probability(0.05)
            },
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: None,
            repeating_queries: Some(QUERIES.iter().map(|q| entry(q)).collect()),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1_000.0,
            },
            sources: [(
                "flows".to_string(),
                SourceFacts {
                    max_distinct_per_evaluation: Some(1_000_000),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        }),
    }
}

async fn roots(workload: &PlanningWorkload) -> Vec<QueryRoot> {
    let mut roots = Vec::new();
    for entry in workload.query_workload.entries() {
        let root = lower_sql_dialect(
            &entry.query.0,
            &catalog(),
            SqlDialect::DataFusionSQL,
            entry.requirements.accuracy.clone(),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", entry.query.0));
        roots.push(QueryRoot::Operator(root));
    }
    roots
}

async fn run(deployment: &DeploymentCapabilities) -> Run {
    let workload = workload();
    let roots = roots(&workload).await;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
        HowToComputeAggregateMeasure,
    };
    use asap_logical_asap_optimization::pass2::asap_aware_cse::{
        pane_candidates, window_composition_pass2_candidates,
    };
    let local = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    let compose = |choices: &[usize]| compose_logical_candidate(&local, choices).unwrap();
    let exact = compose(&vec![0; local.targets.len()]);
    let choices = local
        .targets
        .iter()
        .map(|target| {
            target
                .ways_to_compute
                .iter()
                .position(|choice| {
                    matches!(
                        choice,
                        HowToComputeAggregateMeasure::Summary(
                            asap_types::ir::schema::SketchAlgorithm::Kmv
                        )
                    )
                })
                .expect("KMV distinct-count sketch with a failure bound")
        })
        .collect::<Vec<_>>();
    let summary = compose(&choices);
    let grids = workload
        .query_workload
        .entries()
        .map(|entry| entry.evaluation_grid_ms())
        .collect::<Vec<_>>();
    let panes = pane_candidates(&summary, &grids);
    assert_eq!(panes.len(), WINDOWS_MS.len());
    let mut candidates = vec![exact, summary.clone()];
    candidates.extend(
        window_composition_pass2_candidates(&summary, &panes)
            .unwrap()
            .into_iter()
            .filter(|candidate| candidate.pane_candidates.len() == panes.len())
            .map(|candidate| candidate.roots.unwrap()),
    );
    let settings = CostInputs::default();
    run_explicit_candidates(
        &workload,
        roots,
        candidates,
        asap_planner::PlanningModels::builtin()
            .with_cost_inputs(&settings)
            .with_deployment_capabilities(deployment),
    )
}

/// Pass 2 offers each query's distinct-count sketch in tumbling panes, and
/// Stage 2 maintains a pane chain at ingestion time, as for `m[5m]`.
#[tokio::test]
async fn sql_windows_get_tumbling_panes_and_ingestion_time_builds() {
    let run = run(&DeploymentCapabilities::default()).await;
    for (query, window_ms) in WINDOWS_MS.iter().enumerate() {
        let tumbling = run.logical.iter().any(|c| {
            sketch_builds(&c.dag).iter().any(|(build, _, _)| {
                readers(&c.dag, &c.query_roots, *build).contains(&query)
                    && matches!(window_form(&c.dag, *build), WindowForm::Tumbling { .. })
            })
        });
        assert!(
            tumbling,
            "query {query} ({window_ms} ms) has no pane candidate"
        );
    }
    let at_ingestion = run.physical.iter().any(|p| {
        p.dag().nodes.iter().any(|n| {
            matches!(
                n.payload,
                Operator::ASAP(asap_types::ir::ASAPOp::PaneChain { .. })
            ) && runs_at_ingestion(p, n.id)
        })
    });
    assert!(at_ingestion, "no pane chain maintained at ingestion time");
}

/// Without raw data at query time, the ingestion-time builds still answer
/// both queries, so the workload has a plan (`run_stages_with` panics when
/// none is selected) and it reads no raw scan at query time.
#[tokio::test]
async fn sql_windows_have_a_plan_without_raw_data() {
    let deployment = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let run = run(&deployment).await;
    let selected = run.physical(&run.selection.selected);
    for n in &selected.dag().nodes {
        if matches!(n.payload, Operator::NonASAP(NonASAPOp::Scan { .. })) {
            assert!(runs_at_ingestion(selected, n.id), "{}", selected.id);
        }
    }
}

/// Each query-time scan is priced by the window above it: the 15 minute
/// query's scan reads 3x the rows of the 5 minute one.
#[tokio::test]
async fn sql_scans_are_priced_by_their_window() {
    let run = run(&DeploymentCapabilities::default()).await;
    let mut scanned_rows = std::collections::BTreeSet::new();
    for (id, cost) in &run.selection.costs {
        let p = run.physical(id);
        for (node, node_cost) in &cost.per_node {
            let is_query_time_scan = p.dag().nodes.iter().any(|n| {
                n.id == *node
                    && matches!(n.payload, Operator::NonASAP(NonASAPOp::Scan { .. }))
                    && !runs_at_ingestion(p, n.id)
            });
            if !is_query_time_scan {
                continue;
            }
            let rows: u64 = node_cost
                .detail
                .strip_prefix("scan ")
                .and_then(|d| d.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("scan detail: {}", node_cost.detail));
            scanned_rows.insert(rows);
        }
    }
    // 1 row per ms over each window.
    for window_ms in WINDOWS_MS {
        assert!(
            scanned_rows.contains(&window_ms),
            "no scan of {window_ms} rows: {scanned_rows:?}"
        );
    }
}
