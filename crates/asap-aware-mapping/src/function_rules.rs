//! Function facts shared by value-operation propagation and accumulator realization.
//! Runtime support remains a deployment decision in `CostModel`.
use asap_types::post_asap::{CompositionOperator, ExactKind, ExactParams};
use asap_types::pre_asap::AggIntent;

pub(crate) struct FunctionRules {
    pub accuracy: CompositionOperator,
    pub accumulator: Option<(ExactKind, ExactParams)>,
}

/// Unregistered functions have no approximate-input propagation rule.
pub(crate) fn function_rules(intent: &AggIntent) -> Option<FunctionRules> {
    let (accuracy, accumulator) = match intent {
        AggIntent::Sum { .. } => (
            CompositionOperator::ExactSum,
            Some((ExactKind::Sum, ExactParams::Sum)),
        ),
        AggIntent::Min { .. } | AggIntent::Max { .. } => (
            CompositionOperator::ExactExtremum,
            Some((ExactKind::MinMax, ExactParams::MinMax)),
        ),
        AggIntent::Avg { .. } => (CompositionOperator::ExactAverage, None),
        AggIntent::Rate => (
            CompositionOperator::CounterRate,
            Some((ExactKind::Rate, ExactParams::Rate)),
        ),
        AggIntent::IRate => (
            CompositionOperator::InstantCounterRate,
            Some((ExactKind::IRate, ExactParams::IRate)),
        ),
        AggIntent::Increase => (
            CompositionOperator::CounterIncrease,
            Some((ExactKind::Increase, ExactParams::Increase)),
        ),
        _ => return None,
    };
    Some(FunctionRules {
        accuracy,
        accumulator,
    })
}
