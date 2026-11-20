//! A deployment persists a summary view by taking a kernel's state apart,
//! encoding the `asap_sketchlib` sketch with its ASAPv1 codec, and rebuilding
//! the kernel from the decoded sketch. For the HLL, weighted frequency,
//! UnivMon and Hydra-CMS kernels, the rebuilt state merges and answers as the
//! original does, and inconsistent parts are refused.
use asap_executor::summary_kernels::hll_sketch::registers::HllRegistersP12;
use asap_executor::summary_kernels::univmon::UnivMonAccumulator;
use asap_executor::summary_kernels::weighted_frequency::{
    FrequencyAlgorithm, FrequencySketch, WeightedFrequency,
};
use asap_executor::summary_kernels::{HllSketchAccumulator, HydraCms, HydraCmsGroup, Registers};
use asap_executor::values::Value;
use asap_executor::AggregateCore;
use asap_sketchlib::sketches::hll::HyperLogLogImpl;
use asap_sketchlib::{
    Classic, CountMinSketchWithHeap, CountSketch, CountSketchWithHeap, Hydra, RegularPath, UnivMon,
    Vector2D,
};
use planner_types::ir::scalar::ColumnRef;
use planner_types::summary_type::SketchStatistic;
use std::sync::Arc;

// An HLL state's registers round-trip through ASAPv1: the rebuilt state
// estimates and merges as the original.
#[test]
fn hll_registers_round_trip() {
    let (mut state, mut other) = (
        HllSketchAccumulator::new(12).unwrap(),
        HllSketchAccumulator::new(12).unwrap(),
    );
    for v in 0..2000u32 {
        state.update(&v.to_le_bytes());
        other.update(&(v + 1000).to_le_bytes());
    }
    let Registers::P12(hll) = &state.inner else {
        unreachable!("built at precision 12")
    };
    let decoded =
        HyperLogLogImpl::<Classic, HllRegistersP12>::from_asapv1(&hll.to_asapv1().unwrap())
            .unwrap();
    let rebuilt = HllSketchAccumulator {
        inner: Registers::P12(decoded),
    };
    assert_eq!(rebuilt.registers(), state.registers());
    let cardinality = |s: &dyn AggregateCore| s.estimate(&SketchStatistic::Cardinality).unwrap();
    assert_eq!(cardinality(&rebuilt), cardinality(&state));
    assert_eq!(
        cardinality(rebuilt.merge_with(&other).unwrap().as_ref()),
        cardinality(state.merge_with(&other).unwrap().as_ref()),
    );
}

fn filled_weighted_frequency(algorithm: FrequencyAlgorithm, offset: i64) -> WeightedFrequency {
    let mut state = WeightedFrequency::new(algorithm, 256, 5, 8).unwrap();
    for key in 0..40i64 {
        let weight = 1.0 + ((key * 7 + offset) % 13) as f64 * 0.5;
        state.update(&[Value::Int64(key)], weight).unwrap();
    }
    state
}

fn read(state: &WeightedFrequency) -> (Vec<f64>, String) {
    (state.counters().to_vec(), format!("{:?}", state.rows(8)))
}

// Count-Min and Count Sketch states with a top-k heap round-trip through
// ASAPv1: the rebuilt state's counters, top-k rows and merges equal the
// original's.
#[test]
fn weighted_frequency_sketch_round_trips() {
    for algorithm in [FrequencyAlgorithm::Cms, FrequencyAlgorithm::CountSketch] {
        let state = filled_weighted_frequency(algorithm, 0);
        let other = filled_weighted_frequency(algorithm, 3);
        let decoded = match state.sketch() {
            FrequencySketch::CmsHeap(s) => FrequencySketch::CmsHeap(
                CountMinSketchWithHeap::<Vector2D<f64>, RegularPath>::from_asapv1(
                    &s.to_asapv1().unwrap(),
                )
                .unwrap(),
            ),
            FrequencySketch::CountSketchHeap(s) => FrequencySketch::CountSketchHeap(
                CountSketchWithHeap::<Vector2D<f64>, RegularPath>::from_asapv1(
                    &s.to_asapv1().unwrap(),
                )
                .unwrap(),
            ),
            _ => unreachable!("built with a heap"),
        };
        let rebuilt = WeightedFrequency::from_sketch(decoded).unwrap();
        assert_eq!(rebuilt.algorithm(), state.algorithm());
        assert_eq!(rebuilt.shape(), state.shape());
        assert_eq!(read(&rebuilt), read(&state));
        assert_eq!(
            read(&rebuilt.merge(&other).unwrap()),
            read(&state.merge(&other).unwrap())
        );
    }
}

// A sketch whose shape no state is built with (an even Count Sketch depth) is refused.
#[test]
fn weighted_frequency_refuses_an_even_count_sketch_depth() {
    let sketch = FrequencySketch::CountSketch(
        CountSketch::<Vector2D<f64>, RegularPath>::with_dimensions(2, 64),
    );
    assert!(WeightedFrequency::from_sketch(sketch).is_err());
}

