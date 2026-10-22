pub mod parsed_workload;
pub mod resources;

use std::collections::BTreeMap;

use crate::ir::properties::AccuracyTarget;
use serde::{Deserialize, Serialize};

// ── Query surface ─────────────────────────────────────────────────────────────

/// A raw query string in its source language, before any parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Query(pub String);

/// How often a repeating query fires, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepetitionInterval(pub u32);

/// Milliseconds since the Unix epoch. Workload timestamps use one explicit
/// representation so schedules, observations, and time selections agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimestampMs(pub u64);

/// A non-negative duration in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DurationMs(pub u64);

/// SQL dialect variant — different dialects have different syntax and
/// function sets that affect how the query string is parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SqlDialect {
    #[serde(rename = "datafusion_sql")]
    DataFusionSQL,
    #[serde(rename = "clickhouse_sql")]
    ClickhouseSQL,
    #[serde(rename = "elastic_sql")]
    ElasticSQL,
}

/// Source language of every query in the workload.
/// All queries in a single `QueryWorkload` share the same language.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QueryLanguage {
    #[serde(rename = "promql")]
    PromQL,
    #[serde(rename = "metricsql")]
    MetricsQL,
    #[serde(rename = "sql")]
    SQL(SqlDialect),
    #[serde(rename = "datafusion")]
    DataFusion,
    #[serde(rename = "elastic_dsl")]
    ElasticDSL,
}

// ── Per-query requirements ────────────────────────────────────────────────────

/// Independent accuracy and response-latency constraints attached to one
/// query in the workload.
///
/// JSON: `{"accuracy": {"max_error": 0.01, "accuracy_metric": "rank"}, "max_latency_ms":
/// 200.0}`. An omitted `accuracy` is [`AccuracyTarget::exact`] and an
/// omitted `max_latency_ms` imposes no bound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequirements {
    /// The accuracy the query's answer must meet.
    #[serde(default = "AccuracyTarget::exact")]
    pub accuracy: AccuracyTarget,
    /// The longest one evaluation may take, in milliseconds; `None` for no
    /// bound.
    #[serde(default)]
    pub max_latency_ms: Option<f64>,
}

impl Default for QueryRequirements {
    fn default() -> Self {
        Self {
            accuracy: AccuracyTarget::exact(),
            max_latency_ms: None,
        }
    }
}

// ── Workload entries ──────────────────────────────────────────────────────────

/// How far in advance the planner knows that a query will be executed.
/// This describes knowledge of the query, not how often it runs.
///
/// # Example
///
/// A report announced at `1_000` ms and executed later is predictable;
/// an interactive query typed by a user is ad hoc.
///
/// ```
/// use asap_types::workload::{Predictability, TimestampMs};
///
/// let report = Predictability::Predictable {
///     known_at: Some(TimestampMs(1_000)),
/// };
/// let exploration = Predictability::AdHoc;
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Predictability {
    AdHoc,
    Predictable {
        known_at: Option<TimestampMs>,
    },
    #[default]
    Unknown,
}

/// Evidence-backed forecast used when recurrence is not a fixed interval or
/// an explicit schedule. Demand is normalized to invocations per second at
/// the input boundary, so this type never mixes counts and rates.
///
/// # Example
///
/// This estimate predicts ten executions per second. It is usable for five
/// minutes after `observed_at`.
///
/// ```
/// use asap_types::workload::{DemandEstimate, DurationMs, Rate, TimestampMs};
///
/// let estimate = DemandEstimate {
///     expected_rate: Rate(10.0),
///     observed_at: Some(TimestampMs(60_000)),
///     valid_for: Some(DurationMs(5 * 60 * 1_000)),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemandEstimate {
    pub expected_rate: Rate,
    pub observed_at: Option<TimestampMs>,
    pub valid_for: Option<DurationMs>,
}

/// How a repeated query is expected to recur.
///
/// # Example
///
/// ```
/// use asap_types::workload::{RepeatedDemand, RepetitionInterval, TimestampMs};
///
/// let dashboard = RepeatedDemand::FixedInterval(RepetitionInterval(10_000));
/// let scheduled = RepeatedDemand::Scheduled(vec![
///     TimestampMs(100_000),
///     TimestampMs(200_000),
/// ]);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatedDemand {
    FixedInterval(RepetitionInterval),
    /// Fixed cadence with a known evaluation phase. The timestamp is any
    /// evaluation endpoint on that cadence, not necessarily its first run.
    FixedIntervalAt {
        interval: RepetitionInterval,
        evaluation_phase: TimestampMs,
    },
    Scheduled(Vec<TimestampMs>),
    EstimatedRate(DemandEstimate),
}

