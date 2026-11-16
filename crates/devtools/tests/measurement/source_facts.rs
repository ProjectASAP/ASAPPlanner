//! §2.6: source facts from hand-made data.
//!
//! Source `m`:
//!
//! | Series | Samples (t ms : value) |
//! |---|---|
//! | A `{job=a,instance=1}` | 0:1, 1000:2, 2000:−3, 4000:0 |
//! | B `{job=a,instance=2}` | 0:5, 1000:10 |
//! | C `{job=b,instance=1}` | 500:0.5, 1500:0.25, 2500:100 |
//! | D `{job=b}` (no instance) | 0:7, 2000:7 |
//!
//! Source `lat`: two series, every value in [0.001, 3], one of them −0.0.

use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use asap_devtools::measurement::samples::{read_samples_jsonl, samples_of_generated, Sample};
use asap_devtools::measurement::source_facts::{
    observe_source_statistics, source_facts_of_samples, FactsOptions, ObservedMagnitude,
    SuggestedFact,
};
use asap_planner::PlanningWorkload;
use asap_types::stages::ValueSign;
use asap_types::workload::LabelSetDistinctValues;

use super::common::{per_value, sample, whole_result, workload};

fn fixture() -> Vec<Sample> {
    let a = [("job", "a"), ("instance", "1")];
    let b = [("job", "a"), ("instance", "2")];
    let c = [("job", "b"), ("instance", "1")];
    let d = [("job", "b")];
    let mut samples = vec![
        sample("m", &a, 0, 1.0),
        sample("m", &a, 1_000, 2.0),
        sample("m", &a, 2_000, -3.0),
        sample("m", &a, 4_000, 0.0),
        sample("m", &b, 0, 5.0),
        sample("m", &b, 1_000, 10.0),
        sample("m", &c, 500, 0.5),
        sample("m", &c, 1_500, 0.25),
        sample("m", &c, 2_500, 100.0),
        sample("m", &d, 0, 7.0),
        sample("m", &d, 2_000, 7.0),
    ];
    let x = [("host", "x")];
    let y = [("host", "y")];
    samples.extend([
        sample("lat", &x, 0, 0.001),
        sample("lat", &x, 1_000, 3.0),
        sample("lat", &y, 0, -0.0),
        sample("lat", &y, 1_000, 1.5),
    ]);
    samples
}

const QUERY: &str = "sum by (job, instance) (sum_over_time(m[2s]))";
const LAT_QUERY: &str = "max_over_time(lat[2s])";

/// The fixture's workload, with per-value targets: no fact splits δ, so
/// the label counts are cost-only.
fn per_value_workload() -> PlanningWorkload {
    workload(
        &[
            (QUERY, per_value("absolute", 1.0, 0.01)),
            (LAT_QUERY, per_value("absolute", 1.0, 0.01)),
        ],
        1_000,
        &[],
    )
}

/// The same with a whole-result target: the series and label counts bound
/// G, over which δ splits, so they become guarantee facts.
fn whole_result_workload() -> PlanningWorkload {
    workload(
        &[
            (QUERY, whole_result("absolute", 1.0, 0.01)),
            (LAT_QUERY, per_value("absolute", 1.0, 0.01)),
        ],
        1_000,
        &[],
    )
}

fn observed_m() -> asap_devtools::measurement::source_facts::ObservedSourceStatistics {
    observe_source_statistics(&fixture(), &per_value_workload())
        .remove("m")
        .unwrap()
}

/// Four distinct label sets.
#[test]
fn facts_series_count() {
    assert_eq!(observed_m().series_count, 4);
}

