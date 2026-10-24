//! Shared Stage 2 implementation and Stage 3 evaluation of logical candidates.
//!
//! Enumeration policy belongs to each search route. This module preserves
//! candidate numbering, workload calibration, validation and reporting for
//! callers that supply an explicit stream of complete logical candidates.

use crate::search::pipeline::{
    selected_logical, LogicalCandidate, PrunedCandidate, Stage1ToStage3Run,
};
use crate::{
    CalibrationFit, PlanSelector, PlanningModels, QuerySearchReport, Rejection, RejectionCause,
    Selection, SelectionError,
};
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_logical_asap_optimization::workload_candidates::Stage1Candidate;
use asap_physical_asap_optimization::{
    optimize_physical_asap, optimize_physical_asap_without_query_time_raw_scans, PhysicalCandidate,
};
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// Stage 2 and Stage 3 over `stage1`, the Stage 1 candidates of
/// `per_computation` in enumeration order (all of them, or the ones a caller
/// enumerated), keeping every candidate: candidate `n` (from 1) is `L<n>`,
/// implemented as `P<n>` with nothing materialized and as `P<n>-m<k>` (`k`
/// from 1) for each other materialization assignment.
///
/// It does not prune: `per_computation` must already be pruned for
/// `models.deployment_capabilities`
/// ([`PerComputationCandidates::without_unsupported_summaries`]), with
/// `stage1` enumerated from that pruned inventory, as
/// [`crate::run_stage1_to_stage3_from_pass1`] does. Pruning here would come too
/// late: each of `stage1`'s choices indexes the inventory it was enumerated
/// from, so a candidate with an unsupported summary would already be in
/// `stage1`.
pub fn run_stage2_and_stage3<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    stage1: Vec<Stage1Candidate>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Stage1ToStage3Run<Id> {
    let calibration = fit_candidate_calibration(stage1.iter(), entries, data, models);
    evaluate_logical_candidates(
        &per_computation,
        stage1.into_iter(),
        entries,
        data,
        models,
        true,
        Vec::new(),
        calibration,
        false,
        &std::cell::Cell::new(0),
    )
    .with_inventory(per_computation)
}

