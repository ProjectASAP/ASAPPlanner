//! Planner-declared readouts over reconstructed summary states.
use super::delta_apply::SummaryState;
use planner_types::{post_asap::SketchQuery, pre_asap::ColumnRef};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Unsupported(&'static str),
}
pub fn sketch_query_value(rs: &SummaryState, query: &SketchQuery) -> Result<f64, Error> {
    if let SummaryState::UnivMon(state) = rs {
        use crate::AggregateCore;
        let statistic = match query {
            SketchQuery::Cardinality => crate::Statistic::Cardinality,
            SketchQuery::FrequencyL2 => crate::Statistic::FrequencyL2,
            SketchQuery::FrequencyEntropy => crate::Statistic::FrequencyEntropy,
            SketchQuery::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            } => crate::Statistic::Count,
            _ => return Err(Error::Unsupported("unsupported UnivMon readout")),
        };
        return state
            .query_statistic(statistic, &None, &Default::default())
            .map_err(|_| Error::Unsupported("UnivMon readout failed"));
    }
    match query {
        SketchQuery::FrequencyL2 | SketchQuery::FrequencyEntropy => Err(Error::Unsupported(
            "frequency moment readout requires UnivMon",
        )),
        SketchQuery::Quantile { q } => match rs {
            // Typed PromQL/continuous-percentile readout uses interpolation;
            // portable DDS `quantile` deliberately retains lower-rank parity.
            SummaryState::Dd(sketch) => sketch.quantile_interpolated(*q).ok_or(Error::Unsupported(
                "DDS interpolated quantile is unavailable",
            )),
            _ => Ok(rs.quantile(*q)),
        },
        SketchQuery::Cardinality => Ok(rs.cardinality()),
        // `key: ColumnRef::SampleValue, value: None` means "no specific
        // item" -- the bare bucket total. `key: Named(_), value: Some(v)`
        // is a per-item point lookup (e.g. `count(cms_metric{item="x"})`)
        // -- `value` is where the filter's actual value lives (see
        // `planner_types::post_asap::SketchQuery::PointCount`'s doc for why `readout`
        // can't resolve it itself). Any other combination (e.g. a `Named`
        // key with no value, or `SampleValue` with a value) is a shape
        // this executor doesn't expect to see and reports rather than
        // silently misreading.
        SketchQuery::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        } => Ok(rs.total()),
        SketchQuery::PointCount {
            key: ColumnRef::Named(_) | ColumnRef::Qualified { .. },
            value: Some(v),
        } => rs.estimate(v).ok_or(Error::Unsupported(
            "PointCount by key requires a Frequency-family sketch (Cms/CountSketch/..WithHeap)",
        )),
        SketchQuery::PointCount { .. } => Err(Error::Unsupported(
            "unrecognized PointCount shape (key/value combination not expected)",
        )),
        // Both readout callers branch on `TopK` before ever calling this
        // function (see `readout_cumulative`/`readout_per_window`), so
        // this arm is unreachable in practice; kept for match
        // exhaustiveness (`SketchQuery` has no `#[non_exhaustive]`) and to
        // fail loudly rather than panic if that invariant is ever broken.
        SketchQuery::TopK { .. } => Err(Error::Unsupported(
            "TopK must be read out via topk_ranked, not sketch_query_value",
        )),
    }
}

/// Rank a merged `SummaryState`'s top-k heap items descending by value and
/// cap at the requested `k`. The sort is load-bearing, not defensive
/// polish: `SummaryState::topk_items` reads back a bounded min-heap's
/// backing array as-is (`HHHeap::heap()`, asap_sketchlib) -- it does NOT
/// actually guarantee order despite its own doc wording. Errors for a
/// heap-less family (`Dd`/`Hll`/`Kll`/`Cms`/`CountSketch` -- no item
/// universe to rank), not for an empty heap (a heap-bearing family that
/// simply never received any updates yields `Ok(vec![])`, not an error).
pub fn topk_ranked(rs: &SummaryState, k: usize) -> Result<Vec<(String, f64)>, Error> {
    let mut items = rs.topk_items().ok_or(Error::Unsupported(
        "TopK requires a heap-bearing family (CmsWithHeap/CountSketchWithHeap) -- \
         this state's family carries no item universe to rank",
    ))?;
    items.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0)) // deterministic tie-break for equal counts
    });
    items.truncate(k);
    Ok(items)
}

/// Merge already selected exact panes and finalize using the shared accumulator contract.
pub fn exact_readout(
    states: impl IntoIterator<Item = std::sync::Arc<dyn crate::AggregateCore>>,
    statistic: crate::Statistic,
    key: &Option<crate::KeyByLabelValues>,
    parameters: &std::collections::HashMap<String, String>,
) -> Result<f64, String> {
    let merged = merge_exact_states(states)?;
    merged
        .query_statistic(statistic, key, parameters)
        .map_err(|e| e.to_string())
}

fn merge_exact_states(
    states: impl IntoIterator<Item = std::sync::Arc<dyn crate::AggregateCore>>,
) -> Result<Box<dyn crate::AggregateCore>, String> {
    let mut states = states.into_iter();
    let mut merged = states
        .next()
        .ok_or_else(|| "empty exact state input".to_string())?
        .clone_boxed_core();
    for state in states {
        merged = merged
            .merge_with(state.as_ref())
            .map_err(|e| e.to_string())?;
    }
    Ok(merged)
}

