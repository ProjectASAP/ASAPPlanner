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
//!    by its Pass 2 candidates from the identical-expression rule
//!    ([`identical_expression_variants`]: one per subset of its sharing
//!    groups; the independent candidate is kept);
//! 2. Stage 2, physical ASAP-aware optimization of every Stage 1 candidate
//!    ([`optimize_physical_asap`]);
//! 3. Stage 3, plan selection over the physical candidates ([`select_plan`]).
//!
//! Stage 0, the query-language frontends, runs before it (the `asap-planner`
//! facade lowers the workload). Pass 2's summary-capability and
//! window-composition rules are not planned yet.
//!
//! Every combination is enumerated and plan selection prices every
//! candidate; nothing is capped. A search that avoids building every
//! candidate is future work (issue #671 surveys the options).

use std::rc::Rc;

use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::post_asap::SketchAlgorithm;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{DataWorkload, QueryLanguage, QueryRecurrence};

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use crate::asap_aware_cse::identical_expression_variants;
use crate::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    PerComputationCandidateError, PerComputationCandidates,
};
use crate::stage2_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use crate::stage3_plan_selection::{select_plan, Selection, SelectionError};
use crate::PlanningModels;

/// One whole-workload CandidateLogicalASAPDAG: `choice[t]` indexes
/// `targets[t].choices.candidates` of the Pass 1 inventory. `roots` are the
/// composed query roots in input order, or why the choice did not compose.
#[derive(Debug)]
pub struct LogicalCandidate {
    /// `L<n>`, numbered in enumeration order.
    pub id: String,
    /// Human-readable name of the candidate, e.g. "Q1 exact · Q2 CMS+heap", used only for
    /// display.
    pub display_name: String,
    pub choice: Vec<usize>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// A Stage 1 candidate Pass 2's identical-expression rule added: `id` is
/// `from_logical` with the nodes of each group in `shared` (named by
/// operator kind, children first) made one node. It has `from_logical`'s
/// `choice`.
#[derive(Debug, Clone, PartialEq)]
pub struct IdenticalExpressionCandidate {
    pub id: String,
    pub from_logical: String,
    pub shared: Vec<String>,
}

/// A composed logical candidate that physical ASAP-aware optimization could
/// not implement, with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate {
    pub from_logical: String,
    /// Human-readable name of the candidate, e.g. "Q1 exact · Q2 CMS+heap", used only for
    /// display.
    pub display_name: String,
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
    /// Stage 1, Pass 2: the candidates the identical-expression rule added,
    /// in enumeration order.
    pub stage1_pass2_identical_expression: Vec<IdenticalExpressionCandidate>,
    /// Stage 1: every whole-workload logical candidate, in enumeration
    /// order: each composed Pass 1 candidate, followed by its Pass 2
    /// candidates.
    pub stage1_logical_asap_optimization: Vec<LogicalCandidate>,
    /// Stage 2: one physical candidate (`P<n>` from `L<n>`) per Stage 1
    /// candidate that composed and was implemented.
    pub stage2_physical_asap_optimization: Vec<PhysicalCandidate>,
    /// Stage 2: the Stage 1 candidates Stage 2 could not implement, with the
    /// reasons.
    pub stage2_pruned: Vec<PrunedCandidate>,
    /// Stage 3: plan selection over the Stage 2 candidates.
    pub stage3_plan_selection: Result<Selection, SelectionError>,
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
    /// implementation and materialization.
    pub fn selected_logical(&self) -> Option<&[QueryRoot]> {
        let from = &self.selected()?.from_logical;
        let logical = self
            .stage1_logical_asap_optimization
            .iter()
            .find(|l| &l.id == from)?;
        logical.roots.as_deref().ok()
    }
}

/// Run Stage 1 → Stage 2 → Stage 3 over the frontends' `roots`, enumerating every
/// whole-workload candidate. `targets[i]` is the accuracy
/// requirement of `roots[i]` (`None` imposes none). Only a Pass 1 failure is
/// an error; a candidate that does not compose or cannot be implemented is
/// listed with its reason, and a failed selection is reported in the run.
pub fn run_stage1_to_stage3<Id: Clone + std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
    targets: &[Option<AccuracyTarget>],
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Stage1ToStage3Run<Id>, PerComputationCandidateError> {
    let per_computation = generate_per_computation_candidates(roots)?;
    Ok(run_stage1_to_stage3_from_pass1(
        per_computation,
        targets,
        recurrence,
        data,
        models,
    ))
}

