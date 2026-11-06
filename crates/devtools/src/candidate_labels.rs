//! Display labels for the planning stages' candidates, for the
//! `asap_planner` viewer document and text
//! only. Planning never reads them: they
//! are pure functions of the candidates' planning data (the logical roots
//! and the physical properties assignment).

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::Value;

use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
use asap_types::ir::NonASAPOp;
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::Lifetime;
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::stages::HowToComputeAggregateMeasure;

use crate::planning_stages::PlanningStagesRun;

/// The label of every logical and physical candidate the document shows
/// of `run` ([`PlanningStagesRun::logical_candidates`],
/// [`PlanningStagesRun::physical_candidates`]), by id.
pub fn candidate_labels(run: &PlanningStagesRun) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for candidate in run.logical_candidates() {
        let label = candidate
            .roots
            .as_deref()
            .map(connected_candidate_label)
            .unwrap_or_else(|_| "Unimplemented candidate".into());
        labels.insert(candidate.id.clone(), label);
    }
    for candidate in &run.physical_candidates() {
        let logical = labels
            .get(&candidate.from_logical)
            .cloned()
            .unwrap_or_default();
        labels.insert(candidate.id.clone(), physical_label(&logical, candidate));
    }
    labels
}

/// Joint selections carry their actual connected DAG rather than a Cartesian
/// choice vector. Derive labels from that DAG so a sketch is never called exact.
fn connected_candidate_label(roots: &[QueryRoot]) -> String {
    let mut labels = Vec::new();
    let mut producers: BTreeMap<usize, (String, Vec<usize>)> = BTreeMap::new();
    for (query, root) in roots.iter().enumerate() {
        let mut states = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for operator in root.operator_refs() {
            for node in OperatorNode::reachable(operator) {
                let identity = Rc::as_ptr(&node) as usize;
                if !seen.insert(identity) {
                    continue;
                }
                if matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. })
                ) {
                    let name = build_label(&node.operator);
                    if !states.contains(&name) {
                        states.push(name.clone());
                    }
                    producers
                        .entry(identity)
                        .or_insert_with(|| (name, Vec::new()))
                        .1
                        .push(query);
                } else if matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })) {
                    producers
                        .entry(identity)
                        .or_insert_with(|| ("Scan".into(), Vec::new()))
                        .1
                        .push(query);
                }
            }
        }
        labels.push(format!(
            "Q{} {}",
            query + 1,
            if states.is_empty() {
                "exact".into()
            } else {
                states.join(" + ")
            }
        ));
    }
    let mut shared: Vec<_> = producers
        .values()
        .filter(|(_, queries)| queries.len() > 1)
        .map(|(name, queries)| format!("shared {name} ({})", query_names(queries)))
        .collect();
    shared.sort();
    labels.extend(shared);
    labels.join(" · ")
}

/// A Pass 1 per-computation candidate, e.g. "exact", "Sum acc" or "Kll".
pub fn computation_label(candidate: &HowToComputeAggregateMeasure) -> String {
    match candidate {
        HowToComputeAggregateMeasure::Exact => "exact".to_string(),
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) => {
            let kinds: Vec<String> = kinds.iter().map(|kind| format!("{kind:?}")).collect();
            format!("{} acc", kinds.join("/"))
        }
        HowToComputeAggregateMeasure::Summary(algorithm) => sketch_name(algorithm),
    }
}

/// `what` with the queries that read it, e.g. "Kll for Q1, Q3"; `what`
/// alone when none is found (a node later rules rebuilt).
fn for_queries(what: String, queries: &[usize]) -> String {
    match queries.is_empty() {
        true => what,
        false => format!("{what} for {}", query_names(queries)),
    }
}

