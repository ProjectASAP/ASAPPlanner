//! Temporary whole-workload search implementation, pending memo feature parity.

use crate::search::candidate_evaluation::{evaluate_logical_candidates, fit_candidate_calibration};
use crate::search::legacy::interchangeable::Classes;
use crate::search::legacy::query_candidates::QueryCandidates;
use crate::search::pipeline::{Stage1ToStage3Run, Stage1ToStage3Search};
use crate::PlanningModels;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_logical_asap_optimization::workload_candidates::{
    stage1_logical_asap_optimization, stage1_logical_asap_optimization_skipping,
};
use asap_physical_asap_optimization::every_assignment_scans_raw_at_query_time;
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// The legacy search ([`crate::SearchAlgorithm::Legacy`]) of
/// [`crate::run_stage1_to_stage3_from_pass1`]: independent components searched
/// separately when that is proven exact, else the workload as one.
pub(crate) fn legacy_search<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    search: Stage1ToStage3Search,
) -> Stage1ToStage3Run<Id> {
    let components =
        crate::search::legacy::components::sharing_components(&per_computation, entries, &models);
    let searched_as_one =
        match crate::search::legacy::components::searched_as_one(&components, &models, search) {
            Some(reason) => reason,
            None => match crate::search::legacy::components::search_components(
                &per_computation,
                &components,
                entries,
                data,
                models,
            ) {
                Ok(run) => return run,
                Err(reason) => reason,
            },
        };
    let mut run = search_as_one(per_computation, entries, data, models, search);
    if let Ok(selection) = &mut run.stage3_plan_selection {
        selection.search.searched_as_one = Some(searched_as_one);
    }
    run
}

/// [`crate::run_stage1_to_stage3_from_pass1`] over the whole workload as one
/// component.
fn search_as_one<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    search: Stage1ToStage3Search,
) -> Stage1ToStage3Run<Id> {
    let per_computation =
        per_computation.without_unsupported_summaries(models.deployment_capabilities);
    if search.exhaustive {
        let stage1 = stage1_logical_asap_optimization(&per_computation, entries);
        let calibration = fit_candidate_calibration(
            stage1_logical_asap_optimization(&per_computation, entries),
            entries,
            data,
            models,
        );
        let run = evaluate_logical_candidates(
            &per_computation,
            stage1,
            entries,
            data,
            models,
            search.keep_every_candidate,
            Vec::new(),
            calibration,
            false,
            &std::cell::Cell::new(0),
        );
        return run.with_inventory(per_computation);
    }
    let queries = QueryCandidates::check(&per_computation, entries, data, models);
    let classes = Classes::of(&per_computation, entries, &models);
    // Stage 1 candidates left out because every assignment of theirs scans
    // raw data at query time.
    let stage1_raw_skipped = std::cell::Cell::new(0u64);
    let skip_raw =
        !search.keep_every_candidate && !models.deployment_capabilities.raw_data_retained;
    let stage1 = stage1_logical_asap_optimization_skipping(
        &per_computation,
        entries,
        |choice| match (
            queries.invalid_prefix(choice),
            classes.invalid_prefix(choice),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        },
        |roots| {
            let viable = viable(roots, entries, data, models, skip_raw);
            if !viable {
                stage1_raw_skipped.set(stage1_raw_skipped.get() + 1);
            }
            viable
        },
    );
    // The calibration is fitted over every candidate, as the exhaustive
    // search fits it, the discarded ones included.
    let calibration = fit_candidate_calibration(
        stage1_logical_asap_optimization(&per_computation, entries),
        entries,
        data,
        models,
    );
    // Members of a class of interchangeable queries read the same builds.
    let stage1 = stage1.filter(|candidate| {
        candidate
            .roots
            .as_ref()
            .map_or(true, |roots| classes.members_read_same_builds(roots))
    });
    let mut run = evaluate_logical_candidates(
        &per_computation,
        stage1,
        entries,
        data,
        models,
        search.keep_every_candidate,
        queries.reports(),
        calibration,
        skip_raw,
        &stage1_raw_skipped,
    );
    if let Ok(selection) = &mut run.selection {
        selection.search.interchangeable_queries = classes.members();
    }
    run.with_inventory(per_computation)
}

/// Whether a Stage 1 candidate with roots `roots` may have a valid
/// implementation: in a deployment that keeps no raw data (`skip_raw`),
/// not when every Stage 2 assignment of it scans raw data at query time,
/// which Stage 3 rejects (`RawDataNotRetained`); identical-expression
/// sharing cannot change that ([`stage1_logical_asap_optimization_skipping`]).
pub(crate) fn viable(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    skip_raw: bool,
) -> bool {
    !skip_raw
        || !every_assignment_scans_raw_at_query_time(
            roots,
            entries,
            data,
            models.deployment_capabilities,
        )
}
