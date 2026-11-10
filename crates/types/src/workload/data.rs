//! The data workload: each source's arrival, size and declared facts.

use std::collections::BTreeMap;

use crate::stages::ValueSign;
use serde::{Deserialize, Serialize};

use super::{DurationMs, WorkloadError};

/// How the queried data arrives: already stored, still arriving, or both.
/// This is independent of whether queries repeat.
///
/// JSON: `"at_rest"`, `{"ingesting": {"rows_per_second": 1000.0}}`,
/// `{"mixed": {"rows_per_second": 1000.0}}` or `"unknown"` (the default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DataArrival {
    /// Every row is already stored; nothing arrives, so no summary can be
    /// built at ingestion time.
    AtRest,
    /// Rows keep arriving while the queries run.
    Ingesting {
        /// Rows (PromQL samples) arriving per second over all sources.
        /// Plan selection prices a source's scans with its share of it, by
        /// series count when every source declares one, else with all of
        /// it, when the source does not declare both its `series_count` and
        /// its `sample_interval` ([`SourceFacts`]).
        rows_per_second: f64,
        /// How late a row may arrive relative to the newest row of its
        /// series, in milliseconds. `None`: a PromQL source is taken to
        /// arrive in order, and a SQL source gets no ingestion-time counter
        /// build.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_out_of_order_ms: Option<u64>,
    },
    /// Older rows are stored and new rows keep arriving.
    Mixed {
        /// Rows (PromQL samples) arriving per second over all sources, as
        /// for [`DataArrival::Ingesting`].
        rows_per_second: f64,
        /// As for [`DataArrival::Ingesting`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_out_of_order_ms: Option<u64>,
    },
    /// Not declared: no summary is built at ingestion time.
    #[default]
    Unknown,
}

impl DataArrival {
    /// Rows arriving per second, when the data is ingesting or mixed.
    pub fn rows_per_second(&self) -> Option<f64> {
        match self {
            Self::Ingesting {
                rows_per_second, ..
            }
            | Self::Mixed {
                rows_per_second, ..
            } => Some(*rows_per_second),
            Self::AtRest | Self::Unknown => None,
        }
    }
}

/// Queries per second, samples per second, or another rate whose unit is
/// established by the field that contains it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rate(pub f64);

/// Facts about the data being queried: how it arrives, and what is known
/// about each source. Every fact is declared: the planner has no clock, so a
/// caller drops facts that no longer hold before planning.
///
/// JSON: `{"arrival": {"ingesting": {"rows_per_second": 66666.7}},
/// "sources": {"http_requests_total": {"series_count": 1000000,
/// "metric_type": "counter", "sample_interval": 15000}}}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataWorkload {
    /// How the data arrives (default: unknown).
    pub arrival: DataArrival,
    /// Facts per source, keyed by PromQL metric name or SQL table name
    /// (default: none). A declared histogram or summary is keyed by its
    /// family name; its `metric_type`, `column_signs` and
    /// `sample_interval` apply to every series of the family
    /// ([`Self::series_facts`]). Counts, norms and bounds (`series_count`,
    /// `stream_norms`, …) differ per series name (a `foo_bucket` has one
    /// series per `le`), so they are read from the series' own entry;
    /// pricing a source's own arrival rate reads its `series_count` and
    /// `sample_interval` there together. A source without an entry has no
    /// known facts.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceFacts>,
}

