//! A deployment persists a summary view by reading each sketch kernel's
//! `asap_sketchlib` state with `sketch()`, encoding it with the library's
//! ASAPv1 codec, and rebuilding the kernel with `restore_from_disk`. For every
//! sketch kernel, the restored state (empty, filled, or itself a merge)
//! answers and merges as the original does, and states the kernel never
//! builds are refused.
use asap_executor::summary_kernels::hll_sketch::hll_registers::HllRegistersP12;
use asap_executor::summary_kernels::univmon::UnivMonAccumulator;
use asap_executor::summary_kernels::weighted_frequency::{
    FrequencyAlgorithm, FrequencySketch, WeightedFrequency,
};
use asap_executor::summary_kernels::{
    CountMinSketchAccumulator, DDSketchAccumulator, DatasketchesKLLAccumulator, HllRegisters,
    HllSketchAccumulator, HydraCms, HydraCmsGroup, HydraKllSketchAccumulator, KmvAccumulator,
};
use asap_executor::values::Value;
use asap_executor::AggregateCore;
use asap_sketchlib::input::HydraCounter;
use asap_sketchlib::sketches::hll::HyperLogLogImpl;
use asap_sketchlib::{
    Classic, CountMinF64, CountMinSketch, CountMinSketchWithHeap, CountSketch, CountSketchWithHeap,
    DataInput, DdSketch, FastPath, Hydra, Kll, Kmv, RegularPath, UnivMon, Vector2D,
};
use planner_types::ir::scalar::ColumnRef;
use planner_types::summary_type::SketchStatistic;
use std::sync::Arc;

type Answers = Vec<String>;

/// An answer or the fact that it failed, compared as text; floats to 11
/// significant digits, since a decoded UnivMon may sum its heap in another
/// order and differ in the last bits.
fn answer<E>(result: Result<f64, E>) -> String {
    match result {
        Ok(value) => format!("{value:.10e}"),
        Err(_) => "error".into(),
    }
}

/// Restores an empty state, `state`, and `state` merged with `other` through
/// `restore`, and checks that each restored state answers, and merged with
/// `other` answers, as its source does.
fn assert_restores<T: AggregateCore + Clone + 'static>(
    empty: T,
    state: T,
    other: T,
    restore: impl Fn(&T) -> T,
    answers: impl Fn(&dyn AggregateCore) -> Answers,
) {
    let merged = state.merge_with(&other).unwrap();
    let merged = merged.as_any().downcast_ref::<T>().unwrap().clone();
    for source in [empty, state, merged] {
        let restored = restore(&source);
        assert_eq!(answers(&restored), answers(&source));
        assert_eq!(
            answers(restored.merge_with(&other).unwrap().as_ref()),
            answers(source.merge_with(&other).unwrap().as_ref()),
        );
    }
}

fn statistics(state: &dyn AggregateCore, queries: &[SketchStatistic]) -> Answers {
    queries.iter().map(|q| answer(state.estimate(q))).collect()
}

fn quantiles() -> Vec<SketchStatistic> {
    [0.1, 0.5, 0.9, 1.0]
        .map(|q| SketchStatistic::Quantile { q })
        .to_vec()
}

fn bare_count() -> SketchStatistic {
    SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: None,
    }
}

// HLL registers round-trip through ASAPv1.
#[test]
fn hll_restores() {
    let filled = |offset: u32| {
        let mut state = HllSketchAccumulator::new(12).unwrap();
        for v in 0..2000u32 {
            state.update(&(v + offset).to_le_bytes());
        }
        state
    };
    assert_restores(
        HllSketchAccumulator::new(12).unwrap(),
        filled(0),
        filled(1000),
        |state| {
            let HllRegisters::P12(hll) = state.sketch() else {
                unreachable!("built at precision 12")
            };
            let bytes = hll.to_asapv1().unwrap();
            let decoded = HyperLogLogImpl::<Classic, HllRegistersP12>::from_asapv1(&bytes);
            HllSketchAccumulator::restore_from_disk(HllRegisters::P12(decoded.unwrap())).unwrap()
        },
        |state| statistics(state, &[SketchStatistic::Cardinality]),
    );
}

// DDSketch round-trips through ASAPv1.
#[test]
fn ddsketch_restores() {
    let filled = |offset: f64| {
        let mut sketch = DdSketch::new(0.01);
        for v in 1..=200 {
            sketch.add(&(f64::from(v) + offset));
        }
        DDSketchAccumulator::restore_from_disk(sketch).unwrap()
    };
    assert_restores(
        DDSketchAccumulator::new(0.01),
        filled(0.0),
        filled(50.0),
        |state| {
            let bytes = state.sketch().to_asapv1().unwrap();
            DDSketchAccumulator::restore_from_disk(DdSketch::from_asapv1(&bytes).unwrap()).unwrap()
        },
        |state| statistics(state, &quantiles()),
    );
}

