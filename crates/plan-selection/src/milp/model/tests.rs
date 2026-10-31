use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::groups::{prepare_milp_inputs, AccuracyWeight};
use crate::milp::decode::{decode, ChosenCandidates};
use asap_cost::{node_map, plan_cost, PlanCost, ResourceUnitCost};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::physical::{Materialization, Medium};
use asap_types::stages::{AssignmentRealizer, DagOfSelectedCandidates, PhysicalCandidate};
use asap_types::workload::{DataWorkload, DurationMs, RangeEvaluation};

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
/// ms, its relative error target (`None`: exact), its latency bound and
/// its range evaluation.
#[derive(Clone, Copy)]
pub(crate) struct Query<'a> {
    promql: &'a str,
    interval_ms: u32,
    max_error: Option<f64>,
    max_latency_ms: Option<f64>,
    range_evaluation: Option<RangeEvaluation>,
}

pub(crate) fn query(promql: &str, interval_ms: u32) -> Query<'_> {
    Query {
        promql,
        interval_ms,
        max_error: None,
        max_latency_ms: None,
        range_evaluation: None,
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
    through_stages_with(queries, raw_data_retained, false)
}

/// [`through_stages`] over metric `m` declared non-negative (so relative
/// errors compose through quotients, and Pass 1 keeps DDSketch).
pub(crate) fn through_stages_non_negative(queries: &[Query<'_>]) -> Fixture {
    through_stages_over(queries, true, false, true)
}

/// [`through_stages`], with rows arriving (100 per second) when
/// `ingesting`, so Stage 2 offers ingestion-time candidates.
pub(crate) fn through_stages_with(
    queries: &[Query<'_>],
    raw_data_retained: bool,
    ingesting: bool,
) -> Fixture {
    through_stages_over(queries, raw_data_retained, ingesting, false)
}

/// The fixture builder behind [`through_stages`].
fn through_stages_over(
    queries: &[Query<'_>],
    raw_data_retained: bool,
    ingesting: bool,
    values_non_negative: bool,
) -> Fixture {
    use asap_logical_asap_optimization::generate_per_computation_logical_memo_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_logical_asap_optimization::PaneWidthDomain;
    use asap_logical_asap_optimization::{generate_logical_asap_memo_candidates, Pass2RoundState};
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
            entry.range_evaluation = query.range_evaluation;
            entry
        })
        .collect();
    let mut data = DataWorkload::sampled_every(["m".to_string()], DurationMs(1_000));
    data.sources.get_mut("m").expect("m").series_count = Some(100);
    data.sources.get_mut("m").expect("m").values_non_negative = values_non_negative;
    if ingesting {
        data.arrival = asap_types::workload::DataArrival::Ingesting {
            rows_per_second: 100.0,
        };
    }
    let targets: Vec<AccuracyTarget> = queries.iter().map(target).collect();
    let pass1 = generate_per_computation_logical_memo_candidates(
        asap_logical_asap_optimization::generate_dag_of_logical_memo_groups(&roots),
        &targets,
        &asap_accuracy::BuiltinAccuracyModel,
        &asap_accuracy::NoAccuracyEvidence,
        &data,
        None,
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
    let physical = asap_physical_asap_optimization::generate_physical_asap_memo_candidates(
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
pub(crate) struct Checked {
    pub(crate) solution: SelectionMilpSolution,
    pub(crate) plan: PlanCost,
    pub(crate) dag: DagOfSelectedCandidates,
}

/// `fixture`'s MILP under `cost_inputs`, with `restrict` adding rows first:
/// solved, decoded, realized by Stage 2 and costed by
/// `asap_cost::plan_cost`. At `Optimal` the objective plus the request fees
/// must equal `plan_cost`'s cost; at the deadline be at least it.
pub(crate) fn solve_and_check(
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
    assert!(milp.inexact.is_empty(), "inexact terms: {:?}", milp.inexact);
    restrict(&mut milp);
    let solution = solve_selection_milp(milp, limit.map(|limit| Instant::now() + limit), 0)?;
    let dag = decode(
        &fixture.physical,
        &inputs,
        &ChosenCandidates {
            chosen: solution.chosen.clone(),
            selected: solution.selected.clone(),
            slot_reads: solution.slot_reads.clone(),
        },
    )
    .expect("the solution decodes");
    dag.validate(&fixture.physical)
        .expect("the decoded selection is valid");
    let realizer = asap_physical_asap_optimization::Stage2AssignmentRealizer {
        original_roots: &fixture.roots,
        entries: &fixture.entries,
        data: &fixture.data,
        query_time_results_retained: fixture.query_time_results_retained,
    };
    let realized = realizer
        .realize(&fixture.physical, &dag)
        .expect("the realizer accepts the decoded plan");
    let plan_dag = &realized.plan.dag;
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
        plan_dag,
        &node_map(plan_dag),
        &asap_cost::kept_pane_chains(plan_dag),
        &recurrence,
        &ranges,
        &fixture.data,
        cost_inputs,
        cost_inputs.frozen_calibration_factor.unwrap_or(1.0),
    )
    .expect("plan_cost costs the plan");
    let expected = plan.cost_per_second.expect("cost model 1") - solution.request_cost_per_second;
    let tolerance = 1e-6 * expected.abs().max(1e-12);
    match solution.status {
        SelectionMilpStatus::Optimal => assert!(
            (solution.cost_per_second - expected).abs() <= tolerance,
            "MILP objective {:e} != plan_cost {:e} (without {:e} of request fees); per node: {:#?}",
            solution.cost_per_second,
            expected,
            solution.request_cost_per_second,
            plan.per_node
        ),
        SelectionMilpStatus::TimeLimit => assert!(
            expected <= solution.cost_per_second + tolerance,
            "plan_cost {expected:e} above the incumbent's objective {:e}",
            solution.cost_per_second
        ),
    }
    Ok(Checked {
        solution,
        plan,
        dag,
    })
}

pub(crate) fn cost_inputs() -> CostInputs {
    CostInputs {
        frozen_calibration_factor: Some(1.0),
        ..CostInputs::default()
    }
}

/// The `aws-fargate-us-east-1` cost model of the memo examples.
fn aws_fargate_cost_inputs() -> CostInputs {
    let defaults = cost_inputs();
    CostInputs {
        cost_model: asap_cost::CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
            defaults.resource_speed,
        )),
        ..defaults
    }
}

/// The candidates of `milp` that satisfy `wanted`.
fn candidates_where(
    milp: &SelectionMilp<'_>,
    wanted: impl Fn(&PhysicalCandidate, &OperatorNode) -> bool,
) -> Vec<PhysicalCandidateId> {
    (0..milp.physical.candidates.len())
        .map(PhysicalCandidateId)
        .filter(|&candidate| {
            wanted(
                &milp.physical.candidates[candidate.0],
                milp.physical.physical_node(candidate),
            )
        })
        .collect()
}

/// Adds `Σ x[m] ≥ 1` over the shareable `candidates`: one is selected.
fn require_one_of(milp: &mut SelectionMilp<'_>, candidates: &[PhysicalCandidateId]) {
    let sum: Expression = candidates
        .iter()
        .filter_map(|candidate| milp.selected.get(candidate).copied())
        .map(Expression::from)
        .sum();
    milp.builder.row("test", constraint!(sum >= 1.0));
}

/// Adds `x[m] = 0` and `p[ctx, c] = 0` everywhere for `candidates`.
fn forbid(milp: &mut SelectionMilp<'_>, candidates: &[PhysicalCandidateId]) {
    for candidate in candidates {
        if let Some(&selected) = milp.selected.get(candidate) {
            milp.builder.row("test", constraint!(selected <= 0.0));
        }
        for context in 0..milp.choice.len() {
            if let Some(&choice) = milp.choice[context].get(candidate) {
                milp.builder.row("test", constraint!(choice <= 0.0));
            }
        }
    }
}

/// The candidates that are pane chains or run at ingestion time.
fn pane_chains_and_ingestion(milp: &SelectionMilp<'_>) -> Vec<PhysicalCandidateId> {
    candidates_where(milp, |candidate, node| {
        candidate.timing == ExecutionTiming::IngestionTime
            || matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. }))
    })
}

