//! Readouts over merged exact summary states.
use crate::summary_kernels::exact::ExactAccumulator;
use crate::{AggregateCore, KeyByLabelValues, Statistic};
use std::sync::Arc;

fn merge_exact_states(
    states: impl IntoIterator<Item = Arc<dyn AggregateCore>>,
) -> Result<ExactAccumulator, String> {
    let mut states = states.into_iter();
    let exact = |state: &Arc<dyn AggregateCore>| {
        state
            .as_any()
            .downcast_ref::<ExactAccumulator>()
            .cloned()
            .ok_or_else(|| "readout requires Planner exact state".to_string())
    };
    let mut merged = exact(&states.next().ok_or("empty exact state input")?)?;
    for state in states {
        merged
            .merge_from(&exact(&state)?)
            .map_err(|error| error.to_string())?;
    }
    Ok(merged)
}

/// PromQL counter readouts omit a series with fewer than two samples. Other
/// state/type/range failures remain errors rather than empty results.
pub fn insufficient_counter_samples(state: &dyn AggregateCore, statistic: Statistic) -> bool {
    matches!(statistic, Statistic::Rate | Statistic::Increase)
        && state
            .as_any()
            .downcast_ref::<ExactAccumulator>()
            .is_some_and(|state| state.insufficient_counter_samples(statistic, &None))
}

/// Merge already selected exact panes and read one population. `None` means
/// the population is absent from the result: a counter with too few samples,
/// or an empty MIN/MAX.
pub fn exact_readout(
    states: impl IntoIterator<Item = Arc<dyn AggregateCore>>,
    statistic: Statistic,
    range_ms: Option<(i64, i64)>,
    key: Option<&KeyByLabelValues>,
) -> Result<Option<f64>, String> {
    let merged = merge_exact_states(states)?;
    if merged.insufficient_counter_samples(statistic, &key.cloned()) {
        return Ok(None);
    }
    merged
        .readout(statistic, range_ms, key)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod counter_tests {
    use super::*;
    use planner_types::post_asap::{ExactKind, ExactParams, FieldDataType as SummaryFamilyType};

    fn counter(kind: ExactKind, params: ExactParams, keyed: bool) -> ExactAccumulator {
        ExactAccumulator::new(SummaryFamilyType::ExactAggregate(kind, params), keyed).unwrap()
    }

    // A counter population with a single sample is absent, keyed or not.
    #[test]
    fn planner_counter_population_omits_insufficient_samples() {
        for (kind, params, statistic) in [
            (ExactKind::Rate, ExactParams::Rate, Statistic::Rate),
            (
                ExactKind::Increase,
                ExactParams::Increase,
                Statistic::Increase,
            ),
        ] {
            for keyed in [false, true] {
                let mut state = counter(kind.clone(), params.clone(), keyed);
                let key = keyed.then(|| KeyByLabelValues::new_with_labels(vec!["checkout".into()]));
                state.update(key.as_ref(), 10., 10_000);
                assert_eq!(
                    exact_readout(
                        [Arc::new(state) as Arc<dyn AggregateCore>],
                        statistic,
                        None,
                        key.as_ref()
                    )
                    .unwrap(),
                    None
                );
            }
        }
    }

    // Two ordered samples read a rate; an inverted range and empty input fail.
    #[test]
    fn sparse_counter_is_absent_but_invalid_ranges_still_fail() {
        let mut state = counter(ExactKind::Rate, ExactParams::Rate, false);
        state.update(None, 10., 10_000);
        let rate = Statistic::Rate;
        let one = [Arc::new(state.clone()) as Arc<dyn AggregateCore>];
        assert_eq!(
            exact_readout(one, rate, Some((0, 60_000)), None).unwrap(),
            None
        );
        state.update(None, 20., 20_000);
        let two = || [Arc::new(state.clone()) as Arc<dyn AggregateCore>];
        assert!(exact_readout(two(), rate, Some((0, 60_000)), None)
            .unwrap()
            .is_some());
        assert!(exact_readout(two(), rate, Some((60_000, 0)), None).is_err());
        assert!(exact_readout([], rate, Some((0, 60_000)), None).is_err());
    }
}
