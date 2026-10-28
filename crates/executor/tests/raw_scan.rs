//! Scan acceptance uses the public data store contract and Planner physical DAGs.
use asap_executor::{
    physical_planner::compile,
    plan::PhysicalDAG,
    runtime::{Limits, OutputStream, RunContext, Scope},
    sources::{DataStore, InMemoryDataStore, MaterializedViewRequest, RawDataRequest},
    values::{Batch, SchemaRef, Value},
    Error,
};
use futures::{executor::block_on, stream, StreamExt};
use planner_types::ir::materialized_view::MaterializedView;
use planner_types::ir::operator::{GroupKeys, Source};
use planner_types::ir::physical_export::{
    EdgeRole, GroupingEdgeCompatibility, PhysicalASAPDAG, PhysicalASAPDAGEdge, PhysicalASAPDAGNode,
    PhysicalASAPOperatorPayload, WindowEdgeCompatibility,
};
use planner_types::ir::properties::*;
use planner_types::ir::schema::{DataType, Field, *};
use planner_types::ir::NonASAPOp;
use planner_types::ir::Predicate;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn fixture() -> (planner_types::ir::NonASAPOp, SchemaRef, Vec<Batch>) {
    let schema =
        planner_types::ir::schema::Schema::new(vec![planner_types::ir::schema::Field::plain(
            "value",
            DataType::Int64,
            true,
        )]);
    let output = Arc::new(planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![Field {
            table: None,
            name: "value".into(),
            dtype: FieldDataType::Plain(DataType::Int64),
            nullable: true,
        }],
        time_index: None,
    });
    let scan = planner_types::ir::NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "numbers".into(),
        },
        predicates: vec![Predicate(planner_types::ir::ScalarExpr::IsNotNull(
            Box::new(planner_types::ir::ScalarExpr::Column(0)),
        ))],
        schema,
    };
    let batches = vec![
        Batch::try_new(
            output.clone(),
            vec![vec![Value::Int64(3)], vec![Value::Null]],
        )
        .unwrap(),
        Batch::try_new(
            output.clone(),
            vec![vec![Value::Int64(9)], vec![Value::Int64(2)]],
        )
        .unwrap(),
    ];
    (scan, output, batches)
}
fn plan(
    scan: planner_types::ir::NonASAPOp,
    schema: &SchemaRef,
    state: ExecutionDataState,
) -> PhysicalASAPDAG {
    let node = |id, payload| PhysicalASAPDAGNode {
        id,
        payload,
        output_state: state,
        materialization: Default::default(),
        output_schema: (**schema).clone(),
        guarantee: None,
        retention_ms: None,
    };
    let edge = |producer, consumer| PhysicalASAPDAGEdge {
        producer,
        consumer,
        role: EdgeRole::Input,
        intermediate_schema: (**schema).clone(),
        data_state: state,
        grouping: GroupingEdgeCompatibility::NotApplicable,
        window: WindowEdgeCompatibility::NotApplicable,
    };
    PhysicalASAPDAG {
        nodes: vec![
            node(
                0,
                PhysicalASAPOperatorPayload::NonASAP(
                    scan.map_children(|_| -> usize { panic!("no plan refs") }),
                ),
            ),
            node(
                1,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Sort {
                    keys: vec![planner_types::ir::SortKey {
                        expr: planner_types::ir::ScalarExpr::Column(0),
                        ascending: false,
                        nulls_first: false,
                    }],
                    partition_by: GroupKeys::by(vec![]),
                    child: 0,
                }),
            ),
            node(
                2,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Limit {
                    n: Some(2),
                    offset: 0,
                    partition_by: GroupKeys::by(vec![]),
                    child: 1,
                }),
            ),
        ],
        edges: vec![edge(0, 1), edge(1, 2)],
        roots: vec![2],
        scalar_roots: vec![],
    }
}
/// `batches`, in `schema`, as the `numbers` table.
fn numbers(schema: &SchemaRef, batches: Vec<Batch>) -> Arc<dyn DataStore> {
    Arc::new(
        InMemoryDataStore::default()
            .with_raw_data(
                Source::Table {
                    table_ref: "numbers".into(),
                },
                schema.clone(),
                batches,
            )
            .unwrap(),
    )
}
/// `dag` compiled for `roots`, its scans bound to `store`.
fn bind(
    dag: &PhysicalASAPDAG,
    roots: &[u64],
    store: Arc<dyn DataStore>,
) -> Result<PhysicalDAG<'static, Batch, SchemaRef>, Error> {
    compile(dag, &[], roots)?.instantiate(store)
}
/// The read of `view` from a store that holds no materialized view.
fn no_data(view: &MaterializedView) -> Result<OutputStream<'static, Batch>, Error> {
    Err(Error::Invalid(format!("no view {}", view.node_id)))
}
fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits::default(),
    )
    .unwrap()
}