/// How many nodes of `dag` run `candidate`.
fn runs_of(dag: &DagOfSelectedCandidates, candidate: PhysicalCandidateId) -> usize {
    dag.candidate_of_node
        .iter()
        .filter(|&&c| c == candidate)
        .count()
}

/// `sum(m)` repeating every minute and `sum(m) + max(m)` every 30 s: the
/// second query does more work, so its evaluations take longer.
pub(crate) fn shared_sum() -> Fixture {
    through_stages(
        &[query("sum(m)", 60_000), query("sum(m) + max(m)", 30_000)],
        true,
    )
}

/// `sum_over_time(m[5m])` and `sum_over_time(m[1h])` over raw data.
pub(crate) fn two_windows() -> Fixture {
    through_stages(
        &[
            query("sum_over_time(m[5m])", 60_000),
            query("sum_over_time(m[1h])", 60_000),
        ],
        true,
    )
}

// ---------------------------------------------------------------------------
// Cost terms equal `plan_cost` (§3)
// ---------------------------------------------------------------------------

// The optimum's objective equals `plan_cost` of the plan it decodes to.
#[test]
fn the_optimum_costs_what_plan_cost_says() {
    for costs in [cost_inputs(), aws_fargate_cost_inputs()] {
        let checked = solve_and_check(&shared_sum(), &costs, |_| {}).unwrap();
        assert_eq!(checked.solution.status, SelectionMilpStatus::Optimal);
    }
}

