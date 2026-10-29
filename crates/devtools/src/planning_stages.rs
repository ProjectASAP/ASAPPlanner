//! The four planner stages of #509 run end to end over one workload: the
//! one pipeline behind `stage_pipeline` and the planner-layering acceptance
//! tests, so both see the same candidates, ids and selection.

use asap_frontend_sql::SqlCatalog;
use asap_logical_asap_optimization::pass1::per_computation_candidates::PerComputationCandidates;
pub use asap_logical_asap_optimization::pass2::workload_candidates::Pass2Candidate;
use asap_physical_asap_optimization::PhysicalWorkloadPlan;
use asap_plan_selection::pass::{run_stage1_to_stage3_with, Stage1ToStage3Search};
pub use asap_plan_selection::pass::{LogicalCandidate, MemoSteps, PrunedCandidate};
use asap_plan_selection::PlanningModels;
use asap_plan_selection::{Selection, SelectionError};
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, PlanningWorkload, QueryWorkloadEntry};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug)]
pub struct PlanningStagesRun {
    /// The query-language frontends' roots, one per workload entry.
    pub stage0_query_language_frontends: Vec<QueryRoot>,
    /// Stage 1, Pass 1: the per-computation candidates the logical
    /// candidates' `choice` indexes (query `i` has id `i`).
    pub stage1_pass1_per_computation: PerComputationCandidates<usize>,
    pub stage1_logical_asap_optimization: Vec<LogicalCandidate>,
    /// Evaluated physical plans reference their archived logical source by ID.
    pub stage2_physical_asap_optimization: Vec<PhysicalWorkloadPlan>,
    pub stage2_pruned: Vec<PrunedCandidate>,
    pub stage3_plan_selection: Result<Selection, SelectionError>,
    /// The memo after each step and the selected plan's choice over it.
    pub memo_steps: Option<MemoSteps>,
}

/// Lower a PromQL workload, each series' identity as a column (the row
/// representation per-series state needs at runtime).
pub fn lower_promql_roots(workload: &PlanningWorkload) -> Result<Vec<QueryRoot>, String> {
    asap_frontend_promql::lower_promql_query_workload(workload)
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
                    entry.requirements.accuracy.clone(),
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
    /// The frontends' roots or a composed candidate failed validation.
    Validation(String),
}

impl std::fmt::Display for PlanningStagesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass1(error) => write!(f, "Pass 1: {error}"),
            Self::Validation(error) => write!(f, "validation: {error}"),
        }
    }
}

impl From<PlanningStagesError> for String {
    fn from(error: PlanningStagesError) -> Self {
        error.to_string()
    }
}

/// Traverse the candidate-connection catalog while archiving evaluated plans.
/// `max_combinations` bounds evaluated assignments, not a Cartesian product.
/// Exhaustive mode uses the same engine and honors its time and evaluation
/// budgets; consult the search report before claiming catalog exhaustion.
pub fn run_planning_stages(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
    max_combinations: Option<usize>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    run_planning_stages_over(
        roots,
        &entries,
        &data,
        max_combinations,
        PlanningModels::builtin(),
    )
}

/// [`run_planning_stages`] over roots lowered from any mix of query
/// languages: query `i` is `roots[i]`, with workload entry `entries[i]`
/// (its accuracy target, latency bound, recurrence and predictability).
/// Plan selection compares candidates under `models.plan_selection`'s cost
/// model and rejects those `models.executor_compile_check` does not compile.
/// `models.deployment_capabilities` is what the executor can run and keep:
/// Pass 1 offers only the summaries it supports, and Stages 2 and 3 read its
/// retention.
pub fn run_planning_stages_over(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    max_combinations: Option<usize>,
    models: PlanningModels<'_>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    run_search_over(
        roots,
        entries,
        data,
        models,
        Stage1ToStage3Search {
            max_evaluations: max_combinations,
            search: asap_plan_selection::SearchAlgorithm::CandidateConnections,
            ..Stage1ToStage3Search::exhaustive()
        },
    )
}

