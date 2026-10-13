//! Function facts for value-operation propagation: the composition rule an
//! exact function applies over approximate input, and its accumulator.
//! Not wired into the Stage 1 → Stage 3 pipeline yet (#669).
use asap_types::post_asap::{CompositionOperator, ExactKind, ExactParams};
use asap_types::pre_asap::AggIntent;

pub struct FunctionRules {
    pub accuracy: CompositionOperator,
    pub accumulator: Option<(ExactKind, ExactParams)>,
}

/// Unregistered functions have no approximate-input propagation rule.
pub fn function_rules(intent: &AggIntent) -> Option<FunctionRules> {
    let (accuracy, accumulator) = match intent {
        AggIntent::Sum { .. } => (
            CompositionOperator::ExactSum,
            Some((ExactKind::Sum, ExactParams::Sum)),
        ),
        AggIntent::Min { .. } => (
            CompositionOperator::ExactExtremum,
            Some((ExactKind::Min, ExactParams::Min)),
        ),
        AggIntent::Max { .. } => (
            CompositionOperator::ExactExtremum,
            Some((ExactKind::Max, ExactParams::Max)),
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

#[cfg(test)]
mod tests {
    use super::*;
    // The maintained extrema state must encode the direction independently of query text.
    #[test]
    fn minimum_and_maximum_have_distinct_accumulator_contracts() {
        assert_ne!(
            function_rules(&AggIntent::Min { col: None })
                .unwrap()
                .accumulator,
            function_rules(&AggIntent::Max { col: None })
                .unwrap()
                .accumulator
        );
    }
}