/// Normalized execution recurrence shared by batch and repeating workload
/// entries. This is independent of [`Predictability`]: a one-time query may
/// be known in advance or ad hoc, and a repeated query may still be uncertain.
///
/// # Example
///
/// ```
/// use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval, TimestampMs};
///
/// let scheduled_once = QueryRecurrence::OneTime {
///     invocations: 1,
///     execute_at: Some(TimestampMs(100_000)),
/// };
/// let every_minute = QueryRecurrence::Repeated(
///     RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
/// );
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryRecurrence {
    OneTime {
        invocations: u64,
        execute_at: Option<TimestampMs>,
    },
    Repeated(RepeatedDemand),
    #[default]
    Unknown,
}

impl QueryRecurrence {
    /// The fixed interval at which the query repeats, in milliseconds:
    /// `Some` only for a fixed-interval repetition with a positive interval.
    pub fn fixed_interval_ms(&self) -> Option<u64> {
        match self {
            QueryRecurrence::Repeated(
                RepeatedDemand::FixedInterval(interval)
                | RepeatedDemand::FixedIntervalAt { interval, .. },
            ) => Some(u64::from(interval.0)).filter(|&ms| ms > 0),
            _ => None,
        }
    }
}

/// The most evaluation steps one range request may have, as Prometheus
/// allows for `/api/v1/query_range` (it rejects a request with more).
pub const MAX_RANGE_EVALUATION_STEPS: u64 = 11_000;

/// How one request of a query is answered as a range request, as
/// Prometheus' `/api/v1/query_range` does: it evaluates the query at
/// `start = end − span_ms` and every `step_ms` after, up to and including
/// the request's `end`, e.g. a dashboard panel over the last hour at
/// 1-minute steps (`...&start=now-1h&end=now&step=60`). `span_ms` must be a
/// whole number of steps, so the last step is the request's end.
/// [`QueryRecurrence`] still says how often a request runs.
///
/// JSON: `{"step_ms": 60000, "span_ms": 3600000}`.
///
/// # Example
///
/// ```
/// use asap_types::workload::RangeEvaluation;
///
/// let last_hour_by_minute = RangeEvaluation { step_ms: 60_000, span_ms: 3_600_000 };
/// assert_eq!(last_hour_by_minute.evaluations_per_request(), 61);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangeEvaluation {
    /// Time between consecutive evaluation steps of one request, in
    /// milliseconds; must be positive.
    pub step_ms: u64,
    /// How far before the request's end its first step is, in
    /// milliseconds: a multiple of `step_ms`, 0 for one step per request.
    pub span_ms: u64,
}

impl RangeEvaluation {
    /// Steps one request evaluates, `span_ms / step_ms + 1`: 61 for a 1 h
    /// span at 1 min steps. Stage 3 prices each step as one evaluation.
    pub fn evaluations_per_request(&self) -> u64 {
        self.span_ms / self.step_ms.max(1) + 1
    }
}

/// The greatest common divisor of `a` and `b` (`a` when `b` is 0): the
/// grid two cadences in milliseconds share.
pub fn greatest_common_divisor(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        greatest_common_divisor(b, a % b)
    }
}

/// One query submitted as part of a finite batch, including how many times it
/// will run and, when known, its execution time.
///
/// # Example
///
/// ```
/// use asap_types::workload::*;
///
/// let report = BatchEntry {
///     query: Query("SELECT count(*) FROM events".into()),
///     requirements: QueryRequirements::default(),
///     predictability: Predictability::Predictable {
///         known_at: Some(TimestampMs(1_000)),
///     },
///     invocations: 1,
///     execute_at: Some(TimestampMs(10_000)),
///     range_evaluation: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchEntry {
    pub query: Query,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
    pub invocations: u64,
    pub execute_at: Option<TimestampMs>,
    /// How each invocation is answered as a range request: `None` (the
    /// default) for one evaluation per invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_evaluation: Option<RangeEvaluation>,
}

/// One query with repeated demand. Its recurrence does not imply that the
/// queried data is continuously ingesting; that is described separately by
/// [`DataArrival`].
///
/// # Example
///
/// ```
/// use asap_types::workload::*;
///
/// let dashboard = RepeatingEntry {
///     query: Query("rate(requests[5m])".into()),
///     demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
///     requirements: QueryRequirements::default(),
///     predictability: Predictability::Predictable { known_at: None },
///     // Each refresh evaluates the last hour at 1-minute steps.
///     range_evaluation: Some(RangeEvaluation { step_ms: 60_000, span_ms: 3_600_000 }),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatingEntry {
    pub query: Query,
    pub demand: RepeatedDemand,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
    /// How each request is answered as a range request: `None` (the
    /// default) for one evaluation per request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_evaluation: Option<RangeEvaluation>,
}