/// What is known about one source (a PromQL metric or a SQL table). Every
/// fact is optional; an unknown fact is never read as zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFacts {
    /// How many series (PromQL) or groups of rows (SQL) the source holds
    /// (default: unknown). Plan selection bounds the values a query returns
    /// (`G`) by it, to split a whole-result failure probability, and prices
    /// scans and selectors with it (with `sample_interval`, the source's
    /// arrival rate). Unknown, pricing bounds a per-series count by the rows
    /// read instead. Must be positive.
    #[serde(default)]
    pub series_count: Option<u64>,
    /// Declared Prometheus type of the metric (`# TYPE` metadata). The
    /// planner never infers a type from a name such as `_total` or
    /// `_bucket`; an undeclared metric's samples may take any value.
    #[serde(default)]
    pub metric_type: Option<PrometheusMetricType>,
    /// The declared sign of each column's values, keyed by column name
    /// (default: none declared), e.g. `{"value": "non_negative"}` for a
    /// latency gauge. A `Scan`'s `column_signs` reads it; an undeclared
    /// column is `Unknown`, except a PromQL counter's value, which is
    /// non-negative without a declaration. The deployment must ensure it.
    /// Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub column_signs: BTreeMap<String, ValueSign>,
    /// How often each series of the metric (or group of rows of a SQL
    /// table) receives a sample (default: unknown). A PromQL or MetricsQL
    /// bare instant selector over the metric reads the latest sample within it; lowering such a
    /// selector without it fails with
    /// [`WorkloadError::MissingSampleInterval`]. With `series_count`, it
    /// gives the source's own arrival rate, `series_count` samples per
    /// `sample_interval`, which plan selection prices the source's scans
    /// and time ranges at.
    #[serde(default)]
    pub sample_interval: Option<DurationMs>,
    /// Bounds on the source's stream norms: the inputs Count-Min and Count
    /// Sketch additive error bounds are normalized by. Plan selection binds
    /// them to turn those bounds into absolute errors; without them that
    /// bound is unmet.
    #[serde(default)]
    pub stream_norms: Option<StreamNorms>,
    /// Upper bound on the distinct items any one distinct-count summary over
    /// the source's own rows counts in one complete evaluation, including
    /// every merged tumbling window. One bound serves every query over the source, so
    /// it must hold for the longest evaluation window any of them uses; a
    /// filter or a shorter window only lowers it. Each group's own items
    /// count separately: the bound is per output row. The deployment must
    /// enforce it; an observed distinct count is not a bound. Plan
    /// selection binds it to give classic HLL (and a distinct-count UnivMon)
    /// its failure probability per output row; without it that probability
    /// is unknown. KMV's guarantee needs no bound.
    #[serde(default)]
    pub max_distinct_per_evaluation: Option<u64>,
    /// How many distinct values each label (PromQL) or column (SQL) of the
    /// source takes, keyed by its name (default: none declared), e.g.
    /// `{"host": 1000, "job": 10}`. Plan selection prices the groups of a
    /// `by (k1, k2, ...)` at the product of its keys' counts, at most the
    /// `series_count` (series or groups of rows) of the sources it reads
    /// and the rows it reads per evaluation. No count is assumed for an
    /// undeclared key: such a grouping is priced at the smaller of those
    /// two. When every key of a
    /// grouped result declares a count, their product (at most the series
    /// read) also bounds the values one evaluation returns (`G`), which a
    /// whole-result accuracy target splits its failure probability over:
    /// each count must be an upper bound, counting a missing label (or a
    /// NULL) as one more value, and positive. Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub distinct_values_per_label: BTreeMap<String, u64>,
    /// How many distinct combinations sets of labels (or columns) take
    /// together, for labels that depend on each other (default: none
    /// declared), e.g. `[{"labels": ["service", "endpoint"],
    /// "distinct_values": 625}]` when each of 625 endpoints is in one of 25
    /// services. Plan selection prices a grouping at the smallest product
    /// of declared counts whose labels cover its keys
    /// (`SourceFacts::declared_distinct_combinations`), so
    /// `by (service, endpoint)` is 625 groups rather than 25 x 625, and
    /// `by (region, service, endpoint)` is at most 4 x 625. Nothing is
    /// inferred from the per-label counts. Each set names two or more
    /// distinct labels, and its count is a positive upper bound (a missing
    /// label or NULL counts as one more value) no larger than the product
    /// of its labels' `distinct_values_per_label` counts when all are
    /// declared. Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub distinct_values_per_label_set: Vec<LabelSetDistinctValues>,
    /// How many rows (PromQL samples) the source holds at rest, at most
    /// (default: unknown). Plan selection bounds the rows a scan reads by
    /// it: the whole source for a scan with no window above it, such as a
    /// SQL query with no time predicate, and data at rest that declares no
    /// arrival rate. Must be positive. Omitted from JSON when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_held: Option<u64>,
}

