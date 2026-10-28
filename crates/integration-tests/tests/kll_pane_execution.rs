//! Maintenance -> stored pane state -> independently bound query execution.
mod physical_common;
use asap_executor::sources::InMemoryDataStore;
use asap_executor::{
    operators::{Operator, SummaryEvaluation},
    physical_planner::{compile, test_support::views_at, CompiledPhysicalDAG},
    plan::{PhysicalDAG, PhysicalOperator, PlanProperties},
    runtime::{Input, Limits, OutputStream, RunContext, Scope},
    summary_kernels::datasketches_kll::DatasketchesKLLAccumulator,
    values::{Batch, SchemaRef, Value},
    AggregateCore, Error,
};
use asap_types::ir::materialized_view::MaterializedView;
use asap_types::ir::operator::operator_properties::TimeShift;
use asap_types::ir::operator::{GroupKeys, Reduction, Source};
use asap_types::ir::physical_export::{
    compile_physical_asap_workload_with_node_ids, PhysicalASAPDAG,
};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchKind,
    SketchParams, SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{
    apply_materialization_timings_to_roots, ASAPOp, MaterializationAssignment, NonASAPOp,
    OperatorNode, QueryRoot, TimeRangeKind,
};
use futures::{executor::block_on, StreamExt};
use std::{
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const PANE_MS: i64 = 60_000;

fn family(k: u32) -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k,
                failure_probability: None,
            },
        ),
        Default::default(),
    )
}
/// `t(ts, value)`, time index `ts`.
fn table_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ],
        0,
        vec![],
    )
}
fn query_scope() -> Scope {
    Scope::Query {
        evaluation_time_ms: 300_000,
    }
}

