// cargo run -p asap-planner-cli --bin asap_planner -- \
//     --workload dashboard.json --run-until stage3
// cargo run -p asap-planner-cli --bin asap_planner -- \
//     --workload promql.json --workload sql.json --sql-catalog-table '<json>' \
//     --run-until stage1_pass2 --format json --out run.json
//
// Runs the planner stages over a workload up to and including
// `--run-until` (`stage0`, `stage1_pass1`, `stage1_pass2`, `stage2` or
// `stage3`) and writes each stage's output, as text (`--format text`, the default;
// `asap_planner_cli::planning_stages_text`) or as an `asap-stage-pipeline/v2`
// document for the Plan Viewer (`--format json`), to `--out` or standard
// output. The text is terse by default (a legend per stage, folded θ
// variants, producer ids per input, pruned counts per cause); `--verbose`
// prints every candidate, edge with its reason, group's properties and
// pruned candidate.
//
// The stages run through the facade's own pipeline
// (`asap_planner::run_planning_until`, called by
// `asap_planner_cli::planning_stages::run_stages_until`), so the selected plan
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
//     `query_time_results_retained`, `counter_semantics`,
//     `state_not_mergeable_across_windows`, `excluded_candidates`; an
//     omitted field takes its default).
//     Without `supported_summaries` (or without the file), the Planner's
//     executor's summary families are supported, as `plan_installable`
//     defaults it. To plan for a runtime that builds every summary family,
//     pass a file listing them all, e.g.
//     `tools/dag-viewer/deployment-capabilities/every-summary-family.json`.
//   - `--ignore-accuracy-guarantees`: Stage 3 accepts every query output
//     regardless of its accuracy guarantee
//     (`PlanningModels::ignore_accuracy_guarantees`), comparing candidates
//     on cost alone. By default it enforces them, as `plan_installable`
//     does.
//   - `--report-file <file>`: besides the stage output, writes the selected
//     plan's `SelectedPlanReport` JSON (its predicted usage, per node by
//     phase, per query latency, recommended threads), as `plan_installable`
//     returns it; the calibration gate (`tools/calibration_gate`) reads it.
//     Without a costed plan, no file is written and the run exits 1.
//   - `--price-plan <file>` (needs `--report-file`): instead of planning,
//     cost the `InstallablePlan` in `<file>` (a plan that ran) for the
//     workload and cost inputs, and write its report; a calibration
//     compares it with the plan's measured times.
//   - `--plan-file <file>` (needs `--run-until stage3`): besides the stage
//     output, writes the selected plan as an `InstallablePlan` JSON plan
//     file (`InstallablePlan::of_dag`), which a runtime reads with
//     `InstallablePlan::from_json`. When Stage 3 selects no plan, no file is
//     written (a file already at the path is left as it is) and the run
//     exits 1 after writing the stage output.
//   - `--search-space-pruning <rule>=on|off` (repeatable): switches one
//     exact search-space pruning rule (`asap_types::stages::SearchSpacePruning`),
//     or `all` of them; every rule is on by default. A plan selected with
//     a rule off costs what it costs with it on; the switch is for
//     verifying that and measuring what the rule saves.
//
// With a selected plan, the run also compiles it on the Planner's executor
// (`InstallablePlan::check_compiles`) and reports the result: the document's
// `planner_executor_can_run`, and when the executor cannot run the plan a
// line `note: the Planner's executor cannot run this plan: <reason>` on
// standard error. The result never blocks the plan file or the exit status:
// a runtime other than the Planner's executor may run the plan.
//
// The document (`--format json`):
//   - workload: the queries (`id`, `language`, `text`, `requirements`,
//     `recurrence`, `predictability`) and the `data_workload` planned with;
//   - stage0_query_language_frontends: the frontends' DAG, one root per query;
//   - stage1_logical_asap_optimization (from `stage1_pass1`): from
//     `stage1_pass2`, `pruned_candidates`, the candidates Pass 2 eliminated
//     (`group`, `index_in_group`, and `causes`: each with
//     `through_pruned_candidates` and its `root` reason, a serialized
//     `RootPruneReason`, and `text`); `error` when Pass 1 or Pass 2 failed;
//   - stage2_physical_asap_optimization (from `stage2`):
//     `physical_candidates`, Stage 2's candidates (`id`, the logical
//     candidate's `group` and `index_in_group`, `properties` (`timing`,
//     `lifetime`), and per input its `group` and `edges`: each physical
//     edge whose logical edge holds, with `producer`,
//     `satisfies_physical_requirement`, `rejection` (why
//     $\alpha^{\mathrm{phys}} = 0$, null when it holds) and `selected` (an
//     edge of the plan)); `pruned_candidates`, the physical candidates
//     Stage 2 pruned (`group`, `index_in_group`, `properties`, `reason`, a
//     serialized `PruneReason`); `error` when Stage 2 failed;
//   - stage3_plan_selection (from `stage3`): the `plan` Stage 3 built (a
//     `PhysicalASAPDAG`), its `cost` (`asap_cost::plan_cost`:
//     `cost_per_second`, `source`, `usage` and each node's charge), and its
//     `Selection`: `accepted_accuracy_per_query`, `pruned_candidates` (as
//     Stage 2's), `milp_variables`, `milp_constraints`,
//     `milp_objective_cost_per_second` (the selected plan's cost per
//     second), `milp_upper_bound_cost_per_second` (the greedy feasible
//     plan's, which bounds it from above) and, with
//     `ASAP_STAGE3_ROOT_LP=1`, `milp_lp_bound_cost_per_second` (the LP
//     relaxation's, which bounds it from below); `error` when no plan was selected. `sharing_summary`: each
//     plan node two or more queries read, by what it computes (`operator`,
//     `parameters`, `timing`, `lifetime`) and the `queries` reading it.
//     `materialized_views` are the plan's materialized views as
//     `plan_installable` derives them; `materialized_views_error` when the
//     plan fails `InstallablePlan::validate` (its views cannot be derived).
//     `planner_executor_can_run`: whether the Planner's executor compiles
//     the plan, with `planner_executor_error`, its reason, when it does
//     not. `warnings`: a PromQL query applying `rate`, `increase` or
//     `irate` to a declared gauge or combining a declared summary's
//     quantile series across series (`metric_type_warnings`), each data fact the selected
//     plan assumed because the data workload does not declare it (`assumed_data_facts`,
//     saying whether answers may then be wrong beyond the accuracy target), and a peak
//     memory over 10^12 bytes when the cost model declares no memory capacity;
//     `assumed_data_facts`: those facts as records (`AssumedDataFact`), when there are any;
//   - memo: `steps`, the memo after each step run (`stage0`: the
//     frontends' DAG hash-consed, groups without candidates; `pass1`;
//     `pass2`: Pass 2's output, the memo Stage 2 expands). Each step lists
//     its groups (`id`, `label`, `operator` (its expression, children as
//     group ids), `children`, `parents`, the group properties
//     `result_kind`, `column_lineage`, `column_signs`,
//     `depends_on_evaluation_time`, `shareable_when_equal`,
//     `aggregate_coverage` (as the text prints it, null for a group that
//     is no aggregate), `scanned_sources` and, at `pass2`,
//     `evaluation_grid`; `queries`, `roots_of`, `candidate_count`,
//     `candidate_count_by_origin`) and their candidates
//     (`index`, `origin`, `added` (Pass 2 added it), `label`, `operator`,
//     `children` (its input groups), `fused_groups`, `parameters` (θ, null
//     when exact) and `required_accuracy` per input); at `pass2` a
//     candidate also has `pruned` and, per input, `inputs` (`group` and
//     `edges`: the logical edge from each candidate of the input group,
//     eliminated ones included, with its `producer` (index in the group),
//     `satisfies_logical_requirement` and `rejection`, why
//     $\alpha^{\log} = 0$ as Pass 2 recorded it). With a selected plan, `selected_candidates`
//     maps each group the plan uses to the indices of its candidates there;
//   - `search`, `search_space_pruning_off` (the pruning rules switched
//     off), `accuracy_guarantees` (`"enforced"` or `"ignored"`),
//     `deployment_capabilities` (as planned with) and `run_until`.
// A section of a stage after `--run-until` is null.
//
// The document is stable, so it can be compared as text: memo group,
// candidate and physical candidate ids are canonical (renumbered depth
// first from the query roots, `asap_planner_cli::stable_output::CanonicalIds`),
// lists are in canonical id order, every non-integer number is rounded to
// 12 significant digits, and the MILP's times and solve count are left
// out. The text output keeps the planner's own ids, as in logs and error
// messages, and prints the times and solve count.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use asap_cost::{CostInputs, PlanCost};
use asap_frontend_promql::{metric_type_warnings, promql_metric_names};
use asap_frontend_sql::SqlCatalog;
use asap_planner::{assumed_data_facts, InstallablePlan, PlanningModels, SelectionError};
use asap_planner_cli::candidate_labels::operator_label;
use asap_planner_cli::planning_stages::{
    lower_metricsql_roots, lower_promql_roots, lower_sql_roots, run_stages_until, sql_catalog,
    PlanningStagesError, PlanningStagesRun, RunUntil,
};
use asap_planner_cli::planning_stages_text::{
    aggregate_coverage_text, column_lineage_text, edges_per_input, logical_edges_into,
    physical_edge_rejection, planning_stages_text, root_prune_reason_text, selected_edges,
};
use asap_planner_cli::stable_output::{rounded_to_12_significant_digits, CanonicalIds};
use asap_stage3_plan_selection::{large_peak_memory_warning, AcceptedAccuracy, Selection};
use asap_types::accuracy::{default_metric_for_root, AccuracyTarget};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::operator::Source;
use asap_types::ir::QueryRoot;
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::materialized_view::MaterializedView;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::plan::physical_asap_dag::PhysicalASAPNodeId;
use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PrunedCandidate,
    SearchSpacePruning,
};
use asap_types::workload::{
    DataArrival, DataWorkload, PlanningWorkload, QueryLanguage, QueryWorkloadEntry,
};
use serde_json::{json, Value};

