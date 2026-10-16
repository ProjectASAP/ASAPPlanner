use std::collections::BTreeMap;

use crate::types::AccuracyTarget;
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
/// JSON: `{"accuracy": {"error": 0.01, "accuracy_metric": "rank"}, "max_latency_ms":
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

/// Confidence assigned to an estimated demand value, expressed in `[0, 1]`.
/// Validation rejects values outside that range.
///
/// For example, `Confidence(0.95)` says the demand estimate is supplied with
/// 95% confidence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Confidence(pub f64);

/// Event-time interval from which a demand estimate was learned.
///
/// For example, `{ start: TimestampMs(0), end: TimestampMs(60_000) }`
/// describes an estimate based on the first minute of observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationWindow {
    pub start: TimestampMs,
    pub end: TimestampMs,
}

/// Evidence-backed forecast used when recurrence is not a fixed interval or
/// an explicit schedule. Demand is normalized to invocations per second at
/// the input boundary, so this type never mixes counts and rates.
///
/// # Example
///
/// This estimate predicts ten executions per second, with a possible peak of
/// twenty, based on one minute of observed data. It is usable for five minutes
/// after `observed_at`.
///
/// ```
/// use asap_types::workload::{
///     Confidence, DemandEstimate, DurationMs, EvidenceSource, ObservationWindow,
///     Rate, TimestampMs,
/// };
///
/// let estimate = DemandEstimate {
///     observation_window: ObservationWindow {
///         start: TimestampMs(0),
///         end: TimestampMs(60_000),
///     },
///     expected_rate: Rate(10.0),
///     peak_rate: Some(Rate(20.0)),
///     max_concurrency: Some(4),
///     confidence: Confidence(0.95),
///     source: EvidenceSource::Observed,
///     observed_at: Some(TimestampMs(60_000)),
///     valid_for: Some(DurationMs(5 * 60 * 1_000)),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemandEstimate {
    pub observation_window: ObservationWindow,
    pub expected_rate: Rate,
    pub peak_rate: Option<Rate>,
    pub max_concurrency: Option<u64>,
    pub confidence: Confidence,
    pub source: EvidenceSource,
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
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatingEntry {
    pub query: Query,
    pub demand: RepeatedDemand,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
}

/// Planner-facing normalized form of either [`BatchEntry`] or
/// [`RepeatingEntry`]. It keeps requirements, predictability and recurrence
/// as separate axes; the event time a query reads is in its text. Planning takes one per query, in
/// root order: Stage 2 (physical ASAP-aware optimization) reads the
/// recurrence and predictability, Stage 3 (plan selection) the accuracy
/// target, the latency bound and the recurrence. The default is an empty
/// query, exact, with no latency bound and unknown predictability and
/// recurrence.
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
/// };
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
        }
    }
}

// ── Data workload ─────────────────────────────────────────────────────────────

/// How the queried data arrives: already stored, still arriving, or both,
/// with the facts about its arrival. This is independent of whether queries
/// repeat.
///
/// JSON: `"at_rest"`, `{"ingesting": {"rows_per_second": 1000.0,
/// "sample_interval": 15000}}`, `{"mixed": {...}}` or `"unknown"` (the
/// default). `sample_interval` may be omitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DataArrival {
    /// Every row is already stored; nothing arrives, so no summary can be
    /// built at ingestion time.
    AtRest,
    /// Rows keep arriving while the queries run.
    Ingesting {
        /// Rows (PromQL samples) arriving per second over all sources.
        /// Plan selection prices scans and ingestion-time work with it.
        rows_per_second: f64,
        /// PromQL and MetricsQL only: how often each series receives a
        /// sample. A bare instant selector reads the latest sample within
        /// it; `None` when no query needs it.
        #[serde(default)]
        sample_interval: Option<DurationMs>,
    },
    /// Older rows are stored and new rows keep arriving; the fields are
    /// those of [`DataArrival::Ingesting`].
    Mixed {
        /// As for [`DataArrival::Ingesting`].
        rows_per_second: f64,
        /// As for [`DataArrival::Ingesting`].
        #[serde(default)]
        sample_interval: Option<DurationMs>,
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

    /// The PromQL sample interval, when the data is ingesting or mixed and
    /// declares one.
    pub fn sample_interval(&self) -> Option<DurationMs> {
        match self {
            Self::Ingesting {
                sample_interval, ..
            }
            | Self::Mixed {
                sample_interval, ..
            } => *sample_interval,
            Self::AtRest | Self::Unknown => None,
        }
    }
}

