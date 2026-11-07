//! Stage 2's tests. Each builds Pass 2's output, the memo and its logical
//! edges, through `Memo::find_or_add_group`, `Memo::add_candidate` and
//! `Memo::add_query`, and reads the physical candidates, edges and pruned
//! alternatives; no other stage crate is called.
use std::collections::BTreeSet;

use asap_types::ir::operator::operator_properties::{BinaryOpKind, Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchParams,
    SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, BinaryOperator, NonASAPOp, Operator, QueryRoot, ScalarExpr};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    parameter_list, CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId,
    ExecutionContext, LogicalASAPMemoCandidates, LogicalEdge, LogicalProperties,
    LogicalRequirements, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PruneReason, WindowLayout,
};
use asap_types::workload::{
    DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval, TimestampMs,
};
use ExecutionTiming::{IngestionTime, QueryTime};

use super::generate_physical_asap_memo_candidates;

// ── Fixtures ──────────────────────────────────────────────────────────────

const MEMORY: StorageMedium = StorageMedium::Memory;
const QT_NONE: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::None);
const QT_ONE: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::OneEvaluation(MEMORY));
const QT_ACROSS: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::AcrossEvaluations(MEMORY));
const IT_NONE: (ExecutionTiming, Lifetime) = (IngestionTime, Lifetime::None);
const IT_ACROSS: (ExecutionTiming, Lifetime) = (IngestionTime, Lifetime::AcrossEvaluations(MEMORY));

/// Adds `operator` with `parameters` and `window_layout` to `group`, as
/// Pass 1 or Pass 2 does.
fn candidate(
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

/// The group of `operator`, with `operator` as its exact candidate.
fn group(memo: &mut Memo, operator: Operator<MemoGroupId>) -> CandidateForMemoGroupId {
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    candidate(memo, group, operator, None, None)
}

fn scan_operator() -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
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
    })
}

/// The running example's query A over one scan: a scan group, a KLL state
/// group with an unwindowed candidate and one of 10 s tumbling windows, and
/// the p95 readout group, the query's root. Each edge's $\alpha^{\log}$ is
/// true except the readout's from the unwindowed state when
/// `unwindowed_state_satisfies_the_readout` is false.
struct Example {
    logical: LogicalASAPMemoCandidates,
    scan: CandidateForMemoGroupId,
    unwindowed_state: CandidateForMemoGroupId,
    windowed_state: CandidateForMemoGroupId,
    readout: CandidateForMemoGroupId,
}

fn example(unwindowed_state_satisfies_the_readout: bool) -> Example {
    example_with_root(unwindowed_state_satisfies_the_readout, QueryRoot::Operator)
}

/// [`example`] with the query's root `root(readout group)`.
fn example_with_root(
    unwindowed_state_satisfies_the_readout: bool,
    root: impl Fn(MemoGroupId) -> QueryRoot<MemoGroupId>,
) -> Example {
    let mut memo = Memo::default();
    let scan = group(&mut memo, scan_operator());
    let build = Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan.group,
        family: FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
    });
    let states = memo.find_or_add_group(build.clone()).unwrap();
    let parameters = Some(parameter_list(SketchAlgorithm::Kll)[0].clone());
    let unwindowed_state = candidate(&mut memo, states, build.clone(), parameters.clone(), None);
    let windowed_state = candidate(
        &mut memo,
        states,
        build,
        parameters,
        Some(WindowLayout::Tumbling { width_ms: 10_000 }),
    );
    let readout = group(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: states,
            query: SketchStatistic::Quantile { q: 0.95 },
        }),
    );
    memo.add_query(root(readout.group));
    let edge = |producer, consumer, satisfies_logical_requirement| LogicalEdge {
        producer_candidate_id: producer,
        consumer_candidate_id: consumer,
        input_index_of_consumer: 0,
        satisfies_logical_requirement,
    };
    let edges = vec![
        edge(scan, unwindowed_state, true),
        edge(scan, windowed_state, true),
        edge(
            unwindowed_state,
            readout,
            unwindowed_state_satisfies_the_readout,
        ),
        edge(windowed_state, readout, true),
    ];
    Example {
        logical: LogicalASAPMemoCandidates { memo, edges },
        scan,
        unwindowed_state,
        windowed_state,
        readout,
    }
}

