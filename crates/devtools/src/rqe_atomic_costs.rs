//! Converts sketch-bench's per-operation cost table (`rqe_atomic_costs.json`)
//! into plan selection's measured table
//! ([`PlanSelectionSettings::measured_operation_costs`]).
//!
//! sketch-bench writes one row per sketch configuration:
//!
//! ```json
//! {"sketch": "kll-percall", "sketch_config": {"algorithm": "kll-percall", "params": {"k": 200}},
//!  "insert_cpu_secs": 8.5e-8, "merge_cpu_secs": 1.2e-6, "query_cpu_secs": 4.0e-7, ...}
//! ```
//!
//! and each becomes three entries, one per operation, in CPU nanoseconds:
//!
//! ```json
//! [{"sketch": {"Kll": {"k": 200}}, "operation": "update", "cpu_ns_per_operation": 85.0},
//!  {"sketch": {"Kll": {"k": 200}}, "operation": "merge", "cpu_ns_per_operation": 1200.0},
//!  {"sketch": {"Kll": {"k": 200}}, "operation": "estimate", "cpu_ns_per_operation": 400.0}]
//! ```
//!
//! Only this converter reads the sketch-bench format; the planner reads the
//! table. Rows plan selection cannot price from the table are skipped with
//! the reason: exact accumulators, Hydra structures (shared across groups,
//! measured at one label width) and sketch names with no planner sketch.
//!
//! [`PlanSelectionSettings::measured_operation_costs`]: asap_aware_mapping::stage3_plan_selection::PlanSelectionSettings::measured_operation_costs

use asap_aware_mapping::stage3_plan_selection::{MeasuredOperationCost, SummaryOperation};
use asap_types::post_asap::SketchParams;
use serde_json::Value;

/// A sketch-bench row that gave no entries, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedRow {
    /// The row's `sketch` name, e.g. `exact-sum`.
    pub sketch_name: String,
    /// Why plan selection cannot price from it.
    pub reason: String,
}

/// What [`convert_rqe_atomic_costs`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedCosts {
    /// The measured table: three entries per converted row (update, merge,
    /// estimate), in row order.
    pub measured_operation_costs: Vec<MeasuredOperationCost>,
    /// The rows skipped, in row order.
    pub skipped_rows: Vec<SkippedRow>,
}

/// Converts the parsed `rqe_atomic_costs.json` (a JSON array of rows).
/// Errors (bad input): not an array, a row missing its name, parameters or
/// a CPU time, a time that is not a non-negative number, a parameter of the
/// wrong type, or two rows for one sketch configuration.
pub fn convert_rqe_atomic_costs(rows: &Value) -> Result<ConvertedCosts, String> {
    let rows = rows
        .as_array()
        .ok_or("rqe_atomic_costs.json is not a JSON array of rows")?;
    let mut converted = ConvertedCosts {
        measured_operation_costs: Vec::new(),
        skipped_rows: Vec::new(),
    };
    for (index, row) in rows.iter().enumerate() {
        let sketch_name = row["sketch"]
            .as_str()
            .ok_or(format!("row {index}: no `sketch` name"))?;
        let params = &row["sketch_config"]["params"];
        let sketch = match sketch_params(sketch_name, params) {
            Ok(Some(sketch)) => sketch,
            Ok(None) => {
                converted.skipped_rows.push(SkippedRow {
                    sketch_name: sketch_name.to_string(),
                    reason: skip_reason(sketch_name).to_string(),
                });
                continue;
            }
            Err(error) => return Err(format!("row {index} ({sketch_name}): {error}")),
        };
        if converted
            .measured_operation_costs
            .iter()
            .any(|entry| entry.sketch == sketch)
        {
            return Err(format!(
                "row {index} ({sketch_name}): a second row for {sketch:?}"
            ));
        }
        for (field, operation) in [
            ("insert_cpu_secs", SummaryOperation::Update),
            ("merge_cpu_secs", SummaryOperation::Merge),
            ("query_cpu_secs", SummaryOperation::Estimate),
        ] {
            let seconds = row[field]
                .as_f64()
                .filter(|s| s.is_finite() && *s >= 0.0)
                .ok_or(format!(
                    "row {index} ({sketch_name}): `{field}` is not a non-negative number"
                ))?;
            converted
                .measured_operation_costs
                .push(MeasuredOperationCost {
                    sketch: sketch.clone(),
                    operation,
                    cpu_ns_per_operation: seconds * 1e9,
                });
        }
    }
    Ok(converted)
}