/// Where an empirical workload value came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Declared,
    Observed,
    Derived,
    #[default]
    Unknown,
}

/// When one source's facts were observed and how long they stay valid, in
/// milliseconds. The default (neither set) never expires: a declared fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// When the facts were observed (Unix ms); `None` for declared facts.
    #[serde(default)]
    pub observed_at_ms: Option<u64>,
    /// How long after `observed_at_ms` they stay valid; `None` for no
    /// expiry.
    #[serde(default)]
    pub valid_for_ms: Option<u64>,
}

impl Evidence {
    /// Whether facts with this evidence may be used at `now_ms`: observed
    /// no later than `now_ms` and not expired (expiry is inclusive). A
    /// validity duration without an observation time is never usable.
    pub fn is_fresh_at(&self, now_ms: u64) -> bool {
        match (self.observed_at_ms, self.valid_for_ms) {
            (Some(observed), _) if observed > now_ms => false,
            (Some(observed), Some(valid_for)) => now_ms <= observed.saturating_add(valid_for),
            (None, Some(_)) => false,
            _ => true,
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
/// about each source.
///
/// JSON: `{"arrival": {"ingesting": {"rows_per_second": 66666.7,
/// "sample_interval": 15000}}, "sources": {"http_requests_total":
/// {"series": 1000000, "metric_type": "counter"}}}`.
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
    /// How many series (PromQL) or groups of rows (SQL) the source holds.
    /// Plan selection splits a whole-result failure probability over the
    /// values a query returns (`G`), at most the series of the sources it
    /// reads, and prices scans and selectors with it (default 1,000 when
    /// unknown).
    #[serde(default)]
    pub series: Option<u64>,
    /// Declared Prometheus type of the metric (`# TYPE` metadata). The
    /// planner never infers a type from a name such as `_total`; an
    /// undeclared metric's samples may take any value.
    #[serde(default)]
    pub metric_type: Option<MetricType>,
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
    /// When `series`, `stream_norms` and `max_distinct_per_evaluation` were
    /// observed and how long they hold (default: declared, never expires).
    /// Plan selection binds them into accuracy guarantees only while they
    /// hold at any planning time. `metric_type` is a declaration and does
    /// not expire.
    #[serde(default)]
    pub evidence: Evidence,
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
    /// Validate the arrival and source facts independently of query demand
    /// or language.
    pub fn validate(&self) -> Result<(), WorkloadError> {
        validate_optional_rate(self.arrival.rows_per_second().map(Rate))?;
        if self.arrival.sample_interval() == Some(DurationMs(0)) {
            return Err(WorkloadError::ZeroSampleInterval);
        }
        for facts in self.sources.values() {
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

    /// The PromQL sample interval the data workload declares, if any.
    pub fn sample_interval(&self) -> Option<DurationMs> {
        self.data_workload
            .as_ref()
            .and_then(|data| data.arrival.sample_interval())
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
            if estimate.observation_window.start >= estimate.observation_window.end {
                return Err(WorkloadError::EmptyObservationWindow);
            }
            if !(estimate.confidence.0.is_finite() && (0.0..=1.0).contains(&estimate.confidence.0))
            {
                return Err(WorkloadError::InvalidConfidence(estimate.confidence.0));
            }
            validate_rate(estimate.expected_rate)?;
            validate_optional_rate(estimate.peak_rate)?;
        }
        _ => {}
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

#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum WorkloadError {
    /// Bad input: a PromQL or MetricsQL query has a bare instant selector,
    /// which reads the latest sample within the sample interval, but the
    /// data workload declares none (data at rest, unknown arrival, or
    /// ingesting without `sample_interval`).
    #[error(
        "a PromQL or MetricsQL instant selector needs data_workload.arrival to be \
         ingesting or mixed with a sample_interval"
    )]
    MissingSampleInterval,
    /// Bad input: a declared sample interval of zero.
    #[error("sample_interval must be greater than zero")]
    ZeroSampleInterval,
    #[error("a one-time query must have at least one invocation")]
    ZeroInvocations,
    #[error("a fixed repetition interval must be greater than zero")]
    ZeroRepetitionInterval,
    #[error("a repeated-query schedule must not be empty")]
    EmptySchedule,
    #[error("a demand-estimate observation window must have start < end")]
    EmptyObservationWindow,
    #[error("confidence must be finite and in [0, 1], got {0}")]
    InvalidConfidence(f64),
    #[error("rate must be finite and non-negative, got {0}")]
    InvalidRate(f64),
    #[error("response latency must be finite and non-negative, got {0} ms")]
    InvalidLatency(f64),
    #[error("a stream norm must be finite and non-negative, got {0}")]
    InvalidStreamNorm(f64),
    #[error("a source's max_distinct_per_evaluation must be greater than zero")]
    ZeroMaxDistinct,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Query requirements: an accuracy target (omitted: exact) and an
    /// optional latency bound in ms (omitted: none). The old tagged forms
    /// and unknown fields are rejected.
    #[test]
    fn query_requirements_json_is_a_target_and_an_optional_bound() {
        let parse = |json: &str| serde_json::from_str::<QueryRequirements>(json);
        let empty = serde_json::to_value(parse("{}").unwrap()).unwrap();
        assert_eq!(
            empty,
            serde_json::json!({"accuracy": {"error": 0.0, "failure_probability": 0.0,
                "accuracy_scope": "whole_result"}, "max_latency_ms": null})
        );
        let bounded = serde_json::to_value(
            parse(r#"{"accuracy": {"error": 0.01}, "max_latency_ms": 200.0}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(bounded["accuracy"]["error"], 0.01);
        assert_eq!(bounded["max_latency_ms"], 200.0);
        for old in [
            r#"{"accuracy": "implicit_exact", "response_latency": "unspecified"}"#,
            r#"{"accuracy": {"explicit": {"error": 0.01}}}"#,
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

    /// The arrival's rate is finite and non-negative, and a declared sample
    /// interval is positive.
    #[test]
    fn arrival_rate_and_sample_interval_are_validated() {
        let with = |rows_per_second: f64, sample_interval: Option<u64>| DataWorkload {
            arrival: DataArrival::Mixed {
                rows_per_second,
                sample_interval: sample_interval.map(DurationMs),
            },
            ..DataWorkload::default()
        };
        assert!(with(10.0, Some(15_000)).validate().is_ok());
        assert!(with(0.0, None).validate().is_ok());
        assert_eq!(
            with(-1.0, None).validate(),
            Err(WorkloadError::InvalidRate(-1.0))
        );
        assert_eq!(
            with(1.0, Some(0)).validate(),
            Err(WorkloadError::ZeroSampleInterval)
        );
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
        }]);
        workload.repeating_queries = Some(vec![RepeatingEntry {
            query: Query("dashboard".into()),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Predictable { known_at: None },
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
    fn stale_evidence_is_not_fresh_at_planning_time() {
        let evidence = Evidence {
            observed_at_ms: Some(1_000),
            valid_for_ms: Some(500),
        };
        assert!(evidence.is_fresh_at(1_500));
        assert!(!evidence.is_fresh_at(1_501));
        // Declared facts never expire; a validity without an observation
        // time is never usable.
        assert!(Evidence::default().is_fresh_at(u64::MAX));
        let unanchored = Evidence {
            observed_at_ms: None,
            valid_for_ms: Some(1),
        };
        assert!(!unanchored.is_fresh_at(0));
    }

    #[test]
    fn future_evidence_and_demand_estimates_are_not_fresh() {
        let evidence = Evidence {
            observed_at_ms: Some(2_000),
            valid_for_ms: Some(1_000),
        };
        assert!(!evidence.is_fresh_at(1_999));
        assert!(evidence.is_fresh_at(2_000));

        let estimate = DemandEstimate {
            observation_window: ObservationWindow {
                start: TimestampMs(0),
                end: TimestampMs(1_000),
            },
            expected_rate: Rate(1.0),
            peak_rate: None,
            max_concurrency: None,
            confidence: Confidence(1.0),
            source: EvidenceSource::Observed,
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
                }]),
            },
            data_workload: Some(DataWorkload {
                arrival: DataArrival::Ingesting {
                    rows_per_second: 1_000.0,
                    sample_interval: Some(DurationMs(15_000)),
                },
                sources: BTreeMap::from([(
                    "requests_total".to_string(),
                    SourceFacts {
                        series: Some(10_000),
                        metric_type: Some(MetricType::Counter),
                        stream_norms: Some(StreamNorms {
                            l1: Some(1e9),
                            l2: None,
                        }),
                        max_distinct_per_evaluation: Some(100),
                        evidence: Evidence {
                            observed_at_ms: Some(1_000),
                            valid_for_ms: Some(60_000),
                        },
                    },
                )]),
            }),
        };

        let json = serde_json::to_value(&workload).expect("serialize workload");
        assert_eq!(
            json["data_workload"],
            serde_json::json!({
                "arrival": {"ingesting": {"rows_per_second": 1000.0, "sample_interval": 15000}},
                "sources": {"requests_total": {
                    "series": 10000,
                    "metric_type": "counter",
                    "stream_norms": {"l1": 1e9, "l2": null},
                    "max_distinct_per_evaluation": 100,
                    "evidence": {"observed_at_ms": 1000, "valid_for_ms": 60000}
                }}
            })
        );
        let decoded: PlanningWorkload = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(decoded, workload);

        let text = serde_json::to_string(&workload).unwrap();
        let with_unknown = text.replacen("{", "{\"unknown\":true,", 1);
        assert!(serde_json::from_str::<PlanningWorkload>(&with_unknown).is_err());

        // Omitted facts default to unknown; the sample interval is optional.
        let minimal: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival": {"mixed": {"rows_per_second": 5.0}},
            "sources": {"flows": {"series": 7}}
        }))
        .unwrap();
        assert_eq!(minimal.arrival.sample_interval(), None);
        assert_eq!(minimal.sources["flows"].evidence, Evidence::default());
    }

    /// The former flat shape (per-field evidence, one global cardinality,
    /// per-fact source maps, `continuously_ingesting`) is rejected.
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
                "sources": {"m": {"series": 1, "evidence": {"source": "declared"}}}}),
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
            "error": 0.1, "failure_probability": 0.01, "typo": true
        }))
        .is_err());
    }

    #[test]
    fn estimated_demand_validates_window_rate_and_confidence() {
        let mut workload = base_workload();
        workload.repeating_queries = Some(vec![RepeatingEntry {
            query: Query("estimated".into()),
            demand: RepeatedDemand::EstimatedRate(DemandEstimate {
                observation_window: ObservationWindow {
                    start: TimestampMs(10),
                    end: TimestampMs(10),
                },
                expected_rate: Rate(1.0),
                peak_rate: None,
                max_concurrency: None,
                confidence: Confidence(0.9),
                source: EvidenceSource::Observed,
                observed_at: None,
                valid_for: None,
            }),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Unknown,
        }]);
        assert_eq!(
            workload.validate(),
            Err(WorkloadError::EmptyObservationWindow)
        );
    }
}
