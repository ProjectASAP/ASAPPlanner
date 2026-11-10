//! Execution timing and data-state vocabulary of the operator IR.
//!
//! [`ExecutionTiming`] says when a node's value is produced (ingestion vs.
//! query time); [`ExecutionDataState`] pairs it with the [`DataPrimitive`]
//! the edge carries (raw values vs. summary state). A node's timing comes
//! from the [`crate::stages::PhysicalProperties`] of the physical candidate
//! it realizes.

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
    pub(crate) fn as_str(self) -> &'static str {
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
