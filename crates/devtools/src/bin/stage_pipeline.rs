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
// four planner stages as the facade runs them (`asap_planner::run_planning`).
// The default document keeps the selected plan; `--exhaustive` adds the
// reference enumeration: the selected round's memo choices, each implemented
// by Stage 2 with every materialization assignment, all priced by the
// reference pricer (`asap_plan_selection::reference_pricer`), under
// `reference_pricing`. `--milp-time-limit-s` bounds planning; a timeout keeps
// the best plan found, never an optimality claim for unexplored candidates.
// Unlike `plan_installable`'s default settings, Stage 3 ignores accuracy
// guarantees unless `--enforce-accuracy-guarantees` is given (below);
// `accuracy_guarantees` is `"ignored"` or `"enforced"`.
//   - stage0_query_language_frontends: the frontends' workload DAG, one root per query;
//   - stage1_logical_asap_optimization: `L1`, the selected plan's roots (as
//     Stage 2 built them, with Stage 3's weight proofs), then under
//     `--exhaustive` the enumerated choices `E<n>` (the memo itself, after
//     each step, is under `memo`). The document shows the logical
//     candidates of the physical candidates it shows (below), and those among
//     the first `--max-displayed` that have none, with `combinations` (how
//     many there are), `capped` (whether some are not shown), `pass1_pruned`
//     (the candidates Pass 1 pruned per target, with reasons) and
//     `pruned_candidates` (the same, each on its memo `group`:
//     `PerComputationLogicalMemoCandidates::pruned_candidates`, with
//     `candidate` and `reason`). A candidate that cannot be composed carries
//     `error` instead of `dag`;
//   - stage2_physical_asap_optimization: `P1`, the selected plan as Stage 2
//     built it, then under `--exhaustive` each enumerated choice's physical
//     plans `E<n>-<k>`; an enumerated choice Stage 2 cannot implement is
//     listed under `pruned` with the reason. `pruned_candidates` lists the
//     physical candidates Stage 2 pruned in the selected round
//     (`PhysicalASAPMemoCandidates::pruned_candidates`): each with its memo
//     `group`, `operator_node`, `operator`, `implementation_root`, `timing`,
//     `materialization` and `reason` (`PruneReason`:
//     `PerEntityAggregateNotExecutable`, `RawDataNotRetained`,
//     `InputHasNoBuildableProducer`, `ReachesNoQueryOutput`,
//     `TooFewQueryReaders`). The document shows at
//     most `--max-displayed` (default 64) enumerated physical candidates: the
//     reference pricer's pick, the other valid ones cheapest first, then the
//     rejected ones taking each cause in turn;
//   - stage3_plan_selection: Stage 3's `Selection`: `selected_dag`
//     (`DagOfSelectedCandidates`: `candidate_of_node`, the candidate id of
//     the selected round's Stage 2 output each node runs, one node per run;
//     `edges`, each `producer` node feeding input `input` of a `consumer`
//     node; and `node_of_output_reference`),
//     `nodes_with_proven_non_negative_weights`, the plan's `cost` (its
//     `usage`, `cost_per_second` and per-node charges: each node's `cost` is
//     one computation's price times `multiplier`, the times it is computed),
//     `accepted_accuracy_per_query` (per query: the target's metric or null
//     when not set, the accepted guarantee's metric or null when exact or
//     accepted regardless of guarantee, `accepted_regardless_of_guarantee`,
//     `guarantee_check_rejection`, the sketches read and a one-line summary),
//     `pruned_candidates` and `search_report` (`SearchReport`: `status`,
//     `cost_lower_bound_per_second`, MILP size and timings); `error` when no
//     plan was selected. `pane_retention_ms` maps each pane chain the
//     selected plan keeps beyond one evaluation (by node id in `P1`) to how
//     far back the runtime must keep its panes (`RealizedPlan::pane_retention_ms`).
//     `materialized_views` are the selected plan's materialized views as
//     `plan_installable`'s plan derives them (`installable_plan_of`, then
//     `InstallablePlan::materialized_views`: each view's installed node id,
//     `refresh`, `coverage` with its `definition_id` and `selection`,
//     `inputs` and `retention_ms`), each with the node of `P1` it is
//     installed from (`stage2_node_id`), its position among that node's
//     installed nodes (`installed_position`, a pane chain's panes newest
//     first) and their number (`installed_count`); empty without a
//     selection, with `materialized_views_error` when the plan is not
//     installable. `warnings` are one-line warnings about the workload and
//     the selected plan: a PromQL query applying `rate`, `increase` or
//     `irate` to a metric the data workload declares a gauge, prefixed with
//     its id (the query still plans; `counter_function_over_gauge_warnings`),
//     and the plan's peak memory when over 10^12 bytes and the cost model
//     declares no memory capacity (`large_peak_memory_warning`); absent when
//     there are none;
//   - planning_report: the facade's `PlanningReport`: Pass 1's time and, per
//     pane-width round, Pass 2's and Stage 2's counts and each stage's time,
//     with Stage 3's search report or error;
//   - reference_pricing (with `--exhaustive`): the reference pricer's
//     selection among the enumerated plans: the shown candidates' `costs`
//     and `accepted`, its pick `selected`, every other shown candidate under
//     `rejected` with its `cause` (a failed check: `accuracy_target_missed`,
//     `count_min_weights_not_proven_non_negative`, `no_accuracy_guarantee`,
//     `latency_bound_exceeded`, `latency_over_repeat_interval`,
//     `over_cluster_capacity`; or a loss under the cost model: `costlier`,
//     `slower`, `more_memory`, `tied_lost_on_enumeration_order`),
//     `search_rejected` (rejections of candidates not shown) and
//     `rejected_by_cause`.
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
// `children`, `origin` (`OriginOfCandidateForMemoGroup` in snake case), `budget_share`,
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
// (`memo::enumeration::build_logical_asap_dags_from_chosen_candidates`'s roots, one per root group, as indices
// into `nodes`; null with `error` when it does not
// compose), `budget_share_1` (every chosen candidate sized for the whole
// budget) and `selected` (it is `selected_choice`). With
// `--price-enumerated <n>` (off by default), each of the first `n` listed
// choices is also run through Stage 2 and the reference pricer, and its
// selected plan installed (`memo_pricing::price_choices`, under the
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
// Under `--exhaustive`, `--max-combinations` (default 10^4) bounds the memo
// choices enumerated. Display truncation (`--max-displayed`) is separate.
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
// in, and `stage3_plan_selection.accepted_accuracy_per_query` reports, per
// query, the metric accepted (e.g. "metric not set; accepted rank (Kll)").
// `--cost-model` chooses how plan selection compares valid candidates:
// `usage-over-time` (cost model 1, the default: CPU, scanned bytes and
// memory byte-seconds per second, priced), `provisioned-capacity` (cost
// model 2: peak memory, peak CPU cores and disk, priced) or
// `cluster-capacity` (cost model 3: no prices; `--cluster-memory-bytes`,
// `--cluster-disk-bytes` and the optional `--cluster-cpu-cores` are hard
// limits, and `--cluster-objective` is `lowest-latency` or `least-memory`).
// Cost models 1 and 2 use the illustrative unit costs;
// `aws-fargate-us-east-1` is cost model 1 at AWS Fargate's us-east-1
// Linux/x86 prices (`ResourceUnitCost::aws_fargate_us_east_1`, in US dollars
// per second).
// Each cost's `usage` lists what the plan uses; `cost_per_second` is null under
// cost model 3.
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
// computation's under `fused_pruned`. Without raw data retained, Stage 2
// does not generate a query-time scan of raw data (its `pruned_candidates`
// lists each with `RawDataNotRetained`), and stored data (at rest or
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
// one JSON file (`CostInputs`: `cost_model`, e.g.
// `{"usage_over_time": {"cost_per_cpu_op": 1e-6, ...}}` with the
// deployment's own unit costs, `resource_speed`,
// `non_repeating_query_horizon_seconds` and `measured_operation_costs`;
// an omitted field takes its default). It replaces `--cost-model`, the
// `--cluster-*` flags and `--measured-operation-costs`.
// Accuracy guarantees are ignored by default: stage_pipeline plans with
// `PlanningModels::ignore_accuracy_guarantees`, so the facade passes Stage 3
// an accuracy model that accepts every query output regardless of its
// accuracy guarantee, and candidates are compared on cost alone;
// executability checks still apply. `--enforce-accuracy-guarantees` clears
// it, and Stage 3 rejects an output whose guarantee does not meet its
// query's target, as `plan_installable` does with the default models; Pass 1
// then also prunes each summary with which no query reading it can meet its
// target (`NoErrorBoundForQueryTarget`). The document's top-level `accuracy_guarantees` is
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

