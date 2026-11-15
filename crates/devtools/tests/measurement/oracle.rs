//! §2.11: the exhaustive oracle. Every plan over Stage 2's candidates is
//! enumerated independently of Stage 3
//! (`asap_types::test_support::physical_plan_enumeration`), each is
//! measured, and the selected plan must be within τ of the cheapest
//! measured one, τ = 0.05 + twice the cheapest plan's coefficient of
//! variation over its repetitions.
// provisional answer 17

use std::collections::BTreeSet;

use asap_devtools::measurement::benchmark::{
    bench_plan, installable_plan_of_selection, ArmMeasurement, BenchArm, BenchRequest, DataSetting,
};
use asap_devtools::measurement::calibration_tables::{calibrate, CalibrateRequest, PlanningTables};
use asap_devtools::measurement::cost_calibration::CalibrationFamilies;
use asap_devtools::planning_stages::{
    lower_promql_roots, run_planning_stages_over, PlanningStagesRun,
};
use asap_planner::{PlanningModels, PlanningWorkload};
use asap_types::stages::PhysicalCandidateId;
use asap_types::test_support::physical_plan_enumeration::{
    every_physical_plan, PhysicalPlanChoice,
};

use super::common::{exact, facts, per_value, plan, workload};

const MOST_PLANS: usize = 1_000_000;

fn exact_family() -> PlanningWorkload {
    workload(
        &[("sum by (job) (sum_over_time(m[5m]))", exact())],
        10_000,
        &[("m", facts(8, 1_000))],
    )
}

fn sketch_family() -> PlanningWorkload {
    workload(
        &[(
            "quantile_over_time(0.99, m[5m])",
            per_value("relative", 0.01, 0.01),
        )],
        10_000,
        &[("m", facts(8, 1_000))],
    )
}

fn joint_sharing() -> PlanningWorkload {
    workload(
        &[
            ("sum by (job) (sum_over_time(m[5m]))", exact()),
            (
                "quantile_over_time(0.99, m[5m])",
                per_value("relative", 0.01, 0.01),
            ),
            ("count_over_time(m[5m])", exact()),
        ],
        10_000,
        &[("m", facts(8, 1_000))],
    )
}

fn planning_run(workload: &PlanningWorkload, models: PlanningModels<'_>) -> PlanningStagesRun {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    run_planning_stages_over(
        lower_promql_roots(workload).unwrap(),
        &entries,
        &data,
        models,
    )
    .unwrap()
}

/// Every enumerated plan is closed: each query root's group has a picked
/// candidate, one candidate per group, and each picked candidate's every
/// input has one admissible edge whose consumer is that candidate and
/// whose producer is the candidate picked for the producer's group.
#[test]
fn enumerated_plans_are_closed_under_edges() {
    for workload in [exact_family(), sketch_family()] {
        let run = planning_run(&workload, PlanningModels::builtin());
        let physical = run.run.physical_asap_candidates.expect("Stage 2 ran");
        let plans = every_physical_plan(&physical, MOST_PLANS).unwrap();
        assert!(!plans.is_empty());
        let distinct: BTreeSet<_> = plans.iter().collect();
        assert_eq!(distinct.len(), plans.len(), "a plan is enumerated twice");
        for plan in &plans {
            for query in physical.logical_candidates.memo.queries() {
                for group in query.root.operator_refs() {
                    assert!(plan.candidate_of_group.contains_key(group), "{plan:?}");
                }
            }
            for (&group, &candidate) in &plan.candidate_of_group {
                assert_eq!(
                    physical.physical_candidates[candidate.0]
                        .logical_candidate_id
                        .group,
                    group
                );
                for input in 0..physical.input_count(candidate) {
                    let edge_id = plan.edge_into_input[&(candidate, input)];
                    assert!(physical
                        .admissible_edges_into(candidate, input)
                        .contains(&edge_id));
                    let edge = physical.edge(edge_id);
                    assert_eq!(edge.consumer_candidate_id, candidate);
                    let producer_group = physical.physical_candidates[edge.producer_candidate_id.0]
                        .logical_candidate_id
                        .group;
                    assert_eq!(
                        plan.candidate_of_group[&producer_group],
                        edge.producer_candidate_id
                    );
                }
            }
            assert_eq!(
                plan.edge_into_input.len(),
                plan.candidate_of_group
                    .values()
                    .map(|&candidate| physical.input_count(candidate))
                    .sum::<usize>()
            );
        }
    }
}

