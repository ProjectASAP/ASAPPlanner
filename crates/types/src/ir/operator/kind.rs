//! The kind of an operator without its arguments: the name every operator
//! has in a physical plan DAG. Measured operation costs are keyed by it
//! (the unit each is priced per is the row it reads unless said
//! otherwise), and deployment exclusions match on it.

use serde::{Deserialize, Serialize};

use super::asap::ASAPOp;
use super::node::Operator;
use super::non_asap::NonASAPOp;

/// Every physical operator a physical plan DAG can contain, by the name it
/// has there, e.g. `Scan` or `SummaryTumblingWindowChain`. The unit
/// `asap-cost` prices each per is the row it reads unless said otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OperatorKind {
    /// Per sample read. At ingestion time, an arriving sample; at query
    /// time, a sample fetched and decoded from the database holding raw
    /// data, part of the deployment's cost.
    Scan,
    Values,
    Filter,
    Project,
    /// Per input row, for each measure.
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
    PromqlVectorFromScalar,
    PromqlRelabel,
    PromqlInfoEnrich,
    PromqlSeriesSample,
    PromqlSubquery,
    /// Per input row inserted into its group's state.
    SummaryAgg,
    /// Per group read.
    SummaryEstimate,
    /// Per accumulator finalized (one per group).
    FinalizeExactAccumulator,
    MaintainPopulation,
    EvaluatePopulation,
    /// Per input state merged (each tumbling window of a chain is a state).
    SummaryMerge,
    /// Per input row placed in its tumbling window and inserted into that tumbling window's state.
    // Named `PaneChain` before #1163.
    #[serde(alias = "PaneChain")]
    SummaryTumblingWindowChain,
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
                ASAPOp::SummarySubtract { .. } => Self::SummarySubtract,
                ASAPOp::SummaryDelete { .. } => Self::SummaryDelete,
                ASAPOp::SummaryJoin { .. } => Self::SummaryJoin,
                ASAPOp::Extension { .. } => Self::Extension,
            },
        }
    }
}
