// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --max-candidates 160 --out planner-layering-example1.json
// (also planner-layering-2: #509 Example 2, three SQL statistics over
// `flows`; planner-layering-3a and planner-layering-3b: Example 3, Patterns A
// and B; planner-layering-4a: Example 4, Pattern A repeated monthly;
// planner-layering-4b: Example 4's Q49 crossover, Pattern B's hourly p99
// every 10 min on a deployment that does not keep raw data)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --epsilon 0.01 --delta 0.001 --out run.json
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --table '{"name": "flows", "columns": [{"name": "ts", "type": "timestamp"},
//               {"name": "src_ip", "type": "utf8", "nullable": false}], "time_index": 0}' \
//     --sql "SELECT COUNT(DISTINCT src_ip) FROM flows" --epsilon 0.02 --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages (#509 MVP):
//   - stage0_logical: the frontends' workload DAG, one root per query;
//   - stage1_logical_asap: Stage 1 workload candidates, one per choice of a
//     local alternative for every target (Pass 1, the Cartesian product), for
//     the queries as written and, when Pass 2's identical-expression rule
//     merges something, again with identical sub-DAGs shared ("· shared
//     input"), and, when the summary-capability rule applies, again with
//     one summary sized for its strictest consumer ("· shared summary"),
//     and, when queries read windows of one scan on a common grid, again
//     with one summary per shared segment ("· shared segments"); in
//     enumeration order, only those with a written physical candidate. A
//     repeating query's mergeable alternatives also come in tumbling panes
//     (Pass 2's window-composition rule), e.g. "Q1 Kll · tumbling 1m panes";
//   - stage2_physical_asap: per logical candidate, its physical candidates
//     (operator implementation; everything at query time, then one per
//     down-closed set of summaries maintained at ingestion time, labeled
//     e.g. "· ingestion time: Kll ×5 panes"), no cost;
//   - stage3_selection: per-candidate costs, the selected candidate, and
//     every other written candidate as rejected (`valid: false`: inaccurate,
//     over a latency bound, needing a capability the deployment lacks, or
//     could not be built) or costlier.
//   - shown_of: present when not every plan is written, the totals.
//
// Stage 3 prices every candidate (up to PRICE_LIMIT combinations), so the
// selection does not depend on `--max-candidates`; that flag only limits
// how many plans the document carries (default 64): the cheapest first,
// then invalid ones in enumeration order.
//
// The deployment inputs are the built-in cost and accuracy models and the
// reference executor's capabilities (`asap_executor::capabilities`; without
// raw data retention for planner-layering-4b).
//
//   - deployment: the deployment inputs Stage 3 used (the executor's
//     capabilities, summarized; the cost model and its Stage3Calibration;
//     the accuracy model's name).
//
// Everything is the library's `plan_selection::plan_stages`, the function the
// facade runs; this tool only serializes it. Stage 3 here is over every
// displayed candidate; the facade's dynamic program selects the same winner
// when its assumptions hold.
//
// `--promql` and `--sql` may repeat; a run is one language. `--epsilon`/
// `--delta` apply to every query; without them the queries are exact.
// `--interval-ms` is the source cadence PromQL needs (default 15000).
// `--table '<json>'` (repeatable) declares a table SQL queries read:
// `{"name": ..., "columns": [{"name": ..., "type": "timestamp|utf8|string|
// float64|double|int64|bigint", "nullable": true}], "time_index": 0}`
// (`nullable` defaults to true; without `time_index` the table has no time
// column). There are no default tables.

use std::collections::HashSet;
use std::rc::Rc;

use asap_frontend_sql::SqlCatalog;
use asap_logical_optimizer::pass1::logical_candidates::{
    choice_index, combination_count, LocalLogicalCandidates,
};
use asap_logical_optimizer::Realization;
use asap_plan_selection::{
    plan_stages, Selection, Sharing, COST_MODEL, COST_PER_SECOND, MAX_ENUMERATED_CANDIDATES,
};
use asap_plan_selection::{DeploymentCapabilities, PlanningModels};
use asap_types::ir::export::{
    compile_logical_asap_workload, LogicalASAPDAG, LogicalASAPDAGDocument,
};
use asap_types::ir::schema::{DataType, Field, GroupingStrategy, Schema, SketchAlgorithm};
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, MetricType, PlanningWorkload, Predictability,
    Query, QueryLanguage, QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, RootDemand, SqlDialect, TimeSelection,
    TimestampMs,
};
use serde_json::{json, Value};

