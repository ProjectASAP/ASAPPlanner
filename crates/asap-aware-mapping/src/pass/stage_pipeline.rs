//! The planner pipeline (#509 MVP): the planning stages in order over the
//! frontends' query roots, and [`StagePipeline`], the
//! [`OptimizationPass`](super::OptimizationPass) that runs them.
//!
//! [`plan_stages`] runs, for one workload:
//! 1. logical ASAP-aware optimization: Pass 1 per-computation candidate
//!    generation, then composition of one whole-workload candidate per choice
//!    of a per-computation candidate for every target (the Cartesian product,
//!    in mixed-radix order, the last target varying fastest);
//! 2. physical ASAP-aware optimization of every composed candidate
//!    ([`optimize_physical_asap`]);
//! 3. plan selection over the physical candidates ([`select_plan`]).
//!
//! The language-specific frontends run before it (the `asap-planner` facade
//! lowers the workload). Pass 2 (ASAP-aware CSE) is not planned yet.
//!
//! Plan selection prices every enumerated candidate; there is no search
//! strategy that avoids building them (issue #671 surveys the options). The
//! enumeration is capped at `max_candidates`: a workload with more
//! combinations is selected among the first `max_candidates` in enumeration
//! order, and [`StagePipelineRun::capped`] says so. The first combination
//! takes every target's exact option, so the exact plan is always among them.

use std::rc::Rc;

use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::post_asap::SketchAlgorithm;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{DataWorkload, QueryLanguage};

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use crate::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    PerComputationCandidateError, PerComputationCandidates,
};
use crate::physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use crate::plan_selection::{select_plan, Selection, SelectionError};
use crate::PlanningModels;

/// The enumeration cap [`StagePipeline`] uses.
pub const MAX_ENUMERATED_CANDIDATES: usize = 4096;

/// One whole-workload CandidateLogicalASAPDAG: `choice[t]` indexes
/// `targets[t].choices.candidates` of the Pass 1 inventory. `roots` are the
/// composed query roots in input order, or why the choice did not compose.
#[derive(Debug)]
pub struct LogicalCandidate {
    /// `L<n>`, numbered in enumeration order.
    pub id: String,
    /// E.g. "Q1 exact · Q2 CMS+heap"; exact accumulators in parentheses.
    pub label: String,
    pub choice: Vec<usize>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// A composed logical candidate that physical ASAP-aware optimization could
/// not implement, with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate {
    pub from_logical: String,
    pub label: String,
    pub reason: String,
}

/// Every stage's output for one workload ([`plan_stages`]).
#[derive(Debug)]
pub struct StagePipelineRun<Id> {
    /// Pass 1's per-computation candidates, with the roots they read.
    pub per_computation: PerComputationCandidates<Id>,
    /// Size of the Cartesian product of the targets' candidates, saturating
    /// at `usize::MAX`.
    pub combinations: usize,
    /// Whether `combinations` exceeded the enumeration cap.
    pub capped: bool,
    /// The enumerated logical candidates, in enumeration order.
    pub logical: Vec<LogicalCandidate>,
    /// One physical candidate (`P<n>` from `L<n>`) per logical candidate that
    /// composed and was implemented.
    pub physical: Vec<PhysicalCandidate>,
    pub pruned: Vec<PrunedCandidate>,
    pub selection: Result<Selection, SelectionError>,
}

impl<Id> StagePipelineRun<Id> {
    /// The selected physical candidate, if selection succeeded.
    pub fn selected(&self) -> Option<&PhysicalCandidate> {
        let selected = &self.selection.as_ref().ok()?.selected;
        self.physical.iter().find(|p| &p.id == selected)
    }
}