// Raw-only execution filters nulls and ranks across batches at either phase.
#[test]
fn raw_scan_to_sort_limit_at_both_phases() {
    let (scan, schema, batches) = fixture();
    let store = numbers(&schema, batches);
    for state in [
        ExecutionDataState::QUERY_ROWS,
        ExecutionDataState::INGESTION_ROWS,
    ] {
        let dag = plan(scan.clone(), &schema, state);
        let bound = bind(&dag, &[2], store.clone()).unwrap();
        let ctx = if state == ExecutionDataState::QUERY_ROWS {
            context()
        } else {
            RunContext::new(
                Scope::Ingestion {
                    window_start_ms: 0,
                    window_end_ms: 1,
                },
                Limits::default(),
            )
            .unwrap()
        };
        let rows = block_on(async {
            let mut output = bound.execute(&[2], ctx.clone()).unwrap().remove(0);
            let mut rows = vec![];
            while let Some(batch) = output.next().await {
                rows.extend(batch.unwrap().to_rows().iter().cloned());
            }
            rows
        });
        assert!(
            matches!(rows.as_slice(), [a,b] if matches!(a.as_slice(), [Value::Int64(9)]) && matches!(b.as_slice(), [Value::Int64(3)]))
        );
        assert_eq!(ctx.retained_bytes(), 0);
    }
}
struct CountingStore {
    opened: Arc<AtomicUsize>,
    fail: bool,
}
impl DataStore for CountingStore {
    fn read_raw_data(
        &self,
        _: &Source,
        request: &RawDataRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(Error::Operator("reader failed".into()));
        }
        Ok(stream::iter(vec![Batch::try_new(
            request.schema.clone(),
            vec![vec![Value::Int64(7)]],
        )])
        .boxed_local())
    }
    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        _: &MaterializedViewRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        no_data(view)
    }
}
// Binding and cancellation do not perform I/O; fan-out opens one cursor per run.
#[test]
fn lazy_open_shared_producer_and_cancellation() {
    let (scan, schema, _) = fixture();
    let opened = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(CountingStore {
        opened: opened.clone(),
        fail: false,
    });
    let plan = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let bound = bind(&plan, &[0, 2], store).unwrap();
    let ctx = context();
    let streams = bound.execute(&[0, 2], ctx.clone()).unwrap();
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    ctx.cancel();
    drop(streams);
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    for _ in 0..2 {
        block_on(async {
            let streams = bound.execute(&[0, 2], context()).unwrap();
            let all =
                futures::future::join_all(streams.into_iter().map(|s| s.collect::<Vec<_>>())).await;
            assert!(all.iter().flatten().all(Result::is_ok));
        });
    }
    assert_eq!(opened.load(Ordering::SeqCst), 2);
}
// Unsupported predicates fail before opening any cursor; a store without
// the source and a failing reader fail the run, not as an empty result.
#[test]
fn binding_errors_and_reader_errors_are_not_empty_results() {
    let (mut scan, schema, _) = fixture();
    let unbound = bind(
        &plan(scan.clone(), &schema, ExecutionDataState::QUERY_ROWS),
        &[2],
        Arc::new(InMemoryDataStore::default()),
    )
    .unwrap();
    block_on(async {
        let mut stream = unbound.execute(&[2], context()).unwrap().remove(0);
        assert!(stream.next().await.unwrap().is_err());
    });
    let opened = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(CountingStore {
        opened: opened.clone(),
        fail: true,
    });
    if let planner_types::ir::NonASAPOp::Scan { predicates, .. } = &mut scan {
        predicates.push(Predicate(planner_types::ir::ScalarExpr::Column(0)));
    }
    assert!(bind(
        &plan(scan.clone(), &schema, ExecutionDataState::QUERY_ROWS),
        &[2],
        store.clone()
    )
    .is_err());
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    let (scan, _, _) = fixture();
    let plan = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let bound = bind(&plan, &[2], store).unwrap();
    block_on(async {
        let mut stream = bound.execute(&[2], context()).unwrap().remove(0);
        assert!(stream.next().await.unwrap().is_err());
    });
}