/// The distinct combinations of values a set of labels (or columns) of one
/// source takes together, an upper bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelSetDistinctValues {
    /// The labels (PromQL) or columns (SQL), two or more, in any order.
    pub labels: Vec<String>,
    /// How many distinct combinations of their values the source holds.
    pub distinct_values: u64,
}

/// The most keys declared label sets are searched over: covering them
/// searches every subset of the keys some declared set names.
const MAX_KEYS_COVERED_BY_LABEL_SETS: usize = 16;

impl SourceFacts {
    /// An upper bound on the distinct combinations of values `labels` take
    /// together in this source, from declared counts only: the smallest
    /// product of counts, of declared label sets
    /// (`distinct_values_per_label_set`) and of single labels
    /// (`distinct_values_per_label`), whose labels together cover
    /// `labels`. A set covering labels beyond `labels` still bounds them.
    /// `Some(1)` for no labels; `None` when some label is in no declared
    /// count. Only labels some declared set names are searched over (every
    /// other label contributes its own count); with more than 16 of them,
    /// only the per-label product is used.
    pub(crate) fn declared_distinct_combinations(&self, labels: &[&str]) -> Option<u64> {
        let mut keys = labels.to_vec();
        keys.sort_unstable();
        keys.dedup();
        let label_product = |keys: &[&str]| {
            keys.iter().try_fold(1u64, |product, key| {
                Some(product.saturating_mul(*self.distinct_values_per_label.get(*key)?))
            })
        };
        let (in_sets, alone): (Vec<&str>, Vec<&str>) = keys.iter().partition(|key| {
            self.distinct_values_per_label_set
                .iter()
                .any(|set| set.labels.iter().any(|label| label == *key))
        });
        if in_sets.is_empty() || in_sets.len() > MAX_KEYS_COVERED_BY_LABEL_SETS {
            return label_product(&keys);
        }
        // Each declared count over `in_sets` as (the keys it covers as a bit
        // mask, count).
        let mut counts: Vec<(usize, u64)> = in_sets
            .iter()
            .enumerate()
            .filter_map(|(bit, key)| Some((1 << bit, *self.distinct_values_per_label.get(*key)?)))
            .collect();
        counts.extend(self.distinct_values_per_label_set.iter().filter_map(|set| {
            let covered = in_sets
                .iter()
                .enumerate()
                .filter(|(_, key)| set.labels.iter().any(|label| label == *key))
                .fold(0, |mask, (bit, _)| mask | 1 << bit);
            (covered != 0).then_some((covered, set.distinct_values))
        }));
        // smallest[m]: the smallest product of counts covering the keys in m.
        let every_key = (1usize << in_sets.len()) - 1;
        let mut smallest: Vec<Option<u64>> = vec![None; every_key + 1];
        smallest[0] = Some(1);
        for keys_left in 1..=every_key {
            smallest[keys_left] = counts
                .iter()
                .filter(|(covered, _)| covered & keys_left != 0)
                .filter_map(|(covered, count)| {
                    Some(smallest[keys_left & !covered]?.saturating_mul(*count))
                })
                .min();
        }
        Some(smallest[every_key]?.saturating_mul(label_product(&alone)?))
    }
}

/// Upper bounds on the norms of the frequency (weight) vector any one
/// summary evaluation over a source folds. A filter or a shorter window only
/// lowers them, so a bound for the whole source holds for every summary
/// over it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamNorms {
    /// `‖f‖₁`, the total weight: Count-Min's bound is `ε · ‖f‖₁`.
    pub l1: Option<f64>,
    /// `‖f‖₂`: Count Sketch's bound is `ε · ‖f‖₂`.
    pub l2: Option<f64>,
}

