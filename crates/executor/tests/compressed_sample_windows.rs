//! Compressed sample windows run natively (design doc
//! `compressed-sample-windows.md`, tests E1–E7): a tumbling-window chain of
//! `FieldDataType::CompressedSamples` keeps every sample of each series per
//! window, and `DecodeSamplesInRange` turns the windows overlapping a range
//! (with `reads_unsealed_window`, the open window too) back into the raw
//! sample rows `TimeRange ← Scan` returns, so the exact operators answer
//! from them unchanged.
//!
//! Every expected value here comes from the raw-row plan over the same data
//! or from a codec's blocks filled directly, never from the operator under
//! test. The states are built with the executor's reference codec,
//! `PlainSampleBlockCodec`, unless a test injects its own through
//! `RunContext::with_sample_block_codec` (E7), as a deployment injects its
//! compressed codec.
use asap_executor::physical_planner::{compile, promql_rows, test_support::views_at};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::summary_kernels::{
    CompressedSamplesAccumulator, OpenSampleBlock, PlainSampleBlockCodec, SampleBlockCodec,
};
use asap_executor::values::{Batch, Value};
use asap_executor::KernelError;
use planner_types::accuracy::AccuracyTarget;
use planner_types::ir::operator::{Reduction, Source, TimeShift};
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{DataType, Field, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use planner_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
use planner_types::plan::materialized_view::MaterializedView;
use planner_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use planner_types::summary_type::SummaryUpdate;
use planner_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const SECOND_MS: i64 = 1_000;
const MINUTE_MS: i64 = 60 * SECOND_MS;
const GAUGE: &str = "latency_ms";
const COUNTER: &str = "requests_total";
const INSTANCES: [&str; 3] = ["a", "b", "c"];

// ── Data ────────────────────────────────────────────────────────────────

/// The `(instance, ts, value)` samples of `metric` up to `until_ms`: one
/// per instance about every 5 s, at irregular offsets (so delta-of-delta
/// timestamps take every short code), with repeated values. Gauge series
/// `a` and `c` hold two-decimal values (`m / 100`), series `b` random
/// doubles, and `c` one NaN at sample 100: the shapes a compressed codec
/// treats differently, which the decode returns bit for bit. The counter
/// holds integers and resets half way through.
fn samples(metric: &str, until_ms: i64) -> Vec<(&'static str, i64, f64)> {
    let mut samples = Vec::new();
    for (series, instance) in INSTANCES.iter().enumerate() {
        let mut counter = 0.0;
        let mut k = 0i64;
        loop {
            let t = k * 5 * SECOND_MS + (k * 37 + series as i64 * 211) % 1_300;
            if t > until_ms {
                break;
            }
            let value = if metric == COUNTER {
                if k == 300 {
                    counter = 0.0;
                }
                counter += ((k * 7 + series as i64) % 5) as f64;
                counter
            } else if k % 6 == 0 {
                // A repeated value: the XOR code of an equal value.
                10.0
            } else if *instance == "b" {
                // Not a short decimal.
                ((k * 7_919 + series as i64 * 104_729) % 10_007) as f64 / 7.0
            } else if *instance == "c" && k == 100 {
                // Not finite.
                f64::NAN
            } else {
                // Exactly `m / 10^2`.
                ((k * 7_919 + series as i64 * 104_729) % 100_003) as f64 / 100.0
            };
            samples.push((*instance, t, value));
            k += 1;
        }
    }
    samples
}

fn labels(metric: &str, instance: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("__name__".to_string(), metric.to_string()),
        ("instance".to_string(), instance.to_string()),
    ])
}

