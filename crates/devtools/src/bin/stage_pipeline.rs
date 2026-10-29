// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --exhaustive --enforce-accuracy-guarantees \
//     --max-displayed 100 --out planner-layering-example1.json
// (also planner-layering-2: #509 Example 2, three SQL flow statistics;
// planner-layering-3a and planner-layering-3b: #509 Example 3, Patterns A
// and B; 3a has 2,187 physical candidates, all priced, 64 shown by default)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --workload dashboard.json --out run.json
// (a `PlanningWorkload` JSON file, e.g. with a repeating query's
// `"range_evaluation": {"step_ms": 60000, "span_ms": 3600000}`; PromQL, or
// SQL over `--table` declarations)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --workload promql.json --workload sql.json --table '<json>' --out run.json
// (`--workload` repeats: each file keeps its one language and is lowered by
// its own frontend, and their queries are planned as one workload; see
// below)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --max-error 0.01 --failure-probability 0.001 --out run.json
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --table '{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"},
//               {"name": "service", "type": "utf8"}], "time_index": 0}' \
//     --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages, using the joint candidate-connection engine. The default
// document keeps the selected plan; `--exhaustive` archives evaluated plans
// and rejections from the same finite catalog, not an independent optimizer.
// `--search candidate-connections` explicitly names this engine.
// `--milp-time-limit-s` bounds planning in either mode; a timeout reports a
// partial result, never an optimality claim for unexplored candidates.
// Unlike `plan_installable`'s default settings, Stage 3 ignores accuracy
// guarantees unless `--enforce-accuracy-guarantees` is given (below);
// `accuracy_guarantees` is `"ignored"` or `"enforced"`.
//   - stage0_query_language_frontends: the frontends' workload DAG, one root per query;
//   - stage1_logical_asap_optimization: Pass 1 whole-workload candidates, one per
//     choice of a per-computation candidate for every target (the Cartesian
//     product; under the default search, the choices it composes), each
//     followed by its Pass 2 candidates (the
//     identical-expression rule's, one per subset of its sharing candidates,
//     then the summary-capability rule's, one per choice of shared summary
//     builds), in enumeration order. Every candidate is optimized and
//     priced; the document shows the Stage 1 candidates of the physical
//     candidates it shows (below), and those among the first
//     `--max-displayed` that have none, with `combinations` (how many were enumerated),
//     `capped` (whether some are not shown) and `pass1_pruned` (the
//     candidates Pass 1 pruned per target, with reasons). A candidate that cannot be
//     composed carries `error` instead of `dag`;
//   - stage2_physical_asap_optimization: one physical candidate per composed logical
//     candidate (operator implementation; every sub-DAG NotMaterialized), no
//     cost; a composed logical candidate Stage 2 cannot implement is listed
//     under `pruned` with the reason. The document shows at most
//     `--max-displayed` (default 64) physical candidates: the selected one,
//     the other valid ones cheapest first, then the rejected ones taking
//     each cause in turn;
//   - stage3_plan_selection: the selected candidate, chosen among all candidates;
//     the shown candidates' costs and accepted accuracy (per query: the
//     target's metric or null when not set, the accepted guarantee's
//     metric or null when exact or accepted regardless of guarantee,
//     whether the deployment's accuracy model accepted it directly,
//     whatever its guarantee says (`accepted_regardless_of_guarantee`),
//     the cause the guarantee check would have rejected it with when it was
//     accepted only so (`guarantee_check_rejection`, e.g.
//     `no_accuracy_guarantee`; null otherwise),
//     the sketches read and a one-line
//     summary), and every other shown candidate under `rejected` with
//     its `cause`: a failed check (`failure_probability_resize_failed`,
//     `accuracy_target_missed`, `count_min_weights_not_proven_non_negative`,
//     `per_entity_aggregate_not_executable`, `no_accuracy_guarantee`,
//     `raw_data_not_retained`,
//     `latency_bound_exceeded`, `latency_over_repeat_interval`,
//     `over_cluster_capacity`) or a loss under
//     the cost model (`costlier`, `slower`, `more_memory`,
//     `not_lowest_latency_on_pareto_front`, `dominated`,
//     `tied_lost_on_enumeration_order`). Each node's `cost` is one
//     computation's price times `multiplier`, the times it is computed:
//     once if materialized, else once per query (or materialized node)
//     that reads it, so a shared NotMaterialized node is charged once per
//     query. `search` is the search's report (`SearchReport`: the physical
//     candidates checked and, per query under the default search, its own
//     candidates checked and those discarded with why), and
//     `rejected_by_cause` counts the rejections by cause (every candidate's
//     under `--exhaustive`; the recorded ones under the default search).
//     `materialized_views` are the selected plan's materialized views as
//     `plan_installable`'s plan derives them (`installable_plan_of`, then
//     `InstallablePlan::materialized_views`: each view's installed node id,
//     `refresh`, `coverage` with its `definition_id` and `selection`,
//     `inputs` and `retention_ms`), each with the node of the
//     selected Stage 2 candidate it is installed from (`stage2_node_id`), its
//     position among that node's installed nodes (`installed_position`, a
//     pane chain's panes newest first) and their number
//     (`installed_count`); empty without a selection, with
//     `materialized_views_error` when the plan is not installable. When selection
//     fails, `selected` is null, with the rejections (and under
//     `search_rejected` the search's own, which name no candidate: e.g. a
//     query whose every own candidate was discarded), or with `error` for
//     any other failure. `pane_retention_ms` maps each
//     pane chain the selected plan keeps beyond one evaluation (by node id)
//     to how far back from the newest evaluation the runtime must keep its
//     panes: the window plus the longest range-evaluation span reading it.
//     `warnings` are one-line warnings about the workload and the selected
//     plan: a PromQL query applying `rate`, `increase` or `irate` to a
//     metric the data workload declares a gauge, prefixed with its id (the
//     query still plans; `counter_function_over_gauge_warnings`), and the
//     plan's peak memory when over 10^12 bytes and the cost model declares
//     no memory capacity (`large_peak_memory_warning`); absent when there
//     are none. Each node's `per_node` entry also has its `operator`,
//     `timing` and predicted CPU (`cpu_core_seconds_per_second`), and each
//     shown candidate's `cpu_core_seconds_per_second` is their sum: report
//     only, as `plan_installable`'s predicted usage names them.
// `memo` (unless `--no-memo`) is the memo the search built, from the
// planning round whose plan was selected (`MemoSteps`): `nodes`, one node
// table for the whole section (`MemoNodes`: each distinct node once, matched
// by its serialized JSON, children as indices into the table and before
// their parents); `steps`, one per
// memo step in pipeline order (`stage0`: the frontends' DAG hash-consed,
// groups without candidates; `pass1`: Pass 1's candidates, with the
// accuracy-scoped sizes; `pass2`: local window composition's pane
// candidates; `concrete_states`: the Pass 2 memo rules (scan labels,
// summary inputs, top-k reuse, finer aggregates, rollups, summary results,
// state configurations) and the concrete summary-state groups, the memo the
// search selects from). Each step lists its groups (`id`, `label`,
// `children`, `parents`, `queries`, `roots_of`, the group's `node` in
// `nodes`) and their
// candidates (`candidate_count`, `candidate_count_by_origin`, and the first
// `--max-memo-candidates` (default 64) under `candidates`, with
// `candidates_capped`): each with its `index` in the group, `implements`,
// `children`, `origin` (`CandidateOrigin` in snake case), `budget_share`,
// `added` (not in the group at the step before), a one-line `label`, its
// `node` in `nodes` and `group_of_node` (the `nodes` indices below it that
// are a child group's node, to that group's id).
// `selected_choice` maps each group of the last step the selected plan uses
// (by id, as a string) to its candidate's index; it is null, with
// `selected_choice_unavailable` saying why, when the plan cannot be read as
// one candidate per group. `enumeration` counts each step's candidate DAGs
// (`memo::enumeration::enumerate`) up to `--memo-enumeration-limit`
// (default 1000) under `per_step` (`count`, `complete`; Stage 0 counts its
// one DAG), and lists the first `--max-enumerated` (default 200) `choices`
// of `step`: the last step enumerated completely within that many, else
// the last step. Each choice has its `id` (`E1`, ...), `choice`, `roots`
// (`memo::enumeration::compose`'s roots, one per root group, as indices
// into `nodes`; null with `error` when it does not
// compose), `budget_share_1` (every chosen candidate sized for the whole
// budget) and `selected` (it is `selected_choice`). With
// `--price-enumerated <n>` (off by default), each of the first `n` listed
// choices is also run through Stage 2 and Stage 3 as the selected plan is,
// and its selected plan installed (`memo_pricing::price_choices`, under the
// run's models): `valid`, `rejection` (a Stage 3 rejection cause, or
// `compose_failed`, `invalid_structure`, `stage2_failed`, `stage3_failed`,
// `rejected`, `not_executable` (the executor cannot compile the selected
// plan), `not_installable` (installation refuses it otherwise)) with
// `rejection_reason`,
// `cost_per_second` (its cheapest valid physical plan's) and
// `cheaper_than_selected` (valid and cheaper than the selected plan); and
// `priced` is `{count, cheaper_than_selected, selected_cost_per_second}`.
// Without it `priced` is null.
// `workload.data_workload` is the `DataWorkload` the run planned with, as
// declared (`--workload`, `--example`) or built from `--sample-interval-ms`
// (`--promql`); each query also carries its `recurrence` and
// `predictability` as its workload entry declares them.
// When Pass 1 fails, the document still has the Stage 0 DAG; Stage 1 has
// `error` and no candidates, and nothing is selected.
//
// Under `--exhaustive`, `--max-combinations` (default 10^4) bounds evaluated
// assignments. The report distinguishes catalog exhaustion from budget limits.
// Display truncation (`--max-displayed`) is separate from the planning budgets.
//
// `--workload` may repeat, so one run plans queries in several languages
// (e.g. a PromQL file and a SQL file) together. Each file is lowered by
// its own language's frontend (SQL over the `--table` declarations), the
// queries are numbered `q1..qN` across the files in the order given, and
// the data workload is the union of the files' sources. A source several
// files declare must have the same facts in each, and the files must
// declare the same kind of arrival: ingesting (or mixed) arrivals add
// their `rows_per_second`. Plan selection shares a workload's
// `rows_per_second` out over all its sources, so when the data arrives,
// every source of several files must declare its own rate
// (`series_count` and `sample_interval`). A disagreement or a source
// without its own rate is refused.
// With `--workload`, each query's accuracy (ε, δ, metric) comes from the
// file: `--max-error`, `--failure-probability` and `--accuracy-metric` apply
// to `--promql`/`--sql` queries only.
// `--promql` and `--sql` may repeat and mix, in workload order; `--name`
// names the query before it (default `q<n>`). `--max-error` (ε) and
// `--failure-probability` (δ) apply to every query; without them the
// queries are exact. `--accuracy-metric` (`absolute`, `absolute_stream_l1`, `absolute_stream_l2`,
// `relative` or `rank`) sets the error metric of that target; without it,
// a candidate is accepted in whatever metric its guarantee meets ε (and δ)
// in, and `stage3_plan_selection.accepted` reports, per shown valid
// candidate and query, the metric accepted (e.g. "metric not set; accepted
// rank (Kll)").
// `--cost-model` chooses how plan selection compares valid candidates:
// `usage-over-time` (cost model 1, the default: CPU, scanned bytes and
// memory byte-seconds per second, priced), `provisioned-capacity` (cost
// model 2: peak memory, peak CPU cores and disk, priced) or
// `cluster-capacity` (cost model 3: no prices; `--cluster-memory-bytes`,
// `--cluster-disk-bytes` and the optional `--cluster-cpu-cores` are hard
// limits, and `--cluster-objective` is `lowest-latency`, `least-memory` or
// `pareto-front`). Cost models 1 and 2 use the illustrative unit costs;
// `aws-fargate-us-east-1` is cost model 1 at AWS Fargate's us-east-1
// Linux/x86 prices (`ResourceUnitCost::aws_fargate_us_east_1`, in US dollars
// per second).
// Each shown candidate's `usage` lists what it uses; `cost_per_second` is null under
// cost model 3, and `front` lists the Pareto front under `pareto-front`.
// `--deployment-capabilities '<json>'` declares what the deployment's
// executor can run and keep (`DeploymentCapabilities`):
// `{"supported_summaries": [{"family": "Kll", "readouts": ["Quantile"]}, ...],
// "raw_data_retained": true, "query_time_results_retained": false}`. Every
// field is optional: by default the sketch families and readouts the
// Planner's executor runs are supported (`supported_summaries` null, as
// `plan_installable` defaults it), raw data is retained,
// and no query-time result is kept across evaluations. Pass 1 offers no
// summary the deployment does not support (exact computations are always
// supported); Stage 1's `pass1_pruned` lists, per target with any, every
// candidate Pass 1 pruned and why (e.g. `UnsupportedByDeployment`), its fused
// computation's under `fused_pruned`. Without raw data retained, a candidate
// that scans raw data at query time is invalid, and stored data (at rest or
// mixed) is refused as a contradiction; with `query_time_results_retained`,
// a tumbling-pane chain whose readers evaluate together at its pane width is
// also offered built at query time and kept across evaluations, beside being
// shared within one evaluation. The document's top-level
// `deployment_capabilities` shows the capabilities used, with the
// executor's families filled in.
// `--measured-operation-costs <file>` reads the deployment's measured CPU
// time per operator (`MeasuredOperationCosts`: `{"operators": [...],
// "ns_per_evaluation": ..., "provenance": ...}`, or the earlier JSON array
// `convert_rqe_atomic_costs` writes from sketch-bench's
// `rqe_atomic_costs.json`); a node with an entry is priced from it, the
// others from the formulas times the calibration factor, and each node's
// `price_source` says `measured` or `analytical`.
// `--plan-selection-settings <file>` reads all of Stage 3's settings from
// one JSON file (`PlanSelectionSettings`: `cost_model`, e.g.
// `{"usage_over_time": {"cost_per_cpu_op": 1e-6, ...}}` with the
// deployment's own unit costs, `resource_speed`,
// `non_repeating_query_horizon_seconds` and `measured_operation_costs`;
// an omitted field takes its default). It replaces `--cost-model`, the
// `--cluster-*` flags and `--measured-operation-costs`.
// Accuracy guarantees are ignored by default: stage_pipeline sets the
// settings' `ignore_accuracy_guarantees` (also over
// `--plan-selection-settings`), so Stage 3 accepts every query output
// regardless of its accuracy guarantee and candidates are compared on cost
// alone; executability checks still apply. `--enforce-accuracy-guarantees`
// clears it, and Stage 3 rejects an output whose guarantee does not meet
// its query's target, as `plan_installable` does with the default
// `PlanSelectionSettings`. The document's top-level `accuracy_guarantees` is
// `"ignored"` or `"enforced"`.
// `--sample-interval-ms` is the sample interval declared for every metric the
// PromQL queries read (default 15000). `--series-count <n>`,
// `--rows-per-second <f64>` (the data then ingests at that rate) and
// `--rows-held <n>` declare those facts for every source the queries read
// (metric or `--table`); nothing else is declared. Plan selection prices
// only from declared facts: a scan whose source has no arrival rate
// (`--rows-per-second`, or `--series-count` with the sample interval) and
// no `--rows-held` fails, naming the source and the missing fact.
// `--table '<json>'` (repeatable) declares a table the SQL queries read: `{"name": ...,
// "columns": [{"name": ..., "type": "timestamp|utf8|string|float64|double|
// int64|bigint", "nullable": true}], "time_index": 0}` (`nullable` defaults
// to true; without `time_index` the table has no time column; a
// `time_index` that is not a column index is an error).