fn ingesting() -> DataWorkload {
    DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1000.0,
        },
        ..DataWorkload::default()
    }
}

/// Stage 2 over `logical`, checking what Stage 3 relies on: every emitted
/// candidate has an admissible edge into each input, and the edges are
/// grouped by (consumer, input) over emitted candidates.
fn generate(
    logical: &LogicalASAPMemoCandidates,
    data: &DataWorkload,
    query_time_results_retained: bool,
    raw_data_retained: bool,
) -> PhysicalASAPMemoCandidates {
    let physical = generate_physical_asap_memo_candidates(
        logical,
        data,
        query_time_results_retained,
        raw_data_retained,
    );
    let count = physical.physical_candidates.len();
    let consumer_inputs: Vec<_> = physical
        .edges()
        .map(|(_, edge)| {
            assert!(edge.producer_candidate_id.0 < count && edge.consumer_candidate_id.0 < count);
            (edge.consumer_candidate_id, edge.input_index_of_consumer)
        })
        .collect();
    assert!(consumer_inputs.is_sorted(), "grouped by (consumer, input)");
    for (id, candidate) in physical.physical_candidates.iter().enumerate() {
        let inputs = logical
            .memo
            .candidate(candidate.logical_candidate_id)
            .expression
            .operator
            .children()
            .len();
        for input in 0..inputs {
            assert!(
                !physical
                    .admissible_edges_into(PhysicalCandidateId(id), input)
                    .is_empty(),
                "{candidate:?} input {input}"
            );
        }
    }
    physical
}

/// The deployment of most tests: ingesting data, query-time results
/// retained, raw data retained.
fn stage2(logical: &LogicalASAPMemoCandidates) -> PhysicalASAPMemoCandidates {
    generate(logical, &ingesting(), true, true)
}

fn properties((timing, lifetime): (ExecutionTiming, Lifetime)) -> PhysicalProperties {
    PhysicalProperties::new(timing, lifetime).unwrap()
}

/// The physical properties of `logical_candidate`'s emitted alternatives.
fn offered(
    physical: &PhysicalASAPMemoCandidates,
    logical_candidate: CandidateForMemoGroupId,
) -> Vec<PhysicalProperties> {
    physical
        .physical_candidates
        .iter()
        .filter(|candidate| candidate.logical_candidate_id == logical_candidate)
        .map(|candidate| candidate.physical_properties)
        .collect()
}

/// The pruned alternatives of `logical_candidate`, with their reasons.
fn pruned(
    physical: &PhysicalASAPMemoCandidates,
    logical_candidate: CandidateForMemoGroupId,
) -> Vec<(PhysicalProperties, PruneReason)> {
    physical
        .pruned_candidates
        .iter()
        .filter(|pruned| pruned.candidate.logical_candidate_id == logical_candidate)
        .inspect(|pruned| assert_eq!(pruned.group, logical_candidate.group))
        .map(|pruned| (pruned.candidate.physical_properties, pruned.reason))
        .collect()
}

fn id_of(
    physical: &PhysicalASAPMemoCandidates,
    logical_candidate: CandidateForMemoGroupId,
    state: (ExecutionTiming, Lifetime),
) -> PhysicalCandidateId {
    let position = physical
        .physical_candidates
        .iter()
        .position(|candidate| {
            candidate.logical_candidate_id == logical_candidate
                && candidate.physical_properties == properties(state)
        })
        .unwrap_or_else(|| panic!("{logical_candidate:?} at {state:?} is not offered"));
    PhysicalCandidateId(position)
}

// ── Decision-tree levels 4 and 5 ───────────────────────────────────────────