/// `samples` as the raw data of `metric`, in `schema`, one batch each in
/// the order given (shuffled orders arrive as given).
fn store_with(
    metric: &str,
    schema: &Schema,
    samples: &[(&str, i64, f64)],
) -> Arc<InMemoryDataStore> {
    let schema = Arc::new(schema.clone());
    let rows = samples
        .iter()
        .map(|&(instance, ts, value)| {
            promql_rows::series_row(&schema, &labels(metric, instance), ts, value).unwrap()
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    Arc::new(
        InMemoryDataStore::default()
            .with_raw_data(source(metric), schema, vec![batch])
            .unwrap(),
    )
}

// ── Plans ───────────────────────────────────────────────────────────────

/// Series identity, time, value and the `instance` label.
fn series_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain(PROMQL_SERIES_IDENTITY, DataType::Utf8, false),
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
            Field::plain("instance", DataType::Utf8, false),
        ],
        1,
        vec![],
    )
}

fn source(metric: &str) -> Source {
    Source::TimeSeries {
        metric: metric.into(),
    }
}

fn node(operator: Operator) -> Rc<OperatorNode> {
    OperatorNode::new_shared(operator).unwrap()
}

fn scan(metric: &str, schema: Schema) -> Rc<OperatorNode> {
    node(Operator::NonASAP(NonASAPOp::Scan {
        source: source(metric),
        predicates: vec![],
        schema,
    }))
}

/// The compressed-samples chain of `scan` per series, `width_ms` wide.
fn chain(scan: Rc<OperatorNode>, width_ms: i64) -> Rc<OperatorNode> {
    node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan,
        summary_type: FieldDataType::CompressedSamples,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: width_ms as u64,
    }))
}

/// `TimeShift(offset_ms)` over `child`, or `child` for no offset.
fn shifted(child: Rc<OperatorNode>, offset_ms: i64) -> Rc<OperatorNode> {
    if offset_ms == 0 {
        return child;
    }
    node(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms,
            at: None,
        },
        child,
    }))
}

/// The raw-row read the decode must equal: `TimeRange ← TimeShift? ← Scan`.
fn raw_range(
    scan: Rc<OperatorNode>,
    range_ms: i64,
    kind: TimeRangeKind,
    offset_ms: i64,
) -> Rc<OperatorNode> {
    node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(range_ms as u64),
        kind,
        child: shifted(scan, offset_ms),
    }))
}

/// The decode of `(T − offset − range, T − offset]` from `chain`, the
/// offset as a `TimeShift` over the chain (as Pass 2 re-wraps the shifts
/// it strips).
fn decode(
    chain: Rc<OperatorNode>,
    range_ms: i64,
    kind: TimeRangeKind,
    offset_ms: i64,
    labels: &[Field],
    reads_unsealed_window: bool,
) -> Rc<OperatorNode> {
    node(Operator::ASAP(ASAPOp::DecodeSamplesInRange {
        child: shifted(chain, offset_ms),
        range: Duration::from_millis(range_ms as u64),
        kind,
        labels: labels.to_vec(),
        reads_unsealed_window,
    }))
}

fn id_of(nodes: &[Rc<OperatorNode>], operator: &Rc<OperatorNode>) -> PhysicalASAPNodeId {
    nodes
        .iter()
        .position(|node| Rc::ptr_eq(node, operator))
        .unwrap()
}

/// A query DAG reading a compressed-samples chain kept as a view: the
/// chain at ingestion time, `roots` at query time.
struct ChainPlan {
    dag: PhysicalASAPDAG,
    chain: PhysicalASAPNodeId,
    roots: Vec<PhysicalASAPNodeId>,
    view: MaterializedView,
    width_ms: i64,
}

fn chain_plan(
    chain_node: &Rc<OperatorNode>,
    roots: &[Rc<OperatorNode>],
    width_ms: i64,
) -> ChainPlan {
    let query_roots: Vec<QueryRoot> = roots
        .iter()
        .map(|root| QueryRoot::Operator(root.clone()))
        .collect();
    let (mut dag, nodes) =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(
            &query_roots,
        )
        .unwrap();
    let chain = id_of(&nodes, chain_node);
    for node in dag.nodes.iter_mut().filter(|node| node.id == chain) {
        node.retention_ms = Some(2 * 3_600 * SECOND_MS as u64);
    }
    let view = views_at(&mut dag, &[chain as u64]).unwrap().remove(0);
    ChainPlan {
        chain,
        roots: roots.iter().map(|root| id_of(&nodes, root)).collect(),
        dag,
        view,
        width_ms,
    }
}