// Held memory and schedule groups: a `MatAtQuery(Memory)` result both
// queries read (every 60 s and every 30 s) is held until the slower reader
// finishes, once per execution at the schedule groups' rates.
#[test]
fn held_memory_of_a_result_readers_of_different_latency_share() {
    let fixture = shared_sum();
    let checked = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let shared: Vec<PhysicalCandidateId> = candidates_where(milp, |candidate, _| {
            candidate.materialization == Materialization::MatAtQuery(Medium::Memory)
        })
        .into_iter()
        .filter(|&candidate| milp.queries_reaching(candidate).len() == 2)
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

// A scan read through two offsets in one query evaluation: the scan run
// reads the union of the two windows, and the MILP charges exactly that
// union (`δ` over the window kinds read), as `plan_cost` does.
#[test]
fn a_scan_read_through_two_offsets_is_costed_at_their_union() {
    let fixture = through_stages(
        &[query(
            "sum_over_time(m[5m]) + sum_over_time(m[5m] offset 1h)",
            60_000,
        )],
        true,
    );
    let checked = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let excluded = pane_chains_and_ingestion(milp);
        forbid(milp, &excluded);
    })
    .unwrap();
    assert!(
        checked
            .solution
            .size
            .families
            .contains_key("δ (scan window sets)"),
        "{:?}",
        checked.solution.size.families.keys().collect::<Vec<_>>()
    );
}

// A kept pane chain whose readers have different range-evaluation spans:
// it keeps the panes of the longest span, charged by retention levels.
#[test]
fn a_kept_pane_chain_keeps_the_longest_readers_span() {
    let span = |span_ms| {
        Some(RangeEvaluation {
            step_ms: 60_000,
            span_ms,
        })
    };
    let fixture = through_stages_with(
        &[
            Query {
                range_evaluation: span(600_000),
                ..query("sum_over_time(m[1h])", 60_000)
            },
            Query {
                range_evaluation: span(3_600_000),
                ..query("sum_over_time(m[1h])", 60_000)
            },
        ],
        false,
        true,
    );
    let checked = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let chains = candidates_where(milp, |candidate, node| {
            matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. }))
                && matches!(
                    candidate.materialization,
                    Materialization::MatAtIngestion(_)
                )
        });
        require_one_of(milp, &chains);
    })
    .unwrap();
    assert!(
        checked
            .solution
            .size
            .families
            .contains_key("π (pane retention levels)"),
        "{:?}",
        checked.solution.size.families.keys().collect::<Vec<_>>()
    );
}

