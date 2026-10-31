//! Today's Stage 3 engine behind [`select_plan`](crate::select_plan): the
//! candidate graph with costs, cost bounds, the MILP and the evaluator-backed
//! refinement loop. #1095 replaces it.

pub(crate) mod candidates;
pub(crate) mod costing;
pub(crate) mod milp;
pub(crate) mod refinement;
pub(crate) mod select;

/// Stage 2's realizer over `roots`, `entries` and `data`, for tests that
/// build their own catalogs.
#[cfg(test)]
pub(crate) fn test_realizer<'a>(
    roots: &'a [asap_types::ir::QueryRoot],
    entries: &'a [asap_types::workload::QueryWorkloadEntry],
    data: &'a asap_types::workload::DataWorkload,
) -> asap_physical_asap_optimization::Stage2AssignmentRealizer<'a> {
    asap_physical_asap_optimization::Stage2AssignmentRealizer {
        original_roots: roots,
        entries,
        data,
        query_time_results_retained: false,
    }
}
