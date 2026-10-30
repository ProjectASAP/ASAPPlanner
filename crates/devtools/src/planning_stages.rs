//! The four planner stages of #509 run end to end over one workload: the
//! one pipeline behind `stage_pipeline` and the planner-layering acceptance
//! tests, so both see the same candidates, ids and selection.

use asap_frontend_sql::SqlCatalog;
use asap_physical_asap_optimization::PhysicalWorkloadPlan;
use asap_planner::{run_planning, PlanSearchSettings, PlanningModels, PlanningRun};
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, PlanningWorkload, QueryWorkloadEntry};
use serde_json::Value;
use std::collections::HashSet;

use crate::memo_pricing::{reference_enumeration, ReferenceEnumeration};

/// The memo choices `--exhaustive` prices when no limit is given.
pub const DEFAULT_REFERENCE_CHOICES: usize = 10_000;

#[derive(Debug)]
pub struct PlanningStagesRun {
    /// The query-language frontends' roots, one per workload entry.
    pub stage0_query_language_frontends: Vec<QueryRoot>,
    /// The facade's run: Pass 1, the pane-width rounds and the selection.
    pub run: PlanningRun,
    /// `--exhaustive`: the selected round's memo choices, each implemented
    /// by Stage 2 and all priced by the reference pricer.
    pub reference: Option<ReferenceEnumeration>,
}

/// A logical candidate the document shows: its id and composed roots, or
/// why they do not compose.
#[derive(Debug, Clone)]
pub struct ShownLogicalCandidate {
    pub id: String,
    pub roots: Result<Vec<QueryRoot>, String>,
}

impl PlanningStagesRun {
    /// The logical candidates the document shows: the selected plan (`L1`;
    /// its roots as Stage 2 built them, with Stage 3's weight proofs), then
    /// under `--exhaustive` the reference enumeration's choices (`E<n>`).
    pub fn logical_candidates(&self) -> Vec<ShownLogicalCandidate> {
        let selected = self
            .run
            .selected_plan
            .iter()
            .map(|roots| ShownLogicalCandidate {
                id: "L1".into(),
                roots: Ok(roots.clone()),
            });
        let enumerated = self.reference.iter().flat_map(|reference| {
            reference
                .choices
                .iter()
                .map(|choice| ShownLogicalCandidate {
                    id: choice.id.clone(),
                    roots: choice.roots.clone(),
                })
        });
        selected.chain(enumerated).collect()
    }

    /// The physical candidates the document shows: the selected plan as
    /// Stage 2 built it (`P1`, from `L1`), then under `--exhaustive` every
    /// plan the reference pricer priced (`E<n>-<k>`).
    pub fn physical_candidates(&self) -> Vec<PhysicalWorkloadPlan> {
        let selected =
            self.run
                .selected_physical_plan
                .iter()
                .map(|realized| PhysicalWorkloadPlan {
                    id: "P1".into(),
                    from_logical: "L1".into(),
                    ..realized.plan.clone()
                });
        let enumerated = self
            .reference
            .iter()
            .flat_map(|reference| reference.physical.iter().cloned());
        selected.chain(enumerated).collect()
    }
}

/// Lower a PromQL workload, each series' identity as a column (the row
/// representation per-series state needs at runtime).
pub fn lower_promql_roots(workload: &PlanningWorkload) -> Result<Vec<QueryRoot>, String> {
    asap_frontend_promql::lower_promql_query_workload(
        workload,
        &asap_frontend_promql::HistogramCatalog::default(),
    )
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

/// The planning stages over `workload`, with the reference enumeration of
/// at most `max_combinations` memo choices (default
/// [`DEFAULT_REFERENCE_CHOICES`]).
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
/// `models.deployment_capabilities` is what the executor can run and keep:
/// Pass 1 offers only the summaries it supports, and Stage 2 reads its
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
        PlanSearchSettings::default(),
        Some(max_combinations.unwrap_or(DEFAULT_REFERENCE_CHOICES)),
    )
}

/// The facade's planning, as `plan_installable` runs it, without a
/// reference enumeration. Use the executor's summary capabilities in
/// `models` when comparing with installation behavior.
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
        PlanSearchSettings::default(),
        None,
    )
}

/// The facade's planning with `settings`, and with `reference_choices`
/// the reference enumeration of that many memo choices.
pub fn run_search_over(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
    reference_choices: Option<usize>,
) -> Result<PlanningStagesRun, PlanningStagesError> {
    validate(&roots)?;
    let run = run_planning(roots.clone(), entries, data, models, settings)
        .map_err(|e| PlanningStagesError::Pass1(e.to_string()))?;
    if let Some(roots) = &run.selected_plan {
        validate(roots)?;
    }
    let reference = reference_choices
        .and_then(|max| reference_enumeration(&run, &roots, entries, data, models, max));
    Ok(PlanningStagesRun {
        stage0_query_language_frontends: roots,
        run,
        reference,
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
