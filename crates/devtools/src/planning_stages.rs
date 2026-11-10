//! The four planner stages of #509 run over one workload: the facade's
//! search for the planner-layering acceptance tests, and
//! [`run_stages_until`] for `asap_planner`.

use asap_frontend_sql::SqlCatalog;
use asap_planner::{
    run_planning, PlanSearchSettings, PlanningModels, PlanningReport, PlanningRun, SelectionError,
};
use asap_stage0_memo::memo_of_queries;
use asap_stage1_pass1_per_computation_candidates::{pass1_memo, Pass1Error};
use asap_stage1_pass2_reuse_candidates::generate_logical_asap_memo_candidates;
use asap_stage2_physical_optimization::generate_physical_asap_memo_candidates;
use asap_stage3_plan_selection::select_plan;
use asap_types::ir::schema::series_identity::with_promql_series_identity;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
use asap_types::stages::Memo;
use asap_types::workload::{DataWorkload, PlanningWorkload, QueryWorkloadEntry};
use serde_json::Value;

/// The last stage a run runs (`asap_planner --run-until`); the run's
/// fields for later stages are left empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunUntil {
    Stage0,
    Pass1,
    Pass2,
    Stage2,
    Stage3,
}

impl RunUntil {
    const ALL: [Self; 5] = [
        Self::Stage0,
        Self::Pass1,
        Self::Pass2,
        Self::Stage2,
        Self::Stage3,
    ];

    /// The `--run-until` value, also written as the JSON `run_until`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Stage0 => "stage0",
            Self::Pass1 => "stage1_pass1",
            Self::Pass2 => "stage1_pass2",
            Self::Stage2 => "stage2",
            Self::Stage3 => "stage3",
        }
    }
}

impl std::str::FromStr for RunUntil {
    type Err = String;
    fn from_str(stage: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|until| until.name() == stage)
            .ok_or_else(|| format!("unknown stage {stage}"))
    }
}

#[derive(Debug)]
pub struct PlanningStagesRun {
    /// The last stage run; `run` holds nothing of the stages after it.
    pub run_until: RunUntil,
    /// The query-language frontends' roots, one per workload entry.
    pub stage0_query_language_frontends: Vec<QueryRoot>,
    /// Stage 0's memo groups (the roots hash-consed), without candidates;
    /// built by `run_stages_until` only (`asap_planner` prints them).
    pub stage0_memo_groups: Option<Memo>,
    /// The stages' outputs: Pass 1, Pass 2, Stage 2 and the selection.
    pub run: PlanningRun,
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

/// The planning stages over `workload`.
pub fn run_planning_stages(
    workload: &PlanningWorkload,
    roots: Vec<QueryRoot>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    run_planning_stages_over(roots, &entries, &data, PlanningModels::builtin())
}

/// [`run_planning_stages`] over roots lowered from any mix of query
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
        PlanSearchSettings { inspect: true },
    )
}

/// The facade's planning, as `plan_installable` runs it. Use the executor's summary capabilities in
/// `models` when comparing with installation behavior.
pub fn run_default_search_over(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    run_search_over(roots, entries, data, models, PlanSearchSettings::default())
}

/// The facade's planning with `settings`.
pub fn run_search_over(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    validate(&roots)?;
    let run = run_planning(roots.clone(), entries, data, models, settings)
        .map_err(|e| PlanningStagesError::Pass1(e.to_string()))?;
    Ok(PlanningStagesRun {
        run_until: RunUntil::Stage3,
        stage0_memo_groups: None,
        stage0_query_language_frontends: roots,
        run,
    })
}

/// The stages over `roots` up to and including `until`, as the facade runs
/// them. A stage's failure is reported in the run, as the facade does.
// ponytail: copies run_planning's stage order, which has no stop point;
// #1169 moves the stop point into run_planning.
pub fn run_stages_until(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    until: RunUntil,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    validate(&roots)?;
    // As `run_planning`: each query with its accuracy target, or none when
    // accuracy is not enforced.
    let accuracy_targets: Vec<_> = entries
        .iter()
        .map(|entry| {
            (!models.ignore_accuracy_guarantees).then(|| entry.requirements.accuracy.clone())
        })
        .collect();
    let stage0_dag = memo_of_queries(&roots, &accuracy_targets, data)
        .map_err(|e| PlanningStagesError::Validation(e.to_string()))?;
    let mut stages = PlanningStagesRun {
        run_until: until,
        stage0_memo_groups: Some(stage0_dag.clone()),
        stage0_query_language_frontends: roots.clone(),
        run: PlanningRun {
            pass1_memo: None,
            logical_asap_candidates: None,
            physical_asap_candidates: None,
            selection: Err(SelectionError::NotRun),
            report: PlanningReport::default(),
        },
    };
    if until == RunUntil::Stage0 {
        return Ok(stages);
    }
    let run = &mut stages.run;
    let pass1 = pass1_memo(
        stage0_dag,
        models
            .deployment_capabilities
            .supported_summaries
            .as_deref(),
    );
    // As `run_planning`: a query Pass 1 cannot plan is reported before
    // contradictory deployment capabilities.
    if let Err(Pass1Error::PerComputation(error)) = &pass1 {
        return Err(PlanningStagesError::Pass1(error.to_string()));
    }
    if let Err(error) = models.deployment_capabilities.validate(data) {
        run.selection = Err(SelectionError::InvalidDeploymentCapabilities(error));
        return Ok(stages);
    }
    let pass1 = match pass1 {
        Ok(pass1) => pass1,
        Err(error) => {
            run.selection = Err(SelectionError::LogicalASAPCandidates(error.to_string()));
            return Ok(stages);
        }
    };
    run.pass1_memo = Some(pass1.clone());
    if until == RunUntil::Pass1 {
        return Ok(stages);
    }
    let logical = match generate_logical_asap_memo_candidates(pass1, entries, models.evidence) {
        Ok(logical) => logical,
        Err(error) => {
            run.selection = Err(SelectionError::LogicalASAPCandidates(error.to_string()));
            return Ok(stages);
        }
    };
    let run = &mut stages.run;
    run.logical_asap_candidates = Some(logical);
    if until == RunUntil::Pass2 {
        return Ok(stages);
    }
    let logical = run.logical_asap_candidates.as_ref().expect("set above");
    let physical =
        generate_physical_asap_memo_candidates(logical, data, models.deployment_capabilities);
    if until == RunUntil::Stage2 {
        run.physical_asap_candidates = Some(physical);
        return Ok(stages);
    }
    run.selection = select_plan(&physical, entries, data, models.cost_inputs)
        .map_err(SelectionError::Selection);
    run.physical_asap_candidates = Some(physical);
    Ok(stages)
}

fn validate(roots: &[QueryRoot]) -> Result<(), PlanningStagesError> {
    roots
        .iter()
        .try_for_each(|root| root.validate_structure())
        .map_err(|e| PlanningStagesError::Validation(e.to_string()))
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