/// The Prometheus type of a metric family (`# TYPE` metadata). Not an ASAP
/// summary: [`Self::Summary`] is the Prometheus summary type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrometheusMetricType {
    /// Starts at zero and only increases, or resets to zero: every sample is
    /// non-negative.
    Counter,
    /// May take any value.
    Gauge,
    /// A classic histogram family `foo`: cumulative bucket counters
    /// `foo_bucket{le=…}`, and the counters `foo_sum` and `foo_count`. The
    /// planner reads `foo_sum` as a counter, so it assumes the family's
    /// observations are non-negative.
    Histogram,
    /// A native histogram: its samples are histograms, which the IR cannot
    /// represent yet, so a query reading one is rejected.
    NativeHistogram,
    /// A Prometheus summary family `foo`: quantiles each client computed per
    /// series, `foo{quantile="φ"}`, and the counters `foo_sum` and
    /// `foo_count`.
    Summary,
}

impl DataWorkload {
    /// The facts of the source a PromQL selector over `series` reads, and
    /// that series' own type. A declared histogram family `foo` holds the
    /// series `foo_bucket`, `foo_sum` and `foo_count`; a declared summary
    /// family `foo` holds `foo`, `foo_sum` and `foo_count`; `_sum` and
    /// `_count` series are counters. A declared family wins over a series'
    /// own entry (`foo_sum`'s entry is ignored here when `foo` is a declared
    /// histogram). Any other name is its own source, looked up as is: a
    /// suffix never makes a type.
    pub fn series_facts(
        &self,
        series: &str,
    ) -> Option<(&SourceFacts, Option<PrometheusMetricType>)> {
        series_facts(&self.sources, series)
    }

    /// Data of unknown arrival whose `metrics` are each sampled every
    /// `sample_interval`, with no other facts: what PromQL or MetricsQL
    /// queries over them need to lower their bare instant selectors.
    pub fn sampled_every(
        metrics: impl IntoIterator<Item = String>,
        sample_interval: DurationMs,
    ) -> Self {
        let sampled = SourceFacts {
            sample_interval: Some(sample_interval),
            ..SourceFacts::default()
        };
        Self {
            arrival: DataArrival::Unknown,
            sources: metrics
                .into_iter()
                .map(|metric| (metric, sampled.clone()))
                .collect(),
        }
    }

    /// This data workload with every source it declares holding
    /// `series_count` series (or groups of rows).
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_series_count(mut self, series_count: u64) -> Self {
        for facts in self.sources.values_mut() {
            facts.series_count = Some(series_count);
        }
        self
    }

    /// Validate the arrival and source facts independently of query demand
    /// or language.
    pub fn validate(&self) -> Result<(), WorkloadError> {
        validate_optional_rate(self.arrival.rows_per_second().map(Rate))?;
        for (source_name, facts) in &self.sources {
            if facts.series_count == Some(0) {
                return Err(WorkloadError::ZeroSeriesCount);
            }
            if facts.sample_interval == Some(DurationMs(0)) {
                return Err(WorkloadError::ZeroSampleInterval);
            }
            if let Some(norms) = &facts.stream_norms {
                for value in [norms.l1, norms.l2].into_iter().flatten() {
                    if !(value.is_finite() && value >= 0.0) {
                        return Err(WorkloadError::InvalidStreamNorm(value));
                    }
                }
            }
            if facts.max_distinct_per_evaluation == Some(0) {
                return Err(WorkloadError::ZeroMaxDistinct);
            }
            if facts
                .distinct_values_per_label
                .values()
                .any(|&count| count == 0)
            {
                return Err(WorkloadError::ZeroDistinctValues);
            }
            if facts.rows_held == Some(0) {
                return Err(WorkloadError::ZeroRowsHeld);
            }
            for set in &facts.distinct_values_per_label_set {
                let error_labels = || set.labels.clone();
                let mut distinct_labels: Vec<&String> = set.labels.iter().collect();
                distinct_labels.sort_unstable();
                distinct_labels.dedup();
                if distinct_labels.len() < 2 || distinct_labels.len() != set.labels.len() {
                    return Err(WorkloadError::LabelSetNeedsTwoLabels {
                        source_name: source_name.clone(),
                        labels: error_labels(),
                    });
                }
                if set.distinct_values == 0 {
                    return Err(WorkloadError::ZeroDistinctValues);
                }
                let label_product = set.labels.iter().try_fold(1u64, |product, label| {
                    Some(product.saturating_mul(*facts.distinct_values_per_label.get(label)?))
                });
                if let Some(label_product) = label_product.filter(|&p| set.distinct_values > p) {
                    return Err(WorkloadError::LabelSetCountAboveLabelProduct {
                        source_name: source_name.clone(),
                        labels: error_labels(),
                        distinct_values: set.distinct_values,
                        label_product,
                    });
                }
            }
        }
        Ok(())
    }
}