// SchemaRef drift cannot enter the DAG, and connector batches obey execution limits.
#[test]
fn schema_drift_and_memory_limits_fail_the_scan() {
    struct Drift {
        batch: Batch,
    }
    impl DataStore for Drift {
        fn read_raw_data(
            &self,
            _: &Source,
            _: &RawDataRequest,
            _: RunContext,
        ) -> Result<OutputStream<'_, Batch>, Error> {
            Ok(stream::once(async { Ok(self.batch.clone()) }).boxed_local())
        }
        fn read_materialized_view(
            &self,
            view: &MaterializedView,
            _: &MaterializedViewRequest,
            _: RunContext,
        ) -> Result<OutputStream<'_, Batch>, Error> {
            no_data(view)
        }
    }
    let (scan, schema, batches) = fixture();
    let mut different = (*schema).clone();
    different.fields[0].name = "wrong".into();
    let bad = Batch::try_new(Arc::new(different), vec![vec![Value::Int64(1)]]).unwrap();
    let plan = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let physical_dag = bind(&plan, &[0], Arc::new(Drift { batch: bad })).unwrap();
    block_on(async {
        let mut s = physical_dag.execute(&[0], context()).unwrap().remove(0);
        assert!(s.next().await.unwrap().is_err());
    });
    let physical_dag = bind(&plan, &[0], numbers(&schema, batches)).unwrap();
    let ctx = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits {
            max_bytes: 1,
            max_buffered_batches: 1,
        },
    )
    .unwrap();
    block_on(async {
        let mut s = physical_dag.execute(&[0], ctx.clone()).unwrap().remove(0);
        assert!(s.next().await.unwrap().is_err());
    });
    assert_eq!(ctx.retained_bytes(), 0);
}

// An empty table is a valid empty scan; nullable comparisons retain only TRUE.
#[test]
fn empty_sources_and_three_valued_predicates() {
    use planner_types::ir::scalar::{CompareOpKind, ScalarValue};

    let (mut scan, schema, batches) = fixture();
    if let planner_types::ir::NonASAPOp::Scan {
        predicates, source, ..
    } = &mut scan
    {
        *source = Source::TimeSeries {
            metric: "samples".into(),
        };
        *predicates = vec![Predicate(planner_types::ir::ScalarExpr::Compare {
            semantics: planner_types::ir::ExprSemantics::Sql,
            left: Box::new(planner_types::ir::ScalarExpr::Column(0)),
            op: CompareOpKind::Gt,
            right: Box::new(planner_types::ir::ScalarExpr::Literal(ScalarValue::Int64(
                2,
            ))),
        })];
    }
    for (batches, expected) in [(vec![], 0), (batches, 2)] {
        let store = InMemoryDataStore::default()
            .with_raw_data(
                Source::TimeSeries {
                    metric: "samples".into(),
                },
                schema.clone(),
                batches,
            )
            .unwrap();
        let plan = plan(scan.clone(), &schema, ExecutionDataState::QUERY_ROWS);
        let physical_dag = bind(&plan, &[0], Arc::new(store)).unwrap();
        block_on(async {
            let mut s = physical_dag.execute(&[0], context()).unwrap().remove(0);
            let mut count = 0;
            while let Some(b) = s.next().await {
                count += b.unwrap().to_rows().len();
            }
            assert_eq!(count, expected);
        });
    }
}