const USAGE: &str =
    "usage: stage_pipeline (--example planner-layering-{1,2,3a,3b,4a,4b} | (--promql <query>... \
| --table <json>... --sql <query>...) [--epsilon <f64> --delta <f64>] [--interval-ms <u64>]) \
[--max-candidates <n>] --out <file>";

fn main() {
    if let Err(message) = run(std::env::args().skip(1).collect()) {
        eprintln!("stage_pipeline: {message}\n{USAGE}");
        std::process::exit(2);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut example = None;
    let (mut promql, mut sql, mut tables) = (Vec::new(), Vec::new(), Vec::new());
    let (mut epsilon, mut delta, mut interval_ms) = (None, None, 15_000u64);
    let mut max_candidates = MAX_ENUMERATED_CANDIDATES;
    let mut out = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{v}: {e}"));
        match flag.as_str() {
            "--example" => example = Some(value()?),
            "--promql" => promql.push(value()?),
            "--sql" => sql.push(value()?),
            "--table" => tables.push(value()?),
            "--epsilon" => epsilon = Some(number(value()?)?),
            "--delta" => delta = Some(number(value()?)?),
            "--interval-ms" => interval_ms = value()?.parse().map_err(|e| format!("{e}"))?,
            "--max-candidates" => max_candidates = value()?.parse().map_err(|e| format!("{e}"))?,
            "--out" => out = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let (language, queries) = match (promql.is_empty(), sql.is_empty()) {
        (false, false) => {
            return Err("a run is one language: give --promql or --sql, not both".into())
        }
        (true, false) => (QueryLanguage::SQL(SqlDialect::DataFusionSQL), sql),
        _ => (QueryLanguage::PromQL, promql),
    };
    if !tables.is_empty() && !matches!(language, QueryLanguage::SQL(_)) {
        return Err("--table needs --sql".into());
    }
    let workload = match (example.as_deref(), queries.is_empty()) {
        (Some("planner-layering-1"), true) => planner_layering_example1(),
        (Some("planner-layering-2"), true) => {
            tables = vec![FLOWS.into()];
            planner_layering_example2()
        }
        (Some("planner-layering-3a"), true) => planner_layering_example3a(),
        (Some("planner-layering-3b"), true) => planner_layering_example3b(),
        (Some("planner-layering-4a"), true) => planner_layering_example4a(),
        (Some("planner-layering-4b"), true) => planner_layering_example4b(),
        (Some(other), true) => return Err(format!("unknown example {other}")),
        (None, false) => {
            let accuracy = match (epsilon, delta) {
                (None, None) => AccuracyTarget::Exact,
                (Some(epsilon), None) => AccuracyTarget::Epsilon(epsilon),
                (Some(epsilon), Some(delta)) => AccuracyTarget::EpsilonDelta { epsilon, delta },
                (None, Some(_)) => return Err("--delta needs --epsilon".into()),
            };
            query_batch(language, &queries, accuracy, interval_ms)
        }
        _ => return Err("give exactly one of --example, --promql or --sql".into()),
    };
    let catalog = sql_catalog(&tables)?;
    let out = out.ok_or("--out is required")?;
    let capabilities = DeploymentCapabilities {
        // Example 4b's crossover needs a deployment that does not keep raw data.
        raw_data_retained: example.as_deref() != Some("planner-layering-4b"),
        ..asap_executor::capabilities()
    };
    let document = stage_pipeline(&workload, &catalog, &capabilities, max_candidates)?;
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&out, text).map_err(|e| format!("{out}: {e}"))
}

/// Most Stage 1 combinations built and priced; beyond it the selection is
/// over the first PRICE_LIMIT only, and the tool warns.
const PRICE_LIMIT: usize = 4096;

fn stage_pipeline(
    workload: &PlanningWorkload,
    catalog: &SqlCatalog,
    capabilities: &DeploymentCapabilities,
    max_candidates: usize,
) -> Result<Value, String> {
    let roots = match workload.query_workload.language {
        QueryLanguage::SQL(_) => tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(asap_frontend_sql::lower_sql_batch(
                &workload.query_workload,
                catalog,
            ))
            .into_iter()
            .map(|root| {
                root.map(QueryRoot::Operator)
                    .map_err(|e| format!("lowering: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        // PromQL rows carry each series' full identity as a column: the row
        // representation per-series state needs at runtime.
        _ => asap_frontend_promql::lower_promql_query_workload(workload, 0)
            .map_err(|e| format!("lowering: {e}"))?
            .into_iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => with_promql_series_identity(&node)
                    .map(QueryRoot::Operator)
                    .map_err(|e| format!("series identity: {e}")),
                scalar => Ok(scalar),
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let stage0 = export(&roots)?;
    let demand: Vec<RootDemand> = workload
        .query_workload
        .entries()
        .map(|entry| RootDemand::from(&entry))
        .collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let models = PlanningModels::builtin().with_capabilities(capabilities);
    let run = plan_stages(
        roots.into_iter().enumerate().collect(),
        &demand,
        &data,
        models,
        PRICE_LIMIT,
    )
    .map_err(|e| format!("planning: {e}"))?;
    let enumeration = run.enumeration.expect("display was requested");
    let combinations = enumeration.combinations;
    if combinations > PRICE_LIMIT {
        eprintln!("stage_pipeline: priced the first {PRICE_LIMIT} of {combinations} combinations");
    }
    let selection = &enumeration.selection;
    // Every plan id, physical candidates then those that could not be
    // built; the cheapest `max_candidates` are written.
    let physical_ids: Vec<&str> = enumeration
        .candidates
        .iter()
        .flat_map(|c| c.physical.iter().map(|p| p.id.as_str()))
        .collect();
    let mut ranked = physical_ids.clone();
    let known: HashSet<&str> = ranked.iter().copied().collect();
    ranked.extend(
        selection
            .rejected
            .iter()
            .map(|r| r.id.as_str())
            .filter(|id| !known.contains(id)),
    );
    let total = |id: &str| selection.costs.get(id).map_or(f64::INFINITY, |c| c.total);
    ranked.sort_by(|a, b| total(a).total_cmp(&total(b)));
    let shown: HashSet<&str> = ranked.iter().take(max_candidates.max(1)).copied().collect();
    let mut candidates = Vec::new();
    let mut stage2 = Vec::new();
    for candidate in &enumeration.candidates {
        // Candidates of each variant are numbered after the previous variants'.
        let mut offset = 0;
        let variant = run
            .stage1
            .iter()
            .find(|v| {
                let found = v.sharing == candidate.sharing;
                if !found {
                    offset += combination_count(&v.inventory);
                }
                found
            })
            .expect("the candidate's variant");
        let inventory = &variant.inventory;
        let index = offset + choice_index(inventory, &candidate.choice) + 1;
        let mut label = label(inventory, &target_owners(inventory), &candidate.choice);
        label += match candidate.sharing {
            Sharing::Independent => "",
            Sharing::IdenticalExpressions => " · shared input",
            Sharing::SummaryCapability => " · shared summary",
            Sharing::WindowSegments => " · shared segments",
        };
        let written = candidate
            .physical
            .iter()
            .any(|p| shown.contains(p.id.as_str()))
            || shown.contains(format!("P{index}").as_str());
        if !written {
            continue;
        }
        if let Some(logical) = &candidate.logical {
            let roots: Vec<_> = logical.iter().map(|(_, root)| root.clone()).collect();
            candidates
                .push(json!({ "id": format!("L{index}"), "label": label, "dag": export(&roots)? }));
        }
        for p in candidate
            .physical
            .iter()
            .filter(|p| shown.contains(p.id.as_str()))
        {
            let label = match p.materialization.as_str() {
                "" => label.clone(),
                m => format!("{label} · {m}"),
            };
            stage2.push(
                json!({ "id": p.id, "from_logical": p.from_logical, "label": label, "dag": p.dag }),
            );
        }
    }
    let mut document = json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": workload_queries(workload) },
        "deployment": deployment_json(&models),
        "stage0_logical": { "dag": stage0 },
        "stage1_logical_asap": {
            "combinations": combinations,
            "capped": shown.len() < ranked.len(),
            "candidates": candidates,
        },
        "stage2_physical_asap": { "candidates": stage2 },
        "stage3_selection": stage3_json(selection, &shown),
    });
    if shown.len() < ranked.len() {
        document["shown_of"] = json!({
            "logical": enumeration.candidates.len(),
            "physical": physical_ids.len(),
            "priced": selection.costs.len(),
        });
    }
    Ok(document)
}

/// The deployment inputs Stage 3 used: the executor's capabilities, one
/// line per summary, and the cost and accuracy models.
fn deployment_json(models: &PlanningModels<'_>) -> Value {
    let caps = models.capabilities;
    let summaries = caps.summaries.as_ref().map(|summaries| {
        summaries
            .iter()
            .map(|s| {
                let readouts: Vec<_> = s.readouts.iter().map(|r| format!("{r:?}")).collect();
                json!({ "summary": s.name(), "readouts": readouts })
            })
            .collect::<Vec<_>>()
    });
    let c = &models.calibration;
    json!({
        "capabilities": {
            "source": "asap_executor::capabilities",
            "summaries": summaries,
            "ingestion_time": caps.ingestion_time,
            "query_time_retention": caps.query_time_retention,
            "memory_budget_bytes": caps.memory_budget_bytes,
            "raw_data_retained": caps.raw_data_retained,
            "raw_bytes_per_sample": caps.raw_bytes_per_sample,
        },
        "cost_model": {
            "name": COST_MODEL,
            "unit": COST_PER_SECOND,
            "calibration": {
                "version": c.version,
                "cost_per_cpu_op": c.cost_per_cpu_op,
                "cost_per_scan_byte": c.cost_per_scan_byte,
                "cost_per_retained_byte_second": c.cost_per_retained_byte_second,
                "horizon_s": c.horizon_s,
                "latency_ms_per_cost_unit": c.latency_ms_per_cost_unit,
            },
        },
        // `PlanningModels::builtin()`'s accuracy model and evidence.
        "accuracy_model": { "name": "DefaultAccuracyModel", "evidence": "none" },
    })
}

/// Stage 3's outcome for the written plans `shown`.
fn stage3_json(selection: &Selection, shown: &HashSet<&str>) -> Value {
    let costs: serde_json::Map<_, _> = selection
        .costs
        .iter()
        .filter(|(id, _)| shown.contains(id.as_str()))
        .map(|(id, cost)| {
            let per_node: serde_json::Map<_, _> = cost
                .per_node
                .iter()
                .map(|(node, c)| {
                    (
                        node.0.to_string(),
                        json!({ "cost": c.cost, "detail": c.detail }),
                    )
                })
                .collect();
            (
                id.clone(),
                json!({ "total": cost.total, "unit": cost.unit, "source": cost.source, "per_node": per_node }),
            )
        })
        .collect();
    let rejected: Vec<_> = selection
        .rejected
        .iter()
        .filter(|r| shown.contains(r.id.as_str()))
        .map(|r| json!({ "id": r.id, "valid": r.valid, "reason": r.reason }))
        .collect();
    json!({ "costs": costs, "selected": selection.selected, "rejected": rejected })
}

fn export(roots: &[QueryRoot]) -> Result<LogicalASAPDAG, String> {
    let dag = compile_logical_asap_workload(roots).map_err(|e| format!("export: {e}"))?;
    LogicalASAPDAGDocument::new(dag.clone())
        .validate()
        .map_err(|e| format!("export validation: {e}"))?;
    Ok(dag)
}

/// The first query whose plan reaches each target (enumeration order).
fn target_owners(inventory: &LocalLogicalCandidates<usize>) -> Vec<usize> {
    inventory
        .targets
        .iter()
        .map(|target| {
            inventory
                .roots
                .iter()
                .position(|(_, root)| {
                    let operators = match root {
                        QueryRoot::Operator(node) => vec![node],
                        QueryRoot::Scalar(expr) => expr.operator_refs(),
                    };
                    operators.into_iter().any(|node| {
                        OperatorNode::reachable(node)
                            .iter()
                            .any(|n| Rc::ptr_eq(n, &target.target))
                    })
                })
                .expect("every target is reached from a root")
        })
        .collect()
}

/// E.g. "Q1 exact · Q2 CMS+heap"; exact accumulators are listed in
/// parentheses. A sketch that absorbs the aggregate beneath it is
/// "whole-expression"; one built in tumbling panes names them, e.g.
/// "Kll · tumbling 1m panes".
fn label(inventory: &LocalLogicalCandidates<usize>, owners: &[usize], choice: &[usize]) -> String {
    (0..inventory.roots.len())
        .map(|query| {
            let mut sketches = Vec::new();
            let mut accumulators = Vec::new();
            for (target, (&owner, &index)) in
                inventory.targets.iter().zip(owners.iter().zip(choice))
            {
                if owner != query {
                    continue;
                }
                let window = target.windows[index]
                    .label()
                    .map_or(String::new(), |form| format!(" · {form}"));
                match &target.alternatives[index] {
                    Realization::PassThrough => {}
                    Realization::ExactAggregate { kind, .. } => {
                        accumulators.push(format!("{kind:?} acc{window}"))
                    }
                    Realization::Sketch(kind) => {
                        let name = match kind.algorithm() {
                            SketchAlgorithm::CmsWithHeap => "CMS+heap".to_string(),
                            SketchAlgorithm::CountSketchWithHeap => "CountSketch+heap".to_string(),
                            other => format!("{other:?}"),
                        };
                        let name = match target.groupings[index] {
                            GroupingStrategy::PerSubpopulationInstance => name,
                            GroupingStrategy::SharedMultiSubpopulation { ref kind, .. } => {
                                format!("{kind:?}")
                            }
                        };
                        // The sketch reads the inner aggregate's input and replaces it.
                        sketches.push(match target.absorbs[index] {
                            Some(_) => format!("whole-expression {name}"),
                            None => format!("{name}{window}"),
                        });
                    }
                    other => sketches.push(format!("{other:?}")),
                }
            }
            let mut text = format!("Q{} ", query + 1);
            text += &if sketches.is_empty() {
                "exact".to_string()
            } else {
                sketches.join(" + ")
            };
            if !accumulators.is_empty() {
                text += &format!(" ({})", accumulators.join(", "));
            }
            text
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn workload_queries(workload: &PlanningWorkload) -> Vec<Value> {
    let language = match workload.query_workload.language {
        QueryLanguage::SQL(_) => "sql",
        _ => "promql",
    };
    workload
        .query_workload
        .entries()
        .enumerate()
        .map(|(index, entry)| {
            let accuracy = match entry.requirements.accuracy.target() {
                AccuracyTarget::Exact => json!("exact"),
                AccuracyTarget::Epsilon(epsilon) => json!({ "epsilon": epsilon }),
                AccuracyTarget::EpsilonDelta { epsilon, delta } => {
                    json!({ "epsilon": epsilon, "delta": delta })
                }
            };
            let mut requirements = json!({ "accuracy": accuracy });
            if let LatencyRequirement::ExplicitMaxMs(ms) = entry.requirements.response_latency {
                requirements["latency_ms"] = json!(ms);
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

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

fn query_batch(
    language: QueryLanguage,
    queries: &[String],
    accuracy: AccuracyTarget,
    interval_ms: u64,
) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language,
            query_batch: Some(
                queries
                    .iter()
                    .map(|query| BatchEntry {
                        query: Query(query.clone()),
                        requirements: QueryRequirements {
                            accuracy: AccuracyRequirement::Explicit(accuracy.clone()),
                            ..Default::default()
                        },
                        predictability: Default::default(),
                        invocations: 1,
                        execute_at: None,
                        time_selection: Default::default(),
                    })
                    .collect(),
            ),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: declared(DurationMs(interval_ms)),
            ..Default::default()
        }),
    }
}

/// #509 Example 1 (docs/design_docs/proposals/planner-layering.md) over its
/// shared data workload; matches the acceptance spec's test workload.
fn planner_layering_example1() -> PlanningWorkload {
    let panel = |query: &str, accuracy, response_latency| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(accuracy),
            response_latency,
        },
        predictability: Predictability::Predictable { known_at: None },
        time_selection: TimeSelection {
            scope: QueryTimeScope::RealTime,
            lookback: Some(DurationMs(60_000)),
            as_of: None,
        },
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![
                panel(
                    "sum by (job) (rate(http_requests_total[1m]))",
                    AccuracyTarget::Exact,
                    LatencyRequirement::Unspecified,
                ),
                panel(
                    "topk by (job) (10, sum_over_time(http_requests_total[1m]))",
                    AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.001,
                    },
                    LatencyRequirement::ExplicitMaxMs(100.0),
                ),
            ]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: declared(DurationMs(15_000)),
            ingestion_volume: Evidence::default(),
            ingestion_rate: declared(Rate(1_000_000.0 / 15.0)),
            input_cardinality: declared(1_000_000),
            distribution: declared(DataDistribution::Zipf),
            // `http_requests_total` is a counter: its samples are never negative.
            metric_types: [("http_requests_total".into(), MetricType::Counter)].into(),
        }),
    }
}

/// #509 Example 2's `flows` table, as a `--table`.
const FLOWS: &str = r#"{"name": "flows", "columns": [
    {"name": "ts", "type": "timestamp", "nullable": false},
    {"name": "src_ip", "type": "utf8", "nullable": false}]}"#;

