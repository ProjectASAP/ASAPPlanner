//! The MILP's variables and constraints (A)–(I) and (R) over execution
//! contexts, each on a small memo built through `Memo::find_or_add_group`
//! and `Memo::add_candidate` with hand-made physical candidates, edges and
//! costs; the contexts and their executions; and the reference enumeration
//! of the concepts doc's Appendix A over executions against the MILP.
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use asap_accuracy::estimators::local_guarantee;
use asap_types::ir::operator::operator_properties::BinaryOpKind;
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchParams,
    SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, BinaryOperator, NonASAPOp, Operator, QueryRoot, TimeRangeKind};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    parameter_list, producer_satisfies_physical_requirements, CandidateExpression,
    CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates, LogicalProperties,
    LogicalRequirements, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge, WindowLayout,
};
use asap_types::workload::{
    DataWorkload, QueryRecurrence, QueryRequirements, QueryWorkloadEntry, RangeEvaluation,
    RepeatedDemand, RepetitionInterval, SourceFacts, TimestampMs,
};

use crate::execution_cost::ExecutionCosts;
use crate::milp::error_bounds::{CandidateErrorModel, RootAccuracyLimits, METRIC_COUNT};

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

/// One query every `interval_ms` at `phase_ms`, its requests evaluating
/// `span_ms` back in 10 s steps.
fn entry_every(interval_ms: u32, phase_ms: u64, span_ms: u64) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Default::default(),
        requirements: QueryRequirements::default(),
        predictability: Default::default(),
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(phase_ms),
        }),
        range_evaluation: (span_ms > 0).then_some(RangeEvaluation {
            step_ms: 10_000,
            span_ms,
        }),
    }
}

/// One query every 10 s at `phase_ms`.
fn entry(phase_ms: u64) -> QueryWorkloadEntry {
    entry_every(10_000, phase_ms, 0)
}

/// Every execution of `contexts` costing its candidate's `costs` entry
/// (`None`: not costable), in every context; each `AcrossEvaluations`
/// candidate in `retention` costing that per retained window.
fn uniform_costs(
    contexts: &ExecutionContexts,
    costs: &[Option<f64>],
    retention: &[(usize, f64)],
) -> ExecutionCosts {
    let mut execution_costs = ExecutionCosts::default();
    for &execution in contexts.executions() {
        match costs[execution.candidate().0] {
            Some(cost) => {
                execution_costs.cost_per_second.insert(execution, cost);
            }
            None => {
                execution_costs
                    .uncostable
                    .insert(execution, "not costable".into());
            }
        }
    }
    for &(v, cost) in retention {
        execution_costs
            .retention_cost_per_window_per_second
            .insert(PhysicalCandidateId(v), cost);
    }
    execution_costs
}

/// (K)'s inputs: the data workload the guarantees read, and each query's
/// root limits (`None`: (K) does not apply to it).
struct Accuracy<'a> {
    data: &'a DataWorkload,
    root_limits: &'a [Option<RootAccuracyLimits>],
}

static NO_DATA: std::sync::LazyLock<DataWorkload> = std::sync::LazyLock::new(DataWorkload::default);

/// No query under (K): the tests of (A)–(I) and (R).
fn no_accuracy() -> Accuracy<'static> {
    Accuracy {
        data: &NO_DATA,
        root_limits: &[],
    }
}

fn solve_under(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    retention: &[(usize, f64)],
    entries: &[QueryWorkloadEntry],
    accuracy: &Accuracy<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    let contexts = ExecutionContexts::new(physical, entries);
    let costs = uniform_costs(&contexts, costs, retention);
    let errors = CandidateErrorModel::new(physical, accuracy.data);
    solve_selection(&SelectionInputs::new(
        physical,
        &contexts,
        &costs,
        &errors,
        accuracy.root_limits,
    ))
}

fn solve_with_retention(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    retention: &[(usize, f64)],
    entries: &[QueryWorkloadEntry],
) -> Result<SolvedSelection, SelectionMilpError> {
    solve_under(physical, costs, retention, entries, &no_accuracy())
}

fn solve(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    entries: &[QueryWorkloadEntry],
) -> Result<SolvedSelection, SelectionMilpError> {
    solve_with_retention(physical, costs, &[], entries)
}

