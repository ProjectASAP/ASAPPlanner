// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --out planner-layering-example1.json
// (also planner-layering-2: #509 Example 2, three SQL flow statistics;
// planner-layering-3a and planner-layering-3b: #509 Example 3, Patterns A
// and B; 3a has 972 candidates, all priced, 64 shown by default)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --max-error 0.01 --failure-probability 0.001 --out run.json
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --table '{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"},
//               {"name": "service", "type": "utf8"}], "time_index": 0}' \
//     --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages (#509 MVP):
//   - stage0_query_language_frontends: the frontends' workload DAG, one root per query;
//   - stage1_logical_asap_optimization: Pass 1 whole-workload candidates, one per
//     choice of a per-computation candidate for every target (the Cartesian
//     product), each followed by its Pass 2 candidates (the
//     identical-expression rule's, one per subset of its sharing candidates,
//     then the summary-capability rule's, one per choice of shared summary
//     builds), in enumeration order. Every candidate is optimized and
//     priced; the document shows the first `--max-displayed` (default 64)
//     and the selected one, with `combinations` (how many exist),
//     `capped` (whether some are not shown) and `pass1_pruned` (the
//     candidates Pass 1 pruned per target, with reasons). A candidate that cannot be
//     composed carries `error` instead of `dag`;
//   - stage2_physical_asap_optimization: one physical candidate per composed logical
//     candidate (operator implementation; every sub-DAG NotMaterialized), no
//     cost; a composed logical candidate Stage 2 cannot implement is listed
//     under `pruned` with the reason;
//   - stage3_plan_selection: the selected candidate, chosen among all candidates;
//     the shown candidates' costs and accepted accuracy (per query: the
//     target's metric or null when not set, the accepted guarantee's
//     metric or null when exact or accepted regardless of guarantee,
//     whether the deployment's accuracy model accepted it directly,
//     whatever its guarantee says (`accepted_regardless_of_guarantee`),
//     the sketches read and a one-line
//     summary), and every other shown candidate under `rejected` with
//     its `cause`: a failed check (`failure_probability_resize_failed`,
//     `accuracy_target_missed`, `count_min_weights_not_proven_non_negative`,
//     `no_accuracy_guarantee`, `raw_data_not_retained`,
//     `latency_bound_exceeded`, `over_cluster_capacity`) or a loss under
//     the cost model (`costlier`, `slower`, `more_memory`,
//     `not_lowest_latency_on_pareto_front`, `dominated`,
//     `tied_lost_on_enumeration_order`). Each node's `cost` is one
//     computation's price times `multiplier`, the times it is computed:
//     once if materialized, else once per query (or materialized node)
//     that reads it, so a shared NotMaterialized node is charged once per
//     query. When selection fails, `selected` is null, with the rejections,
//     or with `error` for any other failure.
// When Pass 1 fails, the document still has the Stage 0 DAG; Stage 1 has
// `error` and no candidates, and nothing is selected.
//
// A workload with more than `--max-combinations` (default 10^4) Stage 1
// candidates, Pass 2's included, is refused as soon as enumeration passes
// the limit, and so is one whose Stage 2 physical candidates (one per
// materialization assignment, counted before they are built) pass it: the
// run exits with status 3 and the limit, and writes no document.
// Planning never caps or truncates the candidates it ranks; only the display
// does (`--max-displayed`).
//
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
// field is optional: by default every sketch family and readout the planner
// can build is supported (`supported_summaries` null), raw data is retained,
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
// `deployment_capabilities` shows the inputs used.
// `--measured-operation-costs <file>` reads the deployment's measured CPU
// time per sketch operation (a JSON array, as `convert_rqe_atomic_costs`
// writes from sketch-bench's `rqe_atomic_costs.json`); a sketch node with an
// entry is priced from it, and each node's `price_source` says `measured`
// or `analytical`.
// `--sample-interval-ms` is the sample interval declared for every metric the
// PromQL queries read (default 15000); the data declares no other facts.
// `--table '<json>'` (repeatable) declares a table the SQL queries read: `{"name": ...,
// "columns": [{"name": ..., "type": "timestamp|utf8|string|float64|double|
// int64|bigint", "nullable": true}], "time_index": 0}` (`nullable` defaults
// to true; without `time_index` the table has no time column; a
// `time_index` that is not a column index is an error).

use std::collections::{BTreeSet, HashSet};