/// The plan Stage 3 selects is one of the enumerated plans (validity):
/// its nodes' physical candidates are exactly one plan's candidates.
#[test]
fn enumerated_plans_contain_the_selected_plan() {
    for workload in [exact_family(), sketch_family()] {
        let run = planning_run(&workload, PlanningModels::builtin());
        let (dag, _) = run.run.selection.as_ref().expect("a plan is selected");
        let selected: BTreeSet<PhysicalCandidateId> = dag
            .nodes
            .iter()
            .filter_map(|node| node.selected_physical_candidate)
            .collect();
        let physical = run
            .run
            .physical_asap_candidates
            .as_ref()
            .expect("Stage 2 ran");
        let plans = every_physical_plan(physical, MOST_PLANS).unwrap();
        assert!(
            plans.iter().any(|plan| plan.candidates() == selected),
            "selected {selected:?} is none of the {} enumerated plans",
            plans.len()
        );
    }
}

/// The joint workload's plans are few enough to measure each: at most
/// [`MOST_PLANS`]. Stage 2 gives it 452 physical candidates in 23 groups
/// (the exact family has 27 and 624 plans).
#[test]
fn joint_sharing_plan_space_is_enumerable() {
    let run = planning_run(&joint_sharing(), PlanningModels::builtin());
    let physical = run.run.physical_asap_candidates.expect("Stage 2 ran");
    let plans = every_physical_plan(&physical, MOST_PLANS);
    assert!(plans.is_ok(), "{plans:?}");
}

/// The calibrated tables of this machine.
fn calibrated_tables() -> PlanningTables {
    let calibration = calibrate(&CalibrateRequest {
        families: CalibrationFamilies::All,
        small: false,
        seed: 7,
    })
    .unwrap();
    PlanningTables {
        cost_inputs: asap_cost::CostInputs {
            measured_operation_costs: calibration.costs,
            ..asap_cost::CostInputs::default()
        },
        accuracy: calibration.accuracy,
        warnings: Vec::new(),
    }
}

/// Every enumerated plan and the selected one, measured.
struct OracleRun {
    enumerated: Vec<(PhysicalPlanChoice, ArmMeasurement)>,
    selected: ArmMeasurement,
}

fn run_oracle(workload: &PlanningWorkload) -> OracleRun {
    let tables = calibrated_tables();
    let models = PlanningModels::new(&tables.accuracy).with_cost_inputs(&tables.cost_inputs);
    let run = planning_run(workload, models);
    let physical = run.run.physical_asap_candidates.expect("Stage 2 ran");
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let setting = DataSetting {
        sample_interval_ms: 1_000,
        evaluation_interval_ms: 10_000,
        raw_data_retained: true,
    };
    let request = BenchRequest {
        workload,
        arms: vec![BenchArm::Planned],
        settings: vec![setting.clone()],
        series_count: 8,
        duration_ms: 1_200_000,
        repetitions: 5,
        tables: tables.clone(),
        seed: 1,
        cost_gate: true,
        output_root: None,
    };
    let enumerated = every_physical_plan(&physical, MOST_PLANS)
        .unwrap()
        .into_iter()
        .map(|choice| {
            let installable = installable_plan_of_selection(
                &physical,
                &entries,
                &choice.candidates(),
                &choice.edges(),
            )
            .unwrap();
            let measured = bench_plan(&installable, workload, &setting, &request).unwrap();
            (choice, measured)
        })
        .collect();
    let (selected_plan, _) = plan(
        workload,
        &tables.cost_inputs,
        &tables.accuracy,
        &asap_planner::DeploymentCapabilities::default(),
    );
    let selected = bench_plan(&selected_plan, workload, &setting, &request).unwrap();
    OracleRun {
        enumerated,
        selected,
    }
}

