//! The measured cost table: [`lookup`] (exact match, interpolation in log
//! size, phase shadowing, no extrapolation), its JSON round trip and the
//! formats the `asap_run` design deletes. Every expected price is
//! computed by hand from the entries below.

use asap_cost::{
    lookup, CostInputs, MeasuredOperationCosts, OperatorCost, OperatorKind, StateFamily,
};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::summary_type::{ExactKind, SketchParams};

/// A sketch's size for interpolation: KLL's `k`, 1 for any other sketch.
fn size_of(params: &SketchParams) -> f64 {
    match params {
        SketchParams::Kll { k } => *k as f64,
        _ => 1.0,
    }
}

/// A `SummaryAgg` entry for KLL with `k` at `phase`.
fn kll_entry(
    k: u32,
    phase: Option<ExecutionTiming>,
    ns_per_unit: f64,
    ns_per_invocation: f64,
    memory_bytes_per_state: Option<f64>,
) -> OperatorCost {
    OperatorCost {
        operator: OperatorKind::SummaryAgg,
        phase,
        sketch: Some(SketchParams::Kll { k }),
        exact: None,
        measure: None,
        ns_per_unit,
        ns_per_invocation,
        memory_bytes_per_state,
        ns_per_state_emitted: 0.0,
        measured_series_count: None,
        measures: Vec::new(),
    }
}

/// KLL `k = 200` at 100 ns/unit, 1,000 ns/invocation, 2,000 bytes, and
/// `k = 800` at 300 ns/unit, 3,000 ns/invocation, 8,000 bytes, for both
/// phases.
fn kll_table() -> MeasuredOperationCosts {
    MeasuredOperationCosts {
        operators: vec![
            kll_entry(200, None, 100.0, 1_000.0, Some(2_000.0)),
            kll_entry(800, None, 300.0, 3_000.0, Some(8_000.0)),
        ],
        ..MeasuredOperationCosts::default()
    }
}

/// A price's fields: ns per unit, ns per invocation, bytes per state.
#[derive(Debug, PartialEq)]
struct Price {
    ns_per_unit: f64,
    ns_per_invocation: f64,
    memory_bytes_per_state: Option<f64>,
}

fn kll_price_at(table: &MeasuredOperationCosts, phase: ExecutionTiming, k: u32) -> Option<Price> {
    lookup(
        table,
        OperatorKind::SummaryAgg,
        phase,
        StateFamily::Sketch(&SketchParams::Kll { k }),
        size_of,
    )
    .map(|price| Price {
        ns_per_unit: price.ns_per_unit,
        ns_per_invocation: price.ns_per_invocation,
        memory_bytes_per_state: price.memory_bytes_per_state,
    })
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1e-9 * expected.abs().max(1.0),
        "{actual} != {expected}"
    );
}

/// An entry with exactly the node's parameters is its price, not an
/// interpolation.
#[test]
fn lookup_exact_params_win() {
    let price = kll_price_at(&kll_table(), ExecutionTiming::QueryTime, 200).unwrap();
    assert_eq!(price.ns_per_unit, 100.0);
    assert_eq!(price.ns_per_invocation, 1_000.0);
    assert_eq!(price.memory_bytes_per_state, Some(2_000.0));
}

/// k = 400 lies at ln(400/200) / ln(800/200) = 0.5 between the two
/// entries in log size: 100 + 0.5·200 = 200 ns/unit, 2,000 ns/invocation,
/// 5,000 bytes. Linear in size would give 166.7 ns/unit.
#[test]
fn lookup_interpolates_in_log_size() {
    let price = kll_price_at(&kll_table(), ExecutionTiming::QueryTime, 400).unwrap();
    assert_close(price.ns_per_unit, 200.0);
    assert_close(price.ns_per_invocation, 2_000.0);
    assert_close(price.memory_bytes_per_state.unwrap(), 5_000.0);
}

/// Below the smallest and above the largest measured size, the nearest
/// entry's price: 100 at k = 100 (not 33), 300 at k = 1600 (not 367).
#[test]
fn lookup_no_extrapolation() {
    let table = kll_table();
    let below = kll_price_at(&table, ExecutionTiming::QueryTime, 100).unwrap();
    let above = kll_price_at(&table, ExecutionTiming::QueryTime, 1_600).unwrap();
    assert_eq!(below.ns_per_unit, 100.0);
    assert_eq!(above.ns_per_unit, 300.0);
}

/// A query-time entry for k = 200 at 999 ns/unit hides both both-phase
/// entries at query time: k = 400 has only k = 200 to go by, so 999. At
/// ingestion time there is no ingestion entry, so the both-phase pair
/// interpolates to 200.
#[test]
fn lookup_phase_entries_shadow_both() {
    let mut table = kll_table();
    table.operators.push(kll_entry(
        200,
        Some(ExecutionTiming::QueryTime),
        999.0,
        0.0,
        None,
    ));
    let query = kll_price_at(&table, ExecutionTiming::QueryTime, 400).unwrap();
    let ingestion = kll_price_at(&table, ExecutionTiming::IngestionTime, 400).unwrap();
    assert_eq!(query.ns_per_unit, 999.0);
    assert_close(ingestion.ns_per_unit, 200.0);
}

