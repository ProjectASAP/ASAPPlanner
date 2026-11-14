use super::*;
use crate::ir::properties::ExecutionTiming;
use crate::ir::{ASAPOp, Operator};
use crate::stages::{
    producer_satisfies_physical_requirements, CandidateForMemoGroupId, ExecutionContext, Lifetime,
    PhysicalProperties,
};
use crate::workload::{DataArrival, DataWorkload};

use crate::plan::materialized_view::RefreshWindow;

use crate::ir::operator::arguments::{AtModifier, GroupKeys, Reduction, Source};
use crate::ir::operator::AggIntent;
use crate::test_support::memo_fixtures::{aggregate, range, scan, shift};

use crate::ir::scalar::expr_ir::ColumnRef;

use crate::ir::schema::{DataType, Field, FieldDataType, Schema};
use crate::summary_type::{SketchAlgorithm, SketchParams, SummaryUpdate};

use crate::ir::{NonASAPOp, TimeRangeKind};

use crate::stages::StorageMedium;

use crate::stages::physical::execution_context::EvaluationGrid;

use crate::stages::{
    CandidateForMemoGroup, LogicalASAPMemoCandidates, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup,
};

use ExecutionTiming::{IngestionTime, QueryTime};

/// The KLL build over the scan: not a chain.
const BUILD: CandidateForMemoGroupId = CandidateForMemoGroupId {
    group: MemoGroupId(1),
    index_in_group: 0,
};

/// The chain of tumbling windows 5 s wide over the scan.
const WINDOWS_5S: CandidateForMemoGroupId = CandidateForMemoGroupId {
    group: MemoGroupId(2),
    index_in_group: 0,
};

/// The chain of tumbling windows 10 s wide over the scan.
const WINDOWS_10S: CandidateForMemoGroupId = CandidateForMemoGroupId {
    group: MemoGroupId(3),
    index_in_group: 0,
};

/// X = the scan's last 5 minutes (`m[5m]`): a result that depends on
/// the evaluation time and is not a chain.
const LAST_5M: CandidateForMemoGroupId = CandidateForMemoGroupId {
    group: MemoGroupId(4),
    index_in_group: 0,
};

/// Group 1, KLL states over the scan group 0, groups 2 and 3, chains
/// of KLL states over it 5 s and 10 s wide, and group 4, the scan's last
/// 5 minutes, each with its own expression as its one candidate. Built
/// through `find_or_add_group` and `add_candidate`.
fn memo() -> Memo {
    let mut memo = Memo::new(&DataWorkload::default());
    let scan = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Scan {
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
        }))
        .unwrap();
    let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let build = Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan,
        summary_type: kll.clone(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    });
    let chain = |tumbling_window_width_ms| {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: scan,
            summary_type: kll.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms,
        })
    };
    for operator in [build, chain(5_000), chain(10_000)] {
        let group = memo.find_or_add_group(operator.clone()).unwrap();
        let candidate = CandidateForMemoGroup::new(
            operator,
            Some(SketchParams::Kll { k: 269 }),
            OriginOfCandidateForMemoGroup::Pass1,
            vec![],
        );
        memo.add_candidate(group, candidate).unwrap();
    }
    let operator = Operator::NonASAP(NonASAPOp::TimeRange {
        range: std::time::Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: scan,
    });
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    let candidate =
        CandidateForMemoGroup::new(operator, None, OriginOfCandidateForMemoGroup::Pass1, vec![]);
    memo.add_candidate(group, candidate).unwrap();
    memo
}

/// Stage 2's candidates over `memo`, with `evaluation_grid_of_group`;
/// `delivers_on` reads nothing else.
fn candidates(
    memo: Memo,
    evaluation_grid_of_group: Vec<Option<EvaluationGrid>>,
) -> PhysicalASAPMemoCandidates {
    PhysicalASAPMemoCandidates::new(
        LogicalASAPMemoCandidates {
            memo,
            edges: vec![],
            pruned: vec![],
            evaluation_grid_of_group,
            exact_only_groups_of_query: vec![],
        },
        vec![],
        vec![],
        vec![],
    )
}

