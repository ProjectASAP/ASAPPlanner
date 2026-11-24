//! Cost of compressed-sample windows (design doc
//! `compressed-sample-windows.md`, section 3.3; tests C1–C3 and M1).

use std::time::Duration;

use asap_types::ir::properties::ExecutionDataState;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, PROMQL_SERIES_IDENTITY};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode;
use asap_types::summary_type::SummaryUpdate;
use asap_types::workload::{DurationMs, SourceFacts};

use super::*;
use crate::{
    compressed_samples_state_bytes, ANALYTICAL_DECODE_NS_PER_COMPRESSED_SAMPLE,
    ASSUMED_COMPRESSED_BITS_PER_SAMPLE,
};

/// 10,000 series of the gauge `data`, one sample per second, declaring
/// `bits_per_sample` for `value` when given.
fn ten_thousand_series(bits_per_sample: Option<f64>) -> DataWorkload {
    DataWorkload {
        sources: BTreeMap::from([(
            "data".to_string(),
            SourceFacts {
                series_count: Some(10_000),
                sample_interval: Some(DurationMs(1_000)),
                column_compressed_bits_per_sample: bits_per_sample
                    .map(|bits| BTreeMap::from([("value".to_string(), bits)]))
                    .unwrap_or_default(),
                ..SourceFacts::default()
            },
        )]),
        ..DataWorkload::default()
    }
}

fn node(payload: Payload) -> PhysicalASAPDAGNode {
    PhysicalASAPDAGNode {
        id: 0,
        payload,
        output_state: ExecutionDataState::QUERY_ROWS,
        lifetime: Lifetime::None,
        parameters: None,
        output_schema: Schema::default(),
        retention_ms: None,
        selected_physical_candidate: None,
        view_coverage: None,
    }
}

/// The output schema of a compressed-samples chain keyed by series
/// identity: window end, state, identity.
fn chain_schema() -> Schema {
    Schema::new(vec![
        Field::new("ts", FieldDataType::Plain(DataType::Timestamp), false),
        Field::new("state", FieldDataType::CompressedSamples, false),
        Field::new(
            PROMQL_SERIES_IDENTITY,
            FieldDataType::Plain(DataType::Utf8),
            false,
        ),
    ])
}

/// The 5 min compressed-samples chain of `data`, kept in memory across
/// evaluations for `retention_ms`.
fn chain(retention_ms: u64) -> PhysicalASAPDAGNode {
    PhysicalASAPDAGNode {
        retention_ms: Some(retention_ms),
        output_state: ExecutionDataState::INGESTION_SUMMARY,
        lifetime: Lifetime::AcrossEvaluations(StorageMedium::Memory),
        output_schema: chain_schema(),
        ..node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: FieldDataType::CompressedSamples,
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 300_000,
        }))
    }
}

/// One second of `data`'s arriving rows (10,000), as the scan feeds the
/// chain.
fn one_second_of_rows(scan: &PhysicalASAPDAGNode) -> ProducerOutput<'_> {
    ProducerOutput {
        payload: &scan.payload,
        output_schema: &scan.output_schema,
        output: EdgeStatistics {
            rows: 10_000,
            bytes: 10_000 * 48,
        },
        sources: BTreeSet::from(["data"]),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::from(["value"]),
        tumbling_windows: 1.0,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: Some(1_000),
        raw_rows_per_ms: None,
        folded_per_row: None,
        raw_rows_short_of_range: None,
        state_data_span_ms: None,
        raw_rows_shifted_back_ms: 0,
        states_last_merged: false,
        state_input_rows: None,
    }
}

fn data_scan() -> PhysicalASAPDAGNode {
    node(Payload::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "data".into(),
        },
        predicates: vec![],
        schema: Schema::default(),
    }))
}

/// The chain's usage over `data` declaring `bits_per_sample`.
fn chain_usage<'a>(
    chain: &'a PhysicalASAPDAGNode,
    scan: &'a PhysicalASAPDAGNode,
    data: &'a DataWorkload,
    settings: &'a CostInputs,
) -> NodeResourceUsageAndCost<'a> {
    node_resource_usage_and_cost(
        chain,
        &[one_second_of_rows(scan)],
        (1, 1.0),
        None,
        None,
        data,
        settings,
    )
    .unwrap()
}

