//! The deployment's accuracy model plugs into plan selection (#509 Stage 3)
//! only. Stage 1 composes guarantees with the built-in analytical rules, so
//! a different model leaves the candidates unchanged and changes only which
//! of them Stage 3 accepts.

use std::collections::BTreeSet;
use std::rc::Rc;

use asap_frontend_promql::{lower_promql_query_workload, promql_metric_names};
use asap_plan_selection::{
    AccuracyModel, BuiltinAccuracyModel, PlanOutput, PlanningModels, RejectionCause,
};
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyMetric, AccuracyTarget};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
};

/// A deployment that trusts no approximate estimate: only an exact
/// guarantee meets a target. Composition is the trait's default (the
/// built-in rules).
struct ExactOnlyAccuracyModel;

impl AccuracyModel for ExactOnlyAccuracyModel {
    fn satisfies(&self, guarantee: &AccuracyGuarantee, _target: &AccuracyTarget) -> bool {
        guarantee.is_exact()
    }
}

/// `query` with a 1% rank error, repeated every ten minutes over metrics
/// sampled every 15 s.
fn rank_target_workload(query: &str) -> PlanningWorkload {
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

/// Every Stage 2 candidate for `workload` under `models`, rendered in full:
/// its id and its query roots with every node's guarantee and summary
/// parameters. Two runs that differ only in Stage 3's models must agree.
fn stage2_candidates(workload: &PlanningWorkload, models: PlanningModels<'_>) -> Vec<String> {
    let roots = lower_promql_query_workload(workload).expect("lowers");
    let run = asap_plan_selection::run_stage1_to_stage3(
        roots.into_iter().enumerate().collect(),
        &workload.query_workload.entries().collect::<Vec<_>>(),
        workload.data_workload.as_ref().unwrap(),
        models,
    )
    .expect("Stage 1 accepts the workload");
    run.stage2_physical_asap_optimization
        .iter()
        .map(|candidate| format!("{}: {:?}", candidate.id, candidate.roots))
        .collect()
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
    let workload = rank_target_workload("quantile_over_time(0.99, lat[5m])");

    // The built-in models: Stage 3 checks guarantees with BuiltinAccuracyModel.
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

    // Stages 1 and 2 do not read the model: Stage 3 got the same
    // candidates, with the same guarantees and summary parameters.
    assert_eq!(
        stage3_candidate_ids(&builtin),
        stage3_candidate_ids(&exact_only)
    );
    assert_eq!(
        stage2_candidates(&workload, PlanningModels::builtin()),
        stage2_candidates(
            &workload,
            PlanningModels::builtin().with_accuracy(&exact_only_model)
        )
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

/// A deployment whose own measurements cover every output that has no
/// analytical guarantee: it accepts each such output for any target.
struct AcceptsOutputsWithoutGuarantee;

impl AccuracyModel for AcceptsOutputsWithoutGuarantee {
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
        BuiltinAccuracyModel.satisfies(guarantee, target)
    }

    fn accepts_output_without_guarantee(
        &self,
        _output: &QueryRoot,
        _target: &AccuracyTarget,
    ) -> bool {
        true
    }
}

/// The candidates for which Stage 3 accepted some query's output that has
/// no guarantee.
fn accepted_without_guarantee(output: &PlanOutput) -> BTreeSet<String> {
    let selection = output.selection.as_ref().expect("plan selection ran");
    selection
        .accepted
        .iter()
        .filter(|(_, queries)| queries.iter().any(|q| q.accepted_without_guarantee))
        .map(|(id, _)| id.clone())
        .collect()
}

#[tokio::test]
async fn the_deployment_accuracy_model_decides_outputs_without_a_guarantee() {
    // The user's workload: a difference of two quantile estimates, 1% rank
    // error. Stage 1 has no analytical rule for a difference of rank
    // estimates, so it keeps the summary candidates with no guarantee.
    let workload = rank_target_workload("quantile(0.99, rate(x[5m])) - quantile(0.5, rate(x[5m]))");
    let promql = || FrontendInput::Promql { histograms: None };

    // The built-in model accepts no output without a guarantee: Stage 3
    // rejects every summary candidate as having none and selects exact.
    let builtin = e2e_plan(UserInput::new(
        &workload,
        promql(),
        PlanningModels::builtin(),
    ))
    .await
    .expect("plans with the built-in models");
    let builtin_selection = builtin.selection.as_ref().unwrap();
    let unguaranteed: BTreeSet<String> = builtin_selection
        .rejected
        .iter()
        .filter(|r| r.cause == RejectionCause::NoAccuracyGuarantee)
        .map(|r| r.id.clone())
        .collect();
    assert!(!unguaranteed.is_empty());
    assert!(!reads_sketch(&builtin.plans[0].root));

    // The deployment's model accepts those outputs through the hook.
    let accepting_model = AcceptsOutputsWithoutGuarantee;
    let accepting = e2e_plan(UserInput::new(
        &workload,
        promql(),
        PlanningModels::builtin().with_accuracy(&accepting_model),
    ))
    .await
    .expect("plans with the accepting model");
    let accepting_selection = accepting.selection.as_ref().unwrap();
    // Stages 1 and 2 produced the same candidates under both models.
    assert_eq!(
        stage2_candidates(&workload, PlanningModels::builtin()),
        stage2_candidates(
            &workload,
            PlanningModels::builtin().with_accuracy(&accepting_model)
        )
    );
    // Every candidate the built-in model rejected for having no guarantee
    // is now valid and priced, accepted without a guarantee.
    assert_eq!(accepted_without_guarantee(&accepting), unguaranteed);
    for id in &unguaranteed {
        assert!(accepting_selection.costs.contains_key(id), "{id}");
    }
    assert!(accepting_selection
        .rejected
        .iter()
        .all(|r| r.cause != RejectionCause::NoAccuracyGuarantee));
    // Stage 3 selects one of them, a plan that reads a sketch.
    assert!(unguaranteed.contains(&accepting_selection.selected));
    assert!(reads_sketch(&accepting.plans[0].root));
}

/// The hook decides only outputs with no guarantee: a candidate whose
/// guarantee misses its target under the built-in models (a relative bound
/// for a rank target) is still rejected for that under the accepting model.
#[tokio::test]
async fn the_hook_does_not_override_a_guarantee_that_misses() {
    // The user's workload: one p99 query with a 1% rank-error target.
    let workload = rank_target_workload("quantile_over_time(0.99, lat[5m])");
    let promql = || FrontendInput::Promql { histograms: None };
    // The candidates whose guarantee misses the target under the built-in models.
    let builtin = e2e_plan(UserInput::new(
        &workload,
        promql(),
        PlanningModels::builtin(),
    ))
    .await
    .expect("plans with the built-in models");
    let missed: BTreeSet<String> = builtin
        .selection
        .as_ref()
        .unwrap()
        .rejected
        .iter()
        .filter(|r| r.cause == RejectionCause::AccuracyTargetMissed)
        .map(|r| r.id.clone())
        .collect();
    assert!(!missed.is_empty());
    // The same workload with the model that accepts every output without a guarantee.
    let accepting_model = AcceptsOutputsWithoutGuarantee;
    let accepting = e2e_plan(UserInput::new(
        &workload,
        promql(),
        PlanningModels::builtin().with_accuracy(&accepting_model),
    ))
    .await
    .expect("plans with the accepting model");
    let accepting_selection = accepting.selection.as_ref().unwrap();
    for id in &missed {
        assert!(
            accepting_selection
                .rejected
                .iter()
                .any(|r| &r.id == id && r.cause == RejectionCause::AccuracyTargetMissed),
            "{id}"
        );
    }
}
