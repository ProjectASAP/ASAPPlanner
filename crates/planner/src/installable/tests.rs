use super::*;
use asap_types::summary_type::{SketchAlgorithm, StatisticKind, SupportedSummary};
use std::rc::Rc;

thread_local! {
    /// The plan nodes handed to the executor's compiler by the views'
    /// refreshes in [`InstallablePlan::of_dag`] and
    /// [`InstallablePlan::check_compiles`], summed over their compiles.
    pub(super) static COMPILED_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A p99 panel over a 300 s window every second, with no raw data
/// retained: one DDSketch per series in a chain of one-second windows
/// built at ingestion. Each refresh (here one refresh group) is compiled
/// over the nodes below its views, and the whole plan's checks run once,
/// so the nodes handed to the executor grow with the plan, not with the
/// plan times its views. The count covers both an installation and
/// `check_compiles`.
#[tokio::test]
async fn view_refreshes_compile_only_the_nodes_below_them() {
    let workload: crate::PlanningWorkload = serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "avg by (job) (quantile_over_time(0.99, data[300s]))",
              "demand": {"fixed_interval_at": {"interval": 1000, "evaluation_phase": 0}},
              "requirements": {"accuracy": {"max_error": 0.05, "failure_probability": 0.3,
                  "accuracy_scope": "whole_result"}},
              "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 10000.0}},
              "sources": {"data": {"series_count": 1000, "metric_type": "gauge",
                  "sample_interval": 100, "column_signs": {"value": "non_negative"}}}}}"#,
    )
    .unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    COMPILED_NODES.with(|count| count.set(0));
    let (plan, _) = plan_installable(crate::UserInput::new(
        &workload,
        crate::FrontendInput::Promql,
        crate::PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .expect("a plan");
    let nodes = plan.dag.nodes.len();
    let views = plan.materialized_views().unwrap().len();
    // Measure one installation and one `check_compiles`, independently of
    // how many candidates the optimizer explored before selecting this DAG.
    COMPILED_NODES.with(|count| count.set(0));
    let rebuilt = InstallablePlan::of_dag(plan.dag.clone(), plan.query_texts.clone()).unwrap();
    assert_eq!(rebuilt.materialized_views().unwrap().len(), views);
    plan.check_compiles().unwrap();
    let compiled = COMPILED_NODES.with(std::cell::Cell::get);
    assert!(
        compiled <= 6 * nodes,
        "{compiled} nodes compiled for a plan of {nodes} nodes and {views} views"
    );
}

/// The plan of the `sum_over_time(latency[1h])` panel as Stage 3 builds
/// it: a chain of 10-minute windows of exact sums per `job` over
/// `latency`, built at ingestion and retained for `chain_retention_ms`,
/// read through `TimeRange(1h)` by a merge and its finalization at query
/// time; and the chain's node id. With `shift_view` `(offset_ms,
/// retention_ms)`, the `TimeRange` reads the chain through a `TimeShift`
/// by `offset_ms`, materialized at ingestion and retained for
/// `retention_ms`.
fn chain_plan(
    chain_retention_ms: Option<u64>,
    shift_view: Option<(i64, u64)>,
) -> (PhysicalASAPDAG, PhysicalASAPNodeId) {
    use asap_types::ir::operator::{Reduction, Source};
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
    use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
    use asap_types::plan::physical_asap_dag::ViewCoverage;
    use asap_types::stages::{Lifetime, StorageMedium};
    use asap_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
    let node = |operator| OperatorNode::new_shared(operator).unwrap();
    let scan = node(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("job", DataType::Utf8, false),
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            1,
            vec![],
        ),
    }));
    let chain = node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan,
        summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![0]),
        filter: None,
        tumbling_window_width_ms: 600_000,
    }));
    let shift = shift_view.map(|(offset_ms, _)| {
        node(Operator::NonASAP(NonASAPOp::TimeShift {
            shift: asap_types::ir::operator::arguments::TimeShift {
                offset_ms,
                at: None,
            },
            child: chain.clone(),
        }))
    });
    let windows = node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: std::time::Duration::from_secs(3_600),
        kind: TimeRangeKind::Range,
        child: shift.clone().unwrap_or_else(|| chain.clone()),
    }));
    let merged = node(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![windows],
        group_by: None,
    }));
    let root = node(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: merged,
    }));
    let (mut dag, nodes) =
        asap_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
            QueryRoot::Operator(root),
        ])
        .unwrap();
    let chain_id = nodes.iter().position(|n| Rc::ptr_eq(n, &chain)).unwrap();
    let shift_id = shift.map(|shift| nodes.iter().position(|n| Rc::ptr_eq(n, &shift)).unwrap());
    let scan_id = producer_of(&dag, chain_id);
    let at_ingestion = [Some(chain_id), shift_id, Some(scan_id)];
    for node in &mut dag.nodes {
        let (retention_ms, coverage) = match node.id {
            id if id == chain_id => (chain_retention_ms, "sum of latency by job"),
            id if Some(id) == shift_id => (
                shift_view.map(|(_, retention_ms)| retention_ms),
                "sum of latency by job, shifted",
            ),
            _ => (None, ""),
        };
        if !coverage.is_empty() {
            node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
            node.retention_ms = retention_ms;
            node.view_coverage = Some(ViewCoverage::new(coverage.into(), vec![]));
        }
        if at_ingestion.contains(&Some(node.id)) {
            node.output_state.timing = ExecutionTiming::IngestionTime;
        }
    }
    for edge in &mut dag.edges {
        if at_ingestion.contains(&Some(edge.producer)) {
            edge.data_state_mut().timing = ExecutionTiming::IngestionTime;
        }
    }
    (dag, chain_id)
}

