//! The four planner stages of #509 run end to end over one workload: the
//! one pipeline behind `stage_pipeline` and the planner-layering acceptance
//! tests, so both see the same candidates, ids and selection.

use std::rc::Rc;

use asap_aware_mapping::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    PerComputationCandidates,
};
use asap_aware_mapping::physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_aware_mapping::plan_selection::{select_plan, Selection, SelectionError};
use asap_aware_mapping::PlanningModels;
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::post_asap::SketchAlgorithm;
use asap_types::workload::PlanningWorkload;

/// One whole-workload logical candidate: a choice of a per-computation
/// candidate for every Pass 1 target.
#[derive(Debug)]
pub struct LogicalCandidate {
    /// `L<n>`, numbered in enumeration order from 1.
    pub id: String,
    /// E.g. "Q1 exact · Q2 CMS+heap".
    pub label: String,
    /// One root per workload entry, or why composition cannot substitute
    /// this choice yet.
    pub roots: Result<Vec<QueryRoot>, String>,
}

/// A composed logical candidate physical ASAP-aware optimization cannot
/// implement.
#[derive(Debug)]
pub struct PrunedCandidate {
    pub from_logical: String,
    pub label: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct PlanningStagesRun {
    /// The language-specific frontends' roots, one per workload entry.
    pub frontends: Vec<QueryRoot>,
    /// Size of the Cartesian product of per-computation candidates;
    /// `logical` holds its first `max_candidates`.
    pub combinations: usize,
    pub logical: Vec<LogicalCandidate>,
    /// `P<n>` implements `L<n>`.
    pub physical: Vec<PhysicalCandidate>,
    pub pruned: Vec<PrunedCandidate>,
    pub selection: Result<Selection, SelectionError>,
}

impl PlanningStagesRun {
    pub fn capped(&self) -> bool {
        self.logical.len() < self.combinations
    }
}

/// Lower a PromQL workload, each series' identity as a column (the row
/// representation per-series state needs at runtime).
pub fn lower_promql_roots(workload: &PlanningWorkload) -> Result<Vec<QueryRoot>, String> {
    asap_frontend_promql::lower_promql_query_workload(workload, 0)
        .map_err(|e| format!("lowering: {e}"))?
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => with_promql_series_identity(&node)
                .map(QueryRoot::Operator)
                .map_err(|e| format!("series identity: {e}")),
            scalar => Ok(scalar),
        })
        .collect()
}

/// Logical ASAP-aware optimization (Pass 1 and composition; no Pass 2),
/// physical ASAP-aware optimization and plan selection over `roots`, the
/// lowered entries of `workload`. Logical candidates are every choice of a
/// per-computation candidate for every target, the last target varying
/// fastest, cut at `max_candidates` (`None`: all). Plan selection reads only
/// the accuracy targets and the data workload.
pub fn run_planning_stages(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
    max_candidates: Option<usize>,
) -> Result<PlanningStagesRun, String> {
    validate(&roots)?;
    let inventory =
        generate_per_computation_candidates(roots.iter().cloned().enumerate().collect())
            .map_err(|e| format!("Pass 1: {e}"))?;
    let combinations = inventory
        .targets
        .iter()
        .map(|target| target.choices.candidates.len())
        .product::<usize>();
    let owners = target_owners(&inventory);
    let mut logical = Vec::new();
    let mut physical = Vec::new();
    let mut pruned = Vec::new();
    let mut choice = vec![0; inventory.targets.len()];
    for index in 0..combinations.min(max_candidates.unwrap_or(usize::MAX)) {
        let (id, label) = (
            format!("L{}", index + 1),
            label(&inventory, &owners, &choice),
        );
        let composed = compose_logical_candidate(&inventory, &choice)
            .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>())
            .map_err(|e| e.to_string());
        if let Ok(roots) = &composed {
            validate(roots)?;
            match optimize_physical_asap(&id, roots) {
                Ok(mut candidate) => {
                    candidate.id = format!("P{}", index + 1);
                    candidate.label = label.clone();
                    physical.push(candidate);
                }
                Err(e) => pruned.push(PrunedCandidate {
                    from_logical: id.clone(),
                    label: label.clone(),
                    reason: e.to_string(),
                }),
            }
        }
        logical.push(LogicalCandidate {
            id,
            label,
            roots: composed,
        });
        // Mixed-radix increment: the last target varies fastest.
        for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
            *digit += 1;
            if *digit < target.choices.candidates.len() {
                break;
            }
            *digit = 0;
        }
    }
    let targets: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| Some(entry.requirements.accuracy.target()))
        .collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let selection = select_plan(&physical, &targets, &data, PlanningModels::builtin());
    Ok(PlanningStagesRun {
        frontends: roots,
        combinations,
        logical,
        physical,
        pruned,
        selection,
    })
}

fn validate(roots: &[QueryRoot]) -> Result<(), String> {
    roots
        .iter()
        .try_for_each(|root| root.validate_structure())
        .map_err(|e| format!("validation: {e}"))
}

/// The first query whose plan reaches each target (enumeration order).
fn target_owners(inventory: &PerComputationCandidates<usize>) -> Vec<usize> {
    inventory
        .targets
        .iter()
        .map(|target| {
            inventory
                .roots
                .iter()
                .position(|(_, root)| {
                    let operators = match root {
                        QueryRoot::Operator(node) => vec![node],
                        QueryRoot::Scalar(expr) => expr.operator_refs(),
                    };
                    operators.into_iter().any(|node| {
                        OperatorNode::reachable(node)
                            .iter()
                            .any(|n| Rc::ptr_eq(n, &target.target))
                    })
                })
                .expect("every target is reached from a root")
        })
        .collect()
}

/// E.g. "Q1 exact · Q2 CMS+heap"; exact accumulators are listed in parentheses.
fn label(
    inventory: &PerComputationCandidates<usize>,
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
