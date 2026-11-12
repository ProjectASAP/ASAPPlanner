//! Fixtures this crate's own tests share, beside the memo builders of
//! `asap_types::test_support` (groups with the candidates Pass 1 would add,
//! queries), so no test calls another stage crate: Pass 2's output built
//! by hand (the memo, its logical edges and grids), the running example's
//! query A, and the data workloads and deployment capabilities the tests
//! vary.
use std::collections::BTreeSet;

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{
    CandidateForMemoGroupId, EvaluationGrid, Lifetime, LogicalASAPMemoCandidates, LogicalEdge,
    Memo, MemoGroupId, MemoQuery, StorageMedium,
};
use asap_types::summary_type::{parameter_list, SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::test_support::memo_builders::{candidate, group, scan_group};
use asap_types::test_support::memo_fixtures::{range, readout, state};
use asap_types::workload::{DataArrival, DataWorkload};
use ExecutionTiming::{IngestionTime, QueryTime};

const MEMORY: StorageMedium = StorageMedium::Memory;
pub const QT_NONE: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::None);
pub const QT_ONE: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::OneEvaluation(MEMORY));
pub const QT_ACROSS: (ExecutionTiming, Lifetime) = (QueryTime, Lifetime::AcrossEvaluations(MEMORY));
pub const IT_NONE: (ExecutionTiming, Lifetime) = (IngestionTime, Lifetime::None);
pub const IT_ACROSS: (ExecutionTiming, Lifetime) =
    (IngestionTime, Lifetime::AcrossEvaluations(MEMORY));

pub const GRID_60S: Option<EvaluationGrid> = Some(EvaluationGrid {
    period_ms: 60_000,
    phase_ms: 0,
});

/// The candidate `index_in_group` of `group`.
pub fn candidate_id(group: MemoGroupId, index_in_group: usize) -> CandidateForMemoGroupId {
    CandidateForMemoGroupId {
        group,
        index_in_group,
    }
}

/// The edge from `producer` into input `input` of `consumer`.
pub fn edge(
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

/// The group of `operator`, with one candidate of `operator` per θ among
/// the first `count` of `algorithm`'s list (`memo_builders::sketch_state`
/// adds the whole list).
pub fn sketch_group(
    memo: &mut Memo,
    operator: Operator<MemoGroupId>,
    algorithm: SketchAlgorithm,
    count: usize,
) -> MemoGroupId {
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    for parameters in &parameter_list(algorithm)[..count] {
        candidate(memo, group, operator.clone(), Some(parameters.clone()));
    }
    group
}

/// A query whose root is `root`, which may be a scalar expression
/// (`memo_builders::query` takes only a group).
pub fn add_query_root(memo: &mut Memo, root: QueryRoot<MemoGroupId>) {
    memo.add_query(MemoQuery {
        root,
        accuracy_target: None,
        number_of_values_per_query_evaluation: None,
    });
}

/// Pass 2's output over `memo` with `edges` and `evaluation_grid_of_group`:
/// nothing pruned, and no query restricted to exact candidates.
pub fn logical(
    memo: Memo,
    edges: Vec<LogicalEdge>,
    evaluation_grid_of_group: Vec<Option<EvaluationGrid>>,
) -> LogicalASAPMemoCandidates {
    LogicalASAPMemoCandidates {
        exact_only_groups_of_query: vec![BTreeSet::new(); memo.queries().len()],
        memo,
        edges,
        pruned: vec![],
        evaluation_grid_of_group,
    }
}

/// A memo over no declared source facts.
pub fn memo() -> Memo {
    Memo::new(&DataWorkload::default())
}

/// The running example's query A, `quantile_over_time(0.95, latency[1m])`
/// evaluated every 60 s at phase 0, after Pass 2: the scan; its last
/// minute; the KLL state group, with candidate 0 a KLL build over that
/// minute and candidate 1 a merge of the windows a `TimeRange` selects
/// from a 60 s chain over the scan; and the p95 readout, the query's root.
/// Each edge's $\alpha^{\log}$ is true. Groups read by the query evaluate
/// on its 60 s grid; the scan and the chain do not depend on the
/// evaluation time and have no grid.
pub struct Example {
    pub logical: LogicalASAPMemoCandidates,
    pub scan: CandidateForMemoGroupId,
    pub last_minute: CandidateForMemoGroupId,
    pub build: CandidateForMemoGroupId,
    pub merge: CandidateForMemoGroupId,
    pub chain: CandidateForMemoGroupId,
    pub windows: CandidateForMemoGroupId,
    pub readout: CandidateForMemoGroupId,
}

pub fn example() -> Example {
    example_read_by(QueryRoot::Operator)
}

/// [`example`], its query's root `root` over the readout's group.
pub fn example_read_by(root: impl FnOnce(MemoGroupId) -> QueryRoot<MemoGroupId>) -> Example {
    let mut memo = memo();
    let scan = scan_group(&mut memo, "latency", &["job"], vec![]);
    let last_minute = group(&mut memo, range(60_000, scan));
    let build_state = sketch_group(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), last_minute),
        SketchAlgorithm::Kll,
        1,
    );
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
    let windows = sketch_group(&mut memo, range(60_000, chain), SketchAlgorithm::Kll, 1);
    let merge = candidate(
        &mut memo,
        build_state,
        Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![windows],
            group_by: None,
        }),
        Some(parameter_list(SketchAlgorithm::Kll)[0].clone()),
    );
    let readout = group(
        &mut memo,
        readout(SketchStatistic::Quantile { q: 0.95 }, build_state),
    );
    add_query_root(&mut memo, root(readout));
    let [scan, last_minute, build, chain, windows, readout] =
        [scan, last_minute, build_state, chain, windows, readout]
            .map(|group| candidate_id(group, 0));
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
        logical: logical(memo, edges, evaluation_grid_of_group),
        scan,
        last_minute,
        build,
        merge,
        chain,
        windows,
        readout,
    }
}

pub fn ingesting(max_out_of_order_ms: Option<u64>) -> DataWorkload {
    DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1000.0,
            max_out_of_order_ms,
        },
        ..DataWorkload::default()
    }
}

/// Raw data and query-time results retained.
pub fn retaining_everything() -> DeploymentCapabilities {
    DeploymentCapabilities {
        query_time_results_retained: true,
        ..DeploymentCapabilities::default()
    }
}
