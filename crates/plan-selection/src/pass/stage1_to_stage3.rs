//! Stage 1 (logical ASAP-aware optimization) → Stage 2 (physical ASAP-aware
//! optimization) → Stage 3 (plan selection), starting from the query-language
//! frontends' roots; Stage 0 runs before it in the facade.
//!
//! This is the planner pipeline (#509 MVP). [`Stage1ToStage3Pass`] is the
//! [`OptimizationPass`](super::OptimizationPass) that runs it.
//!
//! [`run_stage1_to_stage3`] runs, for one workload:
//! 1. Stage 1, logical ASAP-aware optimization: Pass 1 per-computation candidate
//!    generation, then composition of one whole-workload candidate per choice
//!    of a per-computation candidate for every target (the Cartesian product,
//!    in mixed-radix order, the last target varying fastest), each followed
//!    by its Pass 2 candidates: those of the identical-expression rule
//!    ([`identical_expression_pass2_candidates`]: one per subset of its
//!    sharing candidates), then those of the summary-capability rule
//!    ([`summary_capability_pass2_candidates`]: one per choice of shared
//!    summary builds), each followed by its own identical-expression
//!    candidates, and, for the composed candidate and each
//!    summary-capability candidate, those of the window-composition rule
//!    ([`window_composition_pass2_candidates`]: one per non-empty subset of
//!    the builds a repeating query could build as tumbling panes), each
//!    followed by its own identical-expression candidates, and, for the
//!    composed candidate, those of the reuse rules (exact top-k reuse, then
//!    each rollup Pass 2 candidate followed by its own exact top-k reuse
//!    Pass 2 candidates: [`topk_reuse_pass2_candidates`],
//!    [`rollup_pass2_candidates`]), each followed by its own
//!    identical-expression candidates; every distinct candidate once; the
//!    independent candidate is kept;
//! 2. Stage 2, physical ASAP-aware optimization of every Stage 1 candidate
//!    ([`optimize_physical_asap`]): one physical candidate per
//!    materialization assignment of its summary builds;
//! 3. Stage 3, plan selection over the physical candidates ([`select_plan`]).
//!
//! Stage 0, the query-language frontends, runs before it (the `asap-planner`
//! facade lowers the workload). The window-composition rule offers
//! per-query tumbling panes only; sliding windows, Exponential Histograms
//! and a pane width shared across queries are not planned yet.
//!
//! The search follows `docs/design_docs/proposals/joint-planning-search.md`.
//! Each query's own candidates are checked before composition (step 2,
//! [`query_candidates`](super::query_candidates)), and a whole-workload
//! choice that picks a discarded one is not composed. Every other
//! candidate is priced as it is built and only the best so far is kept
//! (step 6; under cost model 3's `pareto-front`, the front so far), unless
//! the caller asks to keep every candidate ([`Stage1ToStage3Search`]).

use std::rc::Rc;

use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryLanguage, QueryWorkloadEntry};

use super::query_candidates::QueryCandidates;
use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use crate::PlanningModels;
use crate::{
    CalibrationFit, PlanSelector, QuerySearchReport, Rejection, Selection, SelectionError,
};
use asap_logical_asap_optimization::per_computation_candidates::{
    generate_per_computation_candidates, PerComputationCandidateError, PerComputationCandidates,
};
use asap_logical_asap_optimization::workload_candidates::{
    stage1_logical_asap_optimization, stage1_logical_asap_optimization_skipping, Pass2Candidate,
    Stage1Candidate,
};
use asap_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};

/// One whole-workload CandidateLogicalASAPDAG: `choice[t]` picks target
/// `t`'s candidate, or its fused computation's
/// (`targets[t].computation_for(choice[t])`), of the Pass 1 inventory. `roots` are the
/// composed query roots in input order, or why the choice did not compose.
#[derive(Debug)]
pub struct LogicalCandidate {
    /// `L<n>`, numbered in enumeration order.
    pub id: String,
    pub choice: Vec<usize>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
    /// For a Pass 2 candidate: what it changed of its Pass 1 candidate.
    pub pass2: Option<Pass2Candidate>,
}