use asap_devtools::candidate_labels::{candidate_labels, computation_label, query_of_each_target};
use asap_devtools::planner_layering_examples::{
    example1, example2, pattern_a, pattern_b, PatternARecurrence, SHARED_ROWS_PER_SECOND,
};
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_planning_stages_over, PlanningStagesError,
    PlanningStagesRun,
};
use asap_devtools::SqlCatalog;
use asap_frontend_promql::promql_metric_names;
use asap_logical_asap_optimization::per_computation_candidates::{
    ComputationCandidate, PerComputationCandidates, PruneReason,
};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::{
    ClusterLimits, ClusterObjective, CostModel, MeasuredOperationCost, PlanSelectionSettings,
    Rejection, ResourceUnitCost, ResourceUsage, Selection, SelectionError,
};
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::properties::{default_metric_for_root, AccuracyMetric, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    BatchEntry, DataArrival, DataWorkload, DurationMs, PlanningWorkload, Query, QueryLanguage,
    QueryRecurrence, QueryRequirements, QueryWorkload, RepeatedDemand, SqlDialect,
};
use serde_json::{json, Value};

const USAGE: &str =
    "usage: stage_pipeline (--example planner-layering-{1,2,3a,3b} | ((--promql | --sql) <query> \
[--name <name>]... [--table <json>...] [--max-error <f64> --failure-probability <f64> --accuracy-metric <metric>] [--sample-interval-ms <u64>])) \
[--cost-model usage-over-time|aws-fargate-us-east-1|provisioned-capacity|cluster-capacity [--cluster-memory-bytes <n> \
--cluster-disk-bytes <n> [--cluster-cpu-cores <n>] --cluster-objective lowest-latency|least-memory|pareto-front]] \
[--deployment-capabilities <json>] [--max-combinations <n>] [--max-displayed <n>] --out <file>";

/// The default `--max-combinations`: the most Stage 1 candidates (Pass 2's
/// included) a run enumerates.
const MAX_COMBINATIONS: usize = 10_000;

/// Why a run wrote no document.
enum Failure {
    /// Bad arguments or a query that does not plan (exit status 2).
    Usage(String),
    /// More Stage 1 candidates than `--max-combinations` (exit status 3).
    TooManyCandidates(PlanningStagesError),
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
        Err(Failure::TooManyCandidates(error)) => {
            eprintln!("stage_pipeline: {error}");
            std::process::exit(3);
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
    result: Result<PlanningStagesRun, PlanningStagesError>,
    max_displayed: usize,
) -> Result<Value, Failure> {
    let queries = with_default_metrics(queries, roots);
    match result {
        Ok(run) => Ok(stage_pipeline(queries, &run, max_displayed)),
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
            "stage3_plan_selection": { "costs": {}, "selected": null, "rejected": [] },
        })),
        Err(
            error @ (PlanningStagesError::TooManyCandidates { .. }
            | PlanningStagesError::TooManyPhysicalCandidates { .. }),
        ) => Err(Failure::TooManyCandidates(error)),
        Err(error @ PlanningStagesError::Validation(_)) => Err(error.to_string().into()),
    }
}

fn run(args: Vec<String>) -> Result<(), Failure> {
    let mut example = None;
    // (language, text, name) in workload order.
    let mut queries: Vec<(QueryLanguage, String, Option<String>)> = Vec::new();
    let mut tables = Vec::new();
    let (mut max_error, mut failure_probability, mut sample_interval_ms) = (None, None, 15_000u64);
    let mut accuracy_metric: Option<AccuracyMetric> = None;
    let mut max_displayed = 64usize;
    let mut max_combinations = MAX_COMBINATIONS;
    let mut cost_model: Option<String> = None;
    let (mut cluster_memory, mut cluster_disk, mut cluster_cores) = (None, None, None);
    let mut cluster_objective: Option<String> = None;
    let mut deployment_capabilities = DeploymentCapabilities::default();
    let mut out = None;
    let mut measured_operation_costs_path: Option<String> = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{v}: {e}"));
        match flag.as_str() {
            "--example" => example = Some(value()?),
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
            "--max-displayed" => max_displayed = value()?.parse().map_err(|e| format!("{e}"))?,
            "--max-combinations" => {
                max_combinations = value()?.parse().map_err(|e| format!("{e}"))?
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
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let has_sql = queries
        .iter()
        .any(|(language, _, _)| *language != QueryLanguage::PromQL);
    if !tables.is_empty() && !has_sql {
        return Err("--table needs --sql".into());
    }
    let out = out.ok_or("--out is required")?;
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
    let measured_operation_costs: Vec<MeasuredOperationCost> = match &measured_operation_costs_path
    {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("--measured-operation-costs {path}: {e}"))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("--measured-operation-costs {path}: {e}"))?
        }
        None => Vec::new(),
    };
    let plan_selection = PlanSelectionSettings {
        cost_model,
        measured_operation_costs,
        ..default_settings
    };
    let mut document = match (example.as_deref(), queries.is_empty()) {
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
            let roots = match workload.query_workload.language {
                QueryLanguage::PromQL => lower_promql_roots(&workload)?,
                _ => lower_sql_roots(&workload, &catalog)?,
            };
            let entries: Vec<_> = workload.query_workload.entries().collect();
            let data = workload.data_workload.clone().unwrap_or_default();
            let result = run_planning_stages_over(
                roots.clone(),
                &entries,
                &data,
                Some(max_combinations),
                &plan_selection,
                &deployment_capabilities,
            );
            document_of(workload_queries(&workload), &roots, result, max_displayed)?
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
            let data = DataWorkload::sampled_every(metrics, DurationMs(sample_interval_ms));
            let result = run_planning_stages_over(
                roots.clone(),
                &entries,
                &data,
                Some(max_combinations),
                &plan_selection,
                &deployment_capabilities,
            );
            document_of(described, &roots, result, max_displayed)?
        }
        _ => return Err("give exactly one of --example or --promql/--sql queries".into()),
    };
    // Read-only: the deployment inputs the run planned with.
    document["deployment_capabilities"] =
        serde_json::to_value(&deployment_capabilities).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&out, text).map_err(|e| format!("{out}: {e}").into())
}

