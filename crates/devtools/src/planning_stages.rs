//! The four planner stages of #509 run end to end over one workload: the
//! one pipeline behind `stage_pipeline` and the planner-layering acceptance
//! tests, so both see the same candidates, ids and selection.

use asap_aware_mapping::pass::run_stage1_to_stage3;
pub use asap_aware_mapping::pass::{LogicalCandidate, PrunedCandidate};
use asap_aware_mapping::stage2_physical_asap_optimization::PhysicalCandidate;
use asap_aware_mapping::stage3_plan_selection::{Selection, SelectionError};
use asap_aware_mapping::PlanningModels;
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::QueryRoot;
use asap_types::workload::PlanningWorkload;

#[derive(Debug)]
pub struct PlanningStagesRun {
    /// The query-language frontends' roots, one per workload entry.
    pub stage0_query_language_frontends: Vec<QueryRoot>,
    pub stage1_logical_asap_optimization: Vec<LogicalCandidate>,
    /// `P<n>` implements `L<n>`.
    pub stage2_physical_asap_optimization: Vec<PhysicalCandidate>,
    pub stage2_pruned: Vec<PrunedCandidate>,
    pub stage3_plan_selection: Result<Selection, SelectionError>,
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
/// lowered entries of `workload`: the library's planner pipeline
/// ([`run_stage1_to_stage3`]) with the built-in models. Logical candidates are every
/// choice of a per-computation candidate for every target, the last target
/// varying fastest. Every candidate is enumerated and priced, so the
/// selection is the cheapest of all; a caller that shows fewer cuts only its
/// display. Plan selection reads only the accuracy targets and the data
/// workload. The frontends' roots and every composed candidate are
/// validated.
pub fn run_planning_stages(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
) -> Result<PlanningStagesRun, String> {
    validate(&roots)?;
    let targets: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| Some(entry.requirements.accuracy.target()))
        .collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let run = run_stage1_to_stage3(
        roots.iter().cloned().enumerate().collect(),
        &targets,
        &data,
        PlanningModels::builtin(),
    )
    .map_err(|e| format!("Pass 1: {e}"))?;
    for candidate in &run.stage1_logical_asap_optimization {
        if let Ok(roots) = &candidate.roots {
            validate(roots)?;
        }
    }
    Ok(PlanningStagesRun {
        stage0_query_language_frontends: roots,
        stage1_logical_asap_optimization: run.stage1_logical_asap_optimization,
        stage2_physical_asap_optimization: run.stage2_physical_asap_optimization,
        stage2_pruned: run.stage2_pruned,
        stage3_plan_selection: run.stage3_plan_selection,
    })
}

fn validate(roots: &[QueryRoot]) -> Result<(), String> {
    roots
        .iter()
        .try_for_each(|root| root.validate_structure())
        .map_err(|e| format!("validation: {e}"))
}
