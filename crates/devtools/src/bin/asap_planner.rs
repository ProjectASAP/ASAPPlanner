// cargo run -p asap-devtools --bin asap_planner -- \
//     --workload dashboard.json --run-until stage3
// cargo run -p asap-devtools --bin asap_planner -- \
//     --workload promql.json --workload sql.json --sql-catalog-table '<json>' \
//     --run-until stage1_pass2 --format json --out run.json
//
// Runs the planner stages over a workload up to and including
// `--run-until` (`stage0`, `stage1_pass1`, `stage1_pass2`, `stage2` or
// `stage3`) and writes each stage's output, as text (`--format text`, the default;
// `asap_devtools::planning_stages_text`) or as an `asap-stage-pipeline/v1`
// document for the Stage Viewer (`--format json`), to `--out` or standard
// output.
//
// The stages run as `run_planning` runs them
// (`asap_devtools::planning_stages::run_stages_until`), so the selected plan
// is the one `run_planning` selects.
//
// Inputs, one flag per field of the planner's `UserInput`:
//   - `--workload <file>` (repeatable): a `PlanningWorkload` JSON file. Each
//     file keeps its one language and is lowered by its own frontend; the
//     queries are numbered `q1..qN` across the files in the order given,
//     and the data workload is the union of the files' sources. A source
//     several files declare must have the same facts in each, and the files
//     must declare the same kind of arrival: ingesting (or mixed) arrivals
//     add their `rows_per_second`. Plan selection shares a workload's
//     `rows_per_second` out over all its sources, so when the data arrives,
//     every source of several files must declare its own rate
//     (`series_count` and `sample_interval`). A disagreement or a source
//     without its own rate is refused.
//   - `--sql-catalog-table '<json>'` (repeatable): a table the SQL queries
//     read: `{"name": ..., "columns": [{"name": ..., "type":
//     "timestamp|utf8|string|float64|double|int64|bigint", "nullable":
//     true}], "time_index": 0}` (`nullable` defaults to true; without
//     `time_index` the table has no time column).
//   - `--cost-inputs <file>`: Stage 3's `CostInputs` (`cost_model`,
//     `resource_speed`, `non_repeating_query_horizon_seconds`,
//     `measured_operation_costs`, `frozen_calibration_factor`; an omitted
//     field takes its default).
//   - `--deployment-capabilities <file>`: the `DeploymentCapabilities`
//     (`supported_summaries`, `raw_data_retained`,
//     `query_time_results_retained`; an omitted field takes its default).
//     Without `supported_summaries`, the executor's summary families are
//     supported, as `plan_installable` defaults it.
//   - `--ignore-accuracy-guarantees`: Stage 3 accepts every query output
//     regardless of its accuracy guarantee
//     (`PlanningModels::ignore_accuracy_guarantees`), comparing candidates
//     on cost alone. By default it enforces them, as `plan_installable`
//     does.
//
// The document (`--format json`):
//   - workload: the queries (`id`, `language`, `text`, `requirements`,
//     `recurrence`, `predictability`) and the `data_workload` planned with;
//   - stage0_query_language_frontends: the frontends' DAG, one root per query;
//   - stage1_logical_asap_optimization (from `stage1_pass1`): `pruned_candidates`,
//     the ways to compute a measure Pass 1 pruned (each on its aggregate's
//     memo `group`, with `candidate` and `reason`); `error` when Pass 1
//     failed;
//   - stage2_physical_asap_optimization (from `stage2`): `pruned_candidates`,
//     the physical candidates Stage 2 pruned (each with its memo `group`,
//     `operator_node`, `operator`, `implementation_root`, `timing`,
//     `lifetime` and `reason`);
//   - stage3_plan_selection (from `stage3`): the `plan` Stage 3 built (a
//     `PhysicalASAPDAG`) and its `Selection`: `accepted_accuracy_per_query`,
//     `pruned_candidates`, `milp_variables`, `milp_constraints`,
//     `milp_construction_ms` and `milp_solve_ms`; `error` when no plan was
//     selected.
//     `materialized_views` are the plan's materialized views as
//     `plan_installable` derives them, each with the node of `P1` it is
//     installed from (`stage2_node_id`), its position among that node's
//     installed nodes (`installed_position`) and their number
//     (`installed_count`); `materialized_views_error` when the plan is not
//     installable. `warnings`: a PromQL query applying `rate`, `increase`
//     or `irate` to a declared gauge or combining a declared summary's
//     quantile series across series (`metric_type_warnings`), and a peak memory over 10^12 bytes
//     when the cost model declares no memory capacity;
//   - memo: `nodes`, one node table for the section (each distinct node
//     once, children as indices before their parents), and `steps`, the
//     memo after each step run (`stage0`: the frontends' DAG hash-consed,
//     groups without candidates; `pass1`; `pass2_local_window_composition`:
//     local window composition's tumbling-window candidates; `pass2`: Pass
//     2's output, the memo Stage 2 implements). Each
//     step lists its groups (`id`, `label`, `children`, `parents`,
//     `queries`, `roots_of`, `node`) and their candidates (`index`,
//     `implements`, `children`, `origin`, `accuracy_budget_parts`, `added`
//     (not in the group at the step before), `label`, `node`,
//     `group_of_node`). With `stage3`, `selected_choice` maps each group the
//     plan uses to its candidate's index, or is null with
//     `selected_choice_unavailable`;
//   - `search`, `accuracy_guarantees` (`"enforced"` or `"ignored"`),
//     `deployment_capabilities` (as planned with) and `run_until`.
// A section of a stage after `--run-until` is null.