/// The document for `run`, whose workload queries are `queries`.
fn stage_pipeline(queries: Vec<Value>, run: &PlanningStagesRun, max_displayed: usize) -> Value {
    // The display keeps the first `max_displayed` candidates and the
    // selected one; plan selection has already ranked all of them.
    let selected_logical = run
        .stage3_plan_selection
        .as_ref()
        .ok()
        .and_then(|selection| {
            run.stage2_physical_asap_optimization
                .iter()
                .find(|p| p.id == selection.selected)
                .map(|p| p.from_logical.as_str())
        });
    let shown: HashSet<&str> = run
        .stage1_logical_asap_optimization
        .iter()
        .enumerate()
        .filter(|(index, c)| *index < max_displayed || Some(c.id.as_str()) == selected_logical)
        .map(|(_, c)| c.id.as_str())
        .collect();
    let shown_physical: HashSet<&str> = run
        .stage2_physical_asap_optimization
        .iter()
        .filter(|p| shown.contains(p.from_logical.as_str()))
        .map(|p| p.id.as_str())
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
    json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": queries },
        "stage0_query_language_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "stage1_logical_asap_optimization": {
            "combinations": run.stage1_logical_asap_optimization.len(),
            "capped": candidates.len() < run.stage1_logical_asap_optimization.len(),
            "pass1_pruned": pass1_pruned_json(&run.stage1_pass1_per_computation),
            "candidates": candidates,
        },
        "stage2_physical_asap_optimization": { "candidates": physical, "pruned": pruned },
        "stage3_plan_selection": plan_selection_json(&run.stage3_plan_selection, |id| shown_physical.contains(id)),
    })
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
) -> Value {
    let rejections_json = |rejected: &[Rejection]| -> Vec<Value> {
        rejections_json(rejected.iter().filter(|r| shown(&r.id)))
    };
    let selection = match selection {
        Ok(selection) => selection,
        Err(SelectionError::NoValidCandidate(rejected)) => {
            return json!({ "costs": {}, "selected": null, "rejected": rejections_json(rejected) })
        }
        Err(error) => {
            return json!({ "costs": {}, "selected": null, "rejected": [], "error": error.to_string() })
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
        "front": selection.front,
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
            json!({
                "id": format!("q{}", index + 1),
                "language": language,
                "text": entry.query.0,
                "requirements": requirements,
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

/// The tables `--table` declares.
fn sql_catalog(tables: &[String]) -> Result<SqlCatalog, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use asap_devtools::planning_stages::run_planning_stages;

    /// With no valid candidate, the document is still written: `selected`
    /// is null and every rejection is listed.
    #[test]
    fn failed_selection_lists_rejections_without_a_selection() {
        let rejected = vec![Rejection {
            id: "P1".into(),
            cause: asap_plan_selection::RejectionCause::AccuracyTargetMissed,
            reason: "q1: misses target".into(),
        }];
        let document =
            plan_selection_json(&Err(SelectionError::NoValidCandidate(rejected)), |_| true);
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
            })
        );
    }

    /// With nothing else displayed, the document still shows the selected
    /// candidate and its cost, chosen among all candidates.
    #[test]
    fn display_cap_keeps_the_selected_candidate() {
        let workload = query_batch(
            QueryLanguage::PromQL,
            &["sum(m)".into()],
            AccuracyTarget::exact(),
            15_000,
        );
        let run =
            run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
        let document = stage_pipeline(workload_queries(&workload), &run, 0);
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