// KLL round-trips through ASAPv1. `k` exceeds every sample count, so no
// compaction (seeded from the wall clock) makes merges differ.
#[test]
fn kll_restores() {
    let filled = |offset: f64| {
        let mut state = DatasketchesKLLAccumulator::new(1000);
        for v in 0..150 {
            state.update(f64::from(v) + offset);
        }
        state
    };
    assert_restores(
        DatasketchesKLLAccumulator::new(1000),
        filled(0.0),
        filled(40.0),
        |state| {
            let bytes = state.sketch().to_asapv1().unwrap();
            let decoded = Kll::<f64>::from_asapv1(&bytes).unwrap();
            DatasketchesKLLAccumulator::restore_from_disk(decoded).unwrap()
        },
        |state| statistics(state, &quantiles()),
    );
}

// Count-Min round-trips through ASAPv1.
#[test]
fn count_min_restores() {
    let filled = |offset: usize| {
        let mut state = CountMinSketchAccumulator::new(3, 128).unwrap();
        for v in 0..100 {
            state.update(&format!("k{}", (v + offset) % 30), 0.5 + v as f64);
        }
        state
    };
    let answers = |state: &dyn AggregateCore| {
        let cms = state
            .as_any()
            .downcast_ref::<CountMinSketchAccumulator>()
            .unwrap();
        let mut answers = statistics(state, &[bare_count()]);
        answers.extend((0..30).map(|k| answer::<()>(Ok(cms.query_key(&[format!("k{k}")])))));
        answers
    };
    assert_restores(
        CountMinSketchAccumulator::new(3, 128).unwrap(),
        filled(0),
        filled(7),
        |state| {
            let bytes = state.sketch().to_asapv1().unwrap();
            let decoded = CountMinF64::from_asapv1(&bytes).unwrap();
            CountMinSketchAccumulator::restore_from_disk(decoded).unwrap()
        },
        answers,
    );
}

// KMV round-trips through ASAPv1.
#[test]
fn kmv_restores() {
    let filled = |offset: u32| {
        let mut state = KmvAccumulator::new(64);
        for v in 0..500u32 {
            state.insert_sample(f64::from(v + offset));
        }
        state
    };
    assert_restores(
        KmvAccumulator::new(64),
        filled(0),
        filled(250),
        |state| {
            let bytes = state.sketch().to_asapv1().unwrap();
            KmvAccumulator::restore_from_disk(Kmv::from_asapv1(&bytes).unwrap()).unwrap()
        },
        |state| statistics(state, &[SketchStatistic::Cardinality]),
    );
}

// A HydraKLL grid round-trips through ASAPv1 (KLL cells with `k` above every
// cell's sample count, so nothing compacts).
#[test]
fn hydra_kll_restores() {
    let filled = |offset: f64| {
        let mut state = HydraKllSketchAccumulator::new(3, 16, 1000);
        for v in 0..300 {
            state.update(&[format!("g{}", v % 5)], f64::from(v) + offset);
        }
        state
    };
    let answers = |state: &dyn AggregateCore| {
        let hydra = state
            .as_any()
            .downcast_ref::<HydraKllSketchAccumulator>()
            .unwrap();
        (0..5)
            .flat_map(|g| [0.1, 0.5, 0.9].map(move |q| (g, q)))
            .map(|(g, q)| format!("{:.10e}", hydra.query_key(&[format!("g{g}")], q)))
            .collect()
    };
    assert_restores(
        HydraKllSketchAccumulator::new(3, 16, 1000),
        filled(0.0),
        filled(0.5),
        |state| {
            let bytes = state.sketch().to_asapv1().unwrap();
            let decoded = Hydra::from_asapv1(&bytes).unwrap();
            HydraKllSketchAccumulator::restore_from_disk(decoded).unwrap()
        },
        answers,
    );
}

fn weighted_frequency(
    algorithm: FrequencyAlgorithm,
    capacity: usize,
    offset: i64,
) -> WeightedFrequency {
    let mut state = WeightedFrequency::new(algorithm, 256, 5, capacity).unwrap();
    for key in 0..40i64 {
        let weight = 1.0 + ((key * 7 + offset) % 13) as f64 * 0.5;
        state.update(&[Value::Int64(key)], weight).unwrap();
    }
    state
}

