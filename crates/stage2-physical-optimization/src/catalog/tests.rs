//! Stage 2's tests. Each builds Pass 2's output, the memo and its logical
//! edges, through `Memo::find_or_add_group`, `Memo::add_candidate` and
//! `Memo::add_query`, and reads the physical candidates, edges and pruned
//! alternatives; no other stage crate is called.
use std::collections::BTreeMap;
use std::time::Duration;

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::operator::operator_properties::{
    AtModifier, Reduction, Source, SourceColumn, TimeShift,
};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{ColumnRef, ScalarValue};
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, FieldDataType, SketchAlgorithm, SketchStatistic, SummaryInputExpr,
    SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, QueryRoot, ScalarExpr, TimeRangeKind,
};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    parameter_list, CandidateForMemoGroupId, EvaluationGrid, LogicalASAPMemoCandidates,
    LogicalEdge, Memo, MemoGroupId, MemoQuery, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    PruneCause, PruneReason, PrunedLogicalCandidate, RootPruneReason, ValueSign,
};
use asap_types::test_support::{
    add_candidate, group_with_own_candidate, series_schema, time_series_scan,
};
use asap_types::workload::{DataArrival, DataWorkload};
use ExecutionTiming::{IngestionTime, QueryTime};

use super::generate_physical_asap_memo_candidates;

// ── Fixtures ──────────────────────────────────────────────────────────────

const MEMORY: StorageMedium = StorageMedium::Memory;
const QT_NONE: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::None);
const QT_ONE: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::OneEvaluation(MEMORY));
const QT_ACROSS: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::AcrossEvaluations(MEMORY));
const IT_NONE: (ExecutionTiming, Lifetime) = (IngestionTime, Lifetime::None);
const IT_ACROSS: (ExecutionTiming, Lifetime) = (IngestionTime, Lifetime::AcrossEvaluations(MEMORY));

const GRID_60S: Option<EvaluationGrid> = Some(EvaluationGrid {
    period_ms: 60_000,
    phase_ms: 0,
});

/// The candidate `index_in_group` of `group`.
fn id(group: MemoGroupId, index_in_group: usize) -> CandidateForMemoGroupId {
    CandidateForMemoGroupId {
        group,
        index_in_group,
    }
}

/// The edge from `producer` into input `input` of `consumer`.
fn edge(
    producer: CandidateForMemoGroupId,
    consumer: CandidateForMemoGroupId,
    input: usize,
    satisfies_logical_requirement: bool,
) -> LogicalEdge {
    LogicalEdge {
        producer_candidate_id: producer,
        consumer_candidate_id: consumer,
        input_index_of_consumer: input,
        satisfies_logical_requirement,
    }
}

/// `child[seconds]`, a range relative to the evaluation time.
fn range(seconds: u64, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(seconds),
        kind: TimeRangeKind::Range,
        child,
    })
}

fn kll_build(child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    })
}

/// The group of `operator`, with one candidate of `operator` per θ in
/// `parameters` (the first `count` of the algorithm's list).
fn sketch_group(
    memo: &mut Memo,
    operator: Operator<MemoGroupId>,
    algorithm: SketchAlgorithm,
    count: usize,
) -> MemoGroupId {
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    for parameters in &parameter_list(algorithm)[..count] {
        add_candidate(memo, group, operator.clone(), Some(parameters.clone())).unwrap();
    }
    group
}

fn query(memo: &mut Memo, root: QueryRoot<MemoGroupId>) {
    memo.add_query(MemoQuery {
        root,
        accuracy_target: None,
        values_per_evaluation: None,
    });
}

/// The running example's query A, `quantile_over_time(0.95, latency[1m])`
/// evaluated every 60 s at phase 0, after Pass 2: the scan; its last
/// minute; the KLL state group, with candidate 0 a KLL build over that
/// minute and candidate 1 a merge of the windows a `TimeRange` selects
/// from a 60 s chain over the scan; and the p95 readout, the query's root.
/// Each edge's $\alpha^{\log}$ is true. Groups read by the query evaluate
/// on its 60 s grid; the scan and the chain do not depend on the
/// evaluation time.
struct Example {
    logical: LogicalASAPMemoCandidates,
    scan: CandidateForMemoGroupId,
    last_minute: CandidateForMemoGroupId,
    build: CandidateForMemoGroupId,
    merge: CandidateForMemoGroupId,
    chain: CandidateForMemoGroupId,
    windows: CandidateForMemoGroupId,
    readout: CandidateForMemoGroupId,
}

