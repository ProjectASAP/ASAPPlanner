//! The four planner stages of #509 run end to end over one workload: the
//! one pipeline behind `stage_pipeline` and the planner-layering acceptance
//! tests, so both see the same candidates, ids and selection.

use asap_frontend_sql::SqlCatalog;
use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_logical_asap_optimization::workload_candidates::stage1_logical_asap_optimization;
pub use asap_logical_asap_optimization::workload_candidates::Pass2Candidate;
use asap_physical_asap_optimization::{physical_candidate_count, PhysicalCandidate};
use asap_plan_selection::pass::{
    run_stage1_to_stage3_from_pass1, run_stage2_and_stage3, Stage1ToStage3Search,
};
pub use asap_plan_selection::pass::{LogicalCandidate, PrunedCandidate};
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
    /// `P<n>` implements `L<n>`.
    pub stage2_physical_asap_optimization: Vec<PhysicalCandidate>,
    pub stage2_pruned: Vec<PrunedCandidate>,
    pub stage3_plan_selection: Result<Selection, SelectionError>,
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
    /// Stage 1 enumerated more candidates (Pass 1's and Pass 2's) than the
    /// caller's limit; enumeration stopped there and nothing was planned.
    TooManyCandidates { limit: usize },
    /// Stage 2 would build more physical candidates (one per
    /// materialization assignment of each Stage 1 candidate) than the
    /// caller's limit; they were counted, not built, and nothing was
    /// planned.
    TooManyPhysicalCandidates { limit: usize },
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
the exhaustive search refuses it"
            ),
            Self::TooManyPhysicalCandidates { limit } => write!(
                f,
                "workload has more than {limit} Stage 2 physical candidates (materialization \
options included); the exhaustive search refuses it"
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
/// library itself never caps. The same limit applies to Stage 2's physical
/// candidates, counted before they are built. Stage 2 reads the
/// recurrences, predictabilities and the data workload; plan selection the
/// accuracy targets, the recurrences, the latency bounds and the data
/// workload. The frontends' roots and every composed candidate are
/// validated.
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
    let deployment = models.deployment_capabilities;
    validate(&roots)?;
    let per_computation =
        generate_per_computation_candidates(roots.iter().cloned().enumerate().collect())
            .map_err(|e| PlanningStagesError::Pass1(e.to_string()))?
            .without_unsupported_summaries(deployment);
    // Count Stage 1 candidates as they are enumerated and stop as soon as
    // the count passes the limit; never truncate.
    let mut stage1 = Vec::new();
    for candidate in stage1_logical_asap_optimization(&per_computation, entries) {
        if max_combinations.is_some_and(|limit| stage1.len() == limit) {
            return Err(PlanningStagesError::TooManyCandidates {
                limit: stage1.len(),
            });
        }
        stage1.push(candidate);
    }
    if let Some(limit) = max_combinations {
        let mut physical = 0usize;
        for roots in stage1.iter().filter_map(|c| c.roots.as_ref().ok()) {
            physical += physical_candidate_count(roots, entries, data, deployment).unwrap_or(0);
            if physical > limit {
                return Err(PlanningStagesError::TooManyPhysicalCandidates { limit });
            }
        }
    }
    let run = run_stage2_and_stage3(per_computation, stage1, entries, data, models);
    for candidate in &run.stage1_logical_asap_optimization {
        if let Ok(roots) = &candidate.roots {
            validate(roots)?;
        }
    }
    Ok(PlanningStagesRun {
        stage0_query_language_frontends: roots,
        stage1_pass1_per_computation: run.stage1_pass1_per_computation,
        stage1_logical_asap_optimization: run.stage1_logical_asap_optimization,
        stage2_physical_asap_optimization: run.stage2_physical_asap_optimization,
        stage2_pruned: run.stage2_pruned,
        stage3_plan_selection: run.stage3_plan_selection,
    })
}

/// The default search over `roots` (query `i` is `roots[i]`, with
/// workload entry `entries[i]`): the one the planner's own pass and
/// `plan_installable` run ([`Stage1ToStage3Search::default`]: each query's
/// own candidates checked first, a whole-workload choice that picks a
/// discarded one not composed, independent components searched separately,
/// and, without raw data retained, the candidates that scan it at query
/// time skipped). It keeps only the best candidate so far, so the run holds
/// the selected Stage 1 and Stage 2 candidates alone (none when selection
/// fails), and Stage 3 records the first rejection of each cause and the
/// search's report (`Selection::search`). `models` should be the ones
/// `plan_installable` plans with (the executor's summary families and
/// compile check) to see what it selects. No limit applies.
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
    let per_computation =
        generate_per_computation_candidates(roots.iter().cloned().enumerate().collect())
            .map_err(|e| PlanningStagesError::Pass1(e.to_string()))?;
    let run = run_stage1_to_stage3_from_pass1(per_computation, entries, data, models, search);
    if let Some(Ok(roots)) = run.stage1_selected.as_ref().map(|c| &c.roots) {
        validate(roots)?;
    }
    Ok(PlanningStagesRun {
        stage0_query_language_frontends: roots,
        stage1_pass1_per_computation: run.stage1_pass1_per_computation,
        stage1_logical_asap_optimization: run.stage1_selected.into_iter().collect(),
        stage2_physical_asap_optimization: run.stage3_selected.into_iter().collect(),
        stage2_pruned: Vec::new(),
        stage3_plan_selection: run.stage3_plan_selection,
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