fn candidate(
    logical_candidate_id: CandidateForMemoGroupId,
    timing: ExecutionTiming,
    lifetime: Lifetime,
) -> PhysicalCandidate {
    PhysicalCandidate {
        logical_candidate_id,
        physical_properties: PhysicalProperties::new(timing, lifetime).unwrap(),
    }
}

fn evaluation(period_ms: u64, phase_ms: u64) -> ExecutionContext {
    ExecutionContext::Evaluation {
        grid: EvaluationGrid {
            period_ms,
            phase_ms,
        },
    }
}

fn ingestion(width_ms: u64, phase_ms: u64) -> ExecutionContext {
    ExecutionContext::IngestionPipeline(RefreshWindow { width_ms, phase_ms })
}

const MEMORY: StorageMedium = StorageMedium::Memory;

/// A `None` producer runs in the consumer's context and delivers on
/// every one; a `OneEvaluation` producer only on an evaluation context,
/// the one evaluation its result belongs to. Neither reads the memo.
#[test]
fn delivers_on_by_lifetime_without_windows() {
    let candidates = candidates(Memo::new(&DataWorkload::default()), vec![]);
    let unmaterialized = candidate(BUILD, QueryTime, Lifetime::None);
    let one_evaluation = candidate(BUILD, QueryTime, Lifetime::OneEvaluation(MEMORY));
    let materialized = ExecutionContext::Materialized(PhysicalCandidateId(0));
    for (producer, context, delivers) in [
        (&unmaterialized, evaluation(7_000, 3_000), true),
        (&unmaterialized, ingestion(5_000, 0), true),
        (&unmaterialized, materialized, true),
        (&one_evaluation, evaluation(7_000, 3_000), true),
        (&one_evaluation, ingestion(5_000, 0), false),
        (&one_evaluation, materialized, false),
    ] {
        assert_eq!(
            producer.delivers_on(&candidates, &context),
            delivers,
            "{:?} in {context:?}",
            producer.physical_properties
        );
    }
}

/// A chain producer with `AcrossEvaluations` of width $w$ (grid at phase
/// 0) delivers when $w$ divides the context's period (or refresh-window
/// width) and phase. With phases 0 and 5 s every 10 s, only w = 5 s
/// serves both.
#[test]
fn delivers_on_a_chain_when_its_width_divides_the_context_times() {
    let candidates = candidates(memo(), vec![None; 5]);
    let across_5s = candidate(WINDOWS_5S, QueryTime, Lifetime::AcrossEvaluations(MEMORY));
    let ingestion_10s = candidate(
        WINDOWS_10S,
        IngestionTime,
        Lifetime::AcrossEvaluations(MEMORY),
    );
    for (producer, context, delivers) in [
        // Evaluation grids: w divides period and phase.
        (&across_5s, evaluation(10_000, 0), true),
        (&across_5s, evaluation(10_000, 5_000), true),
        (&across_5s, evaluation(10_000, 2_000), false),
        (&across_5s, evaluation(7_000, 0), false),
        (&ingestion_10s, evaluation(10_000, 0), true),
        (&ingestion_10s, evaluation(10_000, 5_000), false),
        // Refresh windows: w divides width and phase.
        (&across_5s, ingestion(5_000, 0), true),
        (&across_5s, ingestion(10_000, 5_000), true),
        (&across_5s, ingestion(10_000, 2_500), false),
        (&ingestion_10s, ingestion(5_000, 0), false),
    ] {
        assert_eq!(
            producer.delivers_on(&candidates, &context),
            delivers,
            "{:?} {:?} in {context:?}",
            producer.logical_candidate_id,
            producer.physical_properties
        );
    }
}

/// A stored result that is not a chain delivers when its group's
/// evaluation grid holds every time of the context: X =
/// `sum_over_time(m[5m])`'s range, read every 60 s and every 120 s, is
/// evaluated on 60 s @ 0, so it serves both but not a 30 s grid or
/// another phase. Without a known grid it serves none.
#[test]
fn delivers_on_a_stored_result_on_its_group_grid() {
    let grid_60s = Some(EvaluationGrid {
        period_ms: 60_000,
        phase_ms: 0,
    });
    let on_grid = candidates(memo(), vec![None, None, None, None, grid_60s]);
    let without_grid = candidates(memo(), vec![None; 5]);
    let x = candidate(LAST_5M, QueryTime, Lifetime::AcrossEvaluations(MEMORY));
    for (candidates, context, delivers) in [
        (&on_grid, evaluation(60_000, 0), true),
        (&on_grid, evaluation(120_000, 0), true),
        (&on_grid, evaluation(120_000, 60_000), true),
        (&on_grid, evaluation(30_000, 0), false),
        (&on_grid, evaluation(60_000, 15_000), false),
        (&on_grid, ingestion(60_000, 0), true),
        (&without_grid, evaluation(60_000, 0), false),
    ] {
        assert_eq!(x.delivers_on(candidates, &context), delivers, "{context:?}");
    }
}

