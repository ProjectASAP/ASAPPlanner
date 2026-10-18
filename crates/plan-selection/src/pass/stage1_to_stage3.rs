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
//! Every combination is enumerated and plan selection prices every
//! candidate; nothing is capped. A search that avoids building every
//! candidate is future work (issue #671 surveys the options).

use std::rc::Rc;

use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryLanguage, QueryWorkloadEntry};

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use crate::PlanningModels;
use crate::{select_plan, Selection, SelectionError};
use asap_logical_asap_optimization::per_computation_candidates::{
    generate_per_computation_candidates, PerComputationCandidateError, PerComputationCandidates,
};
use asap_logical_asap_optimization::workload_candidates::{
    stage1_logical_asap_optimization, Pass2Candidate, Stage1Candidate,
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
/// ([`run_stage1_to_stage3`]).
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
    /// candidates (those with [`LogicalCandidate::pass2`]).
    pub stage1_logical_asap_optimization: Vec<LogicalCandidate>,
    /// Stage 2: the physical candidates of each Stage 1 candidate that
    /// composed and was implemented, one per materialization assignment:
    /// `P<n>` (nothing materialized), then `P<n>-m<k>`, from `L<n>`.
    pub stage2_physical_asap_optimization: Vec<PhysicalCandidate>,
    /// Stage 2: the Stage 1 candidates Stage 2 could not implement, with the
    /// reasons.
    pub stage2_pruned: Vec<PrunedCandidate>,
    /// Stage 3: plan selection over the Stage 2 candidates.
    pub stage3_plan_selection: Result<Selection, SelectionError>,
    /// Stage 3: the selected candidate's logical roots, with the weight
    /// requirements it confirmed made proofs ([`Self::selected_logical`]).
    pub stage3_selected_logical: Option<Vec<QueryRoot>>,
}

impl<Id> Stage1ToStage3Run<Id> {
    /// The selected physical candidate, if selection succeeded.
    pub fn selected(&self) -> Option<&PhysicalCandidate> {
        let selected = &self.stage3_plan_selection.as_ref().ok()?.selected;
        self.stage2_physical_asap_optimization
            .iter()
            .find(|p| &p.id == selected)
    }

    /// The composed roots of the logical candidate the selected physical
    /// candidate implements: the plan before physical operator
    /// implementation and materialization, with the weight requirements
    /// Stage 3 confirmed made proofs, as in [`Selection::plan`].
    pub fn selected_logical(&self) -> Option<&[QueryRoot]> {
        self.stage3_selected_logical.as_deref()
    }
}

/// Run Stage 1 → Stage 2 → Stage 3 over the frontends' `roots`, enumerating every
/// whole-workload candidate. `entries[i]` is the workload entry of
/// `roots[i]`: its accuracy target, latency bound, recurrence and
/// predictability. Only a Pass 1 failure is
/// an error; a candidate that does not compose or cannot be implemented is
/// listed with its reason, and a failed selection is reported in the run.
pub fn run_stage1_to_stage3<Id: Clone + std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Stage1ToStage3Run<Id>, PerComputationCandidateError> {
    let per_computation = generate_per_computation_candidates(roots)?;
    Ok(run_stage1_to_stage3_from_pass1(
        per_computation,
        entries,
        data,
        models,
    ))
}

/// [`run_stage1_to_stage3`] after Pass 1: enumerates every Stage 1
/// candidate of `per_computation` (the Pass 1 inventory of the frontends'
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
) -> Stage1ToStage3Run<Id> {
    let per_computation =
        per_computation.without_unsupported_summaries(models.deployment_capabilities);
    let stage1 = stage1_logical_asap_optimization(&per_computation, entries).collect();
    run_stage2_and_stage3(per_computation, stage1, entries, data, models)
}

/// Stage 2 and Stage 3 over `stage1`, the Stage 1 candidates of
/// `per_computation` in enumeration order (all of them, or the ones a caller
/// enumerated): candidate `n` (from 1) is `L<n>`, implemented as `P<n>`
/// with nothing materialized and as `P<n>-m<k>` (`k` from 1) for each other
/// materialization assignment.
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
    let combinations = per_computation.combinations();
    let mut logical: Vec<LogicalCandidate> = Vec::new();
    let mut physical = Vec::new();
    let mut pruned = Vec::new();
    for (index, candidate) in stage1.into_iter().enumerate() {
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
                        physical.push(p);
                    }
                }
                Err(error) => pruned.push(PrunedCandidate {
                    from_logical: id.clone(),
                    reason: error.to_string(),
                }),
            }
        }
        logical.push(LogicalCandidate {
            id,
            choice: candidate.choice,
            roots: candidate.roots,
            pass2: candidate.pass2,
        });
    }
    let selection = select_plan(&physical, entries, data, models);
    let targets = &crate::accuracy_targets(entries);
    let selected_logical = selection.as_ref().ok().and_then(|selection| {
        let from = &physical
            .iter()
            .find(|p| p.id == selection.selected)?
            .from_logical;
        let roots = logical
            .iter()
            .find(|l| &l.id == from)?
            .roots
            .as_ref()
            .ok()?;
        // As Stage 3 sized it for its queries' failure probabilities.
        let sized = crate::resize_for_failure_probability(roots, targets, data).ok()?;
        Some(crate::with_confirmed_weights(&sized, data))
    });
    Stage1ToStage3Run {
        stage3_selected_logical: selected_logical,
        stage1_pass1_per_computation: per_computation,
        combinations,
        stage1_logical_asap_optimization: logical,
        stage2_physical_asap_optimization: physical,
        stage2_pruned: pruned,
        stage3_plan_selection: selection,
    }
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
