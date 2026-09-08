//! Offline workload replay through search, selection, and lifecycle planning.
use std::{collections::HashSet, error::Error, rc::Rc, time::Instant};

use asap_aware_mapping::empirical_cost::{
    EmpiricalCostModel, EmpiricalEvidenceProvider, EvidenceArtifact, EvidenceContext,
};
use asap_aware_mapping::{
    default_strategies_with, export_summary_maintenance_plan,
    materialize_with_summary_maintenance_lifecycles, search_workload_with_targets, CostModel,
    DefaultAccuracyModel, DefaultCostModel, Horizon, Replacement,
    SummaryMaintenanceLifecycleCapabilities, WorkloadDemand,
};
use asap_devtools::lower_promql;
use asap_types::{
    dag_export,
    post_asap::{SummaryExpr, SummaryFamilyType, SummaryNode},
    pre_asap::QueryExpr,
    types::AccuracyTarget,
    workload::*,
};
use serde_json::{json, Value};

const CORPUS: &str =
    include_str!("../../../frontend-promql/tests/observability/data/o11y_bench_promql.txt");
const SUPPLEMENTAL: &str = "quantile_over_time(0.95, service_latency_seconds[5m])\nquantile_over_time(0.99, service_latency_seconds[5m])\ncount_over_time(offline_frequency_metric[5m])";

struct Options {
    epsilon: f64,
    evaluations: u64,
    now_ms: u64,
    supplemental: bool,
    queries: Option<String>,
    evidence_path: Option<String>,
    context_path: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            epsilon: 0.01,
            evaluations: 60,
            now_ms: 1_788_825_600_000,
            supplemental: false,
            queries: None,
            evidence_path: None,
            context_path: None,
        }
    }
}

fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Options, Box<dyn Error>> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--supplemental" => options.supplemental = true,
            "--epsilon" => options.epsilon = args.next().ok_or("missing epsilon")?.parse()?,
            "--evaluations" => options.evaluations = args.next().ok_or("missing evaluations")?.parse()?,
            "--now-ms" => options.now_ms = args.next().ok_or("missing now-ms")?.parse()?,
            "--queries" => options.queries = Some(std::fs::read_to_string(args.next().ok_or("missing queries path")?)?),
            "--evidence" => options.evidence_path = Some(args.next().ok_or("missing evidence path")?),
            "--context" => options.context_path = Some(args.next().ok_or("missing context path")?),
            _ => return Err(format!("unknown option {arg}; use --epsilon, --evaluations, --now-ms, --queries, --supplemental, --evidence and --context").into()),
        }
    }
    if !options.epsilon.is_finite()
        || !(0.0..1.0).contains(&options.epsilon)
        || options.epsilon == 0.0
    {
        return Err("epsilon must be finite and strictly between 0 and 1".into());
    }
    if options.evaluations == 0 {
        return Err("evaluations must be positive".into());
    }
    if options.evidence_path.is_some() != options.context_path.is_some() {
        return Err("--evidence and --context must be supplied together".into());
    }
    Ok(options)
}