/// T = 1,700,000,000 s, in ms: the instant an `@ T` pins.
const AT_T: Option<AtModifier> = Some(AtModifier::Timestamp(1_700_000_000_000));

/// The top groups of the finality examples, each with its own expression as
/// its one candidate, over a memo whose data arrives as `arrival`.
struct FinalityExamples {
    candidates: PhysicalASAPMemoCandidates,
    /// SQL `SELECT count(*) FROM metrics`: no time bound.
    count_of_table: CandidateForMemoGroupId,
    /// `sum_over_time(latency[5m] @ T)`: a window that ends at T.
    sum_over_window_at_t: CandidateForMemoGroupId,
    /// `max_over_time((rate(latency[5m] @ end()))[10m:1m] @ T)`: `@ T`
    /// pins the subquery, but not the `@ end()` inside it.
    max_over_subquery_at_t: CandidateForMemoGroupId,
}

/// [`FinalityExamples`] over data arriving as `arrival`, every group
/// evaluated on the grid 60 s @ 0.
fn finality_examples(arrival: DataArrival) -> FinalityExamples {
    let mut memo = Memo::new(&DataWorkload {
        arrival,
        ..DataWorkload::default()
    });
    let mut add = |operator: Operator<MemoGroupId>| {
        let group = memo.find_or_add_group(operator.clone()).unwrap();
        let candidate = CandidateForMemoGroup::new(
            operator,
            None,
            OriginOfCandidateForMemoGroup::Pass1,
            vec![],
        );
        memo.add_candidate(group, candidate).unwrap();
        group
    };
    let first_candidate = |group| CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    };
    let table = add(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "metrics".into(),
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
    }));
    let count_of_table = add(aggregate(
        Reduction::Reduce(GroupKeys::none()),
        AggIntent::Count,
        table,
    ));
    let series = add(scan("latency", &[], vec![]));
    let at_t = add(shift(0, AT_T, series));
    let window_at_t = add(range(300_000, at_t));
    let sum_over_window_at_t = add(aggregate(
        Reduction::PerEntity,
        AggIntent::Sum { col: Some(1) },
        window_at_t,
    ));
    let window = add(range(300_000, series));
    let rate = add(aggregate(Reduction::PerEntity, AggIntent::Rate, window));
    let rate_at_end = add(shift(0, Some(AtModifier::End), rate));
    let subquery = add(Operator::NonASAP(NonASAPOp::PromqlSubquery {
        range: std::time::Duration::from_secs(600),
        resolution: Some(std::time::Duration::from_secs(60)),
        child: rate_at_end,
    }));
    let subquery_at_t = add(shift(0, AT_T, subquery));
    let max_over_subquery_at_t = add(aggregate(
        Reduction::PerEntity,
        AggIntent::Max { col: Some(1) },
        subquery_at_t,
    ));
    let grid_60s = Some(EvaluationGrid {
        period_ms: 60_000,
        phase_ms: 0,
    });
    let group_count = memo.groups().len();
    FinalityExamples {
        candidates: candidates(memo, vec![grid_60s; group_count]),
        count_of_table: first_candidate(count_of_table),
        sum_over_window_at_t: first_candidate(sum_over_window_at_t),
        max_over_subquery_at_t: first_candidate(max_over_subquery_at_t),
    }
}

/// Every arrival under which rows keep arriving: an unknown arrival counts
/// as arriving.
const ARRIVING: [DataArrival; 3] = [
    DataArrival::Ingesting {
        rows_per_second: 1_000.0,
        max_out_of_order_ms: None,
    },
    DataArrival::Mixed {
        rows_per_second: 1_000.0,
        max_out_of_order_ms: None,
    },
    DataArrival::Unknown,
];

