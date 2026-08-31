use crate::types::AccuracyTarget;

// ── Query surface ─────────────────────────────────────────────────────────────

/// A raw query string in its source language, before any parsing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Query(pub String);

/// How often a repeating query fires, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepetitionInterval(pub u32);

/// Milliseconds since the Unix epoch. Workload timestamps use one explicit
/// representation so schedules, observations, and time selections agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimestampMs(pub u64);

/// A non-negative duration in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationMs(pub u64);

/// SQL dialect variant — different dialects have different syntax and
/// function sets that affect how the query string is parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SqlDialect {
    DataFusionSQL,
    ClickhouseSQL,
    ElasticSQL,
}

/// Source language of every query in the workload.
/// All queries in a single `QueryWorkload` share the same language.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QueryLanguage {
    PromQL,
    SQL(SqlDialect),
    DataFusion,
    ElasticDSL,
}

// ── Per-query requirements ────────────────────────────────────────────────────

/// Whether the caller explicitly requested an accuracy target or inherited
/// the normalized exact default.
#[derive(Debug, Clone, PartialEq)]
pub enum AccuracyRequirement {
    Explicit(AccuracyTarget),
    ImplicitExact,
}

impl AccuracyRequirement {
    pub fn target(&self) -> AccuracyTarget {
        match self {
            Self::Explicit(target) => target.clone(),
            Self::ImplicitExact => AccuracyTarget::Exact,
        }
    }
}

/// Optional maximum wall-clock response time for one query execution.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum LatencyRequirement {
    ExplicitMaxMs(f64),
    #[default]
    Unspecified,
}

/// Independent accuracy and response-latency constraints attached to one
/// query in the workload.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryRequirements {
    pub accuracy: AccuracyRequirement,
    pub response_latency: LatencyRequirement,
}