/// Why a row whose name has no planner sketch is skipped.
fn skip_reason(sketch_name: &str) -> &'static str {
    if sketch_name.starts_with("exact-") {
        "an exact accumulator: the table prices sketches only"
    } else if sketch_name.starts_with("hydra-") {
        "a Hydra structure shared across groups: the table prices one sketch per group"
    } else {
        "no planner sketch has this name"
    }
}

/// The planner sketch a sketch-bench name and its parameters configure,
/// `None` for a name with none. Names: `kll*` (`k`), `dd*` (`alpha`), `hll`
/// (`lg_k`), `kmv` (`k`), `cms` and `count-sketch` (`rows`, `cols`), and
/// their top-k variants (a name containing `heap`, with `heap`).
fn sketch_params(sketch_name: &str, params: &Value) -> Result<Option<SketchParams>, String> {
    let integer = |name: &str| -> Result<u32, String> {
        params[name]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(format!(
                "parameter `{name}` is not a 32-bit unsigned integer"
            ))
    };
    let with_heap = sketch_name.contains("heap");
    let sketch = if sketch_name.starts_with("hydra-") || sketch_name.starts_with("exact-") {
        return Ok(None);
    } else if sketch_name.starts_with("kll") {
        SketchParams::Kll {
            k: integer("k")?,
            failure_probability: None,
        }
    } else if sketch_name.starts_with("dd") {
        SketchParams::DDSketch {
            alpha: params["alpha"]
                .as_f64()
                .ok_or("parameter `alpha` is not a number")?,
        }
    } else if sketch_name == "hll" {
        SketchParams::Hll {
            precision: u8::try_from(integer("lg_k")?).map_err(|e| e.to_string())?,
            relative_error: None,
        }
    } else if sketch_name == "kmv" {
        SketchParams::Kmv {
            k: integer("k")?,
            relative_error: None,
        }
    } else if sketch_name.starts_with("cms") && with_heap {
        SketchParams::CmsWithHeap {
            width: integer("cols")?,
            depth: integer("rows")?,
            heap_size: integer("heap")?,
        }
    } else if sketch_name == "cms" {
        SketchParams::Cms {
            width: integer("cols")?,
            depth: integer("rows")?,
        }
    } else if sketch_name.starts_with("count-sketch") && with_heap {
        SketchParams::CountSketchWithHeap {
            width: integer("cols")?,
            depth: integer("rows")?,
            heap_size: integer("heap")?,
        }
    } else if sketch_name == "count-sketch" {
        SketchParams::CountSketch {
            width: integer("cols")?,
            depth: integer("rows")?,
        }
    } else {
        return Ok(None);
    };
    Ok(Some(sketch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Four sketch-bench rows in the format of `rqe_atomic_costs.json`: a
    /// KLL and a Count-Min top-k convert, the exact sum and the Hydra HLL
    /// are skipped. The table written as JSON reads back as the same
    /// entries, with seconds turned into nanoseconds.
    #[test]
    fn a_small_sample_converts_and_round_trips() {
        let sample = json!([
            {"sketch": "kll-percall", "sketch_config": {"algorithm": "kll-percall", "params": {"k": 200}},
             "mem_bytes_per_instance": 9648.0, "insert_cpu_secs": 8.5e-8, "merge_cpu_secs": 1.2e-6,
             "query_cpu_secs": 4.0e-7, "query_accuracy": {}, "accuracy_metric": "mean_rank_err",
             "measured_at": {"items_per_instance": 1000000}},
            {"sketch": "cms-heap-topk-fastpath-vector2d",
             "sketch_config": {"algorithm": "cms-heap-topk-fastpath-vector2d",
                               "params": {"cols": 1024, "heap": 32, "rows": 3}},
             "insert_cpu_secs": 6.0e-8, "merge_cpu_secs": 5.0e-6, "query_cpu_secs": 3.5e-7},
            {"sketch": "exact-sum", "sketch_config": {"algorithm": "exact-sum", "params": {}},
             "insert_cpu_secs": 1.0e-9, "merge_cpu_secs": 1.0e-9, "query_cpu_secs": 1.0e-9},
            {"sketch": "hydra-hll", "sketch_config": {"algorithm": "hydra-hll", "params": {"cols": 1024, "rows": 3}},
             "insert_cpu_secs": 1.0e-6, "merge_cpu_secs": 1.0e-6, "query_cpu_secs": 1.0e-6}
        ]);
        let converted = convert_rqe_atomic_costs(&sample).unwrap();
        let kll = SketchParams::Kll {
            k: 200,
            failure_probability: None,
        };
        let cms = SketchParams::CmsWithHeap {
            width: 1024,
            depth: 3,
            heap_size: 32,
        };
        let expected: Vec<_> = [
            (&kll, SummaryOperation::Update, 85.0),
            (&kll, SummaryOperation::Merge, 1200.0),
            (&kll, SummaryOperation::Estimate, 400.0),
            (&cms, SummaryOperation::Update, 60.0),
            (&cms, SummaryOperation::Merge, 5000.0),
            (&cms, SummaryOperation::Estimate, 350.0),
        ]
        .into_iter()
        .map(|(sketch, operation, ns)| (sketch.clone(), operation, ns))
        .collect();
        let got: Vec<_> = converted
            .measured_operation_costs
            .iter()
            .map(|e| (e.sketch.clone(), e.operation, e.cpu_ns_per_operation))
            .collect();
        assert_eq!(got.len(), expected.len());
        for ((sketch, operation, ns), (want_sketch, want_operation, want_ns)) in
            got.iter().zip(&expected)
        {
            assert_eq!((sketch, operation), (want_sketch, want_operation));
            assert!((ns - want_ns).abs() < 1e-9 * want_ns, "{ns} vs {want_ns}");
        }
        let skipped: Vec<_> = converted
            .skipped_rows
            .iter()
            .map(|s| s.sketch_name.as_str())
            .collect();
        assert_eq!(skipped, ["exact-sum", "hydra-hll"]);

        let written = serde_json::to_string(&converted.measured_operation_costs).unwrap();
        let read: Vec<MeasuredOperationCost> = serde_json::from_str(&written).unwrap();
        assert_eq!(read, converted.measured_operation_costs);
    }

    /// Bad input is an error, not a skipped row: a missing time, a negative
    /// time, a parameter of the wrong type, or a configuration twice.
    #[test]
    fn malformed_rows_are_errors() {
        let kll = |k: Value, insert: Value| {
            json!({"sketch": "kll", "sketch_config": {"params": {"k": k}},
                   "insert_cpu_secs": insert, "merge_cpu_secs": 1e-6, "query_cpu_secs": 1e-6})
        };
        for rows in [
            json!([kll(json!(200), Value::Null)]),
            json!([kll(json!(200), json!(-1.0))]),
            json!([kll(json!("200"), json!(1e-7))]),
            json!([kll(json!(200), json!(1e-7)), kll(json!(200), json!(2e-7))]),
            json!({"rows": []}),
        ] {
            assert!(convert_rqe_atomic_costs(&rows).is_err(), "{rows}");
        }
    }
}
