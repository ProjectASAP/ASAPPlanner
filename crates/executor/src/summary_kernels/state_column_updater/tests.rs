use super::reorder_buffer::HELD_SAMPLE_BYTES;
use super::*;
use planner_types::summary_type::{CounterReadout, ExactKind, ExactParams};

// An exact updater charges the buffer it holds (its capacity); its sealed
// column charges the records at their family's size, without the slack.
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
        // Sealed, a sum is a 24-byte record in a buffer of the column's
        // length.
        assert_eq!(
            updater.seal().charged_state_bytes(),
            groups * 24,
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
    let read = |group: usize| states[group].evaluation(None, None).unwrap().unwrap();
    assert_eq!(read(0), f64::INFINITY);
    assert!(read(1).is_nan());
    let rate = FieldDataType::ExactAggregate(ExactKind::Counter, ExactParams::Counter);
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
    let family = FieldDataType::ExactAggregate(ExactKind::Counter, ExactParams::Counter);
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

/// One state column of `groups` series with 15 samples each (a 15 s window
/// at one sample per second), groups added one at a time as ingestion adds
/// them: the open updater, before sealing.
fn ten_thousand_series(
    family: &FieldDataType,
    parameters: Option<&SketchParams>,
) -> StateColumnUpdater {
    let mut updater = StateColumnUpdater::new(family, parameters).unwrap();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    for group in 0..10_000u32 {
        assert_eq!(updater.add_group().unwrap(), group);
        let mut counter = 0.0;
        for second in 0..15i64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let sample = (seed % 100_000) as f64 / 7.0;
            counter += sample;
            let value = if matches!(family, FieldDataType::ExactAggregate(ExactKind::Counter, _)) {
                counter
            } else {
                sample
            };
            updater.update(group, value, 1_000 * second).unwrap();
        }
    }
    updater
}

// At 10k series and 15 samples per window, a sealed column answers every
// group exactly as the open states do, and holds less: a KLL drops its
// unused levels and buffer slack, an exact record its vector slack and the
// bytes its family does not use.
#[test]
fn sealed_ten_thousand_series_answer_alike_and_hold_less() {
    use planner_types::summary_type::{SketchAlgorithm, SketchStatistic};
    let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let k132 = SketchParams::Kll { k: 132 };
    let updater = ten_thousand_series(&kll, Some(&k132));
    let open_bytes = updater.memory_bytes();
    let GroupStates::Sketches(open) = &updater.states else {
        unreachable!()
    };
    let quantiles = [0.0, 0.1, 0.5, 0.9, 0.99, 1.0];
    let answers = |state: &dyn AggregateCore| {
        quantiles
            .iter()
            .map(|&q| {
                state
                    .estimate(&SketchStatistic::Quantile { q })
                    .unwrap()
                    .to_bits()
            })
            .collect::<Vec<_>>()
    };
    let expected = open
        .iter()
        .map(|state| answers(state.snapshot_accumulator().as_ref()))
        .collect::<Vec<_>>();
    let sealed = updater.seal();
    for (group, expected) in expected.iter().enumerate() {
        assert_eq!(
            &answers(sealed.state(group).as_ref()),
            expected,
            "group {group}"
        );
    }
    let sealed_bytes = sealed.charged_state_bytes();
    println!("KLL k=132, 10k series x 15 samples: open {open_bytes} B, sealed {sealed_bytes} B");
    assert!(
        sealed_bytes * 2 < open_bytes,
        "{sealed_bytes} vs {open_bytes}"
    );
    assert!(sealed_bytes <= 10_000 * 384, "{sealed_bytes}");

    for (kind, parameters, record_bytes) in [
        (ExactKind::Max, ExactParams::Max, 8),
        (ExactKind::Counter, ExactParams::Counter, 48),
    ] {
        let family = FieldDataType::ExactAggregate(kind, parameters);
        let updater = ten_thousand_series(&family, None);
        let open_bytes = updater.memory_bytes();
        let GroupStates::Exact { states: open, .. } = &updater.states else {
            unreachable!()
        };
        // A counter answers both readouts; any other kind none.
        let readouts = match kind {
            ExactKind::Counter => vec![Some(CounterReadout::Rate), Some(CounterReadout::Increase)],
            _ => vec![None],
        };
        let read = |state: &ScalarState| {
            (
                readouts
                    .iter()
                    .map(|&readout| format!("{:?}", state.evaluation(readout, Some((0, 15_000)))))
                    .collect::<Vec<_>>(),
                state.insufficient_counter_raw_data_samples(),
                format!("{state:?}"),
            )
        };
        let expected = open.iter().map(read).collect::<Vec<_>>();
        let sealed = updater.seal();
        let crate::values::StateStorage::Exact(records) = sealed.storage() else {
            unreachable!()
        };
        for (group, expected) in expected.iter().enumerate() {
            assert_eq!(
                &read(&records.get(group)),
                expected,
                "{kind:?} group {group}"
            );
        }
        let sealed_bytes = sealed.charged_state_bytes();
        println!("{kind:?}, 10k series: open {open_bytes} B, sealed {sealed_bytes} B");
        // Each record in its family's bytes plus a validity bit, no slack.
        let records_bytes = 10_000 * record_bytes;
        assert!(
            (records_bytes..=records_bytes + 10_000 / 8 + 64).contains(&sealed_bytes),
            "{kind:?}: {sealed_bytes}"
        );
        assert!(sealed_bytes < open_bytes, "{kind:?}");
    }
}