/// p50 and p99 of one KLL (k = 200) merged from five one-minute panes of
/// `t`, each pane a materialized view.
struct Fixture {
    dag: PhysicalASAPDAG,
    /// Pane `i` reads the minute ending `i` minutes before the evaluation time.
    panes: Vec<u64>,
    views: Vec<MaterializedView>,
    merge: u64,
    p50: u64,
    p99: u64,
}
fn fixture() -> Fixture {
    let new = |operator| OperatorNode::new_shared(operator).unwrap();
    let scan = new(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: table_schema(),
    }));
    let panes: Vec<Rc<OperatorNode>> = (0..5)
        .map(|i| {
            let shifted = new(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeShift {
                shift: TimeShift {
                    offset_ms: i * PANE_MS,
                    at: None,
                },
                child: scan.clone(),
            }));
            let range = new(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                range: std::time::Duration::from_millis(PANE_MS as u64),
                kind: TimeRangeKind::Range,
                child: shifted,
            }));
            new(asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                child: range,
                family: family(200),
                input: SummaryUpdate::column(ColumnRef::Named("value".into())),
                reduction: Reduction::Reduce(GroupKeys::default()),
                grouping: GroupingStrategy::PerSubpopulationInstance,
                filter: None,
            }))
        })
        .collect();
    let merge = new(asap_types::ir::Operator::ASAP(ASAPOp::SummaryMerge {
        children: panes.clone(),
        newest_panes: None,
        group_by: None,
    }));
    let estimate = |q| {
        new(asap_types::ir::Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: merge.clone(),
            query: SketchStatistic::Quantile { q },
        }))
    };
    let roots = apply_materialization_timings_to_roots(
        &[
            QueryRoot::Operator(estimate(0.5)),
            QueryRoot::Operator(estimate(0.99)),
        ],
        &MaterializationAssignment::default(),
    )
    .unwrap();
    let compiled = compile_physical_asap_workload_with_node_ids(&roots).unwrap();
    let mut pane_ids = vec![0; 5];
    let (mut merge, mut p50, mut p99) = (0, 0, 0);
    for node in &compiled.dag.nodes {
        let id = node.id as u64;
        match &compiled.node_ids.operator_node(node.id).unwrap().operator {
            asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) => {
                let shift = match child.children()[0].non_asap() {
                    Some(NonASAPOp::TimeShift { shift, .. }) => shift.offset_ms,
                    _ => unreachable!("a pane reads a shifted range"),
                };
                pane_ids[(shift / PANE_MS) as usize] = id;
            }
            asap_types::ir::Operator::ASAP(ASAPOp::SummaryMerge { .. }) => merge = id,
            asap_types::ir::Operator::ASAP(ASAPOp::SummaryEstimate {
                query: SketchStatistic::Quantile { q },
                ..
            }) => match q {
                0.5 => p50 = id,
                _ => p99 = id,
            },
            _ => {}
        }
    }
    let mut dag = compiled.dag;
    let views = views_at(&mut dag, &pane_ids).unwrap();
    Fixture {
        dag,
        panes: pane_ids,
        views,
        merge,
        p50,
        p99,
    }
}
fn schema_of(dag: &PhysicalASAPDAG, id: u64) -> SchemaRef {
    Arc::new(
        dag.nodes
            .iter()
            .find(|node| node.id as u64 == id)
            .unwrap()
            .output_schema
            .clone(),
    )
}
/// Run `plan` against `store`; the first error of any root, if one fails.
fn try_execute(
    plan: &CompiledPhysicalDAG,
    store: InMemoryDataStore,
    scope: Scope,
) -> Result<Vec<Vec<Batch>>, Error> {
    let dag = plan.instantiate(Arc::new(store))?;
    block_on(async {
        let streams = dag.execute(plan.roots(), RunContext::new(scope, Limits::default())?)?;
        let mut outputs = Vec::new();
        for mut stream in streams {
            let mut batches = Vec::new();
            while let Some(batch) = stream.next().await {
                batches.push((*batch?).clone());
            }
            outputs.push(batches);
        }
        Ok(outputs)
    })
}
fn summary(batch: &Batch) -> Arc<dyn AggregateCore> {
    batch.to_rows()[0]
        .iter()
        .find_map(|value| match value {
            Value::Summary { state, .. } => Some(state.clone()),
            _ => None,
        })
        .expect("missing KLL")
}
/// Pane `pane`'s state, built by `maintenance` (the newest pane's build) in
/// the ingestion run of `(pane, pane + 1]` minutes.
fn pane_state(maintenance: &CompiledPhysicalDAG, pane: i64) -> Batch {
    // Twenty samples in each (start,end] one-minute pane; k=200 avoids
    // compaction so quantiles and sample counts have deterministic oracles.
    let schema = Arc::new(table_schema());
    let rows = (0..20)
        .map(|i| {
            vec![
                Value::Timestamp(pane * PANE_MS + 1 + i * 1000),
                Value::Float64((pane * 20 + i) as f64),
            ]
        })
        .collect();
    let store = InMemoryDataStore::default()
        .with_raw_data(
            Source::Table {
                table_ref: "t".into(),
            },
            schema.clone(),
            vec![Batch::try_new(schema, rows).unwrap()],
        )
        .unwrap();
    let state = physical_common::execute(
        maintenance,
        store,
        Scope::Ingestion {
            window_start_ms: pane * PANE_MS,
            window_end_ms: (pane + 1) * PANE_MS,
        },
    );
    state[0][0].clone()
}
fn restore(schema: SchemaRef, states: &[Arc<dyn AggregateCore>]) -> Batch {
    Batch::try_new(
        schema,
        states
            .iter()
            .map(|state| {
                vec![Value::Summary {
                    family: family(200),
                    state: state.clone(),
                }]
            })
            .collect(),
    )
    .unwrap()
}
fn evaluation(schema: SchemaRef, q: f64) -> Operator {
    Operator::evaluation(
        schema,
        0,
        SummaryEvaluation::Sketch(SketchStatistic::Quantile { q }),
    )
    .unwrap()
}
struct CountStarts {
    operator: Operator,
    starts: Arc<AtomicUsize>,
}
impl PhysicalOperator<Batch, SchemaRef> for CountStarts {
    fn name(&self) -> &str {
        self.operator.name()
    }
    fn properties(&self, inputs: &[PlanProperties]) -> PlanProperties {
        self.operator.properties(inputs)
    }
    fn requires_bounded_input(&self) -> bool {
        self.operator.requires_bounded_input()
    }
    fn input_schemas(&self) -> Vec<SchemaRef> {
        self.operator.input_schemas()
    }
    fn output_schema(&self) -> SchemaRef {
        self.operator.output_schema()
    }
    fn output_bytes(&self, batch: &Batch) -> usize {
        self.operator.output_bytes(batch)
    }
    fn start<'a>(
        &'a self,
        inputs: Vec<Input<'a, Batch>>,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.operator.start(inputs, context)
    }
}

