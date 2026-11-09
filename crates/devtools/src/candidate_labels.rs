//! Display labels for the planning stages' operators and parameters, for
//! the `asap_planner` viewer document and text only. Planning never reads
//! them: they are pure functions of the candidates' planning data.

use serde_json::Value;

use asap_types::ir::schema::SketchParams;
use asap_types::ir::Operator;

/// An operator's kind with its summary type, its summary query, its time
/// range and its tumbling-window width, when it has them, without its
/// inputs: e.g. `SummaryAgg Kll`, `SummaryEstimate Quantile(q=0.5)`,
/// `TimeRange 600s`.
pub fn operator_label<C>(operator: &Operator<C>) -> String {
    let operator = serde_json::to_value(operator.map_children(|_| ())).unwrap_or_default();
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

/// A sketch's parameters θ: e.g. `Kll(k=269)`, `Cms(width=2719, depth=5)`.
pub fn parameters_label(parameters: &SketchParams) -> String {
    let value = serde_json::to_value(parameters).unwrap_or_default();
    let Some((algorithm, fields)) = value.as_object().and_then(|o| o.iter().next()) else {
        return value.to_string();
    };
    let fields: Vec<String> = fields
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    format!("{algorithm}({})", fields.join(", "))
}

/// Summary types, summary queries, measures, time ranges and windows found
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
                    // A summary type, e.g. `Kll` or `ExactAggregate(Rate)`.
                    "summary_type" => details.push(summary_type_text(field)),
                    // A time range's length, e.g. `60s`.
                    "range" => details.push(duration_text(field)),
                    // A window, and a tumbling-window chain's width.
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

/// A serialized summary type: `Kll` (`{"Sketch": "Kll"}`),
/// `ExactAggregate(Rate)` (`{"ExactAggregate": ["Rate", "Rate"]}`, the
/// distinct names), or the JSON of any other value.
fn summary_type_text(value: &Value) -> String {
    match value {
        Value::String(name) => name.clone(),
        Value::Object(fields) if fields.len() == 1 => {
            let (kind, inner) = fields.iter().next().expect("one field");
            let mut names: Vec<String> = match inner {
                Value::String(name) => vec![name.clone()],
                Value::Array(items) => items
                    .iter()
                    .map(|item| item.as_str().map_or(item.to_string(), str::to_owned))
                    .collect(),
                other => vec![other.to_string()],
            };
            names.dedup();
            if kind == "Sketch" {
                names.join(", ")
            } else {
                format!("{kind}({})", names.join(", "))
            }
        }
        other => other.to_string(),
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