/// The selected candidates, one per selected execution, ascending.
fn selected(solved: &SolvedSelection) -> Vec<usize> {
    let mut candidates: Vec<usize> = solved
        .selected_dag
        .candidate_of_node
        .iter()
        .map(|id| id.0)
        .collect();
    candidates.sort_unstable();
    candidates
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

/// The concepts doc's Appendix A over executions: every DAG with one
/// output execution per query output and one producer execution per input
/// of every reached execution (the edges the contexts allow), without
/// cycles; each DAG's cost the sum of its distinct executions' costs plus,
/// per `AcrossEvaluations` candidate, its retention cost per window times
/// the largest need of its edges. Under (K), a DAG counts only when each
/// query's output meets its root limits by its composed error
/// (`CandidateErrorModel::composed_errors`). Returns the cheapest cost.
fn reference_minimum(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    retention: &[(usize, f64)],
    entries: &[QueryWorkloadEntry],
    accuracy: &Accuracy<'_>,
) -> Option<f64> {
    let contexts = ExecutionContexts::new(physical, entries);
    let execution_costs = uniform_costs(&contexts, costs, retention);
    let errors = CandidateErrorModel::new(physical, accuracy.data);
    let inputs = SelectionInputs::new(
        physical,
        &contexts,
        &execution_costs,
        &errors,
        accuracy.root_limits,
    );
    let costed = |execution: Execution| inputs.cost_per_second(execution).is_some();
    // Per (consumer execution, input), its producer executions, each with
    // the edge's context.
    let mut producers: BTreeMap<(Execution, usize), Vec<(Execution, ExecutionContextId)>> =
        BTreeMap::new();
    for &(context, edge) in contexts.edges() {
        let (producer, consumer) = inputs.executions_of_edge(context, edge);
        if costed(producer) && costed(consumer) {
            producers
                .entry((consumer, inputs.edges()[edge.0].input_index_of_consumer))
                .or_default()
                .push((producer, context));
        }
    }
    type Dag = BTreeMap<Execution, Vec<(Execution, ExecutionContextId)>>;
    fn reaches(dag: &Dag, from: Execution, to: Execution) -> bool {
        from == to
            || dag
                .get(&from)
                .into_iter()
                .flatten()
                .any(|&(next, _)| reaches(dag, next, to))
    }
    struct Search<'s, 'a> {
        inputs: &'s SelectionInputs<'a>,
        producers: &'s BTreeMap<(Execution, usize), Vec<(Execution, ExecutionContextId)>>,
        best: Option<f64>,
    }
    impl Search<'_, '_> {
        /// Whether every output of `outputs` meets its query's root limits
        /// in `dag`.
        fn meets_root_limits(&self, dag: &Dag, outputs: &[(QueryId, Execution)]) -> bool {
            let nodes: Vec<Execution> = dag.keys().copied().collect();
            let node = |execution: Execution| nodes.iter().position(|&n| n == execution).unwrap();
            let selected = DagOfSelectedCandidates {
                candidate_of_node: nodes.iter().map(|e| e.candidate()).collect(),
                edges: dag
                    .iter()
                    .flat_map(|(&consumer, choice)| {
                        choice
                            .iter()
                            .enumerate()
                            .map(move |(input, &(producer, _))| (consumer, input, producer))
                    })
                    .map(|(consumer, input, producer)| SelectedEdge {
                        producer: SelectedNodeId(node(producer)),
                        consumer: SelectedNodeId(node(consumer)),
                        input_index_of_consumer: input,
                    })
                    .collect(),
                node_of_output_reference: Vec::new(),
            };
            let composed = self.inputs.errors.composed_errors(&selected);
            outputs.iter().all(|&(query, execution)| {
                let Some(limits) = self.inputs.root_limits_of(query) else {
                    return true;
                };
                let error = &composed[node(execution)];
                !error.without_guarantee
                    && (0..METRIC_COUNT)
                        .all(|m| error.error_bound[m] <= limits.error_bound_limit[m])
                    && limits
                        .failure_probability_limit
                        .is_none_or(|limit| error.failure_probability.unwrap_or(1.0) <= limit)
            })
        }

        fn expand(
            &mut self,
            dag: Dag,
            roots: &BTreeSet<Execution>,
            outputs: &[(QueryId, Execution)],
            mut pending: Vec<Execution>,
        ) {
            let Some(consumer) = pending.pop() else {
                if !self.meets_root_limits(&dag, outputs) {
                    return;
                }
                let mut retained: BTreeMap<PhysicalCandidateId, u64> = BTreeMap::new();
                for choices in dag.values() {
                    for &(producer, context) in choices {
                        if let Execution::Materialized(v) = producer {
                            let need = self.inputs.retention_need_windows(context, v);
                            let windows = retained.entry(v).or_default();
                            *windows = (*windows).max(need);
                        }
                    }
                }
                let cost: f64 = dag
                    .keys()
                    .map(|&execution| self.inputs.cost_per_second(execution).unwrap())
                    .sum::<f64>()
                    + retained
                        .into_iter()
                        .map(|(v, windows)| {
                            self.inputs
                                .costs
                                .retention_cost_per_window_per_second
                                .get(&v)
                                .map_or(0.0, |cost| cost * windows as f64)
                        })
                        .sum::<f64>();
                self.best = Some(self.best.map_or(cost, |known: f64| known.min(cost)));
                return;
            };
            let mut choices: Vec<Vec<(Execution, ExecutionContextId)>> = vec![Vec::new()];
            for i in 0..self.inputs.input_count(consumer.candidate()) {
                let options = self
                    .producers
                    .get(&(consumer, i))
                    .cloned()
                    .unwrap_or_default();
                choices = choices
                    .into_iter()
                    .flat_map(|prefix| {
                        options.iter().map(move |&option| {
                            let mut next = prefix.clone();
                            next.push(option);
                            next
                        })
                    })
                    .collect();
            }
            for choice in choices {
                if choice.iter().any(|&(u, _)| reaches(&dag, u, consumer)) {
                    continue;
                }
                let mut next = dag.clone();
                next.insert(consumer, choice.clone());
                let mut next_pending = pending.clone();
                for &(u, _) in &choice {
                    if !next.contains_key(&u) && !next_pending.contains(&u) && !roots.contains(&u) {
                        next_pending.push(u);
                    }
                }
                self.expand(next, roots, outputs, next_pending);
            }
        }
    }
    // Every choice of one output execution per query output.
    let mut root_choices: Vec<Vec<(QueryId, Execution)>> = vec![Vec::new()];
    for &(query, group) in inputs.query_output_groups() {
        let options: Vec<Execution> = inputs
            .candidates_of_group(group)
            .iter()
            .filter_map(|&v| inputs.output_execution(query, v))
            .collect();
        root_choices = root_choices
            .into_iter()
            .flat_map(|prefix| {
                options.iter().map(move |&option| {
                    let mut next = prefix.clone();
                    next.push((query, option));
                    next
                })
            })
            .collect();
    }
    let mut search = Search {
        inputs: &inputs,
        producers: &producers,
        best: None,
    };
    for outputs in root_choices {
        let roots: BTreeSet<Execution> = outputs.iter().map(|&(_, execution)| execution).collect();
        search.expand(
            BTreeMap::new(),
            &roots,
            &outputs,
            roots.iter().copied().collect(),
        );
    }
    search.best
}

/// A deterministic pseudo-random sequence of costs in [0, 10).
fn pseudo_random_costs() -> impl FnMut() -> f64 {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % 1000) as f64 / 100.0
    }
}

/// Whether the MILP's optimum on `physical` equals the reference
/// enumeration's cheapest DAG, and its decoded DAG validates.
fn agrees_with_the_reference(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    retention: &[(usize, f64)],
    entries: &[QueryWorkloadEntry],
) {
    agrees_with_the_reference_under(physical, costs, retention, entries, &no_accuracy());
}