use std::collections::btree_map::Entry;
use std::collections::BTreeSet;

use asap_cost::CostInputs;
use asap_devtools::candidate_labels::node_label;
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_stages_until, sql_catalog, PlanningStagesError,
    PlanningStagesRun, RunUntil,
};
use asap_devtools::planning_stages_text::planning_stages_text;
use asap_devtools::SqlCatalog;
use asap_frontend_promql::{metric_type_warnings, promql_metric_names};
use asap_planner::{installable_plan_of, PlanningModels, SelectionError};
use asap_stage3_plan_selection::{large_peak_memory_warning, AcceptedAccuracy, Selection};
use asap_types::accuracy::{default_metric_for_root, AccuracyTarget};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::flat::{flatten, FlatDag, FlatNode};
use asap_types::ir::QueryRoot;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::stages::PhysicalASAPMemoCandidates;
use asap_types::stages::{queries_rooted_at, MemoGroup, MemoGroupId};
use asap_types::stages::{Memo, OriginOfCandidateForMemoGroup};
use asap_types::workload::{
    DataArrival, DataWorkload, PlanningWorkload, QueryLanguage, QueryRecurrence,
    QueryWorkloadEntry, RepeatedDemand,
};
use serde_json::{json, Value};

const USAGE: &str = "usage: asap_planner (--workload <file>)... \
--run-until stage0|stage1_pass1|stage1_pass2|stage2|stage3 [--format text|json] [--out <file>] \
[--sql-catalog-table <json>]... [--cost-inputs <file>] [--deployment-capabilities <file>] \
[--ignore-accuracy-guarantees]";

fn main() {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => {}
        Err(Failure::Usage(message)) => {
            eprintln!("asap_planner: {message}\n{USAGE}");
            std::process::exit(2);
        }
        Err(Failure::Other(message)) => {
            eprintln!("asap_planner: {message}");
            std::process::exit(1);
        }
    }
}