/// Query names for query indices, e.g. "Q1, Q3".
fn query_names(queries: &[usize]) -> String {
    queries
        .iter()
        .map(|q| format!("Q{}", q + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `ms` in the largest whole unit: "1m", "10s", "250ms".
fn duration_name(ms: u64) -> String {
    match ms {
        ms if ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        ms if ms % 60_000 == 0 => format!("{}m", ms / 60_000),
        ms if ms % 1_000 == 0 => format!("{}s", ms / 1_000),
        ms => format!("{ms}ms"),
    }
}

/// `logical` followed by what the candidate materializes, e.g.
/// "… · ingestion time: Kll for Q1, 5 × 1m tumbling windows · query time, shared:
/// DDSketch";
/// just `logical` when nothing is.
pub fn physical_label(logical: &str, candidate: &PhysicalWorkloadPlan) -> String {
    let mut parts: Vec<(&str, Vec<String>)> = Vec::new();
    for node in &candidate.dag.nodes {
        let id = &node.id;
        let phase = match (node.output_state.timing, node.lifetime) {
            (_, Lifetime::None) => continue,
            (ExecutionTiming::IngestionTime, _) => "ingestion time",
            (ExecutionTiming::QueryTime, Lifetime::OneEvaluation(_)) => "query time, shared",
            (ExecutionTiming::QueryTime, Lifetime::AcrossEvaluations(_)) => {
                "query time, materialized across evaluations"
            }
        };
        let label = match &node.payload {
            // Name the chain: chains of one family and tumbling-window count differ by
            // their queries and tumbling-window width.
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                tumbling_window_count,
                ..
            }) => format!(
                "{}, {tumbling_window_count} × {} tumbling windows",
                for_queries(
                    state_label(&node.payload),
                    &dag_readers(&candidate.dag, *id)
                ),
                duration_name(*tumbling_window_width_ms)
            ),
            payload => build_label(payload),
        };
        match parts.iter_mut().find(|(p, _)| *p == phase) {
            Some((_, labels)) => labels.push(label),
            None => parts.push((phase, vec![label])),
        }
    }
    std::iter::once(logical.to_string())
        .chain(
            parts
                .into_iter()
                .map(|(phase, labels)| format!("{phase}: {}", labels.join(", "))),
        )
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A summary build, e.g. "Kll ×5 tumbling windows" or "Sum acc".
fn build_label<C>(payload: &Operator<C>) -> String {
    match payload {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_count,
            ..
        }) => {
            format!(
                "{} ×{tumbling_window_count} tumbling windows",
                state_label(payload)
            )
        }
        _ => state_label(payload),
    }
}

/// The state a summary build or tumbling-window chain keeps, e.g. "Kll" or "Sum acc";
/// any other operator's kind.
fn state_label<C>(payload: &Operator<C>) -> String {
    match payload {
        Operator::ASAP(
            ASAPOp::SummaryAgg { family, .. } | ASAPOp::SummaryTumblingWindowChain { family, .. },
        ) => match family {
            FieldDataType::Sketch(algorithm, _) => sketch_name(algorithm),
            FieldDataType::ExactAggregate(kind, _) => format!("{kind:?} acc"),
            _ => payload.kind_name().to_string(),
        },
        _ => payload.kind_name().to_string(),
    }
}

/// The queries (in workload order) whose root reaches node `id` of `dag`.
fn dag_readers(dag: &PhysicalASAPDAG, id: PhysicalASAPNodeId) -> Vec<usize> {
    // Query `q` is the scalar root keyed `q` when there is one, and
    // otherwise the next operator root.
    let mut operator_roots = dag.roots.iter();
    let query_starts: Vec<Vec<PhysicalASAPNodeId>> = (0..dag.roots.len() + dag.scalar_roots.len())
        .map(|q| match dag.scalar_roots.iter().find(|(i, _)| *i == q) {
            Some((_, expr)) => expr.operator_refs().into_iter().copied().collect(),
            None => operator_roots.next().into_iter().copied().collect(),
        })
        .collect();
    query_starts
        .into_iter()
        .enumerate()
        .filter(|(_, starts)| {
            let mut seen = std::collections::HashSet::new();
            let mut stack = starts.clone();
            while let Some(node) = stack.pop() {
                if node == id {
                    return true;
                }
                if seen.insert(node) {
                    stack.extend(
                        dag.edges
                            .iter()
                            .filter(|e| e.consumer == node)
                            .map(|e| e.producer),
                    );
                }
            }
            false
        })
        .map(|(q, _)| q)
        .collect()
}