/// Whether `producer` of `examples`, stored across evaluations at
/// `timing`, delivers on its own grid (60 s @ 0) and on every context
/// (the finer 30 s grid, which its stored times do not hold).
fn delivers_on_grid_and_off_it(
    examples: &FinalityExamples,
    producer: CandidateForMemoGroupId,
    timing: ExecutionTiming,
) -> (bool, bool) {
    let stored = candidate(producer, timing, Lifetime::AcrossEvaluations(MEMORY));
    (
        stored.delivers_on(&examples.candidates, &evaluation(60_000, 0)),
        stored.delivers_on(&examples.candidates, &evaluation(30_000, 0)),
    )
}

/// Case (a) of #1264: SQL `SELECT count(*) FROM metrics` reads no time, but
/// rows keep arriving, so its result is never final. Computed at query time
/// and stored, it is a snapshot of its own evaluation time and delivers on
/// its own grid only. Kept up to date at ingestion time it delivers on
/// every context, and so does any result over data at rest.
#[test]
fn a_count_over_arriving_rows_computed_at_query_time_delivers_on_its_own_grid_only() {
    for arrival in ARRIVING {
        let examples = finality_examples(arrival);
        let producer = examples.count_of_table;
        assert_eq!(
            delivers_on_grid_and_off_it(&examples, producer, QueryTime),
            (true, false),
            "{arrival:?}"
        );
        assert_eq!(
            delivers_on_grid_and_off_it(&examples, producer, IngestionTime),
            (true, true),
            "{arrival:?}"
        );
    }
    let at_rest = finality_examples(DataArrival::AtRest);
    assert_eq!(
        delivers_on_grid_and_off_it(&at_rest, at_rest.count_of_table, QueryTime),
        (true, true)
    );
}

/// Case (b) of #1264: in `max_over_time((rate(latency[5m] @ end()))[10m:1m]
/// @ T)`, `@ T` pins only the subquery's own time; the inner `@ end()`
/// still moves with the request. Even over data at rest, a stored result
/// delivers on its own grid only.
#[test]
fn an_end_anchor_under_at_t_keeps_a_stored_result_on_its_own_grid() {
    for arrival in [DataArrival::AtRest, DataArrival::Unknown] {
        let examples = finality_examples(arrival);
        let producer = examples.max_over_subquery_at_t;
        assert_eq!(
            delivers_on_grid_and_off_it(&examples, producer, QueryTime),
            (true, false),
            "{arrival:?}"
        );
    }
}

/// Case (c) of #1264: `sum_over_time(latency[5m] @ T)` reads the fixed
/// window `(T - 5m, T]`, which does not move with the evaluation time.
/// Over data at rest, or kept up to date at ingestion time, it delivers on
/// every context. Computed at query time over arriving rows it is final
/// only from T on (no row arrives out of order), and no context says
/// whether its times are past T, so it delivers on its own grid only
/// (#1298).
#[test]
fn a_window_pinned_by_at_t_delivers_on_every_context_once_nothing_changes_it() {
    let at_rest = finality_examples(DataArrival::AtRest);
    assert_eq!(
        delivers_on_grid_and_off_it(&at_rest, at_rest.sum_over_window_at_t, QueryTime),
        (true, true)
    );
    for arrival in ARRIVING {
        let examples = finality_examples(arrival);
        let producer = examples.sum_over_window_at_t;
        assert_eq!(
            delivers_on_grid_and_off_it(&examples, producer, QueryTime),
            (true, false),
            "{arrival:?}"
        );
        assert_eq!(
            delivers_on_grid_and_off_it(&examples, producer, IngestionTime),
            (true, true),
            "{arrival:?}"
        );
    }
}