/// With no bytes on k = 800, k = 400 has one memory neighbour only: no
/// measured bytes (the formulas' bytes apply), never an interpolation
/// with 0.
#[test]
fn lookup_memory_needs_both_neighbours() {
    let table = MeasuredOperationCosts {
        operators: vec![
            kll_entry(200, None, 100.0, 1_000.0, Some(2_000.0)),
            kll_entry(800, None, 300.0, 3_000.0, None),
        ],
        ..MeasuredOperationCosts::default()
    };
    let price = kll_price_at(&table, ExecutionTiming::QueryTime, 400).unwrap();
    assert_eq!(price.memory_bytes_per_state, None);
}

/// KLL entries do not price a DDSketch node.
#[test]
fn lookup_other_algorithm_misses() {
    let price = lookup(
        &kll_table(),
        OperatorKind::SummaryAgg,
        ExecutionTiming::QueryTime,
        StateFamily::Sketch(&SketchParams::DDSketch { alpha: 0.01 }),
        size_of,
    );
    assert!(price.is_none());
}

/// A table with one of everything a JSON file can hold.
fn full_table() -> MeasuredOperationCosts {
    MeasuredOperationCosts {
        operators: vec![
            kll_entry(
                200,
                Some(ExecutionTiming::IngestionTime),
                100.5,
                1_000.25,
                Some(2_048.0),
            ),
            OperatorCost {
                operator: OperatorKind::SummaryMerge,
                phase: Some(ExecutionTiming::QueryTime),
                sketch: None,
                exact: Some(ExactKind::Sum),
                measure: None,
                ns_per_unit: 3.75,
                ns_per_invocation: 0.0,
                memory_bytes_per_state: None,
                ns_per_state_emitted: 0.0,
                measured_series_count: None,
                measures: Vec::new(),
            },
            OperatorCost {
                operator: OperatorKind::Aggregate,
                phase: None,
                sketch: None,
                exact: None,
                measure: Some("quantile".into()),
                ns_per_unit: 12.0,
                ns_per_invocation: 250.0,
                memory_bytes_per_state: None,
                ns_per_state_emitted: 0.0,
                measured_series_count: None,
                measures: Vec::new(),
            },
            OperatorCost {
                operator: OperatorKind::Scan,
                phase: None,
                sketch: None,
                exact: None,
                measure: None,
                ns_per_unit: 20.0,
                ns_per_invocation: 5_000.0,
                memory_bytes_per_state: None,
                ns_per_state_emitted: 0.0,
                measured_series_count: None,
                measures: Vec::new(),
            },
        ],
        ns_per_evaluation: 50_000.0,
        ns_per_evaluation_step: 0.0,
        ns_per_ingested_sample: 0.0,
        refresh_io_wait: Default::default(),
    }
}

/// `costs.json` round-trips: written, read back and written again it is
/// byte-equal, and it validates.
#[test]
fn costs_json_round_trips() {
    let table = full_table();
    let written = serde_json::to_string_pretty(&table).unwrap();
    let read: MeasuredOperationCosts = serde_json::from_str(&written).unwrap();
    assert_eq!(read, table);
    assert_eq!(serde_json::to_string_pretty(&read).unwrap(), written);
    let inputs = CostInputs {
        measured_operation_costs: read,
        ..CostInputs::default()
    };
    inputs.validate().unwrap();
}

/// `provenance` is deleted (where a table was measured is recorded by the
/// calibration run, not in the table) and so is the array format of
/// earlier versions: a table carrying either no longer loads.
#[test]
fn costs_json_rejects_removed_fields() {
    let with_provenance = r#"{"operators": [], "ns_per_evaluation": 0.0,
        "provenance": {"executor_version": "0.1.0", "machine": "x86_64"}}"#;
    assert!(
        serde_json::from_str::<MeasuredOperationCosts>(with_provenance).is_err(),
        "a table with `provenance` must not load"
    );
    let legacy_array = r#"[{"sketch": {"Kll": {"k": 200}}, "operation": "update",
        "cpu_ns_per_operation": 100.0}]"#;
    assert!(
        serde_json::from_str::<MeasuredOperationCosts>(legacy_array).is_err(),
        "a table in the array format must not load"
    );
}

/// A `CostInputs` file (the `asap_planner --cost-inputs` format) whose
/// measured table still carries `provenance` is refused, and the message
/// names the field, so a stale file fails loudly instead of loading with
/// the field ignored.
#[test]
fn a_cost_inputs_file_with_provenance_is_refused() {
    let mut inputs = serde_json::to_value(CostInputs::default()).unwrap();
    inputs["measured_operation_costs"] = serde_json::json!({
        "operators": [],
        "ns_per_evaluation": 0.0,
        "provenance": {"executor_version": "0.1.0", "machine": "x86_64"}
    });
    let error = serde_json::from_value::<CostInputs>(inputs)
        .expect_err("a cost-inputs file with `provenance` must not load");
    assert!(error.to_string().contains("provenance"), "{error}");
}