/// One Stage 1 candidate in enumeration order, before Stage 2 numbers it.
#[derive(Debug)]
pub struct Stage1Candidate {
    /// Human-readable name, used only for display.
    pub display_name: String,
    /// The Pass 1 choice (`targets[t].choices.candidates[choice[t]]`); a
    /// Pass 2 candidate has its Pass 1 candidate's.
    pub choice: Vec<usize>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
    /// For a Pass 2 candidate: the position, in the enumeration, of the
    /// Pass 1 candidate it shares the identical sub-DAGs of, and the shared
    /// groups (operator kinds, children first).
    pub pass2_of: Option<(usize, Vec<String>)>,
}

/// Stage 1 (logical ASAP-aware optimization) over the Pass 1 inventory, one
/// candidate at a time: each composed Pass 1 candidate (every choice of a
/// per-computation candidate for every target, the last target varying
/// fastest), followed by its Pass 2 identical-expression candidates. Lazy
/// and uncapped: a caller that limits the candidates stops consuming it.
pub fn stage1_logical_asap_optimization<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
) -> impl Iterator<Item = Stage1Candidate> + '_ {
    let query_of_target = query_of_target(per_computation);
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
            let display_name = display_name(per_computation, &query_of_target, &this);
            let roots = compose_logical_candidate(per_computation, &this)
                .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>());
            let (groups, variants) = match &roots {
                Ok(roots) => {
                    let groups = crate::asap_aware_cse::sharing_groups(roots);
                    let variants = identical_expression_variants(roots, &groups);
                    (groups, variants)
                }
                Err(_) => (Vec::new(), Vec::new()),
            };
            let from = position + queue.len();
            queue.push_back(Stage1Candidate {
                display_name: display_name.clone(),
                choice: this.clone(),
                roots,
                pass2_of: None,
            });
            for variant in variants {
                let shared: Vec<String> = variant
                    .shared_groups
                    .iter()
                    .map(|&g| format!("{} #{}", groups[g].nodes[0].operator.kind_name(), g + 1))
                    .collect();
                queue.push_back(Stage1Candidate {
                    display_name: format!("{display_name} · shared {}", shared.join(", ")),
                    choice: this.clone(),
                    roots: Ok(variant.roots),
                    pass2_of: Some((from, shared)),
                });
            }
        }
        position += 1;
        queue.pop_front()
    })
}

/// [`run_stage1_to_stage3`] after Pass 1: enumerates every Stage 1
/// candidate of `per_computation` (the Pass 1 inventory of the frontends'
/// roots, [`stage1_logical_asap_optimization`]), so a caller can read the
/// inventory, e.g. its
/// [`combinations`](PerComputationCandidates::combinations), before
/// enumerating.
pub fn run_stage1_to_stage3_from_pass1<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    targets: &[Option<AccuracyTarget>],
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Stage1ToStage3Run<Id> {
    let stage1 = stage1_logical_asap_optimization(&per_computation).collect();
    run_stage2_and_stage3(per_computation, stage1, targets, recurrence, data, models)
}

/// Stage 2 and Stage 3 over `stage1`, the Stage 1 candidates of
/// `per_computation` in enumeration order (all of them, or the ones a caller
/// enumerated): candidate `n` (from 1) is `L<n>`, implemented as `P<n>`.
pub fn run_stage2_and_stage3<Id: Clone + std::fmt::Debug>(
    per_computation: PerComputationCandidates<Id>,
    stage1: Vec<Stage1Candidate>,
    targets: &[Option<AccuracyTarget>],
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Stage1ToStage3Run<Id> {
    let combinations = per_computation.combinations();
    let mut logical: Vec<LogicalCandidate> = Vec::new();
    let mut physical = Vec::new();
    let mut pruned = Vec::new();
    let mut pass2 = Vec::new();
    for (index, candidate) in stage1.into_iter().enumerate() {
        let number = index + 1;
        let id = format!("L{number}");
        if let Ok(roots) = &candidate.roots {
            match optimize_physical_asap(&id, roots) {
                Ok(mut implemented) => {
                    implemented.id = format!("P{number}");
                    implemented.display_name = candidate.display_name.clone();
                    physical.push(implemented);
                }
                Err(error) => pruned.push(PrunedCandidate {
                    from_logical: id.clone(),
                    display_name: candidate.display_name.clone(),
                    reason: error.to_string(),
                }),
            }
        }
        if let Some((from, shared)) = candidate.pass2_of {
            pass2.push(IdenticalExpressionCandidate {
                id: id.clone(),
                from_logical: format!("L{}", from + 1),
                shared,
            });
        }
        logical.push(LogicalCandidate {
            id,
            display_name: candidate.display_name,
            choice: candidate.choice,
            roots: candidate.roots,
        });
    }
    let selection = select_plan(&physical, targets, recurrence, data, models);
    Stage1ToStage3Run {
        stage1_pass1_per_computation: per_computation,
        combinations,
        stage1_pass2_identical_expression: pass2,
        stage1_logical_asap_optimization: logical,
        stage2_physical_asap_optimization: physical,
        stage2_pruned: pruned,
        stage3_plan_selection: selection,
    }
}

/// Mixed-radix increment of `choice`: the last target varies fastest.
/// `false` when a carry out of the first target ends the enumeration.
fn next_choice<Id>(inventory: &PerComputationCandidates<Id>, choice: &mut [usize]) -> bool {
    for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
        *digit += 1;
        if *digit < target.choices.candidates.len() {
            return true;
        }
        *digit = 0;
    }
    false
}