/// One exact query's entry.
fn entry() -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Default::default(),
        requirements: asap_types::workload::QueryRequirements {
            accuracy: asap_types::accuracy::AccuracyTarget::exact(),
            max_latency_ms: None,
        },
        predictability: Default::default(),
        recurrence: Default::default(),
        range_evaluation: None,
    }
}

/// The input of `consumer`, which reads one.
fn producer_of(dag: &PhysicalASAPDAG, consumer: PhysicalASAPNodeId) -> PhysicalASAPNodeId {
    dag.edges
        .iter()
        .find(|edge| edge.consumer == consumer)
        .unwrap()
        .producer
}

/// The installable plan of a DAG with a chain is the DAG as Stage 3
/// built it: the chain is one materialized view, refreshed at ingestion
/// one 10-minute window per run, and no raw data is kept for queries.
#[test]
fn a_chain_is_installed_as_one_view() {
    let (dag, chain) = chain_plan(Some(3_600_000), None);
    let plan = installable_plan_of(dag.clone(), &[entry()]).unwrap();
    assert_eq!(plan.dag, dag);
    let views = plan.materialized_views().unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].node_id, chain);
    assert_eq!(views[0].retention_ms(), 3_600_000);
    let groups = plan.ingestion_refresh_groups().unwrap();
    assert_eq!(
        groups[0].refresh_window,
        Some(asap_types::plan::materialized_view::RefreshWindow {
            width_ms: 600_000,
            phase_ms: 0
        })
    );
    assert!(plan.raw_data_retention().unwrap().is_empty());
    plan.check_compiles().unwrap();
}

/// Stage 3 sets each view's retention; installation checks it: a view
/// without one, or retained for less than the hour its query reads back,
/// is refused.
#[test]
fn installation_checks_the_retention_stage3_set() {
    let entries = [entry()];
    assert!(matches!(
        installable_plan_of(chain_plan(None, None).0, &entries),
        Err(InstallablePlanError::Uninstallable(
            InstallableDagError::ViewWithoutRetention { .. }
        ))
    ));
    assert!(matches!(
        installable_plan_of(chain_plan(Some(3_000_000), None).0, &entries),
        Err(InstallablePlanError::Uninstallable(
            InstallableDagError::ViewRetentionTooShort { .. }
        ))
    ));
}

/// A `TimeShift(1h)` view over the chain, read by `TimeRange(1h)`, keeps
/// each row an hour before it is read: its retention must cover 2 h, the
/// hour its reader reads and the hour of its offset, not only the hour
/// read. (The chain below it is read back 2 h too.)
#[test]
fn a_time_shift_view_keeps_its_offset_more() {
    let entries = [entry()];
    let shifted = |retention_ms| chain_plan(Some(7_200_000), Some((3_600_000, retention_ms))).0;
    assert!(matches!(
        installable_plan_of(shifted(3_600_000), &entries),
        Err(InstallablePlanError::Uninstallable(
            InstallableDagError::ViewRetentionTooShort { .. }
        ))
    ));
    let views = installable_plan_of(shifted(7_200_000), &entries)
        .unwrap()
        .materialized_views()
        .unwrap();
    assert_eq!(
        views
            .iter()
            .map(|view| view.retention_ms())
            .collect::<Vec<_>>(),
        [7_200_000, 7_200_000]
    );
}

/// A query-time `Scan` read through a window carries its retention;
/// one without is refused, and one with it is the raw data kept for its
/// source. Read through the chain built at query time, it is read as far
/// back as the chain's `TimeRange(1h)` reads, not one 10-minute window.
#[test]
fn a_query_time_scan_carries_its_retention() {
    let (mut dag, chain) = chain_plan(Some(3_600_000), None);
    let scan = producer_of(&dag, chain);
    for node in &mut dag.nodes {
        if node.id == chain || node.id == scan {
            node.output_state.timing = ExecutionTiming::QueryTime;
        }
    }
    for edge in &mut dag.edges {
        if edge.producer == chain || edge.producer == scan {
            edge.data_state_mut().timing = ExecutionTiming::QueryTime;
        }
    }
    assert!(matches!(
        dag.raw_data_retention(),
        Err(InstallableDagError::ScanWithoutRetention { .. })
    ));
    dag.nodes
        .iter_mut()
        .find(|n| n.id == scan)
        .unwrap()
        .retention_ms = Some(600_000);
    assert!(matches!(
        dag.raw_data_retention(),
        Err(InstallableDagError::ScanRetentionTooShort { .. })
    ));
    dag.nodes
        .iter_mut()
        .find(|n| n.id == scan)
        .unwrap()
        .retention_ms = Some(3_600_000);
    let retention = dag.raw_data_retention().unwrap();
    assert_eq!(retention.len(), 1);
    assert_eq!(retention[0].lookback_ms(), Some(3_600_000));
}

/// Unrestricted summaries become the executor's families; a caller's
/// own list and its retention flags are kept.
#[test]
fn unrestricted_summaries_default_to_the_executors_families() {
    let unrestricted = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let defaulted = executor_summaries_when_unrestricted(&unrestricted);
    assert_eq!(
        defaulted.supported_summaries,
        Some(asap_executor::supported_summary_families())
    );
    assert!(!defaulted.raw_data_retained);
    assert!(!defaulted.supports(&SketchAlgorithm::CountSketch, StatisticKind::ItemFrequency));
    let kll_only = DeploymentCapabilities {
        supported_summaries: Some(vec![SupportedSummary {
            summary_type: SketchAlgorithm::Kll,
            readouts: vec![StatisticKind::Quantile],
        }]),
        ..DeploymentCapabilities::default()
    };
    assert_eq!(executor_summaries_when_unrestricted(&kll_only), kll_only);
}