fn example() -> Example {
    let mut memo = Memo::default();
    let scan = group_with_own_candidate(
        &mut memo,
        time_series_scan("latency", series_schema(&["job"]), vec![]),
    );
    let last_minute = group_with_own_candidate(&mut memo, range(60, scan));
    let state = sketch_group(&mut memo, kll_build(last_minute), SketchAlgorithm::Kll, 1);
    let chain = sketch_group(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: scan,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 60_000,
        }),
        SketchAlgorithm::Kll,
        1,
    );
    // The windows and the merge carry the chain build's θ.
    let windows = sketch_group(&mut memo, range(60, chain), SketchAlgorithm::Kll, 1);
    let merge = add_candidate(
        &mut memo,
        state,
        Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![windows],
            group_by: None,
        }),
        Some(parameter_list(SketchAlgorithm::Kll)[0].clone()),
    )
    .unwrap();
    let readout = group_with_own_candidate(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: SketchStatistic::Quantile { q: 0.95 },
        }),
    );
    query(&mut memo, QueryRoot::Operator(readout));
    let [scan, last_minute, build, chain, windows, readout] =
        [scan, last_minute, state, chain, windows, readout].map(|group| id(group, 0));
    let edges = vec![
        edge(scan, last_minute, 0, true),
        edge(last_minute, build, 0, true),
        edge(windows, merge, 0, true),
        edge(scan, chain, 0, true),
        edge(chain, windows, 0, true),
        edge(build, readout, 0, true),
        edge(merge, readout, 0, true),
    ];
    let mut evaluation_grid_of_group = vec![GRID_60S; memo.groups().len()];
    evaluation_grid_of_group[scan.group.0 as usize] = None;
    evaluation_grid_of_group[chain.group.0 as usize] = None;
    Example {
        logical: LogicalASAPMemoCandidates {
            memo,
            edges,
            pruned: vec![],
            evaluation_grid_of_group,
        },
        scan,
        last_minute,
        build,
        merge,
        chain,
        windows,
        readout,
    }
}

fn ingesting(max_out_of_order_ms: Option<u64>) -> DataWorkload {
    DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1000.0,
            max_out_of_order_ms,
        },
        ..DataWorkload::default()
    }
}

/// Raw data and query-time results retained.
fn retaining_everything() -> DeploymentCapabilities {
    DeploymentCapabilities {
        query_time_results_retained: true,
        ..DeploymentCapabilities::default()
    }
}

/// Stage 2 over `logical`, checking what Stage 3 relies on: the edges are
/// grouped by (consumer, input) over emitted candidates, the admissible
/// indexes list exactly the admissible edges, and every emitted candidate
/// has an admissible edge into each input.
fn generate(
    logical: &LogicalASAPMemoCandidates,
    data: &DataWorkload,
    capabilities: &DeploymentCapabilities,
) -> PhysicalASAPMemoCandidates {
    let physical = generate_physical_asap_memo_candidates(logical, data, capabilities);
    let count = physical.physical_candidates.len();
    let mut consumer_inputs = Vec::new();
    for (edge_id, edge) in physical.edges() {
        assert!(edge.producer_candidate_id.0 < count && edge.consumer_candidate_id.0 < count);
        consumer_inputs.push((edge.consumer_candidate_id, edge.input_index_of_consumer));
        assert_eq!(
            physical
                .admissible_edges_into(edge.consumer_candidate_id, edge.input_index_of_consumer)
                .contains(&edge_id),
            edge.is_logically_and_physically_admissible()
        );
        assert_eq!(
            physical
                .admissible_edges_out_of(edge.producer_candidate_id)
                .contains(&edge_id),
            edge.is_logically_and_physically_admissible()
        );
    }
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
        .map(|pruned| (pruned.candidate.physical_properties, pruned.reason))
        .collect()
}

fn list(states: &[(ExecutionTiming, Lifetime)]) -> Vec<PhysicalProperties> {
    states.iter().copied().map(properties).collect()
}

fn with_reason(
    states: &[(ExecutionTiming, Lifetime)],
    reason: PruneReason,
) -> Vec<(PhysicalProperties, PruneReason)> {
    states
        .iter()
        .map(|&state| (properties(state), reason))
        .collect()
}

// ── Levels 3 and 4 ────────────────────────────────────────────────────────

