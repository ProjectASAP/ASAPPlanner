// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --out planner-layering-example1.json
// (also planner-layering-3a and planner-layering-3b: #509 Example 3,
// Patterns A and B; 3a has 243 candidates, all priced, 64 shown by default)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --epsilon 0.01 --delta 0.001 --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages (#509 MVP):
//   - language_specific_frontends: the frontends' workload DAG, one root per query;
//   - logical_asap_optimization: Pass 1 whole-workload candidates, one per
//     choice of a per-computation candidate for every target (the Cartesian
//     product), in enumeration order. Every candidate is optimized and
//     priced; the document shows the first `--max-displayed` (default 64)
//     and the selected one, with `combinations` (how many exist) and
//     `capped` (whether some are not shown). A candidate that cannot be
//     composed carries `error` instead of `dag`;
//   - physical_asap_optimization: one physical candidate per composed logical
//     candidate (operator implementation; every sub-DAG NotMaterialized), no
//     cost; a composed logical candidate Stage 2 cannot implement is listed
//     under `pruned` with the reason;
//   - plan_selection: the selected candidate, chosen among all candidates;
//     the shown candidates' costs, and every other shown candidate as
//     rejected (`valid: false`) or costlier. Each node's `cost` is one
//     computation's price times `multiplier`, the times it is computed:
//     once if materialized, else once per query (or materialized node)
//     that reads it, so a shared NotMaterialized node is charged once per
//     query. When selection fails, `selected` is null, with the rejections,
//     or with `error` for any other failure.
//
// `--promql` may repeat. `--epsilon`/`--delta` apply to every `--promql`
// query; without them the queries are exact. `--interval-ms` is the source
// cadence PromQL needs (default 15000).

use std::collections::HashSet;

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
[--epsilon <f64> --delta <f64>] [--interval-ms <u64>]) [--max-displayed <n>] --out <file>";

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
    let mut max_displayed = 64usize;
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
            "--max-displayed" => max_displayed = value()?.parse().map_err(|e| format!("{e}"))?,
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
    let document = stage_pipeline(&workload, max_displayed)?;
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&out, text).map_err(|e| format!("{out}: {e}"))
}

fn stage_pipeline(workload: &PlanningWorkload, max_displayed: usize) -> Result<Value, String> {
    let roots = lower_promql_roots(workload)?;
    let run = run_planning_stages(workload, roots)?;
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
    // A choice that composition cannot substitute yet is listed with its
    // error rather than ending the run.
    let candidates: Vec<_> = run
        .stage1_logical_asap_optimization
        .iter()
        .filter(|c| shown.contains(c.id.as_str()))
        .map(|c| match &c.roots {
            Ok(roots) => json!({ "id": c.id, "label": c.label, "dag": export(roots) }),
            Err(error) => json!({ "id": c.id, "label": c.label, "error": error }),
        })
        .collect();
    let physical: Vec<_> = run
        .stage2_physical_asap_optimization
        .iter()
        .filter(|p| shown_physical.contains(p.id.as_str()))
        .map(|p| json!({ "id": p.id, "from_logical": p.from_logical, "label": p.label, "dag": p.dag }))
        .collect();
    let pruned: Vec<_> = run
        .stage2_pruned
        .iter()
        .filter(|p| shown.contains(p.from_logical.as_str()))
        .map(|p| json!({ "from_logical": p.from_logical, "label": p.label, "reason": p.reason }))
        .collect();
    Ok(json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": workload_queries(workload) },
        "language_specific_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "logical_asap_optimization": {
            "combinations": run.stage1_logical_asap_optimization.len(),
            "capped": candidates.len() < run.stage1_logical_asap_optimization.len(),
            "candidates": candidates,
        },
        "physical_asap_optimization": { "candidates": physical, "pruned": pruned },
        "plan_selection": plan_selection_json(&run.stage3_plan_selection, |id| shown_physical.contains(id)),
    }))
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

fn rejections_json<'a>(rejected: impl Iterator<Item = &'a Rejection>) -> Vec<Value> {
    rejected
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
        let document =
            plan_selection_json(&Err(SelectionError::NoValidCandidate(rejected)), |_| true);
        assert_eq!(
            document,
            json!({
                "costs": {},
                "selected": null,
                "rejected": [{ "id": "P1", "valid": false, "reason": "q1: misses target" }],
            })
        );
    }

    /// With nothing else displayed, the document still shows the selected
    /// candidate and its cost, chosen among all candidates.
    #[test]
    fn display_cap_keeps_the_selected_candidate() {
        let workload = promql_batch(&["sum(m)".into()], AccuracyTarget::Exact, 15_000);
        let document = stage_pipeline(&workload, 0).unwrap();
        let selected = document["plan_selection"]["selected"].as_str().unwrap();
        let physical = document["physical_asap_optimization"]["candidates"]
            .as_array()
            .unwrap();
        assert_eq!(physical.len(), 1);
        assert_eq!(physical[0]["id"], selected);
        assert!(document["plan_selection"]["costs"].get(selected).is_some());
        assert_eq!(document["plan_selection"]["rejected"], json!([]));
    }
}