/// Planner-facing normalized form of either [`BatchEntry`] or
/// [`RepeatingEntry`]. It keeps requirements, predictability and recurrence
/// as separate axes; the event time a query reads is in its text. Planning takes one per query, in
/// root order: Stage 2 (physical ASAP-aware optimization) reads the
/// recurrence and predictability, Stage 3 (plan selection) the accuracy
/// target, the latency bound and the recurrence.
///
/// # Example
///
/// ```
/// use asap_types::workload::*;
///
/// let entry = QueryWorkloadEntry {
///     query: Query("rate(requests[5m])".into()),
///     requirements: QueryRequirements::default(),
///     predictability: Predictability::Predictable { known_at: None },
///     recurrence: QueryRecurrence::Repeated(
///         RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
///     ),
///     range_evaluation: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryWorkloadEntry {
    /// The query text in the workload's language.
    pub query: Query,
    /// Its accuracy target and latency bound (default: exact, no bound).
    pub requirements: QueryRequirements,
    /// How far ahead it is known: a build may run at ingestion time only for
    /// `Predictable` queries (default: unknown).
    pub predictability: Predictability,
    /// How often it runs: sets how often each node is priced, and only
    /// repeated queries may keep state from ingestion time (default: unknown).
    pub recurrence: QueryRecurrence,
    /// How each run is answered as a range request, one evaluation per
    /// step; `None` (the default) for one evaluation per run. Stage 1 and
    /// Stage 2 keep pane widths on its step grid, and Stage 3 prices
    /// [`RangeEvaluation::evaluations_per_request`] evaluations per run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_evaluation: Option<RangeEvaluation>,
}

impl QueryWorkloadEntry {
    /// The grid every evaluation of this query falls on, in milliseconds,
    /// anchored at the epoch (every evaluation time is a multiple of it):
    /// its fixed interval ([`QueryRecurrence::fixed_interval_ms`]), or with
    /// a range evaluation of more than one step the greatest common divisor
    /// of the interval and the step, since each run evaluates at every step
    /// up to its end; with a declared evaluation phase
    /// ([`RepeatedDemand::FixedIntervalAt`]) also the greatest common
    /// divisor with that phase (every 20 s at phase 5 s is on a 5 s grid).
    /// `None` when it does not repeat at a positive fixed interval.
    pub fn evaluation_grid_ms(&self) -> Option<u64> {
        let interval_ms = self.recurrence.fixed_interval_ms()?;
        let step_ms = match self.range_evaluation {
            Some(range) if range.step_ms > 0 && range.evaluations_per_request() > 1 => {
                greatest_common_divisor(interval_ms, range.step_ms)
            }
            _ => interval_ms,
        };
        let phase_ms = match &self.recurrence {
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                evaluation_phase, ..
            }) => evaluation_phase.0,
            _ => 0,
        };
        Some(greatest_common_divisor(step_ms, phase_ms))
    }
}

impl From<&BatchEntry> for QueryWorkloadEntry {
    fn from(entry: &BatchEntry) -> Self {
        Self {
            query: entry.query.clone(),
            requirements: entry.requirements.clone(),
            predictability: entry.predictability.clone(),
            recurrence: QueryRecurrence::OneTime {
                invocations: entry.invocations,
                execute_at: entry.execute_at,
            },
            range_evaluation: entry.range_evaluation,
        }
    }
}

impl From<&RepeatingEntry> for QueryWorkloadEntry {
    fn from(entry: &RepeatingEntry) -> Self {
        Self {
            query: entry.query.clone(),
            requirements: entry.requirements.clone(),
            predictability: entry.predictability.clone(),
            recurrence: QueryRecurrence::Repeated(entry.demand.clone()),
            range_evaluation: entry.range_evaluation,
        }
    }
}

// ── Data workload ─────────────────────────────────────────────────────────────

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
        /// series count, when the source does not declare both its
        /// `series_count` and its `sample_interval` ([`SourceFacts`]).
        rows_per_second: f64,
    },
    /// Older rows are stored and new rows keep arriving.
    Mixed {
        /// Rows (PromQL samples) arriving per second over all sources, as
        /// for [`DataArrival::Ingesting`].
        rows_per_second: f64,
    },
    /// Not declared: no summary is built at ingestion time.
    #[default]
    Unknown,
}

impl DataArrival {
    /// Rows arriving per second, when the data is ingesting or mixed.
    pub fn rows_per_second(&self) -> Option<f64> {
        match self {
            Self::Ingesting { rows_per_second } | Self::Mixed { rows_per_second } => {
                Some(*rows_per_second)
            }
            Self::AtRest | Self::Unknown => None,
        }
    }
}

