//! The sign conditions of the sketches a plan selects, checked on the plans
//! the facade selects (T5.5). Each condition reads the sign the memo
//! derived for the sketch's own state group, never a sign recomputed from
//! declarations:
//!
//! - a Count-Min-family state (`Cms`, `CmsWithHeap`, `HydraCms`, ...)
//!   counts weights, and its estimate never falls below the true count only
//!   when every weight is non-negative: its state column must be
//!   `NonNegative`. `topk(3, sum by (job) (neg))` must not count `neg`'s
//!   values in one;
//! - a DDSketch keeps a relative error only when the values it holds have
//!   one sign: `abs(quantile_over_time(0.3, mixed[5m]))` with a relative
//!   5% target must not read a DDSketch over `mixed`, whose 0.3-quantile
//!   may be `-0.001` while the sketch answers `+0.001`;
//! - a relative error on each value of a `sum` or `avg` bounds the error of
//!   the result only when the values have one sign: `sum(x̂ - x) ≤ ε·|sum
//!   x|` needs `|sum x| = sum |x|`.

use std::collections::BTreeSet;

use asap_devtools::planning_stages::lower_promql_roots;
use asap_planner::{
    run_planning, DeploymentCapabilities, PlanSearchSettings, PlanningModels, PlanningWorkload,
    SketchAlgorithm,
};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef};
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::stages::{GroupProperties, ValueSign};
use serde_json::json;

const SOURCES: [&str; 4] = ["pos", "neg", "mixed", "c_total"];

/// `X` is replaced by the source.
const TEMPLATES: [&str; 6] = [
    "sum by (job) (quantile_over_time(0.9, X[5m]))",
    "avg(quantile_over_time(0.5, X[5m]))",
    "topk(3, sum by (job) (X))",
    "sum(-quantile_over_time(0.9, X[5m]))",
    "abs(quantile_over_time(0.3, X[5m]))",
    "count_over_time(X[5m])",
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Relative,
    Absolute,
    Exact,
}

fn accuracy(target: Target) -> serde_json::Value {
    match target {
        Target::Relative => json!({"accuracy": {
            "max_error": 0.05, "failure_probability": 0.3,
            "accuracy_scope": "whole_result", "accuracy_metric": "relative",
        }}),
        Target::Absolute => json!({"accuracy": {
            "max_error": 0.05, "failure_probability": 0.3,
            "accuracy_scope": "whole_result", "accuracy_metric": "absolute",
        }}),
        Target::Exact => json!({}),
    }
}

fn workload(query: &str, target: Target) -> PlanningWorkload {
    let gauge = |sign: Option<&str>| {
        let mut facts =
            json!({"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000});
        if let Some(sign) = sign {
            facts["column_signs"] = json!({"value": sign});
        }
        facts
    };
    serde_json::from_value(json!({
        "query_workload": {
            "language": "promql",
            "repeating_queries": [{
                "query": query,
                "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
                "requirements": accuracy(target),
                "predictability": {"predictable": {"known_at": null}},
            }],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 4000.0}},
            "sources": {
                "pos": gauge(Some("non_negative")),
                "neg": gauge(Some("non_positive")),
                "mixed": gauge(None),
                "c_total": {"series_count": 1000, "metric_type": "counter", "sample_interval": 1000},
            },
        },
    }))
    .expect("the workload deserializes")
}

fn is_count_min_family(algorithm: SketchAlgorithm) -> bool {
    matches!(
        algorithm,
        SketchAlgorithm::Cms
            | SketchAlgorithm::CmsWithHeap
            | SketchAlgorithm::CmsWithSpaceSaving
            | SketchAlgorithm::CmsFold
            | SketchAlgorithm::HydraCms
    )
}

/// The sign of the column a `sum` or `avg` measure reads in its input
/// group: its column, or the sample value.
fn measured_sign(measure: &AggIntent, input: &GroupProperties) -> Option<Option<ValueSign>> {
    let column = match measure {
        AggIntent::Sum { col } | AggIntent::Avg { col } => *col,
        _ => return None,
    };
    let column = column.or_else(|| resolve_column_ref(&ColumnRef::SampleValue, &input.schema).ok());
    Some(column.and_then(|c| input.column_signs.get(c).copied()))
}

