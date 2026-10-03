//! Execution timing and data-state vocabulary of the operator IR.
//!
//! [`ExecutionTiming`] says when a node's value is produced (ingestion vs.
//! query time); [`ExecutionDataState`] pairs it with the [`DataPrimitive`]
//! the edge carries (raw values vs. summary state). The rules that assign
//! and check them over a DAG live in [`crate::ir::timing`], which reports
//! violations as [`ExecutionDataStateError`].

use thiserror::Error;

use crate::ir::SchemaDerivationError;
use crate::pre_asap::schema::Schema;

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
    pub fn is_query_time(&self) -> bool {
        *self == Self::QueryTime
    }
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
    /// placed beneath a maintained summary — the one shape issue #171's
    /// data_state split exists to make unrepresentable.
    #[error(
        "evaluation value under maintenance: {edge} received a {child} input, but a maintained \
         summary can only consume update-path values (or exact accumulator state)"
    )]
    EvaluationUnderMaintenance {
        edge: &'static str,
        child: ExecutionDataState,
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
    /// A maintenance-time value operation at the root of a plan:
    /// nothing maintains state above it, so its output is never read.
    #[error("A maintenance-time value operation cannot be a plan root: its update-path output feeds nothing")]
    MaintenanceRowsAtRoot,
    #[error("unsupported maintenance binary schema or operator")]
    InvalidMaintenanceBinary,
    #[error("checked division requires one valid guard on a read-time division operator")]
    InvalidCheckedDivision,
    /// An exact operator whose input columns are not all `Plain` at its
    /// declared data_state.
    #[error("exact operator consumes non-plain column {column:?} ({dtype})")]
    NonPlainOperand { column: String, dtype: String },
    /// A reserved ASAP operator (`SummarySubtract`, `SummaryDelete`,
    /// `SummaryJoin`, `Extension`) in an executable plan.
    #[error("{operator} is a reserved operator with no execution contract yet")]
    UnimplementedOperator { operator: &'static str },
    /// A node reached by export without a timing: the lifecycle timing pass
    /// was not applied to the DAG first.
    #[error("{operator} node has no execution timing; apply lifecycle timings before export")]
    UntimedNode { operator: &'static str },
}

/// `schema` as a summary-planning node output: fields and time axis kept,
/// unique keys dropped, closed.
pub fn lift_plain(schema: &Schema) -> Schema {
    Schema::lifted(schema.fields.clone(), schema.time_index)
}

/// Why an exact operator's output schema could not be derived.
#[derive(Debug, Error)]
pub enum ExactOperationSchemaError {
    #[error("exact operator input carries summary state, not plain columns")]
    NonPlainInput,
    #[error("schema derivation failed: {0}")]
    Schema(#[from] SchemaDerivationError),
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
