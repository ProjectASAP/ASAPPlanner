//! Exact per-series range reductions over raw data samples execute natively
//! (issue #770): an exact PromQL range function (`sum_over_time`, `rate`,
//! `changes`, `stddev_over_time`, …) reduces each series' samples in the
//! window with a per-entity `Aggregate`, which the executor compiles and
//! runs; `timestamp(v)` does so over an instant selector. The samples are
//! bound at the plan's `Scan`, as a runtime binds them, and its `TimeRange`
//! keeps the window.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::selected_dag;
use asap_types::ir::operator::{Reduction, Source};
use asap_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPOperatorPayload};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{
    apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache, NonASAPOp,
    OperatorNode,
};
use futures::{executor::block_on, StreamExt};

/// One raw data sample: every label of its series, its time and value.
#[derive(Clone)]
struct RawDataSample {
    labels: BTreeMap<String, String>,
    timestamp_ms: i64,
    value: f64,
}

/// Evaluation time; every query's window is `(240 s, 300 s]`.
const EVALUATION_MS: i64 = 300_000;
/// The bits of the Prometheus stale marker, which Remote Write carries
/// when a series ends.
const STALE_MARKER: f64 = f64::from_bits(0x7ff0_0000_0000_0002);

/// `query` lowered exactly, with series identity: a per-entity `Aggregate`
/// over the window's raw data samples, as the exact plan that reduces raw data samples
/// runs it.
fn per_entity_plan(query: &str) -> Rc<OperatorNode> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap();
    assert!(
        matches!(
            root.non_asap(),
            Some(NonASAPOp::Aggregate {
                reduction: Reduction::PerEntity,
                ..
            })
        ),
        "{query}: {:?}",
        root.operator
    );
    root
}

/// A raw data sample of `x` for series `instance` at `timestamp_ms`.
fn sample(instance: &str, timestamp_ms: i64, value: f64) -> RawDataSample {
    RawDataSample {
        labels: BTreeMap::from([
            ("__name__".to_string(), "x".to_string()),
            ("instance".to_string(), instance.to_string()),
        ]),
        timestamp_ms,
        value,
    }
}

/// `values` of series `instance`, 10 s apart from 250 s, inside the window.
fn series(instance: &str, values: &[f64]) -> Vec<RawDataSample> {
    values
        .iter()
        .enumerate()
        .map(|(i, value)| sample(instance, 250_000 + 10_000 * i as i64, *value))
        .collect()
}

/// The labels of an answer row, from the output schema's label columns:
/// its series identity's labels and every other non-empty text column.
fn series_labels(schema: &Schema, row: &[Value]) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for (field, cell) in schema.fields.iter().zip(row) {
        let Value::Utf8(text) = cell else { continue };
        if field.name == PROMQL_SERIES_IDENTITY {
            labels.extend(promql_rows::decode_series_identity(text).unwrap());
        } else if !text.is_empty() {
            labels.insert(field.name.clone(), text.to_string());
        }
    }
    labels
}

/// The plan the planner selects for `query` at exact accuracy, lowered
/// with series identity as the planner receives it.
fn planned(query: &str) -> Rc<OperatorNode> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap();
    selected_dag(root, AccuracyTarget::exact())
}

/// Run `query`'s exact per-entity plan at query time over `samples`, bound
/// at its `Scan` of `x`: each answered series' `instance` and value.
fn execute(query: &str, samples: &[RawDataSample]) -> Vec<(String, f64)> {
    without_name(query, run(query, &per_entity_plan(query), samples))
}

/// Each answer's `instance` and value; the answers carry no metric name.
fn without_name(query: &str, answers: Vec<(BTreeMap<String, String>, f64)>) -> Vec<(String, f64)> {
    answers
        .into_iter()
        .map(|(labels, value)| {
            assert!(!labels.contains_key("__name__"), "{query}: {labels:?}");
            (labels["instance"].clone(), value)
        })
        .collect()
}

