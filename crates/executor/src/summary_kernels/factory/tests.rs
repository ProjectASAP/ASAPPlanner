use super::*;

/// A DDSketch updater reports sketchlib's count of the bins its stores
/// allocate: none before a sample, one 128-bin chunk of the positive
/// store after a few positive samples.
#[test]
fn ddsketch_memory_is_its_allocated_bins() {
    let mut updater = DDSketchAccumulatorUpdater::new(0.05);
    let empty = std::mem::size_of::<DDSketchAccumulator>();
    assert_eq!(updater.memory_usage_bytes(), empty);
    for value in [1.0, 1.5, 2.0, 4.0] {
        updater.update_single(value, 0).unwrap();
    }
    assert_eq!(updater.memory_usage_bytes(), empty + 128 * 8);
    assert_eq!(
        updater.memory_usage_bytes(),
        updater.acc.inner.memory_bytes()
    );
}

/// A numeric sketch takes an Int64 as its exact Float64, and refuses
/// one beyond 2^53, which no Float64 holds exactly.
#[test]
fn a_numeric_sketch_takes_exact_int64_values() {
    use crate::values::Value;
    let mut updater = KllAccumulatorUpdater::new(200);
    for value in 1..=5i64 {
        updater.update_value(&Value::Int64(value), 0).unwrap();
    }
    assert!(updater
        .update_value(&Value::Int64((1 << 53) + 1), 0)
        .is_err());
    assert!(updater.update_value(&Value::Utf8("x".into()), 0).is_err());
    let median = updater
        .take_accumulator()
        .estimate(&planner_types::summary_type::SketchStatistic::Quantile { q: 0.5 })
        .unwrap();
    assert_eq!(median, 3.0);
}

// An HLL build takes Int64 ids and Utf8 names as items.
#[test]
fn an_hll_takes_int64_and_utf8_items() {
    use crate::values::Value;
    let mut updater = HllUpdater {
        acc: HllSketchAccumulator::new(12).unwrap(),
    };
    for id in 0..100i64 {
        updater.update_value(&Value::Int64(id % 50), 0).unwrap();
        updater
            .update_value(&Value::Utf8(format!("n{}", id % 10).into()), 0)
            .unwrap();
    }
    let estimate = updater
        .take_accumulator()
        .estimate(&planner_types::summary_type::SketchStatistic::Cardinality)
        .unwrap();
    assert_eq!(estimate, 60.0);
}
