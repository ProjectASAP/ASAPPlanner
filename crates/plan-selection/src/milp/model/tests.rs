use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Duration;

use super::*;
use crate::owners::{prepare_milp_inputs, AccuracyWeight};
use asap_cost::{node_map, plan_cost, PlanCost, ResourceUnitCost};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::AssignmentRealizer;
use asap_types::workload::{DataWorkload, DurationMs};

// ---------------------------------------------------------------------------
// Through Stages 1 and 2
// ---------------------------------------------------------------------------

/// A workload through the real Stages 1 and 2, with the roots the
/// realizer rebuilds plans from.
pub(crate) struct Fixture {
    pub(crate) physical: PhysicalASAPMemoCandidates,
    pub(crate) roots: Vec<QueryRoot>,
    pub(crate) entries: Vec<QueryWorkloadEntry>,
    pub(crate) data: DataWorkload,
    pub(crate) query_time_results_retained: bool,
}

/// One query of a fixture: PromQL over metric `m`, its repeat interval in
/// ms, its relative error target (`None`: exact) and its latency bound.
#[derive(Clone, Copy)]
pub(crate) struct Query<'a> {
    promql: &'a str,
    interval_ms: u32,
    max_error: Option<f64>,
    max_latency_ms: Option<f64>,
}

pub(crate) fn query(promql: &str, interval_ms: u32) -> Query<'_> {
    Query {
        promql,
        interval_ms,
        max_error: None,
        max_latency_ms: None,
    }
}

pub(crate) fn approximate(promql: &str, interval_ms: u32, max_error: f64) -> Query<'_> {
    Query {
        max_error: Some(max_error),
        ..query(promql, interval_ms)
    }
}

/// `queries` through Stage 1 and Stage 2 as the facade runs them, over the
/// complete pane-width domain, metric `m` with 100 series sampled every
/// second.
pub(crate) fn through_stages(queries: &[Query<'_>], raw_data_retained: bool) -> Fixture {
    use asap_logical_asap_optimization::pass1::logical_memo_candidates::generate_per_computation_logical_memo_candidates;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::logical_memo_candidates::{
        generate_logical_asap_memo_candidates, Pass2RoundState,
    };
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{
        Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
    };
    let target = |query: &Query<'_>| match query.max_error {
        Some(error) => AccuracyTarget::max_error(error).failure_probability(0.01),
        None => AccuracyTarget::exact(),
    };
    let roots: Vec<QueryRoot> = queries
        .iter()
        .map(|query| QueryRoot::Operator(lower_promql(query.promql, target(query))))
        .collect();
    let entries: Vec<QueryWorkloadEntry> = queries
        .iter()
        .map(|query| {
            let mut entry = entry_with(
                target(query),
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                    query.interval_ms,
                ))),
            );
            entry.predictability = Predictability::Predictable { known_at: None };
            entry.requirements.max_latency_ms = query.max_latency_ms;
            entry
        })
        .collect();
    let mut data = DataWorkload::sampled_every(["m".to_string()], DurationMs(1_000));
    data.sources.get_mut("m").expect("m").series_count = Some(100);
    let indexed: Vec<(usize, QueryRoot<Rc<OperatorNode>>)> =
        roots.iter().cloned().enumerate().collect();
    let per_computation =
        generate_per_computation_candidates(&DagOfLogicalMemoGroups::from_roots(&indexed)).unwrap();
    let pass1 = generate_per_computation_logical_memo_candidates(
        DagOfLogicalMemoGroups::from_roots(&indexed),
        per_computation,
        &entries,
        &data,
        None,
    )
    .unwrap();
    let mut round = Pass2RoundState::new(pass1);
    let logical = generate_logical_asap_memo_candidates(
        &mut round,
        &entries,
        PaneWidthDomain::Complete,
        None,
    )
    .unwrap();
    let physical =
        asap_physical_asap_optimization::catalog::generate_physical_asap_memo_candidates(
            &logical,
            &entries,
            &data,
            false,
            raw_data_retained,
            None,
        )
        .unwrap();
    Fixture {
        physical,
        roots,
        entries,
        data,
        query_time_results_retained: false,
    }
}