/// Run `root` at query time over `samples`, bound at its `Scan` of `x`:
/// each answered series' labels and value, by `instance`.
fn run(
    query: &str,
    root: &Rc<OperatorNode>,
    samples: &[RawDataSample],
) -> Vec<(BTreeMap<String, String>, f64)> {
    let timed = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::new(),
    )
    .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let node = |matches: fn(&PhysicalASAPOperatorPayload) -> bool| {
        let found: Vec<_> = dag
            .nodes
            .iter()
            .filter(|node| matches(&node.payload))
            .collect();
        let [node] = found[..] else {
            panic!("{query}: one node expected, found {}", found.len());
        };
        node
    };
    let scan = node(|payload| {
        matches!(
            payload,
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
        )
    });
    // The runtime's `x` provider over every sample it received.
    let schema = Arc::new(scan.output_schema.clone());
    let rows = samples
        .iter()
        .map(|s| promql_rows::series_row(&schema, &s.labels, s.timestamp_ms, s.value).unwrap())
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    let store = InMemoryDataStore::default()
        .with_raw_data(
            Source::TimeSeries { metric: "x".into() },
            schema,
            vec![batch],
        )
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
    let mut answers: Vec<_> = batches
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let labels = series_labels(batch.schema(), &row);
                let value = row.iter().find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                });
                (labels, value.expect("a Float64 answer"))
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0["instance"].cmp(&b.0["instance"]));
    answers
}

/// Asserts `query` over `samples` answers `expected`, a NaN matching a NaN.
fn assert_answers(query: &str, samples: &[RawDataSample], expected: &[(&str, f64)]) {
    assert_same(query, &execute(query, samples), expected);
}

/// Asserts `answers` of `query` are `expected`, each value within a relative
/// 1e-12 (the order of compensated sums), a NaN matching a NaN.
fn assert_same(query: &str, answers: &[(String, f64)], expected: &[(&str, f64)]) {
    let close =
        |x: f64, y: f64| x == y || (x.is_nan() && y.is_nan()) || (x - y).abs() <= 1e-12 * y.abs();
    let same = answers.len() == expected.len()
        && answers
            .iter()
            .zip(expected)
            .all(|((a, x), (b, y))| a == b && close(*x, *y));
    assert!(same, "{query}: {answers:?}, expected {expected:?}");
}

// Each range function's exact per-entity plan compiles and gives, per
// series, the function of that series' window samples, without the metric
// name. Series `a` holds 1, 4, 2, 7 and `b` holds 10, 30, 20.
#[test]
fn exact_per_series_range_functions_execute() {
    let samples = [
        series("a", &[1.0, 4.0, 2.0, 7.0]),
        series("b", &[10.0, 30.0, 20.0]),
    ]
    .concat();
    for (query, a, b) in [
        ("sum_over_time(x[1m])", 14.0, 60.0),
        ("avg_over_time(x[1m])", 3.5, 20.0),
        ("min_over_time(x[1m])", 1.0, 10.0),
        ("max_over_time(x[1m])", 7.0, 30.0),
        ("count_over_time(x[1m])", 4.0, 3.0),
        // Ranks interpolate: a's median sits halfway between 2 and 4.
        ("quantile_over_time(0.5, x[1m])", 3.0, 20.0),
    ] {
        assert_answers(query, &samples, &[("a", a), ("b", b)]);
    }
}

// A stale marker ends a series and is not a sample, as in PromQL: over 3,
// stale, 5 the count is 2, the sum 8 and the lowest rank 3, not 3, NaN and
// NaN.
#[test]
fn a_stale_marker_is_not_a_sample() {
    let samples = series("a", &[3.0, STALE_MARKER, 5.0]);
    for (query, expected) in [
        ("count_over_time(x[1m])", 2.0),
        ("sum_over_time(x[1m])", 8.0),
        ("avg_over_time(x[1m])", 4.0),
        ("min_over_time(x[1m])", 3.0),
        ("max_over_time(x[1m])", 5.0),
        ("quantile_over_time(0, x[1m])", 3.0),
    ] {
        assert_answers(query, &samples, &[("a", expected)]);
    }
    // A series whose only sample is a stale marker has no sample to reduce.
    let only_stale = series("a", &[STALE_MARKER]);
    assert_answers("count_over_time(x[1m])", &only_stale, &[]);
}