/// Query A over ingesting data, everything retained. Time-dependent
/// candidates (the last minute, the build over it, the merge, the windows,
/// the readout) run only at query time; on the 60 s grid they may be
/// materialized across evaluations. The scan and the chain are
/// time-independent, so ingestion time is offered too; the scan has no
/// known grid, so it is not offered `AcrossEvaluations`. A summary build is
/// not offered (IT, None) (A4): fed at ingestion time it keeps its state,
/// so the chain is offered (IT, AcrossEvaluations) only; the scan's
/// (IT, None) feeds the chain built at ingestion time.
#[test]
fn offers_each_candidate_its_timings_and_lifetimes() {
    let example = example();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    let query_time = list(&[QT_NONE, QT_ONE, QT_ACROSS]);
    for candidate in [
        example.last_minute,
        example.build,
        example.merge,
        example.windows,
        example.readout,
    ] {
        assert_eq!(offered(&physical, candidate), query_time, "{candidate:?}");
        assert_eq!(pruned(&physical, candidate), vec![], "{candidate:?}");
    }
    assert_eq!(
        offered(&physical, example.scan),
        list(&[QT_NONE, QT_ONE, IT_NONE])
    );
    assert_eq!(
        offered(&physical, example.chain),
        list(&[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS])
    );
    assert_eq!(pruned(&physical, example.chain), vec![]);
}

/// Decided 2026-11-09 (A4): no summary build (`SummaryAgg`, chain,
/// `MaintainPopulation`) is offered (IngestionTime, None), offered or
/// pruned; the scan, which keeps nothing between batches, is.
#[test]
fn no_summary_build_is_offered_at_ingestion_time_without_a_lifetime() {
    let example = example();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    let memo = &example.logical.memo;
    let ingestion_time_none = properties(IT_NONE);
    let alternatives = physical
        .physical_candidates
        .iter()
        .map(|candidate| {
            (
                candidate.logical_candidate_id,
                candidate.physical_properties,
            )
        })
        .chain(physical.pruned_candidates.iter().map(|pruned| {
            (
                pruned.candidate.logical_candidate_id,
                pruned.candidate.physical_properties,
            )
        }));
    for (logical_candidate, physical_properties) in alternatives {
        let builds = matches!(
            memo.candidate(logical_candidate).expression.operator,
            Operator::ASAP(
                ASAPOp::SummaryAgg { .. }
                    | ASAPOp::SummaryTumblingWindowChain { .. }
                    | ASAPOp::MaintainPopulation { .. }
            )
        );
        assert!(
            !(builds && physical_properties == ingestion_time_none),
            "{logical_candidate:?}"
        );
    }
    assert!(offered(&physical, example.scan).contains(&ingestion_time_none));
}

/// Data at rest (or of unknown arrival) has no ingestion pipeline: every
/// ingestion-time alternative is pruned.
#[test]
fn without_an_ingestion_pipeline_nothing_runs_at_ingestion_time() {
    let example = example();
    for arrival in [DataArrival::AtRest, DataArrival::Unknown] {
        let data = DataWorkload {
            arrival,
            ..DataWorkload::default()
        };
        let physical = generate(&example.logical, &data, &retaining_everything());
        assert_eq!(
            pruned(&physical, example.scan),
            with_reason(&[IT_NONE], PruneReason::NoIngestionPipeline)
        );
        assert_eq!(
            pruned(&physical, example.chain),
            with_reason(&[IT_ACROSS], PruneReason::NoIngestionPipeline)
        );
        assert!(physical
            .physical_candidates
            .iter()
            .all(|candidate| candidate.physical_properties.timing() == QueryTime));
    }
}