fn agrees_with_the_reference_under(
    physical: &PhysicalASAPMemoCandidates,
    costs: &[Option<f64>],
    retention: &[(usize, f64)],
    entries: &[QueryWorkloadEntry],
    accuracy: &Accuracy<'_>,
) {
    let reference = reference_minimum(physical, costs, retention, entries, accuracy);
    match (
        reference,
        solve_under(physical, costs, retention, entries, accuracy),
    ) {
        (Some(reference), Ok(solved)) => {
            assert!(
                (reference - solved.objective_per_second).abs() < 1e-6,
                "costs {costs:?} retention {retention:?}: reference {reference}, MILP {}",
                solved.objective_per_second
            );
            solved.selected_dag.validate(physical).unwrap();
        }
        (None, Err(SelectionMilpError::Infeasible)) => {}
        (reference, milp) => panic!("costs {costs:?}: reference {reference:?}, MILP {milp:?}"),
    }
}

/// On the small example, under many cost assignments and both query
/// phases, the MILP's optimum equals the cheapest DAG the reference
/// enumeration lists (#1207 (c)).
#[test]
fn the_milp_agrees_with_the_reference_enumeration() {
    let physical = example(&[(3, 5, 0)]);
    let mut next = pseudo_random_costs();
    for round in 0..50 {
        let costs: Vec<Option<f64>> = (0..6).map(|id| (round % 7 != id).then(&mut next)).collect();
        for phase_ms in [0, 5_000] {
            agrees_with_the_reference(&physical, &costs, &[], &[entry(phase_ms)]);
        }
    }
}

/// Over execution contexts and retention, the MILP agrees with the
/// reference enumeration on the windowed example with two queries:
/// in one context or two, with range evaluations of different spans, the
/// chain's retention cost per window random too.
#[test]
fn the_milp_agrees_with_the_reference_enumeration_over_contexts_and_retention() {
    let physical = windowed_example();
    let mut next = pseudo_random_costs();
    for round in 0..30 {
        let costs: Vec<Option<f64>> = (0..WINDOWED_CANDIDATES)
            .map(|id| (round % 9 != id).then(&mut next))
            .collect();
        let retention = [(CHAIN_10S, next() / 10.0), (CHAIN_5S, next() / 10.0)];
        for entries in [
            [entry(0), entry(0)],
            [entry(0), entry(5_000)],
            [entry(0), entry_every(10_000, 0, 50_000)],
            [
                entry_every(20_000, 0, 0),
                entry_every(10_000, 5_000, 30_000),
            ],
        ] {
            agrees_with_the_reference(&physical, &costs, &retention, &entries);
        }
    }
}

// Candidates of `windowed_example`.
const RAW: usize = 0;
const RANGE: usize = 1;
const BUILD: usize = 2;
const CHAIN_10S: usize = 3;
const CHAIN_5S: usize = 4;
const READOUT_A: usize = 5;
const READOUT_B: usize = 6;
const BUILD_ONE_EVALUATION: usize = 7;
const WINDOWED_CANDIDATES: usize = 8;

/// Two queries over one KLL state of `latency[1m]`: a scan group (`RAW`),
/// the 1m range over it (`RANGE`), the state group with a build at query
/// time, not materialized (`BUILD`) or `OneEvaluation`
/// (`BUILD_ONE_EVALUATION`), and builds of 10 s and 5 s tumbling windows
/// materialized across evaluations at query time (`CHAIN_10S`,
/// `CHAIN_5S`); query A's p95 (`READOUT_A`) and query B's p50
/// (`READOUT_B`) over it. Every other candidate runs at query time, not
/// materialized; every edge is admissible.
fn windowed_example() -> PhysicalASAPMemoCandidates {
    windowed_example_with(ExecutionTiming::QueryTime, 5_000)
}