use asap_cost::{
    ClusterLimits, ClusterObjective, CostInputs, CostModel, MeasuredOperationCosts,
    ResourceUnitCost, ResourceUsage,
};
use asap_devtools::candidate_labels::{candidate_labels, computation_label, query_of_each_target};
use asap_devtools::memo_pricing::{price_choices, SelectedBaseline};
use asap_devtools::planner_layering_examples::{
    example1, example2, pattern_a, pattern_b, PatternARecurrence, SHARED_ROWS_PER_SECOND,
};
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_search_over, sql_catalog, PlanningStagesError,
    PlanningStagesRun,
};
use asap_devtools::SqlCatalog;
use asap_frontend_promql::{counter_function_over_gauge_warnings, promql_metric_names};
use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    HowToComputeAggregateMeasure, PerComputationCandidates,
};
use asap_logical_asap_optimization::pass2::memo::enumeration::{
    build_logical_asap_dags_from_chosen_candidates, enumerate, ChosenCandidatePerMemoGroup,
};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::reference_pricer::{Rejection, WholePlanSelection};
use asap_plan_selection::{
    large_peak_memory_warning, AcceptedAccuracy, ErrorSelectingPlan, Selection,
};
use asap_planner::MemoSteps;
use asap_planner::{installable_plan_of, PlanSearchSettings, PlanningModels, SelectionError};
use asap_types::ir::flat::{flatten, FlatDag, FlatNode, NodeId};
use asap_types::ir::operator::node::OperatorNode;
use asap_types::ir::properties::{default_metric_for_root, AccuracyMetric, AccuracyTarget};
use asap_types::ir::QueryRoot;
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::stages::{queries_rooted_at, MemoGroupId};
use asap_types::stages::{CandidateMemo, OriginOfCandidateForMemoGroup, PruneReason};
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
--cluster-disk-bytes <n> [--cluster-cpu-cores <n>] --cluster-objective lowest-latency|least-memory]] \
[--plan-selection-settings <file>] [--enforce-accuracy-guarantees] [--deployment-capabilities <json>] [--exhaustive [--max-combinations <n>]] [--milp-time-limit-s <f64>] [--max-displayed <n>] \
[--no-memo] [--max-memo-candidates <n>] [--memo-enumeration-limit <n>] [--max-enumerated <n>] [--price-enumerated <n>] --out <file>";

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
                "pruned_candidates": [],
                "candidates": [],
                "error": format!("Pass 1: {error}"),
            },
            "stage2_physical_asap_optimization": { "candidates": [], "pruned": [], "pruned_candidates": [] },
            "stage3_plan_selection": { "selected_dag": null, "materialized_views": [] },
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
    // `--milp-time-limit-s` limits the MILP search.
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
    let default_settings = CostInputs::default();
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
    let plan_selection: CostInputs = match &plan_selection_settings_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("--plan-selection-settings {path}: {e}"))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("--plan-selection-settings {path}: {e}"))?
        }
        None => CostInputs {
            cost_model,
            measured_operation_costs,
            ..default_settings
        },
    };
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
        .with_deployment_capabilities(&deployment_capabilities)
        .with_ignore_accuracy_guarantees(!enforce_accuracy_guarantees);
    let result = run_search_over(
        roots.clone(),
        &entries,
        &data,
        models,
        PlanSearchSettings {
            time_limit_ms: milp_time_limit_ms,
        },
        exhaustive.then_some(max_combinations),
    );
    // A rate, increase or irate over a declared gauge (the query still plans).
    let mut warnings = counter_function_over_gauge_warnings_of(&described, &data)?;
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
                    &plan_selection.cost_model,
                )
            }),
    );
    let memo = result
        .as_ref()
        .ok()
        .and_then(|run| run.run.memo_steps.as_ref())
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
                    .and_then(|run| run.run.selection.as_ref().ok())
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
    document["search"] = json!(if exhaustive {
        "exhaustive"
    } else {
        "candidate-connections"
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
    let logical = run.logical_candidates();
    let every_physical = run.physical_candidates();
    // The display keeps `max_displayed` physical candidates: under
    // `--exhaustive` the reference pricer's selected one, the other valid
    // ones cheapest first, then the rejected ones taking each cause in turn,
    // so a cause with many candidates does not hide the others.
    let shown_physical = displayed_physical(run, &every_physical, max_displayed);
    let implemented: HashSet<&str> = every_physical
        .iter()
        .map(|p| p.from_logical.as_str())
        .collect();
    // The logical candidates of the shown physical ones, and those among
    // the first `max_displayed` that have none (not composed or not
    // implemented).
    let shown: HashSet<&str> = logical
        .iter()
        .enumerate()
        .filter(|(index, c)| {
            every_physical
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
    let candidates: Vec<_> = logical
        .iter()
        .filter(|c| shown.contains(c.id.as_str()))
        .map(|c| match &c.roots {
            Ok(roots) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "dag": export(roots) })
            }
            Err(error) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "error": error })
            }
        })
        .collect();
    let physical: Vec<_> = every_physical
        .iter()
        .filter(|p| shown_physical.contains(p.id.as_str()))
        .map(|p| json!({ "id": p.id, "from_logical": p.from_logical, "display_name": labels[&p.id], "dag": p.dag }))
        .collect();
    let pruned: Vec<_> = run
        .reference
        .iter()
        .flat_map(|reference| &reference.stage2_failed)
        .filter(|(id, _)| shown.contains(id.as_str()))
        .map(|(id, reason)| json!({ "from_logical": id, "display_name": labels[id], "reason": reason }))
        .collect();
    let mut stage3 = selection_json(&run.run.selection);
    match (&run.run.selection, &run.run.selected_plan) {
        (Ok(selection), Some(roots)) => {
            let pane_retention_ms = run
                .run
                .selected_physical_plan
                .as_ref()
                .map(|realized| realized.pane_retention_ms.clone())
                .unwrap_or_default();
            stage3["pane_retention_ms"] = pane_retention_ms
                .iter()
                .map(|(node, ms)| (node.to_string(), json!(ms)))
                .collect::<serde_json::Map<_, _>>()
                .into();
            match materialized_views_json(roots, selection, &pane_retention_ms, entries) {
                Ok(views) => stage3["materialized_views"] = Value::Array(views),
                Err(error) => {
                    stage3["materialized_views"] = json!([]);
                    stage3["materialized_views_error"] = json!(error);
                }
            }
        }
        _ => stage3["materialized_views"] = json!([]),
    }
    let memo_steps = run.run.memo_steps.as_ref();
    let mut document = json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": queries },
        "stage0_query_language_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "stage1_logical_asap_optimization": {
            "combinations": logical.len(),
            "capped": physical.len() < every_physical.len() || candidates.len() < logical.len()
                || run.reference.as_ref().is_some_and(|reference| reference.capped),
            "pass1_pruned": pass1_pruned_json(&run.run.per_computation_candidates),
            "pruned_candidates": pass1_pruned_candidates_json(memo_steps),
            "candidates": candidates,
        },
        "stage2_physical_asap_optimization": {
            "candidates": physical,
            "pruned": pruned,
            "pruned_candidates": stage2_pruned_candidates_json(memo_steps),
        },
        "stage3_plan_selection": stage3,
        "planning_report": serde_json::to_value(&run.run.report).unwrap_or(Value::Null),
    });
    if let Some(reference) = &run.reference {
        let every: HashSet<&str> = every_physical.iter().map(|p| p.id.as_str()).collect();
        document["reference_pricing"] = reference_pricing_json(
            &reference.selection,
            |id| shown_physical.contains(id),
            |id| every.contains(id),
        );
    }
    document
}

