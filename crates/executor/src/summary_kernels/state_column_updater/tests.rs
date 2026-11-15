use super::*;
use planner_types::summary_type::ExactParams;

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
