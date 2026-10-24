//! Stage orchestration shared by memo selection and the temporary legacy route.
//!
//! On memo routes, logical identity is established before Pass 1; deployment
//! pruning and candidate population retain those group IDs. This module owns
//! the public stage driver rather than an individual search implementation.

use std::rc::Rc;

use asap_logical_asap_optimization::memo::LogicalMemo;

use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryLanguage, QueryWorkloadEntry};

use crate::search::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use crate::PlanningModels;
use crate::{CostModel, RoutedToLegacy, SearchAlgorithm, Selection, SelectionError};
use asap_logical_asap_optimization::per_computation_candidates::{
    generate_per_computation_candidates, generate_per_computation_candidates_with_memo,
    PerComputationCandidateError, PerComputationCandidates,
};
use asap_logical_asap_optimization::workload_candidates::Pass2Candidate;
use asap_physical_asap_optimization::PhysicalCandidate;

pub use crate::search::candidate_evaluation::run_stage2_and_stage3;
use crate::search::legacy::stage1_to_stage3::legacy_search;

/// A selected execution plan and the logical roots from which it was built.
pub(crate) type PlanSearchResult =
    Result<(Selection, PhysicalCandidate, Vec<QueryRoot>), SelectionError>;

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
    /// The search algorithm (joint-planning-search design: the legacy
    /// search, or the memo over logical sub-expressions, "Memo-group
    /// search"); ignored with `exhaustive`. Default
    /// [`SearchAlgorithm::Legacy`] until the MILP selects the exhaustive
    /// search's price on every equivalence test.
    pub search: SearchAlgorithm,
    /// The longest the MILP search may run, in ms; `None` for no limit.
    /// Stopped at the limit, it selects its best plan so far and reports
    /// the gap to its bound ([`crate::MilpReport::gap`]) and
    /// [`crate::MilpReport::time_limit_reached`]: not proven
    /// optimal unless the gap is 0. CandidateConnections defaults to 30 seconds
    /// and at most 128 evaluator refinements; its separate report names the
    /// restricted catalog and termination reason.
    pub milp_time_limit_ms: Option<u64>,
}

impl Stage1ToStage3Search {
    /// The exhaustive search keeping every candidate: what the planner did
    /// before the search was pruned.
    pub fn exhaustive() -> Self {
        Self {
            keep_every_candidate: true,
            exhaustive: true,
            search: SearchAlgorithm::Legacy,
            milp_time_limit_ms: None,
        }
    }

