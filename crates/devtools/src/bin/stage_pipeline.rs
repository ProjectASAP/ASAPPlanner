// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --out planner-layering-example1.json
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --epsilon 0.01 --delta 0.001 --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages (#509 MVP):
//   - stage0_logical: the frontends' workload DAG, one root per query;
//   - stage1_logical_asap: Pass 1 workload candidates, one per choice of a
//     local alternative for every target (the Cartesian product), in
//     enumeration order and capped by `--max-candidates` (default 64);
//   - stage2_physical_asap: one physical candidate per logical candidate
//     (operator implementation only, everything at query time), no cost;
//   - stage3_selection: per-candidate costs, the selected candidate, and
//     every other candidate as rejected (`valid: false`) or costlier.
//
// `--promql` may repeat. `--epsilon`/`--delta` apply to every `--promql`
// query; without them the queries are exact. `--interval-ms` is the source
// cadence PromQL needs (default 15000).

use std::rc::Rc;

use asap_aware_mapping::logical_candidates::{
    compose_logical_candidate, enumerate_local_logical_candidates, LocalLogicalCandidates,
};
use asap_aware_mapping::physical_candidates::stage2_physical;
use asap_aware_mapping::plan_selection::{stage3_select, Selection};
use asap_aware_mapping::{PlanningModels, Realization};
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::post_asap::SketchAlgorithm;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, TimeSelection,
};
use serde_json::{json, Value};

const USAGE: &str = "usage: stage_pipeline (--example planner-layering-1 | --promql <query>... \
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
    let mut max_candidates = 64usize;
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
    let inventory = enumerate_local_logical_candidates(roots.into_iter().enumerate().collect())
        .map_err(|e| format!("Pass 1: {e}"))?;
    let combinations = inventory
        .targets
        .iter()
        .map(|target| target.alternatives.len())
        .product::<usize>();
    let owners = target_owners(&inventory);
    let mut candidates = Vec::new();
    let mut physical = Vec::new();
    let mut choice = vec![0; inventory.targets.len()];
    for index in 0..combinations.min(max_candidates) {
        let roots = compose_logical_candidate(&inventory, &choice)
            .map_err(|e| format!("candidate {choice:?}: {e}"))?;
        let roots: Vec<_> = roots.into_iter().map(|(_, root)| root).collect();
        let (id, label) = (
            format!("L{}", index + 1),
            label(&inventory, &owners, &choice),
        );
        candidates.push(json!({ "id": id, "label": label, "dag": export(&roots)? }));
        let operators = roots
            .into_iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => Ok(node),
                QueryRoot::Scalar(_) => Err("Stage 2: scalar query roots are not physical yet"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut candidate =
            stage2_physical(&id, &operators).map_err(|e| format!("Stage 2 {id}: {e}"))?;
        candidate.id = format!("P{}", index + 1);
        candidate.label = label;
        physical.push(candidate);
        // Mixed-radix increment: the last target varies fastest.
        for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
            *digit += 1;
            if *digit < target.alternatives.len() {
                break;
            }
            *digit = 0;
        }
    }
    let targets: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| Some(entry.requirements.accuracy.target()))
        .collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let selection = stage3_select(&physical, &targets, &data, PlanningModels::builtin())
        .map_err(|e| format!("Stage 3: {e}"))?;
    let stage2: Vec<_> = physical
        .iter()
        .map(|p| json!({ "id": p.id, "from_logical": p.from_logical, "label": p.label, "dag": p.dag }))
        .collect();
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
        "stage3_selection": stage3_json(&selection),
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

/// E.g. "Q1 exact · Q2 CMS+heap"; exact accumulators are listed in parentheses.
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
                match &target.alternatives[index] {
                    Realization::PassThrough => {}
                    Realization::ExactAggregate { kind, .. } => {
                        accumulators.push(format!("{kind:?} acc"))
                    }
                    Realization::Sketch(kind) => sketches.push(match kind.algorithm() {
                        SketchAlgorithm::CmsWithHeap => "CMS+heap".to_string(),
                        SketchAlgorithm::CountSketchWithHeap => "CountSketch+heap".to_string(),
                        other => format!("{other:?}"),
                    }),
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
        }),
    }
}
