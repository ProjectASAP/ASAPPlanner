// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --max-candidates 128 --out planner-layering-example1.json
// (also planner-layering-3a and planner-layering-3b: #509 Example 3,
// Patterns A and B; planner-layering-4a: Example 4, Pattern A repeated
// monthly)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --epsilon 0.01 --delta 0.001 --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages (#509 MVP):
//   - stage0_logical: the frontends' workload DAG, one root per query;
//   - stage1_logical_asap: Stage 1 workload candidates, one per choice of a
//     local alternative for every target (Pass 1, the Cartesian product), for
//     the queries as written and, when Pass 2's identical-expression rule
//     merges something, again with identical sub-DAGs shared ("· shared
//     input"), and, when the summary-capability rule applies, again with
//     one summary sized for its strictest consumer ("· shared summary"); in
//     enumeration order and capped by `--max-candidates` (default 64). A
//     repeating query's mergeable alternatives also come in tumbling panes
//     (Pass 2's window-composition rule), e.g. "Q1 Kll · tumbling 1m panes";
//   - stage2_physical_asap: per logical candidate, its physical candidates
//     (operator implementation; everything at query time, then one per
//     down-closed set of summaries maintained at ingestion time, labeled
//     e.g. "· ingestion time: Kll ×5 panes"), no cost;
//   - stage3_selection: per-candidate costs, the selected candidate, and
//     every other candidate as rejected (`valid: false`: inaccurate, over a
//     latency bound, needing a capability the deployment lacks, or could not
//     be built) or costlier.
//
// The deployment inputs are the built-in cost and accuracy models and the
// reference executor's capabilities (`asap_executor::capabilities`).
//
// Everything is the library's `plan_selection::plan_stages`, the function the
// facade runs; this tool only serializes it. Stage 3 here is over every
// displayed candidate; the facade's dynamic program selects the same winner
// when its assumptions hold.
//
// `--promql` may repeat. `--epsilon`/`--delta` apply to every `--promql`
// query; without them the queries are exact. `--interval-ms` is the source
// cadence PromQL needs (default 15000).

use std::rc::Rc;

use asap_logical_optimizer::pass1::logical_candidates::{
    choice_index, combination_count, LocalLogicalCandidates,
};
use asap_logical_optimizer::Realization;
use asap_plan_selection::PlanningModels;
use asap_plan_selection::{plan_stages, Selection, Sharing, MAX_ENUMERATED_CANDIDATES};
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::schema::{GroupingStrategy, SketchAlgorithm};
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, MetricType, PlanningWorkload, Predictability,
    Query, QueryLanguage, QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, RootDemand, TimeSelection, TimestampMs,
};
use serde_json::{json, Value};

const USAGE: &str =
    "usage: stage_pipeline (--example planner-layering-{1,3a,3b,4a} | --promql <query>... \
[--epsilon <f64> --delta <f64>] [--interval-ms <u64>]) [--max-candidates <n>] --out <file>";