/// Why a run failed: bad arguments (status 2, with the usage line) or
/// anything else (status 1).
enum Failure {
    Usage(String),
    Other(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

/// `path`'s contents as `T`, errors naming `flag` and `path`.
fn read_json<T: serde::de::DeserializeOwned>(flag: &str, path: &str) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{flag} {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{flag} {path}: {e}"))
}

fn run(args: Vec<String>) -> Result<(), Failure> {
    let mut workload_paths: Vec<String> = Vec::new();
    let mut tables = Vec::new();
    let mut run_until = None;
    let mut json_format = false;
    let mut out = None;
    let mut cost_inputs = CostInputs::default();
    let mut deployment_capabilities = DeploymentCapabilities::default();
    let mut ignore_accuracy_guarantees = false;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| Failure::Usage(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--workload" => workload_paths.push(value()?),
            "--sql-catalog-table" => tables.push(value()?),
            "--run-until" => {
                run_until = Some(value()?.parse::<RunUntil>().map_err(Failure::Usage)?)
            }
            "--format" => {
                json_format = match value()?.as_str() {
                    "text" => false,
                    "json" => true,
                    other => return Err(Failure::Usage(format!("unknown format {other}"))),
                }
            }
            "--out" => out = Some(value()?),
            "--cost-inputs" => cost_inputs = read_json(&flag, &value()?)?,
            "--deployment-capabilities" => deployment_capabilities = read_json(&flag, &value()?)?,
            "--ignore-accuracy-guarantees" => ignore_accuracy_guarantees = true,
            other => return Err(Failure::Usage(format!("unknown argument {other}"))),
        }
    }
    let run_until = run_until.ok_or(Failure::Usage("--run-until is required".into()))?;
    if workload_paths.is_empty() {
        return Err(Failure::Usage("--workload is required".into()));
    }
    let mut workloads = Vec::new();
    for path in &workload_paths {
        let workload: PlanningWorkload = read_json("--workload", path)?;
        workload
            .validate()
            .map_err(|e| format!("--workload {path}: {e}"))?;
        workloads.push((path.as_str(), workload));
    }
    let (catalog, table_names) = sql_catalog(&tables)?;
    let (described, roots, entries, data) = lowered_together(&workloads, &catalog, &table_names)?;
    // Plan with the executor's summary families when the deployment declares none.
    if deployment_capabilities.supported_summaries.is_none() {
        deployment_capabilities.supported_summaries =
            Some(asap_executor::supported_summary_families());
    }
    let models = PlanningModels::builtin()
        .with_cost_inputs(&cost_inputs)
        .with_deployment_capabilities(&deployment_capabilities)
        .with_ignore_accuracy_guarantees(ignore_accuracy_guarantees);
    let result = run_stages_until(roots.clone(), &entries, &data, models, run_until);
    // A stage that failed: the output is still written, then the run exits 1.
    let failure = match &result {
        Err(PlanningStagesError::Pass1(error)) => Some(format!("Pass 1: {error}")),
        Ok(run) => match &run.run.selection {
            Err(SelectionError::NotRun) | Ok(_) => None,
            Err(error) => Some(error.to_string()),
        },
        Err(_) => None,
    };
    // A rate, increase or irate over a declared gauge (the query still plans).
    let mut warnings = metric_type_warnings_of(&described, &data)?;
    // A selected plan's very large peak memory, when the deployment
    // declares no memory capacity to check it against.
    warnings.extend(
        result
            .as_ref()
            .ok()
            .and_then(|run| run.run.selection.as_ref().ok())
            .map(|(_plan, _selection)| -> u64 {
                todo!("#1207 PR 6: the selected plan's peak memory, from plan_cost(&plan)")
            })
            .and_then(|peak_memory_bytes| {
                large_peak_memory_warning(peak_memory_bytes, &cost_inputs.cost_model)
            }),
    );
    let text = if json_format {
        let mut document = document_of(described, &roots, &entries, result, run_until)?;
        // Stage 3 is null when Pass 1 rejected a query.
        if let Some(stage3) = document["stage3_plan_selection"].as_object_mut() {
            if !warnings.is_empty() {
                stage3.insert("warnings".into(), json!(warnings));
            }
        }
        document["workload"]["data_workload"] =
            serde_json::to_value(&data).map_err(|e| e.to_string())?;
        document["search"] = json!("candidate-connections");
        document["accuracy_guarantees"] = json!(if ignore_accuracy_guarantees {
            "ignored"
        } else {
            "enforced"
        });
        // Read-only: the deployment inputs the run planned with.
        document["deployment_capabilities"] =
            serde_json::to_value(&deployment_capabilities).map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n"
    } else {
        let mut text = match result {
            Ok(run) => planning_stages_text(&run),
            Err(PlanningStagesError::Pass1(error)) => format!("error: Pass 1: {error}\n"),
            Err(error) => return Err(error.to_string().into()),
        };
        for warning in &warnings {
            text.push_str(&format!("warning: {warning}\n"));
        }
        text
    };
    match out {
        Some(path) => std::fs::write(&path, text).map_err(|e| format!("{path}: {e}"))?,
        None => print!("{text}"),
    }
    match failure {
        Some(error) => Err(Failure::Other(error)),
        None => Ok(()),
    }
}

/// The document for a run that ended with `result` over the frontends'
/// `roots`. A Pass 1 failure still gives a document: the Stage 0 DAG and
/// the error under Stage 1. A failed validation gives none.
fn document_of(
    queries: Vec<Value>,
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    result: Result<PlanningStagesRun, PlanningStagesError>,
    run_until: RunUntil,
) -> Result<Value, String> {
    let queries = with_default_metrics(queries, roots);
    match result {
        Ok(run) => Ok(document(queries, entries, &run)),
        Err(PlanningStagesError::Pass1(error)) => Ok(json!({
            "format": "asap-stage-pipeline/v1",
            "run_until": run_until.name(),
            "workload": { "queries": queries },
            "stage0_query_language_frontends": { "dag": export(roots) },
            "stage1_logical_asap_optimization": {
                "pruned_candidates": [],
                "error": format!("Pass 1: {error}"),
            },
            "stage2_physical_asap_optimization": null,
            "stage3_plan_selection": null,
        })),
        Err(error @ PlanningStagesError::Validation(_)) => Err(error.to_string()),
    }
}

/// The document for `run`, whose workload queries are `queries`: each
/// section of a stage `run` ran, the others null.
fn document(queries: Vec<Value>, entries: &[QueryWorkloadEntry], run: &PlanningStagesRun) -> Value {
    let until = run.run_until;
    let stage1 = (until >= RunUntil::Pass1).then(|| {
        let mut stage1 = json!({});
        // Pass 1 or Pass 2 failed, or the deployment's capabilities
        // contradict the data workload.
        if run.run.logical_asap_candidates.is_none() {
            if let Err(error) = &run.run.selection {
                if !matches!(error, SelectionError::NotRun) {
                    stage1["error"] = json!(error.to_string());
                }
            }
        }
        stage1
    });
    let stage2 = (until >= RunUntil::Stage2).then(|| {
        let mut stage2 = json!({
            "pruned": [],
            "pruned_candidates": stage2_pruned_candidates_json(run.run.physical_asap_candidates.as_ref()),
        });
        // Stage 2 itself failed.
        if run.run.logical_asap_candidates.is_some() && run.run.physical_asap_candidates.is_none() {
            if let Err(error) = &run.run.selection {
                stage2["error"] = json!(error.to_string());
            }
        }
        stage2
    });
    let stage3 = (until == RunUntil::Stage3).then(|| stage3_json(run, entries));
    json!({
        "format": "asap-stage-pipeline/v1",
        "run_until": until.name(),
        "workload": { "queries": queries },
        "stage0_query_language_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "stage1_logical_asap_optimization": stage1,
        "stage2_physical_asap_optimization": stage2,
        "stage3_plan_selection": stage3,
        "memo": memo_json(run),
    })
}

/// Stage 3's section: the plan, the selection and the plan's materialized
/// views.
fn stage3_json(run: &PlanningStagesRun, entries: &[QueryWorkloadEntry]) -> Value {
    let mut stage3 = selection_json(&run.run.selection);
    match &run.run.selection {
        Ok((plan, _)) => match materialized_views_json(plan, entries) {
            Ok(views) => stage3["materialized_views"] = Value::Array(views),
            Err(error) => {
                stage3["materialized_views"] = json!([]);
                stage3["materialized_views_error"] = json!(error);
            }
        },
        Err(_) => stage3["materialized_views"] = json!([]),
    }
    stage3
}

/// The document's `memo` section: the memo after each step `run` ran.
fn memo_json(run: &PlanningStagesRun) -> Value {
    let stage0 = run
        .stage0_memo_groups
        .as_ref()
        .expect("run_stages_until builds Stage 0's memo");
    let mut named = vec![("stage0", "Stage 0, the frontends' DAG hash-consed", stage0)];
    if let Some(pass1) = &run.run.pass1_memo {
        named.push(("pass1", "Stage 1, Pass 1", pass1));
    }
    if let Some(pass2) = &run.pass2_local_window_composition {
        named.push((
            "pass2_local_window_composition",
            "Stage 1, Pass 2: local window composition",
            pass2,
        ));
    }
    if let Some(logical) = &run.run.logical_asap_candidates {
        named.push(("pass2", "Stage 1, Pass 2", &logical.memo));
    }
    let mut table = MemoNodes::default();
    let mut previous: Option<&Memo> = None;
    let mut json_steps = Vec::new();
    for &(step, title, memo) in &named {
        json_steps.push(json!({
            "step": step,
            "title": title,
            "groups": memo_groups_json(&mut table, memo, previous),
        }));
        previous = Some(memo);
    }
    json!({ "nodes": table.nodes, "steps": json_steps })
}

/// The [`metric_type_warnings`] of each PromQL query of `described` (the
/// document's queries) over `data`, prefixed with the query's id; the query
/// still plans.
fn metric_type_warnings_of(
    described: &[Value],
    data: &DataWorkload,
) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    for query in described.iter().filter(|q| q["language"] == "promql") {
        let (id, text) = (&query["id"], query["text"].as_str().unwrap_or_default());
        let id = id.as_str().unwrap_or_default();
        warnings.extend(
            metric_type_warnings(text, data)
                .map_err(|e| format!("{id}: {e}"))?
                .into_iter()
                .map(|warning| format!("{id}: {warning}")),
        );
    }
    Ok(warnings)
}