// Int64 identities only: ASAPv1 encodes heaps whose keys are all one
// variant, and an Int64 is kept as key bytes, a sample as a `u64`. Twelve
// identities fit every heap, so a merge evicts none: `UnivMon::merge` offers
// the candidates in hash-set order, which decides which of tied keys an
// eviction keeps, so merges with evictions differ run to run.
fn filled_univmon(offset: i64) -> UnivMonAccumulator {
    let mut state = UnivMonAccumulator::new(16, 3, 256, 4).unwrap();
    for v in 0..500i64 {
        state
            .insert_value(&Value::Int64((v * 31 + offset) % 12))
            .unwrap();
    }
    state
}

// A UnivMon state round-trips through ASAPv1 with its key bookkeeping: the
// rebuilt state answers count, distinct, L2 and entropy, alone and merged,
// as the original (up to float summation order), and charges the same memory.
#[test]
fn univmon_parts_round_trip() {
    let (state, other) = (filled_univmon(0), filled_univmon(5));
    let decoded = UnivMon::from_asapv1(&state.sketch().to_asapv1().unwrap()).unwrap();
    let rebuilt =
        UnivMonAccumulator::from_parts(decoded, state.max_key_bytes(), state.inserted_key_bytes())
            .unwrap();
    let statistics = [
        SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        SketchStatistic::Cardinality,
        SketchStatistic::FrequencyL2,
        SketchStatistic::FrequencyEntropy,
    ];
    let answers = |s: &dyn AggregateCore| {
        statistics
            .iter()
            .map(|q| s.estimate(q).unwrap())
            .collect::<Vec<_>>()
    };
    // Equal up to float summation order: the decoded heaps may list their
    // keys in another order, so the entropy can differ in its last bits.
    let assert_same = |left: Vec<f64>, right: Vec<f64>| {
        for (l, r) in left.iter().zip(&right) {
            assert!((l - r).abs() <= 1e-12 * r.abs(), "{left:?} vs {right:?}");
        }
    };
    assert_same(answers(&rebuilt), answers(&state));
    assert_same(
        answers(rebuilt.merge_with(&other).unwrap().as_ref()),
        answers(state.merge_with(&other).unwrap().as_ref()),
    );
    assert_eq!(rebuilt.approx_memory_bytes(), state.approx_memory_bytes());
}

// Key bookkeeping no insertions give is refused.
#[test]
fn univmon_refuses_inconsistent_key_bytes() {
    let sketch = || {
        UnivMonAccumulator::new(16, 3, 256, 4)
            .unwrap()
            .sketch()
            .clone()
    };
    assert!(UnivMonAccumulator::from_parts(sketch(), 10, 5).is_err());
    assert!(UnivMonAccumulator::from_parts(sketch(), 0, 5).is_err());
    assert!(UnivMonAccumulator::from_parts(sketch(), 0, 0).is_ok());
}

fn filled_hydra(offset: i32) -> HydraCms {
    let mut grid = HydraCms::new(3, 64, 128, 3).unwrap();
    for i in 0..300 {
        let group = format!("g{}", i % 7);
        let item = format!("i{}", (i * 13 + offset) % 25);
        grid.count(&group, &item, 1 + i % 3).unwrap();
    }
    grid
}

// A Hydra-CMS grid round-trips through ASAPv1 with its total: the rebuilt
// group states answer point counts, alone and merged, as the original.
#[test]
fn hydra_cms_parts_round_trip() {
    let (grid, other) = (filled_hydra(0), filled_hydra(4));
    let decoded = Hydra::from_asapv1(&grid.sketch().to_asapv1().unwrap()).unwrap();
    let rebuilt = HydraCms::from_parts(decoded, grid.total()).unwrap();
    assert_eq!(rebuilt.shape(), grid.shape());
    assert_eq!(rebuilt.total(), grid.total());
    let group = |grid: HydraCms| HydraCmsGroup {
        grid: Arc::new(grid),
        group: "g3".into(),
    };
    let (rebuilt, original, other) = (group(rebuilt), group(grid), group(other));
    let points = |s: &dyn AggregateCore| {
        (0..25)
            .map(|i| {
                s.estimate(&SketchStatistic::PointCount {
                    key: ColumnRef::SampleValue,
                    value: Some(format!("i{i}")),
                })
                .unwrap()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(points(&rebuilt), points(&original));
    assert_eq!(
        points(rebuilt.merge_with(&other).unwrap().as_ref()),
        points(original.merge_with(&other).unwrap().as_ref()),
    );
}

// A total outside the i32 guard is refused.
#[test]
fn hydra_cms_refuses_a_total_outside_i32() {
    let sketch = || HydraCms::new(2, 8, 16, 3).unwrap().sketch().clone();
    assert!(HydraCms::from_parts(sketch(), -1).is_err());
    assert!(HydraCms::from_parts(sketch(), i64::from(i32::MAX) + 1).is_err());
    assert!(HydraCms::from_parts(sketch(), 0).is_ok());
}

// Heap keys the kernel never retains (a `u64` key, bytes that are no tuple
// key) are refused, rather than panicking when the rows are read.
#[test]
fn weighted_frequency_refuses_foreign_heap_keys() {
    use asap_sketchlib::DataInput;
    for key in [DataInput::U64(5), DataInput::Bytes(&[0xff, 0x00, 0x07])] {
        let mut sketch = CountMinSketchWithHeap::<Vector2D<f64>, RegularPath>::new(3, 64, 4);
        sketch.heap_mut().update(&key, 1.0);
        assert!(WeightedFrequency::from_sketch(FrequencySketch::CmsHeap(sketch)).is_err());
    }
}
