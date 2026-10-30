//! The deployment's accuracy model plugs into plan selection (#509 Stage 3)
//! only. Stage 1 composes guarantees with the built-in analytical rules, so
//! a different model leaves the candidates unchanged and changes only which
//! of them Stage 3 accepts.

use std::collections::BTreeSet;
use std::rc::Rc;

use asap_accuracy::{AccuracyModel, BuiltinAccuracyModel};
use asap_frontend_promql::{lower_promql_query_workload, promql_metric_names};
use asap_plan_selection::{PlanOutput, PlanningModels, RejectionCause};
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyMetric, AccuracyTarget};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
};

/// A deployment that trusts no approximate estimate: only an exact
/// guarantee meets a target.
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
                range_evaluation: None,
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

/// A small, fixed physical inventory, independent of deployment accuracy models.
/// Exercise each local implementation with the remaining computations exact.
/// These fixtures test evaluator hooks, not a timed search's archive prefix.
fn physical_candidates(
    workload: &PlanningWorkload,
) -> Vec<asap_physical_asap_optimization::PhysicalWorkloadPlan> {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    use asap_physical_asap_optimization::test_support::optimize_physical_asap_chosen;
    use asap_types::physical::Materialization;

    let roots = lower_promql_query_workload(workload).expect("lowers");
    let inventory =
        generate_per_computation_candidates(roots.into_iter().enumerate().collect()).unwrap();
    let mut choices = BTreeSet::from([vec![0; inventory.targets.len()]]);
    for (index, target) in inventory.targets.iter().enumerate() {
        for implementation in 1..target.choice_count() {
            let mut choice = vec![0; inventory.targets.len()];
            choice[index] = implementation;
            choices.insert(choice);
        }
    }
    choices
        .into_iter()
        .enumerate()
        .map(|(index, choice)| {
            let roots: Vec<_> = compose_logical_candidate(&inventory, &choice)
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
            let mut candidate = optimize_physical_asap_chosen(
                &format!("fixture-{index}"),
                &roots,
                &workload.query_workload.entries().collect::<Vec<_>>(),
                workload.data_workload.as_ref().unwrap(),
                false,
                &mut |_, _, _| Materialization::NotMaterialized,
            )
            .unwrap()
            .expect("query-time fixture is executable");
            candidate.id = format!("fixture-{index}");
            candidate
        })
        .collect()
}

fn plan_every_candidate(
    workload: &PlanningWorkload,
    candidates: &[asap_physical_asap_optimization::PhysicalWorkloadPlan],
    models: PlanningModels<'_>,
) -> Result<PlanOutput, asap_plan_selection::SelectionError> {
    let selection = asap_plan_selection::select_plan(
        candidates,
        &workload.query_workload.entries().collect::<Vec<_>>(),
        workload.data_workload.as_ref().unwrap(),
        models,
    )?;
    let plans = selection
        .plan
        .iter()
        .enumerate()
        .map(|(entry_index, root)| match root {
            QueryRoot::Operator(node) => asap_plan_selection::QueryPlan {
                entry_index,
                root: Rc::clone(node),
            },
            QueryRoot::Scalar(_) => panic!("operator queries only"),
        })
        .collect();
    let mut output = PlanOutput::new(plans);
    output.selection = Some(selection);
    Ok(output)
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

    let candidates = physical_candidates(&workload);

    // The built-in models: Stage 3 checks guarantees with BuiltinAccuracyModel.
    let builtin = plan_every_candidate(&workload, &candidates, PlanningModels::builtin())
        .expect("plans with the built-in models");
    // The candidates whose approximate output the built-in model accepts:
    // the sketch candidates, sized in Stage 1 for the 1% target.
    let builtin_approximate = approximate_accepted(&builtin);
    assert!(!builtin_approximate.is_empty());

    // The same workload with a deployment model that accepts only exact
    // outputs, plugged into Stage 3 through `PlanningModels::with_accuracy`.
    let exact_only_model = ExactOnlyAccuracyModel;
    let exact_only = plan_every_candidate(
        &workload,
        &candidates,
        PlanningModels::builtin().with_accuracy(&exact_only_model),
    )
    .expect("plans with the exact-only model");

    // Stages 1 and 2 do not read the model: Stage 3 got the same
    // candidates, with the same guarantees and summary parameters.
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

/// A deployment that has measured every output itself and accepts each one
/// for any target, whatever its guarantee says.
struct AcceptsEveryOutput;

impl AccuracyModel for AcceptsEveryOutput {
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
        BuiltinAccuracyModel.satisfies(guarantee, target)
    }

    fn accepts_output_regardless_of_guarantee(
        &self,
        _output: &QueryRoot,
        _target: &AccuracyTarget,
    ) -> bool {
        true
    }
}

/// The candidates for which Stage 3 accepted some query's output because
/// the deployment's model accepted it regardless of its guarantee.
fn accepted_regardless_of_guarantee(output: &PlanOutput) -> BTreeSet<String> {
    let selection = output.selection.as_ref().expect("plan selection ran");
    selection
        .accepted
        .iter()
        .filter(|(_, queries)| queries.iter().any(|q| q.accepted_regardless_of_guarantee))
        .map(|(id, _)| id.clone())
        .collect()
}

