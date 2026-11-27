//! What a materialized view's stored state is, in any plan
//! (`MaterializedView::view_state_key`): the view's definition, its own
//! window and its θ. Refresh mode, retention and storage medium do not
//! enter, and neither do the nodes below the view. The plan id
//! (`PhysicalASAPDAG::plan_id`) names one installed plan.
use std::rc::Rc;

use asap_executor::physical_planner::test_support::{set_build_parameters, views_at};
use asap_planner::{
    plan_installable, CostInputs, DeploymentCapabilities, FrontendInput, InstallablePlan,
    PlanningModels, PlanningWorkload, UserInput,
};
use asap_types::ir::operator::{GroupKeys, Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use asap_types::plan::materialized_view::{
    MaterializedView, MaterializedViewRefresh, ViewStateKey,
};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPDAGNode, ViewCoverage};
use asap_types::stages::{Lifetime, StorageMedium};
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic, SummaryUpdate};

const MINUTE_MS: u64 = 60_000;

/// `p50(SummaryMerge(TimeRange(5m, Chain[width_ms](Scan(t)))))` over
/// `t(ts, value)`, every node at query time, the chain KLL `k = 200` (and
/// the merge, which reads its states); with the chain's node id and the
/// merge's.
fn chain_dag(width_ms: u64) -> (PhysicalASAPDAG, usize, usize) {
    let new = |operator| OperatorNode::new_shared(operator).unwrap();
    let scan = new(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    }));
    let chain: Rc<OperatorNode> = new(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan,
        summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::Reduce(GroupKeys::default()),
        filter: None,
        tumbling_window_width_ms: width_ms,
    }));
    let range = new(Operator::NonASAP(NonASAPOp::TimeRange {
        range: std::time::Duration::from_millis(5 * MINUTE_MS),
        kind: TimeRangeKind::Range,
        child: chain,
    }));
    let merge = new(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![range],
        group_by: None,
    }));
    let p50 = new(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: merge,
        query: SketchStatistic::Quantile { q: 0.5 },
    }));
    let mut dag =
        asap_types::test_support::forwarders::physical_asap_dag_at_query_time(&p50).unwrap();
    set_build_parameters(&mut dag.nodes, Some(&SketchParams::Kll { k: 200 }));
    let id_of = |dag: &PhysicalASAPDAG, f: &dyn Fn(&Operator<usize>) -> bool| {
        dag.nodes.iter().find(|node| f(&node.payload)).unwrap().id
    };
    let chain = id_of(&dag, &|operator| {
        matches!(
            operator,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
        )
    });
    let merge = id_of(&dag, &|operator| {
        matches!(operator, Operator::ASAP(ASAPOp::SummaryMerge { .. }))
    });
    node_mut(&mut dag, merge).parameters = Some(SketchParams::Kll { k: 200 });
    (dag, chain, merge)
}

fn node_mut(dag: &mut PhysicalASAPDAG, id: usize) -> &mut PhysicalASAPDAGNode {
    dag.nodes.iter_mut().find(|node| node.id == id).unwrap()
}

/// The view at node `id` of `dag`, with the coverage every view of these
/// tests shares (one definition, an empty selection): materialized at
/// ingestion time, unless its lifetime already keeps it across evaluations
/// (then refreshed on first read).
fn view_at(dag: &mut PhysicalASAPDAG, id: usize) -> MaterializedView {
    node_mut(dag, id).view_coverage = Some(ViewCoverage::new("kll of t.value".into(), vec![]));
    views_at(dag, &[id as u64]).unwrap().remove(0)
}

/// The node of `chain_dag(width_ms)` whose operator `is_view` picks, as a
/// view refreshed on first read (θ `k = 200`, the coverage these tests
/// share), over its chain kept as a view refreshed at ingestion.
fn view_over_chain(width_ms: u64, is_view: fn(&Operator<usize>) -> bool) -> MaterializedView {
    let (mut dag, chain, _) = chain_dag(width_ms);
    let view = dag
        .nodes
        .iter()
        .find(|node| is_view(&node.payload))
        .unwrap()
        .id;
    let node = node_mut(&mut dag, view);
    node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
    node.retention_ms = Some(5 * MINUTE_MS);
    node.parameters = Some(SketchParams::Kll { k: 200 });
    node.view_coverage = Some(ViewCoverage::new("kll of t.value".into(), vec![]));
    views_at(&mut dag, &[chain as u64, view as u64])
        .unwrap()
        .into_iter()
        .find(|materialized| materialized.node_id == view)
        .unwrap()
}

