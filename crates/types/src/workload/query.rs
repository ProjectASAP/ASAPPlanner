//! The query workload: each query, its requirements and its recurrence.

use crate::accuracy::AccuracyTarget;
use serde::{Deserialize, Serialize};

use super::data::validate_rate;
use super::{Rate, WorkloadError};

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
/// let dashboard = RepeatedDemand::FixedIntervalAt {
///     interval: RepetitionInterval(10_000),
///     evaluation_phase: TimestampMs(0),
/// };
/// let scheduled = RepeatedDemand::Scheduled(vec![
///     TimestampMs(100_000),
///     TimestampMs(200_000),
/// ]);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatedDemand {
    /// Fixed cadence with a known evaluation phase. The timestamp is any
    /// evaluation endpoint on that cadence, not necessarily its first run.
    /// There is no variant without a phase: Pass 2's chains and Stage 3's
    /// contexts need the evaluation times, and phase 0 is no safe default
    /// (Prometheus staggers rule groups by a hash).
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
///     RepeatedDemand::FixedIntervalAt {
///         interval: RepetitionInterval(60_000),
///         evaluation_phase: TimestampMs(0),
///     },
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
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt { interval, .. }) => {
                Some(u64::from(interval.0)).filter(|&ms| ms > 0)
            }
            _ => None,
        }
    }

    /// The grid every evaluation with this recurrence and `range_evaluation`
    /// falls on, in milliseconds, anchored at the epoch (every evaluation
    /// time is a multiple of it). The times' grid is the greatest common
    /// divisor of a [`RepeatedDemand::FixedIntervalAt`]'s interval and phase
    /// (every 20 s at phase 5 s is on a 5 s grid), of the scheduled times, or
    /// a one-off query's time. A range evaluation of more than one step also
    /// takes the greatest common divisor with its step, since each run
    /// evaluates at every step up to its end. `None` when the times are not
    /// known: an estimated rate, a one-off query without a time, an unknown
    /// recurrence, or a zero interval. Pass 2's chain widths and the evaluation grids of
    /// Stage 3's contexts, which
    /// [`PhysicalCandidate::delivers_on`](crate::stages::PhysicalCandidate::delivers_on)
    /// reads, both come from it.
    pub fn evaluation_grid_ms(&self, range_evaluation: Option<RangeEvaluation>) -> Option<u64> {
        let times_grid_ms = match self {
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                evaluation_phase, ..
            }) => greatest_common_divisor(self.fixed_interval_ms()?, evaluation_phase.0),
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => times
                .iter()
                .fold(0, |grid_ms, time| greatest_common_divisor(grid_ms, time.0)),
            QueryRecurrence::OneTime {
                execute_at: Some(time),
                ..
            } => time.0,
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(_))
            | QueryRecurrence::OneTime {
                execute_at: None, ..
            }
            | QueryRecurrence::Unknown => return None,
        };
        Some(match range_evaluation {
            Some(range) if range.step_ms > 0 && range.evaluations_per_request() > 1 => {
                greatest_common_divisor(times_grid_ms, range.step_ms)
            }
            _ => times_grid_ms,
        })
    }
}

/// The most evaluation steps one range request may have, as Prometheus
/// allows for `/api/v1/query_range` (it rejects a request with more).
pub(crate) const MAX_RANGE_EVALUATION_STEPS: u64 = 11_000;

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
/// [`DataArrival`](crate::workload::DataArrival).
///
/// # Example
///
/// ```
/// use asap_types::workload::*;
///
/// let dashboard = RepeatingEntry {
///     query: Query("rate(requests[5m])".into()),
///     demand: RepeatedDemand::FixedIntervalAt {
///         interval: RepetitionInterval(10_000),
///         evaluation_phase: TimestampMs(0),
///     },
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
///         RepeatedDemand::FixedIntervalAt {
///             interval: RepetitionInterval(10_000),
///             evaluation_phase: TimestampMs(0),
///         },
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
    /// Stage 2 keep tumbling-window widths on its step grid, and Stage 3 prices
    /// [`RangeEvaluation::evaluations_per_request`] evaluations per run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_evaluation: Option<RangeEvaluation>,
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