// A `NotMaterialized` input of a `MatAtQuery` candidate both queries read
// (`rate(m[5m])` under `sum` and `max`, one schedule group) runs once, in
// the candidate's context, and is costed as `plan_cost` costs it.
#[test]
fn a_shared_results_not_materialized_input_runs_once() {
    let fixture = through_stages(
        &[
            query("sum(rate(m[5m]))", 60_000),
            query("max(rate(m[5m]))", 60_000),
        ],
        true,
    );
    let mut shared = Vec::new();
    let checked = solve_and_check(&fixture, &cost_inputs(), |milp| {
        shared = candidates_where(milp, |candidate, _| {
            matches!(candidate.materialization, Materialization::MatAtQuery(_))
        })
        .into_iter()
        .filter(|&m| milp.queries_reaching(m).len() == 2)
        .filter(|&m| {
            let context = milp.context_of(m).expect("a context");
            milp.choice[context]
                .keys()
                .any(|&c| milp.computes(context, c))
        })
        .collect();
        assert!(!shared.is_empty(), "a shared result over a computed input");
        require_one_of(milp, &shared);
    })
    .unwrap();
    let m = *checked
        .solution
        .selected
        .iter()
        .find(|m| shared.contains(m))
        .expect("selected");
    assert_eq!(runs_of(&checked.dag, m), 1);
    let node = checked
        .dag
        .candidate_of_node
        .iter()
        .position(|&c| c == m)
        .expect("a node");
    for edge in checked.dag.edges.iter().filter(|e| e.consumer.0 == node) {
        let input = checked.dag.candidate_of_node[edge.producer.0];
        let readers = checked
            .dag
            .edges
            .iter()
            .filter(|e| e.producer == edge.producer)
            .count();
        assert_eq!(readers, 1, "m's input {input:?} is read by m alone");
    }
}

// One ingestion scan read by sketches of two configurations (two quantile
// targets) runs once in `I` and is costed once.
#[test]
fn one_ingestion_scan_shared_by_two_sketch_configurations_is_charged_once() {
    let fixture = through_stages_with(
        &[
            approximate("quantile_over_time(0.5, m[5m])", 60_000, 0.01),
            approximate("quantile_over_time(0.9, m[5m])", 60_000, 0.05),
        ],
        true,
        true,
    );
    let checked = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let sketches = candidates_where(milp, |candidate, node| {
            candidate.timing == ExecutionTiming::IngestionTime
                && candidate.materialization != Materialization::NotMaterialized
                && matches!(
                    node.asap(),
                    Some(asap_types::ir::ASAPOp::SummaryAgg { .. })
                        | Some(asap_types::ir::ASAPOp::PaneChain { .. })
                )
        });
        let sum: Expression = sketches
            .iter()
            .filter_map(|c| milp.selected.get(c).copied())
            .map(Expression::from)
            .sum();
        milp.builder.row("test", constraint!(sum >= 2.0));
        // Every query-time scan is excluded: both read ingestion state.
        let scans = candidates_where(milp, |candidate, node| {
            candidate.timing == ExecutionTiming::QueryTime && node.operator.kind_name() == "Scan"
        });
        forbid(milp, &scans);
    })
    .unwrap();
    let ingestion_scans: Vec<usize> = (0..checked.dag.candidate_of_node.len())
        .filter(|&node| {
            let candidate = checked.dag.candidate_of_node[node];
            fixture.physical.candidates[candidate.0].timing == ExecutionTiming::IngestionTime
                && fixture
                    .physical
                    .physical_node(candidate)
                    .operator
                    .kind_name()
                    == "Scan"
        })
        .collect();
    assert_eq!(ingestion_scans.len(), 1, "{:?}", checked.dag);
}