/// Levels 4 and 5 over the running example when the deployment allows
/// everything: every logical candidate's valid (τ, ℓ) are tried, in timing
/// then lifetime order. Materialized across evaluations needs a window
/// layout; an ingestion-time result that is not materialized is read only
/// at ingestion time, so only the windowed state's ingestion-time build
/// reads the ingestion-time scan, and an unmaterialized ingestion-time state
/// or readout reaches no query output.
#[test]
fn every_logical_candidate_expands_into_its_valid_timings_and_lifetimes() {
    let example = example(true);
    let physical = stage2(&example.logical);
    use PruneReason::{AcrossEvaluationsWithoutWindowLayout as NoLayout, ReachesNoQueryOutput};
    let all = |states: &[(ExecutionTiming, Lifetime)]| -> Vec<PhysicalProperties> {
        states.iter().copied().map(properties).collect()
    };
    let reasons = |states: &[((ExecutionTiming, Lifetime), PruneReason)]| {
        states
            .iter()
            .map(|&(state, reason)| (properties(state), reason))
            .collect::<Vec<_>>()
    };
    // Unmaterialized and one-evaluation candidates without a window layout.
    for unwindowed in [example.scan, example.unwindowed_state, example.readout] {
        let expected_offered = if unwindowed == example.scan {
            vec![QT_NONE, QT_ONE, IT_NONE]
        } else {
            vec![QT_NONE, QT_ONE]
        };
        assert_eq!(
            offered(&physical, unwindowed),
            all(&expected_offered),
            "{unwindowed:?}"
        );
    }
    assert_eq!(
        pruned(&physical, example.scan),
        reasons(&[(QT_ACROSS, NoLayout), (IT_ACROSS, NoLayout)])
    );
    for not_read_at_ingestion in [example.unwindowed_state, example.readout] {
        let mut expected = reasons(&[(QT_ACROSS, NoLayout), (IT_ACROSS, NoLayout)]);
        expected.push((properties(IT_NONE), ReachesNoQueryOutput));
        assert_eq!(pruned(&physical, not_read_at_ingestion), expected);
    }
    // The windowed state: every valid (τ, ℓ).
    assert_eq!(
        offered(&physical, example.windowed_state),
        all(&[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS])
    );
    assert_eq!(
        pruned(&physical, example.windowed_state),
        reasons(&[(IT_NONE, ReachesNoQueryOutput)])
    );
}

/// The invalid combination (IngestionTime, OneEvaluation) is rejected by
/// `PhysicalProperties::new`, so Stage 2 neither offers nor prunes it; and
/// no materialized result is stored on disk (#1217).
#[test]
fn the_invalid_combination_and_disk_are_never_alternatives() {
    assert!(PhysicalProperties::new(IngestionTime, Lifetime::OneEvaluation(MEMORY)).is_err());
    let example = example(true);
    let physical = stage2(&example.logical);
    let every_state = physical
        .physical_candidates
        .iter()
        .map(|candidate| candidate.physical_properties)
        .chain(
            physical
                .pruned_candidates
                .iter()
                .map(|pruned| pruned.candidate.physical_properties),
        );
    for state in every_state {
        assert!(
            !(state.timing() == IngestionTime
                && matches!(state.lifetime(), Lifetime::OneEvaluation(_))),
            "{state:?}"
        );
        assert!(
            !matches!(
                state.lifetime(),
                Lifetime::OneEvaluation(StorageMedium::Disk)
                    | Lifetime::AcrossEvaluations(StorageMedium::Disk)
            ),
            "{state:?}"
        );
    }
    // Each logical candidate's five valid memory states are either offered
    // or pruned, once.
    for logical_candidate in [
        example.scan,
        example.unwindowed_state,
        example.windowed_state,
        example.readout,
    ] {
        let states: BTreeSet<String> = offered(&physical, logical_candidate)
            .into_iter()
            .chain(
                pruned(&physical, logical_candidate)
                    .into_iter()
                    .map(|(state, _)| state),
            )
            .map(|state| format!("{state:?}"))
            .collect();
        assert_eq!(states.len(), 5, "{logical_candidate:?}");
    }
}