use std::collections::btree_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_devtools::candidate_labels::{candidate_labels, computation_label, query_of_each_target};
use asap_devtools::memo_pricing::{price_choices, SelectedBaseline};
use asap_devtools::planner_layering_examples::{
    example1, example2, pattern_a, pattern_b, PatternARecurrence, SHARED_ROWS_PER_SECOND,
};
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_search_over, sql_catalog, MemoSteps,
    PlanningStagesError, PlanningStagesRun,
};
use asap_devtools::SqlCatalog;
use asap_frontend_promql::{counter_function_over_gauge_warnings, promql_metric_names};
use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    ComputationCandidate, PerComputationCandidates, PruneReason,
};
use asap_logical_asap_optimization::pass2::memo::candidates::{CandidateMemo, CandidateOrigin};
use asap_logical_asap_optimization::pass2::memo::enumeration::{compose, enumerate, MemoChoice};
use asap_logical_asap_optimization::pass2::memo::MemoGroupId;
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::{
    large_peak_memory_warning, ClusterLimits, ClusterObjective, CostModel, MeasuredOperationCosts,
    PlanSelectionSettings, PlanningModels, Rejection, ResourceUnitCost, ResourceUsage,
    SearchAlgorithm, Selection, SelectionError, Stage1ToStage3Search,
};
use asap_planner::installable_plan_of;
use asap_types::ir::flat::{flatten, FlatDag, FlatNode, NodeId};
use asap_types::ir::operator::node::OperatorNode;
use asap_types::ir::properties::{default_metric_for_root, AccuracyMetric, AccuracyTarget};
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    BatchEntry, DataArrival, DataWorkload, DurationMs, PlanningWorkload, Query, QueryLanguage,
    QueryRecurrence, QueryRequirements, QueryWorkload, QueryWorkloadEntry, RepeatedDemand,
    SqlDialect,
};
use serde_json::{json, Value};