// A stateless subtree read by two state nodes of one query evaluation
// (`avg` reads a sum and a count accumulator over one `q50/q50 + q90`
// subtree) is computed once: no candidate runs twice in the plan.
#[test]
fn a_stateless_subtree_two_state_nodes_read_runs_once() {
    let fixture = through_stages(
        &[query(
            "avg by (job) (quantile_over_time(0.5, m[5m]) / quantile_over_time(0.5, m[5m]) \
             + quantile_over_time(0.9, m[5m]))",
            60_000,
        )],
        true,
    );
    let checked = solve_and_check(&fixture, &cost_inputs(), |_| {}).unwrap();
    let mut runs: BTreeMap<PhysicalCandidateId, usize> = BTreeMap::new();
    for &candidate in &checked.dag.candidate_of_node {
        *runs.entry(candidate).or_default() += 1;
    }
    assert!(runs.values().all(|&n| n == 1), "{runs:?}");
}

// ---------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------

// "Used only if read": a query context may not select a shareable
// candidate no used reader there reads, so a phantom read cannot satisfy
// `MinimumQueryReaders`. With every reader of a two-reader `MatAtQuery`
// result in the second query's context forbidden, selecting it is
// infeasible.
#[test]
fn a_phantom_reader_does_not_satisfy_minimum_query_readers() {
    let fixture = shared_sum();
    let costs = cost_inputs();
    let inputs = prepare_milp_inputs(&fixture.physical, &fixture.entries, &fixture.data, &costs);
    let options = SelectionMilpOptions {
        enforce_accuracy: true,
    };
    let mut milp = build_selection_milp(
        &fixture.physical,
        &inputs,
        &fixture.entries,
        &costs,
        options,
    )
    .unwrap();
    let constrained: Vec<PhysicalCandidateId> = fixture
        .physical
        .reader_constraints
        .iter()
        .filter_map(|constraint| match constraint {
            ReaderConstraint::MinimumQueryReaders {
                candidate, minimum, ..
            } if *minimum >= 2 => Some(*candidate),
            _ => None,
        })
        .filter(|&m| milp.selected.contains_key(&m) && milp.choice[1].contains_key(&m))
        .collect();
    let m = *constrained.first().expect("a two-reader shareable result");
    let selected = milp.selected[&m];
    milp.builder.row("test", constraint!(selected >= 1.0));
    let readers: Vec<PhysicalCandidateId> = milp.readers[1][&m]
        .iter()
        .filter_map(|reader| match reader {
            Reader::Candidate(reader, _) => Some(*reader),
            _ => None,
        })
        .collect();
    assert!(
        !milp.readers[1][&m]
            .iter()
            .any(|reader| matches!(reader, Reader::Output(_))),
        "the result is not an output of the second query"
    );
    for reader in readers {
        let choice = milp.choice[1][&reader];
        milp.builder.row("test", constraint!(choice <= 0.0));
    }
    assert_eq!(
        solve_selection_milp(milp, None, 0).err(),
        Some(SelectionMilpError::Infeasible)
    );
}