fn main() {
    if let Err(message) = run(std::env::args().skip(1).collect()) {
        eprintln!("stage_pipeline: {message}\n{USAGE}");
        std::process::exit(2);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut example = None;
    let mut queries = Vec::new();
    let (mut epsilon, mut delta, mut interval_ms) = (None, None, 15_000u64);
    let mut max_candidates = MAX_ENUMERATED_CANDIDATES;
    let mut out = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{v}: {e}"));
        match flag.as_str() {
            "--example" => example = Some(value()?),
            "--promql" => queries.push(value()?),
            "--epsilon" => epsilon = Some(number(value()?)?),
            "--delta" => delta = Some(number(value()?)?),
            "--interval-ms" => interval_ms = value()?.parse().map_err(|e| format!("{e}"))?,
            "--max-candidates" => max_candidates = value()?.parse().map_err(|e| format!("{e}"))?,
            "--out" => out = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let workload = match (example.as_deref(), queries.is_empty()) {
        (Some("planner-layering-1"), true) => planner_layering_example1(),
        (Some("planner-layering-3a"), true) => planner_layering_example3a(),
        (Some("planner-layering-3b"), true) => planner_layering_example3b(),
        (Some("planner-layering-4a"), true) => planner_layering_example4a(),
        (Some(other), true) => return Err(format!("unknown example {other}")),
        (None, false) => {
            let accuracy = match (epsilon, delta) {
                (None, None) => AccuracyTarget::Exact,
                (Some(epsilon), None) => AccuracyTarget::Epsilon(epsilon),
                (Some(epsilon), Some(delta)) => AccuracyTarget::EpsilonDelta { epsilon, delta },
                (None, Some(_)) => return Err("--delta needs --epsilon".into()),
            };
            promql_batch(&queries, accuracy, interval_ms)
        }
        _ => return Err("give exactly one of --example or --promql".into()),
    };
    let out = out.ok_or("--out is required")?;
    let document = stage_pipeline(&workload, max_candidates)?;
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&out, text).map_err(|e| format!("{out}: {e}"))
}

fn stage_pipeline(workload: &PlanningWorkload, max_candidates: usize) -> Result<Value, String> {
    // PromQL rows carry each series' full identity as a column: the row
    // representation per-series state needs at runtime.
    let roots = asap_frontend_promql::lower_promql_query_workload(workload, 0)
        .map_err(|e| format!("lowering: {e}"))?
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => with_promql_series_identity(&node)
                .map(QueryRoot::Operator)
                .map_err(|e| format!("series identity: {e}")),
            scalar => Ok(scalar),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stage0 = export(&roots)?;
    let demand: Vec<RootDemand> = workload
        .query_workload
        .entries()
        .map(|entry| RootDemand::from(&entry))
        .collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let capabilities = asap_executor::capabilities();
    let run = plan_stages(
        roots.into_iter().enumerate().collect(),
        &demand,
        &data,
        PlanningModels::builtin().with_capabilities(&capabilities),
        max_candidates.max(1),
    )
    .map_err(|e| format!("planning: {e}"))?;
    let enumeration = run.enumeration.expect("display was requested");
    let combinations = enumeration.combinations;
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
        };
        if let Some(logical) = &candidate.logical {
            let roots: Vec<_> = logical.iter().map(|(_, root)| root.clone()).collect();
            candidates
                .push(json!({ "id": format!("L{index}"), "label": label, "dag": export(&roots)? }));
        }
        for p in &candidate.physical {
            let label = match p.materialization.as_str() {
                "" => label.clone(),
                m => format!("{label} · {m}"),
            };
            stage2.push(
                json!({ "id": p.id, "from_logical": p.from_logical, "label": label, "dag": p.dag }),
            );
        }
    }
    Ok(json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": workload_queries(workload) },
        "stage0_logical": { "dag": stage0 },
        "stage1_logical_asap": {
            "combinations": combinations,
            "capped": combinations > max_candidates,
            "candidates": candidates,
        },
        "stage2_physical_asap": { "candidates": stage2 },
        "stage3_selection": stage3_json(&enumeration.selection),
    }))
}

fn stage3_json(selection: &Selection) -> Value {
    let costs: serde_json::Map<_, _> = selection
        .costs
        .iter()
        .map(|(id, cost)| {
            let per_node: serde_json::Map<_, _> = cost
                .per_node
                .iter()
                .map(|(node, c)| {
                    (
                        node.to_string(),
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
        .map(|r| json!({ "id": r.id, "valid": r.valid, "reason": r.reason }))
        .collect();
    json!({ "costs": costs, "selected": selection.selected, "rejected": rejected })
}

/// A logical DAG as a flat node list: node `i` is `nodes[i]`, and children are
/// node indices inside each operator.
fn export(roots: &[QueryRoot]) -> Result<FlatDag, String> {
    for root in roots {
        root.validate_structure()
            .map_err(|e| format!("validation: {e}"))?;
    }
    Ok(flatten(roots).0)
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
                "language": "promql",
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

fn promql_batch(
    queries: &[String],
    accuracy: AccuracyTarget,
    interval_ms: u64,
) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
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