/// Run the planning stages over the frontends' `roots`, enumerating at most
/// `max_candidates` whole-workload candidates. `targets[i]` is the accuracy
/// requirement of `roots[i]` (`None` imposes none). Only a Pass 1 failure is
/// an error; a candidate that does not compose or cannot be implemented is
/// listed with its reason, and a failed selection is reported in the run.
pub fn plan_stages<Id: Clone + std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    max_candidates: usize,
) -> Result<StagePipelineRun<Id>, PerComputationCandidateError> {
    let per_computation = generate_per_computation_candidates(roots)?;
    let combinations = per_computation
        .targets
        .iter()
        .map(|target| target.choices.candidates.len())
        .fold(1usize, usize::saturating_mul);
    let owners = target_owners(&per_computation);
    let mut logical = Vec::new();
    let mut physical = Vec::new();
    let mut pruned = Vec::new();
    let mut choice = vec![0; per_computation.targets.len()];
    for index in 0..combinations.min(max_candidates) {
        let id = format!("L{}", index + 1);
        let label = label(&per_computation, &owners, &choice);
        let roots = compose_logical_candidate(&per_computation, &choice)
            .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>());
        if let Ok(roots) = &roots {
            match optimize_physical_asap(&id, roots) {
                Ok(mut candidate) => {
                    candidate.id = format!("P{}", index + 1);
                    candidate.label = label.clone();
                    physical.push(candidate);
                }
                Err(error) => pruned.push(PrunedCandidate {
                    from_logical: id.clone(),
                    label: label.clone(),
                    reason: error.to_string(),
                }),
            }
        }
        logical.push(LogicalCandidate {
            id,
            label,
            choice: choice.clone(),
            roots,
        });
        // Mixed-radix increment: the last target varies fastest.
        for (digit, target) in choice.iter_mut().zip(&per_computation.targets).rev() {
            *digit += 1;
            if *digit < target.choices.candidates.len() {
                break;
            }
            *digit = 0;
        }
    }
    let selection = select_plan(&physical, targets, data, models);
    Ok(StagePipelineRun {
        per_computation,
        combinations,
        capped: combinations > max_candidates,
        logical,
        physical,
        pruned,
        selection,
    })
}

/// The first root that reaches each target (enumeration order).
fn target_owners<Id>(inventory: &PerComputationCandidates<Id>) -> Vec<usize> {
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

/// E.g. "Q1 exact · Q2 CMS+heap"; exact accumulators are listed in parentheses.
fn label<Id>(
    inventory: &PerComputationCandidates<Id>,
    owners: &[usize],
    choice: &[usize],
) -> String {
    (0..inventory.roots.len())
        .map(|query| {
            let mut sketches = Vec::new();
            let mut accumulators = Vec::new();
            for (target, (&owner, &index)) in
                inventory.targets.iter().zip(owners.iter().zip(choice))
            {
                if owner != query {
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
                    ComputationCandidate::Summary(algorithm) => sketches.push(match algorithm {
                        SketchAlgorithm::CmsWithHeap => "CMS+heap".to_string(),
                        SketchAlgorithm::CountSketchWithHeap => "CountSketch+heap".to_string(),
                        other => format!("{other:?}"),
                    }),
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
/// alone), through [`plan_stages`] with every entry's accuracy target and
/// at most [`MAX_ENUMERATED_CANDIDATES`] candidates. The output is the
/// selected physical candidate's roots.
#[derive(Debug, Default, Clone, Copy)]
pub struct StagePipeline;

impl OptimizationPass for StagePipeline {
    fn name(&self) -> &'static str {
        "stage-pipeline"
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
        let data = workload.data_workload().cloned().unwrap_or_default();
        let run = plan_stages(
            roots,
            &targets,
            &data,
            input.models,
            MAX_ENUMERATED_CANDIDATES,
        )?;
        let StagePipelineRun {
            physical,
            selection,
            ..
        } = run;
        let selection = selection?;
        let selected = physical
            .into_iter()
            .find(|p| p.id == selection.selected)
            .expect("the selected candidate is a physical candidate");
        let mut output = PlanOutput::new(Vec::new());
        for (entry_index, root) in selected.roots.iter().enumerate() {
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
