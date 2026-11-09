//! Tests of [`super::execution_costs`].

use std::time::Duration;

use asap_types::ir::operator::Reduction;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, TimeRangeKind};
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalASAPMemoCandidates, PhysicalCandidate, PhysicalEdge,
    PhysicalProperties, StorageMedium,
};
use asap_types::summary_type::{parameter_list, SketchAlgorithm, SketchParams, SummaryUpdate};
use asap_types::test_support::memo_builders::{group, query, scan_group, try_candidate};
use asap_types::workload::{
    Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand, RepetitionInterval,
    TimestampMs,
};

use super::*;
use crate::retention::edge_retention_need_ms;
use crate::selectable::ExecutionContexts;

const DAY_MS: u64 = 86_400_000;

fn range(days: u64, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(days * DAY_MS),
        kind: TimeRangeKind::Range,
        child,
    })
}

/// The group of `operator`, with its own candidate at θ `parameters`.
fn kll_group(
    memo: &mut Memo,
    operator: Operator<MemoGroupId>,
    parameters: &SketchParams,
) -> MemoGroupId {
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    try_candidate(memo, group, operator, Some(parameters.clone())).unwrap();
    group
}

/// Example 3a's shape: a KLL state over the last five years of
/// 1,000,000 series sampled every 15 s, once a day, either built over
/// the scan's five years or merged from the windows of a 1-day chain
/// built at ingestion time; a second daily query reads the chain's
/// newest day of windows. The scan has a query-time candidate, one at
/// ingestion time and one materialized there (C0, C1, C2); the chain
/// (C3) may read C1 or C2, the five-year range (C4) C0 or C2.
///
/// An ingestion-time consumer reads one batch of the scan, from C1 or
/// C2 alike: the chain's baseline input is that batch, not the five
/// years C2 retains for its query-time consumer. Selecting an
/// execution, one of its edges and the least retention its consumers
/// may need then never costs less than nothing (was −1,356,902/s: the
/// chain costed over five years of rows, the edge from C1 −22,398,978
/// and a retention cost per ms inflated by those rows).
#[test]
fn no_execution_reading_a_producer_costs_less_than_nothing() {
    let kll = &parameter_list(SketchAlgorithm::Kll)[6];
    let summary_type = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let mut memo = Memo::new(&DataWorkload::default());
    let scan = scan_group(&mut memo, "m", &[], vec![]);
    let chain = kll_group(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: scan,
            summary_type: summary_type.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: DAY_MS,
        }),
        kll,
    );
    let five_years = 5 * 365;
    // A `TimeRange` over the chain, and the merge over it, carry its θ.
    let windows = kll_group(&mut memo, range(five_years, chain), kll);
    let newest_day = kll_group(&mut memo, range(1, chain), kll);
    let last_five_years = group(&mut memo, range(five_years, scan));
    let state = kll_group(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: last_five_years,
            summary_type,
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        }),
        kll,
    );
    try_candidate(
        &mut memo,
        state,
        Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![windows],
            group_by: None,
        }),
        Some(kll.clone()),
    )
    .unwrap();
    for root in [state, newest_day] {
        query(&mut memo, root, None);
    }
    use ExecutionTiming::{IngestionTime, QueryTime};
    let materialized = Lifetime::AcrossEvaluations(StorageMedium::Memory);
    let candidate = |group, index_in_group, timing, lifetime| PhysicalCandidate {
        logical_candidate_id: CandidateForMemoGroupId {
            group,
            index_in_group,
        },
        physical_properties: PhysicalProperties::new(timing, lifetime).unwrap(),
    };
    let candidates = vec![
        candidate(scan, 0, QueryTime, Lifetime::None),
        candidate(scan, 0, IngestionTime, Lifetime::None),
        candidate(scan, 0, IngestionTime, materialized),
        candidate(chain, 0, IngestionTime, materialized),
        candidate(last_five_years, 0, QueryTime, Lifetime::None),
        candidate(windows, 0, QueryTime, Lifetime::None),
        candidate(state, 0, QueryTime, Lifetime::None),
        candidate(state, 1, QueryTime, Lifetime::None),
        candidate(newest_day, 0, QueryTime, Lifetime::None),
    ];
    let edge = |u: usize, c: usize, admissible: bool| PhysicalEdge {
        producer_candidate_id: PhysicalCandidateId(u),
        consumer_candidate_id: PhysicalCandidateId(c),
        input_index_of_consumer: 0,
        satisfies_logical_requirement: true,
        satisfies_physical_requirement: admissible,
    };
    let edges = vec![
        edge(0, 3, false),
        edge(1, 3, true),
        edge(2, 3, true),
        edge(0, 4, true),
        edge(1, 4, false),
        edge(2, 4, true),
        edge(3, 5, true),
        edge(3, 8, true),
        edge(4, 6, true),
        edge(5, 7, true),
    ];
    let groups = memo.groups().len();
    let physical = PhysicalASAPMemoCandidates::new(
        LogicalASAPMemoCandidates {
            memo,
            edges: vec![],
            pruned: vec![],
            evaluation_grid_of_group: vec![None; groups],
            exact_only_groups_of_query: vec![],
        },
        candidates,
        edges,
        vec![],
    );
    let daily = QueryWorkloadEntry {
        query: Query("daily".into()),
        requirements: QueryRequirements::default(),
        predictability: Predictability::default(),
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(DAY_MS as u32),
            evaluation_phase: TimestampMs(0),
        }),
        range_evaluation: None,
    };
    let entries = vec![daily.clone(), daily];
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 1_000_000.0 / 15.0}},
        "sources": {"m": {"series_count": 1_000_000, "sample_interval": 15_000}}
    }))
    .unwrap();
    let (contexts, query_outputs) = ExecutionContexts::new(
        &physical,
        &entries,
        &[(QueryId(0), state), (QueryId(1), newest_day)],
    );
    let costs = execution_costs(
        &physical,
        &contexts,
        &query_outputs,
        &entries,
        &data,
        &CostInputs::default(),
        0.0,
    );
    // π_v times the least any consumer of `v` reads back, the least
    // retention (R) lets a plan selecting `v` keep.
    let least_retention = |v: PhysicalCandidateId| {
        let least_need_ms = contexts
            .edges()
            .iter()
            .filter(|(_, _, producer)| producer.candidate == v)
            .filter_map(|&(_, edge, _)| edge_retention_need_ms(&physical, &entries, edge))
            .min()
            .unwrap_or(0);
        costs
            .retention_cost_per_second
            .get(&v)
            .copied()
            .unwrap_or(0.0)
            * least_need_ms as f64
    };
    let mut checked = 0;
    for (index, &(consumer, edge, _)) in contexts.edges().iter().enumerate() {
        let (Ok(execution), Some(edge_cost)) = (
            &costs.per_execution[contexts.index_of(consumer)],
            costs.per_edge[index],
        ) else {
            continue;
        };
        let total = execution + edge_cost + least_retention(consumer.candidate);
        assert!(
            total >= 0.0,
            "{:?} reading through {edge:?}: {execution} + {edge_cost} + retention = {total}",
            consumer.candidate
        );
        checked += 1;
    }
    // Among them the chain reading either scan.
    assert!(checked >= 2, "{checked} edges costed");
}