fn encode_decode(sketch: &FrequencySketch) -> FrequencySketch {
    type M = Vector2D<f64>;
    match sketch {
        FrequencySketch::Cms(s) => FrequencySketch::Cms(
            CountMinSketch::<M, RegularPath>::from_asapv1(&s.to_asapv1().unwrap()).unwrap(),
        ),
        FrequencySketch::CountSketch(s) => FrequencySketch::CountSketch(
            CountSketch::<M, RegularPath>::from_asapv1(&s.to_asapv1().unwrap()).unwrap(),
        ),
        FrequencySketch::CmsHeap(s) => FrequencySketch::CmsHeap(
            CountMinSketchWithHeap::<M, RegularPath>::from_asapv1(&s.to_asapv1().unwrap()).unwrap(),
        ),
        FrequencySketch::CountSketchHeap(s) => FrequencySketch::CountSketchHeap(
            CountSketchWithHeap::<M, RegularPath>::from_asapv1(&s.to_asapv1().unwrap()).unwrap(),
        ),
    }
}

// Every weighted frequency variant (Count-Min and Count Sketch, with and
// without a heap) round-trips through ASAPv1: counters, top-k rows and point
// estimates equal the original's.
#[test]
fn weighted_frequency_restores() {
    for algorithm in [FrequencyAlgorithm::Cms, FrequencyAlgorithm::CountSketch] {
        for capacity in [0, 8] {
            let answers = |state: &dyn AggregateCore| {
                let state = state.as_any().downcast_ref::<WeightedFrequency>().unwrap();
                let mut answers = vec![
                    format!("{:?}", state.counters()),
                    format!("{:?}", state.rows(8)),
                ];
                answers.extend((0..40).map(|k| answer(state.point_estimate(&[Value::Int64(k)]))));
                answers
            };
            assert_restores(
                WeightedFrequency::new(algorithm, 256, 5, capacity).unwrap(),
                weighted_frequency(algorithm, capacity, 0),
                weighted_frequency(algorithm, capacity, 3),
                |state| {
                    let restored =
                        WeightedFrequency::restore_from_disk(encode_decode(state.sketch()))
                            .unwrap();
                    assert_eq!(restored.algorithm(), state.algorithm());
                    assert_eq!(restored.shape(), state.shape());
                    restored
                },
                answers,
            );
        }
    }
}

// A shape no state is built with (an even Count Sketch depth), a heap of
// capacity 0 and a non-finite counter are refused.
#[test]
fn weighted_frequency_refuses_states_it_never_builds() {
    let even_depth = FrequencySketch::CountSketch(
        CountSketch::<Vector2D<f64>, RegularPath>::with_dimensions(2, 64),
    );
    assert!(WeightedFrequency::restore_from_disk(even_depth).is_err());
    let no_heap = CountMinSketchWithHeap::<Vector2D<f64>, RegularPath>::new(3, 64, 0);
    assert!(WeightedFrequency::restore_from_disk(FrequencySketch::CmsHeap(no_heap)).is_err());
    let mut infinite = CountMinSketch::<Vector2D<f64>, RegularPath>::with_dimensions(3, 64);
    infinite.as_storage_mut().as_mut_slice()[5] = f64::INFINITY;
    assert!(WeightedFrequency::restore_from_disk(FrequencySketch::Cms(infinite)).is_err());
}