/// Without raw data retained, the scan cannot run at query time, nor keep
/// its raw rows across evaluations at ingestion time; what reads it at
/// query time then has no buildable producer, so only the ingestion-time
/// chain serves the query.
#[test]
fn without_raw_data_only_ingestion_time_builds_read_the_scan() {
    let mut example = example();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..retaining_everything()
    };
    let physical = generate(&example.logical, &ingesting(None), &capabilities);
    assert_eq!(
        pruned(&physical, example.scan),
        with_reason(&[QT_NONE, QT_ONE], PruneReason::RawDataNotRetained)
    );
    assert_eq!(offered(&physical, example.scan), list(&[IT_NONE]));
    assert_eq!(offered(&physical, example.last_minute), vec![]);
    assert_eq!(offered(&physical, example.build), vec![]);
    assert_eq!(offered(&physical, example.chain), list(&[IT_ACROSS]));
    assert_eq!(
        offered(&physical, example.merge),
        list(&[QT_NONE, QT_ONE, QT_ACROSS])
    );
    assert_eq!(
        pruned(&physical, example.build),
        with_reason(
            &[QT_NONE, QT_ONE, QT_ACROSS],
            PruneReason::InputHasNoBuildableProducer
        )
    );

    // A scan on an evaluation grid is offered `AcrossEvaluations`; at
    // ingestion time that retains the raw rows it scans, so it is pruned
    // too.
    example.logical.evaluation_grid_of_group[example.scan.group.0 as usize] = GRID_60S;
    let physical = generate(&example.logical, &ingesting(None), &capabilities);
    assert_eq!(
        pruned(&physical, example.scan),
        with_reason(
            &[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS],
            PruneReason::RawDataNotRetained
        )
    );
    assert_eq!(offered(&physical, example.scan), list(&[IT_NONE]));
}

/// The default deployment retains no query-time result across
/// evaluations: every (QueryTime, AcrossEvaluations) is pruned, and
/// `OneEvaluation` stays.
#[test]
fn query_time_results_across_evaluations_need_the_deployment_to_retain_them() {
    let example = example();
    let physical = generate(
        &example.logical,
        &ingesting(None),
        &DeploymentCapabilities::default(),
    );
    for candidate in [example.build, example.merge, example.readout] {
        assert_eq!(
            pruned(&physical, candidate),
            with_reason(&[QT_ACROSS], PruneReason::QueryTimeResultsNotRetained),
            "{candidate:?}"
        );
        assert_eq!(offered(&physical, candidate), list(&[QT_NONE, QT_ONE]));
    }
}

/// `sum_over_time(m[1m] @ 1000)`: the `@ T` result is time-independent, so
/// it and its time-dependent input, the range under it, are offered
/// ingestion time; neither has a known grid, so neither is offered
/// `AcrossEvaluations`.
#[test]
fn an_at_t_subtree_is_offered_ingestion_time() {
    let mut memo = Memo::default();
    let scan =
        group_with_own_candidate(&mut memo, time_series_scan("m", series_schema(&[]), vec![]));
    let last_minute = group_with_own_candidate(&mut memo, range(60, scan));
    let pinned = group_with_own_candidate(
        &mut memo,
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 0,
                at: Some(AtModifier::Timestamp(1_000)),
            },
            child: last_minute,
        }),
    );
    query(&mut memo, QueryRoot::Operator(pinned));
    let [scan, last_minute, pinned] = [scan, last_minute, pinned].map(|group| id(group, 0));
    let logical = LogicalASAPMemoCandidates {
        evaluation_grid_of_group: vec![None; memo.groups().len()],
        memo,
        edges: vec![
            edge(scan, last_minute, 0, true),
            edge(last_minute, pinned, 0, true),
        ],
        pruned: vec![],
    };
    let physical = generate(&logical, &ingesting(None), &retaining_everything());
    for candidate in [last_minute, pinned] {
        assert_eq!(
            offered(&physical, candidate),
            list(&[QT_NONE, QT_ONE]),
            "{candidate:?}"
        );
        assert_eq!(
            pruned(&physical, candidate),
            with_reason(&[IT_NONE], PruneReason::ReachesNoQueryOutput),
            "{candidate:?}"
        );
    }
}

/// Decided 2026-11-09 (A7): `x @ 1700000000 [5m]` with no known horizon
/// end reads `x` back without bound, so the scan under the `@ T` is not
/// materialized across evaluations; it still runs at query time.
#[test]
fn a_subtree_under_an_absolute_anchor_is_not_materialized_across_evaluations() {
    let mut memo = Memo::default();
    let scan =
        group_with_own_candidate(&mut memo, time_series_scan("x", series_schema(&[]), vec![]));
    let pinned = group_with_own_candidate(
        &mut memo,
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 0,
                at: Some(AtModifier::Timestamp(1_700_000_000_000)),
            },
            child: scan,
        }),
    );
    let window = group_with_own_candidate(&mut memo, range(300, pinned));
    query(&mut memo, QueryRoot::Operator(window));
    let [scan, pinned, window] = [scan, pinned, window].map(|group| id(group, 0));
    let mut evaluation_grid_of_group = vec![GRID_60S; memo.groups().len()];
    evaluation_grid_of_group[pinned.group.0 as usize] = None;
    evaluation_grid_of_group[window.group.0 as usize] = None;
    let logical = LogicalASAPMemoCandidates {
        evaluation_grid_of_group,
        memo,
        edges: vec![edge(scan, pinned, 0, true), edge(pinned, window, 0, true)],
        pruned: vec![],
    };
    let physical = generate(&logical, &ingesting(None), &retaining_everything());
    let scan_offered = offered(&physical, scan);
    assert!(scan_offered.contains(&properties(QT_NONE)));
    assert!(!scan_offered
        .iter()
        .any(|p| matches!(p.lifetime(), Lifetime::AcrossEvaluations(_))));
    assert!(pruned(&physical, scan)
        .iter()
        .all(|(p, _)| !matches!(p.lifetime(), Lifetime::AcrossEvaluations(_))));
}

