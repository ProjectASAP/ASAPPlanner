//! Planner output binds directly to the shared runtime at a declared rate-value frontier.
use asap_aware_mapping::{
    accuracy::{
        AccuracyEvidenceProvider, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
    },
    cost_model::DefaultCostModel,
    Replacement, ReplacementStrategy, SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_physical_operators::dag::{
    operators::Operator,
    planner::{bind, Source},
    values::{Batch, Value},
    Limits, RunContext, Scope,
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::*,
    pre_asap::{DataType, QueryExpr},
    types::AccuracyTarget,
};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};
struct Evidence;
impl AccuracyEvidenceProvider for Evidence {
    fn topk_max_distinct_items(&self, _: &QueryExpr) -> Option<u64> {
        Some(1000)
    }
    fn propagation_stats(
        &self,
        op: &CompositionOperator,
        _: &SummaryFamilyType,
        _: Option<&SketchQuery>,
    ) -> PropagationStats {
        if matches!(op, CompositionOperator::TopKSelection) {
            PropagationStats {
                topk_selected_lower_bound: Some(101.),
                topk_excluded_upper_bound: Some(100.),
                topk_interval_failure_probability: Some(0.001),
                ..Default::default()
            }
        } else {
            Default::default()
        }
    }
}
// The evidence here exercises binding; it is not inferred from the sample data.
#[test]
fn planner_weighted_topk_binds_at_either_deployment_phase() {
    assert_weighted_binding(&Evidence);
}

// Binding validates representation, while deployment owns evidence acceptance.
#[test]
fn physical_binding_does_not_impose_an_accuracy_acceptance_policy() {
    assert_weighted_binding(&asap_aware_mapping::accuracy::NoAccuracyEvidence);
}

fn assert_weighted_binding(evidence: &dyn AccuracyEvidenceProvider) {
    let root = Rc::new(
        lower_promql(
            "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap(),
    );
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        evidence,
    );
    let plan = strategy
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .find_map(|candidate| match candidate.replacement {
            Replacement::Summary(node) if candidate.rationale.contains("CmsWithHeap") => Some(node),
            _ => None,
        })
        .unwrap();
    let dag = compile_executable_dag(&plan).unwrap();
    let build=dag.nodes.iter().find(|node|matches!(&node.payload,ExecutableOperatorPayload::SummaryAgg{family:SummaryFamilyType::Sketch(kind,_),..}if kind.algorithm()==&SketchAlgorithm::CmsWithHeap)).unwrap();
    let rate_id = dag
        .edges
        .iter()
        .find(|edge| edge.consumer == build.id)
        .unwrap()
        .producer;
    let rates = Arc::new(
        dag.nodes
            .iter()
            .find(|node| node.id == rate_id)
            .unwrap()
            .output_schema
            .clone(),
    );
    let rows = [
        ("auth", "api", 0.125),
        ("auth", "api", 0.25),
        ("checkout", "api", 0.3125),
        ("search", "api", 0.0625),
        ("ingest", "batch", 100.),
        ("export", "batch", 80.),
        ("cleanup", "batch", 20.),
    ]
    .into_iter()
    .map(|(service, job, value)| {
        rates
            .fields
            .iter()
            .map(|field| match field.name.as_str() {
                "service" => Value::Utf8(service.into()),
                "job" => Value::Utf8(job.into()),
                "value" => Value::Float64(value),
                _ => match field.dtype {
                    SummaryFamilyType::Plain(DataType::Timestamp) => Value::Timestamp(60_000),
                    _ => panic!("unexpected rate column {field:?}"),
                },
            })
            .collect()
    })
    .collect();
    let batch = Batch::try_new(rates.clone(), rows).unwrap();
    for (phase, scope) in [
        (
            ExecutionTiming::IngestionTime,
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 60_000,
                revision: 1,
            },
        ),
        (
            ExecutionTiming::QueryTime,
            Scope::Query {
                evaluation_time_ms: 60_000,
                revision: 1,
            },
        ),
    ] {
        let placed = dag
            .with_execution_phases(&dag.nodes.iter().map(|node| (node.id, phase)).collect())
            .unwrap();
        let source = Box::new(Operator::source(rates.clone(), vec![batch.clone()]).unwrap())
            as Source<'static>;
        let graph = bind(
            &placed,
            BTreeMap::from([(rate_id.0 as u64, source)]),
            &[dag.root.0 as u64],
        )
        .unwrap();
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let output = block_on(async {
            let mut output = Vec::new();
            let mut stream = graph
                .execute(&[dag.root.0 as u64], context)
                .unwrap()
                .remove(0);
            while let Some(batch) = stream.next().await {
                output.extend(batch.unwrap().rows().iter().cloned());
            }
            output
        });
        assert_eq!(output.len(), 4);
        let mut scores = output
            .iter()
            .map(|row| {
                row.iter()
                    .find_map(|v| {
                        if let Value::Float64(v) = v {
                            Some(*v)
                        } else {
                            None
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();
        scores.sort_by(f64::total_cmp);
        assert_eq!(scores, vec![0.3125, 0.375, 80., 100.]);
    }
}

use asap_frontend_promql::lower_promql_workload;
use planner_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence as WorkloadEvidence,
    PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload,
    TimeSelection,
};
pub fn lower_promql(
    query: &str,
    accuracy: AccuracyTarget,
) -> Result<QueryExpr, asap_frontend_promql::PromqlError> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: WorkloadEvidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let mut lowered = lower_promql_workload(&workload, 0)?;
    Ok(lowered.remove(0))
}

// The old untyped heap updater must not silently round a Planner rate update.
#[test]
fn rate_updates_cannot_enter_integer_heap_factory() {
    let family = SummaryFamilyType::Sketch(
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
            planner_types::pre_asap::ColumnRef::Named("service".into()),
        )),
        weight: SummaryInputExpr::Column(planner_types::pre_asap::ColumnRef::SampleValue),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
    };
    assert!(
        asap_physical_operators::factory::create_planner_accumulator(
            &family,
            &input,
            &Default::default()
        )
        .is_err()
    );
}