/// [`windowed_example`] with the two chains at `chain_timing` (the scan
/// and the range too at ingestion time), the second `second_width_ms`
/// wide; edges admissible when $\alpha^{\mathrm{phys}}$ holds.
fn windowed_example_with(
    chain_timing: ExecutionTiming,
    second_width_ms: u64,
) -> PhysicalASAPMemoCandidates {
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
    let mut logical = vec![add(&mut memo, scan_group, scan_operator, None, None)];
    let range_operator = Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(60),
        kind: TimeRangeKind::Range,
        child: scan_group,
    });
    let range_group = memo.find_or_add_group(range_operator.clone()).unwrap();
    logical.push(add(&mut memo, range_group, range_operator, None, None));
    let build = Operator::ASAP(ASAPOp::SummaryAgg {
        child: range_group,
        family: FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
    });
    let states = memo.find_or_add_group(build.clone()).unwrap();
    let parameters = parameter_list(SketchAlgorithm::Kll)[0].clone();
    for window_layout in [
        None,
        Some(WindowLayout::Tumbling { width_ms: 10_000 }),
        Some(WindowLayout::Tumbling {
            width_ms: second_width_ms,
        }),
    ] {
        logical.push(add(
            &mut memo,
            states,
            build.clone(),
            Some(parameters.clone()),
            window_layout,
        ));
    }
    for q in [0.95, 0.5] {
        let readout = Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: states,
            query: SketchStatistic::Quantile { q },
        });
        let group = memo.find_or_add_group(readout.clone()).unwrap();
        logical.push(add(&mut memo, group, readout, None, None));
        memo.add_query(QueryRoot::Operator(group));
    }
    // The unwindowed build again, `OneEvaluation`.
    logical.push(logical[BUILD]);
    let qt = |lifetime| PhysicalProperties::new(ExecutionTiming::QueryTime, lifetime).unwrap();
    let mut physical_candidates: Vec<PhysicalCandidate> = logical
        .iter()
        .enumerate()
        .map(|(id, &logical_candidate_id)| PhysicalCandidate {
            logical_candidate_id,
            physical_properties: match id {
                CHAIN_10S | CHAIN_5S => PhysicalProperties::new(
                    chain_timing,
                    Lifetime::AcrossEvaluations(StorageMedium::Memory),
                )
                .unwrap(),
                BUILD_ONE_EVALUATION => qt(Lifetime::OneEvaluation(StorageMedium::Memory)),
                _ => qt(Lifetime::None),
            },
        })
        .collect();
    if chain_timing == ExecutionTiming::IngestionTime {
        for id in [RAW, RANGE] {
            physical_candidates[id].physical_properties =
                PhysicalProperties::new(ExecutionTiming::IngestionTime, Lifetime::None).unwrap();
        }
    }
    // Grouped by (consumer, input).
    let mut edges = vec![(RAW, RANGE, 0)];
    for state in [BUILD, CHAIN_10S, CHAIN_5S] {
        edges.push((RANGE, state, 0));
    }
    for readout in [READOUT_A, READOUT_B] {
        for state in [BUILD, CHAIN_10S, CHAIN_5S, BUILD_ONE_EVALUATION] {
            edges.push((state, readout, 0));
        }
    }
    edges.push((RANGE, BUILD_ONE_EVALUATION, 0));
    let edges = edges
        .into_iter()
        .map(|(producer, consumer, input)| PhysicalEdge {
            producer_candidate_id: PhysicalCandidateId(producer),
            consumer_candidate_id: PhysicalCandidateId(consumer),
            input_index_of_consumer: input,
            satisfies_logical_requirement: true,
            satisfies_physical_requirement: producer_satisfies_physical_requirements(
                &physical_candidates[producer],
                &physical_candidates[consumer],
            ),
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

/// Costs of `windowed_example`'s candidates: `cheap` cost 1, the others
/// 100.
fn costs_where_cheap(cheap: &[usize]) -> Vec<Option<f64>> {
    (0..WINDOWED_CANDIDATES)
        .map(|id| Some(if cheap.contains(&id) { 1.0 } else { 100.0 }))
        .collect()
}

/// Contexts are deduplicated by equality: queries with equal recurrence
/// and range evaluation share one evaluation context, different ones get
/// their own; a materialized chain's inputs run in its own computation
/// context.
#[test]
fn contexts_are_deduplicated_by_equality() {
    let physical = windowed_example();
    let same = ExecutionContexts::new(&physical, &[entry(0), entry(0)]);
    assert_eq!(same.context_of_query(0), same.context_of_query(1));
    let different = ExecutionContexts::new(&physical, &[entry(0), entry(5_000)]);
    assert_ne!(different.context_of_query(0), different.context_of_query(1));
    let spans = ExecutionContexts::new(&physical, &[entry(0), entry_every(10_000, 0, 50_000)]);
    assert_ne!(spans.context_of_query(0), spans.context_of_query(1));
    // Two evaluations and each chain's computation.
    assert_eq!(different.contexts().len(), 4);
    for chain in [CHAIN_10S, CHAIN_5S] {
        let context = different
            .computation_context(PhysicalCandidateId(chain))
            .unwrap();
        assert_eq!(
            different.contexts()[context.0],
            ExecutionContext::Materialized(PhysicalCandidateId(chain))
        );
    }
}

/// $x_{\kappa,v}$: a not-materialized candidate read by two queries of one
/// context runs once there, and its cost is counted once; with the queries
/// in two contexts it runs in each, as two nodes of the selected DAG, and
/// its cost is counted twice.
#[test]
fn a_not_materialized_candidate_runs_once_per_context_that_selects_it() {
    let physical = windowed_example();
    let costs = costs_where_cheap(&[RAW, RANGE, BUILD, READOUT_A, READOUT_B]);
    let shared = solve(&physical, &costs, &[entry(0), entry(0)]).unwrap();
    assert_eq!(selected(&shared), [RAW, RANGE, BUILD, READOUT_A, READOUT_B]);
    assert_eq!(shared.objective_per_second, 5.0);
    shared.selected_dag.validate(&physical).unwrap();
    let apart = solve(&physical, &costs, &[entry(0), entry(5_000)]).unwrap();
    assert_eq!(
        selected(&apart),
        [RAW, RAW, RANGE, RANGE, BUILD, BUILD, READOUT_A, READOUT_B]
    );
    assert_eq!(apart.objective_per_second, 8.0);
    apart.selected_dag.validate(&physical).unwrap();
}

/// A `OneEvaluation` result belongs to one evaluation context: read by
/// two queries of one context, it runs once there.
#[test]
fn a_one_evaluation_result_is_shared_within_its_context() {
    let physical = windowed_example();
    let costs = costs_where_cheap(&[RAW, RANGE, BUILD_ONE_EVALUATION, READOUT_A, READOUT_B]);
    let solved = solve(&physical, &costs, &[entry(0), entry(0)]).unwrap();
    assert_eq!(
        selected(&solved),
        [RAW, RANGE, READOUT_A, READOUT_B, BUILD_ONE_EVALUATION]
    );
    let contexts = ExecutionContexts::new(&physical, &[entry(0), entry(0)]);
    // Never inside a chain's computation.
    assert!(contexts
        .executions()
        .iter()
        .all(|execution| match execution {
            Execution::InContext(context, v) if v.0 == BUILD_ONE_EVALUATION => matches!(
                contexts.contexts()[context.0],
                ExecutionContext::Evaluation { .. }
            ),
            _ => true,
        }));
}

/// #1204 regression: two 10 s queries at phases 0 s and 5 s both read one
/// chain of 5 s tumbling windows materialized across evaluations: one
/// $x^{\mathrm{mat}}$, an edge $y_{\kappa,u,c,i}$ in each query's context,
/// its cost counted once. The 10 s chain delivers only on phase 0
/// (`delivers_on` per context), so it has no edge in phase 5 s's context.
#[test]
fn two_queries_at_phases_0_and_5_s_read_one_5_s_chain() {
    let physical = windowed_example();
    let contexts = ExecutionContexts::new(&physical, &[entry(0), entry(5_000)]);
    let edges_out_of = |chain: usize| -> Vec<ExecutionContextId> {
        contexts
            .edges()
            .iter()
            .filter(|(_, edge)| {
                physical
                    .edges()
                    .nth(edge.0)
                    .unwrap()
                    .1
                    .producer_candidate_id
                    .0
                    == chain
            })
            .map(|&(context, _)| context)
            .collect()
    };
    assert_eq!(edges_out_of(CHAIN_5S).len(), 2);
    assert_eq!(
        edges_out_of(CHAIN_10S),
        [contexts.context_of_query(0).unwrap()]
    );
    let costs = costs_where_cheap(&[RAW, RANGE, CHAIN_5S, READOUT_A, READOUT_B]);
    let solved = solve(&physical, &costs, &[entry(0), entry(5_000)]).unwrap();
    assert_eq!(
        selected(&solved),
        [RAW, RANGE, CHAIN_5S, READOUT_A, READOUT_B]
    );
    assert_eq!(
        solved
            .execution_of_node
            .iter()
            .filter(|execution| matches!(execution, Execution::Materialized(_)))
            .count(),
        1
    );
    assert_eq!(solved.objective_per_second, 5.0);
    solved.selected_dag.validate(&physical).unwrap();
}

/// (R): a chain of 10 s windows over a 1m range retains as many windows as
/// its most demanding selected consumer reads: 6 for queries evaluated
/// once per request, 11 when one evaluates 50 s back from each request's
/// end; the objective costs them at $\pi_v$ each.
#[test]
fn a_chain_retains_what_its_most_demanding_consumer_reads() {
    let physical = windowed_example();
    let costs = costs_where_cheap(&[RAW, RANGE, CHAIN_10S, READOUT_A, READOUT_B]);
    let retention = [(CHAIN_10S, 0.5)];
    let once_per_request =
        solve_with_retention(&physical, &costs, &retention, &[entry(0), entry(0)]).unwrap();
    assert_eq!(
        selected(&once_per_request),
        [RAW, RANGE, CHAIN_10S, READOUT_A, READOUT_B]
    );
    assert_eq!(once_per_request.objective_per_second, 5.0 + 0.5 * 6.0);
    let with_span = solve_with_retention(
        &physical,
        &costs,
        &retention,
        &[entry(0), entry_every(10_000, 0, 50_000)],
    )
    .unwrap();
    assert_eq!(
        selected(&with_span),
        [RAW, RANGE, CHAIN_10S, READOUT_A, READOUT_B]
    );
    assert_eq!(with_span.objective_per_second, 5.0 + 0.5 * 11.0);
    // The MILP's own optimum, through h_v, agrees: its bound is the
    // objective.
    assert!((with_span.lower_bound_per_second - with_span.objective_per_second).abs() < 1e-6);
}

/// The range a chain is built over, and the scan below it, run in the
/// chain's own computation context, not in the queries' evaluation
/// context.
#[test]
fn a_chain_reads_its_inputs_in_its_own_computation_context() {
    let physical = windowed_example();
    let costs = costs_where_cheap(&[RAW, RANGE, CHAIN_10S, READOUT_A, READOUT_B]);
    let solved = solve(&physical, &costs, &[entry(0), entry(0)]).unwrap();
    let chain_context = ExecutionContextId(
        solved
            .execution_of_node
            .iter()
            .find_map(|execution| match execution {
                Execution::InContext(context, v) if v.0 == RANGE => Some(context.0),
                _ => None,
            })
            .unwrap(),
    );
    let contexts = ExecutionContexts::new(&physical, &[entry(0), entry(0)]);
    assert_eq!(
        contexts.computation_context(PhysicalCandidateId(CHAIN_10S)),
        Some(chain_context)
    );
}

/// Chains maintained at ingestion time read their inputs in the
/// ingestion pipeline of their refresh window, shared by the chains with
/// equal ones: two chains 10 s wide share one pipeline, where the scan
/// and the range below them run once; chains 10 s and 5 s wide get one
/// pipeline each.
#[test]
fn chains_at_ingestion_share_the_pipeline_of_their_refresh_window() {
    let pipeline_of = |contexts: &ExecutionContexts| {
        [CHAIN_10S, CHAIN_5S].map(|chain| {
            let context = contexts
                .computation_context(PhysicalCandidateId(chain))
                .unwrap();
            assert!(matches!(
                contexts.contexts()[context.0],
                ExecutionContext::IngestionPipeline(_)
            ));
            context
        })
    };
    let shared = windowed_example_with(ExecutionTiming::IngestionTime, 10_000);
    let contexts = ExecutionContexts::new(&shared, &[entry(0), entry(0)]);
    let [first, second] = pipeline_of(&contexts);
    assert_eq!(first, second);
    let scans: Vec<&Execution> = contexts
        .executions()
        .iter()
        .filter(|execution| execution.candidate().0 == RAW)
        .collect();
    assert_eq!(
        scans,
        [&Execution::InContext(first, PhysicalCandidateId(RAW))]
    );
    let apart = windowed_example_with(ExecutionTiming::IngestionTime, 5_000);
    let contexts = ExecutionContexts::new(&apart, &[entry(0), entry(0)]);
    let [first, second] = pipeline_of(&contexts);
    assert_ne!(first, second);
}

/// The objective's costs from `asap-cost` on the windowed example: $\rho_\kappa$
/// per context (an evaluation every 10 s: 0.1/s; a query-time chain one
/// build per window: 0.1/s for 10 s windows, 0.2/s for 5 s); the range a
/// chain is built over costs nothing in the chain's computation, but does
/// in the queries' evaluation; each chain has a positive cost $\pi_v$ per
/// retained window; every execution is costed.
#[test]
fn execution_costs_follow_the_context_of_each_execution() {
    use crate::execution_cost::{execution_costs, executions_per_second};
    use asap_cost::CostInputs;
    use asap_types::workload::{DataArrival, DataWorkload, DurationMs, SourceFacts};
    let physical = windowed_example();
    let entries = [entry(0), entry(0)];
    let contexts = ExecutionContexts::new(&physical, &entries);
    let cost_inputs = CostInputs::default();
    let rate = |context: ExecutionContextId| {
        executions_per_second(&physical, &contexts.contexts()[context.0], &cost_inputs).unwrap()
    };
    let evaluation = contexts.context_of_query(0).unwrap();
    let chain_10s = contexts
        .computation_context(PhysicalCandidateId(CHAIN_10S))
        .unwrap();
    let chain_5s = contexts
        .computation_context(PhysicalCandidateId(CHAIN_5S))
        .unwrap();
    assert!((rate(evaluation) - 0.1).abs() < 1e-12);
    assert!((rate(chain_10s) - 0.1).abs() < 1e-12);
    assert!((rate(chain_5s) - 0.2).abs() < 1e-12);
    let data = DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 100.0,
        },
        sources: [(
            "latency".to_string(),
            SourceFacts {
                sample_interval: Some(DurationMs(10_000)),
                series_count: Some(1_000),
                ..SourceFacts::default()
            },
        )]
        .into(),
    };
    let costs = execution_costs(&physical, &contexts, &data, &cost_inputs, 1.0);
    assert_eq!(costs.uncostable, BTreeMap::new());
    let range =
        |context| costs.cost_per_second[&Execution::InContext(context, PhysicalCandidateId(RANGE))];
    assert_eq!(range(chain_10s), 0.0);
    assert_eq!(range(chain_5s), 0.0);
    assert!(range(evaluation) > 0.0);
    for chain in [CHAIN_10S, CHAIN_5S] {
        let retention = costs.retention_cost_per_window_per_second[&PhysicalCandidateId(chain)];
        assert!(retention > 0.0 && retention.is_finite());
    }
}

