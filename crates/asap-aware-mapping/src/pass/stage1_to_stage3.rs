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

use asap_types::ir::flat::flatten;
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryLanguage, QueryRecurrence, QueryWorkloadEntry};

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use crate::asap_aware_cse::{
    capability_sharing_candidates, identical_expression_pass2_candidates, pane_candidates,
    rollup_candidates, rollup_pass2_candidates, sharing_candidates,
    summary_capability_pass2_candidates, topk_reuse_candidates, topk_reuse_pass2_candidates,
    window_composition_pass2_candidates, PaneCandidate, RollupCandidate, SharingCandidate,
    TopKReuseCandidate, MAX_REUSE_CANDIDATES,
};
use crate::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, PerComputationCandidateError,
    PerComputationCandidates,
};
use crate::stage2_physical_asap_optimization::{
    fixed_interval_ms, optimize_physical_asap, PhysicalCandidate,
};
use crate::stage3_plan_selection::{select_plan, Selection, SelectionError};
use crate::PlanningModels;

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

/// What a Pass 2 candidate changed of the Pass 1 candidate it derives
/// from: each set of summary builds in `shared_builds` made one build sized
/// for all its readers (the summary-capability rule), then the build of
/// each of `pane_candidates` built as tumbling panes (the
/// window-composition rule), or each of `rollup_candidates` and
/// `topk_reuse_candidates` answered from another computation (the rollup
/// and exact top-k reuse rules), then the nodes of each of
/// `sharing_candidates` made one node (the identical-expression rule). It
/// has its Pass 1 candidate's `choice`; display labels are the devtools'.
#[derive(Debug, Clone)]
pub struct Pass2Candidate {
    /// The position, in Stage 1 enumeration order from 0, of the Pass 1
    /// candidate it derives from: `L<from_pass1_position + 1>`.
    pub from_pass1_position: usize,
    /// Summary-capability rule: each set of the Pass 1 candidate's summary
    /// builds made one build.
    pub shared_builds: Vec<Vec<Rc<OperatorNode>>>,
    /// Window-composition rule: the builds built as tumbling panes.
    pub pane_candidates: Vec<PaneCandidate>,
    /// Rollup rule: the Pass 1 candidate's coarse builds answered from
    /// finer ones.
    pub rollup_candidates: Vec<RollupCandidate>,
    /// Exact top-k reuse rule: the top-k computations that read one ranking.
    pub topk_reuse_candidates: Vec<TopKReuseCandidate>,
    /// Identical-expression rule: the identical nodes made one node.
    pub sharing_candidates: Vec<SharingCandidate>,
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

/// One Stage 1 candidate in enumeration order, before Stage 2 numbers it.
#[derive(Debug)]
pub struct Stage1Candidate {
    /// The Pass 1 choice (`targets[t].computation_for(choice[t])`: target
    /// `t`'s candidate, or its fused computation's); a Pass 2 candidate has
    /// its Pass 1 candidate's.
    pub choice: Vec<usize>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
    /// For a Pass 2 candidate: what it changed of its Pass 1 candidate.
    pub pass2: Option<Pass2Candidate>,
}

/// Stage 1 (logical ASAP-aware optimization) over the Pass 1 inventory, one
/// candidate at a time: each composed Pass 1 candidate (every choice of a
/// per-computation candidate for every target, the last target varying
/// fastest), followed by its Pass 2 candidates: the identical-expression
/// rule's, then the window-composition rule's, then the reuse rules'
/// (exact top-k reuse and rollup), then each summary-capability
/// candidate followed by its own identical-expression and
/// window-composition candidates; every window-composition candidate is
/// followed by its own identical-expression candidates. `recurrence[q]` is
/// how often query `q` is evaluated: the window-composition rule splits
/// only builds of queries that repeat at a fixed interval. A Pass 2
/// candidate equal to an earlier one of the same Pass 1 candidate (same
/// nodes, same sharing) is listed once. Lazy and uncapped: a caller that
/// limits the candidates stops consuming it.
pub fn stage1_logical_asap_optimization<'a, Id: Clone>(
    per_computation: &'a PerComputationCandidates<Id>,
    recurrence: &[QueryRecurrence],
) -> impl Iterator<Item = Stage1Candidate> + 'a {
    let interval_ms: Vec<Option<u64>> = recurrence.iter().map(fixed_interval_ms).collect();
    let mut choice = Some(vec![0; per_computation.targets.len()]);
    let mut queue = std::collections::VecDeque::new();
    let mut position = 0usize;
    std::iter::from_fn(move || {
        while queue.is_empty() {
            let current = choice.as_mut()?;
            let skip = per_computation.absorbed_choice_varies(current);
            let this = current.clone();
            if !next_choice(per_computation, current) {
                choice = None;
            }
            if skip {
                continue;
            }
            let roots = compose_logical_candidate(per_computation, &this)
                .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>());
            let from = position + queue.len();
            let pass2 = match &roots {
                Ok(roots) => pass2_candidates(roots, &interval_ms),
                Err(_) => Vec::new(),
            };
            queue.push_back(Stage1Candidate {
                choice: this.clone(),
                roots,
                pass2: None,
            });
            for Pass2 {
                rules,
                sharing_candidates,
                roots,
            } in pass2
            {
                queue.push_back(Stage1Candidate {
                    choice: this.clone(),
                    roots,
                    pass2: Some(Pass2Candidate {
                        from_pass1_position: from,
                        shared_builds: rules.shared_builds,
                        pane_candidates: rules.pane_candidates,
                        rollup_candidates: rules.rollup_candidates,
                        topk_reuse_candidates: rules.topk_reuse_candidates,
                        sharing_candidates,
                    }),
                });
            }
        }
        position += 1;
        queue.pop_front()
    })
}