/// What one checked solve gives.
struct Checked {
    solution: SelectionMilpSolution,
    plan: PlanCost,
}

/// `fixture`'s class MILP under `cost_inputs`, with `restrict` adding rows
/// first: solved, decoded, realized by Stage 2 and costed by
/// `asap_cost::plan_cost`. The MILP objective plus the request fees must
/// equal `plan_cost`'s cost.
fn solve_and_check(
    fixture: &Fixture,
    cost_inputs: &CostInputs,
    restrict: impl FnOnce(&mut SelectionMilp<'_>),
) -> Result<Checked, SelectionMilpError> {
    solve_and_check_by(fixture, cost_inputs, restrict, None)
}

/// [`solve_and_check`] with HiGHS's time limit `limit`.
fn solve_and_check_by(
    fixture: &Fixture,
    cost_inputs: &CostInputs,
    restrict: impl FnOnce(&mut SelectionMilp<'_>),
    limit: Option<Duration>,
) -> Result<Checked, SelectionMilpError> {
    let inputs = prepare_milp_inputs(
        &fixture.physical,
        &fixture.entries,
        &fixture.data,
        cost_inputs,
    );
    let mut milp = build_selection_milp(
        &fixture.physical,
        &inputs,
        &fixture.entries,
        cost_inputs,
        SelectionMilpOptions {
            enforce_accuracy: true,
        },
    )?;
    for reason in milp.inexact.keys() {
        assert!(
            !reason.starts_with("usage: a producer"),
            "usage-table gap: {:?}",
            milp.inexact
        );
    }
    restrict(&mut milp);
    let solution = solve_selection_milp(milp, limit.map(|limit| Instant::now() + limit), 0)?;
    let selected = crate::milp::decode::decode(&fixture.physical, &solution);
    selected
        .validate(&fixture.physical)
        .expect("the decoded selection is valid");
    let realizer = asap_physical_asap_optimization::catalog::Stage2AssignmentRealizer {
        original_roots: &fixture.roots,
        entries: &fixture.entries,
        data: &fixture.data,
        query_time_results_retained: fixture.query_time_results_retained,
    };
    let realized = realizer
        .realize(&fixture.physical, &selected)
        .expect("the realizer accepts the decoded plan");
    let dag = &realized.plan.dag;
    let recurrence: Vec<_> = fixture
        .entries
        .iter()
        .map(|entry| entry.recurrence.clone())
        .collect();
    let ranges: Vec<_> = fixture
        .entries
        .iter()
        .map(|entry| entry.range_evaluation)
        .collect();
    let (plan, _) = plan_cost(
        dag,
        &node_map(dag),
        &asap_cost::kept_pane_chains(dag),
        &recurrence,
        &ranges,
        &fixture.data,
        cost_inputs,
        cost_inputs.frozen_calibration_factor.unwrap_or(1.0),
    )
    .expect("plan_cost costs the plan");
    let expected = plan.cost_per_second.expect("cost model 1") - solution.request_cost_per_second;
    let tolerance = 1e-6 * expected.abs().max(1e-12);
    assert!(
        (solution.cost_per_second - expected).abs() <= tolerance,
        "MILP objective {:e} != plan_cost {:e} (without {:e} of request fees); per node: {:#?}",
        solution.cost_per_second,
        expected,
        solution.request_cost_per_second,
        plan.per_node
    );
    Ok(Checked { solution, plan })
}

pub(crate) fn cost_inputs() -> CostInputs {
    CostInputs {
        frozen_calibration_factor: Some(1.0),
        ..CostInputs::default()
    }
}

/// The classes of `milp` whose first copy satisfies `wanted`.
fn classes_where(
    milp: &SelectionMilp<'_>,
    wanted: impl Fn(&asap_types::stages::PhysicalCandidate, &OperatorNode) -> bool,
) -> Vec<PhysicalCandidateId> {
    (0..milp.physical.candidates.len())
        .map(PhysicalCandidateId)
        .filter(|&class| {
            let copy = class;
            wanted(
                &milp.physical.candidates[copy.0],
                milp.physical.physical_node(copy),
            )
        })
        .collect()
}

/// Adds `Σ x[i] ≥ 1` over the instances of `classes`: one of them runs.
fn require_one_of(milp: &mut SelectionMilp<'_>, classes: &[PhysicalCandidateId]) {
    let sum: Expression = classes
        .iter()
        .flat_map(|class| {
            milp.instances_of_candidate
                .get(class)
                .cloned()
                .unwrap_or_default()
        })
        .map(|instance| milp.active[instance])
        .sum();
    milp.builder.row("test", constraint!(sum >= 1.0));
}

/// Adds `x[i] = 0` for every instance of `classes`.
fn forbid(milp: &mut SelectionMilp<'_>, classes: &[PhysicalCandidateId]) {
    for class in classes {
        for &instance in milp.instances_of_candidate.get(class).into_iter().flatten() {
            let active = milp.active[instance];
            milp.builder.row("test", constraint!(active <= 0.0));
        }
    }
}

// ---------------------------------------------------------------------------
// Cost terms equal `plan_cost`
// ---------------------------------------------------------------------------

/// The classes whose first copy is a pane chain or runs at ingestion time.
fn pane_chains_and_ingestion(milp: &SelectionMilp<'_>) -> Vec<PhysicalCandidateId> {
    classes_where(milp, |candidate, node| {
        candidate.timing == asap_types::ir::properties::ExecutionTiming::IngestionTime
            || matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. }))
    })
}