const USAGE: &str =
    "usage: stage_pipeline (--example planner-layering-{1,2,3a,3b} | (--workload <file>)... [--table <json>...] | ((--promql | --sql) <query> \
[--name <name>]... [--table <json>...] [--max-error <f64> --failure-probability <f64> --accuracy-metric <metric>] [--sample-interval-ms <u64>] [--series-count <u64>] [--rows-per-second <f64>] [--rows-held <u64>])) \
[--cost-model usage-over-time|aws-fargate-us-east-1|provisioned-capacity|cluster-capacity [--cluster-memory-bytes <n> \
--cluster-disk-bytes <n> [--cluster-cpu-cores <n>] --cluster-objective lowest-latency|least-memory|pareto-front]] \
[--plan-selection-settings <file>] [--enforce-accuracy-guarantees] [--deployment-capabilities <json>] [--exhaustive [--max-combinations <n>] | --search candidate-connections] [--milp-time-limit-s <f64>] [--max-displayed <n>] \
[--no-memo] [--max-memo-candidates <n>] [--memo-enumeration-limit <n>] [--max-enumerated <n>] [--price-enumerated <n>] --out <file>\n\
--search candidate-connections is the default search.";

/// Default evaluator-assignment budget for an exhaustive archive run.
const MAX_COMBINATIONS: usize = 10_000;

/// Why a run wrote no document.
enum Failure {
    /// Bad arguments or a query that does not plan (exit status 2).
    Usage(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Usage(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Failure::Usage(message.into())
    }
}

fn main() {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => {}
        Err(Failure::Usage(message)) => {
            eprintln!("stage_pipeline: {message}\n{USAGE}");
            std::process::exit(2);
        }
    }
}

/// The document for a run that ended with `result` over the frontends'
/// `roots`. A Pass 1 failure still gives a document: the Stage 0 DAG, the
/// error under Stage 1 and no selection. Refusing too many combinations or
/// a failed validation gives none.
fn document_of(
    queries: Vec<Value>,
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    result: Result<PlanningStagesRun, PlanningStagesError>,
    max_displayed: usize,
) -> Result<Value, Failure> {
    let queries = with_default_metrics(queries, roots);
    match result {
        Ok(run) => Ok(stage_pipeline(queries, entries, &run, max_displayed)),
        Err(PlanningStagesError::Pass1(error)) => Ok(json!({
            "format": "asap-stage-pipeline/v1",
            "workload": { "queries": queries },
            "stage0_query_language_frontends": { "dag": export(roots) },
            "stage1_logical_asap_optimization": {
                "combinations": 0,
                "capped": false,
                "pass1_pruned": [],
                "candidates": [],
                "error": format!("Pass 1: {error}"),
            },
            "stage2_physical_asap_optimization": { "candidates": [], "pruned": [] },
            "stage3_plan_selection": { "costs": {}, "selected": null, "rejected": [], "materialized_views": [] },
        })),
        Err(error @ PlanningStagesError::Validation(_)) => Err(error.to_string().into()),
    }
}

