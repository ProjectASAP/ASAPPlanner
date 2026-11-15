//! What the executor computes at signed zeros (sign test plan, T4 rows f1,
//! f2 and f3). A value's sign is its IEEE sign bit, so `-0.0` is
//! `NonPositive`; these rows check that the executor keeps the sign bit
//! where the planner's sign rules need it:
//!
//! - f1: a sum of only `-0.0` values is `-0.0` (the IEEE additive identity
//!   is `-0.0`; starting a sum at `+0.0` gives `+0.0 + -0.0 = +0.0`);
//! - f2: `sgn(-0.0)` is `-0.0`, as in Prometheus (`sgn(v)` returns `v` for
//!   a zero);
//! - f3: `clamp_min` / `clamp_max` resolve signed zeros as Go's
//!   `math.Max(+0, -0) = +0` and `math.Min(+0, -0) = -0`.
//!
//! Each expected value is the IEEE / Prometheus result, written out by hand.
use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::Source;
use asap_types::ir::NonASAPOp;
use asap_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload;
use futures::{executor::block_on, StreamExt};

/// Evaluation time; a `[1m]` window is `(240 s, 300 s]`.
const EVALUATION_MS: i64 = 300_000;

/// One raw sample of `x{instance}`.
#[derive(Clone)]
struct RawDataSample {
    instance: &'static str,
    timestamp_ms: i64,
    value: f64,
}

/// `values` of `x{instance}`, 10 s apart from 250 s.
fn series(instance: &'static str, values: &[f64]) -> Vec<RawDataSample> {
    values
        .iter()
        .enumerate()
        .map(|(i, value)| RawDataSample {
            instance,
            timestamp_ms: 250_000 + 10_000 * i as i64,
            value: *value,
        })
        .collect()
}

/// One sample `value` of `x{instance}` at the evaluation time, which an
/// instant selector reads (its lookback is the sample interval).
fn latest(instance: &'static str, value: f64) -> Vec<RawDataSample> {
    vec![RawDataSample {
        instance,
        timestamp_ms: EVALUATION_MS,
        value,
    }]
}

/// `query`, lowered exactly with series identity and run at query time over
/// `samples` of `x`: every Float64 value of its answer.
fn evaluate(query: &str, samples: &[RawDataSample]) -> Vec<f64> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap();
    let dag = asap_types::test_support::forwarders::physical_asap_dag_at_query_time(&root).unwrap();
    // Every query reads one series, `x`, from one `Scan`.
    let scans: Vec<_> = dag
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, .. }) => {
                Some((source.clone(), node))
            }
            _ => None,
        })
        .collect();
    let [(source, scan)] = &scans[..] else {
        panic!("{query}: one scan of x expected, found {}", scans.len());
    };
    assert_eq!(*source, Source::TimeSeries { metric: "x".into() });
    let schema = Arc::new(scan.output_schema.clone());
    let rows = samples
        .iter()
        .map(|sample| {
            let labels = BTreeMap::from([
                ("__name__".to_string(), "x".to_string()),
                ("instance".to_string(), sample.instance.to_string()),
            ]);
            promql_rows::series_row(&schema, &labels, sample.timestamp_ms, sample.value).unwrap()
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    let store = InMemoryDataStore::default()
        .with_raw_data(source.clone(), schema, vec![batch])
        .unwrap();
    let roots = [dag.roots[0] as u64];
    let bound = compile(&dag, &[], &roots)
        .unwrap()
        .instantiate(Arc::new(store))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
        },
        Limits::default(),
    )
    .unwrap();
    let batches: Vec<Batch> = block_on(async {
        let mut output = bound.execute(&roots, context).unwrap().remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.unwrap()).clone());
        }
        batches
    });
    batches
        .iter()
        .flat_map(|batch| batch.to_rows())
        .flat_map(|row| {
            row.into_iter().filter_map(|cell| match cell {
                Value::Float64(value) => Some(value),
                _ => None,
            })
        })
        .collect()
}

/// `answers` hold exactly `expected`'s bits (so `-0.0` and `+0.0` differ).
fn assert_bits(query: &str, answers: &[f64], expected: &[f64]) {
    let bits = |values: &[f64]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
    assert_eq!(
        bits(answers),
        bits(expected),
        "{query}: answered {answers:?}, expected {expected:?}"
    );
}

/// f1: a sum, or a mean, of only `-0.0` values is `-0.0`: `sum_over_time`
/// and `avg_over_time` over a window of `-0.0`s, and `sum` over two series
/// whose latest sample is `-0.0` (provisional answer Q1: sums start at
/// `-0.0` and add the Kahan compensation only when it is nonzero).
///
/// Fails today: each is `+0.0`.
#[test]
fn f1_a_sum_of_negative_zeros_keeps_its_sign_bit() {
    let window = series("a", &[-0.0, -0.0, -0.0]);
    let two = [latest("a", -0.0), latest("b", -0.0)].concat();
    let mut wrong = Vec::new();
    for (query, samples) in [
        ("sum_over_time(x[1m])", &window),
        ("avg_over_time(x[1m])", &window),
        ("sum(x)", &two),
        ("avg(x)", &two),
    ] {
        let answers = evaluate(query, samples);
        // provisional answer Q1: every answer is -0.0.
        if answers
            .iter()
            .map(|v| v.to_bits())
            .ne([(-0.0f64).to_bits()])
        {
            wrong.push(format!("{query}: answered {answers:?}, expected [-0.0]"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// f2: `sgn(-0.0)` is `-0.0` and `sgn(+0.0)` is `+0.0`, as in Prometheus
/// (provisional answer Q2).
///
/// Fails today: the executor returns the literal `0.0` for `-0.0`.
#[test]
fn f2_sgn_of_negative_zero_is_negative_zero() {
    for (value, expected) in [(-0.0, -0.0), (0.0, 0.0), (-3.0, -1.0)] {
        let samples = latest("a", value);
        // provisional answer Q2
        assert_bits("sgn(x)", &evaluate("sgn(x)", &samples), &[expected]);
    }
}

/// f3: `clamp_min(-0.0, 0)` is `max(+0, -0) = +0` and
/// `clamp_max(+0.0, -0)` is `min(-0, +0) = -0`, as Go resolves signed
/// zeros (provisional answer Q3).
///
/// Rust leaves `f64::max(-0.0, +0.0)` unspecified, so this may fail today.
#[test]
fn f3_clamp_resolves_signed_zeros_as_prometheus() {
    for (query, value, expected) in [
        ("clamp_min(x, 0)", -0.0, 0.0),
        ("clamp_max(x, -0)", 0.0, -0.0),
        ("clamp(x, 0, 5)", -0.0, 0.0),
        ("clamp(x, -5, -0)", 0.0, -0.0),
    ] {
        let samples = latest("a", value);
        // provisional answer Q3
        assert_bits(query, &evaluate(query, &samples), &[expected]);
    }
}
