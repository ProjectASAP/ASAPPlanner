//! The planner stages run over one workload, from its query text:
//! [`run_search_over`] and [`run_planning_stages_over`] run the full search
//! (the integration tests and `planner_fuzz`), and [`run_stages_until`]
//! stops after a chosen stage (`asap_planner`).

use asap_frontend_sql::SqlCatalog;
pub use asap_planner::RunUntil;
use asap_planner::{run_planning_until, PlanSearchSettings, PlanningModels, PlanningRun};
use asap_stage1_pass1_per_computation_candidates::PerComputationCandidateError;
use asap_types::ir::schema::series_identity::with_promql_series_identity;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
use asap_types::stages::InvariantViolation;
use asap_types::workload::{DataWorkload, PlanningWorkload, QueryWorkloadEntry};
use serde_json::Value;

#[derive(Debug)]
pub struct PlanningStagesRun {
    /// The last stage run; `run` holds nothing of the stages after it.
    pub run_until: RunUntil,
    /// The query-language frontends' roots, one per workload entry.
    pub stage0_query_language_frontends: Vec<QueryRoot>,
    /// The stages' outputs: Stage 0's memo, Pass 1, Pass 2, Stage 2 and
    /// the selection.
    pub run: PlanningRun,
}

/// Lower a PromQL workload, each series' identity as a column (the row
/// representation per-series state needs at runtime).
pub fn lower_promql_roots(workload: &PlanningWorkload) -> Result<Vec<QueryRoot>, String> {
    with_series_identity(
        asap_frontend_promql::lower_promql_query_workload(workload)
            .map_err(|e| format!("lowering: {e}"))?,
    )
}

/// Lower a MetricsQL workload, each series' identity as a column, as
/// [`lower_promql_roots`] does for PromQL.
pub fn lower_metricsql_roots(workload: &PlanningWorkload) -> Result<Vec<QueryRoot>, String> {
    with_series_identity(
        asap_frontend_metricsql::lower_metricsql_query_workload(workload)
            .map_err(|e| format!("lowering: {e}"))?,
    )
}

/// `roots` (a PromQL-family frontend's) with each series' identity as a
/// column.
fn with_series_identity(roots: Vec<QueryRoot>) -> Result<Vec<QueryRoot>, String> {
    roots
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(asap_frontend_sql::lower_sql_workload(workload, catalog))
        .map_err(|(_, e)| format!("lowering: {e}"))
}

/// Why a planning run produced no result.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanningStagesError {
    /// Stage 0 (`memo_of_queries`) or Pass 1 failed: there are no
    /// candidates, only the frontends' roots.
    Pass1(String),
    /// The frontends' roots or a composed candidate failed validation.
    Validation(String),
    /// Stage 0's or Pass 1's memo breaks an invariant: a planner bug, not
    /// an unplannable query.
    Invariant(InvariantViolation),
}

impl std::fmt::Display for PlanningStagesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass1(error) => write!(f, "Pass 1: {error}"),
            Self::Validation(error) => write!(f, "validation: {error}"),
            Self::Invariant(violation) => write!(f, "{violation}"),
        }
    }
}

impl From<PlanningStagesError> for String {
    fn from(error: PlanningStagesError) -> Self {
        error.to_string()
    }
}

/// The planning stages over roots lowered from any mix of query
/// languages: query `i` is `roots[i]`, with workload entry `entries[i]`
/// (its accuracy target, latency bound, recurrence and predictability).
/// `models.deployment_capabilities` is what the executor can run and keep:
/// Pass 1 offers only the summaries it supports, and Stage 2 reads its
/// retention.
pub fn run_planning_stages_over(
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
        PlanSearchSettings {
            inspect: true,
            check_invariants: false,
        },
    )
}

/// The facade's planning with `settings`.
pub fn run_search_over(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    run_until(roots, entries, data, models, settings, RunUntil::Stage3)
}

/// The stages over `roots` up to and including `until`, as the facade runs
/// them (`run_planning_until`), each stage's output kept, the costly memo
/// invariants run when `check_invariants`
/// (`PlanSearchSettings::check_invariants`). A failure after Pass 1 is
/// reported in the run, as the facade does.
pub fn run_stages_until(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    until: RunUntil,
    check_invariants: bool,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    let settings = PlanSearchSettings {
        inspect: true,
        check_invariants,
    };
    run_until(roots, entries, data, models, settings, until)
}

fn run_until(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
    until: RunUntil,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    let run =
        run_planning_until(&roots, entries, data, models, settings, until).map_err(|error| {
            match error {
                PerComputationCandidateError::Structure(error) => {
                    PlanningStagesError::Validation(error.to_string())
                }
                error => match error.invariant_violation() {
                    Some(violation) => PlanningStagesError::Invariant(violation.clone()),
                    None => PlanningStagesError::Pass1(error.to_string()),
                },
            }
        })?;
    Ok(PlanningStagesRun {
        run_until: until,
        stage0_query_language_frontends: roots,
        run,
    })
}

/// The SQL catalog of the tables `tables` declare, each a `asap_planner
/// --sql-catalog-table` JSON object: `{"name": ..., "columns": [{"name": ..., "type":
/// "timestamp|utf8|string|float64|double|int64|bigint", "nullable": true}],
/// "time_index": 0}`, and the tables' names in declaration order.
pub fn sql_catalog(tables: &[String]) -> Result<(SqlCatalog, Vec<String>), String> {
    let mut catalog = SqlCatalog::new();
    let mut names = Vec::new();
    for raw in tables {
        let table: Value =
            serde_json::from_str(raw).map_err(|e| format!("--sql-catalog-table: {e}"))?;
        let name = table["name"]
            .as_str()
            .ok_or("--sql-catalog-table: name must be a string")?;
        if names.iter().any(|declared| declared == name) {
            return Err(format!(
                "--sql-catalog-table: table {name} is declared twice"
            ));
        }
        let columns = table["columns"]
            .as_array()
            .ok_or("--sql-catalog-table: columns must be an array")?
            .iter()
            .map(|column| {
                let column_name = column["name"]
                    .as_str()
                    .ok_or("--sql-catalog-table: column name must be a string")?;
                let dtype = match column["type"]
                    .as_str()
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("timestamp") => DataType::Timestamp,
                    Some("utf8" | "string") => DataType::Utf8,
                    Some("float64" | "double") => DataType::Float64,
                    Some("int64" | "bigint") => DataType::Int64,
                    other => {
                        return Err(format!(
                            "--sql-catalog-table: unsupported column type {other:?}"
                        ))
                    }
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
                        "--sql-catalog-table {name}: time_index must be the index of one of its {} columns",
                        columns.len()
                    ))
                }
            },
        };
        names.push(name.to_owned());
        catalog = catalog.with_table(name, schema);
    }
    Ok((catalog, names))
}
