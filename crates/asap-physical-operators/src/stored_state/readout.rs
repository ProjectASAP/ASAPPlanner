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
    merged
        .query_statistic(statistic, key, parameters)
        .map_err(|e| e.to_string())
}