    /// The default search run with `algorithm`.
    pub fn with_algorithm(algorithm: SearchAlgorithm) -> Self {
        Self {
            search: algorithm,
            ..Self::default()
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
    // Legacy routes do not pay for memo identity they will never use.
    // Charge initial memo construction and Pass 1 to the shared budget.
    let generation_started = std::time::Instant::now();
    let uses_memo = search.search == SearchAlgorithm::CandidateConnections
        || (!search.exhaustive
            && match search.search {
                SearchAlgorithm::Legacy => false,
                SearchAlgorithm::MemoBruteForce | SearchAlgorithm::CandidateConnections => true,
                SearchAlgorithm::MemoMilp => matches!(
                    models.plan_selection.cost_model,
                    CostModel::UsageOverTime(_)
                ),
            });
    if uses_memo {
        for (_, root) in &roots {
            root.validate_structure()?;
        }
    }
    let initial = uses_memo.then(|| LogicalMemo::from_roots(&roots));
    let per_computation = match &initial {
        Some(memo) => generate_per_computation_candidates_with_memo(roots, memo)?,
        None => generate_per_computation_candidates(roots)?,
    };
    Ok(run_with_initial_memo(
        per_computation,
        entries,
        data,
        models,
        search,
        initial,
        Some(generation_started.elapsed().as_secs_f64() * 1000.0),
    ))
}

/// [`run_stage1_to_stage3_with`] after Pass 1: searches the Stage 1
/// candidates of `per_computation` (the Pass 1 inventory of the frontends'
/// roots, [`asap_logical_asap_optimization::workload_candidates::stage1_logical_asap_optimization`]) without the summaries the
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
    run_with_initial_memo(per_computation, entries, data, models, search, None, None)
}

fn run_with_initial_memo<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    search: Stage1ToStage3Search,
    initial: Option<LogicalMemo>,
    local_candidate_generation_ms: Option<f64>,
) -> Stage1ToStage3Run<Id> {
    if search.search == SearchAlgorithm::CandidateConnections {
        let pruned = per_computation.without_unsupported_summaries(models.deployment_capabilities);
        let initial = initial.unwrap_or_else(|| LogicalMemo::from_roots(&pruned.roots));
        let mut archive = crate::search::milp::candidate_route::CandidateArchive::default();
        let found = crate::search::milp::candidate_route::candidate_connection_search(
            initial,
            &pruned,
            entries,
            data,
            models,
            search,
            local_candidate_generation_ms,
            &mut archive,
        );
        let mut run = memo_run(
            pruned,
            found,
            entries,
            data,
            SearchAlgorithm::CandidateConnections,
        );
        run.stage1_logical_asap_optimization = archive.logical;
        run.stage2_physical_asap_optimization = archive.physical;
        run.stage2_pruned = archive.pruned;
        return run;
    }
    let routed_to_legacy = match (search.exhaustive, search.search) {
        (_, SearchAlgorithm::Legacy) => None,
        (true, _) => Some(RoutedToLegacy::ExhaustiveRequested),
        (false, SearchAlgorithm::MemoBruteForce) => {
            let pruned =
                per_computation.without_unsupported_summaries(models.deployment_capabilities);
            let found = crate::search::memo::brute_force::brute_force_with_initial_memo(
                initial.unwrap_or_else(|| LogicalMemo::from_roots(&pruned.roots)),
                &pruned,
                entries,
                data,
                models,
            );
            return memo_run(
                pruned,
                found,
                entries,
                data,
                SearchAlgorithm::MemoBruteForce,
            );
        }
        (false, SearchAlgorithm::CandidateConnections) => unreachable!("handled above"),
        (false, SearchAlgorithm::MemoMilp) => {
            if !matches!(
                models.plan_selection.cost_model,
                CostModel::UsageOverTime(_)
            ) {
                Some(RoutedToLegacy::CostModelNotSupported)
            } else {
                let pruned = per_computation
                    .clone()
                    .without_unsupported_summaries(models.deployment_capabilities);
                let found = crate::search::milp::model::search_with_initial_memo(
                    initial.unwrap_or_else(|| LogicalMemo::from_roots(&pruned.roots)),
                    &pruned,
                    entries,
                    data,
                    models,
                    search.milp_time_limit_ms,
                );
                return memo_run(pruned, found, entries, data, SearchAlgorithm::MemoMilp);
            }
        }
    };
    let mut run = legacy_search(per_computation, entries, data, models, search);
    if let Ok(selection) = &mut run.stage3_plan_selection {
        selection.search.algorithm = SearchAlgorithm::Legacy;
        selection.search.routed_to_legacy = routed_to_legacy;
    }
    run
}

/// The run of a memo search (`algorithm`) over `per_computation`, pruned
/// for the deployment, that `found` the selection, its physical candidate
/// and its logical roots, or why none is valid.
fn memo_run<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    found: crate::search::pipeline::PlanSearchResult,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    algorithm: SearchAlgorithm,
) -> Stage1ToStage3Run<Id> {
    let combinations = per_computation.combinations();
    let (selection, selected, logical) = match found {
        Ok((mut selection, selected, logical)) => {
            selection.search.algorithm = algorithm;
            (Ok(selection), Some(selected), Some(logical))
        }
        Err(error) => (Err(error), None, None),
    };
    let selected_logical = logical
        .as_ref()
        .and_then(|roots| selected_logical(roots, entries, data));
    let from_logical = selected.as_ref().map(|s| s.from_logical.clone());
    Stage1ToStage3Run {
        stage1_pass1_per_computation: per_computation,
        combinations,
        stage1_logical_asap_optimization: Vec::new(),
        stage2_physical_asap_optimization: Vec::new(),
        stage2_pruned: Vec::new(),
        stage3_plan_selection: selection,
        stage3_selected: selected,
        stage3_selected_logical: selected_logical,
        // The selected plan's logical candidate, under the id its physical
        // candidates name (`from_logical`).
        stage1_selected: logical
            .zip(from_logical)
            .map(|(roots, id)| LogicalCandidate {
                id,
                choice: Vec::new(),
                roots: Ok(roots),
                pass2: None,
            }),
    }
}

/// `roots`, the selected candidate's Stage 1 roots, as Stage 3 sized them
/// for their queries' failure probabilities, with the weight requirements
/// it confirmed made proofs.
pub(crate) fn selected_logical(
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