/// Gaps A 1000, 1000, 2000; B 1000; C 1000, 1000; D 2000. Per-series
/// medians 1000, 1000, 1000, 2000, whose median is 1000 (the mean gap,
/// 1286, is wrong). A second fixture separates the two medians: P has 10
/// samples 1 s apart, Q and R 2 samples 5 s apart; the pooled median gap
/// is 1000, the median of per-series medians 5000.
// provisional answer 19
#[test]
fn facts_sample_interval_median_gap() {
    assert_eq!(observed_m().sample_interval_ms, Some(1_000));
    let mut samples: Vec<Sample> = (0..10)
        .map(|i| sample("s", &[("id", "p")], i * 1_000, 1.0))
        .collect();
    for id in ["q", "r"] {
        samples.push(sample("s", &[("id", id)], 0, 1.0));
        samples.push(sample("s", &[("id", id)], 5_000, 1.0));
    }
    let workload = workload(
        &[("max_over_time(s[10s])", per_value("absolute", 1.0, 0.01))],
        1_000,
        &[],
    );
    let observed = observe_source_statistics(&samples, &workload)
        .remove("s")
        .unwrap();
    assert_eq!(observed.sample_interval_ms, Some(5_000));
}

/// `job` takes 2 values; `instance` 3 (1, 2 and missing on D).
#[test]
fn facts_distinct_per_label() {
    let observed = observed_m();
    assert_eq!(observed.distinct_values_per_label["job"], 2);
    assert_eq!(observed.distinct_values_per_label["instance"], 3);
}

/// `by (job, instance)` sees 4 combinations (a/1, a/2, b/1, b/missing),
/// not the product 6; `lat`'s query groups by no label set of two, so it
/// gets no entry.
// provisional answer 19
#[test]
fn facts_label_set_from_workload_by() {
    let observed = observe_source_statistics(&fixture(), &per_value_workload());
    let mut sets = observed["m"].distinct_values_per_label_set.clone();
    for set in &mut sets {
        set.labels.sort();
    }
    assert_eq!(
        sets,
        vec![LabelSetDistinctValues {
            labels: vec!["instance".into(), "job".into()],
            distinct_values: 4,
        }]
    );
    assert!(observed["lat"].distinct_values_per_label_set.is_empty());
}

/// Zeros are excluded: `m.value` spans 0.25 to 100 (a min_abs of 0 would
/// be invalid).
#[test]
fn facts_column_magnitudes_exclude_zero() {
    assert_eq!(
        observed_m().column_magnitudes["value"],
        ObservedMagnitude {
            min_abs: 0.25,
            max_abs: 100.0,
        }
    );
    let zeros = vec![sample("z", &[], 0, 0.0), sample("z", &[], 1_000, -0.0)];
    let workload = workload(
        &[("max_over_time(z[2s])", per_value("absolute", 1.0, 0.01))],
        1_000,
        &[],
    );
    let observed = observe_source_statistics(&zeros, &workload)
        .remove("z")
        .unwrap();
    assert!(observed.column_magnitudes.get("value").is_none());
}

/// Signs by sign bit: `m` has −3, so no sign suggestion; `lat` holds
/// −0.0, so no `non_negative` either (a `v >= 0.0` check would accept it).
#[test]
fn facts_sign_suggestion_sign_bit() {
    let report =
        source_facts_of_samples(&fixture(), &per_value_workload(), FactsOptions::default())
            .unwrap();
    assert_eq!(
        report.observed["m"].column_signs["value"],
        ValueSign::Unknown
    );
    assert_ne!(
        report.observed["lat"].column_signs["value"],
        ValueSign::NonNegative
    );
    assert!(
        !report.suggestions.iter().any(|suggestion| matches!(
            &suggestion.fact,
            SuggestedFact::ColumnSign {
                sign: ValueSign::NonNegative,
                ..
            }
        )),
        "{:?}",
        report.suggestions
    );
}

/// With per-value targets, the cost-only facts are written as observed:
/// series count 4, interval 1000, label counts 2 and 3, the label set's 4;
/// the magnitudes with their margin (0.125 to 200).
// provisional answer 5 and provisional answer 6
#[test]
fn facts_cost_only_written_directly() {
    let report =
        source_facts_of_samples(&fixture(), &per_value_workload(), FactsOptions::default())
            .unwrap();
    let m = &report.data_workload.sources["m"];
    assert_eq!(m.series_count, Some(4));
    assert_eq!(m.sample_interval.map(|interval| interval.0), Some(1_000));
    assert_eq!(m.distinct_values_per_label["job"], 2);
    assert_eq!(m.distinct_values_per_label["instance"], 3);
    assert_eq!(m.distinct_values_per_label_set[0].distinct_values, 4);
    assert_eq!(
        report.column_magnitudes["m"]["value"],
        ObservedMagnitude {
            min_abs: 0.125,
            max_abs: 200.0,
        }
    );
}