// An ordinary NaN is a sample, as in PromQL: it counts, it makes the sum and
// mean NaN, min and max skip it, and a quantile ranks it lowest.
#[test]
fn a_nan_sample_is_a_sample() {
    let samples = series("a", &[1.0, f64::NAN, 3.0]);
    for (query, expected) in [
        ("count_over_time(x[1m])", 3.0),
        ("sum_over_time(x[1m])", f64::NAN),
        ("avg_over_time(x[1m])", f64::NAN),
        ("min_over_time(x[1m])", 1.0),
        ("max_over_time(x[1m])", 3.0),
        // Sorted NaN, 1, 3: rank 0.5 × 2 = 1 is the 1.
        ("quantile_over_time(0.5, x[1m])", 1.0),
    ] {
        assert_answers(query, &samples, &[("a", expected)]);
    }
}

// φ follows PromQL: 0 and 1 are the extremes, below 0 is -Inf and above 1
// is +Inf.
#[test]
fn quantile_bounds_follow_promql() {
    let samples = series("a", &[20.0, 40.0, 10.0, 30.0]);
    for (phi, expected) in [
        ("0", 10.0),
        ("1", 40.0),
        ("-1", f64::NEG_INFINITY),
        ("2", f64::INFINITY),
    ] {
        let query = format!("quantile_over_time({phi}, x[1m])");
        assert_answers(&query, &samples, &[("a", expected)]);
    }
}

// The window `(240 s, 300 s]` is left-open: a sample exactly at 240 s is
// outside, one at 300 s inside. A series with no sample inside (`c`, whose
// samples sit at 200 s and on the left edge) has no answer.
#[test]
fn window_edges_and_an_empty_window() {
    let samples = vec![
        sample("a", 240_000, 1000.0),
        sample("a", 270_000, 2.0),
        sample("a", 300_000, 4.0),
        sample("c", 200_000, 7.0),
        sample("c", 240_000, 7.0),
    ];
    assert_answers("count_over_time(x[1m])", &samples, &[("a", 2.0)]);
    assert_answers("sum_over_time(x[1m])", &samples, &[("a", 6.0)]);
    assert_answers("quantile_over_time(1, x[1m])", &samples, &[("a", 4.0)]);
}