// The planner prices each exact kind's state at the bytes its sealed
// column holds per group, the validity bit of min, max and counters
// counted as a byte: within one byte per group of what it holds.
#[test]
fn the_planner_prices_each_sealed_exact_state_at_its_size() {
    let groups = 4_096;
    for (kind, parameters) in [
        (ExactKind::Sum, ExactParams::Sum),
        (ExactKind::Count, ExactParams::Count),
        (ExactKind::Min, ExactParams::Min),
        (ExactKind::Max, ExactParams::Max),
        (ExactKind::Counter, ExactParams::Counter),
        (ExactKind::IRate, ExactParams::IRate),
    ] {
        let family = FieldDataType::ExactAggregate(kind, parameters);
        let mut updater = StateColumnUpdater::new(&family, None).unwrap();
        updater.ensure_groups(groups).unwrap();
        let held = updater.seal().charged_state_bytes();
        let priced = groups * kind.sealed_state_bytes() as usize;
        assert!(
            held <= priced && priced < held + groups,
            "{kind:?}: {held} held, {priced} priced"
        );
    }
}

// A counter that resets keeps its reset-corrected increase through
// sealing: 10, 20, then a reset to 5, then 15 is an increase of 25 (10 to
// 20, then 5 after the reset, then 10), with four samples, read back
// alike from the sealed record; a group with one sample still has too few.
#[test]
fn sealed_counter_records_keep_resets() {
    let family = FieldDataType::ExactAggregate(ExactKind::Counter, ExactParams::Counter);
    let mut updater = StateColumnUpdater::new(&family, None).unwrap();
    updater.ensure_groups(3).unwrap();
    for (second, value) in [(0, 10.0), (1, 20.0), (2, 5.0), (3, 15.0)] {
        updater.update(0, value, 1_000 * second).unwrap();
    }
    updater.update(1, 7.0, 0).unwrap();
    let GroupStates::Exact { states: open, .. } = &updater.states else {
        unreachable!()
    };
    let expected = open.clone();
    let sealed = updater.seal();
    let crate::values::StateStorage::Exact(records) = sealed.storage() else {
        unreachable!()
    };
    let ScalarState::Counter(Some(counter)) = records.get(0) else {
        panic!("group 0 has a counter")
    };
    assert_eq!(counter.total_increase, 25.0);
    assert_eq!(counter.sample_count, 4);
    for (group, expected) in expected.iter().enumerate() {
        assert_eq!(format!("{:?}", records.get(group)), format!("{expected:?}"));
        assert_eq!(
            records.get(group).insufficient_counter_raw_data_samples(),
            expected.insufficient_counter_raw_data_samples()
        );
    }
    assert!(records.get(1).insufficient_counter_raw_data_samples());
    assert!(matches!(records.get(2), ScalarState::Counter(None)));
}