impl DemandEstimate {
    /// Whether this estimate was already observed and has not expired at
    /// `now_ms`. A validity duration without an observation time is not a
    /// usable freshness contract.
    pub fn is_fresh_at(&self, now_ms: u64) -> bool {
        match (self.observed_at, self.valid_for) {
            (Some(observed), _) if observed.0 > now_ms => false,
            (Some(observed), Some(valid_for)) => now_ms <= observed.0.saturating_add(valid_for.0),
            (None, Some(_)) => false,
            _ => true,
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
    /// (default: none). A source without an entry has no known facts.
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
    /// scans and selectors with it (1,000 when unknown). Must be positive.
    #[serde(default)]
    pub series_count: Option<u64>,
    /// Declared Prometheus type of the metric (`# TYPE` metadata). The
    /// planner never infers a type from a name such as `_total`; an
    /// undeclared metric's samples may take any value.
    #[serde(default)]
    pub metric_type: Option<MetricType>,
    /// Whether every sample of the metric is non-negative (default `false`:
    /// unknown). A counter's samples always are; this declares it for a
    /// gauge, such as a latency or a queue length. The deployment must
    /// ensure it. Plan selection reads it as the sign of the metric's
    /// samples, which a relative-error bound needs to carry through a
    /// `sum` or `avg`, and as proof that Count-Min weights over the raw
    /// samples are non-negative. Omitted from JSON when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub values_non_negative: bool,
    /// PromQL and MetricsQL only: how often each series of the metric
    /// receives a sample (default: unknown). A bare instant selector over
    /// the metric reads the latest sample within it; lowering such a
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
    /// every merged pane. One bound serves every query over the source, so
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
    /// series a PromQL grouping reads and the rows it reads. No count is
    /// assumed for an undeclared key: such a grouping is priced at the
    /// series it reads (PromQL, when declared), or else the rows it reads
    /// per evaluation. When every key of a
    /// grouped result declares a count, their product (at most the series
    /// read) also bounds the values one evaluation returns (`G`), which a
    /// whole-result accuracy target splits its failure probability over:
    /// each count must be an upper bound, counting a missing label (or a
    /// NULL) as one more value, and positive. Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub distinct_values_per_label: BTreeMap<String, u64>,
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

/// The Prometheus type of a metric's samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricType {
    /// Starts at zero and only increases, or resets to zero: every sample is
    /// non-negative.
    Counter,
    /// May take any value.
    Gauge,
}

// ── Top-level workload ────────────────────────────────────────────────────────

/// `query_batch` and `repeating_queries` may both be present. [`Self::entries`]
/// normalizes them into one ordered stream without conflating recurrence with
/// data arrival.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryWorkload {
    /// Source language shared by all queries in this workload.
    pub language: QueryLanguage,
    /// One-shot queries executed together as a batch.
    pub query_batch: Option<Vec<BatchEntry>>,
    /// Queries that repeat on a fixed interval.
    pub repeating_queries: Option<Vec<RepeatingEntry>>,
}

/// Complete planner input. Query demand and source-data evidence are parallel
/// concerns and have independent provenance and update lifecycles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningWorkload {
    pub query_workload: QueryWorkload,
    pub data_workload: Option<DataWorkload>,
}

impl QueryWorkload {
    /// One normalized entry stream, independent of the source's legacy
    /// batch/repeating containers. Mixed workloads preserve both kinds.
    pub fn entries(&self) -> impl Iterator<Item = QueryWorkloadEntry> + '_ {
        self.query_batch
            .iter()
            .flatten()
            .map(QueryWorkloadEntry::from)
            .chain(
                self.repeating_queries
                    .iter()
                    .flatten()
                    .map(QueryWorkloadEntry::from),
            )
    }

    pub fn validate(&self) -> Result<(), WorkloadError> {
        for entry in self.entries() {
            validate_entry(&entry)?;
        }
        Ok(())
    }
}

impl DataWorkload {
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

    /// Validate the arrival and source facts independently of query demand
    /// or language.
    pub fn validate(&self) -> Result<(), WorkloadError> {
        validate_optional_rate(self.arrival.rows_per_second().map(Rate))?;
        for facts in self.sources.values() {
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
        }
        Ok(())
    }
}

impl PlanningWorkload {
    /// Validate the query workload and, when present, the data workload. A
    /// PromQL or MetricsQL sample interval is checked by the frontend, only
    /// for a query with a bare instant selector
    /// ([`WorkloadError::MissingSampleInterval`]).
    pub fn validate(&self) -> Result<(), WorkloadError> {
        self.query_workload.validate()?;
        if let Some(data) = &self.data_workload {
            data.validate()?;
        }
        Ok(())
    }