/// The `CompressedBitsPerSample` assumptions of a node's cost.
fn bits_assumptions(cost: &NodeCost) -> Vec<f64> {
    cost.assumed_data_facts
        .iter()
        .filter_map(|fact| match &fact.assumption {
            DataFactAssumption::CompressedBitsPerSample {
                column,
                assumed_bits_per_sample,
            } => {
                assert_eq!(column, "value");
                assert_eq!(fact.sources, ["data"]);
                Some(*assumed_bits_per_sample)
            }
            DataFactAssumption::DDSketchValueMagnitudeRange { .. }
            | DataFactAssumption::InOrderArrival => None,
        })
        .collect()
}

/// C1. A window of 300 samples at the calibrated 58.2 bits per sample
/// (the deployment's sealed block bits over its samples, block overhead
/// included) holds ceil(300 · 58.2 / 8) = 2,183 bytes; the chain's window
/// row is that, the 8-byte blob offset and the 72 bytes beside a state
/// (window end and series identity), 2,263 bytes, kept for 12 + 1 windows
/// of 10,000 series in 1 h: 294,190,000 bytes, section 4's 0.29 GB. The
/// planner sizes the state from the declared fact alone: the codec is the
/// deployment's. Fails if the state is priced as a sketch or an exact
/// accumulator, if the offset is left out, or if the payload is rounded
/// per sample.
#[test]
fn compressed_window_bytes() {
    assert_eq!(compressed_samples_state_bytes(300, 58.2), 2_183);
    assert_eq!(compressed_samples_state_bytes(0, 58.2), 0);
    assert_eq!(compressed_samples_state_bytes(1, 0.5), 1);

    let settings = CostInputs::default();
    let data = ten_thousand_series(Some(58.2));
    let scan = data_scan();
    let chain = chain(3_600_000);
    let usage = chain_usage(&chain, &scan, &data, &settings);
    let window_row_bytes = 2_183 + 8 + 72;
    assert!(
        usage
            .cost
            .detail
            .contains(&format!("keeps {} bytes", window_row_bytes * 13 * 10_000)),
        "{}",
        usage.cost.detail
    );
    assert!(bits_assumptions(&usage.cost).is_empty());
}

/// The decode of `range` from the 5 min chain in read mode
/// `reads_unsealed_window`, with the chain's usage as its producer.
fn decode_work_ms(range: Duration, reads_unsealed_window: bool) -> f64 {
    let settings = CostInputs::default();
    let data = ten_thousand_series(Some(58.2));
    let scan = data_scan();
    let chain = chain(3_600_000);
    let chain_usage = chain_usage(&chain, &scan, &data, &settings);
    let decode = node(Payload::ASAP(ASAPOp::DecodeSamplesInRange {
        child: 0,
        range,
        kind: TimeRangeKind::Range,
        labels: vec![],
        reads_unsealed_window,
    }));
    let usage = node_resource_usage_and_cost(
        &decode,
        &[chain_usage.producer_output(&chain.payload, &chain.output_schema)],
        (1, 1.0),
        None,
        None,
        &data,
        &settings,
    )
    .unwrap();
    assert_eq!(
        usage.output.rows,
        range.as_secs() * 10_000,
        "the decode outputs the range's raw rows"
    );
    usage.work_ms_per_execution
}

/// C2. A 1 h read at 1 sample/s from 5 min windows over 10,000 series
/// decodes (3,600 + 150) × 10,000 samples per evaluation when it reads the
/// unsealed window (half a window outside the range on average) and
/// 3,600 × 10,000 when it reads sealed windows only, each at
/// `ANALYTICAL_DECODE_NS_PER_COMPRESSED_SAMPLE`; it outputs the range's
/// 36,000,000 raw rows either way. Fails if the decode is charged per
/// window or per state (as a merge is), or ignores the read mode.
#[test]
fn decode_charged_per_sample_in_range() {
    let hour = Duration::from_secs(3_600);
    let unsealed = decode_work_ms(hour, true);
    let sealed = decode_work_ms(hour, false);
    let expected_ms = |samples: f64| samples * ANALYTICAL_DECODE_NS_PER_COMPRESSED_SAMPLE / 1e6;
    let relative = |charged: f64, expected: f64| (charged / expected - 1.0).abs();
    assert!(
        relative(unsealed, expected_ms(3_750.0 * 10_000.0)) < 1e-6,
        "unsealed: {unsealed} ms"
    );
    assert!(
        relative(sealed, expected_ms(3_600.0 * 10_000.0)) < 1e-6,
        "sealed: {sealed} ms"
    );
}