fn run(
    dag: &asap_executor::plan::PhysicalDAG<'static, Batch, asap_executor::values::SchemaRef>,
    roots: &[u64],
    context: RunContext,
) -> Vec<Vec<Vec<Value>>> {
    crate::common::run_roots(dag, roots, context)
        .unwrap()
        .into_iter()
        .map(|batches| batches.iter().flat_map(Batch::to_rows).collect())
        .collect()
}

fn chain_schema(plan: &ChainPlan) -> Arc<Schema> {
    Arc::new(
        plan.dag
            .nodes
            .iter()
            .find(|node| node.id == plan.chain)
            .unwrap()
            .output_schema
            .clone(),
    )
}

/// The chain's batches for its window `(start, end]`, built by an
/// ingestion run over the raw rows that arrived in it.
fn build_window(
    plan: &ChainPlan,
    store: &Arc<InMemoryDataStore>,
    start: i64,
    end: i64,
) -> Vec<Batch> {
    build_window_with(plan, store, start, end, Arc::new(PlainSampleBlockCodec))
}

/// [`build_window`] with the run's sample blocks written by `codec`.
fn build_window_with(
    plan: &ChainPlan,
    store: &Arc<InMemoryDataStore>,
    start: i64,
    end: i64,
    codec: Arc<dyn SampleBlockCodec>,
) -> Vec<Batch> {
    let refresh = compile(&plan.dag, &[], &[plan.chain as u64]).unwrap();
    let context = RunContext::new(
        Scope::Ingestion {
            window_start_ms: start,
            window_end_ms: end,
        },
        Limits::default(),
    )
    .unwrap()
    .with_sample_block_codec(codec);
    let rows = {
        let dag = refresh.instantiate(store.clone()).unwrap();
        run(&dag, &[plan.chain as u64], context).remove(0)
    };
    vec![Batch::try_new(chain_schema(plan), rows).unwrap()]
}

/// Refreshes the chain as a runtime does, in time order: each window that
/// ended in `(after, until]` is sealed and appended to the view.
fn seal_windows(plan: &ChainPlan, store: &mut Arc<InMemoryDataStore>, after: i64, until: i64) {
    let mut end = (after / plan.width_ms + 1) * plan.width_ms;
    while end <= until {
        let batches = build_window(plan, store, end - plan.width_ms, end);
        Arc::get_mut(store)
            .unwrap()
            .append_materialized_view(&plan.view, (end - plan.width_ms, end), batches)
            .unwrap();
        end += plan.width_ms;
    }
}

/// Hands the store the open window's snapshot at `t`: the samples that
/// arrived in `(window start, t]` (empty when `t` is on the grid).
fn publish_open_window(plan: &ChainPlan, store: &mut Arc<InMemoryDataStore>, t: i64) {
    let start = t.div_euclid(plan.width_ms) * plan.width_ms;
    let batches = if start == t {
        vec![]
    } else {
        build_window(plan, store, start, t)
    };
    Arc::get_mut(store)
        .unwrap()
        .set_open_window(&plan.view, (start, t), batches);
}

/// Each root's rows at evaluation time `t`.
fn query(plan: &ChainPlan, store: &Arc<InMemoryDataStore>, t: i64) -> Vec<Vec<Vec<Value>>> {
    let roots: Vec<u64> = plan.roots.iter().map(|&id| id as u64).collect();
    let compiled = compile(&plan.dag, std::slice::from_ref(&plan.view), &roots).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: t,
        },
        Limits::default(),
    )
    .unwrap();
    let dag = compiled.instantiate(store.clone()).unwrap();
    run(&dag, &roots, context)
}