/// The SQL catalog of the `--table` declarations.
fn sql_catalog(tables: &[String]) -> Result<SqlCatalog, String> {
    let mut catalog = SqlCatalog::new();
    let mut names = HashSet::new();
    for raw in tables {
        let bad = |what: &str| format!("--table {raw}: {what}");
        let value: Value = serde_json::from_str(raw).map_err(|e| bad(&e.to_string()))?;
        let name = value["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| bad("name must be a non-empty string"))?;
        if !names.insert(name.to_string()) {
            return Err(bad("the table is declared twice"));
        }
        let columns = value["columns"]
            .as_array()
            .filter(|columns| !columns.is_empty())
            .ok_or_else(|| bad("columns must be a non-empty array"))?
            .iter()
            .map(|column| {
                let column_name = column["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| bad("column.name must be a non-empty string"))?;
                let data_type = match column["type"].as_str().map(str::to_ascii_lowercase) {
                    Some(t) if t == "timestamp" => DataType::Timestamp,
                    Some(t) if t == "utf8" || t == "string" => DataType::Utf8,
                    Some(t) if t == "float64" || t == "double" => DataType::Float64,
                    Some(t) if t == "int64" || t == "bigint" => DataType::Int64,
                    _ => return Err(bad(&format!("unsupported column type {}", column["type"]))),
                };
                let nullable = column["nullable"].as_bool().unwrap_or(true);
                Ok(Field::plain(column_name, data_type, nullable))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let schema = match value.get("time_index") {
            None | Some(Value::Null) => Schema::new(columns),
            Some(index) => match index.as_u64().map(|i| i as usize) {
                Some(i) if i < columns.len() => Schema::with_time_index(columns, i, vec![]),
                _ => return Err(bad("time_index must be a column index")),
            },
        };
        catalog = catalog.with_table(name, schema);
    }
    Ok(catalog)
}

/// #509 Example 2: distinct count, entropy and L2 of `src_ip` over the last
/// minute, one-time, as in `planner_layering_example2.rs`. Q3 casts its
/// product to DOUBLE: the design's Int64 `c * c` can overflow, so the L2 rule
/// does not accept it.
fn planner_layering_example2() -> PlanningWorkload {
    const WINDOW: &str = "WHERE ts >= now() - INTERVAL '1 minute'";
    let queries = [
        (format!("SELECT COUNT(DISTINCT src_ip) FROM flows {WINDOW}"), 0.02),
        (
            format!("SELECT -SUM(p * LN(p)) FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows {WINDOW} GROUP BY src_ip)"),
            0.05,
        ),
        (
            format!("SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (SELECT src_ip, COUNT(*) AS c FROM flows {WINDOW} GROUP BY src_ip)"),
            0.01,
        ),
    ];
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(
                queries
                    .into_iter()
                    .map(|(query, epsilon)| BatchEntry {
                        query: Query(query),
                        requirements: QueryRequirements {
                            accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                                epsilon,
                                delta: 0.01,
                            }),
                            ..Default::default()
                        },
                        predictability: Default::default(),
                        invocations: 1,
                        execute_at: None,
                        time_selection: Default::default(),
                    })
                    .collect(),
            ),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: declared(Rate(100_000.0)),
            input_cardinality: declared(10_000_000),
            ..Default::default()
        }),
    }
}