/// A workload's queries as the document describes them, the frontends'
/// roots, the workload entries and the data workload.
type Lowered = (
    Vec<Value>,
    Vec<QueryRoot>,
    Vec<QueryWorkloadEntry>,
    DataWorkload,
);

/// `workload` lowered by its language's frontend (SQL over `catalog`); its
/// data workload is empty when it declares none.
fn lowered(workload: &PlanningWorkload, catalog: &SqlCatalog) -> Result<Lowered, String> {
    let roots = match workload.query_workload.language {
        QueryLanguage::PromQL => lower_promql_roots(workload)?,
        _ => lower_sql_roots(workload, catalog)?,
    };
    let entries = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    Ok((workload_queries(workload), roots, entries, data))
}

/// `workloads` (each `--workload` file's path and workload, in order),
/// each lowered by its own language's frontend, as one workload: their
/// queries in order, numbered `q1..qN` across the files, and one data
/// workload with every file's sources. A source several files declare must
/// have the same facts in each, and their arrivals must agree
/// ([`arrival_together`]). When several files' data arrives, every source
/// must declare its own rate (`series_count` and `sample_interval`): each
/// declared source, each metric a PromQL query reads and, when a file is
/// SQL, each of `sql_tables` (the `--sql-catalog-table` names). Plan selection shares a
/// workload's `rows_per_second` out over all its sources, so one file's
/// rate would otherwise be priced on another file's sources.
fn lowered_together(
    workloads: &[(&str, PlanningWorkload)],
    catalog: &SqlCatalog,
    sql_tables: &[String],
) -> Result<Lowered, String> {
    let (mut described, mut roots, mut entries) = (Vec::new(), Vec::new(), Vec::new());
    let mut data: Option<DataWorkload> = None;
    // The sources the queries read, beside the declared ones.
    let mut read_sources: BTreeSet<String> = BTreeSet::new();
    for (path, workload) in workloads {
        match workload.query_workload.language {
            QueryLanguage::PromQL => {
                for entry in workload.query_workload.entries() {
                    read_sources.extend(
                        promql_metric_names(&entry.query.0)
                            .map_err(|e| format!("--workload {path}: {e}"))?,
                    );
                }
            }
            _ => read_sources.extend(sql_tables.iter().cloned()),
        }
        let (queries, more_roots, more_entries, more_data) =
            lowered(workload, catalog).map_err(|e| format!("--workload {path}: {e}"))?;
        described.extend(queries);
        roots.extend(more_roots);
        entries.extend(more_entries);
        let Some(data) = data.as_mut() else {
            data = Some(more_data);
            continue;
        };
        data.arrival = arrival_together(data.arrival, more_data.arrival).ok_or_else(|| {
            format!(
                "--workload {path}: its data arrival {:?} disagrees with the earlier files' {:?} \
                 (a file without a data_workload declares an unknown arrival); files planned \
                 together must declare the same kind of arrival",
                more_data.arrival, data.arrival
            )
        })?;
        for (source, facts) in more_data.sources {
            match data.sources.entry(source) {
                Entry::Vacant(slot) => {
                    slot.insert(facts);
                }
                Entry::Occupied(earlier) if *earlier.get() != facts => {
                    return Err(format!(
                    "--workload {path}: source {} is declared with other facts in an earlier file",
                    earlier.key()
                ))
                }
                Entry::Occupied(_) => {}
            }
        }
    }
    let data = data.unwrap_or_default();
    if workloads.len() > 1 && data.arrival.rows_per_second().is_some() {
        let has_own_rate = |source: &String| {
            data.sources.get(source).is_some_and(|facts| {
                facts.series_count.is_some() && facts.sample_interval.is_some()
            })
        };
        if let Some(source) = data
            .sources
            .keys()
            .chain(&read_sources)
            .find(|source| !has_own_rate(source))
        {
            return Err(format!(
                "--workload: source {source} declares no rate of its own (series_count and \
                 sample_interval); with several files, each source needs one, since a file's \
                 rows_per_second is shared out over every file's sources"
            ));
        }
    }
    data.validate().map_err(|e| e.to_string())?;
    for (index, query) in described.iter_mut().enumerate() {
        query["id"] = json!(format!("q{}", index + 1));
    }
    Ok((described, roots, entries, data))
}