/// With a whole-result target, the guarantee facts are suggested with a
/// ×2 margin, ⌈2×⌉ for counts: instance 6, job 4, series 8, the label set
/// 8; distinct values per 2 s window at most 2 observed, so 4; norms over
/// one group's 2 s window at most ‖f‖₁ = 2 and ‖f‖₂ = √2 observed, so 4
/// and 2√2. They are written only with `--trust-observed-bounds`.
// provisional answer 5 and provisional answer 19
#[test]
fn facts_guarantee_margin() {
    let suggested = source_facts_of_samples(
        &fixture(),
        &whole_result_workload(),
        FactsOptions::default(),
    )
    .unwrap();
    let observed = &suggested.observed["m"];
    assert_eq!(observed.max_distinct_per_evaluation, Some(2));
    let norms = observed.stream_norms.unwrap();
    assert_eq!(norms.l1, Some(2.0));
    assert!((norms.l2.unwrap() - 2f64.sqrt()).abs() < 1e-12);
    let m_facts: Vec<&SuggestedFact> = suggested
        .suggestions
        .iter()
        .filter(|suggestion| suggestion.source == "m")
        .map(|suggestion| &suggestion.fact)
        .collect();
    let has = |fact: &SuggestedFact| m_facts.contains(&fact);
    assert!(
        has(&SuggestedFact::DistinctValuesPerLabel {
            label: "instance".into(),
            distinct_values: 6
        }),
        "{m_facts:?}"
    );
    assert!(
        has(&SuggestedFact::DistinctValuesPerLabel {
            label: "job".into(),
            distinct_values: 4
        }),
        "{m_facts:?}"
    );
    assert!(has(&SuggestedFact::SeriesCount(8)), "{m_facts:?}");
    assert!(
        has(&SuggestedFact::MaxDistinctPerEvaluation(4)),
        "{m_facts:?}"
    );
    assert!(
        m_facts
            .iter()
            .any(|fact| matches!(fact, SuggestedFact::StreamNorms(norms)
            if norms.l1 == Some(4.0) && (norms.l2.unwrap() - 2.0 * 2f64.sqrt()).abs() < 1e-12)),
        "{m_facts:?}"
    );
    // Not written by default.
    let m = &suggested.data_workload.sources["m"];
    assert_eq!(m.series_count, None);
    assert!(m.distinct_values_per_label.is_empty());
    assert_eq!(m.max_distinct_per_evaluation, None);
    assert_eq!(m.stream_norms, None);
    // Written when trusted.
    let trusted = source_facts_of_samples(
        &fixture(),
        &whole_result_workload(),
        FactsOptions {
            trust_observed_bounds: true,
        },
    )
    .unwrap();
    let m = &trusted.data_workload.sources["m"];
    assert_eq!(m.series_count, Some(8));
    assert_eq!(m.distinct_values_per_label["instance"], 6);
    assert_eq!(m.max_distinct_per_evaluation, Some(4));
}

/// After the margins, the label set's count (⌈2 · 4⌉ = 8) stays at most
/// the product of its labels' counts (4 · 6 = 24), and the written
/// workload validates.
#[test]
fn facts_margined_set_stays_valid() {
    let trusted = source_facts_of_samples(
        &fixture(),
        &whole_result_workload(),
        FactsOptions {
            trust_observed_bounds: true,
        },
    )
    .unwrap();
    let m = &trusted.data_workload.sources["m"];
    for set in &m.distinct_values_per_label_set {
        let product: u64 = set
            .labels
            .iter()
            .map(|label| m.distinct_values_per_label[label])
            .product();
        assert!(set.distinct_values <= product, "{set:?}");
    }
    let mut written = whole_result_workload();
    written.data_workload = Some(trusted.data_workload.clone());
    written.validate().unwrap();
}