const USAGE: &str = "usage: asap_planner (--workload <file>)... \
--run-until stage0|stage1_pass1|stage1_pass2|stage2|stage3 [--format text|json] [--verbose] [--out <file>] \
[--sql-catalog-table <json>]... [--cost-inputs <file>] [--deployment-capabilities <file>] \
[--ignore-accuracy-guarantees] [--plan-file <file>] [--report-file <file>] [--price-plan <file>] \
[--search-space-pruning <rule>=on|off]...";

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
    let mut verbose = false;
    let mut plan_file = None;
    let mut report_file = None;
    let mut price_plan = None;
    let mut search_space_pruning = SearchSpacePruning::default();
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
            "--verbose" => verbose = true,
            "--printing-full-candidates" => {
                return Err(Failure::Usage(
                    "--printing-full-candidates was renamed to --verbose".into(),
                ))
            }
            "--plan-file" => plan_file = Some(value()?),
            "--report-file" => report_file = Some(value()?),
            "--price-plan" => price_plan = Some(value()?),
            "--search-space-pruning" => {
                let setting = value()?;
                let (rule, on) = match setting.split_once('=') {
                    Some((rule, "on")) => (rule, true),
                    Some((rule, "off")) => (rule, false),
                    _ => {
                        return Err(Failure::Usage(format!(
                            "--search-space-pruning {setting}: expected <rule>=on or <rule>=off"
                        )))
                    }
                };
                search_space_pruning
                    .set(rule, on)
                    .map_err(|e| Failure::Usage(format!("--search-space-pruning: {e}")))?;
            }
            other => return Err(Failure::Usage(format!("unknown argument {other}"))),
        }
    }
    let run_until = run_until.ok_or(Failure::Usage("--run-until is required".into()))?;
    if plan_file.is_some() && run_until != RunUntil::Stage3 {
        return Err(Failure::Usage(
            "--plan-file needs --run-until stage3".into(),
        ));
    }
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
    // Plan with the executor's summary families when the deployment declares
    // none.
    if deployment_capabilities.supported_summaries.is_none() {
        deployment_capabilities.supported_summaries =
            Some(asap_executor::supported_summary_families());
    }
    // The checking and measurement switches of `lowest_mean_latency`
    // (never in a cost inputs file), from the environment (the planning
    // crates read none): `ASAP_STAGE3_THETA_DOMINANCE=off`,
    // `ASAP_STAGE3_FEASIBLE_PLAN_BOUND=off`, `ASAP_STAGE3_TIE_BREAKS=full`
    // and `ASAP_STAGE3_PATH_READS=off` turn a speed-up off;
    // `ASAP_STAGE3_ROOT_LP=1` reports the LP bound (cost models 1 and 3);
    // `ASAP_STAGE3_SOLVER_LOG=1` prints the solver's log;
    // `ASAP_STAGE3_NODE_LIMIT=<n>` sets the default stop rule's node limit.
    // `ASAP_STAGE3_EXACT=1` sets the cost inputs' `latency_selection.exact_solve`.
    let knob = |name: &str| std::env::var(name).ok();
    let checks = &mut cost_inputs.latency_selection_checks;
    if knob("ASAP_STAGE3_THETA_DOMINANCE").as_deref() == Some("off") {
        checks.theta_dominance = false;
    }
    if knob("ASAP_STAGE3_FEASIBLE_PLAN_BOUND").as_deref() == Some("off") {
        checks.feasible_plan_bound = false;
    }
    if knob("ASAP_STAGE3_TIE_BREAKS").as_deref() == Some("full") {
        checks.lazy_tie_breaks = false;
    }
    if knob("ASAP_STAGE3_PATH_READS").as_deref() == Some("off") {
        checks.path_reads = false;
    }
    if knob("ASAP_STAGE3_ROOT_LP").is_some() {
        checks.report_root_lp_bound = true;
    }
    if knob("ASAP_STAGE3_SOLVER_LOG").is_some() {
        checks.log_solver = true;
    }
    if let Some(nodes) = knob("ASAP_STAGE3_NODE_LIMIT") {
        checks.node_limit = nodes.parse().map_err(|_| {
            Failure::Usage(format!("ASAP_STAGE3_NODE_LIMIT={nodes}: not a node count"))
        })?;
    }
    if knob("ASAP_STAGE3_EXACT").is_some() {
        cost_inputs.latency_selection.exact_solve = true;
    }
    // Price a given plan instead of planning: its report only.
    if let Some(path) = price_plan {
        let report_path =
            report_file.ok_or(Failure::Usage("--price-plan needs --report-file".into()))?;
        cost_inputs
            .validate()
            .map_err(|e| format!("--cost-inputs: {e}"))?;
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
        let plan = InstallablePlan::from_json(&text).map_err(|e| format!("{path}: {e}"))?;
        let (cost, _) = asap_cost::plan_cost(&plan.dag, &entries, &data, &cost_inputs)
            .map_err(|(node, e)| format!("--price-plan {path}: node {node}: {e}"))?;
        let facts = assumed_data_facts(&plan.dag, &cost, &data);
        let report = asap_planner::selected_plan_report(&cost, facts);
        let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(&report_path, json + "\n")
            .map_err(|e| format!("--report-file {report_path}: {e}"))?;
        return Ok(());
    }
    let models = PlanningModels::builtin()
        .with_cost_inputs(&cost_inputs)
        .with_deployment_capabilities(&deployment_capabilities)
        .with_ignore_accuracy_guarantees(ignore_accuracy_guarantees);
    let result = run_stages_until(
        roots.clone(),
        &entries,
        &data,
        models,
        search_space_pruning,
        run_until,
    );
    // The selected plan's cost, as `plan_installable` reports it.
    let plan_cost = result
        .as_ref()
        .ok()
        .and_then(|run| run.run.selection.as_ref().ok())
        .and_then(|(plan, _)| asap_cost::plan_cost(plan, &entries, &data, &cost_inputs).ok())
        .map(|(cost, _)| cost);
    // The plan report `plan_installable` returns beside the plan.
    let selected_plan_report = result
        .as_ref()
        .ok()
        .and_then(|run| run.run.selection.as_ref().ok())
        .zip(plan_cost.as_ref())
        .map(|((plan, selection), cost)| {
            let facts = asap_planner::assumed_data_facts(plan, cost, &data);
            let mut report = asap_planner::selected_plan_report(cost, facts);
            report.optimality_gap = selection.optimality_gap;
            report
        });
    // The selected plan as a runtime installs it, validated.
    let installable = result
        .as_ref()
        .ok()
        .and_then(|run| run.run.selection.as_ref().ok())
        .map(|(plan, _)| {
            let query_texts = entries.iter().map(|entry| entry.query.0.clone()).collect();
            InstallablePlan::of_dag(plan.clone(), query_texts).map_err(|e| e.to_string())
        });
    // Whether the Planner's executor compiles it: reported, never blocking.
    let executor_can_run = installable
        .as_ref()
        .and_then(|installable| installable.as_ref().ok())
        .map(InstallablePlan::check_compiles);
    // Its materialized views, as `plan_installable` derives them.
    let materialized_views = installable.as_ref().map(|installable| {
        let plan = installable.as_ref().map_err(Clone::clone)?;
        plan.materialized_views().map_err(|e| e.to_string())
    });
    // A stage that failed: the output is still written, then the run exits 1.
    let mut failure = match &result {
        Err(PlanningStagesError::Pass1(error)) => Some(format!("Pass 1: {error}")),
        Ok(run) => match &run.run.selection {
            Err(SelectionError::NotRun) | Ok(_) => None,
            Err(error) => Some(error.to_string()),
        },
        Err(_) => None,
    };
    // A rate, increase or irate over a declared gauge (the query still plans).
    let mut warnings = metric_type_warnings_of(&described, &data)?;
    // The selected plan's data facts assumed because the data workload
    // does not declare them, and its very large peak memory when the
    // deployment declares no memory capacity to check it against.
    let assumed_facts = result
        .as_ref()
        .ok()
        .and_then(|run| run.run.selection.as_ref().ok())
        .zip(plan_cost.as_ref())
        .map(|((plan, _), cost)| assumed_data_facts(plan, cost, &data))
        .unwrap_or_default();
    warnings.extend(assumed_facts.iter().map(ToString::to_string));
    warnings.extend(plan_cost.as_ref().and_then(|cost| {
        large_peak_memory_warning(cost.usage.peak_memory_bytes, &cost_inputs.cost_model)
    }));
    let text = if json_format {
        let mut document = document_of(
            described,
            &roots,
            result,
            run_until,
            plan_cost.as_ref(),
            materialized_views.as_ref(),
        )?;
        // Stage 3 is null when Pass 1 rejected a query.
        if let Some(stage3) = document["stage3_plan_selection"].as_object_mut() {
            if let Some(can_run) = &executor_can_run {
                stage3.insert("planner_executor_can_run".into(), json!(can_run.is_ok()));
                if let Err(reason) = can_run {
                    stage3.insert("planner_executor_error".into(), json!(reason));
                }
            }
            if !warnings.is_empty() {
                stage3.insert("warnings".into(), json!(warnings));
            }
            // The same assumed facts as records, for a machine reader.
            if !assumed_facts.is_empty() {
                stage3.insert(
                    "assumed_data_facts".into(),
                    serde_json::to_value(&assumed_facts).map_err(|e| e.to_string())?,
                );
            }
        }
        // The planner's numbers; the inputs added below stay as given.
        let mut document = rounded_to_12_significant_digits(document);
        document["workload"]["data_workload"] =
            serde_json::to_value(&data).map_err(|e| e.to_string())?;
        document["search"] = json!("candidate-connections");
        document["search_space_pruning_off"] = json!(search_space_pruning.rules_off());
        document["accuracy_guarantees"] = json!(if ignore_accuracy_guarantees {
            "ignored"
        } else {
            "enforced"
        });
        // Read-only: the deployment inputs the run planned with.
        document["deployment_capabilities"] =
            serde_json::to_value(&deployment_capabilities).map_err(|e| e.to_string())?;
        document["cost_inputs"] = serde_json::to_value(&cost_inputs).map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n"
    } else {
        let mut text = match result {
            Ok(run) => planning_stages_text(
                &run,
                plan_cost.as_ref(),
                materialized_views.as_ref(),
                verbose,
            ),
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
    if let Some(Err(reason)) = &executor_can_run {
        eprintln!("note: the Planner's executor cannot run this plan: {reason}");
    }
    if let Some(path) = report_file {
        match &selected_plan_report {
            Some(report) => {
                let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
                std::fs::write(&path, json + "\n")
                    .map_err(|e| format!("--report-file {path}: {e}"))?;
            }
            _ => {
                failure = Some(failure.map_or_else(
                    || format!("no plan was costed (no report file written to {path})"),
                    |error| format!("{error} (no report file written to {path})"),
                ))
            }
        }
    }
    if let Some(path) = plan_file {
        match installable {
            Some(Ok(plan)) => {
                let json = serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?;
                std::fs::write(&path, json + "\n")
                    .map_err(|e| format!("--plan-file {path}: {e}"))?;
            }
            Some(Err(error)) => failure = Some(format!("--plan-file {path}: {error}")),
            // No plan was selected: `failure` already says why.
            None => {
                failure = failure.map(|error| format!("{error} (no plan file written to {path})"))
            }
        }
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
    result: Result<PlanningStagesRun, PlanningStagesError>,
    run_until: RunUntil,
    plan_cost: Option<&PlanCost>,
    materialized_views: Option<&Result<Vec<MaterializedView>, String>>,
) -> Result<Value, String> {
    let queries = with_default_metrics(queries, roots);
    match result {
        Ok(run) => Ok(document(queries, &run, plan_cost, materialized_views)),
        Err(PlanningStagesError::Pass1(error)) => Ok(json!({
            "format": "asap-stage-pipeline/v2",
            "run_until": run_until.name(),
            "workload": { "queries": queries },
            "stage0_query_language_frontends": { "dag": export(roots) },
            "stage1_logical_asap_optimization": { "error": format!("Pass 1: {error}") },
            "stage2_physical_asap_optimization": null,
            "stage3_plan_selection": null,
        })),
        Err(error @ PlanningStagesError::Validation(_)) => Err(error.to_string()),
    }
}

/// The document for `run`, whose workload queries are `queries`: each
/// section of a stage `run` ran, the others null. `plan_cost` is the
/// plan's cost and `materialized_views` its materialized views.
fn document(
    queries: Vec<Value>,
    run: &PlanningStagesRun,
    plan_cost: Option<&PlanCost>,
    materialized_views: Option<&Result<Vec<MaterializedView>, String>>,
) -> Value {
    let until = run.run_until;
    let plan = run.run.selection.as_ref().ok().map(|(plan, _)| plan);
    let ids = canonical_ids_of(run);
    let stage1 = (until >= RunUntil::Pass1).then(|| {
        let mut stage1 = json!({});
        if let Some(logical) = &run.run.logical_asap_candidates {
            stage1["pruned_candidates"] = json!(pass2_pruned_json(logical, &ids));
        } else if let Err(error) = &run.run.selection {
            // Pass 1 or Pass 2 failed, or the deployment's capabilities
            // contradict the data workload.
            if !matches!(error, SelectionError::NotRun) {
                stage1["error"] = json!(error.to_string());
            }
        }
        stage1
    });
    let stage2 = (until >= RunUntil::Stage2).then(|| {
        let mut stage2 = match &run.run.physical_asap_candidates {
            Some(physical) => json!({
                "physical_candidates": physical_candidates_json(physical, plan, &ids),
                "pruned_candidates": pruned_candidates_json(&physical.pruned_candidates, &ids),
            }),
            None => json!({ "physical_candidates": [], "pruned_candidates": [] }),
        };
        // Stage 2 itself failed.
        if run.run.logical_asap_candidates.is_some() && run.run.physical_asap_candidates.is_none() {
            if let Err(error) = &run.run.selection {
                stage2["error"] = json!(error.to_string());
            }
        }
        stage2
    });
    let stage3 =
        (until == RunUntil::Stage3).then(|| stage3_json(run, plan_cost, materialized_views, &ids));
    json!({
        "format": "asap-stage-pipeline/v2",
        "run_until": until.name(),
        "workload": { "queries": queries },
        "stage0_query_language_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "stage1_logical_asap_optimization": stage1,
        "stage2_physical_asap_optimization": stage2,
        "stage3_plan_selection": stage3,
        "memo": memo_json(run, &ids),
    })
}

/// The canonical ids of `run`'s last memo (Pass 2's, else Pass 1's, else
/// Stage 0's) and of its physical candidates.
fn canonical_ids_of(run: &PlanningStagesRun) -> CanonicalIds {
    let logical = run.run.logical_asap_candidates.as_ref();
    let memo = logical
        .map(|logical| &logical.memo)
        .or(run.run.pass1_memo.as_ref())
        .or(run.run.stage0_memo.as_ref())
        .expect("run_stages_until keeps Stage 0's memo");
    CanonicalIds::of(memo, run.run.physical_asap_candidates.as_ref())
}

/// Stage 3's section: the plan, its cost, its sharing summary, the
/// selection and the plan's materialized views.
fn stage3_json(
    run: &PlanningStagesRun,
    plan_cost: Option<&PlanCost>,
    materialized_views: Option<&Result<Vec<MaterializedView>, String>>,
    ids: &CanonicalIds,
) -> Value {
    let mut stage3 = selection_json(&run.run.selection, ids);
    if let Ok((plan, _)) = &run.run.selection {
        stage3["cost"] = plan_cost.map_or(Value::Null, cost_json);
        stage3["sharing_summary"] = json!(sharing_summary_json(plan));
    }
    stage3["materialized_views"] = json!([]);
    match materialized_views {
        Some(Ok(views)) => stage3["materialized_views"] = json!(views),
        Some(Err(error)) => stage3["materialized_views_error"] = json!(error),
        None => {}
    }
    stage3
}

/// The document's `memo` section: the memo after each step `run` ran,
/// and, with a selected plan, `selected_candidates`: each memo group the
/// plan uses, with the indices of its candidates there (a group may be
/// used through several candidates). Ids are `ids`' canonical ones.
fn memo_json(run: &PlanningStagesRun, ids: &CanonicalIds) -> Value {
    let stage0 = run
        .run
        .stage0_memo
        .as_ref()
        .expect("run_stages_until keeps Stage 0's memo");
    let mut steps = vec![json!({
        "step": "stage0",
        "title": "Stage 0, the frontends' DAG hash-consed",
        "groups": memo_groups_json(stage0, None, ids),
    })];
    if let Some(pass1) = &run.run.pass1_memo {
        steps.push(json!({
            "step": "pass1",
            "title": "Stage 1, Pass 1",
            "groups": memo_groups_json(pass1, None, ids),
        }));
        if let Some(logical) = &run.run.logical_asap_candidates {
            steps.push(json!({
                "step": "pass2",
                "title": "Stage 1, Pass 2",
                "groups": memo_groups_json(&logical.memo, Some((pass1, logical)), ids),
            }));
        }
    }
    let mut memo = json!({ "steps": steps });
    if let (Ok((plan, _)), Some(physical)) = (
        &run.run.selection,
        run.run.physical_asap_candidates.as_ref(),
    ) {
        let mut selected: BTreeMap<u32, BTreeSet<usize>> = BTreeMap::new();
        for candidate in plan
            .nodes
            .iter()
            .filter_map(|node| node.selected_physical_candidate)
        {
            let logical = physical.physical_candidates[candidate.0].logical_candidate_id;
            let (group, index_in_group) = ids.candidate(logical);
            selected.entry(group).or_default().insert(index_in_group);
        }
        let selected: serde_json::Map<String, Value> = selected
            .into_iter()
            .map(|(group, indices)| (group.to_string(), json!(indices)))
            .collect();
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
        QueryLanguage::MetricsQL => lower_metricsql_roots(workload)?,
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
/// rate would otherwise be costed on another file's sources.
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
        let (queries, more_roots, more_entries, more_data) =
            lowered(workload, catalog).map_err(|e| format!("--workload {path}: {e}"))?;
        match workload.query_workload.language {
            QueryLanguage::PromQL => {
                for entry in workload.query_workload.entries() {
                    read_sources.extend(
                        promql_metric_names(&entry.query.0)
                            .map_err(|e| format!("--workload {path}: {e}"))?,
                    );
                }
            }
            QueryLanguage::MetricsQL => read_sources.extend(scanned_sources(&more_roots)),
            _ => read_sources.extend(sql_tables.iter().cloned()),
        }
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
        query["id"] = json!(query_id(index));
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

/// Stage 2's physical candidates $\mathcal C$: each with its logical
/// candidate (`group`, `index_in_group`), its `properties` ($\tau$,
/// $\ell$) and, per input $(c, i)$, the input `group` and, as `edges`,
/// each physical edge whose logical edge holds: `producer`,
/// `satisfies_physical_requirement` ($\alpha^{\mathrm{phys}}$), `rejection`
/// (why it fails, null when it holds) and `selected` (an edge of `plan`).
/// In canonical id order (`ids`), each input's edges by producer.
fn physical_candidates_json(
    physical: &PhysicalASAPMemoCandidates,
    plan: Option<&PhysicalASAPDAG>,
    ids: &CanonicalIds,
) -> Vec<Value> {
    let memo = &physical.logical_candidates.memo;
    let selected = selected_edges(plan);
    let mut listed: Vec<(usize, Value)> = physical
        .physical_candidates
        .iter()
        .zip(edges_per_input(physical))
        .enumerate()
        .map(|(id, (candidate, edges_into))| {
            let logical = candidate.logical_candidate_id;
            let groups = memo.candidate(logical).expression.operator.children();
            let inputs: Vec<Value> = groups
                .into_iter()
                .zip(edges_into)
                .enumerate()
                .map(|(input, (group, edges))| {
                    let mut edges: Vec<(usize, Value)> = edges
                        .iter()
                        .filter(|edge| edge.satisfies_logical_requirement)
                        .map(|edge| {
                            let producer = edge.producer_candidate_id;
                            let canonical_producer = ids.physical(producer);
                            (canonical_producer, json!({
                                "producer": canonical_producer,
                                "satisfies_physical_requirement": edge.satisfies_physical_requirement,
                                "rejection": physical_edge_rejection(physical, edge),
                                "selected": selected.contains(&(producer, PhysicalCandidateId(id), input)),
                            }))
                        })
                        .collect();
                    edges.sort_by_key(|(producer, _)| *producer);
                    let edges: Vec<Value> = edges.into_iter().map(|(_, edge)| edge).collect();
                    json!({ "group": ids.group(*group), "edges": edges })
                })
                .collect();
            let canonical_id = ids.physical(PhysicalCandidateId(id));
            let (group, index_in_group) = ids.candidate(logical);
            (canonical_id, json!({
                "id": canonical_id,
                "group": group,
                "index_in_group": index_in_group,
                "properties": candidate.physical_properties,
                "inputs": inputs,
            }))
        })
        .collect();
    listed.sort_by_key(|(id, _)| *id);
    listed.into_iter().map(|(_, candidate)| candidate).collect()
}

/// Candidates Stage 2 or Stage 3 pruned: each logical candidate (`group`,
/// `index_in_group`), the `properties` it would have had and the `reason`;
/// by canonical candidate id (`ids`), then in pruning order.
fn pruned_candidates_json(
    pruned: &[PrunedCandidate<PhysicalCandidate>],
    ids: &CanonicalIds,
) -> Vec<Value> {
    let mut listed: Vec<((u32, usize), Value)> = pruned
        .iter()
        .map(|pruned| {
            let (group, index_in_group) = ids.candidate(pruned.candidate.logical_candidate_id);
            let reason = serde_json::to_value(pruned.reason).unwrap_or_default();
            (
                (group, index_in_group),
                json!({
                    "group": group,
                    "index_in_group": index_in_group,
                    "properties": pruned.candidate.physical_properties,
                    "reason": ids.in_serialized(reason),
                }),
            )
        })
        .collect();
    listed.sort_by_key(|(id, _)| *id);
    listed.into_iter().map(|(_, pruned)| pruned).collect()
}

/// The candidates Pass 2 eliminated: each candidate (`group`,
/// `index_in_group`) with its `causes`, each the eliminated consumers it
/// comes through (`through_pruned_candidates`), the `root` reason and its
/// `text`; by canonical candidate id (`ids`).
fn pass2_pruned_json(logical: &LogicalASAPMemoCandidates, ids: &CanonicalIds) -> Vec<Value> {
    let mut listed: Vec<((u32, usize), Value)> = logical
        .pruned
        .iter()
        .map(|pruned| {
            let causes: Vec<Value> = pruned
                .causes
                .iter()
                .map(|cause| {
                    let through: Vec<Value> = cause
                        .through_pruned_candidates
                        .iter()
                        .map(|id| ids.candidate_json(*id))
                        .collect();
                    let root = serde_json::to_value(&cause.root).unwrap_or_default();
                    json!({
                        "through_pruned_candidates": through,
                        "root": ids.in_serialized(root),
                        "text": root_prune_reason_text(&cause.root),
                    })
                })
                .collect();
            let (group, index_in_group) = ids.candidate(pruned.candidate);
            (
                (group, index_in_group),
                json!({
                    "group": group,
                    "index_in_group": index_in_group,
                    "causes": causes,
                }),
            )
        })
        .collect();
    listed.sort_by_key(|(id, _)| *id);
    listed.into_iter().map(|(_, pruned)| pruned).collect()
}

/// One plan's cost: per second, its predicted CPU, its source, its usage
/// and each node's charge.
fn cost_json(cost: &PlanCost) -> Value {
    let per_node: serde_json::Map<_, _> = cost
        .per_node
        .iter()
        .map(|(node, charge)| {
            (
                node.to_string(),
                json!({
                    "operator": charge.operator,
                    "timing": charge.timing,
                    "cpu_core_seconds_per_second": charge.cpu_core_seconds_per_second,
                    "cost_per_second": charge.cost_per_second,
                    "multiplier": charge.multiplier,
                    "detail": charge.detail,
                    "cost_source": charge.cost_source,
                    "ingest_cpu_core_seconds_per_second":
                        charge.phase_times.ingest_cpu_core_seconds_per_second,
                    "refresh_cpu_ms_per_window": charge.phase_times.refresh_cpu_ms_per_window,
                    "refresh_wall_ms_per_window": charge.phase_times.refresh_wall_ms_per_window,
                    "query_ms_per_step": charge.phase_times.query_ms_per_step,
                    "priced_outside_measured_scale": charge.priced_outside_measured_scale,
                }),
            )
        })
        .collect();
    let usage = &cost.usage;
    json!({
        "cost_per_second": cost.cost_per_second,
        // Report-only: the nodes' predicted CPU, summed as
        // `plan_installable`'s predicted usage sums it.
        "cpu_core_seconds_per_second": cost
            .per_node
            .values()
            .map(|node| node.cpu_core_seconds_per_second)
            .sum::<f64>(),
        "source": cost.source,
        "usage": {
            "cpu_ops_per_second": usage.cpu_ops_per_second,
            "scanned_bytes_per_second": usage.scanned_bytes_per_second,
            "average_memory_bytes": usage.average_memory_bytes,
            "peak_memory_bytes": usage.peak_memory_bytes,
            "disk_bytes": usage.disk_bytes,
            "peak_cpu_cores": usage.peak_cpu_cores,
            "latency_per_query": usage
                .latency_per_query
                .iter()
                .map(|latency| json!({
                    "allowed_lateness_ms": latency.allowed_lateness_ms,
                    "precompute_queueing_ms": latency.precompute_queueing_ms,
                    "precompute_execution_ms": latency.precompute_execution_ms,
                    "query_execution_ms": latency.query_execution_ms,
                    "latency_ms": latency.latency_ms(),
                    "evaluations_per_second": latency.evaluations_per_second,
                }))
                .collect::<Vec<_>>(),
            "precompute_load": usage.precompute_load,
            "recommended_configuration": usage.recommended_configuration,
        },
        "per_node": per_node,
    })
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
/// accuracy accepted per query, the candidates it pruned, the selection
/// MILP's size and its objective; `error` when the facade selected no
/// plan. Its times and solve count, which change from run to run or with
/// the solver, are in the text output only.
fn selection_json(
    selection: &Result<(PhysicalASAPDAG, Selection), SelectionError>,
    ids: &CanonicalIds,
) -> Value {
    let (plan, selection) = match selection {
        Ok(selected) => selected,
        Err(error) => return json!({ "plan": null, "error": error.to_string() }),
    };
    json!({
        "plan": plan,
        "accepted_accuracy_per_query": accepted_json(&selection.accepted_accuracy_per_query),
        "pruned_candidates": pruned_candidates_json(&selection.pruned_candidates, ids),
        "milp_variables": selection.milp_variable_count,
        "milp_constraints": selection.milp_constraint_count,
        "milp_objective_cost_per_second": selection.selected_plan_cost_per_second,
        "milp_objective_mean_latency_ms": selection.selected_plan_mean_latency_ms,
        "milp_latency_ms_per_query": selection.selected_plan_latency_ms_per_query,
        "milp_root_lp_mean_latency_ms": selection.milp_root_lp_mean_latency_ms,
        "milp_upper_bound_cost_per_second": selection.milp_upper_bound_cost_per_second,
        "milp_lp_bound_cost_per_second": selection.milp_lp_bound_cost_per_second,
        "milp_optimality_gap": selection.optimality_gap,
        "executions_pruned_by_latency_bound": selection.executions_pruned_by_latency_bound,
    })
}

/// A logical DAG as a flat node list: node `i` is `nodes[i]`, and children are
/// node indices inside each operator.
fn export(roots: &[QueryRoot]) -> FlatDag {
    flatten(roots).0
}

/// The sources the `Scan`s of `roots` read: metric names and table names.
fn scanned_sources(roots: &[QueryRoot]) -> BTreeSet<String> {
    flatten(roots)
        .0
        .nodes
        .into_iter()
        .filter_map(|node| match node.operator {
            Operator::NonASAP(NonASAPOp::Scan { source, .. }) => Some(match source {
                Source::TimeSeries { metric } => metric,
                Source::Table { table_ref } => table_ref,
            }),
            _ => None,
        })
        .collect()
}

fn workload_queries(workload: &PlanningWorkload) -> Vec<Value> {
    let language = match workload.query_workload.language {
        QueryLanguage::PromQL => "promql",
        QueryLanguage::MetricsQL => "metricsql",
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
            if let Some(interval_ms) = entry.recurrence.fixed_interval_ms() {
                requirements["repeat_interval_ms"] = json!(interval_ms);
            }
            if let Some(range) = entry.range_evaluation {
                requirements["range_evaluation"] = json!(range);
            }
            json!({
                "id": query_id(index),
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

/// `memo`'s groups, each with its operator and its candidates, groups and
/// candidates in canonical id order (`ids`). At Pass 2 (`pass2`: the memo
/// at Pass 1 and Pass 2's output) a candidate is `added` when its index is
/// past the group's Pass 1 candidates (the memo only appends), has
/// `pruned`, and lists its logical edges per input, with the reasons Pass
/// 2 recorded.
fn memo_groups_json(
    memo: &Memo,
    pass2: Option<(&Memo, &LogicalASAPMemoCandidates)>,
    ids: &CanonicalIds,
) -> Vec<Value> {
    let group_ids = |groups: Vec<&MemoGroupId>| {
        groups
            .into_iter()
            .map(|g| ids.group(*g))
            .collect::<Vec<_>>()
    };
    let pruned: BTreeSet<CandidateForMemoGroupId> = pass2
        .map(|(_, logical)| logical.pruned.iter().map(|p| p.candidate).collect())
        .unwrap_or_default();
    let mut listed_groups: Vec<(u32, Value)> = memo
        .groups()
        .iter()
        .enumerate()
        .map(|(id, group)| {
            let group_id = MemoGroupId(id as u32);
            let candidates = &group.candidates;
            let mut by_origin = serde_json::Map::new();
            for candidate in candidates {
                let count = by_origin
                    .entry(origin_name(candidate))
                    .or_insert(json!(0));
                *count = json!(count.as_u64().unwrap_or(0) + 1);
            }
            let mut listed: Vec<(usize, Value)> = candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| {
                    let candidate_id = CandidateForMemoGroupId {
                        group: group_id,
                        index_in_group: index,
                    };
                    let (_, canonical_index) = ids.candidate(candidate_id);
                    let mut value = candidate_json(canonical_index, candidate, ids);
                    if let Some((pass1, logical)) = pass2 {
                        value["added"] = json!(pass1
                            .groups()
                            .get(id)
                            .is_none_or(|before| index >= before.candidates.len()));
                        value["pruned"] = json!(pruned.contains(&candidate_id));
                        value["inputs"] = json!(logical_inputs_json(logical, candidate_id, ids));
                    }
                    (canonical_index, value)
                })
                .collect();
            listed.sort_by_key(|(index, _)| *index);
            let listed: Vec<Value> = listed.into_iter().map(|(_, value)| value).collect();
            let canonical_group = ids.group(group_id);
            (canonical_group, json!({
                "id": canonical_group,
                "label": operator_label(&group.expression),
                "operator": group.expression.map_children(|child| ids.group(*child)),
                "children": group_ids(group.expression.children()),
                "parents": group_ids(group.parent_groups().iter().collect()),
                "result_kind": group.group_properties.result_kind,
                "column_lineage": column_lineage_text(group, |group| ids.group(group)),
                "column_signs": group.group_properties.column_signs,
                "depends_on_evaluation_time": group.group_properties.depends_on_evaluation_time,
                "shareable_when_equal": group.group_properties.shareable_when_equal,
                "aggregate_coverage": group
                    .group_properties
                    .aggregate_coverage
                    .as_ref()
                    .map(|coverage| aggregate_coverage_text(coverage, |group| ids.group(group))),
                "scanned_sources": group.group_properties.scanned_sources,
                "evaluation_grid": pass2
                    .and_then(|(_, logical)| logical.evaluation_grid_of_group[id].as_ref())
                    .map(|grid| json!({"period_ms": grid.period_ms, "phase_ms": grid.phase_ms})),
                "queries": group.reached_by_queries(),
                "roots_of": memo.queries_rooted_at(group_id),
                "candidate_count": candidates.len(),
                "candidate_count_by_origin": by_origin,
                "candidates": listed,
            }))
        })
        .collect();
    listed_groups.sort_by_key(|(id, _)| *id);
    listed_groups.into_iter().map(|(_, group)| group).collect()
}

/// Per input of `consumer`, its `group` and, as `edges`, each candidate of
/// the group ([`logical_edges_into`], eliminated ones included):
/// `producer` (its index in the group), `satisfies_logical_requirement`
/// ($\alpha^{\log}$) and `rejection` (why it fails, null when it holds).
/// Ids are canonical (`ids`), edges by producer.
fn logical_inputs_json(
    logical: &LogicalASAPMemoCandidates,
    consumer: CandidateForMemoGroupId,
    ids: &CanonicalIds,
) -> Vec<Value> {
    let groups = logical
        .memo
        .candidate(consumer)
        .expression
        .operator
        .children();
    groups
        .into_iter()
        .zip(logical_edges_into(logical, consumer))
        .map(|(group, edges)| {
            let mut edges: Vec<(usize, Value)> = edges
                .into_iter()
                .enumerate()
                .map(|(producer, status)| {
                    let (_, producer) = ids.candidate(CandidateForMemoGroupId {
                        group: *group,
                        index_in_group: producer,
                    });
                    let rejection = status.rejection();
                    (
                        producer,
                        json!({
                            "producer": producer,
                            "satisfies_logical_requirement": rejection.is_none(),
                            "rejection": rejection,
                        }),
                    )
                })
                .collect();
            edges.sort_by_key(|(producer, _)| *producer);
            let edges: Vec<Value> = edges.into_iter().map(|(_, edge)| edge).collect();
            json!({ "group": ids.group(*group), "edges": edges })
        })
        .collect()
}

/// One candidate $v = (e_v, p^{\log}_v)$ of a group: its `index`,
/// `origin`, `label`, `operator` $e_v$ (children are its input groups),
/// `children` (the input groups), `fused_groups`, `parameters` (θ) and
/// `required_accuracy` (one per input, null without one). Group ids are
/// canonical (`ids`).
fn candidate_json(index: usize, candidate: &CandidateForMemoGroup, ids: &CanonicalIds) -> Value {
    let operator = &candidate.expression.operator;
    let group_ids = |groups: Vec<&MemoGroupId>| {
        groups
            .into_iter()
            .map(|g| ids.group(*g))
            .collect::<Vec<_>>()
    };
    json!({
        "index": index,
        "origin": origin_name(candidate),
        "added": false,
        "label": operator_label(operator),
        "operator": operator.map_children(|child| ids.group(*child)),
        "children": group_ids(operator.children()),
        "fused_groups": group_ids(candidate.fused_groups.iter().collect()),
        "parameters": candidate.logical_properties.parameters(),
        "required_accuracy": candidate
            .input_requirements
            .iter()
            .map(|requirement| requirement.required_accuracy)
            .collect::<Vec<_>>(),
    })
}

/// The plan's sharing summary: each node that two or more queries read,
/// directly or through other nodes, with its `operator`, `parameters` (θ),
/// `timing`, `lifetime` and the `queries` (`q1`, ...). Described by what
/// it computes, not by node id, so it stays the same when other parts of
/// the plan change; sorted by queries, then operator. Example: two
/// quantiles over one metric sharing a scan give `{"operator": "Scan",
/// "queries": ["q1", "q2"], ...}`.
fn sharing_summary_json(plan: &PhysicalASAPDAG) -> Vec<Value> {
    let position: BTreeMap<PhysicalASAPNodeId, usize> = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.id, position))
        .collect();
    let mut queries_of_node: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); plan.nodes.len()];
    for (query, root) in plan.root_nodes_per_query().iter().enumerate() {
        let mut pending: Vec<PhysicalASAPNodeId> =
            root.operator_refs().into_iter().copied().collect();
        while let Some(id) = pending.pop() {
            let node = position[&id];
            if queries_of_node[node].insert(query) {
                pending.extend(plan.nodes[node].payload.children());
            }
        }
    }
    // Sort key: queries, then operator, then the entry's JSON text (computed
    // once per entry, not per comparison).
    let mut shared: Vec<(Vec<usize>, String, String, Value)> = plan
        .nodes
        .iter()
        .zip(queries_of_node)
        .filter(|(_, queries)| queries.len() > 1)
        .map(|(node, queries)| {
            let operator = operator_label(&node.payload);
            let names: Vec<String> = queries.iter().map(|&q| query_id(q)).collect();
            let entry = json!({
                "operator": operator,
                "parameters": node.parameters,
                "timing": node.output_state.timing,
                "lifetime": node.lifetime,
                "queries": names,
            });
            let text = entry.to_string();
            (queries.into_iter().collect(), operator, text, entry)
        })
        .collect();
    shared.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    shared.into_iter().map(|(_, _, _, entry)| entry).collect()
}

/// The workload query id the documents use for query index `index`
/// (0-based): `0` gives `"q1"`, as in `workload.queries[].id`.
fn query_id(index: usize) -> String {
    format!("q{}", index + 1)
}

/// A candidate's origin, as `OriginOfCandidateForMemoGroup` serializes.
fn origin_name(candidate: &CandidateForMemoGroup) -> String {
    json!(candidate.origin)
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When Pass 1 fails, the document still carries the Stage 0 DAG, with
    /// the error under Stage 1 and no Stage 2 or Stage 3 section, so the
    /// viewer can show both. (Pass 1 fails only on a rule bug, so no
    /// workload reaches this path end to end.)
    #[test]
    fn a_pass1_failure_still_writes_stage0_and_the_error() {
        let workload: PlanningWorkload = serde_json::from_value(json!({
            "query_workload": {"language": "promql", "query_batch": [{
                "query": "sum(rate(m[5m]))",
                "requirements": {"accuracy": {"max_error": 0.0}},
                "predictability": "unknown",
                "invocations": 1,
            }]},
        }))
        .unwrap();
        let roots = lower_promql_roots(&workload).unwrap();
        let document = document_of(
            workload_queries(&workload),
            &roots,
            Err(PlanningStagesError::Pass1(
                "a rule built a bad group".into(),
            )),
            RunUntil::Stage3,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            document["stage0_query_language_frontends"]["dag"]["roots"]
                .as_array()
                .map(Vec::len),
            Some(1),
            "{document}"
        );
        assert_eq!(
            document["stage1_logical_asap_optimization"]["error"],
            "Pass 1: a rule built a bad group"
        );
        assert_eq!(document["stage2_physical_asap_optimization"], Value::Null);
        assert_eq!(document["stage3_plan_selection"], Value::Null);
    }
}
