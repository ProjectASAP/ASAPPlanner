//! The kind of an operator without its arguments: the name every operator
//! has in a physical plan DAG. Measured operation costs are keyed by it,
//! and deployment exclusions match on it.

use serde::{Deserialize, Serialize};

use super::asap::ASAPOp;
use super::node::Operator;
use super::non_asap::NonASAPOp;

/// An operator without its arguments: every physical operator a physical
/// plan DAG can contain, by the name it has there, e.g. `Scan` or
/// `SummaryTumblingWindowChain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OperatorKind {
    Scan,
    Values,
    Filter,
    Project,
    Aggregate,
    Join,
    SetOp,
    Concat,
    Dedup,
    Sort,
    Limit,
    BinaryOp,
    SQLWindowFunc,
    TimeRange,
    TimeShift,
    TimeRangeCover,
    PromqlVectorFromScalar,
    PromqlRelabel,
    PromqlInfoEnrich,
    PromqlSeriesSample,
    PromqlSubquery,
    SummaryAgg,
    SummaryEstimate,
    FinalizeExactAccumulator,
    MaintainPopulation,
    EvaluatePopulation,
    SummaryMerge,
    // Formerly named `PaneChain`; old JSON still loads.
    #[serde(alias = "PaneChain")]
    SummaryTumblingWindowChain,
    SummaryMergeWindows,
    DecodeSamplesInRange,
    SummarySubtract,
    SummaryDelete,
    SummaryJoin,
    Extension,
}

impl OperatorKind {
    /// The kind of `operator`, e.g. `Scan` for a `NonASAPOp::Scan`.
    pub fn of<C>(operator: &Operator<C>) -> Self {
        match operator {
            Operator::NonASAP(op) => match op {
                NonASAPOp::Scan { .. } => Self::Scan,
                NonASAPOp::Values { .. } => Self::Values,
                NonASAPOp::Filter { .. } => Self::Filter,
                NonASAPOp::Project { .. } => Self::Project,
                NonASAPOp::Aggregate { .. } => Self::Aggregate,
                NonASAPOp::Join { .. } => Self::Join,
                NonASAPOp::SetOp { .. } => Self::SetOp,
                NonASAPOp::Concat { .. } => Self::Concat,
                NonASAPOp::Dedup { .. } => Self::Dedup,
                NonASAPOp::Sort { .. } => Self::Sort,
                NonASAPOp::Limit { .. } => Self::Limit,
                NonASAPOp::BinaryOp { .. } => Self::BinaryOp,
                NonASAPOp::SQLWindowFunc { .. } => Self::SQLWindowFunc,
                NonASAPOp::TimeRange { .. } => Self::TimeRange,
                NonASAPOp::TimeShift { .. } => Self::TimeShift,
                NonASAPOp::TimeRangeCover { .. } => Self::TimeRangeCover,
                NonASAPOp::PromqlVectorFromScalar(_) => Self::PromqlVectorFromScalar,
                NonASAPOp::PromqlRelabel { .. } => Self::PromqlRelabel,
                NonASAPOp::PromqlInfoEnrich { .. } => Self::PromqlInfoEnrich,
                NonASAPOp::PromqlSeriesSample { .. } => Self::PromqlSeriesSample,
                NonASAPOp::PromqlSubquery { .. } => Self::PromqlSubquery,
            },
            Operator::ASAP(op) => match op {
                ASAPOp::SummaryAgg { .. } => Self::SummaryAgg,
                ASAPOp::SummaryEstimate { .. } => Self::SummaryEstimate,
                ASAPOp::FinalizeExactAccumulator { .. } => Self::FinalizeExactAccumulator,
                ASAPOp::MaintainPopulation { .. } => Self::MaintainPopulation,
                ASAPOp::EvaluatePopulation { .. } => Self::EvaluatePopulation,
                ASAPOp::SummaryMerge { .. } => Self::SummaryMerge,
                ASAPOp::SummaryTumblingWindowChain { .. } => Self::SummaryTumblingWindowChain,
                ASAPOp::SummaryMergeWindows { .. } => Self::SummaryMergeWindows,
                ASAPOp::DecodeSamplesInRange { .. } => Self::DecodeSamplesInRange,
                ASAPOp::SummarySubtract { .. } => Self::SummarySubtract,
                ASAPOp::SummaryDelete { .. } => Self::SummaryDelete,
                ASAPOp::SummaryJoin { .. } => Self::SummaryJoin,
                ASAPOp::Extension { .. } => Self::Extension,
            },
        }
    }
}