// ── Constraint (K) ──────────────────────────────────────────────────────

/// `memo`'s candidates, each at query time and not materialized, with
/// every edge from a candidate of an input group admissible.
fn all_at_query_time(memo: Memo) -> PhysicalASAPMemoCandidates {
    let mut logical = Vec::new();
    for (g, group) in memo.groups().iter().enumerate() {
        for index_in_group in 0..group.candidates.len() {
            logical.push(CandidateForMemoGroupId {
                group: MemoGroupId(g as u32),
                index_in_group,
            });
        }
    }
    let id_of = |candidate: CandidateForMemoGroupId| {
        PhysicalCandidateId(logical.iter().position(|&c| c == candidate).unwrap())
    };
    let mut edges = Vec::new();
    for (c, &consumer) in logical.iter().enumerate() {
        let children = memo.candidate(consumer).expression.operator.children();
        for (input, &child) in children.into_iter().enumerate() {
            for index_in_group in 0..memo.groups()[child.0 as usize].candidates.len() {
                edges.push(PhysicalEdge {
                    producer_candidate_id: id_of(CandidateForMemoGroupId {
                        group: child,
                        index_in_group,
                    }),
                    consumer_candidate_id: PhysicalCandidateId(c),
                    input_index_of_consumer: input,
                    satisfies_logical_requirement: true,
                    satisfies_physical_requirement: true,
                });
            }
        }
    }
    let physical_candidates = logical
        .iter()
        .map(|&logical_candidate_id| PhysicalCandidate {
            logical_candidate_id,
            physical_properties: PhysicalProperties::new(QT_NONE.0, QT_NONE.1).unwrap(),
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

/// A sketch build of `algorithm` over the scan `rows`, per series.
fn sketch_build(rows: MemoGroupId, algorithm: SketchAlgorithm) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child: rows,
        family: FieldDataType::Sketch(algorithm, GroupingStrategy::default()),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
    })
}