fn run(args: Vec<String>) -> Result<(), Failure> {
    let mut example = None;
    // `--workload` files, planned together in this order.
    let mut workload_paths: Vec<String> = Vec::new();
    // (language, text, name) in workload order.
    let mut queries: Vec<(QueryLanguage, String, Option<String>)> = Vec::new();
    let mut tables = Vec::new();
    let (mut max_error, mut failure_probability, mut sample_interval_ms) = (None, None, 15_000u64);
    // Facts declared for every source the --promql/--sql queries read.
    let (mut series_count, mut rows_per_second, mut rows_held) =
        (None::<u64>, None::<f64>, None::<u64>);
    let mut accuracy_metric: Option<AccuracyMetric> = None;
    let mut max_displayed = 64usize;
    let mut max_combinations = MAX_COMBINATIONS;
    let mut cost_model: Option<String> = None;
    let (mut cluster_memory, mut cluster_disk, mut cluster_cores) = (None, None, None);
    let mut cluster_objective: Option<String> = None;
    let mut deployment_capabilities = DeploymentCapabilities::default();
    let mut out = None;
    let mut measured_operation_costs_path: Option<String> = None;
    let mut plan_selection_settings_path: Option<String> = None;
    let mut enforce_accuracy_guarantees = false;
    let mut exhaustive = false;
    // `--search` explicitly names the candidate-connection engine;
    // `--milp-time-limit-s` limits the MILP search.
    let mut search_algorithm: Option<SearchAlgorithm> = None;
    let mut milp_time_limit_ms: Option<u64> = None;
    let mut max_combinations_given = false;
    let mut write_memo = true;
    let mut memo_caps = MemoCaps {
        candidates_per_group: 64,
        enumeration_limit: 1000,
        enumerated: 200,
        choices_to_price: 0,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{v}: {e}"));
        match flag.as_str() {
            "--example" => example = Some(value()?),
            "--workload" => workload_paths.push(value()?),
            "--promql" => queries.push((QueryLanguage::PromQL, value()?, None)),
            "--sql" => queries.push((
                QueryLanguage::SQL(SqlDialect::DataFusionSQL),
                value()?,
                None,
            )),
            "--name" => {
                let name = value()?;
                match queries.last_mut() {
                    Some((_, _, slot @ None)) => *slot = Some(name),
                    _ => return Err("--name follows a --promql or --sql query".into()),
                }
            }
            "--table" => tables.push(value()?),
            "--max-error" => max_error = Some(number(value()?)?),
            "--failure-probability" => failure_probability = Some(number(value()?)?),
            "--accuracy-metric" => {
                let name = value()?;
                accuracy_metric = Some(
                    serde_json::from_value(Value::String(name.clone()))
                        .map_err(|_| format!("unknown metric {name}"))?,
                )
            }
            "--sample-interval-ms" => {
                sample_interval_ms = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--series-count" => series_count = Some(value()?.parse().map_err(|e| format!("{e}"))?),
            "--rows-per-second" => rows_per_second = Some(number(value()?)?),
            "--rows-held" => rows_held = Some(value()?.parse().map_err(|e| format!("{e}"))?),
            "--max-displayed" => max_displayed = value()?.parse().map_err(|e| format!("{e}"))?,
            "--max-combinations" => {
                max_combinations = value()?.parse().map_err(|e| format!("{e}"))?;
                max_combinations_given = true;
            }
            "--cost-model" => cost_model = Some(value()?),
            "--cluster-memory-bytes" => {
                cluster_memory = Some(value()?.parse::<u64>().map_err(|e| format!("{e}"))?)
            }
            "--cluster-disk-bytes" => {
                cluster_disk = Some(value()?.parse::<u64>().map_err(|e| format!("{e}"))?)
            }
            "--cluster-cpu-cores" => {
                cluster_cores = Some(value()?.parse::<u32>().map_err(|e| format!("{e}"))?)
            }
            "--cluster-objective" => cluster_objective = Some(value()?),
            "--deployment-capabilities" => {
                deployment_capabilities = serde_json::from_str(&value()?)
                    .map_err(|e| format!("--deployment-capabilities: {e}"))?
            }
            "--out" => out = Some(value()?),
            "--measured-operation-costs" => measured_operation_costs_path = Some(value()?),
            "--plan-selection-settings" => plan_selection_settings_path = Some(value()?),
            "--enforce-accuracy-guarantees" => enforce_accuracy_guarantees = true,
            "--exhaustive" => exhaustive = true,
            "--no-memo" => write_memo = false,
            "--max-memo-candidates" => {
                memo_caps.candidates_per_group = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--memo-enumeration-limit" => {
                memo_caps.enumeration_limit = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--max-enumerated" => {
                memo_caps.enumerated = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--price-enumerated" => {
                memo_caps.choices_to_price = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--search" => {
                search_algorithm = Some(match value()?.as_str() {
                    "candidate-connections" => SearchAlgorithm::CandidateConnections,
                    other => {
                        return Err(
                            format!("--search {other}: expected candidate-connections").into()
                        )
                    }
                })
            }
            "--milp-time-limit-s" => {
                let seconds: f64 = value()?
                    .parse()
                    .map_err(|e| format!("--milp-time-limit-s: {e}"))?;
                milp_time_limit_ms = Some((seconds * 1e3) as u64);
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let has_sql = queries
        .iter()
        .any(|(language, _, _)| *language != QueryLanguage::PromQL);
    if !tables.is_empty() && !has_sql && workload_paths.is_empty() {
        return Err("--table needs --sql".into());
    }
    let out = out.ok_or("--out is required")?;
    if (series_count.is_some() || rows_per_second.is_some() || rows_held.is_some())
        && (!workload_paths.is_empty() || example.is_some())
    {
        return Err(
            "--series-count, --rows-per-second and --rows-held declare facts for --promql/--sql \
             queries; a --workload or --example declares its own"
                .into(),
        );
    }
    if max_combinations_given && !exhaustive {
        return Err("--max-combinations limits the exhaustive search: it needs --exhaustive (bounds evaluated assignments)".into());
    }
    if exhaustive && search_algorithm.is_some() {
        return Err(
            "--search picks the default search's algorithm: drop it with --exhaustive".into(),
        );
    }
    let search_algorithm = search_algorithm.unwrap_or(SearchAlgorithm::CandidateConnections);
    if plan_selection_settings_path.is_some()
        && (cost_model.is_some()
            || cluster_memory.is_some()
            || cluster_disk.is_some()
            || cluster_cores.is_some()
            || cluster_objective.is_some()
            || measured_operation_costs_path.is_some())
    {
        return Err(
            "--plan-selection-settings replaces --cost-model, the --cluster-* flags \
                    and --measured-operation-costs"
                .into(),
        );
    }
    let cluster = (cluster_memory, cluster_disk, cluster_objective.as_deref());
    // The settings every flag below leaves at its default; the Fargate
    // preset converts its prices at their `resource_speed`.
    let default_settings = PlanSelectionSettings::default();
    let cost_model = match (cost_model.as_deref(), cluster) {
        (None | Some("usage-over-time"), (None, None, None)) if cluster_cores.is_none() => {
            CostModel::UsageOverTime(ResourceUnitCost::illustrative())
        }
        (Some("aws-fargate-us-east-1"), (None, None, None)) if cluster_cores.is_none() => {
            CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
                default_settings.resource_speed,
            ))
        }
        (Some("provisioned-capacity"), (None, None, None)) if cluster_cores.is_none() => {
            CostModel::ProvisionedCapacity(ResourceUnitCost::illustrative())
        }
        (Some("cluster-capacity"), (Some(memory_bytes), Some(disk_bytes), Some(objective))) => {
            CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes,
                disk_bytes,
                cpu_cores: cluster_cores,
                objective: match objective {
                    "lowest-latency" => ClusterObjective::LowestLatency,
                    "least-memory" => ClusterObjective::LeastMemory,
                    "pareto-front" => ClusterObjective::ParetoFront,
                    other => return Err(format!("unknown cluster objective {other}").into()),
                },
            })
        }
        (Some("cluster-capacity"), _) => {
            return Err(
                "--cost-model cluster-capacity needs --cluster-memory-bytes, \
                 --cluster-disk-bytes and --cluster-objective"
                    .into(),
            )
        }
        (None | Some("usage-over-time" | "aws-fargate-us-east-1" | "provisioned-capacity"), _) => {
            return Err("--cluster-* flags need --cost-model cluster-capacity".into())
        }
        (Some(other), _) => return Err(format!("unknown cost model {other}").into()),
    };
    let measured_operation_costs: MeasuredOperationCosts = match &measured_operation_costs_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("--measured-operation-costs {path}: {e}"))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("--measured-operation-costs {path}: {e}"))?
        }
        None => MeasuredOperationCosts::default(),
    };
    let mut plan_selection: PlanSelectionSettings = match &plan_selection_settings_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("--plan-selection-settings {path}: {e}"))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("--plan-selection-settings {path}: {e}"))?
        }
        None => PlanSelectionSettings {
            cost_model,
            measured_operation_costs,
            ..default_settings
        },
    };
    plan_selection.ignore_accuracy_guarantees = !enforce_accuracy_guarantees;
    if memo_caps.choices_to_price > 0 {
        if !write_memo {
            return Err("--price-enumerated needs the memo (drop --no-memo)".into());
        }
        // The exhaustive search reports no calibration factor to price each
        // choice on the selection's scale.
        if exhaustive
            && plan_selection.frozen_calibration_factor.is_none()
            && !plan_selection.measured_operation_costs.operators.is_empty()
        {
            return Err(
                "--price-enumerated with --exhaustive needs a frozen_calibration_factor \
                        when measured operation costs are given"
                    .into(),
            );
        }
    }
    let from_files = if workload_paths.is_empty() {
        None
    } else if example.is_none() && queries.is_empty() {
        let mut workloads = Vec::new();
        for path in &workload_paths {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("--workload {path}: {e}"))?;
            let workload: PlanningWorkload =
                serde_json::from_str(&text).map_err(|e| format!("--workload {path}: {e}"))?;
            workload
                .validate()
                .map_err(|e| format!("--workload {path}: {e}"))?;
            workloads.push((path.as_str(), workload));
        }
        Some((workloads, sql_catalog(&tables)?))
    } else {
        return Err("give exactly one of --example, --workload or --promql/--sql queries".into());
    };
    // The workload's queries as the document describes them, the
    // frontends' roots, the workload entries and the data workload.
    let (described, roots, entries, data) = match (example.as_deref(), queries.is_empty()) {
        (None, true) if from_files.is_some() => {
            let (workloads, catalog) = from_files.expect("checked");
            lowered_together(&workloads, &catalog, &catalog_table_names(&tables)?)?
        }
        (Some(example), true) => {
            let (workload, catalog) = match example {
                "planner-layering-1" => (example1(), SqlCatalog::new()),
                "planner-layering-2" => example2(),
                "planner-layering-3a" => (
                    pattern_a(
                        PatternARecurrence::OnceAdHoc,
                        DataArrival::Mixed {
                            rows_per_second: SHARED_ROWS_PER_SECOND,
                        },
                    ),
                    SqlCatalog::new(),
                ),
                "planner-layering-3b" => (pattern_b(), SqlCatalog::new()),
                other => return Err(format!("unknown example {other}").into()),
            };
            lowered(&workload, &catalog)?
        }
        (None, false) => {
            let accuracy = match (max_error, failure_probability) {
                (None, None) => AccuracyTarget::exact(),
                (Some(max_error), None) => AccuracyTarget::max_error(max_error),
                (Some(max_error), Some(failure_probability)) => {
                    AccuracyTarget::max_error(max_error).failure_probability(failure_probability)
                }
                (None, Some(_)) => return Err("--failure-probability needs --max-error".into()),
            };
            let accuracy = match accuracy_metric {
                Some(_) if max_error.is_none() => {
                    return Err("--accuracy-metric needs --max-error".into())
                }
                Some(metric) => accuracy.accuracy_metric(metric),
                None => accuracy,
            };
            let catalog = sql_catalog(&tables)?;
            let mut names = HashSet::new();
            let (mut described, mut roots, mut entries) = (Vec::new(), Vec::new(), Vec::new());
            // Every metric the PromQL queries read, each sampled every
            // `--sample-interval-ms`.
            let mut metrics = BTreeSet::new();
            for (index, (language, text, name)) in queries.into_iter().enumerate() {
                let (name, defaulted) = match name {
                    Some(name) => (name, false),
                    None => (format!("q{}", index + 1), true),
                };
                if !names.insert(name.clone()) {
                    return Err(if defaulted {
                        format!(
                            "duplicate query name {name} (the default name of query {})",
                            index + 1
                        )
                    } else {
                        format!("duplicate query name {name}")
                    }
                    .into());
                }
                // Each query lowers in its own language; planning sees one workload.
                let one = query_batch(
                    language.clone(),
                    &[text],
                    accuracy.clone(),
                    sample_interval_ms,
                );
                roots.extend(match language {
                    QueryLanguage::PromQL => lower_promql_roots(&one)?,
                    _ => lower_sql_roots(&one, &catalog)?,
                });
                entries.extend(one.query_workload.entries());
                metrics.extend(
                    one.data_workload
                        .iter()
                        .flat_map(|d| d.sources.keys().cloned()),
                );
                let mut query = workload_queries(&one).remove(0);
                query["id"] = json!(name);
                described.push(query);
            }
            let mut data = DataWorkload::sampled_every(metrics, DurationMs(sample_interval_ms));
            for table in catalog_table_names(&tables)? {
                data.sources.entry(table).or_default();
            }
            for facts in data.sources.values_mut() {
                facts.series_count = series_count;
                facts.rows_held = rows_held;
            }
            if let Some(rows_per_second) = rows_per_second {
                data.arrival = DataArrival::Ingesting { rows_per_second };
            }
            data.validate().map_err(|e| e.to_string())?;
            (described, roots, entries, data)
        }
        _ => {
            return Err(
                "give exactly one of --example, --workload or --promql/--sql queries".into(),
            )
        }
    };
    // Plan with the executor's summary families when the deployment declares none.
    if deployment_capabilities.supported_summaries.is_none() {
        deployment_capabilities.supported_summaries =
            Some(asap_executor::supported_summary_families());
    }
    let models = PlanningModels::builtin()
        .with_plan_selection(&plan_selection)
        .with_deployment_capabilities(&deployment_capabilities);
    let result = run_search_over(
        roots.clone(),
        &entries,
        &data,
        models,
        Stage1ToStage3Search {
            search: search_algorithm,
            exhaustive,
            keep_every_candidate: exhaustive,
            max_evaluations: exhaustive.then_some(max_combinations),
            milp_time_limit_ms,
        },
    );
    // A rate, increase or irate over a declared gauge (the query still plans).
    let mut warnings = counter_function_over_gauge_warnings_of(&described, &data)?;
    // A selected plan's very large peak memory, when the deployment
    // declares no memory capacity to check it against.
    warnings.extend(
        result
            .as_ref()
            .ok()
            .and_then(|run| run.stage3_plan_selection.as_ref().ok())
            .and_then(|selection| {
                large_peak_memory_warning(
                    selection.costs[&selection.selected].usage.peak_memory_bytes,
                    &plan_selection.cost_model,
                )
            }),
    );
    let memo = result
        .as_ref()
        .ok()
        .and_then(|run| run.memo_steps.as_ref())
        .filter(|_| write_memo)
        .map(|steps| {
            let pricing = Pricing {
                stage0_roots: &roots,
                entries: &entries,
                data: &data,
                models,
                selected: result
                    .as_ref()
                    .ok()
                    .and_then(|run| run.stage3_plan_selection.as_ref().ok())
                    .map(SelectedBaseline::of),
            };
            memo_json(steps, &memo_caps, &pricing)
        });
    let mut document = document_of(described, &roots, &entries, result, max_displayed)?;
    if let Some(memo) = memo {
        document["memo"] = memo;
    }
    if !warnings.is_empty() {
        document["stage3_plan_selection"]["warnings"] = json!(warnings);
    }
    document["workload"]["data_workload"] =
        serde_json::to_value(&data).map_err(|e| e.to_string())?;
    document["search"] = json!(match (exhaustive, search_algorithm) {
        (true, _) => "exhaustive",
        (false, SearchAlgorithm::CandidateConnections) => "candidate-connections",
    });
    document["accuracy_guarantees"] = json!(if enforce_accuracy_guarantees {
        "enforced"
    } else {
        "ignored"
    });
    // Read-only: the deployment inputs the run planned with.
    document["deployment_capabilities"] =
        serde_json::to_value(&deployment_capabilities).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&out, text).map_err(|e| format!("{out}: {e}").into())
}

