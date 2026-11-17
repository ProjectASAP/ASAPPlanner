use super::reorder_buffer::HELD_SAMPLE_BYTES;
use super::*;
use planner_types::summary_type::{ExactKind, ExactParams};

// An exact updater charges the buffer it holds (its capacity), the
// same bytes its sealed column charges.
#[test]
fn exact_memory_is_the_buffer_capacity() {
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    for groups in [1, 5, 17] {
        let mut updater = StateColumnUpdater::new(&family, None).unwrap();
        updater.ensure_groups(groups).unwrap();
        let charged = updater.memory_bytes();
        let GroupStates::Exact { states, .. } = &updater.states else {
            unreachable!()
        };
        assert_eq!(
            charged,
            states.capacity() * std::mem::size_of::<ScalarState>()
        );
        assert_eq!(
            charged,
            updater.seal().charged_state_bytes(),
            "{groups} groups"
        );
    }
}

// A state column of exact sums takes +Inf and NaN, as the exact kernel
// does (`AccumulatorUpdater`): PromQL's `sum` over `q / 0` is +Inf, over
// `0 / 0` NaN. A rate column still refuses a non-finite sample, and a
// sum or count column a stale marker, which is not a sample.
#[test]
fn an_exact_sum_column_takes_non_finite_values() {
    let sum = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let mut updater = StateColumnUpdater::new(&sum, None).unwrap();
    updater.ensure_groups(2).unwrap();
    updater.update(0, 1.0, 0).unwrap();
    updater.update(0, f64::INFINITY, 0).unwrap();
    updater.update(1, f64::NAN, 0).unwrap();
    let GroupStates::Exact { states, .. } = &updater.states else {
        unreachable!()
    };
    let read = |group: usize| {
        states[group]
            .evaluation(ExactKind::Sum, None)
            .unwrap()
            .unwrap()
    };
    assert_eq!(read(0), f64::INFINITY);
    assert!(read(1).is_nan());
    let rate = FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate);
    let mut updater = StateColumnUpdater::new(&rate, None).unwrap();
    updater.ensure_groups(1).unwrap();
    assert!(updater.update(0, f64::NAN, 0).is_err());
    let stale = f64::from_bits(crate::values::STALE_MARKER);
    let count = FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count);
    for column in [sum, count] {
        let mut updater = StateColumnUpdater::new(&column, None).unwrap();
        updater.ensure_groups(1).unwrap();
        assert!(updater.update(0, stale, 0).is_err(), "{column:?}");
    }
}

// A reordering counter charges its held-sample queues for their capacity
// until the seal or a merge frees them: with a 1 s bound, samples at 2 s
// and 1.5 s are held; a sample at 3 s applies both (at least 1 s behind)
// and is held itself; releasing everything frees the queue.
#[test]
fn held_counter_samples_are_charged_until_applied() {
    let family = FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate);
    let mut updater = StateColumnUpdater::new(&family, None)
        .unwrap()
        .with_max_out_of_order_ms(1_000);
    updater.ensure_groups(1).unwrap();
    let states = updater.memory_bytes();
    let held_bytes = |updater: &StateColumnUpdater| {
        let counter = updater.counter.as_ref().unwrap();
        counter.reorder_buffer.as_ref().unwrap().held_bytes()
    };
    updater.update(0, 2.0, 2_000).unwrap();
    updater.update(0, 1.0, 1_500).unwrap();
    assert!(held_bytes(&updater) >= 2 * HELD_SAMPLE_BYTES);
    assert_eq!(updater.memory_bytes(), states + held_bytes(&updater));
    updater.update(0, 3.0, 3_000).unwrap();
    assert_eq!(updater.memory_bytes(), states + held_bytes(&updater));
    // A duplicate of a held sample, and samples at or before an applied one.
    assert!(updater.update(0, 9.0, 3_000).is_err());
    assert!(updater.update(0, 9.0, 2_000).is_err());
    assert!(updater.update(0, 9.0, 1_000).is_err());
    updater.release_all_held();
    assert_eq!(held_bytes(&updater), 0);
    assert_eq!(updater.memory_bytes(), states);
}