/// Level 4: without an ingestion pipeline (data at rest, or an unknown
/// arrival) every ingestion-time alternative is pruned; data that is
/// partly stored and partly arriving has one.
#[test]
fn ingestion_time_needs_an_ingestion_pipeline() {
    let example = example(true);
    for (arrival, has_pipeline) in [
        (DataArrival::AtRest, false),
        (DataArrival::Unknown, false),
        (
            DataArrival::Mixed {
                rows_per_second: 10.0,
            },
            true,
        ),
    ] {
        let data = DataWorkload {
            arrival,
            ..DataWorkload::default()
        };
        let physical = generate(&example.logical, &data, true, true);
        assert_eq!(
            physical
                .physical_candidates
                .iter()
                .any(|candidate| candidate.physical_properties.timing() == IngestionTime),
            has_pipeline,
            "{arrival:?}"
        );
        let pruned_for_no_pipeline: Vec<_> = physical
            .pruned_candidates
            .iter()
            .filter(|pruned| pruned.reason == PruneReason::NoIngestionPipeline)
            .map(|pruned| pruned.candidate.physical_properties.timing())
            .collect();
        if has_pipeline {
            assert!(pruned_for_no_pipeline.is_empty());
        } else {
            // Both ingestion-time lifetimes of each of the four candidates.
            assert_eq!(pruned_for_no_pipeline, vec![IngestionTime; 8]);
        }
    }
}

/// Level 5: a query-time result is materialized across evaluations only
/// when the deployment keeps query-time results; the ingestion-time one
/// stays.
#[test]
fn query_time_across_evaluations_needs_retained_query_time_results() {
    let example = example(true);
    let physical = generate(&example.logical, &ingesting(), false, true);
    assert_eq!(
        offered(&physical, example.windowed_state),
        vec![
            properties(QT_NONE),
            properties(QT_ONE),
            properties(IT_ACROSS)
        ]
    );
    assert!(pruned(&physical, example.windowed_state).contains(&(
        properties(QT_ACROSS),
        PruneReason::QueryTimeResultsNotRetained
    )));
}

/// Without raw data retained, the scan runs only at ingestion time; the
/// query-time states then have no buildable producer, and the readout reads
/// the windowed state materialized at ingestion.
#[test]
fn without_raw_data_only_ingestion_time_builds_read_the_scan() {
    let example = example(true);
    let physical = generate(&example.logical, &ingesting(), true, false);
    assert_eq!(offered(&physical, example.scan), vec![properties(IT_NONE)]);
    for state in [QT_NONE, QT_ONE] {
        assert!(pruned(&physical, example.scan)
            .contains(&(properties(state), PruneReason::RawDataNotRetained)));
    }
    assert!(offered(&physical, example.unwindowed_state).is_empty());
    for state in [QT_NONE, QT_ONE, QT_ACROSS] {
        assert!(pruned(&physical, example.windowed_state)
            .contains(&(properties(state), PruneReason::InputHasNoBuildableProducer)));
    }
    assert_eq!(
        offered(&physical, example.windowed_state),
        vec![properties(IT_ACROSS)]
    );
    assert_eq!(
        offered(&physical, example.readout),
        vec![properties(QT_NONE), properties(QT_ONE)]
    );
}

/// An exact per-entity aggregate the executor has no kernel for
/// (`stddev_over_time`) is pruned at every timing and lifetime.
#[test]
fn a_per_entity_aggregate_without_a_kernel_is_pruned_whole() {
    let mut memo = Memo::default();
    let scan = group(&mut memo, scan_operator());
    let aggregate = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::StdDev {
                col: Some(1),
                population: true,
            }],
            output_names: vec!["stddev".into()],
            filters: vec![None],
            having: None,
            child: scan.group,
        }),
    );
    memo.add_query(QueryRoot::Operator(aggregate.group));
    let logical = LogicalASAPMemoCandidates {
        memo,
        edges: vec![LogicalEdge {
            producer_candidate_id: scan,
            consumer_candidate_id: aggregate,
            input_index_of_consumer: 0,
            satisfies_logical_requirement: true,
        }],
    };
    let physical = stage2(&logical);
    assert!(offered(&physical, aggregate).is_empty());
    let reasons = pruned(&physical, aggregate);
    assert_eq!(reasons.len(), 5);
    assert!(reasons
        .iter()
        .all(|(_, reason)| *reason == PruneReason::PerEntityAggregateNotExecutable));
    // The scan now reaches no query output.
    assert!(offered(&physical, scan).is_empty());
}

