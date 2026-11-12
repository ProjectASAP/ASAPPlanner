//! A tumbling-window chain runs natively: each ingestion run builds the
//! chain's window ending at the run's end, the runtime appends it to its
//! time-indexed store, and a query run merges the windows the `TimeRange`
//! (and `TimeShift`) above the chain select; a chain built at query time
//! builds those windows in the query run. And a `TimeShift` with an `@`
//! anchor evaluates its input at the anchor's time.
use asap_executor::physical_planner::{compile, test_support::views_at};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::{AtModifier, Reduction, Source, TimeShift};
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use planner_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
use planner_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use planner_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const MINUTE_MS: i64 = 60_000;
const WIDTH_MS: i64 = 10 * MINUTE_MS;
const HOUR_MS: i64 = 60 * MINUTE_MS;

fn samples_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ],
        1,
        vec![],
    )
}

fn source() -> Source {
    Source::TimeSeries {
        metric: "latency".into(),
    }
}

fn node(operator: Operator) -> Rc<OperatorNode> {
    OperatorNode::new_shared(operator).unwrap()
}

fn scan() -> Rc<OperatorNode> {
    node(Operator::NonASAP(NonASAPOp::Scan {
        source: source(),
        predicates: vec![],
        schema: samples_schema(),
    }))
}

/// The exact per-`job` sum of `value`.
fn sum() -> FieldDataType {
    FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
}

fn range(width_ms: i64, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(width_ms as u64),
        kind: TimeRangeKind::Range,
        child,
    }))
}

fn shift(offset_ms: i64, at: Option<AtModifier>, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift { offset_ms, at },
        child,
    }))
}

/// The sum per job, finalized, of the states `states` gives.
fn finalized_sum(states: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: states,
    }))
}

/// Two series, `a` and `b`, one sample per minute over `(0, 4h]`: `a` is
/// worth `t` minutes, `b` twice that.
fn samples() -> Vec<(&'static str, i64, f64)> {
    (1..=4 * 60)
        .flat_map(|minute| {
            let t = minute * MINUTE_MS;
            [("a", t, minute as f64), ("b", t, 2.0 * minute as f64)]
        })
        .collect()
}

/// The sum per job of the samples in `(start, end]`, computed directly.
fn direct_sums(start: i64, end: i64) -> BTreeMap<String, f64> {
    let mut sums = BTreeMap::new();
    for (job, t, value) in samples() {
        if start < t && t <= end {
            *sums.entry(job.to_string()).or_insert(0.0) += value;
        }
    }
    sums
}

fn store_with_samples() -> InMemoryDataStore {
    let schema = Arc::new(samples_schema());
    let rows = samples()
        .into_iter()
        .map(|(job, t, value)| {
            vec![
                Value::Utf8(job.into()),
                Value::Timestamp(t),
                Value::Float64(value),
            ]
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    InMemoryDataStore::default()
        .with_raw_data(source(), schema, vec![batch])
        .unwrap()
}

/// `root`'s rows as a sum per job: its `job` and `Float64` columns.
fn sums_of(rows: Vec<Vec<Value>>) -> BTreeMap<String, f64> {
    rows.into_iter()
        .map(|row| {
            let job = row.iter().find_map(|value| match value {
                Value::Utf8(job) => Some(job.to_string()),
                _ => None,
            });
            let sum = row.iter().find_map(|value| match value {
                Value::Float64(sum) => Some(*sum),
                _ => None,
            });
            (job.unwrap(), sum.unwrap())
        })
        .collect()
}

fn run(
    dag: &asap_executor::plan::PhysicalDAG<'static, Batch, asap_executor::values::SchemaRef>,
    roots: &[u64],
    context: RunContext,
) -> Vec<Vec<Vec<Value>>> {
    // The roots share producers, so they are read together.
    block_on(futures::future::join_all(
        dag.execute(roots, context)
            .unwrap()
            .into_iter()
            .map(|mut output| async move {
                let mut rows = Vec::new();
                while let Some(batch) = output.next().await {
                    rows.extend(batch.unwrap().to_rows());
                }
                rows
            }),
    ))
}

/// The id of the node of `dag` that is `operator`'s node in `nodes`.
fn id_of(nodes: &[Rc<OperatorNode>], operator: &Rc<OperatorNode>) -> PhysicalASAPNodeId {
    nodes
        .iter()
        .position(|node| Rc::ptr_eq(node, operator))
        .unwrap()
}

/// `quantile`-style panel over a chain, with exact sums: the chain of
/// 10-minute windows over `latency`, built at ingestion and retained for
/// 2 h; query `now` merges the windows in `(T - 1h, T]`, query `shifted`
/// (`offset 1h`) those in `(T - 2h, T - 1h]`.
fn chain_plan() -> (PhysicalASAPDAG, PhysicalASAPNodeId, [PhysicalASAPNodeId; 2]) {
    let chain = node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan(),
        summary_type: sum(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![0]),
        filter: None,
        tumbling_window_width_ms: WIDTH_MS as u64,
    }));
    let merge = |windows| {
        node(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![windows],
            group_by: None,
        }))
    };
    let now = finalized_sum(merge(range(HOUR_MS, chain.clone())));
    let shifted = finalized_sum(merge(range(HOUR_MS, shift(HOUR_MS, None, chain.clone()))));
    let (mut dag, nodes) =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
            QueryRoot::Operator(now.clone()),
            QueryRoot::Operator(shifted.clone()),
        ])
        .unwrap();
    let chain_id = id_of(&nodes, &chain);
    // Stage 3 sets the chain's retention: the shifted query reads 2 h back.
    dag.nodes
        .iter_mut()
        .find(|node| node.id == chain_id)
        .unwrap()
        .retention_ms = Some(2 * HOUR_MS as u64);
    (
        dag,
        chain_id,
        [id_of(&nodes, &now), id_of(&nodes, &shifted)],
    )
}