fn queries(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|q| !q.is_empty() && !q.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

// Attribute memo groups by DAG identity after workload-wide CSE. Scalar
// expressions aren't replacement sites; they remain part of their operator.
fn reachable(node: &QueryExpr, found: &mut HashSet<*const QueryExpr>) {
    if !found.insert(node as *const QueryExpr) {
        return;
    }
    use QueryExpr::*;
    match node {
        PromqlVectorFromScalar(child)
        | PromqlScalarFromVector(child)
        | PromqlRelabel { child, .. }
        | PromqlInfoEnrich { child, .. }
        | PromqlSeriesSample { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Dedup { child, .. }
        | PromqlSubquery { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | SQLWindowFunc { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => reachable(child, found),
        Concat { children, .. } => {
            for child in children {
                reachable(child, found);
            }
        }
        Join { left, right, .. } | SetOp { left, right, .. } => {
            reachable(left, found);
            reachable(right, found);
        }
        BinaryOp { lhs, rhs, .. } => {
            reachable(lhs, found);
            reachable(rhs, found);
        }
        _ => {}
    }
}

fn family_report(
    family: &SummaryFamilyType,
    provider: Option<&EmpiricalEvidenceProvider>,
) -> Value {
    let evidence = if let SummaryFamilyType::Sketch(kind, _) = family {
        match provider {
            Some(provider) => match provider.lookup(kind.algorithm(), kind.params()) {
                Ok(row) => json!({"status": "matched_configuration", "measurement": row,
                    "scope": "offline benchmark primitive; error/read query semantics are recorded in measurement.error.query, not validated against replay query"}),
                Err(error) => json!({"status": "unavailable", "reason": error.to_string()}),
            },
            None => {
                json!({"status": "unavailable", "reason": "no empirical provider in this mode"})
            }
        }
    } else {
        json!({"status": "unavailable", "reason": "offline sketch evidence does not cost exact operators"})
    };
    let sketch = match family {
        SummaryFamilyType::Sketch(kind, _) => {
            json!({"algorithm": kind.algorithm(), "params": kind.params()})
        }
        _ => Value::Null,
    };
    json!({"family": format!("{family:?}"), "sketch": sketch,
        "is_sketch": matches!(family, SummaryFamilyType::Sketch(..)), "offline_evidence": evidence})
}

fn families(
    node: &SummaryNode,
    result: &mut Vec<Value>,
    provider: Option<&EmpiricalEvidenceProvider>,
) {
    use SummaryExpr::*;
    match &node.expr {
        SummaryAgg { family, child, .. } => {
            result.push(family_report(family, provider));
            families(child, result, provider);
        }
        SummaryJoin {
            family,
            outer,
            inner,
            ..
        } => {
            result.push(family_report(family, provider));
            families(outer, result, provider);
            families(inner, result, provider);
        }
        SummaryEstimate { summary_input, .. } | SummaryDelete { summary_input, .. } => {
            families(summary_input, result, provider)
        }
        BinaryOp { lhs, rhs, .. } => {
            families(lhs, result, provider);
            families(rhs, result, provider);
        }
        SummarySubtract { left, right } => {
            families(left, result, provider);
            families(right, result, provider);
        }
        SummaryMerge { children } => {
            for child in children {
                families(child, result, provider);
            }
        }
        KeepPreAsap(_) => {}
    }
}

fn replay(
    name: &str,
    query_texts: &[String],
    accuracy: AccuracyTarget,
    options: &Options,
    model: &dyn CostModel,
    provider: Option<&EmpiricalEvidenceProvider>,
) -> Value {
    let lowering_start = Instant::now();
    let mut roots = Vec::new();
    let mut rows = Vec::new();
    for (index, query) in query_texts.iter().enumerate() {
        match lower_promql(query, accuracy.clone()) {
            Ok(root) => roots.push((index, Rc::new(root), Some(accuracy.clone()))),
            Err(error) => rows.push(json!({"index": index, "query": query, "coverage": "rejected", "reason": error.to_string()})),
        }
    }
    let lowering_ns = lowering_start.elapsed().as_nanos();
    let search_start = Instant::now();
    let space = search_workload_with_targets(
        roots,
        &default_strategies_with(model),
        &DefaultAccuracyModel,
    );
    let search_ns = search_start.elapsed().as_nanos();
    let selection_start = Instant::now();
    let ranked = space.cost_sorted(model);
    let selection = space.global_selection(model);
    let selection_ns = selection_start.elapsed().as_nanos();
    let workload = QueryWorkload {
        language: QueryLanguage::PromQL,
        query_batch: Some(
            query_texts
                .iter()
                .map(|q| BatchEntry {
                    query: Query(q.clone()),
                    requirements: QueryRequirements {
                        accuracy: AccuracyRequirement::Explicit(accuracy.clone()),
                        ..Default::default()
                    },
                    predictability: Predictability::AdHoc,
                    invocations: options.evaluations,
                    execute_at: Some(TimestampMs(options.now_ms)),
                    time_selection: TimeSelection {
                        as_of: Some(TimestampMs(options.now_ms)),
                        ..Default::default()
                    },
                })
                .collect(),
        ),
        repeating_queries: None,
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
        }),
    };
    for (index, root) in &space.roots {
        let started = Instant::now();
        let mut found = HashSet::new();
        reachable(root, &mut found);
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();
        for (group_index, group) in ranked
            .iter()
            .enumerate()
            .filter(|(_, g)| found.contains(&Rc::as_ptr(g.target)))
        {
            let target_intent = asap_aware_mapping::replacement::bindable_intent(group.target)
                .map(|intent| format!("{intent:?}"));
            for (rank, candidate) in group.candidates.iter().enumerate() {
                let mut family_list = Vec::new();
                if let Replacement::Summary(node) = &candidate.replacement {
                    families(node, &mut family_list, provider);
                }
                candidates.push(
                    json!({"group": group_index, "rank": rank, "strategy": candidate.strategy,
                    "target_intent": target_intent,
                    "rationale": candidate.rationale, "families": family_list,
                    "heuristic_score": group.costs[rank].is_finite().then_some(group.costs[rank]),
                    "score_unit": "dimensionless_not_cpu", "consumer_count": group.consumer_count}),
                );
            }
            if let Some(memo) = space.group_for(group.target) {
                rejected.extend(memo.rejected.iter().map(|r| json!({"group": group_index, "strategy": r.strategy, "description": r.description, "reason": r.error.to_string()})));
            }
        }
        let has_sketch = candidates.iter().any(|c| {
            c["families"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["is_sketch"] == true)
        });
        let has_summary = candidates
            .iter()
            .any(|c| !c["families"].as_array().unwrap().is_empty());
        let materialized = selection.materialize(root);
        let root_plan = match materialized {
            Ok(Some(node)) => {
                json!({"graph": dag_export::export_summary(&node), "guarantee": node.guarantee})
            }
            Ok(None) => json!({"reason": "no selected root group"}),
            Err(error) => json!({"reason": error.to_string()}),
        };
        let lifecycle = match materialize_with_summary_maintenance_lifecycles(
            &selection,
            root,
            WorkloadDemand::new(&workload, &[*index]),
            options.now_ms,
            Some(Horizon(3600.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            model,
        ) {
            Ok(Some(plan)) => json!(export_summary_maintenance_plan(&plan)),
            Ok(None) => json!({"reason": "no selected root group", "selected_raw_recompute": true}),
            Err(error) => json!({"reason": error.to_string(), "selected_raw_recompute": true}),
        };
        rows.push(json!({"index": index, "query": query_texts[*index],
            "coverage": if has_sketch { "sketch_candidate" } else if has_summary { "exact_summary_candidate" } else { "exact_fallback" },
            "coverage_scope": "reachable_candidate_sites_not_whole_query_execution",
            "candidates": candidates, "rejections": rejected, "root_plan": root_plan, "lifecycle_plan": lifecycle,
            "report_and_materialization_ns": started.elapsed().as_nanos(),
            "estimated_end_to_end_cpu_savings": null, "estimated_end_to_end_memory_savings": null,
            "unknown_cost_reason": "complete raw scan, residual operators, grouping cardinalities and deployment costs are unavailable"}));
    }
    rows.sort_by_key(|row| row["index"].as_u64());
    let mut coverage_counts = std::collections::BTreeMap::new();
    for row in &rows {
        *coverage_counts
            .entry(row["coverage"].as_str().unwrap_or("unknown"))
            .or_insert(0) += 1;
    }
    let raw_fallback_count = rows
        .iter()
        .filter(|row| row["lifecycle_plan"]["selected_raw_recompute"] == true)
        .count();
    json!({"mode": name, "accuracy": accuracy, "lowering_ns": lowering_ns, "search_ns": search_ns,
        "selection_ns": selection_ns, "query_count": query_texts.len(), "memo_groups": space.len(),
        "coverage_counts": coverage_counts, "lifecycle_raw_fallback_count": raw_fallback_count, "queries": rows})
}

fn report(options: &Options, empirical: Option<&EmpiricalCostModel>) -> Value {
    let mut corpora = vec![(
        if options.queries.is_some() {
            "custom"
        } else {
            "o11y_bench"
        },
        queries(options.queries.as_deref().unwrap_or(CORPUS)),
    )];
    if options.supplemental {
        corpora.push(("supplemental_sketch_queries", queries(SUPPLEMENTAL)));
    }
    let corpora: Vec<Value> = corpora
        .into_iter()
        .map(|(name, queries)| {
            let mut runs = vec![
                replay(
                    "exact",
                    &queries,
                    AccuracyTarget::Exact,
                    options,
                    &DefaultCostModel,
                    None,
                ),
                replay(
                    "default",
                    &queries,
                    AccuracyTarget::Epsilon(options.epsilon),
                    options,
                    &DefaultCostModel,
                    None,
                ),
            ];
            if let Some(model) = empirical {
                runs.push(replay(
                    "empirical",
                    &queries,
                    AccuracyTarget::Epsilon(options.epsilon),
                    options,
                    model,
                    Some(&model.provider),
                ));
            }
            json!({"name": name, "source": match name {
                "o11y_bench" => "repository-vendored grafana/o11y-bench snapshot",
                "custom" => "user-supplied PromQL query file",
                _ => "local supplemental examples, not upstream o11y queries"
            }, "runs": runs})
        })
        .collect();
    json!({"schema_version": 1, "execution_scope": "offline_planner_search_selection_and_lifecycle_no_data_plane",
        "empirical_context": empirical.map(|m| m.provider.context()),
        "empirical_artifact": empirical.map(|m| json!({"schema_version": m.provider.artifact().schema_version,
            "benchmark_version": m.provider.artifact().benchmark_version, "model_version": m.provider.artifact().model_version})),
        "vendored_fixture_source": {"repository": "https://github.com/grafana/o11y-bench", "vendored_snapshot_date": "2026-07-17", "upstream_commit": null,
            "note": "existing 27-query local fixture; upstream revision and scenario data were not provided"},
        "assumptions": {"evaluation_time_ms": options.now_ms, "evaluations_per_query": options.evaluations,
            "data_arrival": "at_rest", "replay_semantics": "repeated reads at one fixed as-of time; query windows and offsets remain in IR",
            "lifecycle_horizon_seconds": 3600, "runtime_capabilities": "hypothetical all supported, no runtime launched",
            "timing": "single wall-clock sample; report/materialization timing includes JSON export"},
        "corpora": corpora})
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options(std::env::args().skip(1))?;
    let empirical = match (&options.evidence_path, &options.context_path) {
        (Some(evidence), Some(context)) => {
            let artifact: EvidenceArtifact =
                serde_json::from_str(&std::fs::read_to_string(evidence)?)?;
            let context: EvidenceContext =
                serde_json::from_str(&std::fs::read_to_string(context)?)?;
            Some(EmpiricalCostModel::new(EmpiricalEvidenceProvider::new(
                artifact, context,
            )?))
        }
        _ => None,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report(&options, empirical.as_ref()))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invalid CLI numbers must fail before they can poison cost estimates.
    #[test]
    fn rejects_invalid_configuration() {
        for value in ["NaN", "0", "1", "-1"] {
            assert!(parse_options(["--epsilon".into(), value.into()]).is_err());
        }
        assert!(parse_options(["--evaluations".into(), "0".into()]).is_err());
    }

    /// Real workload lowering alone never counts as sketch or deployment coverage.
    #[test]
    fn o11y_replay_records_all_queries_and_conservative_costs() {
        let result = report(&Options::default(), None);
        for run in result["corpora"][0]["runs"].as_array().unwrap() {
            assert_eq!(run["query_count"], 27);
            assert!(run["memo_groups"].as_u64().unwrap() > 0);
            for row in run["queries"].as_array().unwrap() {
                assert_ne!(row["coverage"], "rejected");
                assert_ne!(row["coverage"], "sketch_candidate");
                assert!(row["estimated_end_to_end_cpu_savings"].is_null());
                assert_eq!(row["lifecycle_plan"]["selected_raw_recompute"], true);
            }
        }
    }

    /// Supplemental sketches and rejected syntax remain separately identifiable.
    #[test]
    fn sketches_and_rejections_are_explicit() {
        let qs = vec!["quantile_over_time(0.95, latency[5m])".into(), "!!!".into()];
        let result = replay(
            "default",
            &qs,
            AccuracyTarget::Epsilon(0.01),
            &Options::default(),
            &DefaultCostModel,
            None,
        );
        assert_eq!(result["queries"][0]["coverage"], "sketch_candidate");
        assert_eq!(result["queries"][1]["coverage"], "rejected");
        assert!(result["queries"][1]["reason"].is_string());
    }

    /// Count-over-time exercises the measured CMS/CountSketch configuration,
    /// while its evidence remains absent unless an artifact is supplied.
    #[test]
    fn supplemental_count_exposes_frequency_sketch_candidates() {
        let result = replay(
            "default",
            &queries("count_over_time(metric[5m])"),
            AccuracyTarget::Epsilon(0.01),
            &Options::default(),
            &DefaultCostModel,
            None,
        );
        let families: Vec<_> = result["queries"][0]["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["families"].as_array().unwrap())
            .collect();
        for algorithm in ["Cms", "CountSketch"] {
            let family = families
                .iter()
                .find(|f| f["sketch"]["algorithm"] == algorithm)
                .unwrap();
            assert_eq!(family["offline_evidence"]["status"], "unavailable");
        }
    }
}