/// The shared data workload of #509 with `arrival`.
fn shared_data_workload(arrival: DataArrival) -> DataWorkload {
    DataWorkload {
        arrival,
        data_ingestion_interval: declared(DurationMs(15_000)),
        ingestion_volume: Evidence::default(),
        ingestion_rate: declared(Rate(1_000_000.0 / 15.0)),
        input_cardinality: declared(1_000_000),
        distribution: declared(DataDistribution::Zipf),
        metric_types: Default::default(),
    }
}

const YEAR_MS: u64 = 365 * 24 * 3_600_000;
/// Pattern A's batch time T (2026-01-01T00:00:00Z).
const T_MS: u64 = 1_767_225_600_000;
/// #509 Example 3, Pattern A: five p99 reports, (PromQL, lookback, T − as_of).
const PATTERN_A: [(&str, u64, u64); 5] = [
    ("quantile_over_time(0.99, latency_ms[5y])", 5 * YEAR_MS, 0),
    ("quantile_over_time(0.99, latency_ms[1y])", YEAR_MS, 0),
    (
        "quantile_over_time(0.99, latency_ms[1y] offset 1y)",
        YEAR_MS,
        YEAR_MS,
    ),
    (
        "quantile_over_time(0.99, latency_ms[1y] offset 2y)",
        YEAR_MS,
        2 * YEAR_MS,
    ),
    (
        "quantile_over_time(0.99, latency_ms[3y] offset 2y)",
        3 * YEAR_MS,
        2 * YEAR_MS,
    ),
];