/// One warning per PromQL query of `described` (the document's queries)
/// that applies `rate`, `increase` or `irate` to a metric `data` declares a
/// gauge, prefixed with the query's id; the query still plans.
fn counter_function_over_gauge_warnings_of(
    described: &[Value],
    data: &DataWorkload,
) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    for query in described.iter().filter(|q| q["language"] == "promql") {
        let (id, text) = (&query["id"], query["text"].as_str().unwrap_or_default());
        let id = id.as_str().unwrap_or_default();
        warnings.extend(
            counter_function_over_gauge_warnings(text, data)
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
/// SQL, each of `sql_tables` (the `--table` names). Plan selection shares a
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

/// The document for `run`, whose workload queries are `queries`.
fn stage_pipeline(
    queries: Vec<Value>,
    entries: &[QueryWorkloadEntry],
    run: &PlanningStagesRun,
    max_displayed: usize,
) -> Value {
    // The display keeps `max_displayed` physical candidates (plan selection
    // has already ranked all of them): the selected one, the other valid
    // ones cheapest first, then the rejected ones taking each cause in turn,
    // so a cause with many candidates does not hide the others.
    let shown_physical = displayed_physical(run, max_displayed);
    let implemented: HashSet<&str> = run
        .stage2_physical_asap_optimization
        .iter()
        .map(|p| p.from_logical.as_str())
        .collect();
    // The Stage 1 candidates of the shown physical ones, and those among
    // the first `max_displayed` that have none (pruned or not composed).
    let shown: HashSet<&str> = run
        .stage1_logical_asap_optimization
        .iter()
        .enumerate()
        .filter(|(index, c)| {
            run.stage2_physical_asap_optimization
                .iter()
                .any(|p| p.from_logical == c.id && shown_physical.contains(p.id.as_str()))
                || (*index < max_displayed && !implemented.contains(c.id.as_str()))
        })
        .map(|(_, c)| c.id.as_str())
        .collect();
    // Labels are for this document only; planning never reads them.
    let labels = candidate_labels(run);
    // A choice that composition cannot substitute yet is listed with its
    // error rather than ending the run.
    let candidates: Vec<_> = run
        .stage1_logical_asap_optimization
        .iter()
        .filter(|c| shown.contains(c.id.as_str()))
        .map(|c| match &c.roots {
            Ok(roots) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "dag": export(roots) })
            }
            Err(error) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "error": error.to_string() })
            }
        })
        .collect();
    let physical: Vec<_> = run
        .stage2_physical_asap_optimization
        .iter()
        .filter(|p| shown_physical.contains(p.id.as_str()))
        .map(|p| json!({ "id": p.id, "from_logical": p.from_logical, "display_name": labels[&p.id], "dag": p.dag }))
        .collect();
    let pruned: Vec<_> = run
        .stage2_pruned
        .iter()
        .filter(|p| shown.contains(p.from_logical.as_str()))
        .map(|p| json!({ "from_logical": p.from_logical, "display_name": labels[&p.from_logical], "reason": p.reason }))
        .collect();
    let every_physical: HashSet<&str> = run
        .stage2_physical_asap_optimization
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    let mut stage3 = plan_selection_json(
        &run.stage3_plan_selection,
        |id| shown_physical.contains(id),
        |id| every_physical.contains(id),
    );
    match &run.stage3_plan_selection {
        Ok(selection) => match materialized_views_json(selection, entries) {
            Ok(views) => stage3["materialized_views"] = Value::Array(views),
            Err(error) => {
                stage3["materialized_views"] = json!([]);
                stage3["materialized_views_error"] = json!(error);
            }
        },
        Err(_) => stage3["materialized_views"] = json!([]),
    }
    json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": queries },
        "stage0_query_language_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "stage1_logical_asap_optimization": {
            "combinations": run.stage1_logical_asap_optimization.len(),
            "capped": physical.len() < run.stage2_physical_asap_optimization.len()
                || candidates.len() < run.stage1_logical_asap_optimization.len(),
            "pass1_pruned": pass1_pruned_json(&run.stage1_pass1_per_computation),
            "candidates": candidates,
        },
        "stage2_physical_asap_optimization": { "candidates": physical, "pruned": pruned },
        "stage3_plan_selection": stage3,
    })
}

/// The ids of the physical candidates the document shows: at most
/// `max_displayed`, the selected one first, then the other valid ones
/// cheapest first (in candidate order when unpriced), then the rejected
/// ones, one of each cause in turn in candidate order.
fn displayed_physical(run: &PlanningStagesRun, max_displayed: usize) -> HashSet<&str> {
    let every = &run.stage2_physical_asap_optimization;
    let Ok(selection) = &run.stage3_plan_selection else {
        // Without a selection (rejected, or not judged): each cause in turn.
        let causes: Vec<_> = match &run.stage3_plan_selection {
            Err(SelectionError::NoValidCandidate(rejected)) => rejected.iter().collect(),
            _ => Vec::new(),
        };
        let cause_of = |id: &str| causes.iter().find(|r| r.id == id).map(|r| r.cause);
        return round_robin(every.iter().map(|p| (p.id.as_str(), cause_of(&p.id))))
            .take(max_displayed)
            .collect();
    };
    let cause_of = |id: &str| {
        selection
            .rejected
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.cause)
    };
    let mut valid: Vec<&str> = every
        .iter()
        .map(|p| p.id.as_str())
        .filter(|id| *id != selection.selected && selection.costs.contains_key(*id))
        .collect();
    let price = |id: &str| selection.costs[id].cost_per_second.unwrap_or(f64::INFINITY);
    valid.sort_by(|a, b| price(a).total_cmp(&price(b)));
    let rejected = every
        .iter()
        .filter(|p| p.id != selection.selected && !selection.costs.contains_key(&p.id))
        .map(|p| (p.id.as_str(), cause_of(&p.id)));
    std::iter::once(selection.selected.as_str())
        .filter(|id| every.iter().any(|p| p.id == *id))
        .chain(valid)
        .chain(round_robin(rejected))
        // The selected candidate is always shown.
        .take(max_displayed.max(1))
        .collect()
}