/// The arrival of two files' data planned together: ingesting (or mixed)
/// at the sum of their rates, as late as the later of their out-of-order
/// bounds; the same arrival when both are at rest or unknown; `None` when
/// they differ.
fn arrival_together(earlier: DataArrival, later: DataArrival) -> Option<DataArrival> {
    match (earlier, later) {
        (
            DataArrival::Ingesting {
                rows_per_second: a,
                max_out_of_order_ms: late_a,
            },
            DataArrival::Ingesting {
                rows_per_second: b,
                max_out_of_order_ms: late_b,
            },
        ) => Some(DataArrival::Ingesting {
            rows_per_second: a + b,
            max_out_of_order_ms: late_a.max(late_b),
        }),
        (
            DataArrival::Mixed {
                rows_per_second: a,
                max_out_of_order_ms: late_a,
            },
            DataArrival::Mixed {
                rows_per_second: b,
                max_out_of_order_ms: late_b,
            },
        ) => Some(DataArrival::Mixed {
            rows_per_second: a + b,
            max_out_of_order_ms: late_a.max(late_b),
        }),
        (DataArrival::AtRest, DataArrival::AtRest) => Some(DataArrival::AtRest),
        (DataArrival::Unknown, DataArrival::Unknown) => Some(DataArrival::Unknown),
        _ => None,
    }
}