/// The classes whose first copy runs `kind` at `materialization`.
fn classes_of(
    milp: &SelectionMilp<'_>,
    kind: &str,
    materialization: asap_types::physical::Materialization,
) -> Vec<PhysicalCandidateId> {
    classes_where(milp, |candidate, node| {
        node.operator.kind_name() == kind && candidate.materialization == materialization
    })
}

/// `sum(m)` repeating every minute and `sum(m) + max(m)` every 30 s: the
/// second query does more work, so its evaluations take longer.
pub(crate) fn shared_sum() -> Fixture {
    through_stages(
        &[query("sum(m)", 60_000), query("sum(m) + max(m)", 30_000)],
        true,
    )
}

// The optimum's objective equals `plan_cost` of the plan it decodes to.
#[test]
fn the_optimum_costs_what_plan_cost_says() {
    let checked = solve_and_check(&shared_sum(), &cost_inputs(), |_| {}).unwrap();
    assert_eq!(checked.solution.status, SelectionMilpStatus::Optimal);
}

// Held memory: a `MatAtQuery` result both queries read is held for the
// slower reader's latency, once per execution at their combined rate.
#[test]
fn held_memory_of_a_result_readers_of_different_latency_share() {
    use asap_types::physical::{Materialization, Medium};
    let fixture = shared_sum();
    let checked = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let shared: Vec<PhysicalCandidateId> = classes_where(milp, |candidate, _| {
            candidate.materialization == Materialization::MatAtQuery(Medium::Memory)
        })
        .into_iter()
        .filter(|class| milp.inputs.queries_reaching[class.0].len() == 2)
        .collect();
        assert!(!shared.is_empty());
        require_one_of(milp, &shared);
    })
    .unwrap();
    let latencies: Vec<f64> = checked
        .plan
        .usage
        .latency_per_query
        .iter()
        .map(|latency| latency.total_ms())
        .collect();
    assert!(latencies[0] != latencies[1], "{latencies:?}");
    assert!(checked
        .plan
        .per_node
        .values()
        .any(|node| node.detail.contains("holds")));
}

/// `sum_over_time(m[5m])` and `sum_over_time(m[1h])` over raw data, with
/// pane chains and ingestion-time states left out: every plan scans raw
/// data at query time.
pub(crate) fn two_windows() -> Fixture {
    through_stages(
        &[
            query("sum_over_time(m[5m])", 60_000),
            query("sum_over_time(m[1h])", 60_000),
        ],
        true,
    )
}

// Raw retention: with one scan per window, the source keeps the longer
// window's raw data once.
#[test]
fn raw_retention_keeps_the_longest_window() {
    let checked = solve_and_check(&two_windows(), &cost_inputs(), |milp| {
        let excluded = pane_chains_and_ingestion(milp);
        forbid(milp, &excluded);
    })
    .unwrap();
    assert!(checked
        .plan
        .per_node
        .values()
        .any(|node| node.detail.contains("of raw data on disk")));
}