/// The calibration fit of the formulas to the deployment's measured cost
/// table over `stage1`'s physical candidates
/// ([`PlanSelector::needs_calibration`]), numbered as Stage 2 numbers
/// them; `Ok(None)` without a measured table, when no fit is needed.
pub(crate) fn fit_candidate_calibration<S: std::borrow::Borrow<Stage1Candidate>>(
    stage1: impl Iterator<Item = S>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Option<CalibrationFit>, SelectionError> {
    let selector = PlanSelector::new(entries, data, models, false)?;
    if !selector.needs_calibration() {
        return Ok(None);
    }
    let mut fit = CalibrationFit::default();
    for (index, candidate) in stage1.enumerate() {
        let Ok(roots) = &candidate.borrow().roots else {
            continue;
        };
        let number = index + 1;
        let id = format!("L{number}");
        let Ok(implemented) =
            optimize_physical_asap(&id, roots, entries, data, models.deployment_capabilities)
        else {
            continue;
        };
        for (k, mut p) in implemented.into_iter().enumerate() {
            p.id = match k {
                0 => format!("P{number}"),
                k => format!("P{number}-m{k}"),
            };
            fit.add(selector.calibration_fit(&p)?);
        }
    }
    Ok(Some(fit))
}

/// A [`Stage1ToStage3Run`] before it takes the Pass 1 inventory, which the
/// search borrows.
pub(crate) struct EvaluatedCandidates {
    combinations: usize,
    logical: Vec<LogicalCandidate>,
    physical: Vec<PhysicalCandidate>,
    pruned: Vec<PrunedCandidate>,
    pub(crate) selection: Result<Selection, SelectionError>,
    selected: Option<PhysicalCandidate>,
    selected_logical: Option<Vec<QueryRoot>>,
    stage1_selected: Option<LogicalCandidate>,
}

impl EvaluatedCandidates {
    pub(crate) fn with_inventory<Id>(
        self,
        per_computation: PerComputationCandidates<Id>,
    ) -> Stage1ToStage3Run<Id> {
        Stage1ToStage3Run {
            stage1_pass1_per_computation: per_computation,
            combinations: self.combinations,
            stage1_logical_asap_optimization: self.logical,
            stage2_physical_asap_optimization: self.physical,
            stage2_pruned: self.pruned,
            stage3_plan_selection: self.selection,
            stage3_selected: self.selected,
            stage3_selected_logical: self.selected_logical,
            stage1_selected: self.stage1_selected,
        }
    }
}

/// Stage 2 and Stage 3 over `stage1`, each Stage 1 candidate implemented
/// and its physical candidates offered to Stage 3 as it is enumerated.
/// Keeping every candidate lists them all; otherwise only the roots of
/// the Stage 1 candidates whose physical candidates Stage 3 still keeps are
/// held. `queries` is each query's own-candidate report, for the selection.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_logical_candidates<Id>(
    per_computation: &PerComputationCandidates<Id>,
    stage1: impl Iterator<Item = Stage1Candidate>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    keep_every_candidate: bool,
    queries: Vec<QuerySearchReport>,
    calibration: Result<Option<CalibrationFit>, SelectionError>,
    skip_query_time_raw_scans: bool,
    stage1_raw_skipped: &std::cell::Cell<u64>,
) -> EvaluatedCandidates {
    // Stage 2 assignments left out because they scan raw data at query
    // time, and the id of the first one.
    let mut raw_scans_skipped = 0u64;
    let mut first_raw_scan_skipped: Option<String> = None;
    let combinations = per_computation.combinations();
    let mut logical: Vec<LogicalCandidate> = Vec::new();
    let mut physical = Vec::new();
    let mut pruned = Vec::new();
    // Keeping only the best: the roots of the Stage 1 candidates Stage 3
    // keeps a candidate of.
    let mut kept_roots: Vec<LogicalCandidate> = Vec::new();
    let mut selector =
        PlanSelector::new(entries, data, models, keep_every_candidate).and_then(|mut selector| {
            if let Some(fit) = calibration? {
                selector.set_calibration(fit);
            }
            Ok(selector)
        });
    for (index, candidate) in stage1.enumerate() {
        if selector.is_err() && !keep_every_candidate {
            break;
        }
        let number = index + 1;
        let id = format!("L{number}");
        if let Ok(roots) = &candidate.roots {
            let implemented = match skip_query_time_raw_scans {
                // Each candidate keeps its position in its Stage 1
                // candidate's full list (`P<n>-m<k>`); `<n>` counts only the
                // Stage 1 candidates enumerated.
                true => optimize_physical_asap_without_query_time_raw_scans(
                    &id,
                    roots,
                    entries,
                    data,
                    models.deployment_capabilities,
                )
                .map(|(candidates, skipped)| {
                    raw_scans_skipped += skipped as u64;
                    if skipped > 0 && first_raw_scan_skipped.is_none() {
                        let position = (0..).find(|k| !candidates.iter().any(|(p, _)| p == k));
                        first_raw_scan_skipped = position.map(|k| match k {
                            0 => format!("P{number}"),
                            k => format!("P{number}-m{k}"),
                        });
                    }
                    candidates
                }),
                false => optimize_physical_asap(
                    &id,
                    roots,
                    entries,
                    data,
                    models.deployment_capabilities,
                )
                .map(|candidates| candidates.into_iter().enumerate().collect()),
            };
            match implemented {
                Ok(implemented) => {
                    for (k, mut p) in implemented {
                        p.id = match k {
                            0 => format!("P{number}"),
                            k => format!("P{number}-m{k}"),
                        };
                        if let Ok(stage3) = &mut selector {
                            if let Err(error) = stage3.offer(&p) {
                                selector = Err(error);
                            }
                        }
                        if keep_every_candidate {
                            physical.push(p);
                        }
                    }
                }
                Err(error) if keep_every_candidate => pruned.push(PrunedCandidate {
                    from_logical: id.clone(),
                    reason: error.to_string(),
                }),
                Err(_) => {}
            }
        }
        // Keeping every candidate, the selected roots are found in `logical`.
        if let (Ok(stage3), false) = (&selector, keep_every_candidate) {
            kept_roots.retain(|kept| stage3.keeps_from_logical(&kept.id));
            if let (Ok(roots), true) = (&candidate.roots, stage3.keeps_from_logical(&id)) {
                kept_roots.push(LogicalCandidate {
                    id: id.clone(),
                    choice: candidate.choice.clone(),
                    roots: Ok(roots.clone()),
                    pass2: candidate.pass2.clone(),
                });
            }
        }
        if keep_every_candidate {
            logical.push(LogicalCandidate {
                id,
                choice: candidate.choice,
                roots: candidate.roots,
                pass2: candidate.pass2,
            });
        }
    }
    let finished = selector.and_then(PlanSelector::finish);
    let (selection, selected) = match finished {
        Ok((mut selection, selected)) => {
            selection.search.queries = queries;
            selection.search.raw_scan_assignments_skipped = raw_scans_skipped;
            selection.search.raw_scan_candidates_skipped = stage1_raw_skipped.get();
            (Ok(selection), Some(selected))
        }
        Err(SelectionError::NoValidCandidate(mut rejected)) => {
            // The first candidate left out for scanning raw data at query
            // time stands for them, as Stage 3 would have rejected it.
            if let Some(id) = first_raw_scan_skipped.filter(|_| {
                !rejected
                    .iter()
                    .any(|r| r.cause == RejectionCause::RawDataNotRetained)
            }) {
                rejected.push(Rejection {
                    id,
                    cause: RejectionCause::RawDataNotRetained,
                    reason: "scans raw data at query time, but the deployment does not retain \
                             raw data"
                        .to_string(),
                });
            }
            // Stage 1 candidates left out before Stage 2 for the same reason.
            let left_out = stage1_raw_skipped.get();
            if left_out > 0
                && !rejected
                    .iter()
                    .any(|r| r.cause == RejectionCause::RawDataNotRetained)
            {
                rejected.push(Rejection {
                    id: format!("{left_out} Stage 1 candidates"),
                    cause: RejectionCause::RawDataNotRetained,
                    reason: format!(
                        "{left_out} Stage 1 candidates scan raw data at query time in every \
                         materialization, but the deployment does not retain raw data"
                    ),
                });
            }
            // A query whose every own candidate was discarded leaves no
            // candidate to reject: its first discard says why.
            for (query, report) in queries.iter().enumerate() {
                if report.discarded.len() as u64 == report.candidates_checked {
                    if let Some(discarded) = report.discarded.first() {
                        rejected.push(Rejection {
                            id: format!("q{}", query + 1),
                            cause: discarded.cause,
                            reason: format!("q{}: {}", query + 1, discarded.reason),
                        });
                    }
                }
            }
            (Err(SelectionError::NoValidCandidate(rejected)), None)
        }
        Err(error) => (Err(error), None),
    };
    let stage1_selected = selected.as_ref().and_then(|selected| {
        let from = match keep_every_candidate {
            true => &logical,
            false => &kept_roots,
        };
        let found = from.iter().find(|l| l.id == selected.from_logical)?;
        Some(LogicalCandidate {
            id: found.id.clone(),
            choice: found.choice.clone(),
            roots: Ok(found.roots.as_ref().ok()?.clone()),
            pass2: found.pass2.clone(),
        })
    });
    let selected_logical = stage1_selected
        .as_ref()
        .and_then(|l| selected_logical(l.roots.as_ref().ok()?, entries, data));
    EvaluatedCandidates {
        combinations,
        logical,
        physical,
        pruned,
        selection,
        selected,
        selected_logical,
        stage1_selected,
    }
}
