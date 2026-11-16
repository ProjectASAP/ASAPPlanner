//! Converts sketch-bench's per-operation cost table (`rqe_atomic_costs.json`)
//! into plan selection's measured table
//! ([`CostInputs::measured_operation_costs`]).
//!
//! sketch-bench writes one row per sketch configuration:
//!
//! ```json
//! {"sketch": "kll-percall", "sketch_config": {"algorithm": "kll-percall", "params": {"k": 200}},
//!  "insert_cpu_secs": 8.5e-8, "merge_cpu_secs": 1.2e-6, "query_cpu_secs": 4.0e-7, ...}
//! ```
//!
//! and each becomes four `OperatorCost` entries of the table's object form,
//! in CPU nanoseconds per unit, for both phases:
//!
//! ```json
//! {"operators": [
//!   {"operator": "SummaryAgg", "phase": null, "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 85.0, "ns_per_invocation": 0.0},
//!   {"operator": "SummaryTumblingWindowChain", "phase": null, "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 85.0, "ns_per_invocation": 0.0},
//!   {"operator": "SummaryMerge", "phase": null, "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 1200.0, "ns_per_invocation": 0.0},
//!   {"operator": "SummaryEstimate", "phase": null, "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 400.0, "ns_per_invocation": 0.0}],
//!  "ns_per_evaluation": 0.0}
//! ```
//!
//! Only this converter reads the sketch-bench format; the planner reads the
//! table. Rows plan selection cannot price from the table are skipped with
//! the reason: exact accumulators, Hydra structures (shared across groups,
//! measured at one label width) and sketch names with no planner sketch.
//!
//! [`CostInputs::measured_operation_costs`]: asap_cost::CostInputs::measured_operation_costs

use asap_cost::{MeasuredOperationCost, MeasuredOperationCosts, SummaryOperation};
use asap_types::summary_type::SketchParams;
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
    /// The measured table in its object form: per converted row, an update
    /// becomes `SummaryAgg` and `SummaryTumblingWindowChain` entries, a merge a
    /// `SummaryMerge` entry and an estimate a `SummaryEstimate` entry, each
    /// for both phases, in row order.
    pub measured_operation_costs: MeasuredOperationCosts,
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
    // Test-first stub: the rows are still converted into the earlier
    // format's entries, which the implementation replaces by `OperatorCost`
    // entries.
    let mut legacy_entries: Vec<MeasuredOperationCost> = Vec::new();
    let mut skipped_rows = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let sketch_name = row["sketch"]
            .as_str()
            .ok_or(format!("row {index}: no `sketch` name"))?;
        let params = &row["sketch_config"]["params"];
        let sketch = match sketch_params(sketch_name, params) {
            Ok(Some(sketch)) => sketch,
            Ok(None) => {
                skipped_rows.push(SkippedRow {
                    sketch_name: sketch_name.to_string(),
                    reason: skip_reason(sketch_name).to_string(),
                });
                continue;
            }
            Err(error) => return Err(format!("row {index} ({sketch_name}): {error}")),
        };
        if legacy_entries.iter().any(|entry| entry.sketch == sketch) {
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
            legacy_entries.push(MeasuredOperationCost {
                sketch: sketch.clone(),
                operation,
                cpu_ns_per_operation: seconds * 1e9,
            });
        }
    }
    let _ = (legacy_entries, skipped_rows);
    todo!("write the object form: OperatorCost entries, phase None")
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
        SketchParams::Kll { k: integer("k")? }
    } else if sketch_name.starts_with("dd") {
        SketchParams::DDSketch {
            alpha: params["alpha"]
                .as_f64()
                .ok_or("parameter `alpha` is not a number")?,
        }
    } else if sketch_name == "hll" {
        SketchParams::Hll {
            precision: u8::try_from(integer("lg_k")?).map_err(|e| e.to_string())?,
        }
    } else if sketch_name == "kmv" {
        SketchParams::Kmv { k: integer("k")? }
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
    /// are skipped. Each converted row gives four `OperatorCost` entries for
    /// both phases (an update prices `SummaryAgg` and
    /// `SummaryTumblingWindowChain`, a merge `SummaryMerge`, an estimate
    /// `SummaryEstimate`), with seconds turned into nanoseconds per unit;
    /// the table written as JSON reads back equal.
    #[test]
    fn a_small_sample_converts_and_round_trips() {
        use asap_cost::{OperatorCost, OperatorKind};
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
        let kll = SketchParams::Kll { k: 200 };
        let cms = SketchParams::CmsWithHeap {
            width: 1024,
            depth: 3,
            heap_size: 32,
        };
        let expected = [
            (&kll, OperatorKind::SummaryAgg, 85.0),
            (&kll, OperatorKind::SummaryTumblingWindowChain, 85.0),
            (&kll, OperatorKind::SummaryMerge, 1200.0),
            (&kll, OperatorKind::SummaryEstimate, 400.0),
            (&cms, OperatorKind::SummaryAgg, 60.0),
            (&cms, OperatorKind::SummaryTumblingWindowChain, 60.0),
            (&cms, OperatorKind::SummaryMerge, 5000.0),
            (&cms, OperatorKind::SummaryEstimate, 350.0),
        ];
        let table = &converted.measured_operation_costs;
        assert_eq!(table.operators.len(), expected.len());
        for (entry, (sketch, operator, ns)) in table.operators.iter().zip(expected) {
            let OperatorCost {
                operator: got_operator,
                phase,
                sketch: got_sketch,
                exact,
                measure,
                ns_per_unit,
                ns_per_invocation,
                memory_bytes_per_state,
            } = entry;
            assert_eq!(
                (got_operator, got_sketch.as_ref(), *phase),
                (&operator, Some(sketch), None)
            );
            assert_eq!(
                (exact, measure, memory_bytes_per_state),
                (&None, &None, &None)
            );
            assert_eq!(*ns_per_invocation, 0.0);
            assert!(
                (ns_per_unit - ns).abs() < 1e-9 * ns,
                "{ns_per_unit} vs {ns}"
            );
        }
        assert_eq!(table.ns_per_evaluation, 0.0);
        let skipped: Vec<_> = converted
            .skipped_rows
            .iter()
            .map(|s| s.sketch_name.as_str())
            .collect();
        assert_eq!(skipped, ["exact-sum", "hydra-hll"]);

        let written = serde_json::to_string(table).unwrap();
        let read: MeasuredOperationCosts = serde_json::from_str(&written).unwrap();
        assert_eq!(&read, table);
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