// Link-dependent usage: when both windows read one scan instance, it
// scans the hour, and the 5-minute reader's usage is the one over an
// hour of rows (its terms sit on the read and on the scan's window level).
#[test]
fn a_reader_of_a_shared_scan_is_costed_at_the_scan_window() {
    use asap_types::physical::Materialization;
    let checked = solve_and_check(&two_windows(), &cost_inputs(), |milp| {
        let excluded = pane_chains_and_ingestion(milp);
        forbid(milp, &excluded);
        let scans = classes_of(milp, "Scan", Materialization::NotMaterialized);
        let instances: Expression = scans
            .iter()
            .flat_map(|class| {
                milp.instances_of_candidate
                    .get(class)
                    .cloned()
                    .unwrap_or_default()
            })
            .map(|instance| milp.active[instance])
            .sum();
        milp.builder.row("test", constraint!(instances <= 1.0));
    })
    .unwrap();
    let fixture = two_windows();
    let scans: Vec<_> = checked
        .solution
        .instances
        .iter()
        .filter(|(class, _, _)| {
            fixture.physical.physical_node(*class).operator.kind_name() == "Scan"
        })
        .collect();
    assert_eq!(scans.len(), 1);
    assert!(checked
        .solution
        .size
        .families
        .contains_key("t_producers (AND)"));
}

// A query over its latency bound: with the bound below the fastest plan's
// latency there is no plan; at the optimum's latency, the optimum stays.
#[test]
fn a_plan_over_a_latency_bound_is_infeasible() {
    let fixture = shared_sum();
    let optimum = solve_and_check(&fixture, &cost_inputs(), |_| {}).unwrap();
    let latency = optimum.plan.usage.latency_per_query[1].total_ms();
    let mut bounded = shared_sum();
    bounded.entries[1].requirements.max_latency_ms = Some(latency * (1.0 + 1e-9));
    let at_bound = solve_and_check(&bounded, &cost_inputs(), |_| {}).unwrap();
    assert!(at_bound.plan.usage.latency_per_query[1].total_ms() <= latency * (1.0 + 1e-9));
    let mut below = shared_sum();
    below.entries[1].requirements.max_latency_ms = Some(latency * 0.999);
    match solve_and_check(&below, &cost_inputs(), |_| {}) {
        Ok(faster) => assert!(faster.plan.usage.latency_per_query[1].total_ms() <= latency * 0.999),
        Err(error) => assert!(matches!(
            error,
            SelectionMilpError::NoPlanForQuery { query: 1, .. }
        )),
    }
    let mut tight = shared_sum();
    tight.entries[1].requirements.max_latency_ms = Some(1e-9);
    assert!(matches!(
        solve_and_check(&tight, &cost_inputs(), |_| {}).err(),
        Some(SelectionMilpError::NoPlanForQuery { query: 1, .. })
    ));
}

// Symmetry breaking: instance `j + 1` of a class is active only with
// instance `j`.
#[test]
fn instances_activate_in_order() {
    let fixture = shared_sum();
    let costs = cost_inputs();
    let inputs = prepare_milp_inputs(&fixture.physical, &fixture.entries, &fixture.data, &costs);
    let options = SelectionMilpOptions {
        enforce_accuracy: true,
    };
    let milp = build_selection_milp(
        &fixture.physical,
        &inputs,
        &fixture.entries,
        &costs,
        options,
    )
    .unwrap();
    let (class, of_class) = milp
        .instances_of_candidate
        .iter()
        .find(|(_, instances)| instances.len() >= 2)
        .map(|(class, instances)| (*class, instances.clone()))
        .expect("a class with two instances");
    let mut milp = milp;
    let (first, second) = (milp.active[of_class[0]], milp.active[of_class[1]]);
    milp.builder.row("test", constraint!(first <= 0.0));
    milp.builder.row("test", constraint!(second >= 1.0));
    assert_eq!(
        solve_selection_milp(milp, None, 0).err(),
        Some(SelectionMilpError::Infeasible),
        "{class:?}"
    );
    let optimum = solve_and_check(&fixture, &costs, |_| {}).unwrap();
    let mut active: BTreeMap<PhysicalCandidateId, Vec<usize>> = BTreeMap::new();
    for (class, index, _) in &optimum.solution.instances {
        active.entry(*class).or_default().push(*index);
    }
    for indexes in active.values() {
        assert_eq!(*indexes, (0..indexes.len()).collect::<Vec<_>>());
    }
}