fn measured_cost(arm: &ArmMeasurement) -> f64 {
    arm.measured_cost_per_second.expect("a measured cost")
}

/// The selected plan's measured cost is within τ of the cheapest
/// enumerated plan's.
// provisional answer 23
fn assert_within_tau(oracle: &OracleRun) {
    let cheapest = oracle
        .enumerated
        .iter()
        .map(|(_, arm)| arm)
        .min_by(|a, b| measured_cost(a).total_cmp(&measured_cost(b)))
        .expect("at least one plan");
    let repetitions = &cheapest.repetition_costs_per_second;
    let mean = repetitions.iter().sum::<f64>() / repetitions.len() as f64;
    let variance = repetitions
        .iter()
        .map(|cost| (cost - mean).powi(2))
        .sum::<f64>()
        / repetitions.len() as f64;
    let tau = 0.05 + 2.0 * variance.sqrt() / mean;
    assert!(
        measured_cost(&oracle.selected) <= (1.0 + tau) * measured_cost(cheapest),
        "selected {} vs cheapest {} (tau {tau})",
        measured_cost(&oracle.selected),
        measured_cost(cheapest)
    );
}

/// Spearman's rank correlation of two equally long samples, no ties
/// assumed.
fn spearman(left: &[f64], right: &[f64]) -> f64 {
    let ranks = |values: &[f64]| {
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
        let mut ranks = vec![0.0; values.len()];
        for (rank, index) in order.into_iter().enumerate() {
            ranks[index] = rank as f64;
        }
        ranks
    };
    let (left, right) = (ranks(left), ranks(right));
    let n = left.len() as f64;
    let d2: f64 = left.iter().zip(&right).map(|(a, b)| (a - b).powi(2)).sum();
    1.0 - 6.0 * d2 / (n * (n * n - 1.0))
}

/// The exact family (exact from raw data, a query-time `SummaryAgg`, a
/// chain at each feasible width, no-raw variants): the selected plan is
/// within τ, and predicted and measured costs rank the plans alike
/// (Spearman ρ ≥ 0.8).
#[test]
#[ignore = "timing: run on a dedicated host"]
fn oracle_exact_family() {
    let oracle = run_oracle(&exact_family());
    assert_within_tau(&oracle);
    let predicted: Vec<f64> = oracle
        .enumerated
        .iter()
        .map(|(_, arm)| arm.predicted.cost_per_second.unwrap())
        .collect();
    let measured: Vec<f64> = oracle
        .enumerated
        .iter()
        .map(|(_, arm)| measured_cost(arm))
        .collect();
    assert!(spearman(&predicted, &measured) >= 0.8);
}

/// The sketch family (exact, DDSketch α ≤ 0.01 at query time or as a
/// chain; KLL excluded by the relative metric): every enumerated plan meets
/// the target, and the selected one is within τ.
// provisional answer 3
#[test]
#[ignore = "timing: run on a dedicated host"]
fn oracle_sketch_family() {
    let oracle = run_oracle(&sketch_family());
    for (choice, arm) in &oracle.enumerated {
        assert!(
            arm.measured_error.unwrap() <= 0.01 * (1.0 + 1e-9),
            "{choice:?}"
        );
    }
    assert_within_tau(&oracle);
}

/// The two families with `count_over_time(m[5m])`: the selected shared
/// plan is within τ, and the shared node is predicted once.
#[test]
#[ignore = "timing: run on a dedicated host"]
fn oracle_joint_sharing() {
    let oracle = run_oracle(&joint_sharing());
    assert_within_tau(&oracle);
    let nodes = &oracle.selected.predicted.predicted_usage.per_node;
    let ids: BTreeSet<_> = nodes.iter().map(|node| node.node_id).collect();
    assert_eq!(ids.len(), nodes.len(), "a node is predicted twice");
}