// Ingestion runs build each 10-minute window of the chain into the store;
// at T = 4h the query merges the six windows of the last hour, and the
// shifted one the six of the hour before, each equal to the sum computed
// directly over the raw samples. Windows older than the retention are
// dropped, so a range reaching past it is refused.
#[test]
fn a_chain_built_at_ingestion_answers_queries_through_its_time_ranges() {
    let (mut dag, chain, [now, shifted]) = chain_plan();
    let views = views_at(&mut dag, &[chain as u64]).unwrap();
    assert_eq!(views[0].retention_ms(), 2 * HOUR_MS as u64);
    dag.materialized_views().unwrap();
    assert_eq!(
        dag.refresh_window(chain),
        planner_types::plan::materialized_view::RefreshWindow::of_window(0, WIDTH_MS)
    );
    let refresh = compile(&dag, &[], &[chain as u64]).unwrap();
    let query = compile(&dag, &views, &[now as u64, shifted as u64]).unwrap();
    let mut store = Arc::new(store_with_samples());
    let mut window_end = WIDTH_MS;
    while window_end <= 4 * HOUR_MS {
        let context = RunContext::new(
            Scope::Ingestion {
                window_start_ms: window_end - WIDTH_MS,
                window_end_ms: window_end,
            },
            Limits::default(),
        )
        .unwrap();
        let rows = {
            let refresh = refresh.instantiate(store.clone()).unwrap();
            run(&refresh, &[chain as u64], context).remove(0)
        };
        // One row per job, stamped with the window's end.
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|row| matches!(row[0], Value::Timestamp(t) if t == window_end)));
        let schema = Arc::new(
            dag.nodes
                .iter()
                .find(|node| node.id == chain)
                .unwrap()
                .output_schema
                .clone(),
        );
        let batch = Batch::try_new(schema, rows).unwrap();
        Arc::get_mut(&mut store)
            .unwrap()
            .append_materialized_view(&views[0], (window_end - WIDTH_MS, window_end), vec![batch])
            .unwrap();
        window_end += WIDTH_MS;
    }
    let evaluation_time_ms = 4 * HOUR_MS;
    let context = RunContext::new(
        Scope::Query { evaluation_time_ms },
        Limits {
            ..Limits::default()
        },
    )
    .unwrap();
    let answers = {
        let query = query.instantiate(store.clone()).unwrap();
        run(&query, &[now as u64, shifted as u64], context)
    };
    assert_eq!(
        sums_of(answers[0].clone()),
        direct_sums(evaluation_time_ms - HOUR_MS, evaluation_time_ms)
    );
    assert_eq!(
        sums_of(answers[1].clone()),
        direct_sums(
            evaluation_time_ms - 2 * HOUR_MS,
            evaluation_time_ms - HOUR_MS
        )
    );
    // At T = 3h the shifted query reads (1h, 2h], whose windows ended more
    // than 2 h before the newest one (4h) and are gone.
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 3 * HOUR_MS,
        },
        Limits::default(),
    )
    .unwrap();
    let query = query.instantiate(store).unwrap();
    let error = block_on(async {
        let mut outputs = query.execute(&[shifted as u64], context).unwrap();
        let mut error = None;
        while let Some(batch) = outputs[0].next().await {
            if let Err(e) = batch {
                error = Some(e);
            }
        }
        error
    });
    assert!(error.is_some_and(|error| error.to_string().contains("no rows tiling")));
}

