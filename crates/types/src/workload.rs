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

/// One entry in a repeating (streaming) workload: a query that fires
/// every `interval` milliseconds.
#[derive(Debug, Clone)]
pub struct RepeatingEntry {
    pub query: Query,
    /// How often the query fires, in milliseconds.
    pub interval: RepetitionInterval,
    pub requirements: Option<QueryRequirements>,
}

// ── Data characteristics ──────────────────────────────────────────────────────

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

/// Characteristics of the data arriving at the ingestion layer.
/// Used by the cost model and sketch-parameter binder to size sketches
/// and estimate transmission cost without running the query.
#[derive(Debug, Clone)]
pub struct DataCharacteristics {
    /// Number of distinct active time series for this metric.
    pub series_count: u64,
    /// Sample rate per series at the SDK / agent (Hz).
    pub samples_per_sec_per_series: f64,
    /// Wire size of one raw OTLP metric data point after protobuf encoding
    /// (bytes). Typical range: 50–200 bytes.
    pub bytes_per_raw_sample: u32,
    /// Known distinct key values per flush period for frequency / cardinality
    /// sketches. `None` → inferred analytically from inserts and distribution.
    pub distinct_keys_per_window: Option<u64>,
    /// Statistical distribution of keys in the stream.
    pub data_distribution: DataDistribution,
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
    /// Queries that repeat on a fixed interval (streaming / continuous).
    pub repeating_queries: Option<Vec<RepeatingEntry>>,
    /// Workload-level data characteristics used for sketch sizing and cost
    /// estimation. Applies to all queries in this workload.
    pub data_characteristics: Option<DataCharacteristics>,
}