/// The ids of the candidates `output`'s selection rejected with `cause`.
fn rejected_with(output: &PlanOutput, cause: RejectionCause) -> BTreeSet<String> {
    output
        .selection
        .as_ref()
        .expect("plan selection ran")
        .rejected
        .iter()
        .filter(|r| r.cause == cause)
        .map(|r| r.id.clone())
        .collect()
}

/// A deployment model that accepts an output directly also accepts one with
/// no guarantee: the built-in model rejects the summary candidates of a
/// difference of two quantile estimates as having none, and the accepting
/// model makes them valid, and Stage 3 selects one.
#[tokio::test]
async fn the_deployment_accuracy_model_accepts_outputs_without_a_guarantee() {
    // The user's workload: a difference of two quantile estimates, 1% rank
    // error. Stage 1 has no analytical rule for a difference of rank
    // estimates, so it keeps the summary candidates with no guarantee.
    let workload = rank_target_workload("quantile(0.99, rate(x[5m])) - quantile(0.5, rate(x[5m]))");

    let candidates = physical_candidates(&workload);

    // The built-in model (default hook: accepts nothing directly) rejects
    // every summary candidate as having no guarantee and selects exact.
    let builtin = plan_every_candidate(&workload, &candidates, PlanningModels::builtin())
        .expect("plans with the built-in models");
    // Candidates rejected for having no guarantee.
    let unguaranteed = rejected_with(&builtin, RejectionCause::NoAccuracyGuarantee);
    assert!(!unguaranteed.is_empty());
    assert!(accepted_regardless_of_guarantee(&builtin).is_empty());
    assert!(!reads_sketch(&builtin.plans[0].root));

    // The deployment's model accepts every output directly.
    let accepting_model = AcceptsEveryOutput;
    let accepting = plan_every_candidate(
        &workload,
        &candidates,
        PlanningModels::builtin().with_accuracy(&accepting_model),
    )
    .expect("plans with the accepting model");
    let accepting_selection = accepting.selection.as_ref().unwrap();
    assert_eq!(
        stage3_candidate_ids(&builtin),
        stage3_candidate_ids(&accepting)
    );

    // Every candidate the built-in model rejected for having no guarantee
    // is now valid and priced, accepted regardless of its guarantee, and no
    // candidate fails the accuracy check.
    assert!(accepted_regardless_of_guarantee(&accepting).is_superset(&unguaranteed));
    for id in &unguaranteed {
        assert!(accepting_selection.costs.contains_key(id), "{id}");
    }
    assert!(accepting_selection.rejected.iter().all(|r| !matches!(
        r.cause,
        RejectionCause::NoAccuracyGuarantee | RejectionCause::AccuracyTargetMissed
    )));
    // A singleton previously rejected sketch is now selectable; the cheapest
    // plan in the complete fixture need not be that sketch.
    let sketch = candidates
        .iter()
        .find(|c| unguaranteed.contains(&c.id))
        .unwrap();
    let singleton = plan_every_candidate(
        &workload,
        std::slice::from_ref(sketch),
        PlanningModels::builtin().with_accuracy(&accepting_model),
    )
    .unwrap();
    assert_eq!(singleton.selection.as_ref().unwrap().selected, sketch.id);
    assert!(reads_sketch(&singleton.plans[0].root));
}

/// The hook has priority over a guarantee that misses: a candidate whose
/// guarantee misses its target under the built-in models (a relative bound
/// for a rank target) is accepted under the accepting model.
#[tokio::test]
async fn the_deployment_accuracy_model_accepts_outputs_whose_guarantee_misses() {
    // The user's workload: one p99 query with a 1% rank-error target.
    let workload = rank_target_workload("quantile_over_time(0.99, lat[5m])");
    let candidates = physical_candidates(&workload);
    // Under the built-in models: the candidates whose guarantee misses the
    // target (a relative bound for a rank target).
    let builtin = plan_every_candidate(&workload, &candidates, PlanningModels::builtin())
        .expect("plans with the built-in models");
    let missed = rejected_with(&builtin, RejectionCause::AccuracyTargetMissed);
    assert!(!missed.is_empty());
    // The same workload with the model that accepts every output directly.
    let accepting_model = AcceptsEveryOutput;
    let accepting = plan_every_candidate(
        &workload,
        &candidates,
        PlanningModels::builtin().with_accuracy(&accepting_model),
    )
    .expect("plans with the accepting model");
    // Each of them is now valid, priced and accepted regardless of guarantee.
    let accepting_selection = accepting.selection.as_ref().unwrap();
    for id in &missed {
        assert!(accepting_selection.costs.contains_key(id), "{id}");
        assert!(
            accepting_selection.accepted[id][0].accepted_regardless_of_guarantee,
            "{id}"
        );
    }
    // The hook also overrides guarantees that meet the target: every
    // accepted output, including exact and KLL candidates, is accepted
    // regardless of its guarantee.
    assert_eq!(accepting_selection.accepted.len(), candidates.len());
    for (id, queries) in &accepting_selection.accepted {
        assert!(
            queries.iter().all(|q| q.accepted_regardless_of_guarantee),
            "{id}"
        );
    }
}