/// The selected plan's materialized views, as the plan
/// [`installable_plan_of`] (what `plan_installable` returns) derives them:
/// each view's node id in the installable DAG, `refresh`, `coverage`,
/// `inputs` and `retention_ms`,
/// plus the node of the selected Stage 2 candidate it is installed from
/// (`stage2_node_id`, null when Stage 3 recorded no cost to map it by), its
/// position among that node's installed nodes (`installed_position`: a
/// tumbling-window chain's tumbling windows, newest first) and their number (`installed_count`).
fn materialized_views_json(
    plan: &asap_types::plan::physical_asap_dag::PhysicalASAPDAG,
    entries: &[QueryWorkloadEntry],
) -> Result<Vec<Value>, String> {
    let (plan, report) = installable_plan_of(plan, None, entries).map_err(|e| e.to_string())?;
    let views = plan.materialized_views().map_err(|e| e.to_string())?;
    let mut stage2_of = std::collections::HashMap::new();
    for node in report
        .iter()
        .flat_map(|report| &report.predicted_usage.per_node)
    {
        for (position, installed) in node.installed_node_ids.iter().enumerate() {
            stage2_of.insert(
                *installed,
                (node.stage3_node_id, position, node.installed_node_ids.len()),
            );
        }
    }
    views
        .iter()
        .map(|view| {
            let mut value = serde_json::to_value(view).map_err(|e| e.to_string())?;
            let (stage2_node_id, position, count) = match stage2_of.get(&view.node_id) {
                Some((id, position, count)) => (json!(id), json!(position), json!(count)),
                None => (Value::Null, Value::Null, Value::Null),
            };
            value["stage2_node_id"] = stage2_node_id;
            value["installed_position"] = position;
            value["installed_count"] = count;
            Ok(value)
        })
        .collect()
}