/// A sketch algorithm's display name, e.g. "CMS+heap" or "Kll".
fn sketch_name(algorithm: &SketchAlgorithm) -> String {
    match algorithm {
        SketchAlgorithm::CmsWithHeap => "CMS+heap".to_string(),
        SketchAlgorithm::CountSketchWithHeap => "CountSketch+heap".to_string(),
        other => format!("{other:?}"),
    }
}

/// One node's operator kind with its sketch family and parameters, its
/// summary query, its time range and its tumbling-window layout, when it has them: e.g.
/// `SummaryAgg Kll(k=200)`, `SummaryEstimate Quantile(q=0.5)`.
pub fn node_label(node: &asap_types::ir::flat::FlatNode) -> String {
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
    if depth > 6 || details.len() >= 4 {
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
                    // A time range's length, e.g. `60s`.
                    "range" => details.push(duration_text(field)),
                    // A window, and a tumbling-window chain's layout.
                    "window" | "tumbling_window_width_ms" | "tumbling_window_count" => {
                        details.push(format!("{key}={field}"))
                    }
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

/// A serialized `Duration` (`{"secs": 60, "nanos": 0}`) as `60s`, or
/// `1500ms` when it is not whole seconds; any other value as its JSON.
fn duration_text(value: &Value) -> String {
    match (
        value.get("secs").and_then(Value::as_u64),
        value.get("nanos").and_then(Value::as_u64),
    ) {
        (Some(secs), Some(0)) => format!("{secs}s"),
        (Some(secs), Some(nanos)) => format!("{}ms", secs * 1000 + nanos / 1_000_000),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_layering_examples::example1;
    use crate::planning_stages::lower_promql_roots;
    use asap_logical_asap_optimization::test_support::{
        compose_logical_candidate, generate_per_computation_candidates,
    };

    /// Selected DAG labels expose accumulator state without a legacy choice ID.
    #[test]
    fn connected_labels_describe_selected_states() {
        let workload = example1();
        let inventory =
            generate_per_computation_candidates(&asap_logical_asap_optimization::memo_of_queries(
                &lower_promql_roots(&workload).unwrap(),
            ))
            .unwrap();
        let choice = vec![0; inventory.targets.len()];
        let roots: Vec<_> = compose_logical_candidate(&inventory, &choice).unwrap();
        let label = connected_candidate_label(&roots);
        assert!(label.contains("Q1 ") && label.contains("Q2 "));
        assert!(label.contains("acc"), "{label}");
    }

    /// A state reused by two query outputs is named once in the sharing suffix.
    #[test]
    fn connected_labels_name_actual_shared_state() {
        let workload = example1();
        let inventory =
            generate_per_computation_candidates(&asap_logical_asap_optimization::memo_of_queries(
                &lower_promql_roots(&workload).unwrap(),
            ))
            .unwrap();
        let roots =
            compose_logical_candidate(&inventory, &vec![0; inventory.targets.len()]).unwrap();
        let label = connected_candidate_label(&[roots[1].clone(), roots[1].clone()]);
        assert!(label.contains("shared Sum acc (Q1, Q2)"), "{label}");
        assert_eq!(label.matches("shared Sum acc").count(), 1);
    }

    /// Raw logical operators have an exact label, rather than an empty choice label.
    #[test]
    fn connected_labels_name_exact_results() {
        let roots = lower_promql_roots(&example1()).unwrap();
        assert_eq!(connected_candidate_label(&roots), "Q1 exact · Q2 exact");
    }
}