/// Actual codec bytes survive destruction of maintenance state; one shared
/// native merge supplies p50, p99 and the population-count oracle per run.
#[test]
fn five_panes_roundtrip_and_shared_merge_runs_once() {
    let fixture = fixture();
    let maintenance = compile(&fixture.dag, &[], &[fixture.panes[0]]).unwrap();
    let panes: Vec<Batch> = (0..6).map(|pane| pane_state(&maintenance, pane)).collect();
    drop(maintenance);
    let schema = schema_of(&fixture.dag, fixture.panes[0]);
    let merge = Operator::summary_merge(schema.clone(), 0, vec![]).unwrap();
    let compiled = compile(
        &fixture.dag,
        &fixture.views,
        &[fixture.merge, fixture.p50, fixture.p99],
    )
    .unwrap();
    let layout = asap_types::physical::PaneLayout {
        pane_width_ms: 60_000,
        pane_origin_ms: Some(0),
    };
    assert!(
        asap_types::physical::validate_pane_coverage(
            &layout,
            Some(330_000),
            &asap_types::physical::WindowEdgeCoverage::PaneAligned
        )
        .is_err(),
        "moving window edges require residual computation"
    );
    for offset in [0, 1] {
        let restored = restore(
            schema.clone(),
            &panes[offset..offset + 5]
                .iter()
                .map(summary)
                .collect::<Vec<_>>(),
        );
        let evaluation_time_ms = (5 + offset as i64) * 60_000;
        asap_types::physical::validate_pane_coverage(
            &layout,
            Some(evaluation_time_ms),
            &asap_types::physical::WindowEdgeCoverage::PaneAligned,
        )
        .unwrap();
        // View `i` holds the pane ending `i` minutes before the evaluation time.
        let bind = |count: usize| {
            fixture.panes[..count].iter().enumerate().fold(
                InMemoryDataStore::default(),
                |store, (i, &view)| {
                    store
                        .with_materialized_view(view as usize, vec![panes[offset + 4 - i].clone()])
                        .unwrap()
                },
            )
        };
        let scope = Scope::Query { evaluation_time_ms };
        // A five-pane deployment cannot bind only four materialized views.
        let missing = try_execute(&compiled, bind(4), scope.clone())
            .err()
            .unwrap();
        assert!(
            missing.to_string().contains("unbound materialized view"),
            "{missing}"
        );
        let result = physical_common::execute(&compiled, bind(5), scope);
        let kll_state = summary(&result[0][0]);
        let kll = kll_state
            .as_any()
            .downcast_ref::<DatasketchesKLLAccumulator>()
            .unwrap();
        assert_eq!(kll.inner.count(), 100);
        let value = |index: usize| {
            result[index][0].to_rows()[0]
                .iter()
                .find_map(|value| match value {
                    Value::Float64(value) => Some(*value),
                    _ => None,
                })
                .expect("missing quantile")
        };
        assert!((value(1) - (50 + offset * 20) as f64).abs() <= 1.);
        assert!((value(2) - (99 + offset * 20) as f64).abs() <= 1.);
        let starts = Arc::new(AtomicUsize::new(0));
        let mut dag = PhysicalDAG::default();
        dag.add(
            0,
            vec![],
            Operator::source(schema.clone(), vec![restored]).unwrap(),
        )
        .unwrap();
        dag.add(
            1,
            vec![0],
            CountStarts {
                operator: merge.clone(),
                starts: starts.clone(),
            },
        )
        .unwrap();
        dag.add(2, vec![1], evaluation(merge.output_schema(), 0.5))
            .unwrap();
        dag.add(3, vec![1], evaluation(merge.output_schema(), 0.99))
            .unwrap();
        let outputs = block_on(futures::future::join_all(
            dag.execute(
                &[2, 3],
                RunContext::new(query_scope(), Limits::default()).unwrap(),
            )
            .unwrap()
            .into_iter()
            .map(|stream| stream.collect::<Vec<_>>()),
        ));
        assert!(outputs
            .iter()
            .all(|output| output.len() == 1 && output[0].is_ok()));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }
}

