//! PromQL series operators and scalar reads run end to end: each query
//! goes through the PromQL frontend and the planner's selection (exact),
//! then the executor compiles and runs the selected plan over samples
//! bound at its `Scan`s. Expected values follow Prometheus.
use crate::physical_common::run_selected_root as run;
use asap_executor::physical_planner::promql_rows;
use asap_executor::values::Value;
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::selected_dag;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::Source;
use asap_types::ir::schema::PROMQL_SERIES_IDENTITY;
use std::collections::BTreeMap;

/// A PromQL sample or answer: its series' labels and its value.
type Sample = (BTreeMap<String, String>, f64);

/// Evaluation time of the PromQL queries: 2023-11-14T22:13:20Z.
const EVALUATION_MS: i64 = 1_700_000_000_000;

/// A sample of a series, 0.5 s before [`EVALUATION_MS`].
fn sample(labels: &[(&str, &str)], value: f64) -> Sample {
    (
        labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        value,
    )
}

/// PromQL `query` lowered with series identity where it has one (as the
/// planner's facade does), planned exactly, run over `samples`: each
/// answer's labels and value, by labels.
fn promql(query: &str, samples: &[Sample]) -> Vec<Sample> {
    try_promql(query, samples).unwrap_or_else(|error| panic!("{query}: {error}"))
}