fn pattern_a_requirements() -> QueryRequirements {
    QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
            epsilon: 0.005,
            delta: 0.01,
        }),
        response_latency: LatencyRequirement::Unspecified,
    }
}

/// #509 Example 3, Pattern A: an ad hoc batch of five p99 reports over
/// historical intervals, run once at T (2026-01-01), over mixed data.
fn planner_layering_example3a() -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(
                PATTERN_A
                    .into_iter()
                    .map(|(query, lookback, before_t)| BatchEntry {
                        query: Query(query.into()),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::AdHoc,
                        invocations: 1,
                        execute_at: Some(TimestampMs(T_MS)),
                        time_selection: TimeSelection {
                            scope: QueryTimeScope::Longitudinal,
                            lookback: Some(DurationMs(lookback)),
                            as_of: Some(TimestampMs(T_MS - before_t)),
                        },
                    })
                    .collect(),
            ),
            repeating_queries: None,
        },
        data_workload: Some(shared_data_workload(DataArrival::Mixed)),
    }
}

/// #509 Example 4, Pattern A repeated monthly and `Predictable { known_at: T }`:
/// each run reads the intervals ending at its own evaluation time, over mixed
/// data. Example 4's other variants are Example 3's workloads (Pattern B is
/// `planner-layering-3b`).
fn planner_layering_example4a() -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(
                PATTERN_A
                    .into_iter()
                    .map(|(query, lookback, _)| RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                            30 * 24 * 3_600_000,
                        )),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::Predictable {
                            known_at: Some(TimestampMs(T_MS)),
                        },
                        time_selection: TimeSelection {
                            scope: QueryTimeScope::Longitudinal,
                            lookback: Some(DurationMs(lookback)),
                            as_of: None,
                        },
                    })
                    .collect(),
            ),
        },
        data_workload: Some(shared_data_workload(DataArrival::Mixed)),
    }
}

