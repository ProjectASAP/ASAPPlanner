//! The windows of one tumbling-window chain as a store returns them to a
//! `SummaryMerge`: one batch per 1-minute window, one row per series in key
//! order (the series' label, then its state), every state held by the
//! store. Every series is in every window, so the windows list the same
//! rows in the same order, as a chain over a stable set of series does.
//!
//! Example: `WindowShape { series: 1_000, windows: 60, samples_per_window:
//! 600 }` is `rate(m[1h])` at 10 Hz read every 60 s over 1,000 series.
use asap_executor::operators::{Operator, RowWeight};
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_executor::planner::summary_type::{ExactKind, SketchAlgorithm, SketchParams};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, SchemaRef, Value};
use futures::{executor::block_on, StreamExt};
use planner_types::stages::Lifetime;
use std::num::NonZeroU32;
use std::sync::Arc;

/// The width of every window.
pub const WINDOW_MS: i64 = 60_000;

/// How many windows of how many series, each series with how many samples
/// per window.
#[derive(Clone, Copy, Debug)]
pub struct WindowShape {
    pub series: usize,
    pub windows: usize,
    pub samples_per_window: usize,
}

/// A family and its θ (`None` for an exact family), as a build node
/// carries them.
pub type Family = (FieldDataType, Option<SketchParams>);

fn plain(name: &str, dtype: DataType) -> Field {
    Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
    }
}

/// Raw samples: 0 `series`, 1 `ts`, 2 `value`, 3 `item`.
fn samples_schema() -> SchemaRef {
    Arc::new(Schema {
        fields: vec![
            plain("series", DataType::Utf8),
            plain("ts", DataType::Timestamp),
            plain("value", DataType::Float64),
            plain("item", DataType::Utf8),
        ],
        time_index: Some(1),
        unique_keys: vec![],
        closed: false,
    })
}

/// A uniform number in [0, 1) from `seed`.
fn uniform(seed: u64) -> f64 {
    let mut x = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

fn seed(series: usize, window: usize, sample: usize) -> u64 {
    ((series as u64) << 40) ^ ((window as u64) << 20) ^ sample as u64
}

/// Whether `family` is a counter (rate, increase, irate).
pub fn is_counter(family: &FieldDataType) -> bool {
    matches!(
        family,
        FieldDataType::ExactAggregate(ExactKind::Rate | ExactKind::Increase | ExactKind::IRate, _)
    )
}

/// Sample `sample` of `series` in window `window`. A counter grows by up
/// to 10 per sample and resets to 0 at window 3, 10, 17, … of every fifth
/// series; any other value is log-uniform in [1, 1000], like a latency.
pub fn sample_value(
    family: &FieldDataType,
    shape: WindowShape,
    series: usize,
    window: usize,
    sample: usize,
) -> f64 {
    if is_counter(family) {
        let since_reset = if series.is_multiple_of(5) {
            let reset = (window + 4) / 7 * 7;
            (window + 4 - reset) * shape.samples_per_window + sample
        } else {
            window * shape.samples_per_window + sample
        };
        (since_reset as f64) * (1.0 + (series % 10) as f64)
    } else {
        1000f64.powf(uniform(seed(series, window, sample)))
    }
}

/// The item of sample `sample` of `series` in window `window`, for a keyed
/// sketch: one of 50 items, skewed to low numbers (`i0` about 14% of the
/// samples, `i49` about 1%).
pub fn sample_item(series: usize, window: usize, sample: usize) -> String {
    let u = uniform(!seed(series, window, sample));
    format!("i{}", (50.0 * u * u) as usize)
}

/// The timestamp of sample `sample` of window `window`, inside `(window ·
/// 60 s, (window + 1) · 60 s]`.
fn sample_ms(shape: WindowShape, window: usize, sample: usize) -> i64 {
    window as i64 * WINDOW_MS
        + 1
        + sample as i64 * ((WINDOW_MS - 1) / shape.samples_per_window as i64)
}

/// Series `series`' label: its series identity as a PromQL source
/// encodes it (every label, as canonical JSON), zero-padded so key order is
/// number order, e.g.
/// `{"__name__":"requests_total","instance":"i00017","job":"j7","region":"r2"}`.
pub fn series_label(series: usize) -> String {
    format!(
        r#"{{"__name__":"requests_total","instance":"i{series:05}","job":"j{}","region":"r{}"}}"#,
        series % 10,
        series % 3
    )
}

fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: i64::MAX / 2,
        },
        Limits::default(),
    )
    .unwrap()
}

/// Run `operator` over `inputs` from one source: its one output batch.
fn run_one(operator: Operator, schema: SchemaRef, inputs: Vec<Batch>) -> Batch {
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema, inputs).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    dag.add(1, vec![0], operator, Lifetime::None).unwrap();
    block_on(async {
        let mut output = dag.execute(&[1], context()).unwrap().remove(0);
        let batch = output.next().await.expect("one batch").unwrap();
        assert!(output.next().await.is_none(), "one batch");
        (*batch).clone()
    })
}

/// The chain's windows of `family` in `shape`, built as an ingestion run
/// builds each: a per-series build (keyed by `item` for Count-Min with
/// heap) over the window's samples.
pub fn stored_windows(family: &Family, shape: WindowShape) -> Vec<Batch> {
    let schema = samples_schema();
    (0..shape.windows)
        .map(|window| {
            let rows = (0..shape.series)
                .flat_map(|series| {
                    (0..shape.samples_per_window).map(move |sample| (series, sample))
                })
                .map(|(series, sample)| {
                    vec![
                        Value::Utf8(series_label(series).into()),
                        Value::Timestamp(sample_ms(shape, window, sample)),
                        Value::Float64(sample_value(&family.0, shape, series, window, sample)),
                        Value::Utf8(sample_item(series, window, sample).into()),
                    ]
                })
                .collect();
            let input = Batch::try_new(schema.clone(), rows).unwrap();
            let build = if family.0 == FieldDataType::Sketch(SketchAlgorithm::CmsWithHeap) {
                Operator::per_group_keyed_summary_build(
                    schema.clone(),
                    family.0.clone(),
                    family.1.clone(),
                    vec![3],
                    RowWeight::Constant(NonZeroU32::new(1).unwrap()),
                    vec![0],
                )
            } else {
                Operator::per_group_summary_build(
                    schema.clone(),
                    family.0.clone(),
                    family.1.clone(),
                    2,
                    Some(1),
                    vec![0],
                )
            }
            .unwrap();
            run_one(build, schema.clone(), vec![input])
        })
        .collect()
}

/// The `SummaryMerge` of `windows` per series: one row per series, its
/// merged state.
pub fn merge(windows: &[Batch]) -> Batch {
    let schema = windows[0].schema().clone();
    run_one(
        Operator::summary_merge(schema.clone(), 1, vec![0]).unwrap(),
        schema,
        windows.to_vec(),
    )
}