/// A composed logical candidate that physical ASAP-aware optimization could
/// not implement, with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate {
    pub from_logical: String,
    pub reason: String,
}

/// The output of Stage 1, Stage 2 and Stage 3 for one workload
/// ([`run_stage1_to_stage3`]). The candidate lists are filled only when the
/// search keeps every candidate ([`Stage1ToStage3Search::keep_every_candidate`]).
#[derive(Debug)]
pub struct Stage1ToStage3Run<Id> {
    /// Stage 1, Pass 1: the per-computation candidates, with the roots they
    /// read.
    pub stage1_pass1_per_computation: PerComputationCandidates<Id>,
    /// Stage 1, Pass 1: the number of composed whole-workload candidates,
    /// the size of the Cartesian product of the targets' candidates,
    /// saturating at `usize::MAX`. Pass 2 adds to them.
    pub combinations: usize,
    /// Stage 1: every whole-workload logical candidate, in enumeration
    /// order: each composed Pass 1 candidate, followed by its Pass 2
    /// candidates (those with [`LogicalCandidate::pass2`]). Empty unless
    /// every candidate is kept.
    pub stage1_logical_asap_optimization: Vec<LogicalCandidate>,
    /// Stage 2: the physical candidates of each Stage 1 candidate that
    /// composed and was implemented, one per materialization assignment:
    /// `P<n>` (nothing materialized), then `P<n>-m<k>`, from `L<n>`. Empty
    /// unless every candidate is kept.
    pub stage2_physical_asap_optimization: Vec<PhysicalCandidate>,
    /// Stage 2: the Stage 1 candidates Stage 2 could not implement, with the
    /// reasons. Empty unless every candidate is kept.
    pub stage2_pruned: Vec<PrunedCandidate>,
    /// Stage 3: plan selection over the Stage 2 candidates.
    pub stage3_plan_selection: Result<Selection, SelectionError>,
    /// Stage 3: the selected physical candidate, as Stage 2 built it
    /// ([`Self::selected`]).
    pub stage3_selected: Option<PhysicalCandidate>,
    /// Stage 3: the selected candidate's logical roots, with the weight
    /// requirements it confirmed made proofs ([`Self::selected_logical`]).
    pub stage3_selected_logical: Option<Vec<QueryRoot>>,
    /// Stage 1: the logical candidate the selected physical candidate
    /// implements, with its roots as Stage 1 composed them (before Stage 3
    /// sized them): what Stage 2 implemented.
    pub stage1_selected: Option<LogicalCandidate>,
}

impl<Id> Stage1ToStage3Run<Id> {
    /// The selected physical candidate, if selection succeeded.
    pub fn selected(&self) -> Option<&PhysicalCandidate> {
        self.stage3_selected.as_ref()
    }

    /// The composed roots of the logical candidate the selected physical
    /// candidate implements: the plan before physical operator
    /// implementation and materialization, with the weight requirements
    /// Stage 3 confirmed made proofs, as in [`Selection::plan`].
    pub fn selected_logical(&self) -> Option<&[QueryRoot]> {
        self.stage3_selected_logical.as_deref()
    }
}

/// How [`run_stage1_to_stage3_with`] searches, and what it keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stage1ToStage3Search {
    /// Keep every Stage 1 and Stage 2 candidate and every Stage 3 rejection
    /// in the run, for a developer tool that shows them; default `false`:
    /// only the best so far is kept.
    pub keep_every_candidate: bool,
    /// Compose every whole-workload choice, without checking each query's
    /// own candidates first: the exhaustive search the pruned one is tested
    /// against; default `false`.
    pub exhaustive: bool,
}

impl Stage1ToStage3Search {
    /// The exhaustive search keeping every candidate: what the planner did
    /// before the search was pruned.
    pub fn exhaustive() -> Self {
        Self {
            keep_every_candidate: true,
            exhaustive: true,
        }
    }
}

