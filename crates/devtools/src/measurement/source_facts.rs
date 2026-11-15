//! Source facts from data (`asap_run facts`), like a database's `ANALYZE`:
//! per-source statistics over the samples, written into the workload's
//! `data_workload.sources`.
//!
//! Facts that affect only cost are written as observed. Facts a guarantee
//! relies on are printed as suggestions with a ×2 margin, and written only
//! when the caller trusts observed bounds: the next hour's data may exceed
//! an observed value, and then the guarantee is false.
//!
//! Example: four series `m{job, instance}` with samples 1 s apart give
//! `series_count: 4` and `sample_interval: 1000` (cost-only, written), and,
//! when a query has a whole-result target, a suggested
//! `distinct_values_per_label: {"job": 4}` for 2 observed job values.

use std::collections::BTreeMap;

use asap_types::stages::ValueSign;
use asap_types::workload::{DataWorkload, LabelSetDistinctValues, PlanningWorkload, StreamNorms};

use crate::measurement::samples::Sample;

/// The smallest and largest non-zero absolute value of a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObservedMagnitude {
    pub min_abs: f64,
    pub max_abs: f64,
}

/// What one source's samples show.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObservedSourceStatistics {
    /// Distinct label sets.
    pub series_count: u64,
    /// The median of the per-series median gaps; `None` when no series has
    /// two samples.
    pub sample_interval_ms: Option<u64>,
    /// Distinct values per label, a missing label counting as one more.
    pub distinct_values_per_label: BTreeMap<String, u64>,
    /// Distinct combinations of each `by (…)` set of two or more labels the
    /// workload's queries use.
    pub distinct_values_per_label_set: Vec<LabelSetDistinctValues>,
    /// Per column; a column of zeros only has none.
    pub column_magnitudes: BTreeMap<String, ObservedMagnitude>,
    /// Per column, by sign bit; `Unknown` for mixed signs.
    pub column_signs: BTreeMap<String, ValueSign>,
    /// The most distinct values one evaluation window of the workload's
    /// longest window saw, per output group.
    pub max_distinct_per_evaluation: Option<u64>,
    /// The largest norms of one group's stream over the longest window.
    pub stream_norms: Option<StreamNorms>,
}

/// Each source's statistics over `samples`, keyed by metric name.
pub fn observe_source_statistics(
    samples: &[Sample],
    workload: &PlanningWorkload,
) -> BTreeMap<String, ObservedSourceStatistics> {
    let _ = (samples, workload);
    todo!("asap_run: implement after test approval")
}

/// One guarantee fact, suggested with its margin.
#[derive(Debug, Clone, PartialEq)]
pub enum SuggestedFact {
    ColumnSign { column: String, sign: ValueSign },
    SeriesCount(u64),
    DistinctValuesPerLabel { label: String, distinct_values: u64 },
    DistinctValuesPerLabelSet(LabelSetDistinctValues),
    MaxDistinctPerEvaluation(u64),
    StreamNorms(StreamNorms),
}

/// A suggestion for one source.
#[derive(Debug, Clone, PartialEq)]
pub struct GuaranteeFactSuggestion {
    pub source: String,
    pub fact: SuggestedFact,
}

/// How `asap_run facts` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FactsOptions {
    /// `--trust-observed-bounds`: write the suggestions too.
    pub trust_observed_bounds: bool,
}

/// What `asap_run facts` produced.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFactsReport {
    pub observed: BTreeMap<String, ObservedSourceStatistics>,
    /// The workload's data workload with the facts written.
    pub data_workload: DataWorkload,
    /// The magnitudes written per source and column, with their margin;
    /// they go into `SourceFacts::column_magnitudes` once it exists.
    pub column_magnitudes: BTreeMap<String, BTreeMap<String, ObservedMagnitude>>,
    /// The guarantee facts, printed; written too when trusted.
    pub suggestions: Vec<GuaranteeFactSuggestion>,
}

/// The facts of `samples` for `workload`.
pub fn source_facts_of_samples(
    samples: &[Sample],
    workload: &PlanningWorkload,
    options: FactsOptions,
) -> Result<SourceFactsReport, String> {
    let _ = (samples, workload, options);
    todo!("asap_run: implement after test approval")
}
