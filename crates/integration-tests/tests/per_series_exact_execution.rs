//! Exact per-series range reductions over raw data samples execute natively
//! (issue #770): exact `quantile/sum/avg/min/max/count_over_time` reduces
//! each series' samples in the window with a per-entity `Aggregate`, which
//! the executor compiles and runs. The window's rows come from
//! `RawDataSampleSelector`, as a runtime feeds them.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use asap_executor::physical_planner::{compile_candidate, frontier_from_timing, promql_rows};
use asap_executor::runtime::Scope;
use asap_executor::values::Value;
use asap_executor::{series_labels_of_output_row, RawDataSample, RawDataSampleSelector};
use asap_integration_tests::fixtures::lower_promql;
use asap_types::ir::operator::Reduction;
use asap_types::ir::physical_export::{
    compile_physical_asap_dag, PhysicalASAPOperatorPayload, RawDataSampleWindow,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{
    apply_materialization_timings, MaterializationAssignment, NonASAPOp, OperatorNode, TimingMemo,
};

mod physical_common;

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

/// Run `query`'s exact per-entity plan at query time over `samples`, fed
/// through the window's `RawDataSampleSelector`: each answered series'
/// `instance` and value.
fn execute(query: &str, samples: &[RawDataSample]) -> Vec<(String, f64)> {
    let timed = apply_materialization_timings(
        &per_entity_plan(query),
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
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
    let range = node(|payload| {
        matches!(
            payload,
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::TimeRange { .. })
        )
    });
    let scan = node(|payload| {
        matches!(
            payload,
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
        )
    });
    let window = RawDataSampleWindow {
        time_range_node_id: range.id,
        width_ms: 60_000,
        end_offset_ms: 0,
        scan_node_id: scan.id,
    };
    let rows = RawDataSampleSelector::new(&dag, &window)
        .unwrap()
        .window_batch(EVALUATION_MS, samples)
        .unwrap();
    let compiled = compile_candidate(
        &dag,
        &BTreeSet::from([range.id as u64]),
        &[dag.roots[0] as u64],
        &frontier_from_timing(&dag).unwrap(),
    )
    .unwrap();
    let result = physical_common::execute(
        &compiled.query,
        BTreeMap::from([(range.id as u64, rows)]),
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
            revision: 1,
        },
    );
    let mut answers: Vec<(String, f64)> = result[0]
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let labels = series_labels_of_output_row(batch.schema(), &row).unwrap();
                assert!(!labels.contains_key("__name__"), "{labels:?}");
                let value = row.iter().find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                });
                (labels["instance"].clone(), value.expect("a Float64 answer"))
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

/// Asserts `query` over `samples` answers `expected`, a NaN matching a NaN.
fn assert_answers(query: &str, samples: &[RawDataSample], expected: &[(&str, f64)]) {
    let answers = execute(query, samples);
    let same = answers.len() == expected.len()
        && answers
            .iter()
            .zip(expected)
            .all(|((a, x), (b, y))| a == b && (x == y || (x.is_nan() && y.is_nan())));
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
        // Anchored: `a` alone does not match a longer value, `.` any text.
        (
            r#"sum_over_time(x{instance=~"."}[1m])"#,
            &[("a", 5.0), ("b", 40.0), ("c", 100.0)],
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
