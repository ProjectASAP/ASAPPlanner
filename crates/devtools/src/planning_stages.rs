//! The four planner stages of #509 run end to end over one workload: the
//! one pipeline behind `stage_pipeline` and the planner-layering acceptance
//! tests, so both see the same candidates, ids and selection.

use asap_aware_mapping::pass::{run_stage2_and_stage3, stage1_logical_asap_optimization};
pub use asap_aware_mapping::pass::{LogicalCandidate, PrunedCandidate};
use asap_aware_mapping::per_computation_candidates::generate_per_computation_candidates;
use asap_aware_mapping::stage2_physical_asap_optimization::PhysicalCandidate;
use asap_aware_mapping::stage3_plan_selection::{Selection, SelectionError};
use asap_aware_mapping::PlanningModels;
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::QueryRoot;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{DataWorkload, PlanningWorkload, QueryRecurrence};

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

/// Lower a SQL workload over the tables of `catalog`: every entry, batch
/// and repeating, in workload order, each with its own accuracy target.
pub fn lower_sql_roots(
    workload: &PlanningWorkload,
    catalog: &asap_frontend_sql::SqlCatalog,
) -> Result<Vec<QueryRoot>, String> {
    let dialect = match &workload.query_workload.language {
        asap_types::workload::QueryLanguage::SQL(dialect) => dialect.clone(),
        _ => asap_types::workload::SqlDialect::DataFusionSQL,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| e.to_string())?;
    workload
        .query_workload
        .entries()
        .map(|entry| {
            runtime
                .block_on(asap_frontend_sql::lower_sql_dialect(
                    &entry.query.0,
                    catalog,
                    dialect.clone(),
                    entry.requirements.accuracy.requested(),
                ))
                .map(QueryRoot::Operator)
                .map_err(|e| format!("lowering: {e}"))
        })
        .collect()
}

/// Why [`run_planning_stages`] produced no run.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanningStagesError {
    /// Pass 1 failed: there are no candidates, only the frontends' roots.
    Pass1(String),
    /// Stage 1 enumerated more candidates (Pass 1's and Pass 2's) than the
    /// caller's limit; enumeration stopped there and nothing was planned.
    TooManyCandidates { limit: usize },
    /// The frontends' roots or a composed candidate failed validation.
    Validation(String),
}

impl std::fmt::Display for PlanningStagesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass1(error) => write!(f, "Pass 1: {error}"),
            Self::TooManyCandidates { limit } => write!(
                f,
                "workload has more than {limit} Stage 1 candidates, Pass 2's included; \
planning it needs early pruning (#673)"
            ),
            Self::Validation(error) => write!(f, "validation: {error}"),
        }
    }
}

impl From<PlanningStagesError> for String {
    fn from(error: PlanningStagesError) -> Self {
        error.to_string()
    }
}

/// Logical ASAP-aware optimization (Pass 1, composition and Pass 2's
/// identical-expression rule),
/// physical ASAP-aware optimization and plan selection over `roots`, the
/// lowered entries of `workload`: the library's planner pipeline
/// ([`stage1_logical_asap_optimization`], [`run_stage2_and_stage3`]) with
/// the built-in models. Logical candidates are every choice of a
/// per-computation candidate for every target, the last target varying
/// fastest, each followed by its Pass 2 candidates. Every candidate is
/// enumerated and priced, so the selection is the cheapest of all; a caller
/// that shows fewer cuts only its display. With `max_combinations`, Stage 1
/// candidates (Pass 2's included) are counted as they are enumerated, and a
/// workload with more is refused as soon as the count passes the limit; the
/// library itself never caps. Plan selection reads the accuracy targets,
/// the recurrences and the data workload. The frontends' roots and every composed candidate are
/// validated.
pub fn run_planning_stages(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
    max_combinations: Option<usize>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    let targets: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| entry.requirements.accuracy.requested())
        .collect();
    let recurrence: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| entry.recurrence)
        .collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    run_planning_stages_over(roots, &targets, &recurrence, &data, max_combinations)
}

/// [`run_planning_stages`] over roots lowered from any mix of query
/// languages: query `i` is `roots[i]` with accuracy target `targets[i]`,
/// evaluated as `recurrence[i]` says.
pub fn run_planning_stages_over(
    roots: Vec<QueryRoot>,
    targets: &[AccuracyTarget],
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    max_combinations: Option<usize>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    validate(&roots)?;
    let targets: Vec<_> = targets.iter().cloned().map(Some).collect();
    let per_computation =
        generate_per_computation_candidates(roots.iter().cloned().enumerate().collect())
            .map_err(|e| PlanningStagesError::Pass1(e.to_string()))?;
    // Count Stage 1 candidates as they are enumerated and stop as soon as
    // the count passes the limit; never truncate.
    let mut stage1 = Vec::new();
    for candidate in stage1_logical_asap_optimization(&per_computation, recurrence) {
        if max_combinations.is_some_and(|limit| stage1.len() == limit) {
            return Err(PlanningStagesError::TooManyCandidates {
                limit: stage1.len(),
            });
        }
        stage1.push(candidate);
    }
    let run = run_stage2_and_stage3(
        per_computation,
        stage1,
        &targets,
        recurrence,
        data,
        PlanningModels::builtin(),
    );
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

fn validate(roots: &[QueryRoot]) -> Result<(), PlanningStagesError> {
    roots
        .iter()
        .try_for_each(|root| root.validate_structure())
        .map_err(|e| PlanningStagesError::Validation(e.to_string()))
}
