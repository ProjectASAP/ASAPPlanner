//! The baseline MILP's variables and constraints (A)–(I), each on a small
//! memo built through `Memo::find_or_add_group` and `Memo::add_candidate`
//! with hand-made physical candidates, edges and costs $w_v$; and the
//! reference enumeration of the concepts doc's Appendix A against the MILP.
use std::collections::BTreeMap;

use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchParams,
    SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    parameter_list, CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId,
    LogicalASAPMemoCandidates, LogicalProperties, LogicalRequirements, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup, PhysicalASAPMemoCandidates, PhysicalCandidate,
    PhysicalCandidateId, PhysicalEdge, WindowLayout,
};
use asap_types::workload::{
    QueryRecurrence, QueryRequirements, QueryWorkloadEntry, RepeatedDemand, RepetitionInterval,
    TimestampMs,
};

use super::*;

const QT_NONE: (ExecutionTiming, Lifetime) = (ExecutionTiming::QueryTime, Lifetime::None);
const IT_ACROSS: (ExecutionTiming, Lifetime) = (
    ExecutionTiming::IngestionTime,
    Lifetime::AcrossEvaluations(StorageMedium::Memory),
);

fn add(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
    window_layout: Option<WindowLayout>,
) -> CandidateForMemoGroupId {
    let expression = CandidateExpression { operator };
    let logical_properties =
        LogicalProperties::new(&expression, parameters, window_layout).unwrap();
    let input_requirements =
        vec![LogicalRequirements::new(None); expression.operator.children().len()];
    memo.add_candidate(
        group,
        CandidateForMemoGroup {
            expression,
            logical_properties,
            input_requirements,
            fused_groups: Vec::new(),
            origin: OriginOfCandidateForMemoGroup::Pass1,
        },
    )
    .unwrap()
}

/// The running example's query A over one scan: a scan group (candidate
/// 0), a KLL state group with three sizes θ (candidates 1–3) and one of
/// 10 s tumbling windows (candidate 4), and the p95 readout group, the
/// query's root (candidate 5), every candidate at query time and not
/// materialized except the windowed state, at ingestion time and
/// materialized across evaluations. Every edge is admissible unless
/// `inadmissible` names its (producer, consumer, input).
fn example(inadmissible: &[(usize, usize, usize)]) -> PhysicalASAPMemoCandidates {
    let mut memo = Memo::default();
    let scan_operator = Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    });
    let scan_group = memo.find_or_add_group(scan_operator.clone()).unwrap();
    let scan = add(&mut memo, scan_group, scan_operator, None, None);
    let build = Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan_group,
        family: FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
    });
    let states = memo.find_or_add_group(build.clone()).unwrap();
    let list = parameter_list(SketchAlgorithm::Kll);
    let mut logical = vec![scan];
    for parameters in &list[..3] {
        logical.push(add(
            &mut memo,
            states,
            build.clone(),
            Some(parameters.clone()),
            None,
        ));
    }
    logical.push(add(
        &mut memo,
        states,
        build,
        Some(list[0].clone()),
        Some(WindowLayout::Tumbling { width_ms: 10_000 }),
    ));
    let readout_operator = Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: states,
        query: SketchStatistic::Quantile { q: 0.95 },
    });
    let readout_group = memo.find_or_add_group(readout_operator.clone()).unwrap();
    logical.push(add(&mut memo, readout_group, readout_operator, None, None));
    memo.add_query(QueryRoot::Operator(readout_group));
    let physical_candidates: Vec<PhysicalCandidate> = logical
        .iter()
        .enumerate()
        .map(|(id, &logical_candidate_id)| {
            let (timing, lifetime) = if id == 4 { IT_ACROSS } else { QT_NONE };
            PhysicalCandidate {
                logical_candidate_id,
                physical_properties: PhysicalProperties::new(timing, lifetime).unwrap(),
            }
        })
        .collect();
    let mut edges = Vec::new();
    // Grouped by (consumer, input).
    for state in 1..=4 {
        edges.push((0, state, 0));
    }
    for state in 1..=4 {
        edges.push((state, 5, 0));
    }
    let edges = edges
        .into_iter()
        .map(|(producer, consumer, input)| PhysicalEdge {
            producer_candidate_id: PhysicalCandidateId(producer),
            consumer_candidate_id: PhysicalCandidateId(consumer),
            input_index_of_consumer: input,
            satisfies_logical_requirement: !inadmissible.contains(&(producer, consumer, input)),
            satisfies_physical_requirement: true,
        })
        .collect();
    let query_roots = memo.query_roots.clone();
    PhysicalASAPMemoCandidates::new(
        LogicalASAPMemoCandidates {
            memo,
            edges: Vec::new(),
        },
        physical_candidates,
        query_roots,
        edges,
    )
}

/// One query every 10 s at `phase_ms`.
fn entry(phase_ms: u64) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Default::default(),
        requirements: QueryRequirements::default(),
        predictability: Default::default(),
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(10_000),
            evaluation_phase: TimestampMs(phase_ms),
        }),
        range_evaluation: None,
    }
}

