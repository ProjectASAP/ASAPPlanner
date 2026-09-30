//! Maintenance -> stored pane state -> independently bound query execution.
mod physical_common;
use asap_physical_operators::{
    operators::{Operator, ReadoutQuery},
    physical_planner::{InputContract, PhysicalPostASAPDAG, Source},
    plan::{PhysicalExecution, PhysicalOperator, PlanProperties},
    runtime::{Input, Limits, OutputStream, RunContext, Scope},
    summary_kernels::datasketches_kll::DatasketchesKLLAccumulator,
    values::{Batch, Schema, Value},
    AggregateCore, Error,
};
use asap_types::{
    post_asap::{
        SketchAlgorithm, SketchKind, SketchParams, SketchQuery, SummaryFamilyType, SummaryField,
        SummarySchema,
    },
    pre_asap::DataType,
};
use futures::{executor::block_on, StreamExt};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

fn family(k: u32) -> SummaryFamilyType {
    SummaryFamilyType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k }),
        Default::default(),
    )
}
fn raw_schema() -> Schema {
    Arc::new(SummarySchema {
        fields: vec![SummaryField {
            name: "value".into(),
            dtype: SummaryFamilyType::Plain(DataType::Float64),
            nullable: false,
        }],
        time_index: None,
    })
}
fn query_scope() -> Scope {
    Scope::Query {
        evaluation_time_ms: 300_000,
        revision: 1,
    }
}
fn fixture() -> (PhysicalPostASAPDAG, Operator, Schema) {
    let raw = raw_schema();
    let build = Operator::summary_build(raw.clone(), family(200), 0, None, vec![]).unwrap();
    let state = build.schema();
    let maintenance = PhysicalPostASAPDAG::from_operators(
        BTreeMap::from([(0, InputContract::bounded(raw))]),
        BTreeMap::from([(1, (vec![0], build))]),
        vec![1],
    )
    .unwrap();
    let merge = Operator::summary_merge(state.clone(), 0, vec![]).unwrap();
    (maintenance, merge, state)
}
fn pane_state(maintenance: &PhysicalPostASAPDAG, pane: i64) -> Arc<dyn AggregateCore> {
    // Twenty samples in each (start,end] one-minute pane; k=200 avoids
    // compaction so quantiles and sample counts have deterministic oracles.
    let raw = raw_schema();
    let rows = (0..20)
        .map(|i| vec![Value::Float64((pane * 20 + i) as f64)])
        .collect();
    let state = physical_common::execute(
        maintenance,
        BTreeMap::from([(0, Batch::try_new(raw, rows).unwrap())]),
        Scope::Ingestion {
            window_start_ms: pane * 60_000,
            window_end_ms: (pane + 1) * 60_000,
            revision: 1,
        },
    );
    let Value::Summary { state, .. } = &state[0][0].rows()[0][0] else {
        panic!("missing KLL")
    };
    state.clone()
}
fn restore(schema: Schema, states: &[Arc<dyn AggregateCore>]) -> Batch {
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
fn readout(schema: Schema, q: f64) -> Operator {
    Operator::readout(schema, 0, ReadoutQuery::Sketch(SketchQuery::Quantile { q })).unwrap()
}
struct CountStarts {
    operator: Operator,
    starts: Arc<AtomicUsize>,
}
impl PhysicalOperator<Batch, Schema> for CountStarts {
    fn name(&self) -> &str {
        self.operator.name()
    }
    fn properties(&self, inputs: &[PlanProperties]) -> PlanProperties {
        self.operator.properties(inputs)
    }
    fn requires_bounded_input(&self) -> bool {
        self.operator.requires_bounded_input()
    }
    fn input_schemas(&self) -> Vec<Schema> {
        self.operator.input_schemas()
    }
    fn output_schema(&self) -> Schema {
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
    let (maintenance, merge, schema) = fixture();
    let panes: Vec<_> = (0..6).map(|pane| pane_state(&maintenance, pane)).collect();
    drop(maintenance);
    let compiled = PhysicalPostASAPDAG::from_operators(
        (0..5)
            .map(|id| (id, InputContract::bounded(schema.clone())))
            .collect(),
        BTreeMap::from([
            (
                5,
                (
                    vec![0, 1, 2, 3, 4],
                    Operator::union(schema.clone(), 5).unwrap(),
                ),
            ),
            (6, (vec![5], merge.clone())),
            (7, (vec![6], readout(schema.clone(), 0.5))),
            (8, (vec![6], readout(schema.clone(), 0.99))),
        ]),
        vec![6, 7, 8],
    )
    .unwrap();
    let layout = asap_types::post_asap::PaneLayout {
        pane_width_ms: 60_000,
        pane_origin_ms: Some(0),
    };
    assert!(
        asap_types::post_asap::validate_pane_coverage(
            &layout,
            Some(330_000),
            &asap_types::post_asap::WindowEdgeCoverage::PaneAligned
        )
        .is_err(),
        "moving window edges require residual computation"
    );
    for offset in [0, 1] {
        let restored = restore(schema.clone(), &panes[offset..offset + 5]);
        let evaluation_time_ms = (5 + offset as i64) * 60_000;
        asap_types::post_asap::validate_pane_coverage(
            &layout,
            Some(evaluation_time_ms),
            &asap_types::post_asap::WindowEdgeCoverage::PaneAligned,
        )
        .unwrap();
        let inputs: BTreeMap<_, _> = (0..5)
            .map(|id| {
                (
                    id as u64,
                    restore(schema.clone(), &panes[offset + id..offset + id + 1]),
                )
            })
            .collect();
        // A five-pane deployment cannot bind only four state slots.
        let incomplete: BTreeMap<_, _> = inputs
            .iter()
            .take(4)
            .map(|(&id, batch)| {
                (
                    id,
                    Box::new(Operator::source(schema.clone(), vec![batch.clone()]).unwrap())
                        as Source<'_>,
                )
            })
            .collect();
        assert!(compiled.instantiate(incomplete).is_err());
        let result = physical_common::execute(
            &compiled,
            inputs,
            Scope::Query {
                evaluation_time_ms,
                revision: 1,
            },
        );
        let Value::Summary { state, .. } = &result[0][0].rows()[0][0] else {
            panic!("missing merged state")
        };
        let kll = state
            .as_any()
            .downcast_ref::<DatasketchesKLLAccumulator>()
            .unwrap();
        assert_eq!(kll.inner.count(), 100);
        let value = |index: usize| match result[index][0].rows()[0][0] {
            Value::Float64(value) => value,
            _ => panic!("missing quantile"),
        };
        assert!((value(1) - (50 + offset * 20) as f64).abs() <= 1.);
        assert!((value(2) - (99 + offset * 20) as f64).abs() <= 1.);
        let starts = Arc::new(AtomicUsize::new(0));
        let mut dag = PhysicalExecution::default();
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
        dag.add(2, vec![1], readout(schema.clone(), 0.5)).unwrap();
        dag.add(3, vec![1], readout(schema.clone(), 0.99)).unwrap();
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

/// Relabelled KLL parameters and missing bindings fail explicitly.
#[test]
fn panes_reject_parameters_schema_and_missing_binding() {
    let (_maintenance, merge, schema) = fixture();
    let wrong = Value::Summary {
        family: family(200),
        state: Arc::new(DatasketchesKLLAccumulator::new(128)),
    };
    assert!(Batch::try_new(schema.clone(), vec![vec![wrong]]).is_err());
    let compiled = PhysicalPostASAPDAG::from_operators(
        BTreeMap::from([(0, InputContract::bounded(schema))]),
        BTreeMap::from([(1, (vec![0], merge))]),
        vec![1],
    )
    .unwrap();
    assert!(compiled.instantiate(BTreeMap::new()).is_err());
    let raw = raw_schema();
    let source = Operator::source(
        raw.clone(),
        vec![Batch::try_new(raw, vec![vec![Value::Float64(1.)]]).unwrap()],
    )
    .unwrap();
    assert!(compiled
        .instantiate(BTreeMap::from([(0, Box::new(source) as Source<'_>)]))
        .is_err());
}