impl Default for QueryRequirements {
    fn default() -> Self {
        Self {
            accuracy: AccuracyRequirement::ImplicitExact,
            response_latency: LatencyRequirement::Unspecified,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Predictability {
    AdHoc,
    Predictable {
        known_at: Option<TimestampMs>,
    },
    #[default]
    Unknown,
}

/// Semantic relationship between a query and event time. This is supplied by
/// the workload author rather than inferred from issue time or range bounds:
/// those values do not distinguish, for example, a historical replay from a
/// live query with the same lookback.
///
/// `RealTime` follows the newest data, `Longitudinal` analyzes a fixed or
/// historical interval, and `Mixed` combines both (for example, comparing
/// the current hour with the same hour last week).
///
/// # Example
///
/// ```
/// use asap_types::workload::QueryTimeScope;
///
/// let live_dashboard = QueryTimeScope::RealTime;
/// let historical_report = QueryTimeScope::Longitudinal;
/// let week_over_week_dashboard = QueryTimeScope::Mixed;
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum QueryTimeScope {
    RealTime,
    Longitudinal,
    Mixed,
    #[default]
    Unknown,
}

/// Concrete event-time interval selected by a query, kept separate from its
/// semantic real-time/longitudinal classification.
///
/// # Example
///
/// A live dashboard evaluated at `200_000` ms with a five-minute lookback
/// selects events from the preceding five minutes. `as_of: None` makes its
/// upper bound the evaluation time rather than a fixed historical timestamp.
///
/// ```
/// use asap_types::workload::{DurationMs, QueryTimeScope, TimeSelection};
///
/// let last_five_minutes = TimeSelection {
///     scope: QueryTimeScope::RealTime,
///     lookback: Some(DurationMs(5 * 60 * 1_000)),
///     as_of: None,
/// };
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeSelection {
    pub scope: QueryTimeScope,
    pub lookback: Option<DurationMs>,
    /// Fixed upper bound. `None` means the planning/evaluation time.
    pub as_of: Option<TimestampMs>,
}

/// Confidence assigned to an estimated demand value, expressed in `[0, 1]`.
/// Validation rejects values outside that range.
///
/// For example, `Confidence(0.95)` says the demand estimate is supplied with
/// 95% confidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Confidence(pub f64);

/// Event-time interval from which a demand estimate was learned.
///
/// For example, `{ start: TimestampMs(0), end: TimestampMs(60_000) }`
/// describes an estimate based on the first minute of observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub enum RepeatedDemand {
    FixedInterval(RepetitionInterval),
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
#[derive(Debug, Clone, PartialEq)]
pub enum QueryRecurrence {
    OneTime {
        invocations: u64,
        execute_at: Option<TimestampMs>,
    },
    Repeated(RepeatedDemand),
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
///     time_selection: TimeSelection {
///         scope: QueryTimeScope::Longitudinal,
///         lookback: Some(DurationMs(24 * 60 * 60 * 1_000)),
///         as_of: Some(TimestampMs(10_000)),
///     },
/// };
/// ```
#[derive(Debug, Clone)]
pub struct BatchEntry {
    pub query: Query,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
    pub invocations: u64,
    pub execute_at: Option<TimestampMs>,
    pub time_selection: TimeSelection,
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
///     time_selection: TimeSelection {
///         scope: QueryTimeScope::RealTime,
///         lookback: Some(DurationMs(5 * 60 * 1_000)),
///         as_of: None,
///     },
/// };
/// ```
#[derive(Debug, Clone)]
pub struct RepeatingEntry {
    pub query: Query,
    pub demand: RepeatedDemand,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
    pub time_selection: TimeSelection,
}

/// Planner-facing normalized form of either [`BatchEntry`] or
/// [`RepeatingEntry`]. It keeps requirements, predictability, recurrence, and
/// event-time selection as separate axes.
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
///     time_selection: TimeSelection {
///         scope: QueryTimeScope::RealTime,
///         lookback: Some(DurationMs(5 * 60 * 1_000)),
///         as_of: None,
///     },
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct QueryWorkloadEntry {
    pub query: Query,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
    pub recurrence: QueryRecurrence,
    pub time_selection: TimeSelection,
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
            time_selection: entry.time_selection.clone(),
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
            time_selection: entry.time_selection.clone(),
        }
    }
}

// ── Data workload ─────────────────────────────────────────────────────────────

/// Whether the data queried by this workload is static, still arriving, or a
/// mixture of both. This is independent of whether queries repeat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DataArrival {
    AtRest,
    ContinuouslyIngesting,
    Mixed,
    #[default]
    Unknown,
}

/// Statistical distribution of keys in the incoming data stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DataDistribution {
    /// Zipf-distributed keys (s ≈ 1.1). A small number of keys dominate,
    /// so only a fraction of sketch cells are touched per window. Typical
    /// production case.
    #[default]
    Zipf,
    /// All keys are equally probable. Every window fills the sketch more
    /// uniformly; delta-compression benefit is lower.
    Uniform,
    /// Traffic arrives in bursts with a concentrated key set. Effective
    /// fill rate is lower on average but spikes can reach Uniform levels.
    Bursty,
}

/// Where an empirical workload value came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EvidenceSource {
    Declared,
    Observed,
    Derived,
    #[default]
    Unknown,
}

/// A workload value together with the provenance and freshness needed to
/// decide whether it is safe to use. Times and durations are milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence<T> {
    pub value: Option<T>,
    pub source: EvidenceSource,
    pub observed_at_ms: Option<u64>,
    pub valid_for_ms: Option<u64>,
}

impl<T> Default for Evidence<T> {
    fn default() -> Self {
        Self {
            value: None,
            source: EvidenceSource::Unknown,
            observed_at_ms: None,
            valid_for_ms: None,
        }
    }
}

