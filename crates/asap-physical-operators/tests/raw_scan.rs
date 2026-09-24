//! Scan acceptance uses the public connector contract and Planner physical DAGs.
use asap_physical_operators::dag::{
    planner::bind_with_data_sources,
    scan::{DataSources, MemorySource, RawSource},
    values::{Batch, Schema, Value},
    Error, Limits, OutputStream, RunContext, Scope,
};
use futures::{executor::block_on, stream, StreamExt};
use planner_types::{
    post_asap::*,
    pre_asap::{Column, DataType, GroupKeys, Predicate, QueryExpr, Source},
};
use std::{
    collections::BTreeMap,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

fn fixture() -> (QueryExpr, Schema, Vec<Batch>) {
    let schema =
        planner_types::pre_asap::Schema::new(vec![Column::new("value", DataType::Int64, true)]);
    let output = Arc::new(SummarySchema {
        fields: vec![SummaryField {
            name: "value".into(),
            dtype: SummaryFamilyType::Plain(DataType::Int64),
            nullable: true,
        }],
        time_index: None,
    });
    let scan = QueryExpr::Scan {
        source: Source::Table {
            table_ref: "numbers".into(),
        },
        predicates: vec![Predicate(Rc::new(QueryExpr::IsNotNull(Rc::new(
            QueryExpr::Column(0),
        ))))],
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
fn plan(scan: QueryExpr, schema: &Schema, state: ExecutionDataState) -> ExecutableDag {
    let node = |id, payload| ExecutableDagNode {
        id: PostAsapNodeId(id),
        payload,
        output_state: state,
        output_schema: (**schema).clone(),
        guarantee: None,
    };
    let edge = |producer, consumer| ExecutableDagEdge {
        producer: PostAsapNodeId(producer),
        consumer: PostAsapNodeId(consumer),
        role: EdgeRole::Input,
        intermediate_schema: (**schema).clone(),
        data_state: state,
        grouping: GroupingEdgeCompatibility::NotApplicable,
        window: WindowEdgeCompatibility::NotApplicable,
    };
    ExecutableDag {
        nodes: vec![
            node(0, ExecutableOperatorPayload::Fallback { expression: scan }),
            node(
                1,
                ExecutableOperatorPayload::Value {
                    operation: ValueOperation::Sort {
                        keys: vec![planner_types::pre_asap::SortKey {
                            expr: QueryExpr::Column(0),
                            ascending: false,
                            nulls_first: false,
                        }],
                        partition_by: GroupKeys::by(vec![]),
                    },
                },
            ),
            node(
                2,
                ExecutableOperatorPayload::Value {
                    operation: ValueOperation::Limit {
                        n: 2,
                        offset: 0,
                        partition_by: GroupKeys::by(vec![]),
                    },
                },
            ),
        ],
        edges: vec![edge(0, 1), edge(1, 2)],
        root: PostAsapNodeId(2),
    }
}
fn registry(source: Arc<dyn RawSource>) -> DataSources {
    let mut r = DataSources::default();
    r.register(
        Source::Table {
            table_ref: "numbers".into(),
        },
        source,
    )
    .unwrap();
    r
}
fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap()
}

// Raw-only execution filters nulls and ranks across batches at either phase.
#[test]
fn raw_scan_to_sort_limit_at_both_phases() {
    let (scan, schema, batches) = fixture();
    let sources = registry(Arc::new(
        MemorySource::new(schema.clone(), batches).unwrap(),
    ));
    for state in [
        ExecutionDataState::QUERY_ROWS,
        ExecutionDataState::INGESTION_ROWS,
    ] {
        let dag = plan(scan.clone(), &schema, state);
        let bound = bind_with_data_sources(&dag, BTreeMap::new(), &[2], &sources).unwrap();
        let ctx = if state == ExecutionDataState::QUERY_ROWS {
            context()
        } else {
            RunContext::new(
                Scope::Ingestion {
                    window_start_ms: 0,
                    window_end_ms: 1,
                    revision: 1,
                },
                Limits::default(),
            )
            .unwrap()
        };
        let rows = block_on(async {
            let mut output = bound.execute(&[2], ctx.clone()).unwrap().remove(0);
            let mut rows = vec![];
            while let Some(batch) = output.next().await {
                rows.extend(batch.unwrap().rows().iter().cloned());
            }
            rows
        });
        assert!(
            matches!(rows.as_slice(), [a,b] if matches!(a.as_slice(), [Value::Int64(9)]) && matches!(b.as_slice(), [Value::Int64(3)]))
        );
        assert_eq!(ctx.retained_bytes(), 0);
    }
}
struct CountingSource {
    schema: Schema,
    opened: Arc<AtomicUsize>,
    fail: bool,
}
impl RawSource for CountingSource {
    fn schema(&self) -> Schema {
        self.schema.clone()
    }
    fn scan(&self, _: RunContext) -> Result<OutputStream<'_, Batch>, Error> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(Error::Operator("reader failed".into()));
        }
        Ok(stream::iter(vec![Batch::try_new(
            self.schema.clone(),
            vec![vec![Value::Int64(7)]],
        )])
        .boxed_local())
    }
}
// Binding and cancellation do not perform I/O; fan-out opens one cursor per run.
#[test]
fn lazy_open_shared_producer_and_cancellation() {
    let (scan, schema, _) = fixture();
    let opened = Arc::new(AtomicUsize::new(0));
    let sources = registry(Arc::new(CountingSource {
        schema: schema.clone(),
        opened: opened.clone(),
        fail: false,
    }));
    let plan = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let bound = bind_with_data_sources(&plan, BTreeMap::new(), &[0, 2], &sources).unwrap();
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
// Unavailable sources and unsupported predicates fail before opening any cursor.
#[test]
fn binding_errors_and_reader_errors_are_not_empty_results() {
    let (mut scan, schema, _) = fixture();
    assert!(DataSources::default().bind(&scan).is_err());
    let opened = Arc::new(AtomicUsize::new(0));
    let sources = registry(Arc::new(CountingSource {
        schema: schema.clone(),
        opened: opened.clone(),
        fail: true,
    }));
    if let QueryExpr::Scan { predicates, .. } = &mut scan {
        predicates.push(Predicate(Rc::new(QueryExpr::Column(0))));
    }
    assert!(sources.bind(&scan).is_err());
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    let (scan, _, _) = fixture();
    let plan = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let bound = bind_with_data_sources(&plan, BTreeMap::new(), &[2], &sources).unwrap();
    block_on(async {
        let mut stream = bound.execute(&[2], context()).unwrap().remove(0);
        assert!(stream.next().await.unwrap().is_err());
    });
}

// Schema drift cannot enter the DAG, and connector batches obey execution limits.
#[test]
fn schema_drift_and_memory_limits_fail_the_scan() {
    struct Drift {
        expected: Schema,
        batch: Batch,
    }
    impl RawSource for Drift {
        fn schema(&self) -> Schema {
            self.expected.clone()
        }
        fn scan(&self, _: RunContext) -> Result<OutputStream<'_, Batch>, Error> {
            Ok(stream::once(async { Ok(self.batch.clone()) }).boxed_local())
        }
    }
    let (scan, schema, batches) = fixture();
    let mut different = (*schema).clone();
    different.fields[0].name = "wrong".into();
    let bad = Batch::try_new(Arc::new(different), vec![vec![Value::Int64(1)]]).unwrap();
    let sources = registry(Arc::new(Drift {
        expected: schema.clone(),
        batch: bad,
    }));
    let plan = plan(scan, &schema, ExecutionDataState::QUERY_ROWS);
    let graph = bind_with_data_sources(&plan, BTreeMap::new(), &[0], &sources).unwrap();
    block_on(async {
        let mut s = graph.execute(&[0], context()).unwrap().remove(0);
        assert!(s.next().await.unwrap().is_err());
    });
    let sources = registry(Arc::new(MemorySource::new(schema, batches).unwrap()));
    let graph = bind_with_data_sources(&plan, BTreeMap::new(), &[0], &sources).unwrap();
    let ctx = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits {
            max_bytes: 1,
            max_buffered_batches: 1,
        },
    )
    .unwrap();
    block_on(async {
        let mut s = graph.execute(&[0], ctx.clone()).unwrap().remove(0);
        assert!(s.next().await.unwrap().is_err());
    });
    assert_eq!(ctx.retained_bytes(), 0);
}

