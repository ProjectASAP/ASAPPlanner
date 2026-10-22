//! The memo over logical sub-expressions solved as a mixed-integer linear
//! program (joint-planning-search design, "Memo-group search"), behind
//! [`crate::SearchAlgorithm::MemoMilp`].

use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_physical_asap_optimization::PhysicalCandidate;
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::{PlanningModels, Selection, SelectionError};

/// A memo search's result: the selection, its physical candidate and its
/// logical roots, or why no plan is valid.
pub(crate) type MemoSearchResult =
    Result<(Selection, PhysicalCandidate, Vec<QueryRoot>), SelectionError>;

/// The cheapest valid plan of `per_computation`'s memo under cost model 1,
/// as [`super::search::brute_force`] returns it, with the solver's gap,
/// iterations and time in the selection's report
/// ([`crate::SearchReport::solver_gap`]). `None` while the model is not
/// built: the caller runs the legacy search and reports
/// [`crate::RoutedToLegacy::MilpNotBuilt`].
pub(crate) fn search<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Option<MemoSearchResult> {
    let _ = (per_computation, entries, data, models);
    None
}
