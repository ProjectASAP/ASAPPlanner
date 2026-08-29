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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Predictability {
    AdHoc,
    Predictable {
        known_at: Option<TimestampMs>,
    },
    #[default]
    Unknown,
}

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeSelection {
    pub scope: QueryTimeScope,
    pub lookback: Option<DurationMs>,
    /// Fixed upper bound. `None` means the planning/evaluation time.
    pub as_of: Option<TimestampMs>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Confidence(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationWindow {
    pub start: TimestampMs,
    pub end: TimestampMs,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpectedDemand {
    InvocationCount(u64),
    AverageRate(Rate),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DemandEstimate {
    pub observation_window: ObservationWindow,
    pub expected: ExpectedDemand,
    pub peak_rate: Option<Rate>,
    pub max_concurrency: Option<u64>,
    pub confidence: Confidence,
    pub source: EvidenceSource,
    pub observed_at: Option<TimestampMs>,
    pub valid_for: Option<DurationMs>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RepeatedDemand {
    FixedInterval(RepetitionInterval),
    Scheduled(Vec<TimestampMs>),
    EstimatedRate(DemandEstimate),
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryRecurrence {
    OneTime {
        invocations: u64,
        execute_at: Option<TimestampMs>,
    },
    Repeated(RepeatedDemand),
    Unknown,
}

/// One entry in a one-shot batch: a query plus its optional SLA constraints.
#[derive(Debug, Clone)]
pub struct BatchEntry {
    pub query: Query,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
    pub invocations: u64,
    pub execute_at: Option<TimestampMs>,
    pub time_selection: TimeSelection,
}

/// One query that fires every `interval` milliseconds. Its recurrence does
/// not imply that the queried data is continuously ingesting.
#[derive(Debug, Clone)]
pub struct RepeatingEntry {
    pub query: Query,
    pub demand: RepeatedDemand,
    pub requirements: QueryRequirements,
    pub predictability: Predictability,
    pub time_selection: TimeSelection,
}

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
            (Some(observed), Some(valid_for)) if now_ms > observed.saturating_add(valid_for) => {
                None
            }
            (None, Some(_)) => None,
            _ => Some(value),
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
            if let ExpectedDemand::AverageRate(rate) = estimate.expected {
                validate_rate(rate)?;
            }
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
    fn mixed_batch_and_repeated_entries_normalize_without_conflating_axes() {
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
                expected: ExpectedDemand::AverageRate(Rate(1.0)),
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