// A physical candidate can be compiled once without readers and rebound per
// run; a run over a store without its table fails.
#[test]
fn compile_without_readers_and_rebind_inputs() {
    let (scan, schema, batches) = fixture();
    let dag = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let compiled = compile(&dag, &[], &[2]).unwrap();
    // The Scan is the one (raw data) slot.
    assert_eq!(compiled.input_slots().count(), 1);
    for _ in 0..2 {
        let physical_dag = compiled
            .instantiate(numbers(&schema, batches.clone()))
            .unwrap();
        let mut outputs = physical_dag.execute(compiled.roots(), context()).unwrap();
        let result = block_on(outputs.remove(0).collect::<Vec<_>>());
        assert!(result.iter().all(Result::is_ok));
        assert_eq!(
            result
                .iter()
                .map(|b| b.as_ref().unwrap().to_rows().len())
                .sum::<usize>(),
            2
        );
    }
    let unbound = compiled
        .instantiate(Arc::new(InMemoryDataStore::default()))
        .unwrap();
    let mut outputs = unbound.execute(compiled.roots(), context()).unwrap();
    let result = block_on(outputs.remove(0).collect::<Vec<_>>());
    assert!(result.iter().any(Result::is_err));
}

// A store whose column has another type than the slot's fails the read.
#[test]
fn a_store_of_another_schema_fails_the_read() {
    let (scan, schema, _) = fixture();
    let dag = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let compiled = compile(&dag, &[], &[2]).unwrap();
    let mut other = (*schema).clone();
    other.fields[0].dtype = FieldDataType::Plain(DataType::Float64);
    let physical_dag = compiled
        .instantiate(numbers(&Arc::new(other), vec![]))
        .unwrap();
    let error = block_on(async {
        let mut output = physical_dag.execute(&[2], context()).unwrap().remove(0);
        match output.next().await.unwrap() {
            Err(error) => error,
            Ok(_) => panic!("a store of another schema was read"),
        }
    });
    assert!(error.to_string().contains("slot's schema"), "{error}");
}

// A filtering Scan reserves room for the filtered copy and the predicate's
// temporaries (masks, per-value columns): at peak, twice its input batch.
#[test]
fn a_filtering_scan_reserves_for_its_predicate_temporaries() {
    let (scan, schema, _) = fixture();
    let batch = Batch::try_new(
        schema.clone(),
        (0..10_000).map(|i| vec![Value::Int64(i)]).collect(),
    )
    .unwrap();
    let input_bytes = batch.memory_bytes();
    let plan = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let physical_dag = bind(&plan, &[0], numbers(&schema, vec![batch])).unwrap();
    let ctx = context();
    block_on(async {
        let mut s = physical_dag.execute(&[0], ctx.clone()).unwrap().remove(0);
        while let Some(batch) = s.next().await {
            batch.unwrap();
        }
    });
    assert!(
        ctx.peak_bytes() >= 2 * input_bytes,
        "peak {} for an input of {input_bytes} bytes",
        ctx.peak_bytes()
    );
}

/// The evaluation time of the windowed runs.
const T_MS: i64 = 1_000_000_000;
const MINUTE_MS: i64 = 60_000;

/// `(ts, status)` with `ts` the time index: the `requests` table the
/// windowed plans read.
fn requests_schema() -> planner_types::ir::schema::Schema {
    planner_types::ir::schema::Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("status", DataType::Int64, false),
        ],
        0,
        vec![],
    )
}

