//! The deployment's accuracy model plugs into plan selection (#509 Stage 3)
//! only. Stage 1 composes guarantees with the built-in analytical rules, so
//! a different model leaves the candidates unchanged and changes only which
//! of them Stage 3 accepts.

use std::collections::BTreeSet;
use std::rc::Rc;

use asap_frontend_promql::promql_metric_names;
use asap_logical_asap_optimization::accuracy::PropagationStats;
use asap_plan_selection::{
    AccuracyModel, DefaultAccuracyModel, PlanOutput, PlanningModels, RejectionCause,
};
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::ir::properties::{
    AccuracyError, AccuracyGuarantee, AccuracyMetric, AccuracyTarget, CompositionOperator,
};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode};
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
};

/// A deployment that trusts no approximate estimate: the built-in
/// composition rules, but only an exact guarantee meets a target.
struct ExactOnlyAccuracyModel;

impl AccuracyModel for ExactOnlyAccuracyModel {
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[AccuracyGuarantee],
        local: Option<&AccuracyGuarantee>,
        stats: &PropagationStats,
    ) -> Result<AccuracyGuarantee, AccuracyError> {
        DefaultAccuracyModel.propagate(op, inputs, local, stats)
    }

    fn satisfies(&self, guarantee: &AccuracyGuarantee, _target: &AccuracyTarget) -> bool {
        guarantee.is_exact()
    }
}

/// `quantile_over_time(0.99, lat[5m])` with a 1% rank error, repeated every
/// ten minutes over a metric sampled every 15 s.
fn p99_workload() -> PlanningWorkload {
    let query = "quantile_over_time(0.99, lat[5m])";
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query(query.into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(600_000)),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank),
                    max_latency_ms: None,
                },
                predictability: Predictability::Unknown,
            }]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1.0,
            },
            ..DataWorkload::sampled_every(promql_metric_names(query).unwrap(), DurationMs(15_000))
        }),
    }
}

/// Whether `root` reads a sketch state.
fn reads_sketch(root: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(root).into_iter().any(|node| {
        matches!(
            node.asap(),
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(..),
                ..
            })
        )
    })
}

/// Every candidate Stage 3 was given: the valid ones it priced and the
/// ones it rejected.
fn stage3_candidate_ids(output: &PlanOutput) -> BTreeSet<String> {
    let selection = output.selection.as_ref().expect("plan selection ran");
    selection
        .costs
        .keys()
        .cloned()
        .chain(selection.rejected.iter().map(|r| r.id.clone()))
        .collect()
}

/// The candidates for which Stage 3 accepted an approximate output for some
/// query.
fn approximate_accepted(output: &PlanOutput) -> BTreeSet<String> {
    let selection = output.selection.as_ref().expect("plan selection ran");
    selection
        .accepted
        .iter()
        .filter(|(_, queries)| queries.iter().any(|q| q.accepted_metric.is_some()))
        .map(|(id, _)| id.clone())
        .collect()
}

#[tokio::test]
async fn the_deployment_accuracy_model_decides_only_in_plan_selection() {
    // The user's workload: one p99 query with a 1% rank-error target.
    let workload = p99_workload();

    // The built-in models: Stage 3 checks guarantees with DefaultAccuracyModel.
    let builtin = e2e_plan(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin(),
    ))
    .await
    .expect("plans with the built-in models");
    // The candidates whose approximate output the built-in model accepts:
    // the sketch candidates, sized in Stage 1 for the 1% target.
    let builtin_approximate = approximate_accepted(&builtin);
    assert!(!builtin_approximate.is_empty());

    // The same workload with a deployment model that accepts only exact
    // outputs, plugged into Stage 3 through `PlanningModels::with_accuracy`.
    let exact_only_model = ExactOnlyAccuracyModel;
    let exact_only = e2e_plan(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_accuracy(&exact_only_model),
    ))
    .await
    .expect("plans with the exact-only model");

    // Stages 1 and 2 do not read the model: Stage 3 got the same candidates.
    assert_eq!(
        stage3_candidate_ids(&builtin),
        stage3_candidate_ids(&exact_only)
    );
    // Stage 3 now rejects each of those candidates for missing its target,
    // accepts no approximate output and selects an exact plan.
    let exact_only_selection = exact_only.selection.as_ref().unwrap();
    for id in &builtin_approximate {
        assert!(exact_only_selection
            .rejected
            .iter()
            .any(|r| &r.id == id && r.cause == RejectionCause::AccuracyTargetMissed));
    }
    assert!(approximate_accepted(&exact_only).is_empty());
    assert!(!reads_sketch(&exact_only.plans[0].root));
}