// Label matchers select series as in PromQL: `=~` and `!~` are fully
// anchored RE2 matches, like `=` and `!=`. Series `a` holds 1, 4, `b` 10, 30
// and `c` 100.
#[test]
fn label_matchers_select_series() {
    let samples = [
        series("a", &[1.0, 4.0]),
        series("b", &[10.0, 30.0]),
        series("c", &[100.0]),
    ]
    .concat();
    for (query, expected) in [
        (r#"sum_over_time(x{instance="a"}[1m])"#, &[("a", 5.0)][..]),
        (
            r#"sum_over_time(x{instance!="a"}[1m])"#,
            &[("b", 40.0), ("c", 100.0)],
        ),
        (
            r#"sum_over_time(x{instance=~"a|b"}[1m])"#,
            &[("a", 5.0), ("b", 40.0)],
        ),
        (
            r#"sum_over_time(x{instance!~"a"}[1m])"#,
            &[("b", 40.0), ("c", 100.0)],
        ),
        // `.` matches each one-character value.
        (
            r#"sum_over_time(x{instance=~"."}[1m])"#,
            &[("a", 5.0), ("b", 40.0), ("c", 100.0)],
        ),
    ] {
        assert_answers(query, &samples, expected);
    }
}

// A pattern matches the whole value (`api` neither `api2` nor `xapi`), `.`
// also matches a newline, and a `{` that opens no repetition is a literal
// brace (`/api/{id}`), as in Prometheus.
#[test]
fn label_regexes_are_anchored_dotall_and_keep_literal_braces() {
    let samples = [
        series("api", &[1.0]),
        series("api2", &[10.0]),
        series("xapi", &[100.0]),
        series("/api/{id}", &[1000.0]),
        series("two\nlines", &[10000.0]),
    ]
    .concat();
    for (query, expected) in [
        (
            r#"sum_over_time(x{instance=~"api"}[1m])"#,
            &[("api", 1.0)][..],
        ),
        (
            r#"sum_over_time(x{instance!~"api"}[1m])"#,
            &[
                ("/api/{id}", 1000.0),
                ("api2", 10.0),
                ("two\nlines", 10000.0),
                ("xapi", 100.0),
            ],
        ),
        (
            r#"sum_over_time(x{instance=~"/api/{id}"}[1m])"#,
            &[("/api/{id}", 1000.0)],
        ),
        (
            r#"sum_over_time(x{instance=~"two.lines"}[1m])"#,
            &[("two\nlines", 10000.0)],
        ),
    ] {
        assert_answers(query, &samples, expected);
    }
}

// A label a series lacks matches as the empty text, as in PromQL: only `a`
// carries `zone`, so `zone=~""` and `zone!~".+"` select `b` and `c`.
#[test]
fn a_missing_label_matches_as_empty_text() {
    let mut samples = [
        series("a", &[1.0, 4.0]),
        series("b", &[10.0]),
        series("c", &[100.0]),
    ]
    .concat();
    for sample in samples.iter_mut().filter(|s| s.labels["instance"] == "a") {
        sample.labels.insert("zone".into(), "eu".into());
    }
    for query in [
        r#"sum_over_time(x{zone=~""}[1m])"#,
        r#"sum_over_time(x{zone!~".+"}[1m])"#,
    ] {
        assert_answers(query, &samples, &[("b", 10.0), ("c", 100.0)]);
    }
    assert_answers(
        r#"sum_over_time(x{zone=~"e."}[1m])"#,
        &samples,
        &[("a", 5.0)],
    );
}

/// Series `a` holds 1, 4, 2, 7 (a counter reset at 4 → 2), `b` 10, 30, 20
/// and `c` the single sample 5, each from 250 s, 10 s apart.
fn counter_and_gauge_samples() -> Vec<RawDataSample> {
    [
        series("a", &[1.0, 4.0, 2.0, 7.0]),
        series("b", &[10.0, 30.0, 20.0]),
        series("c", &[5.0]),
    ]
    .concat()
}

/// `(instance, value)` pairs of `a`, `b` and `c`, without a series whose
/// value is `None` (PromQL emits no sample for it).
fn answers(a: f64, b: f64, c: Option<f64>) -> Vec<(&'static str, f64)> {
    [("a", Some(a)), ("b", Some(b)), ("c", c)]
        .into_iter()
        .filter_map(|(instance, value)| value.map(|value| (instance, value)))
        .collect()
}

// Each newly wired range function's per-entity kernel gives its PromQL
// definition over the window `(0 s, 300 s]`. Rate-like functions extrapolate
// as Prometheus `extrapolatedRate`: a's increase is 6 + 4 (the reset) over
// samples spanning 30 s, extended 3 s back (the zero bound, 30 · 1 / 10) and
// 5 s forward (half the 10 s interval); b's is 20 + 30 over 20 s, extended
// 5 s each way. A single sample (c) has no rate, slope or smoothing.
#[test]
fn per_entity_range_functions_match_promql() {
    let samples = counter_and_gauge_samples();
    for (query, a, b, c) in [
        ("rate(x[5m])", 10.0 * 38.0 / 30.0 / 300.0, 0.2, None),
        ("increase(x[5m])", 10.0 * 38.0 / 30.0, 60.0, None),
        // No reset correction or zero bound: 6 over 30 s, 10 over 20 s.
        ("delta(x[5m])", 6.0 * 40.0 / 30.0, 10.0 * 30.0 / 20.0, None),
        // The last two samples: 2 → 7, and b's reset 30 → 20 reads 20.
        ("irate(x[5m])", 0.5, 2.0, None),
        ("idelta(x[5m])", 5.0, -10.0, None),
        // Least-squares slopes: a 80 / 500, b 100 / 200.
        ("deriv(x[5m])", 0.16, 0.5, None),
        // The fit at 300 s plus 60 s of slope.
        ("predict_linear(x[5m], 60)", 18.7, 70.0, None),
        ("changes(x[5m])", 3.0, 2.0, Some(0.0)),
        ("resets(x[5m])", 1.0, 1.0, Some(0.0)),
        ("ts_of_first_over_time(x[5m])", 250.0, 250.0, Some(250.0)),
        ("ts_of_last_over_time(x[5m])", 280.0, 270.0, Some(250.0)),
        ("ts_of_max_over_time(x[5m])", 280.0, 260.0, Some(250.0)),
        ("ts_of_min_over_time(x[5m])", 250.0, 250.0, Some(250.0)),
        // Medians 3 and 20; absolute deviations 2, 1, 1, 4 and 10, 10, 0.
        ("mad_over_time(x[5m])", 1.5, 10.0, Some(0.0)),
        // Population: squared deviations 21 over 4, and 200 over 3.
        (
            "stddev_over_time(x[5m])",
            5.25f64.sqrt(),
            (200.0f64 / 3.0).sqrt(),
            Some(0.0),
        ),
        ("stdvar_over_time(x[5m])", 5.25, 200.0 / 3.0, Some(0.0)),
        ("present_over_time(x[5m])", 1.0, 1.0, Some(1.0)),
        // Levels 1, 4, 4.5, 6.625 and 10, 30, 35 (Prometheus
        // `funcDoubleExponentialSmoothing` by hand).
        (
            "double_exponential_smoothing(x[5m], 0.5, 0.5)",
            6.625,
            35.0,
            None,
        ),
    ] {
        assert_answers(query, &samples, &answers(a, b, c));
    }
}

// `last_over_time` and `first_over_time` keep the metric name, as
// Prometheus does; every other range function drops it.
#[test]
fn last_and_first_over_time_keep_the_metric_name() {
    let samples = counter_and_gauge_samples();
    for (query, a, b, c) in [
        ("last_over_time(x[5m])", 7.0, 20.0, 5.0),
        ("first_over_time(x[5m])", 1.0, 10.0, 5.0),
    ] {
        let answers_with_name = run(query, &per_entity_plan(query), &samples);
        assert!(
            answers_with_name
                .iter()
                .all(|(labels, _)| labels.get("__name__").map(String::as_str) == Some("x")),
            "{query}: {answers_with_name:?}"
        );
        let values: Vec<_> = answers_with_name
            .into_iter()
            .map(|(labels, value)| (labels["instance"].clone(), value))
            .collect();
        assert_same(query, &values, &answers(a, b, Some(c)));
    }
}

// The example queries run end to end: lowered by the PromQL frontend,
// planned exactly by the planner, compiled and executed. The planner keeps
// `rate` and `irate` as an exact per-series state (Rate, and IRate's last
// two samples) and the rest as per-entity aggregates.
#[test]
fn planned_exact_range_functions_execute() {
    let samples = counter_and_gauge_samples();
    for (query, a, b, c) in [
        ("changes(x[5m])", 3.0, 2.0, Some(0.0)),
        ("deriv(x[5m])", 0.16, 0.5, None),
        ("rate(x[5m])", 10.0 * 38.0 / 30.0 / 300.0, 0.2, None),
        ("irate(x[5m])", 0.5, 2.0, None),
        (
            "stddev_over_time(x[5m])",
            5.25f64.sqrt(),
            (200.0f64 / 3.0).sqrt(),
            Some(0.0),
        ),
        ("mad_over_time(x[5m])", 1.5, 10.0, Some(0.0)),
        ("present_over_time(x[5m])", 1.0, 1.0, Some(1.0)),
    ] {
        let answers_of = without_name(query, run(query, &planned(query), &samples));
        assert_same(query, &answers_of, &answers(a, b, c));
    }
}

// `timestamp(x)` answers each series' latest sample time in seconds, from
// the instant selector's window; a series whose latest sample is a stale
// marker (c) has ended and has no answer.
#[test]
fn planned_timestamp_answers_the_latest_sample_time() {
    let samples = vec![
        sample("a", 250_000, 7.0),
        sample("a", 299_500, 8.0),
        sample("b", 300_000, 1.0),
        sample("c", 299_500, 2.0),
        sample("c", 300_000, STALE_MARKER),
    ];
    let query = "timestamp(x)";
    let answers_of = without_name(query, run(query, &planned(query), &samples));
    assert_same(query, &answers_of, &[("a", 299.5), ("b", 300.0)]);
}

// A stale marker ends a series and is not a sample for the new kernels
// either: over 1, stale, 3 a series changed once.
#[test]
fn new_range_functions_skip_stale_markers() {
    let samples = series("a", &[1.0, STALE_MARKER, 3.0]);
    assert_answers("changes(x[5m])", &samples, &[("a", 1.0)]);
    assert_answers("present_over_time(x[5m])", &samples, &[("a", 1.0)]);
}

// A per-entity distinct count over several columns counts distinct tuples
// of all of them, as the cross-group path does, so a tuple with a NULL
// component is skipped; it used to read only the first column. Rows of
// `(x, y)`: group y = NULL holds (1, NULL), (2, NULL) and no complete
// tuple; group y = "k" holds (1, "k"), (3, "k").
#[test]
fn per_entity_distinct_count_reads_every_column() {
    use asap_types::ir::schema::{DataType, Field, Schema};
    use asap_types::ir::{Operator, TimeRangeKind};
    let node = |op| OperatorNode::new_shared(Operator::NonASAP(op)).unwrap();
    let mut schema = Schema::new(vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("x", DataType::Float64, false),
        Field::plain("y", DataType::Utf8, true),
    ]);
    schema.time_index = Some(0);
    schema.closed = true;
    let root = node(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![asap_types::ir::operator::AggIntent::Cardinality {
            cols: vec![1, 2],
            accuracy: AccuracyTarget::exact(),
        }],
        output_names: vec![String::new()],
        filters: vec![],
        having: None,
        child: node(NonASAPOp::TimeRange {
            range: std::time::Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: node(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: "t".into(),
                },
                predicates: vec![],
                schema,
            }),
        }),
    });
    let row = |seconds: i64, x: f64, y: Option<&str>| {
        vec![
            Value::Timestamp(seconds * 1000),
            Value::Float64(x),
            y.map_or(Value::Null, |y| Value::Utf8(y.into())),
        ]
    };
    let rows = vec![
        row(10, 1.0, None),
        row(20, 2.0, None),
        row(30, 1.0, Some("k")),
        row(40, 3.0, Some("k")),
    ];
    let counts: BTreeMap<String, f64> =
        crate::physical_common::execute_raw_rows_at(&root, rows, EVALUATION_MS)
            .into_iter()
            .map(|row| {
                let label = row.iter().find_map(|v| match v {
                    Value::Utf8(y) => Some(y.to_string()),
                    _ => None,
                });
                let count = row.iter().find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                });
                (label.unwrap_or("NULL".into()), count.expect("a count"))
            })
            .collect();
    assert_eq!(
        counts,
        BTreeMap::from([("NULL".to_string(), 0.0), ("k".to_string(), 2.0)])
    );
}