fn quantile(state: MemoGroupId, q: f64) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query: SketchStatistic::Quantile { q },
    })
}

/// The scan group of `latency` (one candidate).
fn latency_scan(memo: &mut Memo) -> MemoGroupId {
    let scan = Operator::NonASAP(NonASAPOp::Scan {
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
    let group = memo.find_or_add_group(scan.clone()).unwrap();
    add(memo, group, scan, None, None);
    group
}

/// The state group of `algorithm` over `rows`, one candidate per
/// `parameters`, in order.
fn state_group(
    memo: &mut Memo,
    rows: MemoGroupId,
    algorithm: SketchAlgorithm,
    parameters: &[SketchParams],
) -> MemoGroupId {
    let build = sketch_build(rows, algorithm);
    let group = memo.find_or_add_group(build.clone()).unwrap();
    for parameters in parameters {
        add(memo, group, build.clone(), Some(parameters.clone()), None);
    }
    group
}

/// The quantile `q` group, read from the KLL state `kll` (candidate 0)
/// or the DDSketch state `ddsketch` (candidate 1).
fn quantile_group(memo: &mut Memo, kll: MemoGroupId, ddsketch: MemoGroupId, q: f64) -> MemoGroupId {
    let group = memo.find_or_add_group(quantile(kll, q)).unwrap();
    add(memo, group, quantile(kll, q), None, None);
    add(memo, group, quantile(ddsketch, q), None, None);
    group
}

const KLL_SIZES: usize = 4;
const DDSKETCH_ALPHAS: [f64; 4] = [0.1, 0.05, 0.02, 0.01];

/// #1207's problem example: query A, `quantile_over_time(0.95,
/// latency[1m])`, and query B, the ratio of the 0.99 and 0.50 quantiles of
/// `latency[1m]`. Candidates: 0 the scan; 1–4 the KLL state, `k` from the
/// first four listed sizes; 5–8 the DDSketch state, `alpha` from
/// [`DDSKETCH_ALPHAS`]; A's p95 read from KLL (9) or DDSketch (10); B's
/// p99 (11, 12) and p50 (13, 14) likewise; 15 B's ratio.
fn problem_example() -> PhysicalASAPMemoCandidates {
    let mut memo = Memo::default();
    let scan = latency_scan(&mut memo);
    let kll = state_group(
        &mut memo,
        scan,
        SketchAlgorithm::Kll,
        &parameter_list(SketchAlgorithm::Kll)[..KLL_SIZES],
    );
    let ddsketch_parameters: Vec<SketchParams> = DDSKETCH_ALPHAS
        .iter()
        .map(|&alpha| SketchParams::DDSketch { alpha })
        .collect();
    let ddsketch = state_group(
        &mut memo,
        scan,
        SketchAlgorithm::DDSketch,
        &ddsketch_parameters,
    );
    let p95 = quantile_group(&mut memo, kll, ddsketch, 0.95);
    let p99 = quantile_group(&mut memo, kll, ddsketch, 0.99);
    let p50 = quantile_group(&mut memo, kll, ddsketch, 0.50);
    let ratio = Operator::NonASAP(NonASAPOp::BinaryOp {
        operator: BinaryOperator {
            checked_relative_division: false,
            checked_finite_division: false,
            kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
            vector_match: None,
        },
        return_bool: false,
        lhs: p99,
        rhs: p50,
    });
    let ratio_group = memo.find_or_add_group(ratio.clone()).unwrap();
    add(&mut memo, ratio_group, ratio, None, None);
    memo.add_query(QueryRoot::Operator(p95));
    memo.add_query(QueryRoot::Operator(ratio_group));
    all_at_query_time(memo)
}

const P95_FROM_KLL: usize = 9;
const P95_FROM_DDSKETCH: usize = 10;
const RATIO: usize = 15;

/// Costs of the problem example: a larger sketch costs more, and a
/// DDSketch less than a KLL of the same rank.
fn problem_example_costs() -> Vec<Option<f64>> {
    let mut costs = vec![Some(1.0)];
    costs.extend((0..KLL_SIZES).map(|size| Some(10.0 + 10.0 * size as f64)));
    costs.extend((0..DDSKETCH_ALPHAS.len()).map(|size| Some(5.0 + 10.0 * size as f64)));
    costs.extend(std::iter::repeat_n(Some(1.0), 7));
    costs
}

/// A query every 10 s at phase 0 with `accuracy`.
fn entry_with(accuracy: AccuracyTarget) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        requirements: QueryRequirements {
            accuracy,
            max_latency_ms: None,
        },
        ..entry(0)
    }
}