/// `Limit(Sort(TimeRange(5m, [TimeShift(shift_ms)] Scan(requests, status = 500))))`:
/// node 0 the Scan, 1 the shift (when any), 2 the range, 3 the sort, 4 the
/// limit and root.
fn windowed_plan(shift_ms: Option<i64>) -> PhysicalASAPDAG {
    use planner_types::ir::scalar::{CompareOpKind, ScalarValue};
    let schema = requests_schema();
    let state = ExecutionDataState::QUERY_ROWS;
    let node = |id, payload| PhysicalASAPDAGNode {
        id,
        payload: PhysicalASAPOperatorPayload::NonASAP(payload),
        output_state: state,
        materialization: Default::default(),
        output_schema: schema.clone(),
        guarantee: None,
        retention_ms: None,
    };
    let edge = |producer, consumer| PhysicalASAPDAGEdge {
        producer,
        consumer,
        role: EdgeRole::Input,
        intermediate_schema: schema.clone(),
        data_state: state,
        grouping: GroupingEdgeCompatibility::NotApplicable,
        window: WindowEdgeCompatibility::NotApplicable,
    };
    let status_is_500 = Predicate(planner_types::ir::ScalarExpr::Compare {
        semantics: planner_types::ir::ExprSemantics::Sql,
        left: Box::new(planner_types::ir::ScalarExpr::Column(1)),
        op: CompareOpKind::Eq,
        right: Box::new(planner_types::ir::ScalarExpr::Literal(ScalarValue::Int64(
            500,
        ))),
    });
    let mut nodes = vec![node(
        0,
        NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "requests".into(),
            },
            predicates: vec![status_is_500],
            schema: schema.clone(),
        },
    )];
    let mut edges = vec![];
    let mut below_range = 0;
    if let Some(offset_ms) = shift_ms {
        nodes.push(node(
            1,
            NonASAPOp::TimeShift {
                shift: planner_types::ir::operator::TimeShift {
                    offset_ms,
                    at: None,
                },
                child: 0,
            },
        ));
        edges.push(edge(0, 1));
        below_range = 1;
    }
    nodes.push(node(
        2,
        NonASAPOp::TimeRange {
            range: std::time::Duration::from_millis(5 * MINUTE_MS as u64),
            kind: planner_types::ir::TimeRangeKind::Range,
            child: below_range,
        },
    ));
    nodes.push(node(
        3,
        NonASAPOp::Sort {
            keys: vec![planner_types::ir::SortKey {
                expr: planner_types::ir::ScalarExpr::Column(0),
                ascending: true,
                nulls_first: false,
            }],
            partition_by: GroupKeys::by(vec![]),
            child: 2,
        },
    ));
    nodes.push(node(
        4,
        NonASAPOp::Limit {
            n: Some(100),
            offset: 0,
            partition_by: GroupKeys::by(vec![]),
            child: 3,
        },
    ));
    edges.extend([edge(below_range, 2), edge(2, 3), edge(3, 4)]);
    PhysicalASAPDAG {
        nodes,
        edges,
        roots: vec![4],
        scalar_roots: vec![],
    }
}

/// One request a minute over the 12 minutes up to `T_MS`, `status` 500 on
/// even minutes and 200 on odd ones, in `schema` (`host` filled when it has one).
fn requests(schema: &SchemaRef) -> Batch {
    let rows = (0..=12)
        .map(|minute: i64| {
            schema
                .fields
                .iter()
                .map(|field| match field.name.as_str() {
                    "ts" => Value::Timestamp(T_MS - minute * MINUTE_MS),
                    "status" => Value::Int64(if minute % 2 == 0 { 500 } else { 200 }),
                    "host" => Value::Utf8("a".into()),
                    other => panic!("column {other}"),
                })
                .collect()
        })
        .collect();
    Batch::try_new(schema.clone(), rows).unwrap()
}

