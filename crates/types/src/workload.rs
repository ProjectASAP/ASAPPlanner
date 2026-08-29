use crate::types::AccuracyTarget;

// ── Query surface ─────────────────────────────────────────────────────────────

/// A raw query string in its source language, before any parsing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Query(pub String);

/// How often a repeating query fires, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepetitionInterval(pub u32);

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

/// SLA constraints attached to a single query.
/// Both fields are optional: an absent bound means "no constraint on this axis."
#[derive(Debug, Clone)]
pub struct QueryRequirements {
    /// Maximum acceptable approximation error.
    pub accuracy: Option<AccuracyTarget>,
    /// Maximum acceptable end-to-end query latency in milliseconds.
    pub latency_ms: Option<f64>,
}

// ── Workload entries ──────────────────────────────────────────────────────────

/// One entry in a one-shot batch: a query plus its optional SLA constraints.
#[derive(Debug, Clone)]
pub struct BatchEntry {
    pub query: Query,
    pub requirements: Option<QueryRequirements>,
}

/// One query that fires every `interval` milliseconds. Its recurrence does
/// not imply that the queried data is continuously ingesting.
#[derive(Debug, Clone)]
pub struct RepeatingEntry {
    pub query: Query,
    /// How often the query fires, in milliseconds.
    pub interval: RepetitionInterval,
    pub requirements: Option<QueryRequirements>,
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
/// `query_batch` and `repeating_queries` are mutually exclusive today; both
/// may be present in the future when mixed batch+streaming workloads are
/// supported.
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