/// Sealing shrinks each DDSketch to its occupied buckets (stores grow 128
/// slots at a time): the same quantiles, less memory.
#[test]
fn sealed_ddsketch_windows_answer_alike_and_hold_less() {
    use planner_types::summary_type::{SketchAlgorithm, SketchStatistic};
    let ddsketch = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
    let alpha = SketchParams::DDSketch { alpha: 0.05 };
    let updater = ten_thousand_series(&ddsketch, Some(&alpha));
    let open_bytes = updater.memory_bytes();
    let GroupStates::Sketches(open) = &updater.states else {
        unreachable!()
    };
    let quantiles = [0.0, 0.1, 0.5, 0.9, 0.99, 1.0];
    let answers = |state: &dyn AggregateCore| {
        quantiles
            .iter()
            .map(|&q| {
                state
                    .estimate(&SketchStatistic::Quantile { q })
                    .unwrap()
                    .to_bits()
            })
            .collect::<Vec<_>>()
    };
    let expected = open
        .iter()
        .map(|state| answers(state.snapshot_accumulator().as_ref()))
        .collect::<Vec<_>>();
    let sealed = updater.seal();
    for (group, expected) in expected.iter().enumerate() {
        assert_eq!(
            &answers(sealed.state(group).as_ref()),
            expected,
            "group {group}"
        );
    }
    let sealed_bytes = sealed.charged_state_bytes();
    println!("DDSketch alpha=0.05, 10k series x 15 samples: open {open_bytes} B, sealed {sealed_bytes} B");
    assert!(
        sealed_bytes * 2 < open_bytes,
        "{sealed_bytes} vs {open_bytes}"
    );
}

/// Sealing a DDSketch column stores each state in its view layout
/// (`DdSketch::to_view_bytes`), the layout a stored window is priced at:
/// the column holds the views, each padded to 8 bytes, and one 4-byte
/// offset per state plus one; every state answers and merges as the open
/// sketch it was sealed from.
#[test]
fn a_sealed_ddsketch_column_stores_view_layouts() {
    use crate::values::StateStorage;
    use planner_types::summary_type::{SketchAlgorithm, SketchStatistic};
    let ddsketch = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
    let alpha = SketchParams::DDSketch { alpha: 0.01 };
    let updater = ten_thousand_series(&ddsketch, Some(&alpha));
    let GroupStates::Sketches(open) = &updater.states else {
        unreachable!()
    };
    let open: Vec<_> = open
        .iter()
        .map(|state| state.snapshot_accumulator())
        .collect();
    let view_bytes: Vec<Vec<u8>> = open
        .iter()
        .map(|state| crate::values::state_view_bytes(state.as_ref()).unwrap())
        .collect();
    let sealed = updater.seal();
    assert!(matches!(sealed.storage(), StateStorage::Mapped(_)));
    assert!(!sealed.is_held_outside_run());
    let expected_bytes: usize = view_bytes
        .iter()
        .map(|bytes| bytes.len().div_ceil(8) * 8)
        .sum::<usize>()
        + 4 * (open.len() + 1);
    assert_eq!(sealed.state_memory_bytes(), expected_bytes);
    assert_eq!(sealed.charged_state_bytes(), expected_bytes);
    let quantiles = [0.0, 0.01, 0.5, 0.99, 1.0];
    let answers = |state: &dyn AggregateCore| {
        quantiles.map(|q| {
            state
                .estimate(&SketchStatistic::Quantile { q })
                .unwrap()
                .to_bits()
        })
    };
    for (group, state) in open.iter().enumerate() {
        assert_eq!(
            answers(sealed.state(group).as_ref()),
            answers(state.as_ref()),
            "group {group}"
        );
    }
    println!("DDSketch alpha=0.01, 10k series x 15 samples: views {expected_bytes} B");
}
