//! The accuracy composition rule of each exact function applied to an
//! approximate input, shared by Pass 1's guarantee composition and the
//! accuracy algebra.
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::CompositionOperator;

/// The composition operator that propagates an approximate input's guarantee
/// through `intent`; `None` for a function with no registered rule over
/// approximate input, whose callers must fail closed.
pub(crate) fn approximate_input_composition(intent: &AggIntent) -> Option<CompositionOperator> {
    Some(match intent {
        AggIntent::Sum { .. } => CompositionOperator::ExactSum,
        AggIntent::Min { .. } | AggIntent::Max { .. } => CompositionOperator::ExactExtremum,
        AggIntent::Avg { .. } => CompositionOperator::ExactAverage,
        AggIntent::Rate => CompositionOperator::CounterRate,
        AggIntent::IRate => CompositionOperator::InstantCounterRate,
        AggIntent::Increase => CompositionOperator::CounterIncrease,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_functions_have_distinct_definition_rules() {
        assert_eq!(
            approximate_input_composition(&AggIntent::Rate),
            Some(CompositionOperator::CounterRate)
        );
        assert_eq!(
            approximate_input_composition(&AggIntent::IRate),
            Some(CompositionOperator::InstantCounterRate)
        );
        assert_eq!(
            approximate_input_composition(&AggIntent::Increase),
            Some(CompositionOperator::CounterIncrease)
        );
    }
}