/// PromQL counter readouts omit a series with fewer than two samples. Other
/// state/type/range failures remain errors rather than empty results.
pub fn insufficient_counter_samples(
    state: &dyn crate::AggregateCore,
    statistic: crate::Statistic,
) -> bool {
    matches!(
        statistic,
        crate::Statistic::Rate | crate::Statistic::Increase
    ) && (state
        .as_any()
        .downcast_ref::<crate::summary_kernels::IncreaseAccumulator>()
        .is_some_and(|state| {
            state.sample_count < 2 || state.last_seen_timestamp == state.starting_timestamp
        })
        || state
            .as_any()
            .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
            .is_some_and(|state| state.insufficient_counter_samples(statistic, &None)))
}

pub fn exact_readout_optional(
    states: impl IntoIterator<Item = std::sync::Arc<dyn crate::AggregateCore>>,
    statistic: crate::Statistic,
    key: &Option<crate::KeyByLabelValues>,
    parameters: &std::collections::HashMap<String, String>,
) -> Result<Option<f64>, String> {
    let merged = merge_exact_states(states)?;
    let counter = key.as_ref().and_then(|key| {
        merged
            .as_any()
            .downcast_ref::<crate::summary_kernels::KeyedCounterState>()
            .and_then(|state| state.increases.get(key))
    });
    let exact_insufficient = merged
        .as_any()
        .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
        .is_some_and(|state| state.insufficient_counter_samples(statistic, key));
    if exact_insufficient
        || insufficient_counter_samples(merged.as_ref(), statistic)
        || counter.is_some_and(|counter| insufficient_counter_samples(counter, statistic))
    {
        return Ok(None);
    }
    merged
        .query_statistic(statistic, key, parameters)
        .map(Some)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod counter_tests {
    use super::*;
    use crate::{summary_kernels::IncreaseAccumulator, AggregateCore, Measurement, Statistic};
    use std::sync::Arc;

    #[test]
    fn planner_counter_population_omits_insufficient_samples() {
        use planner_types::post_asap::{ExactKind, ExactParams, SummaryFamilyType};
        for (kind, params, statistic) in [
            (ExactKind::Rate, ExactParams::Rate, Statistic::Rate),
            (
                ExactKind::Increase,
                ExactParams::Increase,
                Statistic::Increase,
            ),
        ] {
            for keyed in [false, true] {
                let mut state = crate::summary_kernels::exact::ExactAccumulator::new(
                    SummaryFamilyType::ExactAggregate(kind.clone(), params.clone()),
                    keyed,
                )
                .unwrap();
                let key = keyed
                    .then(|| crate::KeyByLabelValues::new_with_labels(vec!["checkout".into()]));
                state.update(key.as_ref(), 10., 10_000);
                assert_eq!(
                    exact_readout_optional(
                        [Arc::new(state) as Arc<dyn AggregateCore>],
                        statistic,
                        &key,
                        &Default::default()
                    )
                    .unwrap(),
                    None
                );
            }
        }
    }

    #[test]
    fn sparse_counter_is_absent_but_invalid_ranges_still_fail() {
        let mut state =
            IncreaseAccumulator::new(Measurement::new(10.), 10_000, Measurement::new(10.), 10_000);
        let parameters = std::collections::HashMap::from([
            ("range_start_ms".into(), "0".into()),
            ("range_end_ms".into(), "60000".into()),
        ]);
        assert_eq!(
            exact_readout_optional(
                [Arc::new(state.clone()) as Arc<dyn AggregateCore>],
                Statistic::Rate,
                &None,
                &parameters
            )
            .unwrap(),
            None
        );
        let mut repeated = state.clone();
        repeated.update(Measurement::new(10.), 10_000);
        assert_eq!(
            exact_readout_optional(
                [Arc::new(repeated) as Arc<dyn AggregateCore>],
                Statistic::Rate,
                &None,
                &parameters
            )
            .unwrap(),
            None
        );
        let mut keyed = crate::summary_kernels::KeyedCounterState::new();
        let label = crate::KeyByLabelValues::new_with_labels(vec!["checkout".into()]);
        keyed.update(label.clone(), state.clone());
        assert_eq!(
            exact_readout_optional(
                [Arc::new(keyed.clone()) as Arc<dyn AggregateCore>],
                Statistic::Rate,
                &Some(label),
                &parameters
            )
            .unwrap(),
            None
        );
        assert!(exact_readout_optional(
            [Arc::new(keyed) as Arc<dyn AggregateCore>],
            Statistic::Rate,
            &Some(crate::KeyByLabelValues::new_with_labels(vec![
                "missing".into()
            ])),
            &parameters
        )
        .is_err());
        state.update(Measurement::new(20.), 20_000);
        assert!(exact_readout_optional(
            [Arc::new(state.clone()) as Arc<dyn AggregateCore>],
            Statistic::Rate,
            &None,
            &parameters
        )
        .unwrap()
        .is_some());
        let invalid = std::collections::HashMap::from([
            ("range_start_ms".into(), "60000".into()),
            ("range_end_ms".into(), "0".into()),
        ]);
        assert!(exact_readout_optional(
            [Arc::new(state) as Arc<dyn AggregateCore>],
            Statistic::Rate,
            &None,
            &invalid
        )
        .is_err());
        assert!(exact_readout_optional([], Statistic::Rate, &None, &parameters).is_err());
    }
}