/// C3. Retained for its longest reader, 1 h (Stage 3's $h_v$; a 5 min
/// reader beside it changes nothing), the chain keeps ceil(3,600 / 300) + 1 = 13 windows
/// per series, not 2 (the 5 min reader's) and not 12. With nothing
/// declared it is priced at `ASSUMED_COMPRESSED_BITS_PER_SAMPLE` (72 bits:
/// 2,700 state bytes, 2,780 per row) and records a
/// `CompressedBitsPerSample` assumption naming `data`. Fails if the
/// retention misses the window being built, if an undeclared column is
/// priced as free or at a production average, or if the assumption goes
/// unreported.
#[test]
fn chain_keeps_longest_range_plus_one_window() {
    assert_eq!(ASSUMED_COMPRESSED_BITS_PER_SAMPLE, 72.0);
    let settings = CostInputs::default();
    let data = ten_thousand_series(None);
    let scan = data_scan();
    let chain = chain(3_600_000);
    let usage = chain_usage(&chain, &scan, &data, &settings);
    let window_row_bytes = 2_700 + 8 + 72;
    assert!(
        usage
            .cost
            .detail
            .contains(&format!("keeps {} bytes", window_row_bytes * 13 * 10_000)),
        "{}",
        usage.cost.detail
    );
    assert_eq!(bits_assumptions(&usage.cost), [72.0]);
}

/// M1. The calibration step's facts price the state: a source declaring
/// 19.3 bits per sample, 130 ns to append and seal one and 18 ns to decode
/// one (what a deployment's codec measured on its data) gets 300-sample
/// windows of ceil(300 · 19.3 / 8) = 724 bytes, an encode of 130 ns per
/// arriving row in place of the analytical 100, a 1 h unsealed decode of
/// 37,500,000 samples at 18 ns (675 ms) in place of 20 ns, and records no
/// assumption. Fails if a declared fact is ignored or the analytical
/// constants are charged beside it.
#[test]
fn calibrated_facts_price_the_window() {
    let settings = CostInputs::default();
    let mut data = ten_thousand_series(Some(19.3));
    let facts = data.sources.get_mut("data").unwrap();
    facts
        .column_compressed_sample_encode_ns
        .insert("value".to_string(), 130.0);
    facts
        .column_compressed_sample_decode_ns
        .insert("value".to_string(), 18.0);
    let scan = data_scan();
    let chain = chain(3_600_000);
    let usage = chain_usage(&chain, &scan, &data, &settings);
    assert_eq!(compressed_samples_state_bytes(300, 19.3), 724);
    assert!(
        usage
            .cost
            .detail
            .contains(&format!("keeps {} bytes", (724 + 8 + 72) * 13 * 10_000)),
        "{}",
        usage.cost.detail
    );
    assert!(
        usage.cost.detail.contains("10000 rows x 130 ops"),
        "{}",
        usage.cost.detail
    );
    assert!(bits_assumptions(&usage.cost).is_empty());

    let decode = node(Payload::ASAP(ASAPOp::DecodeSamplesInRange {
        child: 0,
        range: Duration::from_secs(3_600),
        kind: TimeRangeKind::Range,
        labels: vec![],
        reads_unsealed_window: true,
    }));
    let decoded = node_resource_usage_and_cost(
        &decode,
        &[usage.producer_output(&chain.payload, &chain.output_schema)],
        (1, 1.0),
        None,
        None,
        &data,
        &settings,
    )
    .unwrap();
    let expected_ms = 37_500_000.0 * 18.0 / 1e6;
    assert!(
        (decoded.work_ms_per_execution / expected_ms - 1.0).abs() < 1e-6,
        "{} ms",
        decoded.work_ms_per_execution
    );
}