/// What a store was asked: each raw data request's time range and schema,
/// and its predicates' count.
type Requests = std::sync::Mutex<Vec<(Option<(i64, i64)>, SchemaRef, usize)>>;

/// A store of the `requests` table, `batch`, that records each request and
/// returns the request's columns (by name) of the rows, only those in its
/// time range when `applies_request`.
struct RecordingStore {
    batch: Batch,
    applies_request: bool,
    requests: Arc<Requests>,
    rows_returned: Arc<AtomicUsize>,
}
impl DataStore for RecordingStore {
    fn read_raw_data(
        &self,
        _: &Source,
        request: &RawDataRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        self.requests.lock().unwrap().push((
            request.time_range_ms,
            request.schema.clone(),
            request.predicates.len(),
        ));
        let schema = self.batch.schema();
        let time = schema.time_index.unwrap();
        let columns: Vec<usize> = request
            .schema
            .fields
            .iter()
            .map(|field| {
                schema
                    .fields
                    .iter()
                    .position(|column| column.name == field.name)
                    .unwrap()
            })
            .collect();
        let time_range_ms = request.time_range_ms.filter(|_| self.applies_request);
        let rows = self
            .batch
            .to_rows()
            .into_iter()
            .filter(|row| {
                time_range_ms.is_none_or(|(start, end)| {
                    matches!(row[time], Value::Timestamp(t) if t > start && t <= end)
                })
            })
            .map(|row| columns.iter().map(|&c| row[c].clone()).collect())
            .collect();
        let batch = Batch::try_new(request.schema.clone(), rows)?;
        self.rows_returned
            .fetch_add(batch.num_rows(), Ordering::SeqCst);
        Ok(stream::once(async move { Ok(batch) }).boxed_local())
    }
    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        _: &MaterializedViewRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        no_data(view)
    }
}

/// The `(ts, status)` rows root 4 of `dag` returns at `T_MS` over `store`.
fn windowed_rows(dag: &PhysicalASAPDAG, store: Arc<dyn DataStore>) -> Vec<(i64, i64)> {
    let compiled = compile(dag, &[], &[4]).unwrap();
    let physical_dag = compiled.instantiate(store).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: T_MS,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut output = physical_dag.execute(&[4], context).unwrap().remove(0);
        let mut rows = vec![];
        while let Some(batch) = output.next().await {
            rows.extend(
                batch
                    .unwrap()
                    .to_rows()
                    .into_iter()
                    .map(|row| match row[..] {
                        [Value::Timestamp(ts), Value::Int64(status)] => (ts, status),
                        _ => panic!("row {row:?}"),
                    }),
            );
        }
        rows
    })
}

/// The `(ts, status)` of the status-500 requests of `(T - 5m, T]`, oldest first.
fn last_five_minutes_of_500s() -> Vec<(i64, i64)> {
    [4, 2, 0]
        .into_iter()
        .map(|minute| (T_MS - minute * MINUTE_MS, 500))
        .collect()
}

fn recording(
    batch: Batch,
    applies_request: bool,
) -> (Arc<RecordingStore>, Arc<Requests>, Arc<AtomicUsize>) {
    let requests = Arc::new(Requests::default());
    let rows_returned = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(RecordingStore {
        batch,
        applies_request,
        requests: requests.clone(),
        rows_returned: rows_returned.clone(),
    });
    (provider, requests, rows_returned)
}

// The Scan under `TimeRange(5m)` asks for `(T - 5m, T]`, and under a
// `TimeShift(10m)` too for `(T - 15m, T - 10m]`, with its schema and predicate.
#[test]
fn a_table_slot_requests_its_window_at_the_evaluation_time() {
    let schema = Arc::new(requests_schema());
    for (shift_ms, expected) in [
        (None, (T_MS - 5 * MINUTE_MS, T_MS)),
        (
            Some(10 * MINUTE_MS),
            (T_MS - 15 * MINUTE_MS, T_MS - 10 * MINUTE_MS),
        ),
    ] {
        let (provider, requests, _) = recording(requests(&schema), false);
        windowed_rows(&windowed_plan(shift_ms), provider);
        assert_eq!(
            *requests.lock().unwrap(),
            vec![(Some(expected), schema.clone(), 1)]
        );
    }
}