/// T5.5. For each of 72 workloads (4 sources × 6 templates × 3 targets),
/// the plan the facade selects meets every sketch's sign condition. Today
/// `abs(quantile_over_time(0.3, mixed[5m]))` under a relative target is
/// expected to read a DDSketch over `mixed` (B12). // provisional answer Q7
#[test]
fn selected_sketches_satisfy_their_sign_conditions() {
    let capabilities = DeploymentCapabilities {
        supported_summaries: Some(asap_executor::supported_summary_families()),
        ..DeploymentCapabilities::default()
    };
    let mut violations = Vec::new();
    let mut not_planned = Vec::new();
    let mut sketches_over_pos = 0usize;
    let mut selected_sketches = Vec::new();
    for source in SOURCES {
        for template in TEMPLATES {
            for target in [Target::Relative, Target::Absolute, Target::Exact] {
                let query = template.replace('X', source);
                let label = format!("{query} [{target:?}]");
                let workload = workload(&query, target);
                let roots = lower_promql_roots(&workload).expect("the query lowers");
                let entries: Vec<_> = workload.query_workload.entries().collect();
                let data = workload.data_workload.clone().unwrap_or_default();
                let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
                let run = match run_planning(
                    roots,
                    &entries,
                    &data,
                    models,
                    PlanSearchSettings { inspect: true },
                ) {
                    Ok(run) => run,
                    Err(e) => {
                        not_planned.push(format!("{label}: {e}"));
                        continue;
                    }
                };
                let (dag, physical) = match (&run.selection, &run.physical_asap_candidates) {
                    (Ok((dag, _)), Some(physical)) => (dag, physical),
                    (selection, _) => {
                        not_planned.push(format!("{label}: {:?}", selection.as_ref().err()));
                        continue;
                    }
                };
                let memo = &physical.logical_candidates.memo;
                let group_of = |node: &asap_planner::PhysicalASAPDAG, id: usize| {
                    node.nodes
                        .iter()
                        .find(|n| n.id == id)
                        .and_then(|n| n.selected_physical_candidate)
                        .map(|c| physical.physical_candidates[c.0].logical_candidate_id)
                };
                // The selected nodes that build or merge a sketch state.
                let sketch_nodes: BTreeSet<usize> = dag
                    .nodes
                    .iter()
                    .filter(|node| {
                        group_of(dag, node.id).is_some_and(|candidate| {
                            memo.groups()[candidate.group.0 as usize]
                                .group_properties
                                .sketch_algorithm_of_state()
                                .is_some()
                        })
                    })
                    .map(|node| node.id)
                    .collect();
                for &id in &sketch_nodes {
                    let candidate = group_of(dag, id).unwrap();
                    let properties = &memo.groups()[candidate.group.0 as usize].group_properties;
                    let algorithm = properties.sketch_algorithm_of_state().unwrap();
                    selected_sketches.push(format!("{label}: {algorithm:?}"));
                    if source == "pos" {
                        sketches_over_pos += 1;
                    }
                    if is_count_min_family(algorithm)
                        && properties.state_column_sign() != Some(ValueSign::NonNegative)
                    {
                        violations.push(format!(
                            "{label}: {algorithm:?} state column sign {:?}, not NonNegative",
                            properties.state_column_sign()
                        ));
                    }
                    if algorithm == SketchAlgorithm::DDSketch
                        && target == Target::Relative
                        && !properties
                            .state_column_sign()
                            .is_some_and(ValueSign::is_one_sign)
                    {
                        violations.push(format!(
                            "{label}: DDSketch under a relative target, state column sign {:?}",
                            properties.state_column_sign()
                        ));
                    }
                }
                if target != Target::Relative {
                    continue;
                }
                // A relative `sum` or `avg` above a sketch reads values of
                // one sign.
                let reaches_sketch = |id: usize| {
                    let mut pending = vec![id];
                    let mut seen = BTreeSet::new();
                    while let Some(node) = pending.pop() {
                        if !seen.insert(node) {
                            continue;
                        }
                        if node != id && sketch_nodes.contains(&node) {
                            return true;
                        }
                        pending.extend(
                            dag.edges
                                .iter()
                                .filter(|e| e.consumer == node)
                                .map(|e| e.producer),
                        );
                    }
                    false
                };
                for node in &dag.nodes {
                    let Some(candidate) = group_of(dag, node.id) else {
                        continue;
                    };
                    let expression = &memo.candidate(candidate).expression.operator;
                    let Operator::NonASAP(NonASAPOp::Aggregate {
                        measures, child, ..
                    }) = expression
                    else {
                        continue;
                    };
                    if !reaches_sketch(node.id) {
                        continue;
                    }
                    let input = &memo.groups()[child.0 as usize].group_properties;
                    for measure in measures {
                        let Some(sign) = measured_sign(measure, input) else {
                            continue;
                        };
                        if !sign.is_some_and(ValueSign::is_one_sign) {
                            violations.push(format!(
                                "{label}: relative {measure:?} above a sketch reads sign {sign:?}"
                            ));
                        }
                    }
                }
            }
        }
    }
    println!("selected sketches ({}):", selected_sketches.len());
    for s in &selected_sketches {
        println!("  SELECTED {s}");
    }
    println!("not planned ({}): {not_planned:#?}", not_planned.len());
    for v in &violations {
        println!("VIOLATION {v}");
    }
    assert!(
        sketches_over_pos > 0,
        "no sketch was selected over `pos`: the check is vacuous"
    );
    assert!(violations.is_empty(), "{violations:#?}");
}