/// `query_of_target[t]`: the index of the query root that first reads target
/// `t` (the first root, in input order, from which the target is reachable).
/// [`display_name`] lists each target's choice under that query.
fn query_of_target<Id>(inventory: &PerComputationCandidates<Id>) -> Vec<usize> {
    inventory
        .targets
        .iter()
        .map(|target| {
            inventory
                .roots
                .iter()
                .position(|(_, root)| {
                    root_operators(root).into_iter().any(|node| {
                        OperatorNode::reachable(node)
                            .iter()
                            .any(|n| Rc::ptr_eq(n, &target.target))
                    })
                })
                .expect("every target is reached from a root")
        })
        .collect()
}

fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// A human-readable name of a candidate, e.g. "Q1 exact · Q2 CMS+heap"
/// (exact accumulators are listed in parentheses), used only for display in
/// the `stage_pipeline` devtool and the viewer; planning never reads it.
fn display_name<Id>(
    inventory: &PerComputationCandidates<Id>,
    query_of_target: &[usize],
    choice: &[usize],
) -> String {
    // Targets a chosen whole-expression candidate absorbs are not computed.
    let absorbed: Vec<usize> = inventory
        .targets
        .iter()
        .zip(choice)
        .filter(|(target, &index)| {
            matches!(
                target.choices.candidates[index],
                ComputationCandidate::WholeExpression(_)
            )
        })
        .filter_map(|(target, _)| target.absorbs)
        .collect();
    (0..inventory.roots.len())
        .map(|query| {
            let mut sketches = Vec::new();
            let mut accumulators = Vec::new();
            for (t, (target, (&target_query, &index))) in inventory
                .targets
                .iter()
                .zip(query_of_target.iter().zip(choice))
                .enumerate()
            {
                if target_query != query || absorbed.contains(&t) {
                    continue;
                }
                match &target.choices.candidates[index] {
                    ComputationCandidate::Exact => {}
                    ComputationCandidate::ExactAccumulator { kind, .. } => {
                        accumulators.push(format!("{kind:?} acc"))
                    }
                    ComputationCandidate::AvgAsSumCount => {
                        accumulators.push("Sum/Count acc".into())
                    }
                    ComputationCandidate::Summary(algorithm)
                    | ComputationCandidate::WholeExpression(algorithm) => {
                        sketches.push(match algorithm {
                            SketchAlgorithm::CmsWithHeap => "CMS+heap".to_string(),
                            SketchAlgorithm::CountSketchWithHeap => "CountSketch+heap".to_string(),
                            other => format!("{other:?}"),
                        })
                    }
                }
            }
            let mut text = format!("Q{} ", query + 1);
            text += &if sketches.is_empty() {
                "exact".to_string()
            } else {
                sketches.join(" + ")
            };
            if !accumulators.is_empty() {
                text += &format!(" ({})", accumulators.join(", "));
            }
            text
        })
        .collect::<Vec<_>>()
        .join(" · ")
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
        let targets: Vec<_> = workload
            .query_workload()
            .entries()
            .map(|entry| Some(entry.requirements.accuracy.target()))
            .collect();
        let recurrence: Vec<_> = workload
            .query_workload()
            .entries()
            .map(|entry| entry.recurrence)
            .collect();
        let data = workload.data_workload().cloned().unwrap_or_default();
        let run = run_stage1_to_stage3(roots, &targets, &recurrence, &data, input.models)?;
        let Stage1ToStage3Run {
            stage2_physical_asap_optimization: physical,
            stage3_plan_selection: selection,
            ..
        } = run;
        let selection = selection?;
        let selected = physical
            .into_iter()
            .find(|p| p.id == selection.selected)
            .expect("the selected candidate is a physical candidate");
        // The executor reads no data workload: answer every open weight
        // requirement plan selection confirmed.
        let roots = crate::stage3_plan_selection::with_confirmed_weights(&selected.roots, &data);
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