/// `latency`'s values declared non-negative: a DDSketch interior quantile
/// keeps its relative error.
fn non_negative_latency() -> DataWorkload {
    let mut data = DataWorkload::default();
    data.sources.insert(
        "latency".into(),
        SourceFacts {
            values_non_negative: true,
            ..Default::default()
        },
    );
    data
}

/// The `parameters` the selected candidates of `states` (candidate ids)
/// have, in candidate order.
fn selected_parameters(
    physical: &PhysicalASAPMemoCandidates,
    solved: &SolvedSelection,
    states: std::ops::RangeInclusive<usize>,
) -> Vec<SketchParams> {
    selected(solved)
        .into_iter()
        .filter(|candidate| states.contains(candidate))
        .map(|candidate| {
            physical
                .logical_candidates
                .memo
                .candidate(physical.physical_candidates[candidate].logical_candidate_id)
                .logical_properties
                .parameters()
                .unwrap()
                .clone()
        })
        .collect()
}

/// The cheapest of `list` whose rank error for a quantile is at most
/// `limit`.
fn smallest_kll_within(limit: f64) -> SketchParams {
    parameter_list(SketchAlgorithm::Kll)
        .iter()
        .find(|parameters| {
            local_guarantee(
                &FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
                Some(parameters),
                &SketchStatistic::Quantile { q: 0.95 },
            )
            .and_then(|g| g.spec.error.evaluate())
            .is_some_and(|error| error <= limit)
        })
        .unwrap()
        .clone()
}

/// #1207's problem example under (K): A's rank target 0.01 is met by a
/// KLL state (its DDSketch readout, cheaper, is relative error, which a
/// rank target does not accept: the metric rule at the root), and B's
/// ratio within relative error 0.03 reads one DDSketch state through both
/// operands, its cost paid once. B's per-operand bound is
/// $(\alpha + \alpha) / (1 - 0.1) \le 0.03$, so `alpha` 0.02 (each operand
/// alone well within 0.03) is too weak: (K) picks 0.01. Without (K) the
/// weakest sizes win.
#[test]
fn the_problem_example_meets_each_query_target_at_its_root() {
    let physical = problem_example();
    let entries = [
        entry_with(AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank)),
        entry_with(AccuracyTarget::max_error(0.03).accuracy_metric(AccuracyMetric::Relative)),
    ];
    let data = non_negative_latency();
    let root_limits: Vec<_> = entries
        .iter()
        .map(|entry| {
            Some(RootAccuracyLimits::of(
                &entry.requirements.accuracy,
                Some(1),
            ))
        })
        .collect();
    let accuracy = Accuracy {
        data: &data,
        root_limits: &root_limits,
    };
    let costs = problem_example_costs();
    let solved = solve_under(&physical, &costs, &[], &entries, &accuracy).unwrap();
    let chosen = selected(&solved);
    assert!(chosen.contains(&P95_FROM_KLL) && !chosen.contains(&P95_FROM_DDSKETCH));
    assert!(chosen.contains(&RATIO));
    assert_eq!(
        selected_parameters(&physical, &solved, 1..=KLL_SIZES),
        vec![smallest_kll_within(0.01)]
    );
    // One DDSketch state, alpha 0.01, read by both of B's operands.
    assert_eq!(
        selected_parameters(&physical, &solved, 5..=8),
        vec![SketchParams::DDSketch { alpha: 0.01 }]
    );
    let ddsketch_node = solved
        .selected_dag
        .candidate_of_node
        .iter()
        .position(|&c| (5..=8).contains(&c.0))
        .unwrap();
    assert_eq!(
        solved
            .selected_dag
            .edges
            .iter()
            .filter(|edge| edge.producer.0 == ddsketch_node)
            .count(),
        2
    );
    // Without (K): the cheapest sizes, A from DDSketch.
    let unchecked = solve_under(&physical, &costs, &[], &entries, &no_accuracy()).unwrap();
    assert!(selected(&unchecked).contains(&P95_FROM_DDSKETCH));
    assert_eq!(
        selected_parameters(&physical, &unchecked, 5..=8),
        vec![SketchParams::DDSketch { alpha: 0.1 }]
    );
}