impl<T> Evidence<T> {
    /// Return the value only while its freshness contract holds. Declared or
    /// timeless evidence with no `valid_for_ms` does not expire.
    pub fn value_at(&self, now_ms: u64) -> Option<&T> {
        let value = self.value.as_ref()?;
        match (self.observed_at_ms, self.valid_for_ms) {
            (Some(observed), _) if observed > now_ms => None,
            (Some(observed), Some(valid_for)) if now_ms > observed.saturating_add(valid_for) => {
                None
            }
            (None, Some(_)) => None,
            _ => Some(value),
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate(pub f64);

/// Workload-level facts about the data being queried. Unlike the former
/// ingestion-only `DataCharacteristics`, this also represents data at rest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataWorkload {
    pub arrival: DataArrival,
    pub ingestion_volume: Evidence<u64>,
    pub ingestion_rate: Evidence<Rate>,
    pub input_cardinality: Evidence<u64>,
    pub distribution: Evidence<DataDistribution>,
}

// ── Top-level workload ────────────────────────────────────────────────────────

/// The single normalised input type accepted by every entry point into the
/// planner (HTTP POST /plan, YAML file, query-log replay, OpAMP callback).
///
/// `query_batch` and `repeating_queries` may both be present. [`Self::entries`]
/// normalizes them into one ordered stream without conflating recurrence with
/// data arrival.
#[derive(Debug, Clone)]
pub struct QueryWorkload {
    /// Source language shared by all queries in this workload.
    pub language: QueryLanguage,
    /// One-shot queries executed together as a batch.
    pub query_batch: Option<Vec<BatchEntry>>,
    /// Queries that repeat on a fixed interval.
    pub repeating_queries: Option<Vec<RepeatingEntry>>,
    /// Workload-level data facts used for accuracy and cost estimation.
    /// Applies to all queries in this workload.
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
        if let Some(data) = &self.data_workload {
            if matches!(data.arrival, DataArrival::AtRest)
                && data.ingestion_rate.value.is_some_and(|rate| rate.0 > 0.0)
            {
                return Err(WorkloadError::AtRestWithPositiveIngestionRate);
            }
            validate_optional_rate(data.ingestion_rate.value)?;
        }
        Ok(())
    }
}

fn validate_entry(entry: &QueryWorkloadEntry) -> Result<(), WorkloadError> {
    if let LatencyRequirement::ExplicitMaxMs(ms) = entry.requirements.response_latency {
        if !ms.is_finite() || ms < 0.0 {
            return Err(WorkloadError::InvalidLatency(ms));
        }
    }
    match &entry.recurrence {
        QueryRecurrence::OneTime { invocations: 0, .. } => {
            return Err(WorkloadError::ZeroInvocations)
        }
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(0))) => {
            return Err(WorkloadError::ZeroRepetitionInterval)
        }
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
    #[error("data at rest cannot have a positive ingestion rate")]
    AtRestWithPositiveIngestionRate,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_workload() -> QueryWorkload {
        QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: None,
            data_workload: None,
        }
    }

    #[test]
    fn entries_preserve_recurrence_and_time_scope_as_independent_axes() {
        let mut workload = base_workload();
        workload.query_batch = Some(vec![BatchEntry {
            query: Query("historical".into()),
            requirements: QueryRequirements::default(),
            predictability: Predictability::AdHoc,
            invocations: 1,
            execute_at: None,
            time_selection: TimeSelection {
                scope: QueryTimeScope::Longitudinal,
                lookback: Some(DurationMs(300_000)),
                as_of: Some(TimestampMs(1_000_000)),
            },
        }]);
        workload.repeating_queries = Some(vec![RepeatingEntry {
            query: Query("dashboard".into()),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Predictable { known_at: None },
            time_selection: TimeSelection {
                scope: QueryTimeScope::RealTime,
                lookback: Some(DurationMs(300_000)),
                as_of: None,
            },
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
        assert_eq!(
            entries[0].time_selection.scope,
            QueryTimeScope::Longitudinal
        );
        assert_eq!(entries[1].time_selection.scope, QueryTimeScope::RealTime);
        workload.validate().unwrap();
    }

    #[test]
    fn stale_evidence_is_unknown_at_planning_time() {
        let evidence = Evidence {
            value: Some(Rate(10.0)),
            source: EvidenceSource::Observed,
            observed_at_ms: Some(1_000),
            valid_for_ms: Some(500),
        };
        assert_eq!(evidence.value_at(1_500), Some(&Rate(10.0)));
        assert_eq!(evidence.value_at(1_501), None);
    }

    #[test]
    fn future_evidence_and_demand_estimates_are_not_fresh() {
        let evidence = Evidence {
            value: Some(Rate(2.0)),
            source: EvidenceSource::Observed,
            observed_at_ms: Some(2_000),
            valid_for_ms: Some(1_000),
        };
        assert_eq!(evidence.value_at(1_999), None);
        assert_eq!(evidence.value_at(2_000), Some(&Rate(2.0)));

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
    fn at_rest_rejects_a_positive_ingestion_rate() {
        let mut workload = base_workload();
        workload.data_workload = Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ingestion_rate: Evidence {
                value: Some(Rate(1.0)),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            workload.validate(),
            Err(WorkloadError::AtRestWithPositiveIngestionRate)
        );
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
            time_selection: TimeSelection::default(),
        }]);
        assert_eq!(
            workload.validate(),
            Err(WorkloadError::EmptyObservationWindow)
        );
    }
}