// ── Physical edges ─────────────────────────────────────────────────────────

/// Each logical edge becomes one physical edge per pair of emitted
/// alternatives, grouped by (consumer, input), with $\alpha^{\log}$ copied
/// from the logical edge and $\alpha^{\mathrm{phys}}$ from timing and
/// lifetime; the admissible accessors return exactly the edges with both.
#[test]
fn physical_edges_carry_both_compatibilities() {
    let example = example(true);
    let physical = stage2(&example.logical);
    let edges: Vec<_> = physical.edges().collect();
    // Scan (3 alternatives) → unwindowed state (2) and windowed state (4);
    // states (2 + 4) → readout (2).
    assert_eq!(edges.len(), 3 * 2 + 3 * 4 + (2 + 4) * 2);
    for (_, edge) in &edges {
        let producer = physical.physical_candidates[edge.producer_candidate_id.0];
        let consumer = physical.physical_candidates[edge.consumer_candidate_id.0];
        assert!(edge.satisfies_logical_requirement);
        assert_eq!(
            edge.satisfies_physical_requirement,
            asap_types::stages::producer_satisfies_physical_requirements(&producer, &consumer)
        );
    }
    // The ingestion-time scan feeds only the windowed state's ingestion
    // build; the query-time one every query-time state.
    let scan_at_ingestion = id_of(&physical, example.scan, IT_NONE);
    let windowed_at_ingestion = id_of(&physical, example.windowed_state, IT_ACROSS);
    let into_ingestion_build = physical.admissible_edges_into(windowed_at_ingestion, 0);
    assert_eq!(into_ingestion_build.len(), 1);
    assert_eq!(
        edges[into_ingestion_build[0].0].1.producer_candidate_id,
        scan_at_ingestion
    );
    let windowed_query_time = id_of(&physical, example.windowed_state, QT_NONE);
    assert!(edges.iter().any(|(_, edge)| {
        edge.producer_candidate_id == scan_at_ingestion
            && edge.consumer_candidate_id == windowed_query_time
            && !edge.satisfies_physical_requirement
    }));
    assert_eq!(physical.admissible_edges_out_of(scan_at_ingestion).len(), 1);
    // The readout reads every emitted state alternative: each state is
    // query-time or materialized at ingestion.
    let readout = id_of(&physical, example.readout, QT_NONE);
    assert_eq!(physical.admissible_edges_into(readout, 0).len(), 2 + 4);
    assert!(physical.admissible_edges_into(readout, 1).is_empty());
    for (id, edge) in &edges {
        let admissible = edge.satisfies_logical_requirement && edge.satisfies_physical_requirement;
        assert_eq!(
            physical
                .admissible_edges_into(edge.consumer_candidate_id, edge.input_index_of_consumer)
                .contains(id),
            admissible
        );
        assert_eq!(
            physical
                .admissible_edges_out_of(edge.producer_candidate_id)
                .contains(id),
            admissible
        );
    }
}

/// A logical edge whose producer does not satisfy the consumer's logical
/// requirement keeps its physical edges with $\alpha^{\log}$ false; with no
/// other consumer, the producer's alternatives reach no query output.
#[test]
fn an_unsatisfied_logical_requirement_is_not_admissible() {
    let example = example(false);
    let physical = stage2(&example.logical);
    assert!(offered(&physical, example.unwindowed_state).is_empty());
    assert!(pruned(&physical, example.unwindowed_state)
        .iter()
        .any(|&(state, reason)| state == properties(QT_NONE)
            && reason == PruneReason::ReachesNoQueryOutput));
    let readout = id_of(&physical, example.readout, QT_NONE);
    assert_eq!(physical.admissible_edges_into(readout, 0).len(), 4);
    // Pruned producers have no physical edges.
    assert!(physical.edges().all(|(_, edge)| {
        physical.physical_candidates[edge.producer_candidate_id.0].logical_candidate_id
            != example.unwindowed_state
    }));
}