/// The ids of the physical candidates the document shows: at most
/// `max_displayed`. Without a reference enumeration, the selected plan.
/// Under `--exhaustive`, the reference pricer's selected one first, then
/// the other valid ones cheapest first (in candidate order when unpriced),
/// then the rejected ones, one of each cause in turn in candidate order.
fn displayed_physical<'a>(
    run: &PlanningStagesRun,
    every: &'a [PhysicalWorkloadPlan],
    max_displayed: usize,
) -> HashSet<&'a str> {
    // The selected plan is always shown.
    let selected_plan = every.iter().map(|p| p.id.as_str()).filter(|id| *id == "P1");
    let Some(reference) = &run.reference else {
        return selected_plan.collect();
    };
    let shown: HashSet<&str> = displayed_reference(reference, every, max_displayed);
    selected_plan.chain(shown).collect()
}

/// [`displayed_physical`] among the reference enumeration's plans.
fn displayed_reference<'a>(
    reference: &asap_devtools::memo_pricing::ReferenceEnumeration,
    every: &'a [PhysicalWorkloadPlan],
    max_displayed: usize,
) -> HashSet<&'a str> {
    let every: Vec<&'a PhysicalWorkloadPlan> = every.iter().filter(|p| p.id != "P1").collect();
    let Ok(selection) = &reference.selection else {
        // Without a selection (rejected, or not judged): each cause in turn.
        let causes: Vec<_> = match &reference.selection {
            Err(ErrorSelectingPlan::NoValidCandidate(rejected)) => rejected.iter().collect(),
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
    every
        .iter()
        .map(|p| p.id.as_str())
        .filter(|id| *id == selection.selected)
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
    roots: &[QueryRoot],
    selection: &Selection,
    pane_retention_ms: &std::collections::BTreeMap<
        asap_types::ir::physical_export::PhysicalASAPNodeId,
        u64,
    >,
    entries: &[QueryWorkloadEntry],
) -> Result<Vec<Value>, String> {
    let (plan, report) = installable_plan_of(roots, Some(selection), pane_retention_ms, entries)
        .map_err(|e| e.to_string())?;
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

/// The candidates Pass 1 pruned, per target that has any: the target's
/// index, the query it is listed under, and each pruned candidate of its
/// own (`pruned`) and of its fused computation (`fused_pruned`) with the
/// reason, e.g. `UnsupportedByDeployment`.
fn pass1_pruned_json(inventory: &PerComputationCandidates) -> Vec<Value> {
    let pruned_json = |pruned: &[(HowToComputeAggregateMeasure, PruneReason)]| -> Vec<Value> {
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
        .filter_map(|(index, _)| {
            let pruned = inventory.pruned_of_target(index, false);
            let fused_pruned = inventory.pruned_of_target(index, true);
            if pruned.is_empty() && fused_pruned.is_empty() {
                return None;
            }
            Some(json!({
                "target": index,
                "query": format!("Q{}", query_of_target[index] + 1),
                "pruned": pruned_json(&pruned),
                "fused_pruned": pruned_json(&fused_pruned),
            }))
        })
        .collect()
}

/// The ways to compute a measure Pass 1 pruned
/// (`PerComputationLogicalMemoCandidates::pruned_candidates`), in pruning
/// order: each with its aggregate's memo `group`, the `candidate` and the
/// `reason` (`PruneReason`, e.g. `UnsupportedByDeployment`). Empty without
/// memo steps.
fn pass1_pruned_candidates_json(steps: Option<&MemoSteps>) -> Vec<Value> {
    steps
        .map_or(&[][..], |steps| &steps.pass1_pruned_candidates)
        .iter()
        .map(|pruned| {
            json!({
                "group": pruned.group.0,
                "candidate": computation_label(&pruned.candidate),
                "reason": format!("{:?}", pruned.reason),
            })
        })
        .collect()
}

/// The physical candidates Stage 2 pruned
/// (`PhysicalASAPMemoCandidates::pruned_candidates`), in pruning order:
/// each with its memo `group`, its `operator_node` (index into Stage 2's
/// operator nodes), the node's `operator` label, whether it is an
/// `implementation_root`, its `timing` and `materialization`, and the
/// `reason` (`PruneReason`, e.g.
/// `RawDataNotRetained`). Empty without memo steps.
fn stage2_pruned_candidates_json(steps: Option<&MemoSteps>) -> Vec<Value> {
    let Some(steps) = steps else {
        return Vec::new();
    };
    steps
        .stage2_pruned_candidates
        .iter()
        .map(|pruned| {
            let key = &pruned.candidate;
            let node = &steps.stage2_operator_nodes[key.operator_node.0];
            let flat = export(&[QueryRoot::Operator(node.physical_node.clone())]);
            json!({
                "group": pruned.group.0,
                "operator_node": key.operator_node.0,
                "operator": flat.nodes.last().map(node_label).unwrap_or_default(),
                "implementation_root": node.is_implementation_root,
                "timing": key.timing,
                "materialization": key.materialization,
                "reason": format!("{:?}", pruned.reason),
            })
        })
        .collect()
}

/// The reference pricer over every enumerated candidate (`--exhaustive`),
/// listing the costs and rejections of the `shown` ones.
fn reference_pricing_json(
    selection: &Result<WholePlanSelection, ErrorSelectingPlan>,
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
        Err(ErrorSelectingPlan::NoValidCandidate(rejected)) => {
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
            return json!({ "costs": {}, "selected": null, "rejected": [], "error": error.to_string() });
        }
    };
    let costs: serde_json::Map<_, _> = selection
        .costs
        .iter()
        .filter(|(id, _)| shown(id))
        .map(|(id, cost)| (id.clone(), cost_json(cost)))
        .collect();
    let accepted: serde_json::Map<_, _> = selection
        .accepted
        .iter()
        .filter(|(id, _)| shown(id))
        .map(|(id, queries)| (id.clone(), accepted_json(queries)))
        .collect();
    json!({
        "costs": costs,
        "accepted": accepted,
        "selected": selection.selected,
        "rejected": rejections_json(&selection.rejected),
        "search_rejected": search_rejected(&selection.rejected),
        "rejected_by_cause": rejected_by_cause(&selection.rejected),
        "pane_retention_ms": selection
            .pane_retention_ms
            .iter()
            .map(|(node, ms)| (node.to_string(), json!(ms)))
            .collect::<serde_json::Map<_, _>>(),
    })
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
                .map(|e| json!({ "producer": e.producer.0, "consumer": e.consumer.0, "input": e.input }))
                .collect::<Vec<_>>(),
            "node_of_output_reference": nodes(&dag.node_of_output_reference),
        },
        "nodes_with_proven_non_negative_weights":
            nodes(&selection.nodes_with_proven_non_negative_weights),
        "cost": cost_json(&selection.cost),
        "accepted_accuracy_per_query": accepted_json(&selection.accepted_accuracy_per_query),
        "pruned_candidates": selection
            .pruned_candidates
            .iter()
            .map(|p| json!({ "group": p.group.0, "candidate": p.candidate.0, "reason": format!("{:?}", p.reason) }))
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
/// choice composed by `memo::enumeration::build_logical_asap_dags_from_chosen_candidates`).
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
        if enumeration.complete && enumeration.all_chosen_candidates.len() <= caps.enumerated {
            listed = Some((step, memo));
        }
        per_step.insert(
            step.into(),
            json!({ "count": enumeration.all_chosen_candidates.len(), "complete": enumeration.complete }),
        );
    }
    let last = &steps.concrete_states;
    let (step, memo) = listed.unwrap_or(("concrete_states", last));
    let selected = steps.selected_chosen_candidates.as_ref().ok();
    let enumerated = enumerate(memo, caps.enumerated).all_chosen_candidates;
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
                    memo.candidates[group.0 as usize][candidate].accuracy_budget_parts == 1
                }),
                "selected": step == "concrete_states" && selected == Some(choice),
            });
            match build_logical_asap_dags_from_chosen_candidates(memo, choice) {
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
    if let Err(reason) = &steps.selected_chosen_candidates {
        memo["selected_choice_unavailable"] = json!(reason);
    }
    memo
}