/// A rate state built over a SQL table (rows without a series order) is
/// offered ingestion time only when the data bounds how late a row
/// arrives; over PromQL samples, taken to arrive in order, it always is.
#[test]
fn a_counter_state_runs_at_ingestion_time_over_ordered_rows() {
    let rate_state = |memo: &mut Memo, source: Source| {
        let scan = group_with_own_candidate(
            memo,
            Operator::NonASAP(NonASAPOp::Scan {
                source,
                predicates: vec![],
                schema: series_schema(&[]),
            }),
        );
        let state = group_with_own_candidate(
            memo,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: scan,
                summary_type: FieldDataType::exact(ExactKind::Rate),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
                tumbling_window_width_ms: 60_000,
            }),
        );
        query(memo, QueryRoot::Operator(state));
        (id(scan, 0), id(state, 0))
    };
    let mut memo = Memo::default();
    let (table, table_state) = rate_state(
        &mut memo,
        Source::Table {
            table_ref: "events".into(),
        },
    );
    let (series, series_state) = rate_state(
        &mut memo,
        Source::TimeSeries {
            metric: "c_total".into(),
        },
    );
    let logical = LogicalASAPMemoCandidates {
        evaluation_grid_of_group: vec![None; memo.groups().len()],
        memo,
        edges: vec![
            edge(table, table_state, 0, true),
            edge(series, series_state, 0, true),
        ],
        pruned: vec![],
    };
    let at_ingestion = |physical: &PhysicalASAPMemoCandidates, state| {
        offered(physical, state).contains(&properties(IT_ACROSS))
    };
    let unbounded = generate(&logical, &ingesting(None), &retaining_everything());
    assert!(!at_ingestion(&unbounded, table_state));
    assert!(at_ingestion(&unbounded, series_state));
    let bounded = generate(&logical, &ingesting(Some(2_000)), &retaining_everything());
    assert!(at_ingestion(&bounded, table_state));
    assert!(at_ingestion(&bounded, series_state));
}

/// `topk(5, count by (job) (m))` as a Count-Min heap keyed by `job` and
/// weighted by the sample value: pruned in every alternative unless the
/// data declares `m`'s values non-negative, and then offered.
#[test]
fn a_count_min_build_needs_weights_proven_non_negative() {
    let heap = |declared: BTreeMap<SourceColumn, ValueSign>| {
        let mut memo = Memo::new(declared);
        let scan = group_with_own_candidate(
            &mut memo,
            time_series_scan("m", series_schema(&["job"]), vec![]),
        );
        let state = sketch_group(
            &mut memo,
            Operator::ASAP(ASAPOp::SummaryAgg {
                child: scan,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::CmsWithHeap),
                input: SummaryUpdate {
                    item: Some(SummaryInputExpr::EntityIdentity(
                        EntityIdentity::PromqlLabelSet { excluding: vec![] },
                    )),
                    weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                },
                reduction: Reduction::Reduce(Default::default()),
                filter: None,
            }),
            SketchAlgorithm::CmsWithHeap,
            1,
        );
        query(&mut memo, QueryRoot::Operator(state));
        let (scan, state) = (id(scan, 0), id(state, 0));
        let logical = LogicalASAPMemoCandidates {
            evaluation_grid_of_group: vec![None; memo.groups().len()],
            memo,
            edges: vec![edge(scan, state, 0, true)],
            pruned: vec![],
        };
        let physical = generate(&logical, &ingesting(None), &retaining_everything());
        (offered(&physical, state), pruned(&physical, state))
    };
    let (kept, unproven) = heap(BTreeMap::new());
    assert_eq!(kept, vec![]);
    assert_eq!(
        unproven,
        with_reason(
            &[QT_NONE, QT_ONE],
            PruneReason::CountMinWeightsNotProvenNonNegative
        )
    );
    let (kept, unproven) = heap(BTreeMap::from([(
        SourceColumn {
            source: "m".into(),
            column: "value".into(),
        },
        ValueSign::NonNegative,
    )]));
    assert_eq!(kept, list(&[QT_NONE, QT_ONE]));
    assert_eq!(unproven, vec![]);
}