/// The physical candidates Stage 2 pruned
/// (`PhysicalASAPMemoCandidates::pruned_candidates`), in pruning order.
/// Empty without memo steps.
fn stage2_pruned_candidates_json(stage2: Option<&PhysicalASAPMemoCandidates>) -> Vec<Value> {
    let Some(stage2) = stage2 else {
        return Vec::new();
    };
    let _ = &stage2.pruned_candidates;
    todo!("#1207 PR 9 (#1205): each pruned PhysicalCandidate with its reason")
}

/// Per query, the accuracy Stage 3 accepted: the accepted metric, the
/// sketches read and a one-line summary.
fn accepted_json(queries: &[AcceptedAccuracy]) -> Value {
    queries
        .iter()
        .map(|accepted| {
            json!({
                "accepted_metric": accepted.accepted_metric,
                "sketches": accepted.sketches,
                "summary": accepted.to_string(),
            })
        })
        .collect()
}

/// Stage 3's plan and selection ([`Selection`]): the plan it built, the
/// accuracy accepted per query, the candidates it pruned and the selection
/// MILP's size and time; `error` when the facade selected no plan.
fn selection_json(selection: &Result<(PhysicalASAPDAG, Selection), SelectionError>) -> Value {
    let (plan, selection) = match selection {
        Ok(selected) => selected,
        Err(error) => return json!({ "plan": null, "error": error.to_string() }),
    };
    json!({
        "plan": plan,
        "accepted_accuracy_per_query": accepted_json(&selection.accepted_accuracy_per_query),
        "pruned_candidates": selection
            .pruned_candidates
            .iter()
            .map(|p| json!({ "candidate": format!("{:?}", p.candidate), "reason": format!("{:?}", p.reason) }))
            .collect::<Vec<_>>(),
        "milp_variables": selection.milp_variables,
        "milp_constraints": selection.milp_constraints,
        "milp_construction_ms": selection.milp_construction_ms,
        "milp_solve_ms": selection.milp_solve_ms,
    })
}

/// A logical DAG as a flat node list: node `i` is `nodes[i]`, and children are
/// node indices inside each operator.
fn export(roots: &[QueryRoot]) -> FlatDag {
    flatten(roots).0
}

fn workload_queries(workload: &PlanningWorkload) -> Vec<Value> {
    let language = match workload.query_workload.language {
        QueryLanguage::PromQL => "promql",
        _ => "sql",
    };
    workload
        .query_workload
        .entries()
        .enumerate()
        .map(|(index, entry)| {
            let accuracy = json!(entry.requirements.accuracy.clone());
            let mut requirements = json!({ "accuracy": accuracy });
            if let Some(ms) = entry.requirements.max_latency_ms {
                requirements["max_latency_ms"] = json!(ms);
            }
            if let QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) =
                &entry.recurrence
            {
                requirements["repeat_interval_ms"] = json!(interval.0);
            }
            if let Some(range) = entry.range_evaluation {
                requirements["range_evaluation"] = json!(range);
            }
            json!({
                "id": format!("q{}", index + 1),
                "language": language,
                "text": entry.query.0,
                "requirements": requirements,
                // How often and when the query runs, as the workload entry
                // declares it.
                "recurrence": entry.recurrence,
                "predictability": entry.predictability,
            })
        })
        .collect()
}