    /// The sample interval the data workload declares for `metric`, if any.
    pub fn sample_interval_of(&self, metric: &str) -> Option<DurationMs> {
        self.data_workload
            .as_ref()?
            .sources
            .get(metric)?
            .sample_interval
    }
}

fn validate_entry(entry: &QueryWorkloadEntry) -> Result<(), WorkloadError> {
    if let Some(ms) = entry.requirements.max_latency_ms {
        if !ms.is_finite() || ms < 0.0 {
            return Err(WorkloadError::InvalidLatency(ms));
        }
    }
    match &entry.recurrence {
        QueryRecurrence::OneTime { invocations: 0, .. } => {
            return Err(WorkloadError::ZeroInvocations)
        }
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(0)))
        | QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(0),
            ..
        }) => return Err(WorkloadError::ZeroRepetitionInterval),
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) if schedule.is_empty() => {
            return Err(WorkloadError::EmptySchedule)
        }
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
            validate_rate(estimate.expected_rate)?;
        }
        _ => {}
    }
    if let Some(range) = entry.range_evaluation {
        if range.step_ms == 0 {
            return Err(WorkloadError::ZeroRangeEvaluationStep);
        }
        if range.span_ms % range.step_ms != 0 {
            return Err(WorkloadError::RangeEvaluationSpanNotMultipleOfStep {
                span_ms: range.span_ms,
                step_ms: range.step_ms,
            });
        }
        let steps = range.evaluations_per_request();
        if steps > MAX_RANGE_EVALUATION_STEPS {
            return Err(WorkloadError::TooManyRangeEvaluationSteps { steps });
        }
    }
    Ok(())
}

fn validate_optional_rate(rate: Option<Rate>) -> Result<(), WorkloadError> {
    rate.map(validate_rate).transpose().map(|_| ())
}

fn validate_rate(rate: Rate) -> Result<Rate, WorkloadError> {
    if rate.0.is_finite() && rate.0 >= 0.0 {
        Ok(rate)
    } else {
        Err(WorkloadError::InvalidRate(rate.0))
    }
}

