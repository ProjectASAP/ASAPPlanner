// cargo run -p asap-devtools --bin asap_planner -- \
//     --workload dashboard.json --run-until stage3
// cargo run -p asap-devtools --bin asap_planner -- \
//     --workload promql.json --workload sql.json --sql-catalog-table '<json>' \
//     --run-until stage1_pass2 --format json --out run.json
//
// Runs the planner stages over a workload up to and including
// `--run-until` (`stage0`, `stage1_pass1`, `stage1_pass2`, `stage2` or
// `stage3`) and writes each stage's output, as text (`--format text`, the default;
// `asap_devtools::planning_stages_text`) or as an `asap-stage-pipeline/v2`
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
//   - stage1_logical_asap_optimization (from `stage1_pass1`): with
//     `stage3`, `candidates` holds `L1`, the selected plan's roots (as the
//     realizer built them); `error` when Pass 1 or Pass 2 failed;
//   - stage2_physical_asap_optimization (from `stage2`):
//     `physical_candidates`, Stage 2's candidates (`id`, the logical
//     candidate's `group` and `index_in_group`, `properties` with `timing`
//     and `lifetime`, and per input its `group`, `requirement`
//     (`parameter_floor_for_required_accuracy`) and `edges`: every
//     producer of the group with `satisfies_logical_requirement`,
//     `satisfies_physical_requirement`, `rejection` (why it is not
//     admissible, null when it is) and `selected` (an edge of the selected
//     DAG)); `pruned_candidates`, the physical candidates Stage 2 pruned
//     (`group`, `index_in_group`, `properties`, `reason`); with `stage3`,
//     `candidates` holds `P1`, the selected plan as the realizer built it;
//   - stage3_plan_selection (from `stage3`): Stage 3's `Selection`:
//     `selected_dag` (`candidate_of_node`, `edges`,
//     `node_of_output_reference`),
//     the plan's `cost` (its `usage`, `cost_per_second` and per-node
//     charges), `accepted_accuracy_per_query`, `pruned_candidates` (the
//     candidates Stage 3 pruned: `group`, `candidate`, `reason` and
//     `dominated_by`, the candidate that dominates it or null) and
//     `search_report`; `error` when no plan was selected.
//     `tumbling_window_retention_ms` maps each tumbling-window chain the plan
//     materializes beyond one evaluation (by node id in `P1`) to how far back its
//     tumbling windows are retained.
//     `materialized_views` are the plan's materialized views as
//     `plan_installable` derives them, each with the node of `P1` it is
//     installed from (`stage2_node_id`), its position among that node's
//     installed nodes (`installed_position`) and their number
//     (`installed_count`); `materialized_views_error` when the plan is not
//     installable. `warnings`: a PromQL query applying `rate`, `increase`
//     or `irate` to a declared gauge or combining a declared summary's
//     quantile series across series (`metric_type_warnings`), and a peak memory over 10^12 bytes
//     when the cost model declares no memory capacity;
//   - memo: `steps`, the memo after each step run (`stage0`: the
//     frontends' DAG hash-consed, groups without candidates; `pass1`;
//     `pass2_local_window_composition`: local window composition's
//     tumbling-window candidates; `pass2`: Pass 2's output, the memo Stage
//     2 expands). Each step lists its groups (`id`, `label`, `operator`
//     (its expression, children as group ids), `children`, `parents`,
//     `queries`, `roots_of`) and their candidates (`index`, `origin`,
//     `added` (not in the group at the step before), `label`, `operator`,
//     `children` (its input groups), `fused_groups`, `logical_properties`
//     (`parameters`, `window_layout`) and `input_requirements` (one
//     `parameter_floor_for_required_accuracy` per input)). With a selected
//     plan, `selected_candidates` maps each group the plan uses to the
//     indices of its candidates there;
//   - `search`, `accuracy_guarantees` (`"enforced"` or `"ignored"`),
//     `deployment_capabilities` (as planned with) and `run_until`.
// A section of a stage after `--run-until` is null.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use asap_cost::{CostInputs, ResourceUsage};
use asap_devtools::candidate_labels::operator_label;
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_stages_until, sql_catalog, PlanningStagesError,
    PlanningStagesRun, RunUntil,
};
use asap_devtools::planning_stages_text::{edge_rejection, planning_stages_text, same_candidate};
use asap_devtools::SqlCatalog;
use asap_frontend_promql::{metric_type_warnings, promql_metric_names};
use asap_planner::{installable_plan_of, PlanningModels, SelectionError};
use asap_stage3_plan_selection::{large_peak_memory_warning, AcceptedAccuracy, Selection};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::properties::{default_metric_for_root, AccuracyTarget};
use asap_types::ir::QueryRoot;
use asap_types::physical::PhysicalProperties;
use asap_types::stages::{
    queries_rooted_at, CandidateForMemoGroup, LogicalRequirements, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup, PhysicalASAPMemoCandidates, PruneReason, WindowLayout,
};
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
            .and_then(|selection| {
                large_peak_memory_warning(
                    selection.cost.usage.peak_memory_bytes,
                    &cost_inputs.cost_model,
                )
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
            "format": "asap-stage-pipeline/v2",
            "run_until": run_until.name(),
            "workload": { "queries": queries },
            "stage0_query_language_frontends": { "dag": export(roots) },
            "stage1_logical_asap_optimization": {
                "combinations": 0,
                "capped": false,
                "candidates": [],
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
    let labels = asap_devtools::candidate_labels::candidate_labels(run);
    let logical: Vec<_> = run
        .logical_candidates()
        .iter()
        .map(|c| match &c.roots {
            Ok(roots) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "dag": export(roots) })
            }
            Err(error) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "error": error })
            }
        })
        .collect();
    let physical: Vec<_> = run
        .physical_candidates()
        .iter()
        .map(|p| json!({ "id": p.id, "from_logical": p.from_logical, "display_name": labels[&p.id], "dag": p.dag }))
        .collect();
    let stage1 = (until >= RunUntil::Pass1).then(|| {
        let mut stage1 = json!({
            "combinations": logical.len(),
            "capped": false,
            "candidates": logical,
        });
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
        let stage2_output = run.run.physical_asap_candidates.as_ref();
        let mut stage2 = json!({
            "candidates": physical,
            "physical_candidates": stage2_output.map_or_else(Vec::new, |stage2| {
                physical_candidates_json(stage2, run.run.selection.as_ref().ok())
            }),
            "pruned_candidates": stage2_output.map_or_else(Vec::new, stage2_pruned_candidates_json),
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
        "format": "asap-stage-pipeline/v2",
        "run_until": until.name(),
        "workload": { "queries": queries },
        "stage0_query_language_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "stage1_logical_asap_optimization": stage1,
        "stage2_physical_asap_optimization": stage2,
        "stage3_plan_selection": stage3,
        "memo": memo_json(run),
    })
}

/// Stage 3's section: the selection, the selected plan's tumbling-window retention and
/// its materialized views.
fn stage3_json(run: &PlanningStagesRun, entries: &[QueryWorkloadEntry]) -> Value {
    let mut stage3 = selection_json(&run.run.selection);
    match (&run.run.selection, &run.run.selected_physical_plan) {
        (Ok(selection), Some(realized)) => {
            stage3["tumbling_window_retention_ms"] = realized
                .tumbling_window_retention_ms
                .iter()
                .map(|(node, ms)| (node.to_string(), json!(ms)))
                .collect::<serde_json::Map<_, _>>()
                .into();
            match materialized_views_json(realized, selection, entries) {
                Ok(views) => stage3["materialized_views"] = Value::Array(views),
                Err(error) => {
                    stage3["materialized_views"] = json!([]);
                    stage3["materialized_views_error"] = json!(error);
                }
            }
        }
        _ => stage3["materialized_views"] = json!([]),
    }
    stage3
}

/// The document's `memo` section: the memo after each step `run` ran,
/// and, with a selected plan, `selected_candidates`: each memo group the
/// selected DAG uses, with the indices of its candidates there ($x$ over
/// the memo; a group may be used through several candidates, e.g. one per
/// execution context).
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
    let mut previous: Option<&Memo> = None;
    let mut json_steps = Vec::new();
    for &(step, title, memo) in &named {
        json_steps.push(json!({
            "step": step,
            "title": title,
            "groups": memo_groups_json(memo, previous),
        }));
        previous = Some(memo);
    }
    let mut memo = json!({ "steps": json_steps });
    if let (Ok(selection), Some(stage2)) = (
        &run.run.selection,
        run.run.physical_asap_candidates.as_ref(),
    ) {
        let mut selected: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        for candidate in &selection.selected_dag.candidate_of_node {
            let logical = stage2.physical_candidates[candidate.0].logical_candidate_id;
            selected
                .entry(logical.group.0.to_string())
                .or_default()
                .insert(logical.index_in_group);
        }
        memo["selected_candidates"] = json!(selected);
    }
    memo
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
/// at the sum of their rates; the same arrival when both are at rest or
/// unknown; `None` when they differ.
fn arrival_together(earlier: DataArrival, later: DataArrival) -> Option<DataArrival> {
    match (earlier, later) {
        (
            DataArrival::Ingesting { rows_per_second: a },
            DataArrival::Ingesting { rows_per_second: b },
        ) => Some(DataArrival::Ingesting {
            rows_per_second: a + b,
        }),
        (DataArrival::Mixed { rows_per_second: a }, DataArrival::Mixed { rows_per_second: b }) => {
            Some(DataArrival::Mixed {
                rows_per_second: a + b,
            })
        }
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
    realized: &asap_types::stages::RealizedPlan,
    selection: &Selection,
    entries: &[QueryWorkloadEntry],
) -> Result<Vec<Value>, String> {
    let (plan, report) =
        installable_plan_of(realized, Some(selection), entries).map_err(|e| e.to_string())?;
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

/// Stage 2's physical candidates $\mathcal C$: each with its logical
/// candidate (`group`, `index_in_group`), its `properties` ($\tau$, $\ell$)
/// and, per input $(c, i)$, the input `group`, the `requirement` (the
/// parameter floor, null without one) and every producer $u$ of the group
/// as an `edges` entry: `producer`, `satisfies_logical_requirement`
/// ($\alpha^{\log}$), `satisfies_physical_requirement` ($\alpha^{\mathrm{phys}}$),
/// `rejection` (why it is not admissible, null when it is) and `selected`
/// (a $y$ edge of the selected DAG).
fn physical_candidates_json(
    stage2: &PhysicalASAPMemoCandidates,
    selection: Option<&Selection>,
) -> Vec<Value> {
    let memo = &stage2.logical_candidates.memo;
    let selected: BTreeSet<(usize, usize, usize)> = selection
        .map(|selection| {
            let dag = &selection.selected_dag;
            dag.edges
                .iter()
                .map(|edge| {
                    (
                        dag.candidate_of_node[edge.producer.0].0,
                        dag.candidate_of_node[edge.consumer.0].0,
                        edge.input_index_of_consumer,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let mut inputs: Vec<Vec<Value>> = stage2
        .physical_candidates
        .iter()
        .map(|candidate| {
            let logical = memo.candidate(candidate.logical_candidate_id);
            logical
                .expression
                .operator
                .children()
                .into_iter()
                .zip(&logical.input_requirements)
                .map(|(group, requirement)| {
                    json!({
                        "group": group.0,
                        "requirement": requirement_json(requirement),
                        "edges": [],
                    })
                })
                .collect()
        })
        .collect();
    for (_, edge) in stage2.edges() {
        let (producer, consumer, input) = (
            edge.producer_candidate_id.0,
            edge.consumer_candidate_id.0,
            edge.input_index_of_consumer,
        );
        inputs[consumer][input]["edges"]
            .as_array_mut()
            .expect("edges is an array")
            .push(json!({
                "producer": producer,
                "satisfies_logical_requirement": edge.satisfies_logical_requirement,
                "satisfies_physical_requirement": edge.satisfies_physical_requirement,
                "rejection": edge_rejection(stage2, &edge),
                "selected": selected.contains(&(producer, consumer, input)),
            }));
    }
    stage2
        .physical_candidates
        .iter()
        .zip(inputs)
        .enumerate()
        .map(|(id, (candidate, inputs))| {
            let logical = candidate.logical_candidate_id;
            json!({
                "id": id,
                "group": logical.group.0,
                "index_in_group": logical.index_in_group,
                "properties": physical_properties_json(&candidate.physical_properties),
                "inputs": inputs,
            })
        })
        .collect()
}

/// $p^{\mathrm{phys}}$: `{"timing": ..., "lifetime": ...}`.
fn physical_properties_json(properties: &PhysicalProperties) -> Value {
    json!({ "timing": properties.timing(), "lifetime": properties.lifetime() })
}

/// $r^{\log}_{c,i}$: `{"parameter_floor_for_required_accuracy": θ or null}`.
fn requirement_json(requirement: &LogicalRequirements) -> Value {
    json!({
        "parameter_floor_for_required_accuracy": requirement.parameter_floor_for_required_accuracy(),
    })
}

/// The physical candidates Stage 2 pruned
/// (`PhysicalASAPMemoCandidates::pruned_candidates`), in pruning order:
/// each logical candidate (`group`, `index_in_group`), the `properties` it
/// would have had and the `reason`.
fn stage2_pruned_candidates_json(stage2: &PhysicalASAPMemoCandidates) -> Vec<Value> {
    stage2
        .pruned_candidates
        .iter()
        .map(|pruned| {
            let key = &pruned.candidate;
            json!({
                "group": key.logical_candidate_id.group.0,
                "index_in_group": key.logical_candidate_id.index_in_group,
                "properties": physical_properties_json(&key.physical_properties),
                "reason": prune_reason_name(&pruned.reason),
            })
        })
        .collect()
}

/// A `PruneReason` in snake case, without the candidate `DominatedBy` names.
fn prune_reason_name(reason: &PruneReason) -> &'static str {
    match reason {
        PruneReason::PerEntityAggregateNotExecutable => "per_entity_aggregate_not_executable",
        PruneReason::NoIngestionPipeline => "no_ingestion_pipeline",
        PruneReason::RawDataNotRetained => "raw_data_not_retained",
        PruneReason::QueryTimeResultsNotRetained => "query_time_results_not_retained",
        PruneReason::AcrossEvaluationsWithoutWindowLayout => {
            "across_evaluations_without_window_layout"
        }
        PruneReason::InputHasNoBuildableProducer => "input_has_no_buildable_producer",
        PruneReason::ReachesNoQueryOutput => "reaches_no_query_output",
        PruneReason::DominatedBy(_) => "dominated_by",
        PruneReason::UsageNotCostable => "usage_not_costable",
    }
}

/// One plan's cost: per second, its predicted CPU, unit and source, its
/// usage and each node's charge.
fn cost_json(cost: &asap_cost::PlanCost) -> Value {
    let per_node: serde_json::Map<_, _> = cost
        .per_node
        .iter()
        .map(|(node, c)| {
            (
                node.to_string(),
                json!({
                    "operator": c.operator,
                    "timing": c.timing,
                    "cpu_core_seconds_per_second": c.cpu_core_seconds_per_second,
                    "cost_per_second": c.cost_per_second,
                    "multiplier": c.multiplier,
                    "detail": c.detail,
                    "price_source": c.price_source,
                }),
            )
        })
        .collect();
    json!({
        "cost_per_second": cost.cost_per_second,
        // Report-only: the nodes' predicted CPU, summed as
        // `plan_installable`'s predicted usage sums it.
        "cpu_core_seconds_per_second": cost
            .per_node
            .values()
            .map(|node| node.cpu_core_seconds_per_second)
            .sum::<f64>(),
        "cost_unit": cost.cost_unit,
        "source": cost.source,
        "usage": usage_json(&cost.usage),
        "per_node": per_node,
    })
}

/// Per query, the accuracy Stage 3 accepted: the target's metric, the
/// accepted metric, whether it was accepted regardless of its guarantee,
/// why the guarantee check would have rejected it, the sketches read and a
/// one-line summary.
fn accepted_json(queries: &[AcceptedAccuracy]) -> Value {
    queries
        .iter()
        .map(|accepted| {
            json!({
                "target_metric": accepted.target_metric,
                "accepted_metric": accepted.accepted_metric,
                "accepted_regardless_of_guarantee": accepted.accepted_regardless_of_guarantee,
                "guarantee_check_rejection": accepted.guarantee_check_rejection,
                "sketches": accepted.sketches,
                "summary": accepted.to_string(),
            })
        })
        .collect()
}

/// Stage 3's selection ([`Selection`]): the selected DAG over the selected
/// round's Stage 2 candidates (one node per run of a candidate), the nodes
/// whose weights it proved non-negative, the plan's cost, the accuracy accepted per query, the
/// candidates it pruned and its search report; `error` when the facade
/// selected no plan.
fn selection_json(selection: &Result<Selection, SelectionError>) -> Value {
    let selection = match selection {
        Ok(selection) => selection,
        Err(error) => return json!({ "selected_dag": null, "error": error.to_string() }),
    };
    let dag = &selection.selected_dag;
    let nodes = |nodes: &[asap_types::stages::SelectedNodeId]| -> Vec<usize> {
        nodes.iter().map(|node| node.0).collect()
    };
    json!({
        "selected_dag": {
            "candidate_of_node": dag.candidate_of_node.iter().map(|id| id.0).collect::<Vec<_>>(),
            "edges": dag
                .edges
                .iter()
                .map(|e| json!({ "producer": e.producer.0, "consumer": e.consumer.0, "input": e.input_index_of_consumer }))
                .collect::<Vec<_>>(),
            "node_of_output_reference": nodes(&dag.node_of_output_reference),
        },
        "cost": cost_json(&selection.cost),
        "accepted_accuracy_per_query": accepted_json(&selection.accepted_accuracy_per_query),
        "pruned_candidates": selection
            .pruned_candidates
            .iter()
            .map(|p| {
                let dominated_by = match p.reason {
                    PruneReason::DominatedBy(candidate) => json!(candidate.0),
                    _ => Value::Null,
                };
                json!({
                    "group": p.group.0,
                    "candidate": p.candidate.0,
                    "reason": prune_reason_name(&p.reason),
                    "dominated_by": dominated_by,
                })
            })
            .collect::<Vec<_>>(),
        "search_report": selection.search_report,
    })
}

/// A candidate's resource usage, in the units its field names give.
fn usage_json(usage: &ResourceUsage) -> Value {
    json!({
        "cpu_ops_per_second": usage.cpu_ops_per_second,
        "scanned_bytes_per_second": usage.scanned_bytes_per_second,
        "average_memory_bytes": usage.average_memory_bytes,
        "peak_memory_bytes": usage.peak_memory_bytes,
        "disk_bytes": usage.disk_bytes,
        "peak_cpu_cores": usage.peak_cpu_cores,
        "latency_per_query": usage
            .latency_per_query
            .iter()
            .map(|l| json!({ "compaction_ms": l.compaction_ms, "query_ms": l.query_ms }))
            .collect::<Vec<_>>(),
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

/// `memo`'s groups, each with its operator and its candidates; a candidate
/// is `added` when `previous` (the memo at the step before) has no such
/// candidate.
fn memo_groups_json(memo: &Memo, previous: Option<&Memo>) -> Vec<Value> {
    let ids = |groups: &[MemoGroupId]| groups.iter().map(|g| g.0).collect::<Vec<_>>();
    memo.groups()
        .iter()
        .enumerate()
        .map(|(id, group)| {
            let candidates = &group.candidates;
            let mut by_origin = serde_json::Map::new();
            for candidate in candidates {
                let count = by_origin
                    .entry(origin_name(&candidate.origin))
                    .or_insert(json!(0));
                *count = json!(count.as_u64().unwrap_or(0) + 1);
            }
            let earlier = previous
                .and_then(|memo| memo.groups().get(id))
                .map_or(&[][..], |group| group.candidates.as_slice());
            let listed: Vec<Value> = candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| {
                    let added = previous.is_some()
                        && !earlier
                            .iter()
                            .any(|before| same_candidate(before, candidate));
                    candidate_json(index, candidate, added)
                })
                .collect();
            json!({
                "id": id,
                "label": operator_label(&group.expression),
                "operator": group.expression.map_children(|child| child.0),
                "children": group.expression.children().into_iter().map(|g| g.0).collect::<Vec<_>>(),
                "parents": ids(group.parent_groups()),
                "queries": group.reached_by_queries(),
                "roots_of": queries_rooted_at(&memo.query_roots, MemoGroupId(id as u32)),
                "candidate_count": candidates.len(),
                "candidate_count_by_origin": by_origin,
                "candidates_capped": false,
                "candidates": listed,
            })
        })
        .collect()
}

/// One candidate $v = (e_v, p^{\log}_v)$ of a group: its `index`, `origin`,
/// `added`, `label`, `operator` $e_v$ (children are its input groups),
/// `children` (the input groups), `fused_groups`, `logical_properties`
/// (`parameters` θ, `window_layout` ω) and `input_requirements` (one
/// $r^{\log}_{c,i}$ per input).
fn candidate_json(index: usize, candidate: &CandidateForMemoGroup, added: bool) -> Value {
    let operator = &candidate.expression.operator;
    let window_layout = match candidate.logical_properties.window_layout() {
        Some(WindowLayout::Tumbling { width_ms }) => {
            json!({ "tumbling": { "width_ms": width_ms } })
        }
        None => Value::Null,
    };
    json!({
        "index": index,
        "origin": origin_name(&candidate.origin),
        "added": added,
        "label": operator_label(operator),
        "operator": operator.map_children(|child| child.0),
        "children": operator.children().into_iter().map(|g| g.0).collect::<Vec<_>>(),
        "fused_groups": candidate.fused_groups.iter().map(|g| g.0).collect::<Vec<_>>(),
        "logical_properties": {
            "parameters": candidate.logical_properties.parameters(),
            "window_layout": window_layout,
        },
        "input_requirements": candidate
            .input_requirements
            .iter()
            .map(requirement_json)
            .collect::<Vec<_>>(),
    })
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
