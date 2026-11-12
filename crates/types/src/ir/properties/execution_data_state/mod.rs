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
    /// What a node with `operator` and `result_kind` outputs: a summary
    /// state (a summary operator's state or a maintained population, or any
    /// `State` result) or raw values.
    pub fn of<C>(
        operator: &crate::ir::Operator<C>,
        result_kind: crate::ir::OperatorResultKind,
    ) -> Self {
        use crate::ir::{ASAPOp, Operator, OperatorResultKind};
        match operator {
            Operator::ASAP(
                ASAPOp::SummaryAgg { .. }
                | ASAPOp::SummaryTumblingWindowChain { .. }
                | ASAPOp::SummaryJoin { .. }
                | ASAPOp::MaintainPopulation { .. },
            ) => Self::SummaryState,
            _ if result_kind == OperatorResultKind::State => Self::SummaryState,
            _ => Self::Raw,
        }
    }

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
mod tests;