/// The same bounded candidate-connection search used by `plan_installable`.
/// The run retains the selected logical and physical plans, validation
/// diagnostics, and search metrics. Use the executor's summary capabilities
/// and compile check in `models` when comparing with installation behavior.
pub fn run_default_search_over(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    run_search_over(
        roots,
        entries,
        data,
        models,
        Stage1ToStage3Search::default(),
    )
}

/// [`run_default_search_over`] with the search `search` (e.g. the memo
/// over logical sub-expressions searched as a MILP, with a time limit).
pub fn run_search_over(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    search: Stage1ToStage3Search,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    validate(&roots)?;
    // Use the production entry point so Pass 1 generation is included in the
    // same planning budget and report rather than happening outside the timer.
    let run = run_stage1_to_stage3_with(
        roots.iter().cloned().enumerate().collect(),
        entries,
        data,
        models,
        search,
    )
    .map_err(|e| PlanningStagesError::Pass1(e.to_string()))?;
    if let Some(Ok(roots)) = run.stage1_selected.as_ref().map(|c| &c.roots) {
        validate(roots)?;
    }
    Ok(PlanningStagesRun {
        stage0_query_language_frontends: roots,
        stage1_pass1_per_computation: run.stage1_pass1_per_computation,
        stage1_logical_asap_optimization: if search.keep_every_candidate {
            run.stage1_logical_asap_optimization
        } else {
            run.stage1_selected.into_iter().collect()
        },
        stage2_physical_asap_optimization: if search.keep_every_candidate {
            run.stage2_physical_asap_optimization
        } else {
            run.stage3_selected.into_iter().collect()
        },
        stage2_pruned: run.stage2_pruned,
        stage3_plan_selection: run.stage3_plan_selection,
        memo_steps: run.memo_steps,
    })
}

fn validate(roots: &[QueryRoot]) -> Result<(), PlanningStagesError> {
    roots
        .iter()
        .try_for_each(|root| root.validate_structure())
        .map_err(|e| PlanningStagesError::Validation(e.to_string()))
}

/// The SQL catalog of the tables `tables` declare, each a `stage_pipeline
/// --table` JSON object: `{"name": ..., "columns": [{"name": ..., "type":
/// "timestamp|utf8|string|float64|double|int64|bigint", "nullable": true}],
/// "time_index": 0}`.
pub fn sql_catalog(tables: &[String]) -> Result<SqlCatalog, String> {
    let mut catalog = SqlCatalog::new();
    let mut names = HashSet::new();
    for raw in tables {
        let table: Value = serde_json::from_str(raw).map_err(|e| format!("--table: {e}"))?;
        let name = table["name"]
            .as_str()
            .ok_or("--table: name must be a string")?;
        if !names.insert(name.to_owned()) {
            return Err(format!("--table: table {name} is declared twice"));
        }
        let columns = table["columns"]
            .as_array()
            .ok_or("--table: columns must be an array")?
            .iter()
            .map(|column| {
                let column_name = column["name"]
                    .as_str()
                    .ok_or("--table: column name must be a string")?;
                let dtype = match column["type"]
                    .as_str()
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("timestamp") => DataType::Timestamp,
                    Some("utf8" | "string") => DataType::Utf8,
                    Some("float64" | "double") => DataType::Float64,
                    Some("int64" | "bigint") => DataType::Int64,
                    other => return Err(format!("--table: unsupported column type {other:?}")),
                };
                let nullable = column["nullable"].as_bool().unwrap_or(true);
                Ok(Field::plain(column_name, dtype, nullable))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let schema = match table.get("time_index") {
            None | Some(Value::Null) => Schema::new(columns),
            Some(index) => match index.as_u64().map(|index| index as usize) {
                Some(index) if index < columns.len() => {
                    Schema::with_time_index(columns, index, vec![])
                }
                _ => {
                    return Err(format!(
                        "--table {name}: time_index must be the index of one of its {} columns",
                        columns.len()
                    ))
                }
            },
        };
        catalog = catalog.with_table(name, schema);
    }
    Ok(catalog)
}