/// `producer_satisfies_physical_requirements` over every valid producer state and every valid
/// consumer state: a query-time producer serves only query-time
/// consumers, an ingestion-time producer that is not materialized only
/// ingestion-time consumers, and one materialized across evaluations
/// serves both. Chain widths do not matter: alignment is Stage 3's
/// per-context check.
#[test]
fn producer_satisfies_physical_requirements_over_every_valid_timing_and_lifetime() {
    let states = |medium| {
        [
            candidate(BUILD, QueryTime, Lifetime::None),
            candidate(BUILD, QueryTime, Lifetime::OneEvaluation(medium)),
            candidate(WINDOWS_10S, QueryTime, Lifetime::AcrossEvaluations(medium)),
            candidate(BUILD, IngestionTime, Lifetime::None),
            candidate(
                WINDOWS_5S,
                IngestionTime,
                Lifetime::AcrossEvaluations(medium),
            ),
        ]
    };
    // Rows: producer state; columns: consumer state, in `states` order.
    let expected = [
        [true, true, true, false, false],
        [true, true, true, false, false],
        [true, true, true, false, false],
        [false, false, false, true, true],
        [true, true, true, true, true],
    ];
    for producer_medium in [StorageMedium::Memory, StorageMedium::Disk] {
        for consumer_medium in [StorageMedium::Memory, StorageMedium::Disk] {
            for (producer, row) in states(producer_medium).iter().zip(expected) {
                for (consumer, satisfies) in states(consumer_medium).iter().zip(row) {
                    assert_eq!(
                        producer_satisfies_physical_requirements(
                            producer,
                            consumer.physical_properties.timing()
                        ),
                        satisfies,
                        "{:?} -> {:?}",
                        producer.physical_properties,
                        consumer.physical_properties
                    );
                }
            }
        }
    }
}

/// The edge from candidate `producer` into input 0 of `consumer`.
fn physical_edge(
    producer: usize,
    consumer: usize,
    satisfies_logical_requirement: bool,
    satisfies_physical_requirement: bool,
) -> PhysicalEdge {
    PhysicalEdge {
        producer_candidate_id: PhysicalCandidateId(producer),
        consumer_candidate_id: PhysicalCandidateId(consumer),
        input_index_of_consumer: 0,
        satisfies_logical_requirement,
        satisfies_physical_requirement,
    }
}

/// An edge is admissible only when it satisfies both the logical and the
/// physical requirement.
#[test]
fn an_edge_is_admissible_when_both_requirements_hold() {
    for (logical, physical) in [(false, false), (false, true), (true, false), (true, true)] {
        assert_eq!(
            physical_edge(0, 1, logical, physical).is_logically_and_physically_admissible(),
            logical && physical,
            "alpha_log {logical}, alpha_phys {physical}"
        );
    }
}

/// `new` indexes only the admissible edges, into each (consumer, input)
/// and out of each producer; every edge stays in `edges()`. The last
/// 5 minutes read by a query-time KLL build (admissible), by an
/// ingestion-time chain (not physically admissible) and by a query-time
/// chain that misses the logical requirement.
#[test]
fn new_indexes_only_admissible_edges() {
    let physical_candidates = vec![
        candidate(LAST_5M, QueryTime, Lifetime::None),
        candidate(BUILD, QueryTime, Lifetime::None),
        candidate(
            WINDOWS_5S,
            IngestionTime,
            Lifetime::AcrossEvaluations(MEMORY),
        ),
        candidate(WINDOWS_10S, QueryTime, Lifetime::AcrossEvaluations(MEMORY)),
    ];
    let edges = vec![
        physical_edge(0, 1, true, true),
        physical_edge(0, 2, true, false),
        physical_edge(0, 3, false, true),
    ];
    let candidates = PhysicalASAPMemoCandidates::new(
        LogicalASAPMemoCandidates {
            memo: memo(),
            edges: vec![],
            pruned: vec![],
            evaluation_grid_of_group: vec![None; 5],
            exact_only_groups_of_query: vec![],
        },
        physical_candidates,
        edges.clone(),
        vec![],
    );
    assert_eq!(
        candidates.edges().map(|(_, edge)| edge).collect::<Vec<_>>(),
        edges
    );
    assert_eq!(
        candidates.admissible_edges_into(PhysicalCandidateId(1), 0),
        [PhysicalEdgeId(0)]
    );
    for consumer in [2, 3] {
        assert!(candidates
            .admissible_edges_into(PhysicalCandidateId(consumer), 0)
            .is_empty());
    }
    assert_eq!(
        candidates.admissible_edges_out_of(PhysicalCandidateId(0)),
        [PhysicalEdgeId(0)]
    );
    for producer in 1..4 {
        assert!(candidates
            .admissible_edges_out_of(PhysicalCandidateId(producer))
            .is_empty());
    }
}