/// `candidates` (id, cause) reordered to take one of each cause in turn,
/// each cause's in candidate order, causes in order of first appearance.
fn round_robin<'a, C: PartialEq>(
    candidates: impl Iterator<Item = (&'a str, Option<C>)>,
) -> impl Iterator<Item = &'a str> {
    let mut groups: Vec<(Option<C>, std::collections::VecDeque<&'a str>)> = Vec::new();
    for (id, cause) in candidates {
        match groups.iter_mut().find(|(c, _)| *c == cause) {
            Some((_, ids)) => ids.push_back(id),
            None => groups.push((cause, std::collections::VecDeque::from([id]))),
        }
    }
    std::iter::from_fn(move || {
        let ids: Vec<&'a str> = groups
            .iter_mut()
            .filter_map(|(_, ids)| ids.pop_front())
            .collect();
        (!ids.is_empty()).then_some(ids)
    })
    .flatten()
}

/// The selected plan's materialized views, as the plan
/// [`installable_plan_of`] (what `plan_installable` returns) derives them:
/// each view's node id in the installable DAG, `refresh`, `coverage`,
/// `inputs` and `retention_ms`,
/// plus the node of the selected Stage 2 candidate it is installed from
/// (`stage2_node_id`, null when Stage 3 recorded no cost to map it by), its
/// position among that node's installed nodes (`installed_position`: a
/// pane chain's panes, newest first) and their number (`installed_count`).
fn materialized_views_json(
    selection: &Selection,
    entries: &[QueryWorkloadEntry],
) -> Result<Vec<Value>, String> {
    let (plan, report) = installable_plan_of(&selection.plan, Some(selection), entries)
        .map_err(|e| e.to_string())?;
    let views = plan.materialized_views().map_err(|e| e.to_string())?;
    let mut stage2_of = std::collections::HashMap::new();
    for node in report
        .iter()
        .filter_map(|report| report.predicted_usage.as_ref())
        .flat_map(|usage| &usage.per_node)
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

/// The candidates Pass 1 pruned, per target that has any: the target's
/// index, the query it is listed under, and each pruned candidate of its
/// own (`pruned`) and of its fused computation (`fused_pruned`) with the
/// reason, e.g. `UnsupportedByDeployment`.
fn pass1_pruned_json(inventory: &PerComputationCandidates<usize>) -> Vec<Value> {
    let pruned_json = |pruned: &[(ComputationCandidate, PruneReason)]| -> Vec<Value> {
        pruned
            .iter()
            .map(|(candidate, reason)| {
                json!({ "candidate": computation_label(candidate), "reason": format!("{reason:?}") })
            })
            .collect()
    };
    let query_of_target = query_of_each_target(inventory);
    inventory
        .targets
        .iter()
        .enumerate()
        .filter_map(|(index, target)| {
            let fused_pruned = target.fused.as_ref().map_or(&[][..], |f| &f.pruned[..]);
            if target.choices.pruned.is_empty() && fused_pruned.is_empty() {
                return None;
            }
            Some(json!({
                "target": index,
                "query": format!("Q{}", query_of_target[index] + 1),
                "pruned": pruned_json(&target.choices.pruned),
                "fused_pruned": pruned_json(fused_pruned),
            }))
        })
        .collect()
}

/// Plan selection over every candidate, listing the costs and rejections
/// of the `shown` ones.
fn plan_selection_json(
    selection: &Result<Selection, SelectionError>,
    shown: impl Fn(&str) -> bool,
    is_candidate: impl Fn(&str) -> bool,
) -> Value {
    let rejections_json = |rejected: &[Rejection]| -> Vec<Value> {
        rejections_json(rejected.iter().filter(|r| shown(&r.id)))
    };
    // The rejections of candidates the run did not keep (the default
    // search keeps only the best; it records the first rejection of each
    // cause) and the search's own, which name no candidate: a query whose
    // every own candidate was discarded, or the candidates it skipped.
    let search_rejected = |rejected: &[Rejection]| -> Vec<Value> {
        self::rejections_json(rejected.iter().filter(|r| !is_candidate(&r.id)))
    };
    let selection = match selection {
        Ok(selection) => selection,
        Err(SelectionError::NoValidCandidate(rejected)) => {
            let search_rejected = search_rejected(rejected);
            return json!({
                "costs": {},
                "selected": null,
                "rejected": rejections_json(rejected),
                "search_rejected": search_rejected,
                "rejected_by_cause": rejected_by_cause(rejected),
            });
        }
        Err(error) => {
            let mut output = json!({ "costs": {}, "selected": null, "rejected": [], "error": error.to_string() });
            // A budget stop without an incumbent still carries the search's
            // report in its solver diagnostic. Decode that typed report for
            // the viewer while preserving the complete original error.
            if let SelectionError::Solver(diagnostic) = error {
                if let Some((_, report)) = diagnostic.split_once("Search report: ") {
                    if let Some(Ok(report)) = serde_json::Deserializer::from_str(report)
                        .into_iter::<asap_plan_selection::CandidateConnectionReport>()
                        .next()
                    {
                        output["search"] = json!({ "candidate_connections": report });
                    }
                }
            }
            return output;
        }
    };
    let costs: serde_json::Map<_, _> = selection
        .costs
        .iter()
        .filter(|(id, _)| shown(id))
        .map(|(id, cost)| {
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
            (
                id.clone(),
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
                }),
            )
        })
        .collect();
    let accepted: serde_json::Map<_, _> = selection
        .accepted
        .iter()
        .filter(|(id, _)| shown(id))
        .map(|(id, queries)| {
            let queries: Vec<Value> = queries
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
                .collect();
            (id.clone(), Value::Array(queries))
        })
        .collect();
    json!({
        "costs": costs,
        "accepted": accepted,
        "selected": selection.selected,
        "rejected": rejections_json(&selection.rejected),
        "search_rejected": search_rejected(&selection.rejected),
        "rejected_by_cause": rejected_by_cause(&selection.rejected),
        "front": selection.front,
        "search": selection.search,
        "pane_retention_ms": selection
            .pane_retention_ms
            .iter()
            .map(|(node, ms)| (node.to_string(), json!(ms)))
            .collect::<serde_json::Map<_, _>>(),
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

/// How many of `rejected` have each cause, by cause: over every candidate
/// under `--exhaustive`; under the default search, the rejections it
/// recorded (the first of each cause).
fn rejected_by_cause(rejected: &[Rejection]) -> Value {
    let mut counts = serde_json::Map::new();
    for rejection in rejected {
        let cause = serde_json::to_value(rejection.cause)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let count = counts.entry(cause).or_insert(json!(0));
        *count = json!(count.as_u64().unwrap_or(0) + 1);
    }
    Value::Object(counts)
}

fn rejections_json<'a>(rejected: impl Iterator<Item = &'a Rejection>) -> Vec<Value> {
    rejected
        .map(|r| json!({ "id": r.id, "cause": r.cause, "reason": r.reason }))
        .collect()
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

/// The names of the tables `--table` declares.
fn catalog_table_names(tables: &[String]) -> Result<Vec<String>, String> {
    tables
        .iter()
        .map(|raw| {
            let table: Value = serde_json::from_str(raw).map_err(|e| format!("--table: {e}"))?;
            table["name"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "--table: name must be a string".to_string())
        })
        .collect()
}

fn query_batch(
    language: QueryLanguage,
    queries: &[String],
    accuracy: AccuracyTarget,
    sample_interval_ms: u64,
) -> PlanningWorkload {
    // Each metric a PromQL query reads is sampled every `sample_interval_ms`; a
    // query that does not parse names none and fails when lowered.
    let metrics: BTreeSet<String> = match language {
        QueryLanguage::PromQL => queries
            .iter()
            .flat_map(|query| promql_metric_names(query).unwrap_or_default())
            .collect(),
        _ => BTreeSet::new(),
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language,
            query_batch: Some(
                queries
                    .iter()
                    .map(|query| BatchEntry {
                        query: Query(query.clone()),
                        requirements: QueryRequirements {
                            accuracy: accuracy.clone(),
                            ..Default::default()
                        },
                        predictability: Default::default(),
                        invocations: 1,
                        execute_at: None,
                        range_evaluation: None,
                    })
                    .collect(),
            ),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            metrics,
            DurationMs(sample_interval_ms),
        )),
    }
}

/// How much of the memo the document writes (`--max-memo-candidates`,
/// `--memo-enumeration-limit`, `--max-enumerated`) and how many listed
/// choices it prices (`--price-enumerated`).
struct MemoCaps {
    candidates_per_group: usize,
    enumeration_limit: usize,
    enumerated: usize,
    choices_to_price: usize,
}

/// What pricing an enumerated choice needs: the run's workload, models and
/// selection.
struct Pricing<'a> {
    stage0_roots: &'a [QueryRoot],
    entries: &'a [QueryWorkloadEntry],
    data: &'a DataWorkload,
    models: PlanningModels<'a>,
    selected: Option<SelectedBaseline>,
}