/// Group id (as a string) to candidate index.
fn choice_json(choice: &ChosenCandidatePerMemoGroup) -> Value {
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
            let node = table.add(std::slice::from_ref(&group.logical_node)).0[0];
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
                    let (roots, originals) = table.add(std::slice::from_ref(&candidate.root_node));
                    let group_of_node: serde_json::Map<_, _> = originals
                        .iter()
                        .filter_map(|(original, node)| {
                            candidate
                                .input_groups
                                .iter()
                                .find(|child| {
                                    Rc::ptr_eq(
                                        &memo.groups[child.0 as usize].logical_node,
                                        original,
                                    )
                                })
                                .map(|child| (node.to_string(), json!(child.0)))
                        })
                        .collect();
                    json!({
                        "index": index,
                        "implements": std::iter::once(id as u32)
                            .chain(candidate.fused_groups.iter().map(|g| g.0))
                            .collect::<Vec<_>>(),
                        "children": ids(&candidate.input_groups),
                        "origin": origin_name(&candidate.origin),
                        "budget_share": candidate.accuracy_budget_parts,
                        "added": !earlier.iter().any(|before| {
                            Rc::ptr_eq(&before.root_node, &candidate.root_node)
                                && before.input_groups == candidate.input_groups
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
                "children": ids(&group.child_groups),
                "parents": ids(group.parent_groups()),
                "queries": group.reached_by_queries(),
                "roots_of": queries_rooted_at(&memo.query_roots, MemoGroupId(id as u32)),
                "node": node,
                "candidate_count": candidates.len(),
                "candidate_count_by_origin": by_origin,
                "candidates_capped": listed.len() < candidates.len(),
                "candidates": listed,
            })
        })
        .collect()
}