// A `NotMaterialized` query-time candidate selected in two contexts is two
// nodes; a shareable candidate two queries read is one node.
#[test]
fn runs_decode_one_node_per_context_and_one_per_shareable_candidate() {
    let fixture = through_stages(
        &[query("sum(m)", 60_000), query("sum(m) * 2", 60_000)],
        true,
    );
    let unshared = solve_and_check(&fixture, &cost_inputs(), |milp| {
        let shared = candidates_where(milp, |candidate, _| {
            candidate.timing == ExecutionTiming::QueryTime
                && candidate.materialization != Materialization::NotMaterialized
        });
        forbid(milp, &shared);
        let ingestion = candidates_where(milp, |candidate, _| {
            candidate.timing == ExecutionTiming::IngestionTime
        });
        forbid(milp, &ingestion);
    })
    .unwrap();
    let mut runs: BTreeMap<PhysicalCandidateId, usize> = BTreeMap::new();
    for &candidate in &unshared.dag.candidate_of_node {
        *runs.entry(candidate).or_default() += 1;
    }
    assert!(runs.values().any(|&n| n == 2), "{runs:?}");
    let mut shared_result = Vec::new();
    let shared = solve_and_check(&fixture, &cost_inputs(), |milp| {
        shared_result = candidates_where(milp, |candidate, _| {
            candidate.materialization == Materialization::MatAtQuery(Medium::Memory)
        })
        .into_iter()
        .filter(|&m| milp.queries_reaching(m).len() == 2)
        .collect();
        require_one_of(milp, &shared_result);
    })
    .unwrap();
    let m = *shared
        .solution
        .selected
        .iter()
        .find(|m| shared_result.contains(m))
        .expect("selected");
    assert_eq!(runs_of(&shared.dag, m), 1);
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
    let mut tight = shared_sum();
    tight.entries[1].requirements.max_latency_ms = Some(1e-9);
    assert!(matches!(
        solve_and_check(&tight, &cost_inputs(), |_| {}).err(),
        Some(SelectionMilpError::Infeasible)
    ));
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

// Accuracy rows on `q0.5 / q0.9` over metric `m` declared non-negative,
// weights from the composition: a candidate unusable for the query (a
// DDSketch denominator sized for the whole target) is never depended on,
// and two at `parts = 2` are depended on together unless their pair is
// excluded. (Pass 1 already prunes KLL, whose rank error composes no
// guarantee through the quotient.)
#[test]
fn accuracy_rows_follow_the_composition_on_a_ratio() {
    let fixture = through_stages_non_negative(&[approximate(
        "quantile_over_time(0.5, m[5m]) / quantile_over_time(0.9, m[5m])",
        60_000,
        0.01,
    )]);
    let costs = cost_inputs();
    let inputs = prepare_milp_inputs(&fixture.physical, &fixture.entries, &fixture.data, &costs);
    let of = |wanted: &dyn Fn(&AccuracyWeight, usize) -> bool| -> Vec<PhysicalCandidateId> {
        inputs.accuracy_weights[0]
            .iter()
            .filter(|(class, weight)| {
                wanted(weight, inputs.candidate_data[class.0].accuracy_budget_parts)
            })
            .map(|(class, _)| *class)
            .collect()
    };
    let unusable = of(&|weight, _| matches!(weight, AccuracyWeight::Unusable(_)));
    let usable_at_two = of(&|weight, parts| *weight == AccuracyWeight::One && parts == 2);
    assert!(!unusable.is_empty() && usable_at_two.len() >= 2);
    let solve = |inputs: &MilpInputs, restrict: &dyn Fn(&mut SelectionMilp<'_>)| {
        let mut milp = build_selection_milp(
            &fixture.physical,
            inputs,
            &fixture.entries,
            &costs,
            SelectionMilpOptions {
                enforce_accuracy: true,
            },
        )
        .unwrap();
        restrict(&mut milp);
        solve_selection_milp(milp, None, 0).map(|_| ())
    };
    // Each candidate's runs: `x[m]`, or `p` in a context computing it.
    let runs_of = |milp: &SelectionMilp<'_>, candidates: &[PhysicalCandidateId]| -> Expression {
        let mut sum = Expression::from(0.0);
        for &candidate in candidates {
            if let Some(&selected) = milp.selected.get(&candidate) {
                sum += selected;
            }
            for context in 0..milp.choice.len() {
                if milp.computes(context, candidate) {
                    if let Some(&choice) = milp.choice[context].get(&candidate) {
                        sum += choice;
                    }
                }
            }
        }
        sum
    };
    assert_eq!(
        solve(&inputs, &|milp| {
            let sum = runs_of(milp, &unusable);
            milp.builder.row("test", constraint!(sum >= 1.0));
        })
        .err(),
        Some(SelectionMilpError::Infeasible)
    );
    let two_of = |milp: &mut SelectionMilp<'_>| {
        let sum = runs_of(milp, &usable_at_two);
        milp.builder.row("test", constraint!(sum >= 2.0));
    };
    assert!(solve(&inputs, &two_of).is_ok());
    let mut excluded = inputs.clone();
    for (index, &first) in usable_at_two.iter().enumerate() {
        for &second in &usable_at_two[index + 1..] {
            excluded.accuracy_excluded_pairs[0].push((first, second));
        }
    }
    assert_eq!(
        solve(&excluded, &two_of).err(),
        Some(SelectionMilpError::Infeasible)
    );
}