/// $\alpha^{\mathrm{phys}}$ ignores alignment; Stage 3 checks it per
/// execution context with `delivers_on` on the emitted candidates: the
/// windowed state materialized across evaluations (10 s windows) delivers
/// to a query every 10 s at phase 0, not at phase 5 s, while its
/// unmaterialized alternative runs in any context.
#[test]
fn alignment_is_checked_per_context_with_delivers_on() {
    let example = example(true);
    let physical = stage2(&example.logical);
    let memo = &physical.logical_candidates.memo;
    let every_10s_at = |phase_ms| ExecutionContext::Evaluation {
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(10_000),
            evaluation_phase: TimestampMs(phase_ms),
        }),
        range_evaluation: None,
    };
    for state in [QT_ACROSS, IT_ACROSS] {
        let materialized =
            physical.physical_candidates[id_of(&physical, example.windowed_state, state).0];
        assert!(materialized.delivers_on(memo, &every_10s_at(0)));
        assert!(!materialized.delivers_on(memo, &every_10s_at(5_000)));
    }
    let unmaterialized =
        physical.physical_candidates[id_of(&physical, example.windowed_state, QT_NONE).0];
    assert!(unmaterialized.delivers_on(memo, &every_10s_at(5_000)));
}

// ── Pruning ────────────────────────────────────────────────────────────────

/// A query whose root group is also read at ingestion time: the scan
/// group's unmaterialized ingestion-time alternative is not read by the
/// query, but by the windowed state's ingestion-time build, so it is
/// emitted.
#[test]
fn a_root_group_read_at_ingestion_time_keeps_its_ingestion_time_alternative() {
    let mut example = example(true);
    example
        .logical
        .memo
        .add_query(QueryRoot::Operator(example.scan.group));
    let physical = stage2(&example.logical);
    assert_eq!(
        offered(&physical, example.scan),
        vec![properties(QT_NONE), properties(QT_ONE), properties(IT_NONE)]
    );
}

/// A scalar root reaches the groups it reads: the readout under
/// `scalar(…)` is offered as under an operator root.
#[test]
fn a_scalar_root_reaches_the_groups_it_reads() {
    let example = example_with_root(true, |readout| {
        QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(readout))
    });
    let physical = stage2(&example.logical);
    assert_eq!(
        offered(&physical, example.readout),
        vec![properties(QT_NONE), properties(QT_ONE)]
    );
}

/// A two-input consumer whose second input group has no candidate cannot
/// be built, nor can the readout of that empty group; the first input's
/// producers stay, read by the first query.
#[test]
fn a_consumer_without_a_producer_for_one_input_cannot_be_built() {
    let mut example = example(true);
    let memo = &mut example.logical.memo;
    let empty_states = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryAgg {
            child: example.scan.group,
            family: FieldDataType::Sketch(SketchAlgorithm::DDSketch, GroupingStrategy::default()),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            grouping: GroupingStrategy::default(),
            filter: None,
        }))
        .unwrap();
    let median = group(
        memo,
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: empty_states,
            query: SketchStatistic::Quantile { q: 0.5 },
        }),
    );
    let ratio = group(
        memo,
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                vector_match: None,
            },
            return_bool: false,
            lhs: example.readout.group,
            rhs: median.group,
        }),
    );
    memo.add_query(QueryRoot::Operator(ratio.group));
    for (producer, input_index_of_consumer) in [(example.readout, 0), (median, 1)] {
        example.logical.edges.push(LogicalEdge {
            producer_candidate_id: producer,
            consumer_candidate_id: ratio,
            input_index_of_consumer,
            satisfies_logical_requirement: true,
        });
    }
    let physical = stage2(&example.logical);
    assert!(offered(&physical, median).is_empty());
    assert!(offered(&physical, ratio).is_empty());
    for unbuildable in [median, ratio] {
        assert_eq!(
            pruned(&physical, unbuildable)
                .into_iter()
                .filter(|(_, reason)| *reason == PruneReason::InputHasNoBuildableProducer)
                .map(|(state, _)| state)
                .collect::<Vec<_>>(),
            vec![properties(QT_NONE), properties(QT_ONE), properties(IT_NONE)],
            "{unbuildable:?}"
        );
    }
    assert_eq!(
        offered(&physical, example.readout),
        vec![properties(QT_NONE), properties(QT_ONE)]
    );
}
