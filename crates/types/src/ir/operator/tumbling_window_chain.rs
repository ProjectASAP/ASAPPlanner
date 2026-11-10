//! Tumbling-window chains ([`ASAPOp::SummaryTumblingWindowChain`]): one
//! row per window of width `w`, on a grid anchored at the epoch, a state
//! with a time index. A chain has no range and no window count: a
//! `TimeRange` (and `TimeShift`) over it selects the windows a query reads
//! (#1207 PR 1), and how many windows it retains is $h_v$ (#1207 PR 7).
use super::asap::ASAPOp;
use super::input::OperatorInput;
use super::node::Operator;

/// The `SummaryTumblingWindowChain` a summary state carries merged: the
/// chain itself, or the one chain a `SummaryMerge` that keeps its groups
/// reads. `None` for any other input, a rollup (regrouping merge) included.
pub fn merged_chain<I: OperatorInput>(state: &I) -> Option<I> {
    match state.operator() {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => Some(state.clone()),
        Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            group_by: None,
            ..
        }) => match children.as_slice() {
            [child]
                if matches!(
                    child.operator(),
                    Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
                ) =>
            {
                Some(child.clone())
            }
            _ => None,
        },
        _ => None,
    }
}