/// #509 Example 3, Pattern B: a p99 panel over the last 5 min, every minute.
fn planner_layering_example3b() -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query("quantile_over_time(0.99, latency_ms[5m])".into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.01,
                    }),
                    response_latency: LatencyRequirement::ExplicitMaxMs(200.0),
                },
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection {
                    scope: QueryTimeScope::RealTime,
                    lookback: Some(DurationMs(300_000)),
                    as_of: None,
                },
            }]),
        },
        data_workload: Some(shared_data_workload(DataArrival::ContinuouslyIngesting)),
    }
}

/// #509 Example 4's Q49 crossover (`stage3_b_panes_smaller_than_their_raw_data_win`):
/// Pattern B's p99 over the last hour every 10 min, over 1,000 series sampled
/// every second. Run on a deployment that does not keep raw data, where the
/// 10-min KLL panes are smaller than the samples they cover.
fn planner_layering_example4b() -> PlanningWorkload {
    let mut workload = planner_layering_example3b();
    let entry = &mut workload
        .query_workload
        .repeating_queries
        .as_mut()
        .expect("Pattern B repeats")[0];
    entry.query = Query("quantile_over_time(0.99, latency_ms[60m])".into());
    entry.demand = RepeatedDemand::FixedInterval(RepetitionInterval(600_000));
    entry.time_selection.lookback = Some(DurationMs(3_600_000));
    let data = workload.data_workload.as_mut().expect("Pattern B's data");
    data.data_ingestion_interval = declared(DurationMs(1_000));
    data.ingestion_rate = declared(Rate(1_000.0));
    data.input_cardinality = declared(1_000);
    workload
}