/// Relabelled KLL parameters, a view bound in another schema and a missing
/// view fail explicitly.
#[test]
fn panes_reject_parameters_schema_and_missing_binding() {
    let fixture = fixture();
    let schema = schema_of(&fixture.dag, fixture.panes[0]);
    let wrong = Value::Summary {
        family: family(200),
        state: Arc::new(DatasketchesKLLAccumulator::new(128)),
    };
    assert!(Batch::try_new(schema, vec![vec![wrong]]).is_err());
    let compiled = compile(&fixture.dag, &fixture.views, &[fixture.merge]).unwrap();
    let missing = try_execute(&compiled, InMemoryDataStore::default(), query_scope())
        .err()
        .unwrap();
    assert!(
        missing.to_string().contains("unbound materialized view"),
        "{missing}"
    );
    let raw = Arc::new(table_schema());
    let raw_rows =
        Batch::try_new(raw, vec![vec![Value::Timestamp(1), Value::Float64(1.)]]).unwrap();
    let store = fixture
        .panes
        .iter()
        .fold(InMemoryDataStore::default(), |store, &view| {
            store
                .with_materialized_view(view as usize, vec![raw_rows.clone()])
                .unwrap()
        });
    let relabelled = try_execute(&compiled, store, query_scope()).err().unwrap();
    assert!(
        relabelled.to_string().contains("another schema"),
        "{relabelled}"
    );
}

/// Finds a view's states only by its definition id and the requested time
/// range, as a deployment's summary store does; never by node id.
struct DefinitionStore {
    states: std::collections::BTreeMap<(String, (i64, i64)), Batch>,
    requested: std::sync::Mutex<Vec<(String, (i64, i64))>>,
}
impl asap_executor::sources::DataStore for DefinitionStore {
    fn read_raw_data(
        &self,
        source: &Source,
        _: &asap_executor::sources::RawDataRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        Err(Error::Invalid(format!("no raw data: {source:?}")))
    }
    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        request: &asap_executor::sources::MaterializedViewRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        let key = (view.coverage.definition_id.clone(), request.time_range_ms);
        self.requested.lock().unwrap().push(key.clone());
        let state = self
            .states
            .get(&key)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("no state for {key:?}")))?;
        Ok(futures::stream::iter([Ok(state)]).boxed_local())
    }
}

/// A store keyed only by `(definition_id, time range)` answers the five-pane
/// query: the five panes share one definition and request the five
/// one-minute ranges that tile the window, and the answers equal those of
/// binding each pane's state to its node.
#[test]
fn a_store_keyed_by_definition_and_time_range_answers_the_panes() {
    let fixture = fixture();
    let maintenance = compile(&fixture.dag, &[], &[fixture.panes[0]]).unwrap();
    // The state of the pane of minute `m`, `(m, m + 1]` minutes.
    let panes: Vec<Batch> = (0..5).map(|m| pane_state(&maintenance, m)).collect();
    let definition = fixture.views[0].coverage.definition_id.clone();
    assert!(fixture
        .views
        .iter()
        .all(|view| view.coverage.definition_id == definition));
    let states = (0..5)
        .map(|m| {
            (
                (definition.clone(), (m * PANE_MS, (m + 1) * PANE_MS)),
                panes[m as usize].clone(),
            )
        })
        .collect();
    let store = Arc::new(DefinitionStore {
        states,
        requested: Default::default(),
    });
    let compiled = compile(&fixture.dag, &fixture.views, &[fixture.p50, fixture.p99]).unwrap();
    let scope = query_scope();
    let by_definition = block_on(async {
        let dag = compiled.instantiate(store.clone()).unwrap();
        let streams = dag
            .execute(
                compiled.roots(),
                RunContext::new(scope.clone(), Limits::default()).unwrap(),
            )
            .unwrap();
        let mut outputs = Vec::new();
        for mut stream in streams {
            let mut rows = Vec::new();
            while let Some(batch) = stream.next().await {
                rows.extend(batch.unwrap().to_rows());
            }
            outputs.push(rows);
        }
        outputs
    });
    let mut requested: Vec<(i64, i64)> = store
        .requested
        .lock()
        .unwrap()
        .iter()
        .map(|(_, range)| *range)
        .collect();
    requested.sort_unstable();
    assert_eq!(
        requested,
        (0..5)
            .map(|m| (m * PANE_MS, (m + 1) * PANE_MS))
            .collect::<Vec<_>>()
    );
    // View `i` holds the pane ending `i` minutes before the evaluation time.
    let by_node =
        fixture
            .panes
            .iter()
            .enumerate()
            .fold(InMemoryDataStore::default(), |store, (i, &view)| {
                store
                    .with_materialized_view(view as usize, vec![panes[4 - i].clone()])
                    .unwrap()
            });
    let by_node: Vec<Vec<Vec<Value>>> = physical_common::execute(&compiled, by_node, scope)
        .iter()
        .map(|batches| batches.iter().flat_map(Batch::to_rows).collect())
        .collect();
    assert_eq!(format!("{by_definition:?}"), format!("{by_node:?}"));
}
