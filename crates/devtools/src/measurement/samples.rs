//! The samples a run ingests: read from a JSONL file, one sample per line,
//! or generated with `aqpbm-datagen` from the workload's source facts.
//!
//! Example line: `{"labels": {"__name__": "m", "job": "a"}, "ts_ms": 1000,
//! "value": 2.0}`: series `m{job="a"}` has value 2 at t = 1 s.

use std::collections::BTreeMap;

use aqpbm_datagen::{GeneratedTimeSeries, TimeSeriesDescription};
use asap_types::workload::{DataWorkload, SourceFacts};
use serde::{Deserialize, Serialize};

/// A series' labels, the metric name under `__name__`.
pub type SeriesLabels = BTreeMap<String, String>;

/// The label holding a PromQL series' metric name.
pub const METRIC_NAME_LABEL: &str = "__name__";

/// One sample of one series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub labels: SeriesLabels,
    pub ts_ms: i64,
    pub value: f64,
}

/// Why a JSONL file is not a list of samples.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleReadError {
    /// 1-based line number.
    pub line: usize,
    pub reason: String,
}

/// The samples of a JSONL text, in file order.
pub fn read_samples_jsonl(text: &str) -> Result<Vec<Sample>, SampleReadError> {
    let _ = text;
    todo!("asap_run: implement after test approval")
}

/// How `aqpbm-datagen` generates data from source facts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeneratedDataSettings {
    pub seed: u64,
    pub start_ms: i64,
    pub duration_ms: u64,
}

/// The description `aqpbm-datagen` generates source `metric` from:
/// `series_count` series, one sample per `sample_interval`, each label's
/// `distinct_values_per_label` as its cardinality.
pub fn time_series_description_of(
    metric: &str,
    facts: &SourceFacts,
    settings: &GeneratedDataSettings,
) -> Result<TimeSeriesDescription, String> {
    let _ = (metric, facts, settings);
    todo!("asap_run: implement after test approval")
}

/// The samples of every source `data` declares, generated in time order.
pub fn generate_samples(
    data: &DataWorkload,
    settings: &GeneratedDataSettings,
) -> Result<Vec<Sample>, String> {
    let _ = (data, settings);
    todo!("asap_run: implement after test approval")
}

/// The samples of `generated`, as series of metric `metric`. An error when
/// the generator yields one, e.g. a counter whose running sum passed 2^53.
pub fn samples_of_generated(
    metric: &str,
    generated: &GeneratedTimeSeries,
) -> Result<Vec<Sample>, String> {
    let _ = (metric, generated);
    todo!("asap_run: implement after test approval")
}