fn solve(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    entries: &[QueryWorkloadEntry],
) -> Result<SolvedSelection, SelectionMilpError> {
    solve_selection(&SelectionInputs::new(physical, costs, entries))
}

/// The selected candidates, ascending.
fn selected(solved: &SolvedSelection) -> Vec<usize> {
    solved
        .selected_dag
        .candidate_of_node
        .iter()
        .map(|id| id.0)
        .collect()
}

/// (A), (B), (C)/(D) and the objective: the readout (C) reads exactly one
/// state (A), the cheapest, which is selected with the scan it reads (B);
/// the decoded DAG validates and its objective is the selected costs' sum.
#[test]
fn one_producer_per_input_the_cheapest_and_its_inputs_selected() {
    let physical = example(&[]);
    let costs = [
        Some(1.0),
        Some(5.0),
        Some(2.0),
        Some(9.0),
        Some(7.0),
        Some(1.0),
    ];
    let solved = solve(&physical, &costs, &[entry(0)]).unwrap();
    assert_eq!(selected(&solved), [0, 2, 5]);
    solved.selected_dag.validate(&physical).unwrap();
    assert_eq!(solved.selected_dag.edges.len(), 2);
    assert_eq!(solved.objective_per_second, 4.0);
    assert_eq!(
        solved.selected_dag.node_of_output_reference,
        [SelectedNodeId(2)]
    );
}

/// (G) and (H): $y$ exists only on admissible edges, so the cheapest state
/// is not chosen when its edge into the readout fails $\alpha^{\log}$.
#[test]
fn an_inadmissible_edge_is_never_selected() {
    let physical = example(&[(2, 5, 0)]);
    let costs = [
        Some(1.0),
        Some(5.0),
        Some(2.0),
        Some(9.0),
        Some(7.0),
        Some(1.0),
    ];
    let solved = solve(&physical, &costs, &[entry(0)]).unwrap();
    assert_eq!(selected(&solved), [0, 1, 5]);
}

/// A candidate `asap-cost` could not cost has $x_v = 0$; with no costable
/// root candidate, (C) is infeasible.
#[test]
fn an_uncostable_candidate_is_not_selected_and_an_uncostable_root_is_infeasible() {
    let physical = example(&[]);
    let mut costs = [Some(1.0), Some(5.0), None, Some(9.0), Some(7.0), Some(1.0)];
    assert_eq!(
        selected(&solve(&physical, &costs, &[entry(0)]).unwrap()),
        [0, 1, 5]
    );
    costs[5] = None;
    assert_eq!(
        solve(&physical, &costs, &[entry(0)]).unwrap_err(),
        SelectionMilpError::Infeasible
    );
}

/// (F): a candidate that costs nothing is still not selected when no edge
/// or query output uses it, so every decoded node is reached.
#[test]
fn a_free_candidate_nothing_reads_is_not_selected() {
    let physical = example(&[]);
    let costs = [
        Some(1.0),
        Some(5.0),
        Some(2.0),
        Some(0.0),
        Some(0.0),
        Some(1.0),
    ];
    // At phase 5 s the windowed state (candidate 4) cannot be read: free,
    // it is not selected; candidate 3, also free, is read.
    let solved = solve(&physical, &costs, &[entry(5_000)]).unwrap();
    assert_eq!(selected(&solved), [0, 3, 5]);
    solved.selected_dag.validate(&physical).unwrap();
}

/// A tumbling-window state materialized across evaluations, 10 s wide,
/// serves a query every 10 s at phase 0 but not at phase 5 s: its edge
/// gets no $y$ when the producer does not deliver on the consumer's
/// evaluations (`PhysicalCandidate::delivers_on`).
#[test]
fn a_windowed_state_is_read_only_by_queries_it_delivers_on() {
    let physical = example(&[]);
    let costs = [
        Some(10.0),
        Some(5.0),
        Some(6.0),
        Some(7.0),
        Some(1.0),
        Some(1.0),
    ];
    assert_eq!(
        selected(&solve(&physical, &costs, &[entry(0)]).unwrap()),
        [0, 4, 5]
    );
    assert_eq!(
        selected(&solve(&physical, &costs, &[entry(5_000)]).unwrap()),
        [0, 1, 5]
    );
}

/// (E): an edge closing a cycle (the readout feeding a state's input) is
/// cheaper than the scan, but the selection stays acyclic.
#[test]
fn a_cycle_is_never_selected() {
    let mut physical = example(&[]);
    let mut edges: Vec<PhysicalEdge> = physical.edges().map(|(_, edge)| edge).collect();
    edges.push(PhysicalEdge {
        producer_candidate_id: PhysicalCandidateId(5),
        consumer_candidate_id: PhysicalCandidateId(1),
        input_index_of_consumer: 0,
        satisfies_logical_requirement: true,
        satisfies_physical_requirement: true,
    });
    edges.sort_by_key(|edge| (edge.consumer_candidate_id, edge.input_index_of_consumer));
    physical = PhysicalASAPMemoCandidates::new(
        physical.logical_candidates.clone(),
        physical.physical_candidates.clone(),
        physical.query_roots.clone(),
        edges,
    );
    let costs = [
        Some(100.0),
        Some(1.0),
        Some(50.0),
        Some(50.0),
        Some(50.0),
        Some(1.0),
    ];
    let solved = solve(&physical, &costs, &[entry(0)]).unwrap();
    assert_eq!(selected(&solved), [0, 1, 5]);
    solved.selected_dag.validate(&physical).unwrap();
}