/// The rows of the raw-row plan `root` at `t`, read from the raw data.
fn raw_rows(root: &Rc<OperatorNode>, store: &Arc<InMemoryDataStore>, t: i64) -> Vec<Vec<Value>> {
    let dag =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time(root).unwrap();
    let roots = [dag.roots[0] as u64];
    let compiled = compile(&dag, &[], &roots).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: t,
        },
        Limits::default(),
    )
    .unwrap();
    let dag = compiled.instantiate(store.clone()).unwrap();
    run(&dag, &roots, context).remove(0)
}

/// Rows as sortable text, floats by their bits: equal means equal bit for
/// bit, in any row order.
fn canonical(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut rows: Vec<String> = rows
        .into_iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    Value::Float64(v) => format!("f{:016x}", v.to_bits()),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    rows.sort();
    rows
}

// ── E1 ──────────────────────────────────────────────────────────────────

/// E1. `DecodeSamplesInRange` returns exactly the rows `TimeRange ← Scan`
/// returns over the same raw samples: range and instant reads, with and
/// without an offset, over 1 min and 5 min windows, at evaluation times on
/// the window grid and off it (every 7 s over 20 minutes), compared bit for
/// bit with the `instance` label column the chain keys its series by. A decode
/// that drops an edge sample (`(a, b]` read as `[a, b)`), misses the open
/// window, skips a window that only overlaps the range, re-encodes a value
/// lossily or mislabels a series fails it.
#[test]
fn decoded_rows_equal_raw_rows() {
    let until = 40 * MINUTE_MS;
    let data = samples(GAUGE, until);
    for width_ms in [MINUTE_MS, 5 * MINUTE_MS] {
        for (kind, range_ms) in [
            (TimeRangeKind::Range, 5 * MINUTE_MS),
            (TimeRangeKind::Range, 150 * SECOND_MS),
            (TimeRangeKind::Instant, 5 * MINUTE_MS),
        ] {
            for offset_ms in [0, 7 * SECOND_MS] {
                let scan_node = scan(GAUGE, series_schema());
                let chain_node = chain(scan_node.clone(), width_ms);
                let decoded = decode(chain_node.clone(), range_ms, kind, offset_ms, &[], true);
                let raw = raw_range(scan(GAUGE, series_schema()), range_ms, kind, offset_ms);
                let plan = chain_plan(&chain_node, &[decoded], width_ms);
                let mut store = store_with(GAUGE, &series_schema(), &data);
                let mut sealed_until = 0;
                let mut t = 20 * MINUTE_MS;
                while t <= until {
                    seal_windows(&plan, &mut store, sealed_until, t);
                    sealed_until = t;
                    publish_open_window(&plan, &mut store, t);
                    let got = query(&plan, &store, t).remove(0);
                    assert!(!got.is_empty() || kind == TimeRangeKind::Instant);
                    assert_eq!(
                        canonical(got),
                        canonical(raw_rows(&raw, &store, t)),
                        "width {width_ms} {kind:?} range {range_ms} offset {offset_ms} at {t}"
                    );
                    t += 7 * SECOND_MS;
                }
            }
        }
    }
}

// ── E2 ──────────────────────────────────────────────────────────────────

/// `query` lowered by the PromQL front end, with series identities, as the
/// Planner plans it.
fn lower(query: &str) -> Rc<OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::exact(),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(60_000),
        )),
    };
    let root = asap_frontend_promql::lower_promql_query_workload(&workload)
        .unwrap()
        .remove(0)
        .as_operator()
        .cloned()
        .expect("operator root");
    planner_types::ir::schema::series_identity::with_promql_series_identity(&root).unwrap()
}

/// The one `Scan` of `root`.
fn the_scan(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    let scans: Vec<_> = OperatorNode::reachable(root)
        .into_iter()
        .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
        .collect();
    assert_eq!(scans.len(), 1, "one selector");
    scans[0].clone()
}