/// A counter 0, 5, 10, 2, 7 resets once: still non-negative; magnitudes
/// over the raw samples (zeros excluded): 2 to 10.
#[test]
fn facts_counter_reset() {
    let samples: Vec<Sample> = [0.0, 5.0, 10.0, 2.0, 7.0]
        .iter()
        .enumerate()
        .map(|(i, &value)| sample("c_total", &[], i as i64 * 1_000, value))
        .collect();
    let workload = workload(
        &[("rate(c_total[5s])", per_value("absolute", 1.0, 0.01))],
        1_000,
        &[],
    );
    let observed = observe_source_statistics(&samples, &workload)
        .remove("c_total")
        .unwrap();
    assert_eq!(observed.column_signs["value"], ValueSign::NonNegative);
    assert_eq!(
        observed.column_magnitudes["value"],
        ObservedMagnitude {
            min_abs: 2.0,
            max_abs: 10.0,
        }
    );
}

/// A series of one sample has no gap and does not panic: the interval
/// comes from the other series.
#[test]
fn facts_single_sample_series() {
    let samples = vec![
        sample("s", &[("id", "lonely")], 0, 1.0),
        sample("s", &[("id", "p")], 0, 1.0),
        sample("s", &[("id", "p")], 3_000, 1.0),
    ];
    let workload = workload(
        &[("max_over_time(s[10s])", per_value("absolute", 1.0, 0.01))],
        1_000,
        &[],
    );
    let observed = observe_source_statistics(&samples, &workload)
        .remove("s")
        .unwrap();
    assert_eq!(observed.sample_interval_ms, Some(3_000));
    let alone = vec![sample("t", &[], 0, 1.0)];
    let workload = workload_over("t");
    assert_eq!(
        observe_source_statistics(&alone, &workload)["t"].sample_interval_ms,
        None
    );
}

fn workload_over(metric: &str) -> PlanningWorkload {
    workload(
        &[(
            &format!("max_over_time({metric}[10s])"),
            per_value("absolute", 1.0, 0.01),
        )],
        1_000,
        &[],
    )
}

/// The JSONL schema: one `{labels, ts_ms, value}` object per line.
// provisional answer 18
#[test]
fn facts_reads_jsonl_samples() {
    let text = "{\"labels\": {\"__name__\": \"m\", \"job\": \"a\"}, \"ts_ms\": 1000, \"value\": 2.0}\n\
                {\"labels\": {\"__name__\": \"m\", \"job\": \"b\"}, \"ts_ms\": 2000, \"value\": -0.5}\n";
    let samples = read_samples_jsonl(text).unwrap();
    assert_eq!(
        samples,
        vec![
            sample("m", &[("job", "a")], 1_000, 2.0),
            sample("m", &[("job", "b")], 2_000, -0.5),
        ]
    );
    let error = read_samples_jsonl("{\"labels\": {}, \"ts_ms\": 1}\n").unwrap_err();
    assert_eq!(error.line, 1);
}

/// Generated data round-trips through facts: 50 series over `host` (100
/// values) × `endpoint` (8 values) every 10 s give series count 50,
/// interval 10,000, at most 50 hosts and 8 endpoints.
#[test]
fn facts_datagen_round_trip() {
    let description = TimeSeriesDescription {
        labels: vec![
            SeriesLabelSpec {
                name: "host".into(),
                cardinality: 100,
                skewness: 0.0,
            },
            SeriesLabelSpec {
                name: "endpoint".into(),
                cardinality: 8,
                skewness: 1.1,
            },
        ],
        series_num: 50,
        start_ms: 0,
        sample_interval_ms: 10_000,
        duration_ms: 600_000,
        value_distribution: DataDistribution::Uniform(UniformParameter {
            lower_bound: 0.0,
            upper_bound: 1.0,
            seed: 0,
        }),
        counter: false,
        seed: 42,
    };
    let samples = samples_of_generated("g", &description.generate().unwrap()).unwrap();
    let observed = observe_source_statistics(&samples, &workload_over("g"))
        .remove("g")
        .unwrap();
    assert_eq!(observed.series_count, 50);
    assert_eq!(observed.sample_interval_ms, Some(10_000));
    assert!(observed.distinct_values_per_label["host"] <= 50);
    assert!(observed.distinct_values_per_label["endpoint"] <= 8);
}