impl DemandEstimate {
    /// Whether this estimate was already observed and has not expired at
    /// `now_ms`. A validity duration without an observation time is not a
    /// usable freshness contract.
    #[cfg(test)]
    fn is_fresh_at(&self, now_ms: u64) -> bool {
        match (self.observed_at, self.valid_for) {
            (Some(observed), _) if observed.0 > now_ms => false,
            (Some(observed), Some(valid_for)) => now_ms <= observed.0.saturating_add(valid_for.0),
            (None, Some(_)) => false,
            _ => true,
        }
    }
}

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

    pub(crate) fn validate(&self) -> Result<(), WorkloadError> {
        for entry in self.entries() {
            validate_entry(&entry)?;
        }
        Ok(())
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
        QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::*;

    /// Only a fixed-interval repetition with a positive interval has a
    /// fixed interval; a zero interval, a schedule, an estimated rate, a
    /// one-time query and an unknown recurrence have none.
    #[test]
    fn fixed_interval_ms_is_set_only_for_a_positive_fixed_interval() {
        let repeated = |demand| QueryRecurrence::Repeated(demand);
        assert_eq!(
            repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(10_000),
                evaluation_phase: TimestampMs(0),
            })
            .fixed_interval_ms(),
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
            repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(0),
                evaluation_phase: TimestampMs(0),
            })
            .fixed_interval_ms(),
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

    /// A fixed interval without an evaluation phase is rejected at parse
    /// time, naming `fixed_interval_at` (#1260).
    #[test]
    fn fixed_interval_without_phase_is_rejected() {
        let error =
            serde_json::from_value::<RepeatedDemand>(serde_json::json!({"fixed_interval": 60000}))
                .unwrap_err()
                .to_string();
        assert!(error.contains("fixed_interval_at"), "{error}");
    }

    /// A repeating query with a zero fixed interval is rejected.
    #[test]
    fn zero_repetition_interval_is_rejected() {
        let mut workload = base_workload();
        workload.repeating_queries = Some(vec![RepeatingEntry {
            query: Query("dashboard".into()),
            demand: RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(0),
                evaluation_phase: TimestampMs(0),
            },
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
                "demand": {"fixed_interval_at": {"interval": 30000, "evaluation_phase": 0}},
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
        let grid = |entry: &QueryWorkloadEntry| {
            entry.recurrence.evaluation_grid_ms(entry.range_evaluation)
        };
        assert_eq!(grid(&entry), Some(30_000));
        assert_eq!(grid(&QueryWorkloadEntry::from(&plain)), Some(30_000));
        let every = |interval_ms, phase_ms| {
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(interval_ms),
                evaluation_phase: TimestampMs(phase_ms),
            })
        };
        // Refreshes every 30 s at phase 0 and steps every 60 s: evaluations
        // on a 30 s grid.
        let stepped = QueryWorkloadEntry {
            recurrence: every(30_000, 0),
            ..entry.clone()
        };
        assert_eq!(grid(&stepped), Some(30_000));
        // At a declared phase, also on that phase from the epoch: every
        // minute at phase 125 s is on a 5 s grid; at phase 120 s, on 60 s.
        let at_phase = |phase_ms| QueryWorkloadEntry {
            recurrence: every(60_000, phase_ms),
            range_evaluation: None,
            ..entry.clone()
        };
        assert_eq!(grid(&at_phase(125_000)), Some(5_000));
        assert_eq!(grid(&at_phase(120_000)), Some(60_000));
        // A one-step range (span 0) is one evaluation per request: its
        // step does not narrow the grid (refreshing every 60 s at a 45 s
        // step would otherwise give a 15 s grid for nothing).
        let one_step = QueryWorkloadEntry {
            range_evaluation: Some(RangeEvaluation {
                step_ms: 45_000,
                span_ms: 0,
            }),
            recurrence: every(60_000, 0),
            ..entry.clone()
        };
        assert_eq!(grid(&one_step), Some(60_000));

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
            demand: RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(10_000),
                evaluation_phase: TimestampMs(0),
            },
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
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt { .. })
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