// The warm start is a feasible plan whose completion's objective equals
// what `plan_cost` says of it: fixing every binary to it, the solve
// returns the warm start's cost.
#[test]
fn the_warm_start_is_a_feasible_plan_costed_exactly() {
    for fixture in [shared_sum(), two_windows()] {
        let mut start_cost = None;
        let checked = solve_and_check(&fixture, &cost_inputs(), |milp| {
            let completion = crate::milp::completion::Completion::new(milp);
            let start =
                crate::milp::warm_start::warm_start(milp, &completion, None).expect("a warm start");
            start_cost = Some(start.cost_per_second);
            let values: HashMap<Variable, f64> = start.values.into_iter().collect();
            let mut fixed = Vec::new();
            for choices in &milp.choice {
                fixed.extend(choices.values().copied());
            }
            fixed.extend(milp.selected.values().copied());
            for variable in fixed {
                let value = values.get(&variable).copied().unwrap_or(0.0);
                milp.builder.row("test", constraint!(variable == value));
            }
        })
        .unwrap();
        let start_cost = start_cost.expect("costed");
        let tolerance = 1e-6 * start_cost.abs().max(1e-12);
        assert!(
            (checked.solution.cost_per_second - start_cost).abs() <= tolerance,
            "warm start {start_cost:e} != plan_cost {:e}",
            checked.solution.cost_per_second
        );
    }
}

// A deadline yields a plan: HiGHS starts from the warm start, so even no
// time ends with a validated incumbent, reported as the deadline.
#[test]
fn a_deadline_yields_the_warm_starts_plan() {
    let checked =
        solve_and_check_by(&two_windows(), &cost_inputs(), |_| {}, Some(Duration::ZERO)).unwrap();
    assert_eq!(checked.solution.status, SelectionMilpStatus::TimeLimit);
    let start = checked
        .solution
        .warm_start_cost_per_second
        .expect("the warm start was costed");
    assert!(checked.solution.cost_per_second <= start * (1.0 + 1e-9));
}

// RangeHeavy templates 7 and 4: the widened scan puts a `Project` in the
// memo group of the scan it reads. The group's roots are split by
// self-read depth, so the plan is acyclic, valid, accepted by the realizer
// and costed exactly.
#[test]
fn a_widened_scan_reading_its_own_memo_group_plans_exactly() {
    let fixture = through_stages(
        &[
            approximate("sum by (job) (sum_over_time(m[1h]))", 60_000, 0.05),
            approximate("sum_over_time(m[1h])", 60_000, 0.05),
        ],
        true,
    );
    let reads_own_group = (0..fixture.physical.candidates.len()).any(|candidate| {
        let group = fixture
            .physical
            .memo_group_of(PhysicalCandidateId(candidate));
        fixture.physical.candidates[candidate]
            .producer_candidates_per_input
            .iter()
            .any(|producers| {
                producers
                    .iter()
                    .any(|p| p.0 != candidate && fixture.physical.memo_group_of(*p) == group)
            })
    });
    assert!(
        reads_own_group,
        "the fixture has a root reading its own group"
    );
    for costs in [cost_inputs(), aws_fargate_cost_inputs()] {
        let checked = solve_and_check(&fixture, &costs, |_| {}).unwrap();
        assert_eq!(checked.solution.status, SelectionMilpStatus::Optimal);
    }
}