#[test]
fn strongly_connected_components_group_cycles_only() {
    let component = strongly_connected_components(5, [(0, 1), (1, 2), (2, 1), (3, 3)].into_iter());
    assert_eq!(component[1], component[2]);
    assert_ne!(component[0], component[1]);
    assert_ne!(component[3], component[4]);
    assert_ne!(component[0], component[4]);
}

/// The concepts doc's Appendix A over physical candidates: every DAG with
/// one admissible producer per input of every reached candidate, from
/// each root candidate, without cycles; each DAG's cost the sum of its
/// distinct candidates' $w_v$. Returns the cheapest cost.
fn reference_minimum(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    entries: &[QueryWorkloadEntry],
) -> Option<f64> {
    let inputs = SelectionInputs::new(physical, costs, entries);
    let edges = inputs.edges();
    let producers = |c: PhysicalCandidateId, i: usize| -> Vec<PhysicalCandidateId> {
        physical
            .admissible_edges_into(c, i)
            .iter()
            .map(|edge| &edges[edge.0])
            .filter(|edge| {
                inputs.delivers_to_consumer(edge) && costs[edge.producer_candidate_id.0].is_some()
            })
            .map(|edge| edge.producer_candidate_id)
            .collect()
    };
    fn reaches(
        dag: &BTreeMap<PhysicalCandidateId, Vec<PhysicalCandidateId>>,
        from: PhysicalCandidateId,
        to: PhysicalCandidateId,
    ) -> bool {
        from == to
            || dag
                .get(&from)
                .into_iter()
                .flatten()
                .any(|&next| reaches(dag, next, to))
    }
    fn expand(
        inputs: &SelectionInputs<'_>,
        producers: &dyn Fn(PhysicalCandidateId, usize) -> Vec<PhysicalCandidateId>,
        dag: BTreeMap<PhysicalCandidateId, Vec<PhysicalCandidateId>>,
        mut pending: Vec<PhysicalCandidateId>,
        best: &mut Option<f64>,
    ) {
        let Some(consumer) = pending.pop() else {
            let cost: f64 = dag
                .keys()
                .map(|v| inputs.cost_per_second[v.0].unwrap())
                .sum();
            *best = Some(best.map_or(cost, |known: f64| known.min(cost)));
            return;
        };
        let mut choices: Vec<Vec<PhysicalCandidateId>> = vec![Vec::new()];
        for i in 0..inputs.input_count(consumer) {
            choices = choices
                .into_iter()
                .flat_map(|prefix| {
                    producers(consumer, i).into_iter().map(move |u| {
                        let mut next = prefix.clone();
                        next.push(u);
                        next
                    })
                })
                .collect();
        }
        for choice in choices {
            if choice.iter().any(|&u| reaches(&dag, u, consumer)) {
                continue;
            }
            let mut next = dag.clone();
            next.insert(consumer, choice.clone());
            let mut next_pending = pending.clone();
            for &u in &choice {
                if !next.contains_key(&u) && !next_pending.contains(&u) {
                    next_pending.push(u);
                }
            }
            expand(inputs, producers, next, next_pending, best);
        }
    }
    let mut best = None;
    for (_, group) in inputs.query_output_groups() {
        for root in inputs.candidates_of_group(group) {
            if costs[root.0].is_some() {
                expand(&inputs, &producers, BTreeMap::new(), vec![root], &mut best);
            }
        }
    }
    best
}

/// On the small example, under many cost assignments and both query
/// phases, the MILP's optimum equals the cheapest DAG the reference
/// enumeration lists (#1207 (c)).
#[test]
fn the_milp_agrees_with_the_reference_enumeration() {
    let physical = example(&[(3, 5, 0)]);
    // A deterministic pseudo-random sequence of costs in [0, 10).
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % 1000) as f64 / 100.0
    };
    for round in 0..50 {
        let costs: Vec<Option<f64>> = (0..6).map(|id| (round % 7 != id).then(&mut next)).collect();
        for phase_ms in [0, 5_000] {
            let entries = [entry(phase_ms)];
            let reference = reference_minimum(&physical, &costs, &entries);
            let milp = solve(&physical, &costs, &entries);
            match (reference, milp) {
                (Some(reference), Ok(solved)) => {
                    assert!(
                        (reference - solved.objective_per_second).abs() < 1e-9,
                        "costs {costs:?} phase {phase_ms}: reference {reference}, MILP {}",
                        solved.objective_per_second
                    );
                    solved.selected_dag.validate(&physical).unwrap();
                }
                (None, Err(SelectionMilpError::Infeasible)) => {}
                (reference, milp) => {
                    panic!("costs {costs:?}: reference {reference:?}, MILP {milp:?}")
                }
            }
        }
    }
}