fn is_merge(operator: &Operator<usize>) -> bool {
    matches!(operator, Operator::ASAP(ASAPOp::SummaryMerge { .. }))
}

fn is_time_range(operator: &Operator<usize>) -> bool {
    matches!(operator, Operator::NonASAP(NonASAPOp::TimeRange { .. }))
}

/// Chains of 1-minute and of 5-minute windows that differ in nothing else
/// have one coverage and different keys: the view node's own window
/// differs.
#[test]
fn chain_views_differing_only_in_window_width_have_different_view_state_keys() {
    let (mut one_minute, chain, _) = chain_dag(MINUTE_MS);
    let one_minute = view_at(&mut one_minute, chain);
    let (mut five_minutes, chain, _) = chain_dag(5 * MINUTE_MS);
    let five_minutes = view_at(&mut five_minutes, chain);
    assert_eq!(one_minute.coverage(), five_minutes.coverage());
    assert_ne!(one_minute.view_state_key(), five_minutes.view_state_key());
}

/// The last 5 minutes of a 1-minute chain are five window rows, of a
/// 5-minute chain one: a `TimeRange` view over a chain keeps the chain's
/// windows, so the two have different keys.
#[test]
fn time_range_views_over_chains_of_different_widths_have_different_view_state_keys() {
    let over_one_minute = view_over_chain(MINUTE_MS, is_time_range);
    let over_five_minutes = view_over_chain(5 * MINUTE_MS, is_time_range);
    assert_eq!(over_one_minute.coverage(), over_five_minutes.coverage());
    assert_ne!(
        over_one_minute.view_state_key(),
        over_five_minutes.view_state_key()
    );
}

/// A `TimeRangeCover` of the last 10 minutes over a 5-minute and a 1-minute
/// chain of one scan of `t`, as a view refreshed on first read (KLL
/// `k = 200`), over its chains kept as views refreshed at ingestion.
fn cover_view() -> MaterializedView {
    let new = |operator| OperatorNode::new_shared(operator).unwrap();
    let scan = new(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    }));
    let chain = |width_ms: u64| {
        new(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: scan.clone(),
            summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
            input: SummaryUpdate::column(ColumnRef::Named("value".into())),
            reduction: Reduction::Reduce(GroupKeys::default()),
            filter: None,
            tumbling_window_width_ms: width_ms,
        }))
    };
    let cover = new(Operator::NonASAP(NonASAPOp::TimeRangeCover {
        range: std::time::Duration::from_millis(10 * MINUTE_MS),
        offset_ms: 0,
        levels: vec![chain(5 * MINUTE_MS), chain(MINUTE_MS)],
    }));
    let merge = new(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![cover],
        group_by: None,
    }));
    let p50 = new(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: merge,
        query: SketchStatistic::Quantile { q: 0.5 },
    }));
    let mut dag =
        asap_types::test_support::forwarders::physical_asap_dag_at_query_time(&p50).unwrap();
    set_build_parameters(&mut dag.nodes, Some(&SketchParams::Kll { k: 200 }));
    let chains: Vec<u64> = dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
            )
        })
        .map(|node| node.id as u64)
        .collect();
    let cover = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                Operator::NonASAP(NonASAPOp::TimeRangeCover { .. })
            )
        })
        .unwrap()
        .id;
    let node = node_mut(&mut dag, cover);
    node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
    node.retention_ms = Some(10 * MINUTE_MS);
    node.parameters = Some(SketchParams::Kll { k: 200 });
    node.view_coverage = Some(ViewCoverage::new("kll of t.value".into(), vec![]));
    let mut ids = chains;
    ids.push(cover as u64);
    views_at(&mut dag, &ids)
        .unwrap()
        .into_iter()
        .find(|view| view.node_id == cover)
        .unwrap()
}

/// A `TimeRangeCover` view's rows are windows of each of its levels: its
/// key's window lists the levels' widths, widest first.
#[test]
fn a_time_range_cover_view_keys_its_levels_widths() {
    let view = cover_view();
    assert_eq!(
        serde_json::to_value(&view.view_state_key().window).unwrap(),
        serde_json::json!({"tumbling_levels": {"widths_ms": [300000, 60000]}})
    );
}