// A store that ignores the request returns rows outside the window and
// the predicate; the executor re-applies both, so the answer is unchanged.
#[test]
fn a_provider_ignoring_the_request_gives_the_same_answer() {
    let schema = Arc::new(requests_schema());
    let store = InMemoryDataStore::default()
        .with_raw_data(
            Source::Table {
                table_ref: "requests".into(),
            },
            schema.clone(),
            vec![requests(&schema)],
        )
        .unwrap();
    assert_eq!(
        windowed_rows(&windowed_plan(None), Arc::new(store)),
        last_five_minutes_of_500s()
    );
}

// A store that applies the request's time range returns fewer rows, and the
// answer is unchanged.
#[test]
fn a_provider_applying_the_request_gives_the_same_answer() {
    let schema = Arc::new(requests_schema());
    let (provider, _, rows_returned) = recording(requests(&schema), true);
    assert_eq!(
        windowed_rows(&windowed_plan(None), provider),
        last_five_minutes_of_500s()
    );
    // The 5 rows of `(T - 5m, T]`, not the table's 13.
    assert_eq!(rows_returned.load(Ordering::SeqCst), 5);
}

// A store whose table has a column the Scan does not read is asked for the
// Scan's schema, both when it ignores and when it applies the time range.
#[test]
fn a_provider_with_an_extra_column_gives_the_same_answer() {
    let wide = Arc::new(planner_types::ir::schema::Schema::with_time_index(
        vec![
            Field::plain("host", DataType::Utf8, false),
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("status", DataType::Int64, false),
        ],
        1,
        vec![],
    ));
    for applies_request in [false, true] {
        let (provider, requests, _) = recording(requests(&wide), applies_request);
        assert_eq!(
            windowed_rows(&windowed_plan(None), provider),
            last_five_minutes_of_500s()
        );
        // `ts` and `status`, not the table's `host`.
        assert_eq!(*requests.lock().unwrap()[0].1, requests_schema());
    }
}

/// `FinalizeExactAccumulator(SummaryAgg(count, TimeRange(5m, [TimeShift(shift_ms)]
/// Scan(requests, status = 500))))`: the count of 500s over the window. Node 0
/// the Scan, 1 the shift (when any), 2 the range, 3 the count (the
/// materialized view), 4 its value and root.
fn windowed_count_plan(shift_ms: Option<i64>) -> PhysicalASAPDAG {
    use planner_types::ir::operator::Reduction;
    use planner_types::ir::ASAPOp;
    let mut dag = windowed_plan(shift_ms);
    dag.nodes.retain(|node| node.id < 3);
    dag.edges.retain(|edge| edge.consumer < 3);
    let family = FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count);
    let state = planner_types::ir::schema::Schema::new(vec![Field {
        table: None,
        name: "count".into(),
        dtype: family.clone(),
        nullable: false,
    }]);
    let value = planner_types::ir::schema::Schema::new(vec![Field::plain(
        "count",
        DataType::Float64,
        false,
    )]);
    let node = |id, payload, output_schema: &planner_types::ir::schema::Schema, output_state| {
        PhysicalASAPDAGNode {
            id,
            payload,
            output_state,
            materialization: Default::default(),
            output_schema: output_schema.clone(),
            guarantee: None,
            retention_ms: None,
        }
    };
    dag.nodes.push(node(
        3,
        PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
            child: 2,
            family,
            input: SummaryUpdate {
                item: None,
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: Default::default(),
            },
            reduction: Reduction::Reduce(GroupKeys::by(vec![])),
            grouping: GroupingStrategy::default(),
            filter: None,
        }),
        &state,
        ExecutionDataState::INGESTION_SUMMARY,
    ));
    dag.nodes.push(node(
        4,
        PhysicalASAPOperatorPayload::ASAP(ASAPOp::FinalizeExactAccumulator { child: 3 }),
        &value,
        ExecutionDataState::QUERY_ROWS,
    ));
    let edge = |producer, consumer, schema: &planner_types::ir::schema::Schema, data_state| {
        PhysicalASAPDAGEdge {
            producer,
            consumer,
            role: EdgeRole::Input,
            intermediate_schema: schema.clone(),
            data_state,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        }
    };
    dag.edges.extend([
        edge(2, 3, &requests_schema(), ExecutionDataState::QUERY_ROWS),
        edge(3, 4, &state, ExecutionDataState::INGESTION_SUMMARY),
    ]);
    dag
}

