//! Display labels for the planning stages' candidates, for the
//! `asap_planner` viewer document and text
//! only. Planning never reads them: they
//! are pure functions of the candidates' planning data (the logical roots
//! and the physical properties assignment).

use serde_json::Value;

use asap_types::ir::schema::SketchAlgorithm;
use asap_types::stages::HowToComputeAggregateMeasure;

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
                    "window" | "tumbling_window_width_ms" => details.push(format!("{key}={field}")),
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
mod tests {}