// An empty table is a valid empty scan; nullable comparisons retain only TRUE.
#[test]
fn empty_sources_and_three_valued_predicates() {
    use planner_types::pre_asap::{CompareOpKind, ScalarValue};
    let (mut scan, schema, batches) = fixture();
    if let QueryExpr::Scan {
        predicates, source, ..
    } = &mut scan
    {
        *source = Source::TimeSeries {
            metric: "samples".into(),
        };
        *predicates = vec![Predicate(Rc::new(QueryExpr::Compare {
            left: Rc::new(QueryExpr::Column(0)),
            op: CompareOpKind::Gt,
            right: Rc::new(QueryExpr::Literal(ScalarValue::Int64(2))),
        }))];
    }
    for (batches, expected) in [(vec![], 0), (batches, 2)] {
        let mut sources = DataSources::default();
        sources
            .register(
                Source::TimeSeries {
                    metric: "samples".into(),
                },
                Arc::new(MemorySource::new(schema.clone(), batches).unwrap()),
            )
            .unwrap();
        let plan = plan(scan.clone(), &schema, ExecutionDataState::QUERY_ROWS);
        let graph = bind_with_data_sources(&plan, BTreeMap::new(), &[0], &sources).unwrap();
        block_on(async {
            let mut s = graph.execute(&[0], context()).unwrap().remove(0);
            let mut count = 0;
            while let Some(b) = s.next().await {
                count += b.unwrap().rows().len();
            }
            assert_eq!(count, expected);
        });
    }
}