/// [`promql`], or the run's error.
fn try_promql(query: &str, samples: &[Sample]) -> Result<Vec<Sample>, String> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap_or(root);
    let root = selected_dag(root, AccuracyTarget::exact());
    let (schema, rows) = run(&root, EVALUATION_MS, |source, schema| {
        let Source::TimeSeries { metric } = source else {
            panic!("a time series")
        };
        samples
            .iter()
            .filter(|(labels, _)| labels["__name__"] == *metric)
            .map(|(labels, value)| {
                promql_rows::series_row(schema, labels, EVALUATION_MS - 500, *value).unwrap()
            })
            .collect()
    })?;
    let mut answers: Vec<_> = rows
        .iter()
        .map(|row| {
            let mut labels = BTreeMap::new();
            let mut value = None;
            for (field, cell) in schema.fields.iter().zip(row) {
                match cell {
                    Value::Utf8(text) if field.name == PROMQL_SERIES_IDENTITY => {
                        labels.extend(promql_rows::decode_series_identity(text).unwrap())
                    }
                    Value::Utf8(text) if !text.is_empty() => {
                        labels.insert(field.name.clone(), text.to_string());
                    }
                    Value::Float64(v) => value = Some(*v),
                    _ => {}
                }
            }
            (labels, value.expect("a sample value"))
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(answers)
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    sample(pairs, 0.0).0
}

// `vector(s)` is one label-less sample; `hour()` reads the
// evaluation time; `time()` is the evaluation time in seconds.
#[test]
fn vector_and_time_follow_promql() {
    assert_eq!(promql("vector(1)", &[]), [(labels(&[]), 1.0)]);
    assert_eq!(promql("hour()", &[]), [(labels(&[]), 22.0)]);
    let m = [sample(&[("__name__", "m"), ("i", "a")], 1_700_000_100.0)];
    assert_eq!(promql("m - time()", &m), [(labels(&[("i", "a")]), 100.0)]);
}

// `label_replace` writes the expanded replacement where its anchored regex
// matches the source label, and leaves other series unchanged; an empty
// result removes the label. `label_join` joins its sources, an absent one
// as empty.
#[test]
fn label_rewrites_follow_promql() {
    let m = [
        sample(&[("__name__", "m"), ("src", "abc")], 1.0),
        sample(&[("__name__", "m"), ("src", "xabc")], 2.0),
        sample(&[("__name__", "m"), ("src", "a")], 3.0),
    ];
    assert_eq!(
        promql(r#"label_replace(m, "dst", "$1-x", "src", "a(b.*)")"#, &m),
        [
            (
                labels(&[("__name__", "m"), ("dst", "bc-x"), ("src", "abc")]),
                1.0
            ),
            (labels(&[("__name__", "m"), ("src", "a")]), 3.0),
            (labels(&[("__name__", "m"), ("src", "xabc")]), 2.0),
        ]
    );
    assert_eq!(
        promql(r#"label_replace(m, "src", "", "src", "a")"#, &m),
        [
            (labels(&[("__name__", "m")]), 3.0),
            (labels(&[("__name__", "m"), ("src", "abc")]), 1.0),
            (labels(&[("__name__", "m"), ("src", "xabc")]), 2.0),
        ]
    );
    // Two series that end with one label set are an error, as in Prometheus.
    let error = try_promql(r#"label_replace(m, "src", "", "src", "a.*")"#, &m).unwrap_err();
    assert!(error.contains("same labelset"), "{error}");
    let m = [
        sample(&[("__name__", "m"), ("a", "x"), ("b", "y")], 1.0),
        sample(&[("__name__", "m"), ("b", "z")], 2.0),
    ];
    assert_eq!(
        promql(r#"label_join(m, "dst", ",", "a", "b")"#, &m),
        [
            (
                labels(&[("__name__", "m"), ("a", "x"), ("b", "y"), ("dst", "x,y")]),
                1.0
            ),
            (labels(&[("__name__", "m"), ("b", "z"), ("dst", ",z")]), 2.0),
        ]
    );
}

// `limitk(k, v)` keeps each group's first k series in label order;
// `limit_ratio(r, v)` and `limit_ratio(r - 1, v)` keep complementary
// series, the same ones Prometheus keeps (it hashes each label set).
#[test]
fn series_sampling_follows_promql() {
    let m: Vec<_> = ["d", "b", "a", "c", "e"]
        .iter()
        .map(|i| sample(&[("__name__", "m"), ("i", i)], 1.0))
        .collect();
    let kept = |query: &str| -> Vec<String> {
        promql(query, &m)
            .into_iter()
            .map(|(labels, _)| labels["i"].clone())
            .collect()
    };
    assert_eq!(kept("limitk(2, m)"), ["a", "b"]);
    assert_eq!(kept("limitk(0, m)"), Vec::<String>::new());
    let half = kept("limit_ratio(0.5, m)");
    let rest = kept("limit_ratio(-0.5, m)");
    assert_eq!(half, ["a", "e"]);
    let mut all = [half.clone(), rest.clone()].concat();
    all.sort();
    assert_eq!(all, ["a", "b", "c", "d", "e"], "{half:?} and {rest:?}");
    assert_eq!(kept("limit_ratio(1, m)").len(), 5);
}

// `v op scalar(w)` reads `w`'s one sample, NaN unless it has exactly
// one; `scalar` of `count` reads its integer count as a float.
#[test]
fn scalar_of_a_vector_binds_its_value() {
    let samples = [
        sample(&[("__name__", "m"), ("i", "a")], 1.0),
        sample(&[("__name__", "m"), ("i", "b")], 5.0),
        sample(&[("__name__", "n"), ("j", "x")], 2.0),
    ];
    assert_eq!(
        promql("m + scalar(n)", &samples),
        [(labels(&[("i", "a")]), 3.0), (labels(&[("i", "b")]), 7.0)]
    );
    // Two series of `m`: scalar(m) is NaN.
    let answers = promql("n + scalar(m)", &samples);
    assert!(answers.len() == 1 && answers[0].1.is_nan(), "{answers:?}");
    // `scalar` of an aggregate: sum(n) is 2.
    assert_eq!(
        promql("m > scalar(sum(n))", &samples),
        [(labels(&[("__name__", "m"), ("i", "b")]), 5.0)]
    );
    // count(m) is 2, max(m) 5, min(m) 1.
    assert_eq!(
        promql("m / scalar(count(m))", &samples),
        [(labels(&[("i", "a")]), 0.5), (labels(&[("i", "b")]), 2.5)]
    );
    assert_eq!(
        promql("m >= scalar(max(m))", &samples),
        [(labels(&[("__name__", "m"), ("i", "b")]), 5.0)]
    );
    assert_eq!(
        promql("m - scalar(min(m))", &samples),
        [(labels(&[("i", "a")]), 0.0), (labels(&[("i", "b")]), 4.0)]
    );
    // `vector(s)` of a scalar read: one label-less sample of n's value.
    assert_eq!(promql("vector(scalar(n))", &samples), [(labels(&[]), 2.0)]);
}

// `histogram_quantiles(v, "q", φ1, φ2)` is a `Concat` of one
// `histogram_quantile` per φ, each relabelled with its constant `q`.
// Buckets le 1: 4, le 2: 8, +Inf: 10: φ 0.5 (rank 5) interpolates to 1.25
// in the le 2 bucket; φ 0.9 (rank 9) is past every finite bucket, so the
// largest finite bound, 2.
#[test]
fn histogram_quantiles_concatenates_relabelled_quantiles() {
    let buckets = [("1", 4.0), ("2", 8.0), ("+Inf", 10.0)]
        .map(|(le, count)| sample(&[("__name__", "x_bucket"), ("le", le)], count));
    assert_eq!(
        promql(r#"histogram_quantiles(x_bucket, "q", 0.5, 0.9)"#, &buckets),
        [
            (labels(&[("q", "0.5")]), 1.25),
            (labels(&[("q", "0.9")]), 2.0)
        ]
    );
}