/// A store of one materialized view's states, `states`, that records each
/// view request's time range.
struct RecordingViewStore {
    states: Batch,
    time_ranges_ms: std::sync::Mutex<Vec<(i64, i64)>>,
}
impl DataStore for RecordingViewStore {
    fn read_raw_data(
        &self,
        source: &Source,
        _: &RawDataRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        Err(Error::Invalid(format!("no raw data {source:?}")))
    }
    fn read_materialized_view(
        &self,
        _: &MaterializedView,
        request: &MaterializedViewRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        self.time_ranges_ms
            .lock()
            .unwrap()
            .push(request.time_range_ms);
        assert_eq!(request.schema, self.states.schema());
        let states = self.states.clone();
        Ok(stream::once(async move { Ok(states) }).boxed_local())
    }
}

// A materialized view slot (the count over the range, as an expanded pane
// over its range) is asked for its window, `(T - 5m, T]` or
// `(T - 15m, T - 10m]` under a 10-minute shift, and its state is the
// answer's.
#[test]
fn a_materialized_view_provider_is_asked_for_the_window_it_covers() {
    use asap_executor::physical_planner::test_support::views_at;
    use asap_executor::summary_kernels::exact::ExactAccumulator;
    for (shift_ms, expected) in [
        (None, (T_MS - 5 * MINUTE_MS, T_MS)),
        (
            Some(10 * MINUTE_MS),
            (T_MS - 15 * MINUTE_MS, T_MS - 10 * MINUTE_MS),
        ),
    ] {
        let mut dag = windowed_count_plan(shift_ms);
        let views = views_at(&mut dag, &[3]).unwrap();
        let family = FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count);
        let mut count = ExactAccumulator::new(family.clone(), false).unwrap();
        for _ in last_five_minutes_of_500s() {
            count.update(None, 1.0, 0);
        }
        let states = Batch::try_new(
            Arc::new(
                dag.nodes
                    .iter()
                    .find(|node| node.id == 3)
                    .unwrap()
                    .output_schema
                    .clone(),
            ),
            vec![vec![Value::Summary {
                family,
                state: Arc::new(count),
            }]],
        )
        .unwrap();
        let store = Arc::new(RecordingViewStore {
            states,
            time_ranges_ms: Default::default(),
        });
        let compiled = compile(&dag, &views, &[4]).unwrap();
        let physical_dag = compiled.instantiate(store.clone()).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: T_MS,
            },
            Limits::default(),
        )
        .unwrap();
        let rows = block_on(async {
            let mut output = physical_dag.execute(&[4], context).unwrap().remove(0);
            let mut rows = vec![];
            while let Some(batch) = output.next().await {
                rows.extend(batch.unwrap().to_rows());
            }
            rows
        });
        assert!(
            matches!(rows.as_slice(), [row] if matches!(row.as_slice(), [Value::Float64(c)] if *c == 3.0)),
            "{rows:?}"
        );
        assert_eq!(*store.time_ranges_ms.lock().unwrap(), vec![expected]);
    }
}
