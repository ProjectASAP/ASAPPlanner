//! Plan selection prices a stored view at the bytes the executor's store
//! holds for it: a 10,000-series exact Max chain of 15 s windows, built at
//! ingestion over two windows, priced and then stored.
use std::rc::Rc;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, test_support::views_at};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_types::ir::operator::{Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
use asap_types::workload::PlanningWorkload;
use futures::executor::block_on;

const SERIES: usize = 10_000;
const WIDTH_MS: i64 = 15_000;

fn node(operator: Operator) -> Rc<OperatorNode> {
    OperatorNode::new_shared(operator).unwrap()
}

fn samples_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
            Field::plain(PROMQL_SERIES_IDENTITY, DataType::Utf8, false),
        ],
        0,
        vec![vec![2, 0]],
    )
}

fn source() -> Source {
    Source::TimeSeries {
        metric: "queue_depth".into(),
    }
}

/// `max_over_time(queue_depth[15s])` over a chain of 15 s per-series Max
/// windows, the chain kept `retention_ms`: the DAG and the chain's id.
fn chain_plan(retention_ms: u64) -> (PhysicalASAPDAG, PhysicalASAPNodeId) {
    let scan = node(Operator::NonASAP(NonASAPOp::Scan {
        source: source(),
        predicates: vec![],
        schema: samples_schema(),
    }));
    let chain = node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan,
        summary_type: FieldDataType::ExactAggregate(ExactKind::Max, ExactParams::Max),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: WIDTH_MS as u64,
    }));
    let windows = node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: std::time::Duration::from_millis(WIDTH_MS as u64),
        kind: TimeRangeKind::Range,
        child: chain.clone(),
    }));
    let merge = node(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![windows],
        group_by: None,
    }));
    let root = node(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: merge,
        counter_readout: None,
    }));
    let (mut dag, nodes) =
        asap_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
            QueryRoot::Operator(root),
        ])
        .unwrap();
    let id = nodes.iter().position(|n| Rc::ptr_eq(n, &chain)).unwrap();
    dag.nodes
        .iter_mut()
        .find(|node| node.id == id)
        .unwrap()
        .retention_ms = Some(retention_ms);
    views_at(&mut dag, &[id as u64]).unwrap();
    (dag, id)
}

/// The 15 samples of each series in window `window` (`((window - 1) *
/// 15 s, window * 15 s]`); each series identity is a label set of about
/// 55 bytes, the length the cost model assumes.
fn window_samples(window: i64) -> Batch {
    let rows = (0..15i64)
        .flat_map(|second| {
            (0..SERIES).map(move |series| {
                vec![
                    Value::Timestamp((window - 1) * WIDTH_MS + second * 1_000 + 1),
                    Value::Float64((series as i64 + second) as f64),
                    Value::Utf8(
                        format!(
                            "{{\"instance\":\"host-{series:05}:9100\",\"job\":\"api\",\"region\":\"r{}\"}}",
                            series % 3
                        )
                        .into(),
                    ),
                ]
            })
        })
        .collect();
    Batch::try_new(Arc::new(samples_schema()), rows).unwrap()
}

/// The bytes the store holds for the chain after two windows are built and
/// appended, each with its own copy of the series identities.
fn stored_bytes_of_two_windows() -> usize {
    // A view kept 30 s holds both 15 s windows.
    let (mut dag, chain) = chain_plan(2 * WIDTH_MS as u64);
    let views = views_at(&mut dag, &[chain as u64]).unwrap();
    let refresh = compile(&dag, &[], &[chain as u64]).unwrap();
    let raw = InMemoryDataStore::default()
        .with_raw_data(
            source(),
            Arc::new(samples_schema()),
            vec![window_samples(1), window_samples(2)],
        )
        .unwrap();
    let raw = Arc::new(raw);
    let mut store = InMemoryDataStore::default();
    for window in 1..=2 {
        let context = RunContext::new(
            Scope::Ingestion {
                window_start_ms: (window - 1) * WIDTH_MS,
                window_end_ms: window * WIDTH_MS,
            },
            Limits::default(),
        )
        .unwrap();
        let run = refresh.instantiate(raw.clone()).unwrap();
        let batches: Vec<Batch> = block_on(async {
            let outputs = run.execute_ingestion(&[chain as u64], context)?.await?;
            Ok::<_, asap_executor::Error>(
                outputs
                    .into_iter()
                    .next()
                    .unwrap()
                    .iter()
                    .map(|batch| (**batch).clone())
                    .collect(),
            )
        })
        .unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), SERIES);
        store
            .append_materialized_view(
                &views[0],
                ((window - 1) * WIDTH_MS, window * WIDTH_MS),
                batches,
            )
            .unwrap();
    }
    store.materialized_view_bytes()
}

/// The bytes plan selection prices the chain's view at when it keeps two
/// windows (the one stored and the one being built, at a 15 s retention).
fn priced_bytes_of_two_windows() -> u64 {
    let (dag, chain) = chain_plan(WIDTH_MS as u64);
    let workload: PlanningWorkload = serde_json::from_value(serde_json::json!({
        "query_workload": {"language": "promql", "repeating_queries": [{
            "query": "max_over_time(queue_depth[15s])",
            "demand": {"fixed_interval_at": {"interval": 15000, "evaluation_phase": 0}},
            "requirements": {},
            "predictability": {"predictable": {"known_at": null}},
        }]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 10000.0}},
            "sources": {"queue_depth": {"series_count": SERIES, "metric_type": "gauge",
                "sample_interval": 1000}},
        },
    }))
    .unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let (cost, _) =
        asap_cost::plan_cost(&dag, &entries, &data, &asap_cost::CostInputs::default()).unwrap();
    let detail = &cost.per_node[&chain].detail;
    let keeps = detail
        .split("; keeps ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("the chain keeps its windows: {detail}"));
    keeps.parse().unwrap()
}

/// The store holds the series identities once for the view, a 4-byte key
/// per row, each window's 8-byte `window_end` and state index, and each
/// sealed 8-byte Max record: plan selection prices the two windows within
/// 15% of that.
#[test]
fn a_stored_chain_is_priced_at_what_the_store_holds() {
    let stored = stored_bytes_of_two_windows() as f64;
    let priced = priced_bytes_of_two_windows() as f64;
    let ratio = priced / stored;
    assert!(
        (1.0 / 1.15..=1.15).contains(&ratio),
        "priced {priced} bytes, stored {stored} ({:.1} and {:.1} per row), ratio {ratio:.3}",
        priced / (2 * SERIES) as f64,
        stored / (2 * SERIES) as f64
    );
}