/// [`DataWorkload::series_facts`] over `sources`, a data workload's
/// [`DataWorkload::sources`]: the facts of the source a PromQL selector over
/// `series` reads, and that series' own type.
pub(crate) fn series_facts<'a>(
    sources: &'a BTreeMap<String, SourceFacts>,
    series: &str,
) -> Option<(&'a SourceFacts, Option<PrometheusMetricType>)> {
    use PrometheusMetricType::{Counter, Histogram, Summary};
    for (suffix, series_type) in [
        ("_bucket", None),
        ("_sum", Some(Counter)),
        ("_count", Some(Counter)),
    ] {
        let Some(family) = series.strip_suffix(suffix) else {
            continue;
        };
        let Some(facts) = sources.get(family) else {
            continue;
        };
        match (facts.metric_type, series_type) {
            (Some(Histogram), None) => return Some((facts, Some(Histogram))),
            (Some(Histogram | Summary), Some(counter)) => return Some((facts, Some(counter))),
            _ => {}
        }
    }
    let facts = sources.get(series)?;
    Some((facts, facts.metric_type))
}

fn validate_optional_rate(rate: Option<Rate>) -> Result<(), WorkloadError> {
    rate.map(validate_rate).transpose().map(|_| ())
}

pub(super) fn validate_rate(rate: Rate) -> Result<Rate, WorkloadError> {
    if rate.0.is_finite() && rate.0 >= 0.0 {
        Ok(rate)
    } else {
        Err(WorkloadError::InvalidRate(rate.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::*;

    /// A stream norm is a finite, non-negative magnitude.
    #[test]
    fn stream_norms_must_be_finite_and_non_negative() {
        let with = |l1: f64| DataWorkload {
            sources: BTreeMap::from([(
                "m".to_string(),
                SourceFacts {
                    stream_norms: Some(StreamNorms {
                        l1: Some(l1),
                        l2: None,
                    }),
                    ..SourceFacts::default()
                },
            )]),
            ..DataWorkload::default()
        };
        assert!(with(10.0).validate().is_ok());
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    with(bad).validate(),
                    Err(WorkloadError::InvalidStreamNorm(_))
                ),
                "{bad}"
            );
        }
    }

    /// A distinct-count bound is positive: a summary counts at least one item.
    #[test]
    fn max_distinct_per_evaluation_must_be_positive() {
        let with = |bound: u64| DataWorkload {
            sources: BTreeMap::from([(
                "m".to_string(),
                SourceFacts {
                    max_distinct_per_evaluation: Some(bound),
                    ..SourceFacts::default()
                },
            )]),
            ..DataWorkload::default()
        };
        assert!(with(1).validate().is_ok());
        assert_eq!(with(0).validate(), Err(WorkloadError::ZeroMaxDistinct));
    }

    /// A declared `rows_held` is positive.
    #[test]
    fn rows_held_must_be_positive() {
        let with = |rows: u64| DataWorkload {
            sources: BTreeMap::from([(
                "t".to_string(),
                SourceFacts {
                    rows_held: Some(rows),
                    ..SourceFacts::default()
                },
            )]),
            ..DataWorkload::default()
        };
        assert!(with(1).validate().is_ok());
        assert_eq!(with(0).validate(), Err(WorkloadError::ZeroRowsHeld));
    }

    /// A declared distinct-value count is positive: a label with no value
    /// would make every grouping over it empty.
    #[test]
    fn distinct_values_per_label_must_be_positive() {
        let with = |count: u64| DataWorkload {
            sources: BTreeMap::from([(
                "m".to_string(),
                SourceFacts {
                    distinct_values_per_label: BTreeMap::from([
                        ("host".to_string(), 1_000),
                        ("job".to_string(), count),
                    ]),
                    ..SourceFacts::default()
                },
            )]),
            ..DataWorkload::default()
        };
        assert!(with(1).validate().is_ok());
        assert_eq!(with(0).validate(), Err(WorkloadError::ZeroDistinctValues));
    }

    /// The `http` source of the eval's RollupHeavy workload: 4 regions, 25
    /// services and 625 endpoints, each endpoint in one service, so
    /// (service, endpoint) takes 625 combinations, not 25 x 625.
    fn hierarchical_facts(declared_sets: &[(&[&str], u64)]) -> SourceFacts {
        SourceFacts {
            distinct_values_per_label: BTreeMap::from([
                ("region".to_string(), 4),
                ("service".to_string(), 25),
                ("endpoint".to_string(), 625),
            ]),
            distinct_values_per_label_set: declared_sets
                .iter()
                .map(|(labels, distinct_values)| LabelSetDistinctValues {
                    labels: labels.iter().map(|label| label.to_string()).collect(),
                    distinct_values: *distinct_values,
                })
                .collect(),
            ..SourceFacts::default()
        }
    }

    /// A grouping's combinations are bounded by the smallest product over
    /// declared sets and labels covering its keys: the declared set itself,
    /// a superset of keys through the set times the other labels, and a
    /// subset of a declared set through that set. Without a declared set,
    /// the per-label product stands.
    #[test]
    fn declared_label_sets_bound_the_combinations_of_their_labels() {
        let declared = hierarchical_facts(&[(&["service", "endpoint"], 625)]);
        let undeclared = hierarchical_facts(&[]);
        let combinations =
            |facts: &SourceFacts, labels: &[&str]| facts.declared_distinct_combinations(labels);
        assert_eq!(combinations(&declared, &["service", "endpoint"]), Some(625));
        assert_eq!(combinations(&declared, &["endpoint", "service"]), Some(625));
        assert_eq!(
            combinations(&declared, &["region", "service", "endpoint"]),
            Some(4 * 625)
        );
        assert_eq!(
            combinations(&undeclared, &["service", "endpoint"]),
            Some(25 * 625)
        );
        assert_eq!(
            combinations(&undeclared, &["region", "service", "endpoint"]),
            Some(4 * 25 * 625)
        );
        assert_eq!(combinations(&declared, &["service"]), Some(25));
        assert_eq!(combinations(&declared, &[]), Some(1));
        // A key no fact covers leaves the combinations unbound.
        assert_eq!(combinations(&declared, &["service", "status"]), None);
        // A declared set covering more labels bounds its subsets: 3 users
        // have made 7 (user, session) pairs, so at most 7 sessions, while
        // `session` alone declares no count.
        let sessions = SourceFacts {
            distinct_values_per_label_set: vec![LabelSetDistinctValues {
                labels: vec!["user".to_string(), "session".to_string()],
                distinct_values: 7,
            }],
            ..SourceFacts::default()
        };
        assert_eq!(
            sessions.declared_distinct_combinations(&["session"]),
            Some(7)
        );
        // 70 keys, two in a declared set: only those two are searched over.
        let mut wide = hierarchical_facts(&[(&["service", "endpoint"], 625)]);
        let names: Vec<String> = (0..68).map(|i| format!("l{i}")).collect();
        for name in &names {
            wide.distinct_values_per_label.insert(name.clone(), 1);
        }
        let mut labels: Vec<&str> = names.iter().map(String::as_str).collect();
        labels.extend(["service", "endpoint"]);
        assert_eq!(wide.declared_distinct_combinations(&labels), Some(625));
    }

    /// A declared set count is an input error, naming the source, when it
    /// is above the product of its labels' declared counts, or when the set
    /// has fewer than two distinct labels (one label's count belongs in
    /// `distinct_values_per_label`) or zero values.
    #[test]
    fn a_label_set_count_is_at_most_its_labels_product() {
        let with = |facts: SourceFacts| DataWorkload {
            sources: BTreeMap::from([("http".to_string(), facts)]),
            ..DataWorkload::default()
        };
        assert!(with(hierarchical_facts(&[(&["service", "endpoint"], 625)]))
            .validate()
            .is_ok());
        // `status` declares no count, so the set's product is unknown.
        assert!(with(hierarchical_facts(&[(&["service", "status"], 1_000)]))
            .validate()
            .is_ok());
        let error = with(hierarchical_facts(&[(&["region", "service"], 101)]))
            .validate()
            .unwrap_err();
        assert_eq!(
            error,
            WorkloadError::LabelSetCountAboveLabelProduct {
                source_name: "http".to_string(),
                labels: vec!["region".to_string(), "service".to_string()],
                distinct_values: 101,
                label_product: 100,
            }
        );
        assert!(error.to_string().contains("http"), "{error}");
        for labels in [&["service"][..], &["service", "service"][..]] {
            let error = with(hierarchical_facts(&[(labels, 25)])).validate();
            assert!(
                matches!(
                    &error,
                    Err(WorkloadError::LabelSetNeedsTwoLabels { source_name, .. })
                        if source_name == "http"
                ),
                "{error:?}"
            );
        }
        assert_eq!(
            with(hierarchical_facts(&[(&["service", "endpoint"], 0)])).validate(),
            Err(WorkloadError::ZeroDistinctValues)
        );
    }

    /// A declared series count is positive: with zero series, `absent(m)`
    /// would still return one row the bound does not count.
    #[test]
    fn series_count_must_be_positive() {
        let with = |series_count: u64| DataWorkload {
            sources: BTreeMap::from([(
                "m".to_string(),
                SourceFacts {
                    series_count: Some(series_count),
                    ..SourceFacts::default()
                },
            )]),
            ..DataWorkload::default()
        };
        assert!(with(1).validate().is_ok());
        assert_eq!(with(0).validate(), Err(WorkloadError::ZeroSeriesCount));
    }

    /// The arrival's rate is finite and non-negative, and each source's
    /// declared sample interval is positive.
    #[test]
    fn arrival_rate_and_sample_interval_are_validated() {
        let with = |rows_per_second: f64, sample_interval: Option<u64>| DataWorkload {
            arrival: DataArrival::Mixed {
                rows_per_second,
                max_out_of_order_ms: None,
            },
            sources: BTreeMap::from([(
                "m".to_string(),
                SourceFacts {
                    sample_interval: sample_interval.map(DurationMs),
                    ..SourceFacts::default()
                },
            )]),
        };
        assert!(with(10.0, Some(15_000)).validate().is_ok());
        assert!(with(0.0, None).validate().is_ok());
        assert_eq!(
            with(-1.0, None).validate(),
            Err(WorkloadError::InvalidRate(-1.0))
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                matches!(
                    with(bad, None).validate(),
                    Err(WorkloadError::InvalidRate(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(
            with(1.0, Some(0)).validate(),
            Err(WorkloadError::ZeroSampleInterval)
        );
    }

    /// The former flat shape (per-field evidence, one global cardinality,
    /// per-fact source maps, `continuously_ingesting`) is rejected, as are
    /// freshness evidence on a source and a sample interval on the arrival.
    #[test]
    fn the_former_data_workload_shape_is_rejected() {
        let declared = |value: serde_json::Value| {
            serde_json::json!({"value": value, "source": "declared",
                "observed_at_ms": null, "valid_for_ms": null})
        };
        for old in [
            serde_json::json!({
                "arrival": "continuously_ingesting",
                "data_ingestion_interval": declared(15000.into()),
                "ingestion_rate": declared(1000.0.into()),
                "input_cardinality": declared(10000.into()),
            }),
            serde_json::json!({"arrival": "continuously_ingesting"}),
            serde_json::json!({"arrival": "at_rest", "input_cardinality": declared(1.into())}),
            serde_json::json!({"arrival": "at_rest",
                "metric_types": {"m": "counter"}}),
            serde_json::json!({"arrival": "at_rest",
                "sources": {"m": {"series_count": 1, "evidence": {"observed_at_ms": 1}}}}),
            serde_json::json!({"arrival": "at_rest", "sources": {"m": {"series": 1}}}),
            serde_json::json!({"arrival": {"ingesting":
                {"rows_per_second": 1.0, "sample_interval": 15000}}}),
            serde_json::json!({"arrival": {"at_rest": {"rows_per_second": 1.0}}}),
        ] {
            assert!(
                serde_json::from_value::<DataWorkload>(old.clone()).is_err(),
                "{old}"
            );
        }
    }

    /// `metric_type` takes each Prometheus type by its snake_case name.
    #[test]
    fn prometheus_metric_types_have_snake_case_wire_names() {
        for (metric_type, name) in [
            (PrometheusMetricType::Counter, "counter"),
            (PrometheusMetricType::Gauge, "gauge"),
            (PrometheusMetricType::Histogram, "histogram"),
            (PrometheusMetricType::NativeHistogram, "native_histogram"),
            (PrometheusMetricType::Summary, "summary"),
        ] {
            let facts: SourceFacts =
                serde_json::from_value(serde_json::json!({ "metric_type": name })).unwrap();
            assert_eq!(facts.metric_type, Some(metric_type));
        }
    }

    /// A declared histogram family supplies its `_bucket`, `_sum` and
    /// `_count` series, a declared summary family its `_sum` and `_count`
    /// series, with `_sum` and `_count` counters; any other name, including
    /// a suffixed one whose family is undeclared or a counter, is looked up
    /// as is.
    #[test]
    fn series_facts_resolve_declared_histogram_and_summary_families() {
        use PrometheusMetricType::{Counter, Gauge, Histogram, Summary};
        let facts = |metric_type| SourceFacts {
            metric_type: Some(metric_type),
            sample_interval: Some(DurationMs(15_000)),
            ..SourceFacts::default()
        };
        let data = DataWorkload {
            arrival: DataArrival::Unknown,
            sources: BTreeMap::from([
                ("h".to_string(), facts(Histogram)),
                ("s".to_string(), facts(Summary)),
                ("c".to_string(), facts(Counter)),
                ("g_sum".to_string(), facts(Gauge)),
            ]),
        };
        let series_type = |series: &str| data.series_facts(series).map(|(_, t)| t);
        assert_eq!(series_type("h"), Some(Some(Histogram)));
        assert_eq!(series_type("h_bucket"), Some(Some(Histogram)));
        assert_eq!(series_type("h_sum"), Some(Some(Counter)));
        assert_eq!(series_type("h_count"), Some(Some(Counter)));
        assert_eq!(series_type("s"), Some(Some(Summary)));
        assert_eq!(series_type("s_sum"), Some(Some(Counter)));
        assert_eq!(series_type("s_count"), Some(Some(Counter)));
        assert_eq!(series_type("s_bucket"), None);
        assert_eq!(series_type("c_sum"), None);
        assert_eq!(series_type("g_sum"), Some(Some(Gauge)));
        assert_eq!(
            data.series_facts("h_sum").unwrap().0.sample_interval,
            Some(DurationMs(15_000))
        );
    }
}
