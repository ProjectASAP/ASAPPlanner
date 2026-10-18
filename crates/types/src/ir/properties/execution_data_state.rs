//! Execution timing and data-state vocabulary of the operator IR.
//!
//! [`ExecutionTiming`] says when a node's value is produced (ingestion vs.
//! query time); [`ExecutionDataState`] pairs it with the [`DataPrimitive`]
//! the edge carries (raw values vs. summary state). [`Materialization`] is
//! the per-sub-DAG choice a node's timing is derived from. The rules that assign
//! and check them over a DAG live in [`crate::ir::properties::timing`], which reports
//! violations as [`ExecutionDataStateError`].

use thiserror::Error;

use crate::physical::Materialization;

/// When a post-ASAP value is produced.
#[derive(
    Default, Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTiming {
    IngestionTime,
    #[default]
    QueryTime,
}

impl ExecutionTiming {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IngestionTime => "ingestion_time",
            Self::QueryTime => "query_time",
        }
    }
}

/// The primitive representation carried by a post-ASAP edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DataPrimitive {
    /// Directly usable values, including approximate summary evaluations.
    /// This does not imply original input data or an exact guarantee.
    Raw,
    SummaryState,
}

impl DataPrimitive {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::SummaryState => "summary_state",
        }
    }
}

/// The two-dimensional edge contract: when a value exists and which data
/// primitive it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ExecutionDataState {
    pub timing: ExecutionTiming,
    pub primitive: DataPrimitive,
}

impl ExecutionDataState {
    pub const INGESTION_ROWS: Self = Self {
        timing: ExecutionTiming::IngestionTime,
        primitive: DataPrimitive::Raw,
    };
    pub const INGESTION_SUMMARY: Self = Self {
        timing: ExecutionTiming::IngestionTime,
        primitive: DataPrimitive::SummaryState,
    };
    pub const QUERY_ROWS: Self = Self {
        timing: ExecutionTiming::QueryTime,
        primitive: DataPrimitive::Raw,
    };
}

impl std::fmt::Display for ExecutionDataState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.timing.as_str(), self.primitive.as_str())
    }
}