/// `OriginOfCandidateForMemoGroup` in snake case.
fn origin_name(origin: &OriginOfCandidateForMemoGroup) -> String {
    match origin {
        OriginOfCandidateForMemoGroup::Pass1 => "pass1",
        OriginOfCandidateForMemoGroup::SharedBuild => "shared_build",
        OriginOfCandidateForMemoGroup::SummaryResult => "summary_result",
        OriginOfCandidateForMemoGroup::SummaryInput => "summary_input",
        OriginOfCandidateForMemoGroup::Panes => "panes",
        OriginOfCandidateForMemoGroup::Rollup => "rollup",
        OriginOfCandidateForMemoGroup::TopKReuse => "top_k_reuse",
        OriginOfCandidateForMemoGroup::WidenedScans => "widened_scans",
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
        let document = reference_pricing_json(
            &Err(ErrorSelectingPlan::NoValidCandidate(rejected)),
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
        let selected = document["reference_pricing"]["selected"].as_str().unwrap();
        let physical = document["stage2_physical_asap_optimization"]["candidates"]
            .as_array()
            .unwrap();
        // The facade's selected plan (`P1`) and the reference pricer's.
        assert_eq!(physical.len(), 2);
        assert!(physical.iter().any(|p| p["id"] == selected));
        assert!(document["reference_pricing"]["costs"]
            .get(selected)
            .is_some());
        assert_eq!(document["reference_pricing"]["rejected"], json!([]));
    }
}