/// Run Stage 1 → Stage 2 → Stage 3 over the frontends' `roots` with the
/// default search ([`Stage1ToStage3Search::default`]: each query's
/// candidates checked first, only the best kept). `entries[i]` is the
/// workload entry of `roots[i]`: its accuracy target, latency bound,
/// recurrence and predictability. Only a Pass 1 failure is an error; a
/// failed selection is reported in the run.
pub fn run_stage1_to_stage3<Id: Clone + std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Stage1ToStage3Run<Id>, PerComputationCandidateError> {
    run_stage1_to_stage3_with(
        roots,
        entries,
        data,
        models,
        Stage1ToStage3Search::default(),
    )
}

/// [`run_stage1_to_stage3`] with the search `search`. Keeping every
/// candidate, a candidate that does not compose or cannot be implemented is
/// listed with its reason.
pub fn run_stage1_to_stage3_with<Id: Clone + std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    search: Stage1ToStage3Search,
) -> Result<Stage1ToStage3Run<Id>, PerComputationCandidateError> {
    let per_computation = generate_per_computation_candidates(roots)?;
    Ok(run_stage1_to_stage3_from_pass1(
        per_computation,
        entries,
        data,
        models,
        search,
    ))
}

/// [`run_stage1_to_stage3_with`] after Pass 1: searches the Stage 1
/// candidates of `per_computation` (the Pass 1 inventory of the frontends'
/// roots, [`stage1_logical_asap_optimization`]) without the summaries the
/// deployment does not support
/// ([`PerComputationCandidates::without_unsupported_summaries`]), so a
/// caller can read the inventory, e.g. its
/// [`combinations`](PerComputationCandidates::combinations), before
/// enumerating.
pub fn run_stage1_to_stage3_from_pass1<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    search: Stage1ToStage3Search,
) -> Stage1ToStage3Run<Id> {
    let components = super::components::source_components(
        &per_computation
            .roots
            .iter()
            .map(|(_, root)| root.clone())
            .collect::<Vec<_>>(),
    );
    let searched_as_one = match super::components::searched_as_one(&components, &models, search) {
        Some(reason) => reason,
        None => match super::components::search_components(
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

/// [`run_stage1_to_stage3_from_pass1`] over the whole workload as one
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
        let calibration = calibration(
            stage1_logical_asap_optimization(&per_computation, entries),
            entries,
            data,
            models,
        );
        let run = search_stage2_and_stage3(
            &per_computation,
            stage1,
            entries,
            data,
            models,
            search.keep_every_candidate,
            Vec::new(),
            calibration,
        );
        return run.with_inventory(per_computation);
    }
    let queries = QueryCandidates::check(&per_computation, entries, data, models);
    let stage1 = stage1_logical_asap_optimization_skipping(&per_computation, entries, |choice| {
        queries.invalid_prefix(choice)
    });
    // The calibration is fitted over every candidate, as the exhaustive
    // search fits it, the discarded ones included.
    let calibration = calibration(
        stage1_logical_asap_optimization(&per_computation, entries),
        entries,
        data,
        models,
    );
    let run = search_stage2_and_stage3(
        &per_computation,
        stage1,
        entries,
        data,
        models,
        search.keep_every_candidate,
        queries.reports(),
        calibration,
    );
    run.with_inventory(per_computation)
}

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
/// [`run_stage1_to_stage3_from_pass1`] does. Pruning here would come too
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
    let calibration = calibration(stage1.iter(), entries, data, models);
    search_stage2_and_stage3(
        &per_computation,
        stage1.into_iter(),
        entries,
        data,
        models,
        true,
        Vec::new(),
        calibration,
    )
    .with_inventory(per_computation)
}