/// The document's `memo` section: the memo after each step, the selected
/// plan's choice over the last step's memo, and the candidate DAGs each
/// step's memo stands for (`memo::enumeration::enumerate`; each listed
/// choice composed by `memo::enumeration::compose`).
fn memo_json(steps: &MemoSteps, caps: &MemoCaps, pricing: &Pricing<'_>) -> Value {
    let named = [
        (
            "stage0",
            "Stage 0, the frontends' DAG hash-consed",
            &steps.stage0,
        ),
        ("pass1", "Stage 1, Pass 1", &steps.pass1),
        (
            "pass2",
            "Stage 1, Pass 2: local window composition",
            &steps.pass2,
        ),
        (
            "concrete_states",
            "Stage 1, Pass 2 rules and concrete state groups",
            &steps.concrete_states,
        ),
    ];
    let mut table = MemoNodes::default();
    let mut previous: Option<&CandidateMemo> = None;
    let mut json_steps = Vec::new();
    let mut per_step = serde_json::Map::new();
    // The last step (with candidates) enumerated completely within `enumerated`.
    let mut listed = None;
    for &(step, title, memo) in &named {
        json_steps.push(json!({
            "step": step,
            "title": title,
            "groups": memo_groups_json(&mut table, memo, previous, caps.candidates_per_group),
        }));
        previous = Some(memo);
        if step == "stage0" {
            // One DAG: the frontends' own.
            per_step.insert(step.into(), json!({ "count": 1, "complete": true }));
            continue;
        }
        let enumeration = enumerate(memo, caps.enumeration_limit);
        if enumeration.complete && enumeration.choices.len() <= caps.enumerated {
            listed = Some((step, memo));
        }
        per_step.insert(
            step.into(),
            json!({ "count": enumeration.choices.len(), "complete": enumeration.complete }),
        );
    }
    let last = &steps.concrete_states;
    let (step, memo) = listed.unwrap_or(("concrete_states", last));
    let selected = steps.selected_choice.as_ref().ok();
    let enumerated = enumerate(memo, caps.enumerated).choices;
    let priced = (caps.choices_to_price > 0).then(|| {
        price_choices(
            memo,
            &enumerated[..caps.choices_to_price.min(enumerated.len())],
            pricing.stage0_roots,
            pricing.entries,
            pricing.data,
            pricing.models,
            pricing.selected,
        )
    });
    let mut choices: Vec<_> = enumerated
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let mut entry = json!({
                "id": format!("E{}", index + 1),
                "choice": choice_json(choice),
                "budget_share_1": choice.iter().all(|(group, &candidate)| {
                    memo.candidates[group.0 as usize][candidate].budget_share == 1
                }),
                "selected": step == "concrete_states" && selected == Some(choice),
            });
            match compose(memo, choice) {
                Ok(roots) => {
                    let roots: Vec<_> = roots.into_iter().map(|(_, node)| node).collect();
                    entry["roots"] = json!(table.add(&roots).0);
                }
                Err(error) => {
                    entry["roots"] = Value::Null;
                    entry["error"] = json!(error.to_string());
                }
            }
            entry
        })
        .collect();
    for (entry, priced) in choices.iter_mut().zip(priced.iter().flatten()) {
        entry["valid"] = json!(priced.valid);
        entry["rejection"] = json!(priced.rejection);
        entry["rejection_reason"] = json!(priced.rejection_reason);
        entry["cost_per_second"] = json!(priced.cost_per_second);
        entry["cheaper_than_selected"] = json!(priced.cheaper_than_selected);
    }
    let priced = priced.map_or(Value::Null, |priced| {
        json!({
            "count": priced.len(),
            "cheaper_than_selected": priced.iter().filter(|p| p.cheaper_than_selected).count(),
            "selected_cost_per_second": pricing.selected.and_then(|s| s.cost_per_second),
        })
    });
    let mut memo = json!({
        "nodes": table.nodes,
        "steps": json_steps,
        "selected_choice": selected.map_or(Value::Null, choice_json),
        "enumeration": {
            "step": step,
            "per_step": per_step,
            "limit": caps.enumeration_limit,
            "choices": choices,
            "priced": priced,
        },
    });
    if let Err(reason) = &steps.selected_choice {
        memo["selected_choice_unavailable"] = json!(reason);
    }
    memo
}

/// Group id (as a string) to candidate index.
fn choice_json(choice: &MemoChoice) -> Value {
    Value::Object(
        choice
            .iter()
            .map(|(group, &candidate)| (group.0.to_string(), json!(candidate)))
            .collect(),
    )
}

/// The `memo` section's node table (`memo.nodes`): each distinct node once,
/// children before parents, its children as indices into the table. Nodes
/// are matched by their serialized JSON (children included), since the
/// steps rebuild equal nodes as different `Rc`s.
#[derive(Default)]
struct MemoNodes {
    nodes: Vec<FlatNode>,
    index_of: HashMap<String, NodeId>,
}

impl MemoNodes {
    /// Adds `roots` and every node below them. Returns each root's index
    /// and, for each node of that DAG, the original node and its index.
    fn add(
        &mut self,
        roots: &[Rc<OperatorNode>],
    ) -> (Vec<NodeId>, Vec<(Rc<OperatorNode>, NodeId)>) {
        let roots: Vec<_> = roots.iter().cloned().map(QueryRoot::Operator).collect();
        let (dag, originals) = flatten(&roots);
        let mut index_in_table: Vec<NodeId> = Vec::with_capacity(dag.nodes.len());
        for mut node in dag.nodes {
            node.operator = node.operator.map_children(|&child| index_in_table[child]);
            let key = serde_json::to_string(&node).expect("a flat node serializes");
            let index = *self.index_of.entry(key).or_insert_with(|| {
                self.nodes.push(node);
                self.nodes.len() - 1
            });
            index_in_table.push(index);
        }
        let roots = dag
            .roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(id) => index_in_table[*id],
                QueryRoot::Scalar(_) => unreachable!("memo DAGs have operator roots"),
            })
            .collect();
        (roots, originals.into_iter().zip(index_in_table).collect())
    }
}

/// `memo`'s groups, each with its first `max_candidates` candidates, their
/// nodes added to `table`; a candidate is `added` when `previous` (the memo
/// at the step before) has no candidate of that group with the same node and
/// children.
fn memo_groups_json(
    table: &mut MemoNodes,
    memo: &CandidateMemo,
    previous: Option<&CandidateMemo>,
    max_candidates: usize,
) -> Vec<Value> {
    let ids = |groups: &[MemoGroupId]| groups.iter().map(|g| g.0).collect::<Vec<_>>();
    memo.groups
        .iter()
        .zip(&memo.candidates)
        .enumerate()
        .map(|(id, (group, candidates))| {
            let node = table.add(std::slice::from_ref(&group.node)).0[0];
            let mut by_origin = serde_json::Map::new();
            for candidate in candidates {
                let count = by_origin
                    .entry(origin_name(&candidate.origin))
                    .or_insert(json!(0));
                *count = json!(count.as_u64().unwrap_or(0) + 1);
            }
            let earlier = previous
                .and_then(|memo| memo.candidates.get(id))
                .map_or(&[][..], Vec::as_slice);
            let listed: Vec<_> = candidates
                .iter()
                .take(max_candidates)
                .enumerate()
                .map(|(index, candidate)| {
                    let (roots, originals) = table.add(std::slice::from_ref(&candidate.node));
                    let group_of_node: serde_json::Map<_, _> = originals
                        .iter()
                        .filter_map(|(original, node)| {
                            candidate
                                .children
                                .iter()
                                .find(|child| {
                                    Rc::ptr_eq(&memo.groups[child.0 as usize].node, original)
                                })
                                .map(|child| (node.to_string(), json!(child.0)))
                        })
                        .collect();
                    json!({
                        "index": index,
                        "implements": ids(&candidate.implements),
                        "children": ids(&candidate.children),
                        "origin": origin_name(&candidate.origin),
                        "budget_share": candidate.budget_share,
                        "added": !earlier.iter().any(|before| {
                            Rc::ptr_eq(&before.node, &candidate.node)
                                && before.children == candidate.children
                        }),
                        "label": candidate_node_label(&table.nodes, roots[0], &group_of_node),
                        "node": roots[0],
                        "group_of_node": group_of_node,
                    })
                })
                .collect();
            json!({
                "id": id,
                "label": node_label(&table.nodes[node]),
                "children": ids(&group.children),
                "parents": ids(&group.parents),
                "queries": group.queries,
                "roots_of": group.roots_of,
                "node": node,
                "candidate_count": candidates.len(),
                "candidate_count_by_origin": by_origin,
                "candidates_capped": listed.len() < candidates.len(),
                "candidates": listed,
            })
        })
        .collect()
}