// A chain built at query time, of 1-minute windows over the raw samples,
// read by `sum_over_time(latency[5m])` and `sum_over_time(latency[5m]
// offset 1m)`: at T = 4h the run builds, from the `Scan`'s rows in `(T -
// 6m, T]`, one state per job and window `(T - (k+1)m, T - km]` stamped with
// its end, and the `TimeRange`s merge the windows of `(T - 5m, T]` and
// `(T - 6m, T - 1m]`, equal to the sums computed directly over the raw
// samples (one sample per minute, on the windows' ends).
#[test]
fn a_chain_built_at_query_time_answers_queries_through_its_time_ranges() {
    let chain = node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan(),
        summary_type: sum(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![0]),
        filter: None,
        tumbling_window_width_ms: MINUTE_MS as u64,
    }));
    let merge = |windows| {
        node(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![windows],
            group_by: None,
        }))
    };
    let now = finalized_sum(merge(range(5 * MINUTE_MS, chain.clone())));
    let shifted = finalized_sum(merge(range(
        5 * MINUTE_MS,
        shift(MINUTE_MS, None, chain.clone()),
    )));
    let (dag, nodes) =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
            QueryRoot::Operator(now.clone()),
            QueryRoot::Operator(shifted.clone()),
        ])
        .unwrap();
    let chain_node = dag
        .nodes
        .iter()
        .find(|node| node.id == id_of(&nodes, &chain))
        .unwrap();
    assert_eq!(
        chain_node.output_state.timing,
        planner_types::ir::properties::ExecutionTiming::QueryTime
    );
    let roots = [id_of(&nodes, &now) as u64, id_of(&nodes, &shifted) as u64];
    let query = compile(&dag, &[], &roots).unwrap();
    let evaluation_time_ms = 4 * HOUR_MS;
    let context = RunContext::new(Scope::Query { evaluation_time_ms }, Limits::default()).unwrap();
    let answers = {
        let query = query.instantiate(Arc::new(store_with_samples())).unwrap();
        run(&query, &roots, context)
    };
    assert_eq!(
        sums_of(answers[0].clone()),
        direct_sums(evaluation_time_ms - 5 * MINUTE_MS, evaluation_time_ms)
    );
    assert_eq!(
        sums_of(answers[1].clone()),
        direct_sums(
            evaluation_time_ms - 6 * MINUTE_MS,
            evaluation_time_ms - MINUTE_MS
        )
    );
}

// A range window with tumbling-window ends: at T = 10m, `(T - 3m, T]` in
// windows of 1 minute keeps the rows at 7m (out), 7m + 1 ms, 8m, 9m 30 s and
// 10m, their times replaced by their windows' ends 8m, 8m, 10m and 10m.
#[test]
fn a_range_window_stamps_each_row_with_its_tumbling_windows_end() {
    use asap_executor::operators::Operator as ExecutorOperator;
    use asap_executor::plan::PhysicalDAG;
    let schema = Arc::new(samples_schema());
    let times = [
        7 * MINUTE_MS,
        7 * MINUTE_MS + 1,
        8 * MINUTE_MS,
        570_000,
        10 * MINUTE_MS,
    ];
    let rows = times
        .iter()
        .map(|&t| {
            vec![
                Value::Utf8("a".into()),
                Value::Timestamp(t),
                Value::Float64(1.0),
            ]
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        ExecutorOperator::source(schema.clone(), vec![batch]).unwrap(),
    )
    .unwrap();
    let window = ExecutorOperator::time_range(schema, 1, 0, 3 * MINUTE_MS, None, None)
        .unwrap()
        .with_tumbling_window_ends(MINUTE_MS)
        .unwrap();
    dag.add(1, vec![0], window).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 10 * MINUTE_MS,
        },
        Limits::default(),
    )
    .unwrap();
    let ends: Vec<i64> = run(&dag, &[1], context)
        .remove(0)
        .into_iter()
        .map(|row| match row[1] {
            Value::Timestamp(t) => t,
            ref other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ends, [8, 8, 10, 10].map(|minutes| minutes * MINUTE_MS));
}

// `sum_over_time(latency[5m] @ end())` in a range request over (1h, 3h]:
// every step's run evaluates the sum over the request's last five minutes,
// `(3h - 5m, 3h]`; an instant query (no request bounds) over its own last
// five minutes.
#[test]
fn a_time_shift_at_end_evaluates_its_input_at_the_requests_end() {
    let per_job = node(Operator::ASAP(ASAPOp::SummaryAgg {
        child: range(5 * MINUTE_MS, scan()),
        summary_type: sum(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![0]),
        filter: None,
    }));
    let anchored = shift(0, Some(AtModifier::End), finalized_sum(per_job));
    let (dag, nodes) =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
            QueryRoot::Operator(anchored.clone()),
        ])
        .unwrap();
    let root = id_of(&nodes, &anchored) as u64;
    let query = compile(&dag, &[], &[root]).unwrap();
    assert_eq!(query.operator_name(root), Some("TimeShift"));
    let query = query.instantiate(Arc::new(store_with_samples())).unwrap();
    let (start, end) = (HOUR_MS, 3 * HOUR_MS);
    for step in [start, 2 * HOUR_MS, end] {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: step,
            },
            Limits::default(),
        )
        .unwrap()
        .with_query_range(start, end)
        .unwrap();
        assert_eq!(
            sums_of(run(&query, &[root], context).remove(0)),
            direct_sums(end - 5 * MINUTE_MS, end),
            "step {step}"
        );
    }
    let instant = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 2 * HOUR_MS,
        },
        Limits::default(),
    )
    .unwrap();
    assert_eq!(
        sums_of(run(&query, &[root], instant).remove(0)),
        direct_sums(2 * HOUR_MS - 5 * MINUTE_MS, 2 * HOUR_MS)
    );
}