/// The calibration fit of the formulas to the deployment's measured cost
/// table over `stage1`'s physical candidates
/// ([`PlanSelector::needs_calibration`]), numbered as Stage 2 numbers
/// them; `Ok(None)` without a measured table, when no fit is needed.
fn calibration<S: std::borrow::Borrow<Stage1Candidate>>(
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
struct Searched {
    combinations: usize,
    logical: Vec<LogicalCandidate>,
    physical: Vec<PhysicalCandidate>,
    pruned: Vec<PrunedCandidate>,
    selection: Result<Selection, SelectionError>,
    selected: Option<PhysicalCandidate>,
    selected_logical: Option<Vec<QueryRoot>>,
    stage1_selected: Option<LogicalCandidate>,
}

impl Searched {
    fn with_inventory<Id>(
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
fn search_stage2_and_stage3<Id>(
    per_computation: &PerComputationCandidates<Id>,
    stage1: impl Iterator<Item = Stage1Candidate>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    keep_every_candidate: bool,
    queries: Vec<QuerySearchReport>,
    calibration: Result<Option<CalibrationFit>, SelectionError>,
) -> Searched {
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
            match optimize_physical_asap(&id, roots, entries, data, models.deployment_capabilities)
            {
                Ok(implemented) => {
                    for (k, mut p) in implemented.into_iter().enumerate() {
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
            (Ok(selection), Some(selected))
        }
        Err(SelectionError::NoValidCandidate(mut rejected)) => {
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
    Searched {
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

/// `roots`, the selected candidate's Stage 1 roots, as Stage 3 sized them
/// for their queries' failure probabilities, with the weight requirements
/// it confirmed made proofs.
pub(super) fn selected_logical(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
) -> Option<Vec<QueryRoot>> {
    let targets = &crate::accuracy_targets(entries);
    let sized = crate::resize_for_failure_probability(roots, targets, data).ok()?;
    Some(crate::with_confirmed_weights(&sized, data))
}

/// The planner pipeline behind the optimization-pass interface: the
/// workload's roots in entry order, PromQL roots with each series' full
/// identity as a column (the row representation per-series state needs at
/// runtime; a root with no such representation is planned over its labels
/// alone), through [`run_stage1_to_stage3`] with every entry's accuracy target. The
/// output is the selected physical candidate's roots, with the open weight
/// requirements plan selection confirmed made proofs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stage1ToStage3Pass;

impl OptimizationPass for Stage1ToStage3Pass {
    fn name(&self) -> &'static str {
        "stage1-to-stage3"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let workload = input.workload;
        let promql = workload.query_workload().language == QueryLanguage::PromQL;
        let mut roots: Vec<(usize, QueryRoot)> = workload
            .operator_indices()
            .iter()
            .zip(workload.exprs())
            .map(|(&index, node)| {
                let node = match promql {
                    true => with_promql_series_identity(node).unwrap_or_else(|_| Rc::clone(node)),
                    false => Rc::clone(node),
                };
                (index, QueryRoot::Operator(node))
            })
            .chain(
                workload
                    .scalar_roots()
                    .iter()
                    .map(|(index, expr)| (*index, QueryRoot::Scalar(expr.clone()))),
            )
            .collect();
        roots.sort_by_key(|(index, _)| *index);
        let entries: Vec<_> = workload.query_workload().entries().collect();
        let data = workload.data_workload().cloned().unwrap_or_default();
        let run = run_stage1_to_stage3(roots, &entries, &data, input.models)?;
        let selection = run.stage3_plan_selection?;
        let roots = selection.plan.clone();
        let mut output = PlanOutput::new(Vec::new());
        for (entry_index, root) in roots.iter().enumerate() {
            match root {
                QueryRoot::Operator(node) => output.plans.push(QueryPlan {
                    entry_index,
                    root: Rc::clone(node),
                }),
                QueryRoot::Scalar(expr) => {
                    // One plan per distinct operator the scalar reads.
                    let mut refs: Vec<&Rc<OperatorNode>> = Vec::new();
                    for node in expr.operator_refs() {
                        if !refs.iter().any(|r| Rc::ptr_eq(r, node)) {
                            refs.push(node);
                        }
                    }
                    output
                        .scalar_plans
                        .extend(refs.into_iter().map(|node| QueryPlan {
                            entry_index,
                            root: Rc::clone(node),
                        }));
                    output.scalar_roots.push((entry_index, expr.clone()));
                }
            }
        }
        output.selection = Some(selection);
        Ok(output)
    }
}