/// A DDSketch interior quantile keeps its relative error only over values
/// proven non-negative: without the declaration B has no guarantee and,
/// its ratio reading no KLL (rank error into a relative-error contract),
/// no plan meets (K).
#[test]
fn a_ddsketch_interior_quantile_needs_non_negative_values() {
    let physical = problem_example();
    let entries = [
        entry_with(AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank)),
        entry_with(AccuracyTarget::max_error(0.03).accuracy_metric(AccuracyMetric::Relative)),
    ];
    let root_limits: Vec<_> = entries
        .iter()
        .map(|entry| {
            Some(RootAccuracyLimits::of(
                &entry.requirements.accuracy,
                Some(1),
            ))
        })
        .collect();
    let accuracy = Accuracy {
        data: &NO_DATA,
        root_limits: &root_limits,
    };
    assert_eq!(
        solve_under(
            &physical,
            &problem_example_costs(),
            &[],
            &entries,
            &accuracy
        )
        .unwrap_err(),
        SelectionMilpError::Infeasible
    );
}

/// Failure probabilities add along the selected edges (a union bound),
/// a producer reached twice counted twice: a `Concat` of p95 and p50 read
/// from one KLL state fails with twice its δ. A per-value target of 1.5δ
/// is infeasible, 2δ is met; the error limit is loose in both.
#[test]
fn failure_probabilities_add_along_the_selected_edges() {
    let mut memo = Memo::default();
    let scan = latency_scan(&mut memo);
    let kll_parameters = parameter_list(SketchAlgorithm::Kll)[KLL_SIZES - 1].clone();
    let kll = state_group(
        &mut memo,
        scan,
        SketchAlgorithm::Kll,
        std::slice::from_ref(&kll_parameters),
    );
    let readouts: Vec<MemoGroupId> = [0.95, 0.5]
        .into_iter()
        .map(|q| {
            let group = memo.find_or_add_group(quantile(kll, q)).unwrap();
            add(&mut memo, group, quantile(kll, q), None, None);
            group
        })
        .collect();
    let both = Operator::NonASAP(NonASAPOp::Concat { children: readouts });
    let both_group = memo.find_or_add_group(both.clone()).unwrap();
    add(&mut memo, both_group, both, None, None);
    memo.add_query(QueryRoot::Operator(both_group));
    let physical = all_at_query_time(memo);
    let delta = local_guarantee(
        &FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
        Some(&kll_parameters),
        &SketchStatistic::Quantile { q: 0.95 },
    )
    .unwrap()
    .spec
    .failure_probability
    .evaluate()
    .unwrap();
    assert!(delta > 0.0);
    let costs = vec![Some(1.0); physical.physical_candidates.len()];
    for (failure_probability, feasible) in [(1.5 * delta, false), (2.0 * delta, true)] {
        let target = AccuracyTarget::max_error(0.5)
            .failure_probability(failure_probability)
            .accuracy_scope(AccuracyScope::PerValue);
        let entries = [entry_with(target.clone())];
        let root_limits = [Some(RootAccuracyLimits::of(&target, None))];
        let accuracy = Accuracy {
            data: &NO_DATA,
            root_limits: &root_limits,
        };
        assert_eq!(
            solve_under(&physical, &costs, &[], &entries, &accuracy).is_ok(),
            feasible,
            "δ target {failure_probability}"
        );
        agrees_with_the_reference_under(&physical, &costs, &[], &entries, &accuracy);
    }
}

/// Big-M on the selected edge only: an input with an exact producer and
/// a cheap approximate one whose error is over the target. The
/// approximate producer's bound binds only when its edge is selected, so
/// the exact one is chosen under a tight target and the cheap one under a
/// loose target.
#[test]
fn a_producer_s_error_counts_only_on_its_selected_edge() {
    let physical = problem_example();
    let data = non_negative_latency();
    let costs = problem_example_costs();
    // Only A; B is exact by every target here except through DDSketch, so
    // it is left without (K).
    for (target, expected_kll) in [
        (0.01, smallest_kll_within(0.01)),
        (0.5, parameter_list(SketchAlgorithm::Kll)[0].clone()),
    ] {
        let entries = [
            entry_with(AccuracyTarget::max_error(target).accuracy_metric(AccuracyMetric::Rank)),
            entry(0),
        ];
        let root_limits = [
            Some(RootAccuracyLimits::of(
                &entries[0].requirements.accuracy,
                Some(1),
            )),
            None,
        ];
        let accuracy = Accuracy {
            data: &data,
            root_limits: &root_limits,
        };
        let solved = solve_under(&physical, &costs, &[], &entries, &accuracy).unwrap();
        assert_eq!(
            selected_parameters(&physical, &solved, 1..=KLL_SIZES),
            vec![expected_kll],
            "target {target}"
        );
    }
}

/// On the problem example under (K), with many cost assignments and
/// targets, the MILP's optimum equals the cheapest DAG of the reference
/// enumeration whose composed errors meet every root limit.
#[test]
fn the_milp_agrees_with_the_reference_enumeration_under_accuracy() {
    let physical = problem_example();
    let data = non_negative_latency();
    let mut next = pseudo_random_costs();
    for round in 0..12 {
        let costs: Vec<Option<f64>> = (0..physical.physical_candidates.len())
            .map(|_| Some(next()))
            .collect();
        let a = [0.005, 0.01, 0.05][round % 3];
        let b = [0.03, 0.05, 0.25, 0.01][round % 4];
        let entries = [
            entry_with(AccuracyTarget::max_error(a).accuracy_metric(AccuracyMetric::Rank)),
            entry_with(AccuracyTarget::max_error(b)),
        ];
        let root_limits: Vec<_> = entries
            .iter()
            .map(|entry| {
                Some(RootAccuracyLimits::of(
                    &entry.requirements.accuracy,
                    Some(1),
                ))
            })
            .collect();
        let accuracy = Accuracy {
            data: &data,
            root_limits: &root_limits,
        };
        agrees_with_the_reference_under(&physical, &costs, &[], &entries, &accuracy);
    }
}