/// `CandidateOrigin` in snake case.
fn origin_name(origin: &CandidateOrigin) -> String {
    match origin {
        CandidateOrigin::Pass1 => "pass1",
        CandidateOrigin::SharedBuild => "shared_build",
        CandidateOrigin::SummaryResult => "summary_result",
        CandidateOrigin::SummaryInput => "summary_input",
        CandidateOrigin::Panes => "panes",
        CandidateOrigin::Rollup => "rollup",
        CandidateOrigin::TopKReuse => "top_k_reuse",
        CandidateOrigin::WidenedScans => "widened_scans",
    }
    .into()
}

/// A candidate's own nodes (those above its child groups' nodes), root
/// first, as `node_label`s joined by " ← ".
fn candidate_node_label(
    nodes: &[FlatNode],
    root: NodeId,
    group_of_node: &serde_json::Map<String, Value>,
) -> String {
    let mut labels = Vec::new();
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if group_of_node.contains_key(&id.to_string()) || !seen.insert(id) {
            continue;
        }
        labels.push(node_label(&nodes[id]));
        pending.extend(nodes[id].operator.children().into_iter().rev().copied());
    }
    labels.join(" ← ")
}

/// One node's operator kind with its sketch family and parameters, its
/// summary query and its time range, when it has them: e.g.
/// `SummaryAgg Kll(k=200)`, `SummaryEstimate Quantile(q=0.5)`.
fn node_label(node: &asap_types::ir::flat::FlatNode) -> String {
    let operator = serde_json::to_value(&node.operator).unwrap_or_default();
    // `{"ASAP": {"SummaryAgg": {...}}}` (or a unit variant's string).
    let (kind, fields) = match operator.as_object().and_then(|o| o.values().next()) {
        Some(Value::Object(inner)) => match inner.iter().next() {
            Some((kind, fields)) => (kind.clone(), fields.clone()),
            None => (String::new(), Value::Null),
        },
        Some(Value::String(kind)) => (kind.clone(), Value::Null),
        _ => (operator.to_string(), Value::Null),
    };
    let mut details = Vec::new();
    find_details(&fields, &mut details, 0);
    let mut label = kind;
    for detail in details {
        label.push(' ');
        label.push_str(&detail);
    }
    label
}

/// Sketch algorithms with parameters, summary queries and time ranges found
/// in an operator's fields (not inside its schemas).
fn find_details(value: &Value, details: &mut Vec<String>, depth: usize) {
    if depth > 6 || details.len() >= 3 {
        return;
    }
    let compact = |value: &Value| -> String {
        match value {
            Value::Object(fields) => fields
                .iter()
                .map(|(key, value)| match value {
                    Value::Object(inner) if inner.len() == 1 => {
                        let (k, v) = inner.iter().next().unwrap();
                        format!("{key}({k}={v})")
                    }
                    Value::Object(inner) => format!(
                        "{key}({})",
                        inner
                            .iter()
                            .filter(|(_, v)| !v.is_object() && !v.is_array())
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    other => format!("{key}={other}"),
                })
                .collect::<Vec<_>>()
                .join(" "),
            other => other.to_string(),
        }
    };
    match value {
        Value::Object(fields) => {
            if let (Some(Value::String(algorithm)), Some(params)) =
                (fields.get("algorithm"), fields.get("params"))
            {
                let params = params
                    .as_object()
                    .and_then(|p| p.values().next())
                    .map(compact)
                    .unwrap_or_default();
                details.push(format!("{algorithm}({params})"));
                return;
            }
            for (key, field) in fields {
                match key.as_str() {
                    "schema" | "input_schema" | "output_schema" | "predicates" | "cols" => {}
                    "query" => details.push(compact(field)),
                    // An aggregate's measures, e.g. `quantile(q=0.5)`.
                    "measures" => {
                        for measure in field.as_array().into_iter().flatten() {
                            let Some(kind) = measure.get("kind").and_then(Value::as_str) else {
                                continue;
                            };
                            let params: Vec<_> = measure
                                .as_object()
                                .into_iter()
                                .flatten()
                                .filter(|(key, value)| {
                                    *key != "kind" && (value.is_number() || value.is_string())
                                })
                                .map(|(key, value)| format!("{key}={value}"))
                                .collect();
                            details.push(format!("{kind}({})", params.join(", ")));
                        }
                    }
                    "range" | "window" => details.push(format!("{key}={field}")),
                    _ => find_details(field, details, depth + 1),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                find_details(item, details, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_devtools::planning_stages::run_planning_stages;

    /// A node two DAGs share is stored once in the memo's node table, even
    /// when each DAG built it as its own `Rc`, and each DAG rebuilt from the
    /// table equals its own `FlatDag` export.
    #[test]
    fn memo_node_table_stores_a_shared_node_once() {
        let root = |query: &str| {
            let workload = query_batch(
                QueryLanguage::PromQL,
                &[query.into()],
                AccuracyTarget::exact(),
                15_000,
            );
            match lower_promql_roots(&workload).unwrap().remove(0) {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => unreachable!(),
            }
        };
        let (sum, max) = (root("sum(m)"), root("max(m)"));
        let mut table = MemoNodes::default();
        let (sum_root, _) = table.add(std::slice::from_ref(&sum));
        let after_sum = table.nodes.len();
        let (max_root, _) = table.add(std::slice::from_ref(&max));
        // `max(m)` adds only its own aggregate: the scan is already there.
        assert_eq!(table.nodes.len(), after_sum + 1);
        // The DAG below `root`, renumbered in table order.
        let rebuilt = |root: NodeId| {
            let mut reached = BTreeSet::new();
            let mut pending = vec![root];
            while let Some(id) = pending.pop() {
                if reached.insert(id) {
                    pending.extend(table.nodes[id].operator.children().into_iter().copied());
                }
            }
            let local: HashMap<_, _> = reached.iter().enumerate().map(|(i, &id)| (id, i)).collect();
            let nodes: Vec<_> = reached
                .iter()
                .map(|&id| {
                    let mut node = table.nodes[id].clone();
                    node.operator = node.operator.map_children(|child| local[child]);
                    node
                })
                .collect();
            FlatDag {
                nodes,
                roots: vec![QueryRoot::Operator(local[&root])],
            }
        };
        for (node, root) in [(sum, sum_root[0]), (max, max_root[0])] {
            assert_eq!(
                serde_json::to_value(rebuilt(root)).unwrap(),
                serde_json::to_value(export(&[QueryRoot::Operator(node)])).unwrap()
            );
        }
    }

    /// The exhaustive search's default `--max-combinations` is 10^4.
    #[test]
    fn exhaustive_default_candidate_limit_is_ten_thousand() {
        assert_eq!(MAX_COMBINATIONS, 10_000);
    }

    /// With no valid candidate, the document is still written: `selected`
    /// is null and every rejection is listed.
    #[test]
    fn failed_selection_lists_rejections_without_a_selection() {
        let rejected = vec![Rejection {
            id: "P1".into(),
            cause: asap_plan_selection::RejectionCause::AccuracyTargetMissed,
            reason: "q1: misses target".into(),
        }];
        let document = plan_selection_json(
            &Err(SelectionError::NoValidCandidate(rejected)),
            |_| true,
            |_| true,
        );
        assert_eq!(
            document,
            json!({
                "costs": {},
                "selected": null,
                "rejected": [{
                    "id": "P1",
                    "cause": "accuracy_target_missed",
                    "reason": "q1: misses target",
                }],
                "search_rejected": [],
                "rejected_by_cause": { "accuracy_target_missed": 1 },
            })
        );
    }

    /// With nothing else displayed, the document still shows the selected
    /// candidate and its cost, chosen among all candidates.
    #[test]
    fn display_cap_keeps_the_selected_candidate() {
        let mut workload = query_batch(
            QueryLanguage::PromQL,
            &["sum(m)".into()],
            AccuracyTarget::exact(),
            15_000,
        );
        // `m` holds 1,000 series: pricing uses only declared facts.
        workload.data_workload = workload
            .data_workload
            .map(|data| data.with_series_count(1_000));
        let run =
            run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let document = stage_pipeline(workload_queries(&workload), &entries, &run, 0);
        let selected = document["stage3_plan_selection"]["selected"]
            .as_str()
            .unwrap();
        let physical = document["stage2_physical_asap_optimization"]["candidates"]
            .as_array()
            .unwrap();
        assert_eq!(physical.len(), 1);
        assert_eq!(physical[0]["id"], selected);
        assert!(document["stage3_plan_selection"]["costs"]
            .get(selected)
            .is_some());
        assert_eq!(document["stage3_plan_selection"]["rejected"], json!([]));
    }
}