/// Why a planning workload is rejected. Every variant is bad input.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum WorkloadError {
    /// Bad input: a PromQL or MetricsQL query has a bare instant selector,
    /// which reads the latest sample within its metric's sample interval,
    /// but the data workload declares no `sample_interval` for that metric.
    #[error(
        "a PromQL or MetricsQL instant selector needs its metric's sample_interval \
         in data_workload.sources"
    )]
    MissingSampleInterval,
    /// Bad input: a source declares a sample interval of zero.
    #[error("sample_interval must be greater than zero")]
    ZeroSampleInterval,
    /// Bad input: a one-time query with zero invocations.
    #[error("a one-time query must have at least one invocation")]
    ZeroInvocations,
    /// Bad input: a repeating query whose fixed interval is zero.
    #[error("a fixed repetition interval must be greater than zero")]
    ZeroRepetitionInterval,
    /// Bad input: a range evaluation whose `step_ms` is zero.
    #[error("a range evaluation's step_ms must be greater than zero")]
    ZeroRangeEvaluationStep,
    /// Bad input: a range evaluation whose `span_ms` is not a whole number
    /// of steps. Prometheus places steps forward from the start, so its
    /// last step would not be the request's end and would fall off the
    /// end's grid.
    #[error(
        "a range evaluation's span_ms ({span_ms}) must be a multiple of its step_ms ({step_ms})"
    )]
    RangeEvaluationSpanNotMultipleOfStep {
        /// The declared span.
        span_ms: u64,
        /// The declared step.
        step_ms: u64,
    },
    /// Bad input: a range evaluation with more steps per request than
    /// [`MAX_RANGE_EVALUATION_STEPS`], which Prometheus would reject.
    #[error(
        "a range evaluation has {steps} steps per request, more than {}",
        MAX_RANGE_EVALUATION_STEPS
    )]
    TooManyRangeEvaluationSteps {
        /// The steps one request would evaluate, `span_ms / step_ms + 1`.
        steps: u64,
    },
    /// Bad input: a scheduled repeating query with no timestamps.
    #[error("a repeated-query schedule must not be empty")]
    EmptySchedule,
    /// Bad input: a rate (a demand estimate's, or the arrival's
    /// `rows_per_second`) that is negative or not finite.
    #[error("rate must be finite and non-negative, got {0}")]
    InvalidRate(f64),
    /// Bad input: a query's `max_latency_ms` that is negative or not
    /// finite.
    #[error("response latency must be finite and non-negative, got {0} ms")]
    InvalidLatency(f64),
    /// Bad input: a source's stream norm bound that is negative or not
    /// finite.
    #[error("a stream norm must be finite and non-negative, got {0}")]
    InvalidStreamNorm(f64),
    /// Bad input: a source's `max_distinct_per_evaluation` of zero.
    #[error("a source's max_distinct_per_evaluation must be greater than zero")]
    ZeroMaxDistinct,
    /// Bad input: a label (or column) declared with zero distinct values in
    /// a source's `distinct_values_per_label`.
    #[error("a source's distinct_values_per_label counts must be greater than zero")]
    ZeroDistinctValues,
    /// Bad input: a source declares a `series_count` of zero. A source
    /// with no series has nothing to plan for, and `absent(m)` over it
    /// still returns one row.
    #[error("a source's series_count must be greater than zero")]
    ZeroSeriesCount,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a fixed-interval repetition with a positive interval has a
    /// fixed interval; a zero interval, a schedule, an estimated rate, a
    /// one-time query and an unknown recurrence have none.
    #[test]
    fn fixed_interval_ms_is_set_only_for_a_positive_fixed_interval() {
        let repeated = |demand| QueryRecurrence::Repeated(demand);
        assert_eq!(
            repeated(RepeatedDemand::FixedInterval(RepetitionInterval(10_000))).fixed_interval_ms(),
            Some(10_000)
        );
        assert_eq!(
            repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(60_000),
                evaluation_phase: TimestampMs(5_000),
            })
            .fixed_interval_ms(),
            Some(60_000)
        );
        assert_eq!(
            repeated(RepeatedDemand::FixedInterval(RepetitionInterval(0))).fixed_interval_ms(),
            None
        );
        assert_eq!(
            repeated(RepeatedDemand::Scheduled(vec![TimestampMs(1_000)])).fixed_interval_ms(),
            None
        );
        assert_eq!(
            QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            }
            .fixed_interval_ms(),
            None
        );
        assert_eq!(QueryRecurrence::Unknown.fixed_interval_ms(), None);
    }

    /// Query requirements: an accuracy target (omitted: exact) and an
    /// optional latency bound in ms (omitted: none). The old tagged forms
    /// and unknown fields are rejected.
    #[test]
    fn query_requirements_json_is_a_target_and_an_optional_bound() {
        let parse = |json: &str| serde_json::from_str::<QueryRequirements>(json);
        let empty = serde_json::to_value(parse("{}").unwrap()).unwrap();
        assert_eq!(
            empty,
            serde_json::json!({"accuracy": {"max_error": 0.0, "failure_probability": 0.0,
                "accuracy_scope": "whole_result"}, "max_latency_ms": null})
        );
        let bounded = serde_json::to_value(
            parse(r#"{"accuracy": {"max_error": 0.01}, "max_latency_ms": 200.0}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(bounded["accuracy"]["max_error"], 0.01);
        assert_eq!(bounded["max_latency_ms"], 200.0);
        for old in [
            r#"{"accuracy": "implicit_exact", "response_latency": "unspecified"}"#,
            r#"{"accuracy": {"explicit": {"max_error": 0.01}}}"#,
            r#"{"response_latency": {"explicit_max_ms": 100.0}}"#,
            r#"{"max_latency_ms": 100.0, "unknown": 1}"#,
        ] {
            assert!(parse(old).is_err(), "{old}");
        }
    }

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
            arrival: DataArrival::Mixed { rows_per_second },
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

    /// A repeating query with a zero fixed interval is rejected.
    #[test]
    fn zero_repetition_interval_is_rejected() {
        let mut workload = base_workload();
        workload.repeating_queries = Some(vec![RepeatingEntry {
            query: Query("dashboard".into()),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(0)),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Predictable { known_at: None },
            range_evaluation: None,
        }]);
        assert_eq!(
            workload.validate(),
            Err(WorkloadError::ZeroRepetitionInterval)
        );
    }

    /// A range evaluation is optional JSON on a repeating entry, carried to
    /// its normalized entry; its step must be positive and a request may
    /// have at most `MAX_RANGE_EVALUATION_STEPS` steps. An entry without
    /// one serializes as before, with no `range_evaluation` key.
    #[test]
    fn range_evaluation_is_optional_carried_and_validated() {
        let dashboard = |range: serde_json::Value| {
            let mut entry = serde_json::json!({
                "query": "rate(requests_total[5m])",
                "demand": {"fixed_interval": 30000},
                "requirements": {},
                "predictability": {"predictable": {"known_at": null}},
            });
            if !range.is_null() {
                entry["range_evaluation"] = range;
            }
            serde_json::from_value::<RepeatingEntry>(entry).unwrap()
        };
        let plain = dashboard(serde_json::Value::Null);
        assert_eq!(plain.range_evaluation, None);
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("range_evaluation")
            .is_none());

        let hour = dashboard(serde_json::json!({"step_ms": 60000, "span_ms": 3600000}));
        let range = RangeEvaluation {
            step_ms: 60_000,
            span_ms: 3_600_000,
        };
        assert_eq!(hour.range_evaluation, Some(range));
        assert_eq!(range.evaluations_per_request(), 61);
        let entry = QueryWorkloadEntry::from(&hour);
        assert_eq!(entry.range_evaluation, Some(range));
        // Refreshes every 30 s and steps every 60 s: evaluations on a 30 s grid.
        assert_eq!(entry.evaluation_grid_ms(), Some(30_000));
        assert_eq!(
            QueryWorkloadEntry::from(&plain).evaluation_grid_ms(),
            Some(30_000)
        );
        // At a declared phase, also on that phase from the epoch: every
        // minute at phase 125 s is on a 5 s grid; at phase 120 s, on 60 s.
        let at_phase = |phase_ms| QueryWorkloadEntry {
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(60_000),
                evaluation_phase: TimestampMs(phase_ms),
            }),
            range_evaluation: None,
            ..entry.clone()
        };
        assert_eq!(at_phase(125_000).evaluation_grid_ms(), Some(5_000));
        assert_eq!(at_phase(120_000).evaluation_grid_ms(), Some(60_000));
        // A one-step range (span 0) is one evaluation per request: its
        // step does not narrow the grid (refreshing every 60 s at a 45 s
        // step would otherwise give a 15 s grid for nothing).
        let one_step = QueryWorkloadEntry {
            range_evaluation: Some(RangeEvaluation {
                step_ms: 45_000,
                span_ms: 0,
            }),
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            ..entry.clone()
        };
        assert_eq!(one_step.evaluation_grid_ms(), Some(60_000));

        let validate = |range: RangeEvaluation| {
            let mut workload = base_workload();
            workload.repeating_queries = Some(vec![RepeatingEntry {
                range_evaluation: Some(range),
                ..hour.clone()
            }]);
            workload.validate()
        };
        assert_eq!(validate(range), Ok(()));
        assert_eq!(
            validate(RangeEvaluation {
                step_ms: 0,
                span_ms: 60_000
            }),
            Err(WorkloadError::ZeroRangeEvaluationStep)
        );
        assert_eq!(
            validate(RangeEvaluation {
                step_ms: 1_000,
                span_ms: 11_000_000
            }),
            Err(WorkloadError::TooManyRangeEvaluationSteps { steps: 11_001 })
        );
        assert_eq!(
            validate(RangeEvaluation {
                step_ms: 1_000,
                span_ms: 10_999_000
            }),
            Ok(())
        );
        // Prometheus places steps forward from the start, so a span that is
        // not a whole number of steps would put the last step off the end's
        // grid: refreshing every 60 s at 60 s steps over 90 s evaluates at
        // E − 90 s and E − 30 s. It is rejected.
        assert_eq!(
            validate(RangeEvaluation {
                step_ms: 60_000,
                span_ms: 90_000
            }),
            Err(WorkloadError::RangeEvaluationSpanNotMultipleOfStep {
                span_ms: 90_000,
                step_ms: 60_000
            })
        );
        let unknown = serde_json::json!({"step_ms": 1, "span_ms": 0, "start": 0});
        assert!(serde_json::from_value::<RangeEvaluation>(unknown).is_err());
    }

    fn base_workload() -> QueryWorkload {
        QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: None,
        }
    }

    #[test]
    fn entries_keep_each_entrys_recurrence() {
        let mut workload = base_workload();
        workload.query_batch = Some(vec![BatchEntry {
            query: Query("historical".into()),
            requirements: QueryRequirements::default(),
            predictability: Predictability::AdHoc,
            invocations: 1,
            execute_at: None,
            range_evaluation: None,
        }]);
        workload.repeating_queries = Some(vec![RepeatingEntry {
            query: Query("dashboard".into()),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Predictable { known_at: None },
            range_evaluation: None,
        }]);

        let entries: Vec<_> = workload.entries().collect();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            entries[0].recurrence,
            QueryRecurrence::OneTime { .. }
        ));
        assert!(matches!(
            entries[1].recurrence,
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(_))
        ));
        workload.validate().unwrap();
    }

    #[test]
    fn future_demand_estimates_are_not_fresh() {
        let estimate = DemandEstimate {
            expected_rate: Rate(1.0),
            observed_at: Some(TimestampMs(2_000)),
            valid_for: Some(DurationMs(1_000)),
        };
        assert!(!estimate.is_fresh_at(1_999));
        assert!(estimate.is_fresh_at(2_000));
    }

    #[test]
    fn canonical_workloads_have_a_strict_json_round_trip() {
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: None,
                repeating_queries: Some(vec![RepeatingEntry {
                    query: Query("rate(requests_total[5m])".into()),
                    demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
                    requirements: QueryRequirements {
                        accuracy: AccuracyTarget::max_error(0.01).failure_probability(0.001),
                        max_latency_ms: Some(100.0),
                    },
                    predictability: Predictability::Predictable { known_at: None },
                    range_evaluation: None,
                }]),
            },
            data_workload: Some(DataWorkload {
                arrival: DataArrival::Ingesting {
                    rows_per_second: 1_000.0,
                },
                sources: BTreeMap::from([(
                    "requests_total".to_string(),
                    SourceFacts {
                        series_count: Some(10_000),
                        metric_type: Some(MetricType::Counter),
                        values_non_negative: true,
                        sample_interval: Some(DurationMs(15_000)),
                        stream_norms: Some(StreamNorms {
                            l1: Some(1e9),
                            l2: None,
                        }),
                        max_distinct_per_evaluation: Some(100),
                        distinct_values_per_label: BTreeMap::from([("job".to_string(), 10)]),
                    },
                )]),
            }),
        };

        let json = serde_json::to_value(&workload).expect("serialize workload");
        assert_eq!(
            json["data_workload"],
            serde_json::json!({
                "arrival": {"ingesting": {"rows_per_second": 1000.0}},
                "sources": {"requests_total": {
                    "series_count": 10000,
                    "metric_type": "counter",
                    "values_non_negative": true,
                    "sample_interval": 15000,
                    "stream_norms": {"l1": 1e9, "l2": null},
                    "max_distinct_per_evaluation": 100,
                    "distinct_values_per_label": {"job": 10}
                }}
            })
        );
        let decoded: PlanningWorkload = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(decoded, workload);

        let text = serde_json::to_string(&workload).unwrap();
        let with_unknown = text.replacen("{", "{\"unknown\":true,", 1);
        assert!(serde_json::from_str::<PlanningWorkload>(&with_unknown).is_err());

        // Omitted facts default to unknown.
        let minimal: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival": {"mixed": {"rows_per_second": 5.0}},
            "sources": {"flows": {"series_count": 7}}
        }))
        .unwrap();
        assert_eq!(
            minimal.sources["flows"],
            SourceFacts {
                series_count: Some(7),
                ..SourceFacts::default()
            }
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

    #[test]
    fn query_language_wire_names_do_not_split_acronyms() {
        let cases = [
            (QueryLanguage::PromQL, serde_json::json!("promql")),
            (QueryLanguage::MetricsQL, serde_json::json!("metricsql")),
            (
                QueryLanguage::SQL(SqlDialect::DataFusionSQL),
                serde_json::json!({"sql": "datafusion_sql"}),
            ),
            (
                QueryLanguage::SQL(SqlDialect::ClickhouseSQL),
                serde_json::json!({"sql": "clickhouse_sql"}),
            ),
            (QueryLanguage::ElasticDSL, serde_json::json!("elastic_dsl")),
        ];
        for (language, expected) in cases {
            assert_eq!(serde_json::to_value(language).unwrap(), expected);
        }
    }

    #[test]
    fn struct_enum_variants_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<QueryRecurrence>(serde_json::json!({
                "one_time": {"invocations": 1, "execute_at": null, "typo": true}
            }))
            .is_err()
        );
        assert!(serde_json::from_value::<Predictability>(serde_json::json!({
            "predictable": {"known_at": null, "typo": true}
        }))
        .is_err());
        assert!(serde_json::from_value::<AccuracyTarget>(serde_json::json!({
            "max_error": 0.1, "failure_probability": 0.01, "typo": true
        }))
        .is_err());
    }

    #[test]
    fn estimated_demand_validates_its_rate() {
        let mut workload = base_workload();
        workload.repeating_queries = Some(vec![RepeatingEntry {
            query: Query("estimated".into()),
            demand: RepeatedDemand::EstimatedRate(DemandEstimate {
                expected_rate: Rate(-1.0),
                observed_at: None,
                valid_for: None,
            }),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Unknown,
            range_evaluation: None,
        }]);
        assert_eq!(workload.validate(), Err(WorkloadError::InvalidRate(-1.0)));
    }
}