/// `root` with its `TimeRange ← Scan` replaced by an unsealed-read decode
/// of `chain` (the chain over that same scan, so its rows carry the scan's
/// label columns and the decode derives none): the candidate Pass 2 adds
/// to the raw-range group.
fn with_decode(
    root: &Rc<OperatorNode>,
    chain_node: &Rc<OperatorNode>,
    replaced: &mut usize,
) -> Rc<OperatorNode> {
    if let Some(NonASAPOp::TimeRange { range, kind, child }) = root.non_asap() {
        if let Some(NonASAPOp::Scan { .. }) = child.non_asap() {
            *replaced += 1;
            return decode(
                chain_node.clone(),
                range.as_millis() as i64,
                *kind,
                0,
                &[],
                true,
            );
        }
    }
    Rc::new(
        root.with_new_children(|child| with_decode(child, chain_node, replaced))
            .unwrap(),
    )
}

/// E2. Each PromQL function the design lists as answered from compressed
/// samples gives, through the decode, the raw-row plan's answer bit for
/// bit, every 10 s for 30 minutes, over 5 min windows read with the open
/// window: quantile, max, min, sum, count, avg over time; `rate` and
/// `increase` over a counter that resets; `last_over_time`; and `topk`
/// over values. A decode that loses or duplicates one sample, reorders
/// two (so a counter reset looks different), or rounds a value fails at
/// least one of them.
#[test]
fn exact_answers_equal_raw_plan() {
    let width_ms = 5 * MINUTE_MS;
    let until = 45 * MINUTE_MS;
    for (metric, promql) in [
        (GAUGE, "quantile_over_time(0.99, latency_ms[5m])"),
        (GAUGE, "max_over_time(latency_ms[5m])"),
        (GAUGE, "min_over_time(latency_ms[5m])"),
        (GAUGE, "sum_over_time(latency_ms[5m])"),
        (GAUGE, "count_over_time(latency_ms[5m])"),
        (GAUGE, "avg_over_time(latency_ms[5m])"),
        (GAUGE, "last_over_time(latency_ms[5m])"),
        (GAUGE, "topk(2, last_over_time(latency_ms[5m]))"),
        (COUNTER, "rate(requests_total[5m])"),
        (COUNTER, "increase(requests_total[5m])"),
    ] {
        let raw = lower(promql);
        let scan_node = the_scan(&raw);
        let Some(NonASAPOp::Scan { schema, .. }) = scan_node.non_asap() else {
            unreachable!()
        };
        let schema = schema.clone();
        let chain_node = chain(scan_node.clone(), width_ms);
        let mut replaced = 0;
        let decoded = with_decode(&raw, &chain_node, &mut replaced);
        assert_eq!(replaced, 1, "{promql}: one raw range read");
        let plan = chain_plan(&chain_node, &[decoded], width_ms);
        let mut store = store_with(metric, &schema, &samples(metric, until));
        let mut sealed_until = 0;
        let mut t = 15 * MINUTE_MS;
        while t <= until {
            seal_windows(&plan, &mut store, sealed_until, t);
            sealed_until = t;
            publish_open_window(&plan, &mut store, t);
            assert_eq!(
                canonical(query(&plan, &store, t).remove(0)),
                canonical(raw_rows(&raw, &store, t)),
                "{promql} at {t}"
            );
            t += 10 * SECOND_MS;
        }
    }
}

// ── E3 ──────────────────────────────────────────────────────────────────