/// A plan-construction-time data_state violation. Typed (not a string) so a
/// strategy can degrade to a conservative fallback on the specific variant
/// it expects, and so tests can assert the *reason* a plan was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExecutionDataStateError {
    #[error("invalid maintained-population maintenance/evaluation contract")]
    InvalidMaintainedPopulation,
    /// A query-time value (a `SummaryEstimate` or query-time operator output)
    /// placed beneath a summary built at ingestion time — the one shape issue
    /// #171's data_state split exists to make unrepresentable.
    #[error(
        "query-time value under an ingestion-time summary: {edge} received a {child} input, but a \
         summary built at ingestion time can only consume ingestion-time rows (or exact \
         accumulator state)"
    )]
    QueryTimeValueUnderIngestionSummary {
        edge: &'static str,
        child: ExecutionDataState,
    },
    /// A summary build at query time over an input (rows or exact-accumulator
    /// state) produced at ingestion time and not stored there (the input node
    /// is not `MatAtIngestion`): at query time that input no longer exists.
    /// Bad input (a hand-built or candidate-fixed plan), not an internal bug.
    #[error(
        "a query-time {edge} reads {operator} output produced at ingestion time that is not \
         stored (not MatAtIngestion)"
    )]
    UnstoredIngestionInputUnderQueryTimeSummary {
        edge: &'static str,
        operator: &'static str,
    },
    /// Any other edge whose child data_state the parent does not accept
    /// (e.g. plain update rows fed straight into a `SummaryEstimate`, or a
    /// sketch's opaque state fed into a query-time operator).
    #[error("{edge} does not accept a {child} input")]
    IllegalChildDataState {
        edge: &'static str,
        child: ExecutionDataState,
    },
    /// A `SummaryAgg` whose child is summary state of a family other than an
    /// exact accumulator — re-accumulating opaque sketch/sample/… state on
    /// the update path has no defined semantics here.
    #[error(
        "SummaryAgg.child carries {family} summary state; only exact accumulator state can be \
         composed into another maintained summary"
    )]
    UnsupportedStateComposition { family: String },
    /// One shared node assigned two different execution timings by its
    /// consumers; no single execution of it can serve both.
    #[error("shared node is assigned conflicting timings: {first} and {second}")]
    ConflictingTiming {
        first: ExecutionDataState,
        second: ExecutionDataState,
    },
    /// A plan root that runs at ingestion time without being stored there
    /// (a timing fixed by a candidate builder): no query reads its rows. A
    /// root assigned `MatAtIngestion` is valid; the query reads the stored
    /// output.
    #[error(
        "{operator} root runs at ingestion time but is not MatAtIngestion: no query reads its \
         output"
    )]
    UnstoredIngestionRoot { operator: &'static str },
    /// An explicit materialization on a node whose timing a candidate builder
    /// fixed (e.g. a query-time `FinalizeExactAccumulator` read boundary)
    /// that needs the other timing.
    #[error(
        "{operator} is fixed at {} by its candidate; materialization {materialization:?} needs \
         the other timing",
        fixed.as_str()
    )]
    MaterializationConflictsWithFixedTiming {
        operator: &'static str,
        fixed: ExecutionTiming,
        materialization: Materialization,
    },
    /// A summary build with a filter placed at ingestion time. An
    /// ingestion-time build reads raw samples, which carry none of the
    /// query's columns the filter reads, so only a query-time build can
    /// apply it.
    #[error("{operator} has a filter, which only a query-time build can apply")]
    FilteredSummaryAtIngestion { operator: &'static str },
    /// An ingestion-time `BinaryOp` the update path cannot evaluate: not
    /// arithmetic without vector matching, operands that are not the same
    /// per-series rows, or a schema other than non-null timestamps, at most
    /// one series identity and one Float64 value.
    #[error("unsupported ingestion-time binary schema or operator")]
    InvalidIngestionTimeBinary,
    #[error("checked division requires one valid guard on a read-time division operator")]
    InvalidCheckedDivision,
    /// An exact operator whose input columns are not all `Plain` at its
    /// declared data_state.
    #[error("exact operator consumes non-plain column {column:?} ({dtype})")]
    NonPlainOperand { column: String, dtype: String },
    /// An ASAP operator with no execution timing contract yet in an
    /// executable plan: the reserved `SummarySubtract`, `SummaryDelete`,
    /// `SummaryJoin` and `Extension`.
    #[error("{operator} has no execution timing contract yet")]
    UnimplementedOperator { operator: &'static str },
    /// A node assigned `MatAtIngestion` over a sub-DAG that can only run at
    /// query time (#509 Algorithm 3, line 13).
    #[error("{operator}: upstream of ingestion-time node not at ingestion time")]
    UpstreamNotAtIngestion { operator: &'static str },
    /// A node reached by export without a timing: the materialization timing pass
    /// was not applied to the DAG first.
    #[error(
        "{operator} node has no execution timing; apply materialization timings before export"
    )]
    UntimedNode { operator: &'static str },
    /// The exported physical DAG failed its own validation, e.g. an empty
    /// batch or a hand-timed plan the timing pass would have rejected.
    #[error("invalid physical ASAP DAG: {0}")]
    InvalidPhysicalDAG(#[from] crate::ir::physical_export::PhysicalASAPDAGValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both execution phases use raw values, distinct from maintained state.
    #[test]
    fn raw_primitive_labels() {
        assert_eq!(
            ExecutionDataState::INGESTION_ROWS.primitive,
            DataPrimitive::Raw
        );
        assert_eq!(ExecutionDataState::QUERY_ROWS.primitive, DataPrimitive::Raw);
        assert_eq!(
            ExecutionDataState::INGESTION_ROWS.to_string(),
            "ingestion_time/raw"
        );
        assert_eq!(ExecutionDataState::QUERY_ROWS.to_string(), "query_time/raw");
        assert_eq!(DataPrimitive::SummaryState.as_str(), "summary_state");
    }

    #[test]
    fn execution_phase_wire_names_are_ingestion_and_query_time() {
        for (phase, name) in [
            (ExecutionTiming::IngestionTime, "ingestion_time"),
            (ExecutionTiming::QueryTime, "query_time"),
        ] {
            assert_eq!(phase.as_str(), name);
            assert_eq!(serde_json::to_value(phase).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<ExecutionTiming>(serde_json::json!(name)).unwrap(),
                phase
            );
        }
        assert!(serde_json::from_str::<ExecutionTiming>("\"maintenance_time\"").is_err());
        assert!(serde_json::from_str::<ExecutionTiming>("\"MaintenanceTime\"").is_err());
    }
}