/// A Pass 2 candidate: what the rules other than the identical-expression
/// rule changed, the identical nodes it shares, and its roots.
struct Pass2 {
    rules: Rules,
    sharing_candidates: Vec<SharingCandidate>,
    roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// What the rules other than the identical-expression rule changed: the
/// summary builds shared (summary-capability rule), the builds built as
/// tumbling panes (window-composition rule) and the computations answered
/// from another (rollup and exact top-k reuse rules).
#[derive(Debug, Clone, Default)]
struct Rules {
    shared_builds: Vec<Vec<Rc<OperatorNode>>>,
    pane_candidates: Vec<PaneCandidate>,
    rollup_candidates: Vec<RollupCandidate>,
    topk_reuse_candidates: Vec<TopKReuseCandidate>,
}

impl Pass2 {
    fn failed(rules: &Rules, error: PerComputationCandidateError) -> Self {
        Self {
            rules: rules.clone(),
            sharing_candidates: Vec::new(),
            roots: Err(error),
        }
    }
}

/// The Pass 2 candidates of the composed candidate with roots `roots`, in
/// enumeration order, each distinct candidate once. `interval_ms[q]` is the
/// fixed interval of query `q`, if it repeats at one.
fn pass2_candidates(roots: &[QueryRoot], interval_ms: &[Option<u64>]) -> Vec<Pass2> {
    let mut found = Pass2List {
        seen: vec![flatten(roots).0],
        out: Vec::new(),
    };
    let none = Rules::default();
    found.identical_expression(&none, roots);
    found.window_composition(&none, roots, interval_ms);
    found.reuse(&none, roots);
    let candidates = capability_sharing_candidates(roots);
    let pass2_candidates = match summary_capability_pass2_candidates(roots, &candidates) {
        Ok(pass2_candidates) => pass2_candidates,
        Err(error) => {
            found.out.push(Pass2::failed(&none, error));
            return found.out;
        }
    };
    for pass2_candidate in pass2_candidates {
        let rules = Rules {
            shared_builds: pass2_candidate.shared_builds,
            ..Rules::default()
        };
        match pass2_candidate.roots {
            Ok(roots) => {
                found.push(&rules, Vec::new(), &roots);
                found.identical_expression(&rules, &roots);
                found.window_composition(&rules, &roots, interval_ms);
            }
            // Listed as a candidate that cannot be composed; the other
            // Pass 2 candidates go on.
            Err(error) => found.out.push(Pass2::failed(&rules, error)),
        }
    }
    found.out
}

/// Pass 2 candidates found so far, and the flattened DAG of every candidate
/// listed (the Pass 1 candidate's included), so each is listed once.
struct Pass2List {
    seen: Vec<asap_types::ir::flat::FlatDag>,
    out: Vec<Pass2>,
}

impl Pass2List {
    fn push(
        &mut self,
        rules: &Rules,
        sharing_candidates: Vec<SharingCandidate>,
        roots: &[QueryRoot],
    ) {
        let flat = flatten(roots).0;
        if self.seen.contains(&flat) {
            return;
        }
        self.seen.push(flat);
        self.out.push(Pass2 {
            rules: rules.clone(),
            sharing_candidates,
            roots: Ok(roots.to_vec()),
        });
    }