/// E3. At an evaluation time in the middle of a window, an unsealed read
/// sees the samples of `(window start, T]` from the open window's
/// snapshot, and a sealed read does not: with 5 min windows at
/// T = 12 min 30 s, the unsealed read of `(T − 5 min, T]` holds every
/// sample of the raw read, the sealed one only those up to 10 min. A
/// runtime that ignores the snapshot, or a sealed read that reads it,
/// fails.
#[test]
fn open_window_is_read() {
    let width_ms = 5 * MINUTE_MS;
    let t = 12 * MINUTE_MS + 30 * SECOND_MS;
    let data = samples(GAUGE, 20 * MINUTE_MS);
    let chain_node = chain(scan(GAUGE, series_schema()), width_ms);
    let unsealed = decode(
        chain_node.clone(),
        width_ms,
        TimeRangeKind::Range,
        0,
        &[],
        true,
    );
    let sealed = decode(
        chain_node.clone(),
        width_ms,
        TimeRangeKind::Range,
        0,
        &[],
        false,
    );
    let plan = chain_plan(&chain_node, &[unsealed, sealed], width_ms);
    let mut store = store_with(GAUGE, &series_schema(), &data);
    seal_windows(&plan, &mut store, 0, t);
    publish_open_window(&plan, &mut store, t);
    let answers = query(&plan, &store, t);
    let raw = raw_range(
        scan(GAUGE, series_schema()),
        width_ms,
        TimeRangeKind::Range,
        0,
    );
    let raw_rows = raw_rows(&raw, &store, t);
    let in_open_window =
        |row: &Vec<Value>| matches!(row[1], Value::Timestamp(ts) if ts > 10 * MINUTE_MS);
    assert!(
        raw_rows.iter().any(in_open_window),
        "the data has open-window samples"
    );
    assert_eq!(canonical(answers[0].clone()), canonical(raw_rows.clone()));
    let up_to_sealed: Vec<_> = raw_rows
        .into_iter()
        .filter(|row| !in_open_window(row))
        .collect();
    assert_eq!(canonical(answers[1].clone()), canonical(up_to_sealed));
}

// ── E4, E5, E7: the sealed state ────────────────────────────────────────

/// The sealed compressed-samples states of one window, per series
/// identity, read back from the chain's output rows.
fn sealed_states(batches: &[Batch]) -> BTreeMap<String, CompressedSamplesAccumulator> {
    batches
        .iter()
        .flat_map(Batch::to_rows)
        .map(|row| {
            let identity = row
                .iter()
                .find_map(|value| match value {
                    Value::Utf8(identity) if identity.contains("instance") => {
                        Some(identity.to_string())
                    }
                    _ => None,
                })
                .unwrap();
            let state = row
                .iter()
                .find_map(|value| match value {
                    Value::Summary { family, state } => {
                        assert_eq!(family, &FieldDataType::CompressedSamples);
                        Some(
                            state
                                .as_any()
                                .downcast_ref::<CompressedSamplesAccumulator>()
                                .expect("a compressed-samples kernel")
                                .clone(),
                        )
                    }
                    _ => None,
                })
                .unwrap();
            (identity, state)
        })
        .collect()
}

/// The window `(start, end]` of `data`, per series identity, appended in
/// time order to an open block of `codec` and sealed: the reference block.
fn reference_blocks(
    data: &[(&str, i64, f64)],
    start: i64,
    end: i64,
    codec: &dyn SampleBlockCodec,
) -> BTreeMap<String, Vec<u8>> {
    let mut sorted: Vec<_> = data
        .iter()
        .filter(|(_, t, _)| start < *t && *t <= end)
        .copied()
        .collect();
    sorted.sort_by_key(|&(instance, t, _)| (instance, t));
    let mut blocks: BTreeMap<String, Box<dyn OpenSampleBlock>> = BTreeMap::new();
    for (instance, t, value) in sorted {
        let identity = promql_rows::encode_series_identity(&labels(GAUGE, instance)).unwrap();
        blocks
            .entry(identity)
            .or_insert_with(|| codec.new_block())
            .append(t, value);
    }
    blocks
        .into_iter()
        .map(|(identity, block)| (identity, block.seal()))
        .collect()
}

/// A deployment's codec as a test injects one: the reference blocks behind
/// a one-byte tag, so a block it wrote is told apart from a reference one.
#[derive(Debug)]
struct TaggedCodec;

const TAG: u8 = 0xA5;

/// An open block of [`TaggedCodec`].
struct TaggedBlock(Box<dyn OpenSampleBlock>);

