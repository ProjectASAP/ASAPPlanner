//! Planner output binds directly to the shared runtime over raw counter samples.
use crate::common;
use asap_executor::sources::InMemoryDataStore;
use asap_executor::{
    physical_planner::{
        compile,
        promql_rows::{series_row, with_series_identity},
    },
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use common::{compile_physical_asap_dag, computation_candidates};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use planner_types::ir::properties::AccuracyTarget;
use planner_types::ir::schema::*;
use planner_types::ir::ASAPOp;
use std::{collections::BTreeMap, rc::Rc, sync::Arc};
// The heap candidates of `topk by(job)(2, sum by(service, job)(rate(m[1m])))`
// rank the summed per-series rates of raw counter samples bound at the
// `Scan`, both maintained at ingestion and run at query time.
#[test]
fn planner_weighted_topk_binds_at_either_deployment_phase() {
    assert_weighted_binding(SketchAlgorithm::CmsWithHeap);
    assert_weighted_binding(SketchAlgorithm::CountSketchWithHeap);
}

/// Whether `dag` builds a heap sketch of `algorithm`.
fn builds(dag: &PhysicalASAPDAG, algorithm: &SketchAlgorithm) -> bool {
    dag.nodes.iter().any(|node| {
        matches!(&node.payload,
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) if kind.algorithm() == algorithm)
    })
}

/// The logical candidate of `root` at ε = 0.1 that builds a heap sketch of
/// `algorithm`.
fn heap_candidate(
    root: Rc<planner_types::ir::OperatorNode>,
    algorithm: &SketchAlgorithm,
) -> Rc<planner_types::ir::OperatorNode> {
    computation_candidates(root)
        .into_iter()
        .find(|candidate| builds(&compile_physical_asap_dag(candidate).unwrap(), algorithm))
        .unwrap_or_else(|| panic!("missing {algorithm:?} candidate"))
}

/// `plan` as Stage 3 hands it to the executor when the data declares `m` a
/// counter: its rate weights' open requirement confirmed non-negative.
fn confirmed_as_counter(
    plan: Rc<planner_types::ir::OperatorNode>,
) -> Rc<planner_types::ir::OperatorNode> {
    use planner_types::ir::QueryRoot;
    use planner_types::workload::{MetricType, SourceFacts};
    let data = DataWorkload {
        sources: [(
            "m".to_string(),
            SourceFacts {
                metric_type: Some(MetricType::Counter),
                ..SourceFacts::default()
            },
        )]
        .into(),
        ..DataWorkload::default()
    };
    match planner_types::workload::bounds::with_confirmed_weights(
        &[QueryRoot::Operator(plan)],
        &data,
    )
    .remove(0)
    {
        QueryRoot::Operator(node) => node,
        QueryRoot::Scalar(_) => unreachable!("an operator root"),
    }
}