/// `queries` with, for each approximate target that sets no metric, the
/// metric its root's result is naturally stated in
/// ([`default_metric_for_root`]; null for a quantile, which has none) as
/// `requirements.default_metric`. Display only: Stage 3 accepts any metric.
fn with_default_metrics(mut queries: Vec<Value>, roots: &[QueryRoot]) -> Vec<Value> {
    for (query, root) in queries.iter_mut().zip(roots) {
        let accuracy = &query["requirements"]["accuracy"];
        let Ok(target) = serde_json::from_value::<AccuracyTarget>(accuracy.clone()) else {
            continue;
        };
        if target.accuracy_metric.is_none() && !target.is_exact() {
            query["requirements"]["default_metric"] = json!(default_metric_for_root(root));
        }
    }
    queries
}

/// The `memo` section's node table (`memo.nodes`): each distinct node once,
/// children before parents, its children as indices into the table.
#[derive(Default)]
struct MemoNodes {
    nodes: Vec<FlatNode>,
}

/// `memo`'s groups, each with its candidates, their nodes added to
/// `table`; a candidate is `added` when `previous` (the memo at the step
/// before) has no such candidate.
fn memo_groups_json(table: &mut MemoNodes, memo: &Memo, previous: Option<&Memo>) -> Vec<Value> {
    let ids = |groups: &[MemoGroupId]| groups.iter().map(|g| g.0).collect::<Vec<_>>();
    memo.groups()
        .iter()
        .enumerate()
        .map(|(id, group)| {
            let candidates = &group.candidates;
            let node = group_node(table, group);
            let mut by_origin = serde_json::Map::new();
            for candidate in candidates {
                let count = by_origin
                    .entry(origin_name(&candidate.origin))
                    .or_insert(json!(0));
                *count = json!(count.as_u64().unwrap_or(0) + 1);
            }
            let listed: Vec<Value> = if candidates.is_empty() {
                Vec::new()
            } else {
                let _ = previous;
                todo!(
                    "#1207 PR 9 (#1205): each candidate's expression, properties and requirements"
                )
            };
            json!({
                "id": id,
                "label": node_label(&table.nodes[node]),
                "children": group.expression.children().into_iter().map(|g| g.0).collect::<Vec<_>>(),
                "parents": ids(group.parent_groups()),
                "queries": group.reached_by_queries(),
                "roots_of": queries_rooted_at(&memo.queries, MemoGroupId(id as u32)),
                "node": node,
                "candidate_count": candidates.len(),
                "candidate_count_by_origin": by_origin,
                "candidates_capped": false,
                "candidates": listed,
            })
        })
        .collect()
}

/// The node of `group`'s own operator, added to `table`.
fn group_node(table: &mut MemoNodes, group: &MemoGroup) -> usize {
    let _ = (table, group);
    todo!("#1207 PR 9: a group's node from its expression")
}

/// `OriginOfCandidateForMemoGroup` in snake case.
fn origin_name(origin: &OriginOfCandidateForMemoGroup) -> String {
    match origin {
        OriginOfCandidateForMemoGroup::Pass1 => "pass1",
        OriginOfCandidateForMemoGroup::SummaryInput => "summary_input",
        OriginOfCandidateForMemoGroup::TumblingWindows => "tumbling_windows",
        OriginOfCandidateForMemoGroup::Rollup => "rollup",
        OriginOfCandidateForMemoGroup::TopKReuse => "top_k_reuse",
        OriginOfCandidateForMemoGroup::WidenedScans => "widened_scans",
    }
    .into()
}