    /// The identical-expression Pass 2 candidates of `roots`, which the
    /// other rules changed as `rules` says.
    fn identical_expression(&mut self, rules: &Rules, roots: &[QueryRoot]) {
        let candidates = sharing_candidates(roots);
        for pass2_candidate in identical_expression_pass2_candidates(roots, &candidates) {
            self.push(
                rules,
                pass2_candidate.sharing_candidates,
                &pass2_candidate.roots,
            );
        }
    }

    /// The window-composition Pass 2 candidates of `roots`, which the other
    /// rules changed as `rules` says, each followed by its
    /// identical-expression Pass 2 candidates.
    fn window_composition(
        &mut self,
        rules: &Rules,
        roots: &[QueryRoot],
        interval_ms: &[Option<u64>],
    ) {
        let candidates = pane_candidates(roots, interval_ms);
        let pass2_candidates = match window_composition_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Pass2::failed(rules, error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            let rules = Rules {
                pane_candidates: pass2_candidate.pane_candidates,
                ..rules.clone()
            };
            match pass2_candidate.roots {
                Ok(roots) => {
                    self.push(&rules, Vec::new(), &roots);
                    self.identical_expression(&rules, &roots);
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
            }
        }
    }

    /// The reuse Pass 2 candidates of `roots`: the exact top-k reuse
    /// rule's, then each rollup Pass 2 candidate followed by its own exact
    /// top-k reuse Pass 2 candidates; each followed by its
    /// identical-expression Pass 2 candidates. They are listed eagerly,
    /// like every Pass 2 rule's: more than [`MAX_REUSE_CANDIDATES`] rollup
    /// and top-k reuse candidates together list one error.
    fn reuse(&mut self, rules: &Rules, roots: &[QueryRoot]) {
        let candidates = rollup_candidates(roots);
        // Every rollup subset is combined with every top-k subset: cap the
        // product, not each factor.
        if candidates.len() + topk_reuse_candidates(roots).len() > MAX_REUSE_CANDIDATES {
            self.out.push(Pass2::failed(
                rules,
                PerComputationCandidateError::Unsupported(
                    "more than 20 rollup and top-k reuse candidates together (2^n Pass 2 candidates)",
                ),
            ));
            return;
        }
        self.topk_reuse(rules, roots);
        let pass2_candidates = match rollup_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Pass2::failed(rules, error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            let mut rules = rules.clone();
            rules
                .rollup_candidates
                .extend(pass2_candidate.rollup_candidates);
            match pass2_candidate.roots {
                Ok(roots) => {
                    self.push(&rules, Vec::new(), &roots);
                    self.identical_expression(&rules, &roots);
                    self.topk_reuse(&rules, &roots);
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
            }
        }
    }

    /// The exact top-k reuse Pass 2 candidates of `roots`, each followed by
    /// its identical-expression Pass 2 candidates.
    fn topk_reuse(&mut self, rules: &Rules, roots: &[QueryRoot]) {
        let candidates = topk_reuse_candidates(roots);
        let pass2_candidates = match topk_reuse_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Pass2::failed(rules, error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            let mut rules = rules.clone();
            rules
                .topk_reuse_candidates
                .extend(pass2_candidate.topk_reuse_candidates);
            match pass2_candidate.roots {
                Ok(roots) => {
                    self.push(&rules, Vec::new(), &roots);
                    self.identical_expression(&rules, &roots);
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
            }
        }
    }
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
    let per_computation = per_computation.without_unsupported_summaries(models.deployment);
    let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
    let stage1 = stage1_logical_asap_optimization(&per_computation, &recurrence).collect();
    run_stage2_and_stage3(per_computation, stage1, entries, data, models)
}

/// Stage 2 and Stage 3 over `stage1`, the Stage 1 candidates of
/// `per_computation` in enumeration order (all of them, or the ones a caller
/// enumerated): candidate `n` (from 1) is `L<n>`, implemented as `P<n>`
/// with nothing materialized and as `P<n>-m<k>` (`k` from 1) for each other
/// materialization assignment.
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
            match optimize_physical_asap(&id, roots, entries, data, models.deployment) {
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
    let targets = &crate::stage3_plan_selection::accuracy_targets(entries);
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
        let sized =
            crate::stage3_plan_selection::resize_for_failure_probability(roots, targets, data)
                .ok()?;
        Some(crate::stage3_plan_selection::with_confirmed_weights(
            &sized, data,
        ))
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

/// Mixed-radix increment of `choice`, each digit over its target's
/// candidates and then its fused computation's: the last target varies fastest.
/// `false` when a carry out of the first target ends the enumeration.
fn next_choice<Id>(inventory: &PerComputationCandidates<Id>, choice: &mut [usize]) -> bool {
    for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
        *digit += 1;
        if *digit < target.choice_count() {
            return true;
        }
        *digit = 0;
    }
    false
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;

    /// The composed candidate of `queries`, every target at its first
    /// (exact) candidate.
    fn exact(queries: &[String]) -> Vec<QueryRoot> {
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let root = lower_promql(q, asap_types::types::AccuracyTarget::exact());
                (
                    i,
                    QueryRoot::Operator(with_promql_series_identity(&root).unwrap()),
                )
            })
            .collect();
        let inventory = generate_per_computation_candidates(roots).unwrap();
        compose_logical_candidate(&inventory, &vec![0; inventory.targets.len()])
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect()
    }

    /// Rollup and exact top-k reuse candidates multiply (every subset of
    /// one with every subset of the other): past 20 together the candidate
    /// lists one error instead of enumerating 2^21 Pass 2 candidates.
    #[test]
    fn reuse_candidates_past_20_together_are_an_error() {
        let mut queries = Vec::new();
        for i in 0..11 {
            queries.push(format!("sum by (job) (m{i})"));
            queries.push(format!("sum by (job, instance) (m{i})"));
        }
        for i in 0..10 {
            queries.push(format!("topk by (job) (5, sum by (job, instance) (t{i}))"));
            queries.push(format!("topk by (job) (10, sum by (job, instance) (t{i}))"));
        }
        let roots = exact(&queries);
        assert_eq!(rollup_candidates(&roots).len(), 11);
        assert_eq!(topk_reuse_candidates(&roots).len(), 10);
        let mut found = Pass2List {
            seen: vec![flatten(&roots).0],
            out: Vec::new(),
        };
        found.reuse(&Rules::default(), &roots);
        let [failed] = found.out.as_slice() else {
            panic!("one error, {} candidates", found.out.len())
        };
        assert!(matches!(
            failed.roots,
            Err(PerComputationCandidateError::Unsupported(_))
        ));
    }
}