/// A merge of the last 5 minutes from a 1-minute chain and one from a
/// 5-minute chain hold one state, computed two ways: one key. The chain
/// below the view says how the state is computed, not what it is.
#[test]
fn merges_of_one_range_over_chains_of_different_widths_share_a_view_state_key() {
    let over_one_minute = view_over_chain(MINUTE_MS, is_merge);
    let over_five_minutes = view_over_chain(5 * MINUTE_MS, is_merge);
    assert_eq!(over_one_minute.coverage(), over_five_minutes.coverage());
    assert_eq!(
        over_one_minute.view_state_key(),
        over_five_minutes.view_state_key()
    );
}

/// One chain view refreshed at ingestion and kept in memory for ever, and
/// the same view refreshed on first read and kept on disk for an hour:
/// one stored state, so one key; two plans, so two plan ids.
#[test]
fn views_differing_only_in_refresh_retention_or_medium_have_equal_view_state_keys() {
    let (mut at_ingestion, chain, _) = chain_dag(MINUTE_MS);
    let at_ingestion_view = view_at(&mut at_ingestion, chain);
    let (mut on_first_read, chain, _) = chain_dag(MINUTE_MS);
    let node = node_mut(&mut on_first_read, chain);
    node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Disk);
    node.retention_ms = Some(60 * MINUTE_MS);
    let on_first_read_view = view_at(&mut on_first_read, chain);
    assert_eq!(
        at_ingestion_view.refresh(),
        MaterializedViewRefresh::AtIngestion
    );
    assert_eq!(
        on_first_read_view.refresh(),
        MaterializedViewRefresh::OnFirstRead
    );
    assert_ne!(
        at_ingestion_view.retention_ms(),
        on_first_read_view.retention_ms()
    );
    assert_eq!(
        at_ingestion_view.view_state_key(),
        on_first_read_view.view_state_key()
    );
    assert_ne!(at_ingestion.plan_id(), on_first_read.plan_id());
    assert_eq!(at_ingestion_view.plan_id(), at_ingestion.plan_id());
}

/// The committed sketch workload `chains-10m-5m-3m-10s` (quantiles over
/// `data` with `[10m]`, `[5m]` and `[3m]` ranges, the last every 10 s),
/// planned as its `mvp` snapshot is: raw data not retained, the snapshot's
/// cost inputs.
async fn chains_plan() -> InstallablePlan {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
    let workload: PlanningWorkload = serde_json::from_str(&read(
        "crates/planner-cli/tests/sketch_plan_snapshots/workloads/chains-10m-5m-3m-10s.json",
    ))
    .unwrap();
    let capabilities: DeploymentCapabilities = serde_json::from_str(&read(
        "crates/planner-cli/tests/sketch_plan_snapshots/deployments/mvp.capabilities.json",
    ))
    .unwrap();
    let cost_inputs: CostInputs = serde_json::from_str(&read(
        "tools/dag-viewer/cost-inputs/aws-fargate-us-east-1.json",
    ))
    .unwrap();
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_cost_inputs(&cost_inputs);
    plan_installable(UserInput::new(&workload, FrontendInput::Promql, models))
        .await
        .unwrap()
        .0
}

/// Main's snapshot of this workload has three views of one coverage: a
/// 5-minute chain, a 10-second chain and 60-second windows merged from it.
/// Their keys differ pairwise.
#[tokio::test]
async fn planned_views_of_one_coverage_get_distinct_view_state_keys() {
    let views = chains_plan().await.materialized_views().unwrap();
    let coverage = views[0].coverage();
    let same_coverage: Vec<&MaterializedView> = views
        .iter()
        .filter(|view| view.coverage() == coverage)
        .collect();
    assert_eq!(same_coverage.len(), 3, "{views:#?}");
    for (i, first) in same_coverage.iter().enumerate() {
        for second in &same_coverage[i + 1..] {
            assert_ne!(
                first.view_state_key(),
                second.view_state_key(),
                "views {} and {}",
                first.node_id,
                second.node_id
            );
        }
    }
}

