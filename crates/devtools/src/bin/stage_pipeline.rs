// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --out planner-layering-example1.json
// (also planner-layering-3a and planner-layering-3b: #509 Example 3,
// Patterns A and B)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --epsilon 0.01 --delta 0.001 --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages (#509 MVP):
//   - language_specific_frontends: the frontends' workload DAG, one root per query;
//   - logical_asap_optimization: Pass 1 whole-workload candidates, one per
//     choice of a per-computation candidate for every target (the Cartesian
//     product), in enumeration order and capped by `--max-candidates`
//     (default 64). A candidate that cannot be composed carries `error`
//     instead of `dag`;
//   - physical_asap_optimization: one physical candidate per composed logical
//     candidate (operator implementation; every sub-DAG NotMaterialized), no
//     cost; a composed logical candidate Stage 2 cannot implement is listed
//     under `pruned` with the reason;
//   - plan_selection: per-candidate costs, the selected candidate, and
//     every other candidate as rejected (`valid: false`) or costlier. Each
//     node's `cost` is one computation's price times `multiplier`, the
//     times it is computed: once if materialized, else once per query (or
//     materialized node) that reads it, so a shared NotMaterialized node is
//     charged once per query. When
//     selection fails, `selected` is null, with the rejections, or with
//     `error` for any other failure.
//
// `--promql` may repeat. `--epsilon`/`--delta` apply to every `--promql`
// query; without them the queries are exact. `--interval-ms` is the source
// cadence PromQL needs (default 15000).

use asap_aware_mapping::plan_selection::{Rejection, Selection, SelectionError};
use asap_devtools::planner_layering_examples::{
    declared, example1, pattern_a, pattern_b, PatternARecurrence,
};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages};
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::QueryRoot;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataWorkload, DurationMs, LatencyRequirement,
    PlanningWorkload, Query, QueryLanguage, QueryRecurrence, QueryRequirements, QueryWorkload,
    RepeatedDemand,
};
use serde_json::{json, Value};

const USAGE: &str =
    "usage: stage_pipeline (--example planner-layering-{1,3a,3b} | --promql <query>... \
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
        (Some("planner-layering-1"), true) => example1(),
        (Some("planner-layering-3a"), true) => {
            pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::Mixed)
        }
        (Some("planner-layering-3b"), true) => pattern_b(),
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
    let roots = lower_promql_roots(workload)?;
    let run = run_planning_stages(workload, roots, Some(max_candidates))?;
    // A choice that composition cannot substitute yet is listed with its
    // error rather than ending the run.
    let candidates: Vec<_> = run
        .logical
        .iter()
        .map(|c| match &c.roots {
            Ok(roots) => json!({ "id": c.id, "label": c.label, "dag": export(roots) }),
            Err(error) => json!({ "id": c.id, "label": c.label, "error": error }),
        })
        .collect();
    let physical: Vec<_> = run
        .physical
        .iter()
        .map(|p| json!({ "id": p.id, "from_logical": p.from_logical, "label": p.label, "dag": p.dag }))
        .collect();
    let pruned: Vec<_> = run
        .pruned
        .iter()
        .map(|p| json!({ "from_logical": p.from_logical, "label": p.label, "reason": p.reason }))
        .collect();
    Ok(json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": workload_queries(workload) },
        "language_specific_frontends": { "dag": export(&run.frontends) },
        "logical_asap_optimization": {
            "combinations": run.combinations,
            "capped": run.capped(),
            "candidates": candidates,
        },
        "physical_asap_optimization": { "candidates": physical, "pruned": pruned },
        "plan_selection": plan_selection_json(&run.selection),
    }))
}

fn plan_selection_json(selection: &Result<Selection, SelectionError>) -> Value {
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
        .map(|(id, cost)| {
            let per_node: serde_json::Map<_, _> = cost
                .per_node
                .iter()
                .map(|(node, c)| {
                    (
                        node.to_string(),
                        json!({ "cost": c.cost, "multiplier": c.multiplier, "detail": c.detail }),
                    )
                })
                .collect();
            (
                id.clone(),
                json!({ "total": cost.total, "unit": cost.unit, "source": cost.source, "per_node": per_node }),
            )
        })
        .collect();
    json!({
        "costs": costs,
        "selected": selection.selected,
        "rejected": rejections_json(&selection.rejected),
    })
}

fn rejections_json(rejected: &[Rejection]) -> Vec<Value> {
    rejected
        .iter()
        .map(|r| json!({ "id": r.id, "valid": r.valid, "reason": r.reason }))
        .collect()
}

/// A logical DAG as a flat node list: node `i` is `nodes[i]`, and children are
/// node indices inside each operator.
fn export(roots: &[QueryRoot]) -> FlatDag {
    flatten(roots).0
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

#[cfg(test)]
mod tests {
    use super::*;

    /// With no valid candidate, the document is still written: `selected`
    /// is null and every rejection is listed.
    #[test]
    fn failed_selection_lists_rejections_without_a_selection() {
        let rejected = vec![Rejection {
            id: "P1".into(),
            valid: false,
            reason: "q1: misses target".into(),
        }];
        let document = plan_selection_json(&Err(SelectionError::NoValidCandidate(rejected)));
        assert_eq!(
            document,
            json!({
                "costs": {},
                "selected": null,
                "rejected": [{ "id": "P1", "valid": false, "reason": "q1: misses target" }],
            })
        );
    }

    /// A run with no physical candidate (here, none enumerated) still emits
    /// every stage, with no selection.
    #[test]
    fn run_without_candidates_emits_plan_selection() {
        let workload = promql_batch(&["sum(m)".into()], AccuracyTarget::Exact, 15_000);
        let document = stage_pipeline(&workload, 0).unwrap();
        assert_eq!(document["physical_asap_optimization"]["pruned"], json!([]));
        assert_eq!(document["plan_selection"]["selected"], Value::Null);
        assert_eq!(document["plan_selection"]["rejected"], json!([]));
    }
}
