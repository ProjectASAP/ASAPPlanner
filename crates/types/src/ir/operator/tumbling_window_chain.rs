//! Tumbling-window chains ([`ASAPOp::SummaryTumblingWindowChain`]): one
//! row per window of width `w`, on a grid anchored at the epoch, a state
//! with a time index. A chain has no range and no window count: a
//! `TimeRange` (and `TimeShift`s) over it select the windows a query reads,
//! a `SummaryMerge` over that selection is the state of the chain's build
//! over the same range, and how many windows it retains is $h_v$ (#1207
//! PR 7).
use super::asap::ASAPOp;
use super::input::OperatorInput;
use super::node::Operator;
use super::non_asap::NonASAPOp;

/// Whether `state` is a state with a time index: a chain, or a `TimeRange`
/// or `TimeShift` over one.
pub(crate) fn is_time_indexed_state<I: OperatorInput>(state: &I) -> bool {
    match state.operator() {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => true,
        Operator::NonASAP(
            NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
        ) => is_time_indexed_state(&child),
        _ => false,
    }
}