// Cost models 2 and 3 are not encoded.
#[test]
fn cost_models_2_and_3_are_unsupported() {
    use asap_cost::{ClusterLimits, ClusterObjective, CostModel};
    let fixture = shared_sum();
    for model in [
        CostModel::ProvisionedCapacity(ResourceUnitCost::illustrative()),
        CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: u64::MAX,
            disk_bytes: u64::MAX,
            cpu_cores: None,
            objective: ClusterObjective::LowestLatency,
        }),
    ] {
        let costs = CostInputs {
            cost_model: model,
            ..cost_inputs()
        };
        let inputs =
            prepare_milp_inputs(&fixture.physical, &fixture.entries, &fixture.data, &costs);
        let options = SelectionMilpOptions {
            enforce_accuracy: true,
        };
        assert!(matches!(
            build_selection_milp(
                &fixture.physical,
                &inputs,
                &fixture.entries,
                &costs,
                options
            )
            .err(),
            Some(SelectionMilpError::UnsupportedCostModel(_))
        ));
    }
}

// Accuracy rows: under `parts = 2` an approximate numerator and an
// approximate denominator compose; an approximate denominator sized for
// the whole budget (`parts = 1`) is never read.
#[test]
fn accuracy_rows_admit_a_ratio_at_two_parts_and_reject_a_lone_denominator() {
    let fixture = through_stages(
        &[approximate(
            "quantile_over_time(0.5, m[5m]) / quantile_over_time(0.9, m[5m])",
            60_000,
            0.01,
        )],
        true,
    );
    let at = |weight: AccuracyWeight, parts: usize| {
        move |milp: &SelectionMilp<'_>| -> Vec<PhysicalCandidateId> {
            milp.inputs.accuracy_weights[0]
                .iter()
                .filter(|(class, known)| {
                    **known == weight
                        && milp.inputs.candidate_data[class.0].accuracy_budget_parts == parts
                })
                .map(|(class, _)| *class)
                .collect()
        }
    };
    let admitted = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let numerators = at(AccuracyWeight::One, 2)(milp);
        let denominators = at(AccuracyWeight::Two, 2)(milp);
        require_one_of(milp, &numerators);
        require_one_of(milp, &denominators);
    });
    assert!(admitted.is_ok(), "{:?}", admitted.err());
    let lone = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let denominators = at(AccuracyWeight::Two, 1)(milp);
        assert!(!denominators.is_empty());
        require_one_of(milp, &denominators);
    });
    assert_eq!(lone.err(), Some(SelectionMilpError::Infeasible));
}

// The warm start is a feasible plan with no sharing: with every binary
// fixed to it, the model is feasible and costs what `plan_cost` says.
#[test]
fn the_warm_start_is_a_feasible_plan() {
    for fixture in [shared_sum(), two_windows()] {
        solve_and_check(&fixture, &cost_inputs(), |milp| {
            let start: HashMap<Variable, f64> = crate::milp::warm_start::warm_start(milp)
                .expect("a plan for every query")
                .into_iter()
                .collect();
            for binary in milp.builder.binaries.clone() {
                let value = start.get(&binary).copied().unwrap_or(0.0);
                milp.builder.row("test", constraint!(binary == value));
            }
        })
        .unwrap();
    }
}

// A deadline yields a plan: HiGHS starts from the warm start, so even a
// short limit ends with a validated incumbent, never "no plan".
#[test]
fn a_deadline_yields_a_plan() {
    let fixture = two_windows();
    let checked = solve_and_check_by(
        &fixture,
        &cost_inputs(),
        |_| {},
        Some(Duration::from_millis(200)),
    )
    .unwrap();
    assert!(matches!(
        checked.solution.status,
        SelectionMilpStatus::Optimal | SelectionMilpStatus::TimeLimit
    ));
}
