//! Cost of compressed-sample windows (design doc
//! `compressed-sample-windows.md`, section 3.3; tests C1–C3 and M1).

use std::time::Duration;

use asap_sketchlib::{CompressedSamples, SampleCodec};
use asap_types::ir::properties::ExecutionDataState;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, PROMQL_SERIES_IDENTITY};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode;
use asap_types::summary_type::SummaryUpdate;
use asap_types::workload::{DurationMs, SourceFacts};

use super::*;
use crate::{
    compressed_bits_per_sample, compressed_samples_state_bytes,
    ANALYTICAL_DECODE_NS_PER_COMPRESSED_SAMPLE, ASSUMED_COMPRESSED_BITS_PER_SAMPLE,
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
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: Some(1_000),
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
            _ => None,
        })
        .collect()
}

/// C1. A window of 300 samples at 58.2 bits holds 25 + ceil(300 · 58.2 / 8)
/// = 25 + 2,183 = 2,208 bytes (the codec-0 header and the payload). The
/// formula equals the kernel's `encoded_bytes` for sealed codec-0 blocks
/// whose bits per sample are exact (256 samples, so `payload_bits / 256` is
/// exact in binary; values that are not short decimals, so sealing keeps
/// codec 0), and is at most 6 bytes over a sealed codec-1 block (its header
/// is 19 bytes). The chain's window row is that, the 8-byte blob offset and
/// the 72 bytes beside a state (window end and series identity), 2,288
/// bytes, kept for 12 + 1 windows of 10,000 series in 1 h: 297,440,000
/// bytes, section 4's 0.30 GB. Fails if the state is priced as a sketch or
/// an exact accumulator, if the header or offset is left out, or if the
/// payload is rounded per sample.
#[test]
fn compressed_window_bytes() {
    assert_eq!(compressed_samples_state_bytes(300, 58.2), 2_208);
    assert_eq!(compressed_samples_state_bytes(0, 58.2), 25);
    let sealed = |samples: &[(i64, f64)]| {
        let mut block = CompressedSamples::new();
        for (timestamp_ms, value) in samples {
            block.push(*timestamp_ms, *value);
        }
        block.seal()
    };
    let codec_0_blocks: [Vec<(i64, f64)>; 3] = [
        (0..256)
            .map(|i| (i * 1_000, (i + 1) as f64 / 3.0))
            .collect(),
        (0..256).map(|i| (i * 1_000, 1.0 / 3.0)).collect(),
        (0..256)
            .map(|i| (i * 1_000 + (i % 7) * 13, ((i * 7919) % 1000) as f64 / 7.0))
            .collect(),
    ];
    for samples in codec_0_blocks {
        let block = sealed(&samples);
        assert_eq!(block.codec(), SampleCodec::Gorilla);
        let bits_per_sample = block.payload_bits() as f64 / samples.len() as f64;
        assert_eq!(
            compressed_samples_state_bytes(samples.len() as u64, bits_per_sample) as usize,
            block.encoded_bytes()
        );
    }
    // Two-decimal latencies: sealed as codec 1.
    let latencies: Vec<(i64, f64)> = (0..300)
        .map(|i| (i * 1_000, ((i * 7919) % 50_000) as f64 / 100.0))
        .collect();
    let block = sealed(&latencies);
    assert_eq!(block.codec(), SampleCodec::DecimalZstd);
    let bits_per_sample = block.payload_bits() as f64 / latencies.len() as f64;
    let charged = compressed_samples_state_bytes(latencies.len() as u64, bits_per_sample) as usize;
    assert!(
        (block.encoded_bytes()..=block.encoded_bytes() + 6).contains(&charged),
        "charged {charged}, encoded {}",
        block.encoded_bytes()
    );

    let settings = CostInputs::default();
    let data = ten_thousand_series(Some(58.2));
    let scan = data_scan();
    let chain = chain(3_600_000);
    let usage = chain_usage(&chain, &scan, &data, &settings);
    let window_row_bytes = 2_208 + 8 + 72;
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
/// 25 + 2,700 = 2,725 state bytes, 2,805 per row) and records a
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
    let window_row_bytes = 25 + 2_700 + 8 + 72;
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

/// M1. The calibration step's measurement is the payload bits of the
/// blocks the kernel seals, over their samples. Two 256-sample blocks 1 s
/// apart whose values are not short decimals, (i + 1) / 3 and the constant
/// 1/3, seal as codec 0 and take 16,787 and 525 payload bits (counted by a
/// separate reference implementation of the format): 17,312 / 512 =
/// 33.8125 bits per sample. Over two-decimal values the measurement is the
/// sealed codec-1 block's payload bits, so it measures whichever codec wins
/// on the data. A workload declaring the value is priced with it (25 +
/// 1,268 bytes per 300-sample window) and records no assumption. Fails if
/// the calibration counts the header, measures the open (unsealed) block,
/// averages per block instead of per sample, or if a declared value is
/// ignored.
#[test]
fn calibrated_bits_are_the_encoders() {
    let blocks = vec![
        (0..256)
            .map(|i| (i * 1_000, (i + 1) as f64 / 3.0))
            .collect::<Vec<_>>(),
        (0..256).map(|i| (i * 1_000, 1.0 / 3.0)).collect::<Vec<_>>(),
    ];
    let measured = compressed_bits_per_sample(&blocks);
    assert_eq!(measured, 33.8125);

    let latencies: Vec<(i64, f64)> = (0..300)
        .map(|i| (i * 1_000, ((i * 7919) % 50_000) as f64 / 100.0))
        .collect();
    let mut block = CompressedSamples::new();
    for (timestamp_ms, value) in &latencies {
        block.push(*timestamp_ms, *value);
    }
    let block = block.seal();
    assert_eq!(block.codec(), SampleCodec::DecimalZstd);
    assert_eq!(
        compressed_bits_per_sample(&[latencies]),
        block.payload_bits() as f64 / 300.0
    );

    let settings = CostInputs::default();
    let data = ten_thousand_series(Some(measured));
    let scan = data_scan();
    let chain = chain(3_600_000);
    let usage = chain_usage(&chain, &scan, &data, &settings);
    let state_bytes = compressed_samples_state_bytes(300, measured);
    assert_eq!(state_bytes, 25 + 1_268);
    assert!(
        usage.cost.detail.contains(&format!(
            "keeps {} bytes",
            (state_bytes + 8 + 72) * 13 * 10_000
        )),
        "{}",
        usage.cost.detail
    );
    assert!(bits_assumptions(&usage.cost).is_empty());
}