// ── Edges and pruning ─────────────────────────────────────────────────────

/// Each logical edge becomes an edge between every pair of the two
/// candidates' emitted alternatives, with $\alpha^{\log}$ copied and
/// $\alpha^{\mathrm{phys}}$ from the producer's timing and lifetime and the
/// consumer's timing: the chain's four alternatives read the scan's three
/// (12 edges); a query-time chain reads only the query-time scan, and the
/// ingestion-time chain only the ingestion-time scan, which is not
/// materialized.
#[test]
fn edges_carry_both_requirements() {
    let example = example();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    let candidate = |id: PhysicalCandidateId| physical.physical_candidates[id.0];
    let into_chain: Vec<_> = physical
        .edges()
        .map(|(_, edge)| edge)
        .filter(|edge| candidate(edge.consumer_candidate_id).logical_candidate_id == example.chain)
        .collect();
    assert_eq!(into_chain.len(), 12);
    for edge in into_chain {
        let producer = candidate(edge.producer_candidate_id).physical_properties;
        let consumer = candidate(edge.consumer_candidate_id).physical_properties;
        assert!(edge.satisfies_logical_requirement);
        assert_eq!(
            edge.satisfies_physical_requirement,
            producer.timing() == consumer.timing(),
            "{producer:?} -> {consumer:?}"
        );
    }
}

/// A logical edge with $\alpha^{\log}$ false is kept but not admissible:
/// a readout whose only producer fails it has no buildable producer, and
/// what only it reads reaches no query output.
#[test]
fn an_input_without_an_admissible_producer_is_not_buildable() {
    let mut example = example();
    for logical_edge in &mut example.logical.edges {
        if logical_edge.consumer_candidate_id == example.readout {
            logical_edge.satisfies_logical_requirement = false;
        }
    }
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    assert!(physical.physical_candidates.is_empty());
    assert_eq!(
        pruned(&physical, example.readout),
        with_reason(
            &[QT_NONE, QT_ONE, QT_ACROSS],
            PruneReason::InputHasNoBuildableProducer
        )
    );
    assert_eq!(
        pruned(&physical, example.build),
        with_reason(
            &[QT_NONE, QT_ONE, QT_ACROSS],
            PruneReason::ReachesNoQueryOutput
        )
    );
}

/// A candidate Pass 2 eliminated is not expanded: the build over the last
/// minute, here eliminated with its input, leaves the merge as the
/// readout's only producer.
#[test]
fn candidates_pass_2_eliminated_are_not_expanded() {
    let mut example = example();
    let (build, last_minute) = (example.build, example.last_minute);
    example.logical.edges.retain(|edge| {
        ![edge.producer_candidate_id, edge.consumer_candidate_id].contains(&build)
            && edge.consumer_candidate_id != last_minute
    });
    example.logical.pruned = [build, last_minute]
        .map(|candidate| PrunedLogicalCandidate {
            candidate,
            causes: vec![PruneCause {
                through_pruned_candidates: vec![],
                root: RootPruneReason::NoConsumer,
            }],
        })
        .to_vec();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    for candidate in [build, last_minute] {
        assert_eq!(offered(&physical, candidate), vec![]);
        assert_eq!(pruned(&physical, candidate), vec![]);
    }
    assert_eq!(
        offered(&physical, example.readout),
        list(&[QT_NONE, QT_ONE, QT_ACROSS])
    );
}

/// A scalar query's operand is a query output too: `latency_p95 * 2` over
/// the readout keeps the readout's query-time alternatives.
#[test]
fn a_scalar_roots_operand_is_a_query_output() {
    let mut example = example();
    example.logical.memo.queries.clear();
    query(
        &mut example.logical.memo,
        QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(ScalarExpr::PromqlScalarFromVector(example.readout.group)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
            semantics: ExprSemantics::Promql,
        }),
    );
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    assert_eq!(
        offered(&physical, example.readout),
        list(&[QT_NONE, QT_ONE, QT_ACROSS])
    );
}
