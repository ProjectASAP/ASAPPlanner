//! A state's full content as text, so two states compare bit for bit:
//! an exact accumulator's encoded fields, a sketch's counters or answers.
use asap_executor::planner::ir::scalar::ColumnRef;
use asap_executor::planner::summary_type::SketchStatistic;
use asap_executor::summary_kernels::weighted_frequency::WeightedFrequency;
use asap_executor::summary_kernels::{
    exact::ExactAccumulator, univmon::UnivMonAccumulator, DDSketchAccumulator,
    DatasketchesKLLAccumulator, HllSketchAccumulator, KmvAccumulator,
};
use asap_executor::AggregateCore;

/// The full content of a state, for byte-for-byte comparison (a KLL's count
/// and quantiles, a UnivMon's answers).
pub fn fingerprint(state: &dyn AggregateCore) -> String {
    let any = state.as_any();
    if let Some(s) = any.downcast_ref::<ExactAccumulator>() {
        // MessagePack keeps each f64's bits, NaN payloads and -0.0 included.
        return format!("{:?}", rmp_serde::to_vec(s).unwrap());
    }
    if let Some(s) = any.downcast_ref::<DatasketchesKLLAccumulator>() {
        // asap_sketchlib seeds an unseeded KLL's compaction coin from the
        // wall clock; with `k` above the sample count nothing compacts, so
        // its count and quantiles are deterministic.
        return format!(
            "KLL k={} n={} q={:?}",
            s.inner.k(),
            s.inner.count(),
            [0.1, 0.5, 0.9, 1.0].map(|q| s.inner.quantile(q).to_bits())
        );
    }
    if let Some(s) = any.downcast_ref::<DDSketchAccumulator>() {
        return format!("{s:?}");
    }
    if let Some(s) = any.downcast_ref::<HllSketchAccumulator>() {
        return format!("{s:?}");
    }
    if let Some(s) = any.downcast_ref::<KmvAccumulator>() {
        return format!("{s:?}");
    }
    if let Some(s) = any.downcast_ref::<UnivMonAccumulator>() {
        // Its heaps' internal order depends on hash-map iteration, so even
        // two merges of the same states differ in which heavy hitters they
        // keep, and so in their distinct count: compare its other answers.
        use asap_executor::planner::summary_type::SketchStatistic as Q;
        let count = Q::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        return [count, Q::FrequencyL2, Q::FrequencyEntropy]
            .iter()
            .map(|query| s.estimate(query).unwrap().to_bits().to_string())
            .collect::<Vec<_>>()
            .join(",");
    }
    if let Some(s) = any.downcast_ref::<WeightedFrequency>() {
        return format!("{:?} {:?}", s.counters(), s.rows(usize::MAX));
    }
    if let Some(s) = any.downcast_ref::<asap_executor::summary_kernels::HydraCmsGroup>() {
        // The group's name and its count of every item the samples use.
        let counts = (0..13)
            .map(|item| {
                let query = SketchStatistic::PointCount {
                    key: ColumnRef::Named("item".into()),
                    value: Some(format!("i{item}")),
                };
                s.estimate(&query).unwrap().to_string()
            })
            .collect::<Vec<_>>();
        return format!("{} {}", s.group, counts.join(","));
    }
    panic!("no fingerprint for this state")
}