impl OpenSampleBlock for TaggedBlock {
    fn append(&mut self, timestamp_ms: i64, value: f64) {
        self.0.append(timestamp_ms, value);
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn memory_bytes(&self) -> usize {
        self.0.memory_bytes()
    }
    fn seal(&self) -> Vec<u8> {
        let mut bytes = vec![TAG];
        bytes.extend(self.0.seal());
        bytes
    }
}

impl SampleBlockCodec for TaggedCodec {
    fn new_block(&self) -> Box<dyn OpenSampleBlock> {
        Box::new(TaggedBlock(PlainSampleBlockCodec.new_block()))
    }
    fn decode(&self, block: &[u8]) -> Result<Vec<(i64, f64)>, KernelError> {
        match block.split_first() {
            Some((&TAG, rest)) => PlainSampleBlockCodec.decode(rest),
            _ => Err("not a tagged block".into()),
        }
    }
}

/// E4. Samples that arrive out of timestamp order within a window (the
/// raw batch shuffled) seal into the same block, byte for byte, as the
/// samples appended in time order to a reference-codec block and sealed.
/// A builder that appends in arrival order, or drops a late-within-window
/// sample, fails.
#[test]
fn out_of_order_within_window_is_exact() {
    let width_ms = 5 * MINUTE_MS;
    let mut data = samples(GAUGE, 10 * MINUTE_MS);
    // A fixed shuffle: reverse each run of 7 samples.
    for run in data.chunks_mut(7) {
        run.reverse();
    }
    let chain_node = chain(scan(GAUGE, series_schema()), width_ms);
    let plan = chain_plan(&chain_node, std::slice::from_ref(&chain_node), width_ms);
    let store = store_with(GAUGE, &series_schema(), &data);
    let states = sealed_states(&build_window(&plan, &store, width_ms, 2 * width_ms));
    let expected = reference_blocks(&data, width_ms, 2 * width_ms, &PlainSampleBlockCodec);
    assert_eq!(states.len(), INSTANCES.len());
    for (identity, state) in states {
        assert_eq!(state.block(), expected[&identity], "{identity}");
    }
}

/// E5. The sealed column holds each state in the bytes the cost model
/// prices it at: its sealed block's length plus an 8-byte offset, no
/// slack per state. A column that keeps the open
/// encoder's spare capacity, or one allocation per state, fails.
#[test]
fn sealed_bytes_match_cost_model() {
    let width_ms = 5 * MINUTE_MS;
    let data = samples(GAUGE, 10 * MINUTE_MS);
    let chain_node = chain(scan(GAUGE, series_schema()), width_ms);
    let plan = chain_plan(&chain_node, std::slice::from_ref(&chain_node), width_ms);
    let store = store_with(GAUGE, &series_schema(), &data);
    let batches = build_window(&plan, &store, width_ms, 2 * width_ms);
    let state_column_index = chain_schema(&plan)
        .fields
        .iter()
        .position(|field| field.dtype == FieldDataType::CompressedSamples)
        .unwrap();
    let held: usize = batches
        .iter()
        .map(|batch| {
            batch
                .state_column(state_column_index)
                .expect("a state column")
                .state_memory_bytes()
        })
        .sum();
    let priced: usize = reference_blocks(&data, width_ms, 2 * width_ms, &PlainSampleBlockCodec)
        .values()
        .map(|block| block.len() + 8)
        .sum();
    assert_eq!(held, priced);
}

/// E7. A sealed state persists as its codec's bytes: a window built with
/// the codec injected through `RunContext::with_sample_block_codec` (a
/// test codec that tags its blocks) holds exactly the bytes that codec
/// seals for the window's samples appended in time order, and restoring
/// those bytes with it reads back the same samples. A kernel that ignores
/// the injected codec, or adds its own serialization (a second format a
/// data plane would have to read), fails.
#[test]
fn sealed_window_bytes_are_the_portable_format() {
    let width_ms = MINUTE_MS;
    let data = samples(GAUGE, 5 * MINUTE_MS);
    let chain_node = chain(scan(GAUGE, series_schema()), width_ms);
    let plan = chain_plan(&chain_node, std::slice::from_ref(&chain_node), width_ms);
    let store = store_with(GAUGE, &series_schema(), &data);
    let codec: Arc<dyn SampleBlockCodec> = Arc::new(TaggedCodec);
    let states = sealed_states(&build_window_with(
        &plan,
        &store,
        3 * width_ms,
        4 * width_ms,
        codec.clone(),
    ));
    let expected = reference_blocks(&data, 3 * width_ms, 4 * width_ms, &TaggedCodec);
    assert_eq!(states.len(), INSTANCES.len());
    let bits = |samples: Vec<(i64, f64)>| -> Vec<(i64, u64)> {
        samples.into_iter().map(|(t, v)| (t, v.to_bits())).collect()
    };
    for (identity, state) in states {
        let bytes = state.block().to_vec();
        assert_eq!(bytes, expected[&identity], "{identity}");
        let restored =
            CompressedSamplesAccumulator::restore_from_disk(bytes, codec.clone()).unwrap();
        assert_eq!(
            bits(restored.samples_in(i64::MIN, i64::MAX).unwrap()),
            bits(TaggedCodec.decode(&expected[&identity]).unwrap()),
        );
    }
}

// ── E6 ──────────────────────────────────────────────────────────────────

/// E6. `max_over_time(latency_ms[30m])` over 200 series, 1 sample/s, decoded from
/// 1 min windows, holds at most a few series' decoded rows at once: its
/// peak reserved bytes stay under twice one series' raw rows plus 1 MiB,
/// where decoding every series before evaluating (about 360,000 rows)
/// would need over 10 MiB. A decode that materializes the whole range, or
/// a range function that holds every series' rows, fails.
#[test]
fn decode_holds_one_series_per_thread() {
    let width_ms = MINUTE_MS;
    let range_ms = 30 * MINUTE_MS;
    let t = 40 * MINUTE_MS;
    let raw = lower("max_over_time(latency_ms[30m])");
    let scan_node = the_scan(&raw);
    let Some(NonASAPOp::Scan { schema, .. }) = scan_node.non_asap() else {
        unreachable!()
    };
    let schema = schema.clone();
    let mut data = Vec::new();
    let instances: Vec<&'static str> = (0..200)
        .map(|i| &*Box::leak(format!("i{i}").into_boxed_str()))
        .collect();
    for (series, instance) in instances.iter().enumerate() {
        for second in 1..=t / SECOND_MS {
            data.push((
                *instance,
                second * SECOND_MS,
                ((second * 31 + series as i64) % 1_000) as f64,
            ));
        }
    }
    let chain_node = chain(scan_node.clone(), width_ms);
    let mut replaced = 0;
    let max_over_time = with_decode(&raw, &chain_node, &mut replaced);
    assert_eq!(replaced, 1);
    let plan = chain_plan(&chain_node, &[max_over_time], width_ms);
    let mut store = store_with(GAUGE, &schema, &data);
    seal_windows(&plan, &mut store, 0, t);
    publish_open_window(&plan, &mut store, t);
    let roots: Vec<u64> = plan.roots.iter().map(|&id| id as u64).collect();
    let compiled = compile(&plan.dag, std::slice::from_ref(&plan.view), &roots).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: t,
        },
        Limits::default(),
    )
    .unwrap();
    let answers = {
        let dag = compiled.instantiate(store.clone()).unwrap();
        run(&dag, &roots, context.clone()).remove(0)
    };
    assert_eq!(answers.len(), instances.len());
    let one_series_rows_bytes = (range_ms / SECOND_MS) as usize * 48;
    assert!(
        context.peak_bytes() <= 2 * one_series_rows_bytes + (1 << 20),
        "peak {} bytes",
        context.peak_bytes()
    );
}