// Heap keys the kernel never retains (a `u64` key, bytes that are no tuple
// key) are refused, rather than panicking when the rows are read.
#[test]
fn weighted_frequency_refuses_foreign_heap_keys() {
    for key in [DataInput::U64(5), DataInput::Bytes(&[0xff, 0x00, 0x07])] {
        let mut sketch = CountMinSketchWithHeap::<Vector2D<f64>, RegularPath>::new(3, 64, 4);
        sketch.heap_mut().update(&key, 1.0);
        assert!(WeightedFrequency::restore_from_disk(FrequencySketch::CmsHeap(sketch)).is_err());
    }
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

// A UnivMon state round-trips through ASAPv1 with its key bookkeeping: count,
// distinct, L2, entropy and the memory charge equal the original's.
#[test]
fn univmon_restores() {
    assert_restores(
        UnivMonAccumulator::new(16, 3, 256, 4).unwrap(),
        filled_univmon(0),
        filled_univmon(5),
        |state| {
            let decoded = UnivMon::from_asapv1(&state.sketch().to_asapv1().unwrap()).unwrap();
            let restored = UnivMonAccumulator::restore_from_disk(
                decoded,
                state.max_key_bytes(),
                state.inserted_key_bytes(),
            )
            .unwrap();
            assert_eq!(restored.approx_memory_bytes(), state.approx_memory_bytes());
            restored
        },
        |state| {
            let queries = [
                bare_count(),
                SketchStatistic::Cardinality,
                SketchStatistic::FrequencyL2,
                SketchStatistic::FrequencyEntropy,
            ];
            statistics(state, &queries)
        },
    );
}

// Key bookkeeping no insertions give, and a shape the executor never builds
// (more rows than ASAPv1 stores), are refused.
#[test]
fn univmon_refuses_inconsistent_parts() {
    let sketch = || {
        UnivMonAccumulator::new(16, 3, 256, 4)
            .unwrap()
            .sketch()
            .clone()
    };
    assert!(UnivMonAccumulator::restore_from_disk(sketch(), 10, 5).is_err());
    assert!(UnivMonAccumulator::restore_from_disk(sketch(), 0, 5).is_err());
    assert!(UnivMonAccumulator::restore_from_disk(sketch(), 0, 0).is_ok());
    let too_deep = UnivMon::new(16, asap_sketchlib::MATRIX_MAX_ROWS + 1, 2, 4);
    assert!(UnivMonAccumulator::restore_from_disk(too_deep, 0, 0).is_err());
}

// A terminal-mode UnivMon (updated with `update_weighted_terminal`) is refused:
// the kernel's next insert or merge would panic on it.
#[test]
fn univmon_refuses_a_terminal_mode_sketch() {
    let mut sketch = UnivMon::new(16, 3, 256, 4);
    sketch.update_weighted_terminal(&DataInput::U64(1), 1);
    assert!(UnivMonAccumulator::restore_from_disk(sketch, 0, 0).is_err());
}

// A retained key longer than the recorded longest key, or byte keys with
// zero recorded bytes, are refused.
#[test]
fn univmon_refuses_key_bytes_below_the_retained_keys() {
    let mut state = UnivMonAccumulator::new(16, 3, 256, 4).unwrap();
    state
        .insert_value(&Value::Utf8("a-long-identity".into()))
        .unwrap();
    let sketch = state.sketch().clone();
    assert!(UnivMonAccumulator::restore_from_disk(sketch.clone(), 1, 100).is_err());
    assert!(UnivMonAccumulator::restore_from_disk(sketch.clone(), 0, 0).is_err());
    assert!(UnivMonAccumulator::restore_from_disk(
        sketch,
        state.max_key_bytes(),
        state.inserted_key_bytes()
    )
    .is_ok());
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

// A Hydra-CMS grid round-trips through ASAPv1: the group rows sharing the
// restored grid answer point counts as the original's.
#[test]
fn hydra_cms_restores() {
    let group = |grid: HydraCms| HydraCmsGroup {
        grid: Arc::new(grid),
        group: "g3".into(),
    };
    assert_restores(
        group(HydraCms::new(3, 64, 128, 3).unwrap()),
        group(filled_hydra(0)),
        group(filled_hydra(4)),
        |row| {
            let decoded = Hydra::from_asapv1(&row.grid.sketch().to_asapv1().unwrap()).unwrap();
            let restored = HydraCms::restore_from_disk(decoded).unwrap();
            assert_eq!(restored.shape(), row.grid.shape());
            group(restored)
        },
        |state| {
            let points = (0..25).map(|i| SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value: Some(format!("i{i}")),
            });
            statistics(state, &points.collect::<Vec<_>>())
        },
    );
}

// The i32 guard counts the restored grid's weight: a grid holding 600 is not
// allowed another i32::MAX - 500.
#[test]
fn hydra_cms_restored_total_is_the_grids() {
    let mut grid = HydraCms::new(2, 8, 16, 3).unwrap();
    grid.count("g", "i", 600).unwrap();
    let mut restored = HydraCms::restore_from_disk(grid.sketch().clone()).unwrap();
    assert!(restored.count("g", "i", i32::MAX - 500).is_err());
}

// Grids `HydraCms::new` never builds are refused: a cell of another shape
// than the prototype, a cell other than Count-Min, key columns other than
// the one group column, and a zero dimension.
#[test]
fn hydra_cms_refuses_grids_it_never_builds() {
    let cms = || {
        HydraCounter::CountMinSketch(CountMinSketch::<Vector2D<i32>, FastPath>::with_dimensions(
            3, 16,
        ))
    };
    let mut other_shape = HydraCms::new(2, 8, 16, 3).unwrap().sketch().clone();
    *other_shape.cells.get_mut(1, 1).unwrap() =
        HydraCounter::CountMinSketch(CountMinSketch::<Vector2D<i32>, FastPath>::with_dimensions(
            3, 32,
        ));
    assert!(HydraCms::restore_from_disk(other_shape).is_err());
    let kll_cells =
        Hydra::with_schema(2, 8, ["group"], HydraCounter::Kll(Kll::with_k(200))).unwrap();
    assert!(HydraCms::restore_from_disk(kll_cells).is_err());
    let other_key = Hydra::with_schema(2, 8, ["label"], cms()).unwrap();
    assert!(HydraCms::restore_from_disk(other_key).is_err());
    let mut zero_rows = Hydra::with_schema(2, 8, ["group"], cms()).unwrap();
    zero_rows.rows = 0;
    assert!(HydraCms::restore_from_disk(zero_rows).is_err());
    let built = Hydra::with_schema(2, 8, ["group"], cms()).unwrap();
    assert!(HydraCms::restore_from_disk(built).is_ok());
}

// HydraKLL grids with Count-Min cells or another key column are refused.
#[test]
fn hydra_kll_refuses_grids_it_never_builds() {
    let cms = HydraCounter::CountMinSketch(
        CountMinSketch::<Vector2D<i32>, FastPath>::with_dimensions(3, 16),
    );
    let cms_cells = Hydra::with_schema(2, 8, ["key"], cms).unwrap();
    assert!(HydraKllSketchAccumulator::restore_from_disk(cms_cells).is_err());
    let kll = HydraCounter::Kll(Kll::with_k(200));
    let other_key = Hydra::with_schema(2, 8, ["group"], kll).unwrap();
    assert!(HydraKllSketchAccumulator::restore_from_disk(other_key).is_err());
}

// A KMV `k` below the kernel's minimum is refused.
#[test]
fn kmv_refuses_a_k_below_its_minimum() {
    assert!(KmvAccumulator::restore_from_disk(Kmv::new(1)).is_err());
}

// Two different 15-byte keys retained in one layer add up to 30 bytes, more
// than an `inserted_key_bytes` of 15 records, so the bookkeeping is refused.
#[test]
fn univmon_refuses_a_layer_holding_more_key_bytes_than_inserted() {
    let mut state = UnivMonAccumulator::new(16, 3, 256, 4).unwrap();
    for key in ["identity-one-aa", "identity-two-bb"] {
        state.insert_value(&Value::Utf8(key.into())).unwrap();
    }
    let longest = state.max_key_bytes();
    assert_eq!(state.inserted_key_bytes(), 2 * longest);
    let sketch = state.sketch().clone();
    assert!(UnivMonAccumulator::restore_from_disk(sketch.clone(), longest, longest).is_err());
    assert!(UnivMonAccumulator::restore_from_disk(sketch, longest, 2 * longest).is_ok());
}

// A grid whose declared rows disagree with its cells (both non-zero) is
// refused by Hydra-CMS and HydraKLL, and HydraKLL refuses a zero dimension.
#[test]
fn hydra_grids_refuse_declared_rows_other_than_their_cells() {
    let mut cms_grid = HydraCms::new(2, 8, 16, 3).unwrap().sketch().clone();
    cms_grid.rows = 3;
    assert!(HydraCms::restore_from_disk(cms_grid).is_err());
    let kll_grid = || HydraKllSketchAccumulator::new(2, 8, 200).sketch().clone();
    let mut more_rows = kll_grid();
    more_rows.rows = 3;
    assert!(HydraKllSketchAccumulator::restore_from_disk(more_rows).is_err());
    let mut zero_cols = kll_grid();
    zero_cols.cols = 0;
    assert!(HydraKllSketchAccumulator::restore_from_disk(zero_cols).is_err());
    assert!(HydraKllSketchAccumulator::restore_from_disk(kll_grid()).is_ok());
}

// Every update adds its weight to one cell per grid row, so all grid rows sum
// to the same total; a grid whose row 1 holds more than row 0 is refused,
// since the total read from row 0 would let its counters overflow.
#[test]
fn hydra_cms_refuses_grid_rows_with_different_totals() {
    let mut grid = HydraCms::new(2, 8, 16, 3).unwrap().sketch().clone();
    let HydraCounter::CountMinSketch(cell) = grid.cells.get_mut(1, 0).unwrap() else {
        unreachable!("a Count-Min grid")
    };
    cell.as_storage_mut().as_mut_slice()[0] = i32::MAX;
    assert!(HydraCms::restore_from_disk(grid).is_err());
}