fn assert_weighted_binding(algorithm: SketchAlgorithm) {
    let root = lower_promql(
        "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
        AccuracyTarget::max_error(0.1),
    )
    .unwrap();
    // The per-series rate reads each raw series by its full identity.
    let root = with_series_identity(&root).unwrap();
    let plan = confirmed_as_counter(heap_candidate(root, &algorithm));
    let dag = compile_physical_asap_dag(&plan).unwrap();
    let scan = dag
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::NonASAP(planner_types::ir::NonASAPOp::Scan {
                source,
                ..
            }) => Some((source.clone(), Arc::new(node.output_schema.clone()))),
            _ => None,
        })
        .unwrap();
    let (source, samples) = scan;
    // One counter series per rate: two samples 32 s apart in `(0, 60 s]`,
    // the first an increase above zero, so `rate(m[1m])` extrapolates to the
    // whole minute and is exactly `increase / 32 s`. The two `auth` series
    // differ in `instance`.
    let rows = [
        ("auth", "api", "a", 0.125),
        ("auth", "api", "b", 0.25),
        ("checkout", "api", "a", 0.3125),
        ("search", "api", "a", 0.0625),
        ("ingest", "batch", "a", 100.),
        ("export", "batch", "a", 80.),
        ("cleanup", "batch", "a", 20.),
    ]
    .into_iter()
    .flat_map(|(service, job, instance, rate)| {
        let labels = BTreeMap::from([
            ("__name__".to_string(), "m".to_string()),
            ("instance".to_string(), instance.to_string()),
            ("job".to_string(), job.to_string()),
            ("service".to_string(), service.to_string()),
        ]);
        let increase = rate * 32.;
        [(14_000, increase), (46_000, 2. * increase)]
            .map(|(ts, value)| series_row(&samples, &labels, ts, value).unwrap())
    })
    .collect();
    let batch = Batch::try_new(samples.clone(), rows).unwrap();
    // The summaries `MatAtIngestion` run as data arrives; `NotMaterialized`
    // (the default) runs everything when the query does.
    for (placed, scope) in [
        (
            continuously_maintained_dag(&plan),
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 60_000,
            },
        ),
        (
            dag.clone(),
            Scope::Query {
                evaluation_time_ms: 60_000,
            },
        ),
    ] {
        let compiled = compile(&placed, &[], &[dag.roots[0] as u64]).unwrap();
        let store = InMemoryDataStore::default()
            .with_raw_data(source.clone(), samples.clone(), vec![batch.clone()])
            .unwrap();
        let physical_dag = compiled.instantiate(Arc::new(store)).unwrap();
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let output = block_on(async {
            let mut output = Vec::new();
            let mut stream = physical_dag
                .execute(&[dag.roots[0] as u64], context)
                .unwrap()
                .remove(0);
            while let Some(batch) = stream.next().await {
                output.extend(batch.unwrap().to_rows().iter().cloned());
            }
            output
        });
        // Two items per `job`: the outer `by(job)` partitions the ranking.
        // Each row is `[job, topk_2, service]`: the top-k's columns, then
        // the ranked item's label.
        let schema = &dag
            .nodes
            .iter()
            .find(|node| node.id == dag.roots[0])
            .unwrap()
            .output_schema;
        let column = |name: &str| schema.fields.iter().position(|f| f.name == name).unwrap();
        let (job, value) = (column("job"), column("topk_2"));
        assert_eq!(column("service"), 2);
        let mut ranked = output
            .iter()
            .map(|row| match (&row[job], &row[value]) {
                (Value::Utf8(job), Value::Float64(score)) => (job.to_string(), *score),
                other => panic!("unexpected (job, value) {other:?}"),
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let expected = [
            ("api", 0.3125),
            ("api", 0.375),
            ("batch", 80.),
            ("batch", 100.),
        ];
        assert_eq!(
            ranked,
            expected.map(|(job, score)| (job.to_string(), score)),
            "{}",
            if placed == dag {
                "query time"
            } else {
                "ingestion time"
            }
        );
    }
}

use asap_frontend_promql::{lower_promql_query_workload, HistogramCatalog};
use planner_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};
pub fn lower_promql(
    query: &str,
    accuracy: AccuracyTarget,
) -> Result<Rc<planner_types::ir::OperatorNode>, asap_frontend_promql::PromqlError> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy,
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
            DurationMs(1_000),
        )),
    };
    Ok(
        lower_promql_query_workload(&workload, &HistogramCatalog::default())?
            .remove(0)
            .as_operator()
            .cloned()
            .expect("operator root"),
    )
}

// The old untyped heap updater must not silently round a Planner rate update.
#[test]
fn rate_updates_cannot_enter_integer_heap_factory() {
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 272,
                depth: 5,
                heap_size: 100,
            },
        ),
        Default::default(),
    );
    let input = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(
            planner_types::ir::scalar::ColumnRef::Named("service".into()),
        )),
        weight: SummaryInputExpr::Column(planner_types::ir::scalar::ColumnRef::SampleValue),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
    };
    assert!(asap_executor::factory::create_planner_accumulator(
        &family,
        &input,
        &Default::default()
    )
    .is_err());
}

/// `candidate` with every `SummaryAgg` `MatAtIngestion`, the materialization
/// a continuously served query would get: each summary and every input it
/// consumes run at ingestion time, the rest at query time. The candidate pins
/// its finalize boundary to query time, which would put the summaries above
/// it upstream of a query-time node, so the pin is lifted first.
fn continuously_maintained_dag(candidate: &Rc<planner_types::ir::OperatorNode>) -> PhysicalASAPDAG {
    use planner_types::ir::{
        apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache,
        Operator as IrOperator, OperatorNode,
    };
    use planner_types::physical::{Materialization, Medium};
    fn unpinned(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        let mut copy = (**node).clone();
        copy.operator = node.operator.map_children(unpinned);
        if matches!(
            copy.operator,
            IrOperator::ASAP(ASAPOp::FinalizeExactAccumulator { .. })
        ) {
            copy.timing = None;
        }
        Rc::new(copy)
    }
    let candidate = unpinned(candidate);
    let mut assignment = MaterializationAssignment::default();
    for node in OperatorNode::reachable(&candidate) {
        if matches!(node.operator, IrOperator::ASAP(ASAPOp::SummaryAgg { .. })) {
            assignment.set(&node, Materialization::MatAtIngestion(Medium::Memory));
        }
    }
    let timed =
        apply_materialization_timings(&candidate, &assignment, &mut NodeToTimedNodeCache::new())
            .unwrap();
    planner_types::ir::physical_export::compile_physical_asap_dag(&timed).unwrap()
}