/// The key of the planned 5-minute chain, written out as its JSON: what a
/// durable store recovers states by, so a change to it is a change to what
/// a deployment has stored. The JSON reads back as the same key.
#[tokio::test]
async fn the_view_state_key_of_a_planned_chain_is_pinned_and_round_trips_through_json() {
    let plan = chains_plan().await;
    let views = plan.materialized_views().unwrap();
    let five_minute_chain = views
        .iter()
        .find(|view| {
            plan.dag.nodes.iter().any(|node| {
                node.id == view.node_id
                    && matches!(
                        node.payload,
                        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                            tumbling_window_width_ms: 300_000,
                            ..
                        })
                    )
            })
        })
        .expect("a 5-minute chain view");
    let key = five_minute_chain.view_state_key();
    assert_eq!(
        serde_json::to_value(key).unwrap(),
        serde_json::json!({
            "definition_id": "bfbffb8f01ab202d585c3e18c9f78080",
            "window": {"tumbling": {"width_ms": 300000}},
            "logical_properties": {"parameters": {"Kll": {"k": 2863}}}
        })
    );
    let json = serde_json::to_string(key).unwrap();
    assert_eq!(&serde_json::from_str::<ViewStateKey>(&json).unwrap(), key);
}

/// `quantile_over_time(0.99, data[5m])` every minute, alone and next to a
/// query over another metric: two plans, one key for the quantile's view.
#[tokio::test]
async fn a_view_planned_in_two_workloads_keeps_its_view_state_key() {
    let workload = |queries: &[&str]| -> PlanningWorkload {
        let repeating: Vec<serde_json::Value> = queries
            .iter()
            .map(|query| {
                serde_json::json!({
                    "demand": {"fixed_interval_at": {"evaluation_phase": 0, "interval": 60000}},
                    "predictability": {"predictable": {"known_at": null}},
                    "query": query,
                    "requirements": {"accuracy": {"failure_probability": 0.01, "max_error": 0.01}}
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "data_workload": {
                "arrival": {"ingesting": {"rows_per_second": 2000.0}},
                "sources": {
                    "data": {"metric_type": "gauge", "sample_interval": 15000, "series_count": 10000},
                    "other": {"metric_type": "gauge", "sample_interval": 15000, "series_count": 10000}
                }
            },
            "query_workload": {"language": "promql", "query_batch": null, "repeating_queries": repeating}
        }))
        .unwrap()
    };
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        query_time_results_retained: false,
        ..Default::default()
    };
    let plan = |workload: PlanningWorkload| {
        let capabilities = capabilities.clone();
        async move {
            let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
            plan_installable(UserInput::new(&workload, FrontendInput::Promql, models))
                .await
                .unwrap()
                .0
        }
    };
    let alone = plan(workload(&["quantile_over_time(0.99, data[5m])"])).await;
    let beside = plan(workload(&[
        "quantile_over_time(0.99, data[5m])",
        "sum_over_time(other[1m])",
    ]))
    .await;
    let alone_views = alone.materialized_views().unwrap();
    let beside_views = beside.materialized_views().unwrap();
    assert!(!alone_views.is_empty());
    assert_ne!(alone.dag.plan_id(), beside.dag.plan_id());
    // A view's operator, as the plan file writes it (its inputs' node ids
    // removed): the quantile's views are found in the second plan by it.
    let operator_of = |plan: &InstallablePlan, view: &MaterializedView| {
        let node = plan
            .dag
            .nodes
            .iter()
            .find(|node| node.id == view.node_id)
            .unwrap();
        let mut payload = serde_json::to_value(&node.payload).unwrap();
        strip_node_ids(&mut payload);
        payload
    };
    for view in &alone_views {
        let same_operator: Vec<&MaterializedView> = beside_views
            .iter()
            .filter(|other| operator_of(&beside, other) == operator_of(&alone, view))
            .collect();
        assert_eq!(same_operator.len(), 1, "{:?}", view.view_state_key());
        assert_eq!(same_operator[0].view_state_key(), view.view_state_key());
    }
}

/// `value` without the node ids its operator's inputs are written as.
fn strip_node_ids(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for key in ["child", "children", "summary_input", "levels"] {
                fields.remove(key);
            }
            fields.values_mut().for_each(strip_node_ids);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_node_ids),
        _ => {}
    }
}
