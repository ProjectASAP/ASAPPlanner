use crate::accumulators::{
    CountMinSketchAccumulator, CountMinSketchWithHeapAccumulator, CountSketchAccumulator,
    CountSketchWithHeapAccumulator, DDSketchAccumulator, DatasketchesKLLAccumulator,
    HydraKllSketchAccumulator, IncreaseAccumulator, KeyedCounterState, KeyedMaxState,
    KeyedMinState, KeyedSumCountAccumulator, MaxAccumulator, MinAccumulator, SumAccumulator,
};
use crate::{AggregateCore, KeyByLabelValues, Measurement};
// Production dispatch consumes Planner SummaryAgg payloads directly. The
// config adapter below is compiled only for isolated historical kernel tests.
use crate::accumulators::hll_sketch_accumulator::HllSketchAccumulator;
use crate::accumulators::univmon_accumulator::UnivMonAccumulator;
use planner_types::post_asap::{ExactKind, SketchAlgorithm, SketchParams, SummaryFamilyType};

/// Generate the two boilerplate clone-based `AccumulatorUpdater` methods
/// for updaters whose inner `acc` field implements `Clone + AggregateCore`.
/// Not applicable to `IncreaseAccumulatorUpdater` (its `acc` is `Option<_>`
/// with non-trivial `None` handling).
macro_rules! impl_clone_accumulator_methods {
    ($acc_field:ident) => {
        fn take_accumulator(&mut self) -> Box<dyn AggregateCore> {
            let result = Box::new(self.$acc_field.clone());
            self.reset();
            result
        }

        fn snapshot_accumulator(&self) -> Box<dyn AggregateCore> {
            Box::new(self.$acc_field.clone())
        }

        fn into_accumulator(self: Box<Self>) -> Box<dyn AggregateCore> {
            // Consume the updater and MOVE the accumulator out — no clone.
            // Avoids the expensive `Clone` (a full msgpack serialize/deserialize
            // round-trip for sketch accumulators) when a pane is evicted at
            // window close.
            let this = *self;
            Box::new(this.$acc_field)
        }
    };
}

/// Shared update interface for query-time and maintenance-time accumulation.
///
/// This provides a uniform interface over all accumulator types so that the
/// worker loop doesn't need to know which concrete type it's dealing with.
pub trait AccumulatorUpdater: Send {
    /// Validate an immutable maintenance input before an updater can silently
    /// discard a value outside its representable domain.
    fn validate_single_input(&self, value: f64) -> Result<(), String> {
        if value.is_finite() {
            Ok(())
        } else {
            Err("accumulator input must be finite".into())
        }
    }

    /// Feed a single (value, timestamp_ms) pair — for SingleSubpopulation types.
    fn update_single(&mut self, value: f64, timestamp_ms: i64);

    /// Feed a keyed (key, value, timestamp_ms) triple — for MultipleSubpopulation types.
    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, timestamp_ms: i64);

    /// Extract the final accumulator as a boxed `AggregateCore`.
    fn take_accumulator(&mut self) -> Box<dyn AggregateCore>;

    /// Non-destructive read of the current accumulator state (clone without reset).
    /// Used by pane-based sliding windows to read shared panes.
    fn snapshot_accumulator(&self) -> Box<dyn AggregateCore>;

    /// Consume the updater and return its accumulator BY MOVE, avoiding the
    /// `Clone` that `take_accumulator`/`snapshot_accumulator` pay (for sketch
    /// accumulators that clone is a full msgpack serialize/deserialize
    /// round-trip). Used by `merge_panes_for_window` when a pane is evicted at
    /// window close. Default falls back to a clone for updaters that can't
    /// cheaply move their inner accumulator out.
    fn into_accumulator(self: Box<Self>) -> Box<dyn AggregateCore> {
        self.snapshot_accumulator()
    }

    /// Reset internal state for reuse (avoids re-allocation).
    fn reset(&mut self);

    /// Whether this updater is keyed (MultipleSubpopulation).
    fn is_keyed(&self) -> bool;

    /// Estimated memory usage in bytes.
    fn memory_usage_bytes(&self) -> usize;
}

// ---------------------------------------------------------------------------
// SumAccumulatorUpdater
// ---------------------------------------------------------------------------

pub struct SumAccumulatorUpdater {
    acc: SumAccumulator,
}

impl SumAccumulatorUpdater {
    pub fn new() -> Self {
        Self {
            acc: SumAccumulator::new(),
        }
    }
}

impl Default for SumAccumulatorUpdater {
    fn default() -> Self {
        Self::new()
    }
}

impl AccumulatorUpdater for SumAccumulatorUpdater {
    fn update_single(&mut self, value: f64, _timestamp_ms: i64) {
        self.acc.update(value);
    }

    fn update_keyed(&mut self, _key: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
        self.update_single(value, timestamp_ms);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = SumAccumulator::new();
    }

    fn is_keyed(&self) -> bool {
        false
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<SumAccumulator>()
    }
}

// ---------------------------------------------------------------------------
// MinAccumulatorUpdater / MaxAccumulatorUpdater
// ---------------------------------------------------------------------------

macro_rules! extremum_updater {
    ($updater:ident, $acc:ty) => {
        #[derive(Default)]
        pub struct $updater {
            acc: $acc,
        }

        impl $updater {
            pub fn new() -> Self {
                Self::default()
            }
        }

        impl AccumulatorUpdater for $updater {
            fn update_single(&mut self, value: f64, _timestamp_ms: i64) {
                self.acc.update(value);
            }

            fn update_keyed(&mut self, _key: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
                self.update_single(value, timestamp_ms);
            }

            impl_clone_accumulator_methods!(acc);

            fn reset(&mut self) {
                self.acc = <$acc>::new();
            }

            fn is_keyed(&self) -> bool {
                false
            }

            fn memory_usage_bytes(&self) -> usize {
                std::mem::size_of::<$acc>()
            }
        }
    };
}

extremum_updater!(MinAccumulatorUpdater, MinAccumulator);
extremum_updater!(MaxAccumulatorUpdater, MaxAccumulator);

// ---------------------------------------------------------------------------
// IncreaseAccumulatorUpdater
// ---------------------------------------------------------------------------

pub struct IncreaseAccumulatorUpdater {
    acc: Option<IncreaseAccumulator>,
}

impl IncreaseAccumulatorUpdater {
    pub fn new() -> Self {
        Self { acc: None }
    }
}

impl Default for IncreaseAccumulatorUpdater {
    fn default() -> Self {
        Self::new()
    }
}

impl AccumulatorUpdater for IncreaseAccumulatorUpdater {
    fn update_single(&mut self, value: f64, timestamp_ms: i64) {
        let measurement = Measurement::new(value);
        match &mut self.acc {
            Some(acc) => acc.update(measurement, timestamp_ms),
            None => {
                self.acc = Some(IncreaseAccumulator::new(
                    measurement.clone(),
                    timestamp_ms,
                    measurement,
                    timestamp_ms,
                ));
            }
        }
    }

    fn update_keyed(&mut self, _key: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
        self.update_single(value, timestamp_ms);
    }

    // Hand-written: acc is Option<_> with non-trivial None handling.
    fn take_accumulator(&mut self) -> Box<dyn AggregateCore> {
        let acc = self.acc.take().unwrap_or_else(|| {
            IncreaseAccumulator::new(Measurement::new(0.0), 0, Measurement::new(0.0), 0)
        });
        let result = Box::new(acc);
        self.reset();
        result
    }

    fn snapshot_accumulator(&self) -> Box<dyn AggregateCore> {
        match &self.acc {
            Some(acc) => Box::new(acc.clone()),
            None => Box::new(IncreaseAccumulator::new(
                Measurement::new(0.0),
                0,
                Measurement::new(0.0),
                0,
            )),
        }
    }

    fn reset(&mut self) {
        self.acc = None;
    }

    fn is_keyed(&self) -> bool {
        false
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<Option<IncreaseAccumulator>>()
    }
}

// ---------------------------------------------------------------------------
// KllAccumulatorUpdater
// ---------------------------------------------------------------------------

pub struct KllAccumulatorUpdater {
    acc: DatasketchesKLLAccumulator,
    k: u16,
}

impl KllAccumulatorUpdater {
    pub fn new(k: u16) -> Self {
        Self {
            acc: DatasketchesKLLAccumulator::new(k),
            k,
        }
    }
}

impl AccumulatorUpdater for KllAccumulatorUpdater {
    fn update_single(&mut self, value: f64, _timestamp_ms: i64) {
        self.acc.update(value);
    }

    fn update_keyed(&mut self, _key: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
        self.update_single(value, timestamp_ms);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = DatasketchesKLLAccumulator::new(self.k);
    }

    fn is_keyed(&self) -> bool {
        false
    }

    fn memory_usage_bytes(&self) -> usize {
        // KLL sketch size is hard to estimate precisely; use a rough estimate
        std::mem::size_of::<DatasketchesKLLAccumulator>() + 4096
    }
}

// ---------------------------------------------------------------------------
// DDSketchAccumulatorUpdater — pendant to KllAccumulatorUpdater
// ---------------------------------------------------------------------------
//
// Drives the agent-aggregated DDSketch path: the worker either
// (a) merges an inbound `DDSketchAccumulator` from the
// modified-OTLP `Data::Ddsketch` ingest (via the worker's
// `merge_with`), or (b) consumes raw values via `update_single`
// when an OTLP scalar datapoint matches an aggregation typed as
// DDSketch. (b) is the less common path but it lets the same
// aggregation slot serve both pre-aggregated agent sketches and
// raw OTLP gauges.
pub struct DDSketchAccumulatorUpdater {
    acc: DDSketchAccumulator,
    alpha: f64,
}

impl DDSketchAccumulatorUpdater {
    pub fn new(alpha: f64) -> Self {
        Self {
            acc: DDSketchAccumulator::new(alpha),
            alpha,
        }
    }
}

impl AccumulatorUpdater for DDSketchAccumulatorUpdater {
    fn validate_single_input(&self, value: f64) -> Result<(), String> {
        let (minimum, maximum) =
            asap_sketchlib::sketches::ddsketch::ddsketch_indexable_bounds(self.alpha);
        if value.is_finite() && value > 0.0 && value >= minimum && value <= maximum {
            Ok(())
        } else {
            Err("DDS maintenance input is outside its positive representable domain".into())
        }
    }

    fn update_single(&mut self, value: f64, _timestamp_ms: i64) {
        // sketch-core's DdSketch (the inner of DDSketchAccumulator)
        // exposes `update(f64)` for single-value ingestion. The
        // worker calls this when a raw OTLP datapoint matches an
        // aggregation typed as DDSketch — the sketch-merge path
        // uses `merge_with` directly.
        self.acc.inner.update(value);
    }

    fn update_keyed(&mut self, _key: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
        self.update_single(value, timestamp_ms);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = DDSketchAccumulator::new(self.alpha);
    }

    fn is_keyed(&self) -> bool {
        false
    }

    fn memory_usage_bytes(&self) -> usize {
        // Bucket store is variable; rough estimate matches KLL.
        std::mem::size_of::<DDSketchAccumulator>() + 4096
    }
}

// ---------------------------------------------------------------------------
// KeyedSumCountAccumulatorUpdater
// ---------------------------------------------------------------------------

pub struct KeyedSumCountAccumulatorUpdater {
    acc: KeyedSumCountAccumulator,
}

impl KeyedSumCountAccumulatorUpdater {
    pub fn new() -> Self {
        Self::for_family(ExactKind::Sum)
    }

    pub fn for_family(family: ExactKind) -> Self {
        Self {
            acc: KeyedSumCountAccumulator::for_family(family),
        }
    }
}

impl Default for KeyedSumCountAccumulatorUpdater {
    fn default() -> Self {
        Self::new()
    }
}

impl AccumulatorUpdater for KeyedSumCountAccumulatorUpdater {
    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
        self.acc.update(key.clone(), value);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = KeyedSumCountAccumulator::for_family(self.acc.family.clone());
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<KeyedSumCountAccumulator>()
            + self.acc.sums.len() * (std::mem::size_of::<KeyByLabelValues>() + 16)
    }
}

// ---------------------------------------------------------------------------
// KeyedMinStateUpdater / KeyedMaxStateUpdater
// ---------------------------------------------------------------------------

macro_rules! multiple_extremum_updater {
    ($updater:ident, $acc:ty) => {
        #[derive(Default)]
        pub struct $updater {
            acc: $acc,
        }

        impl $updater {
            pub fn new() -> Self {
                Self::default()
            }
        }

        impl AccumulatorUpdater for $updater {
            fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
                debug_assert!(
                    false,
                    "update_single called on keyed updater; use update_keyed"
                );
            }

            fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
                self.acc.update(key.clone(), value);
            }

            impl_clone_accumulator_methods!(acc);

            fn reset(&mut self) {
                self.acc = <$acc>::new();
            }

            fn is_keyed(&self) -> bool {
                true
            }

            fn memory_usage_bytes(&self) -> usize {
                std::mem::size_of::<$acc>()
                    + self.acc.values.len() * (std::mem::size_of::<KeyByLabelValues>() + 8)
            }
        }
    };
}

multiple_extremum_updater!(KeyedMinStateUpdater, KeyedMinState);
multiple_extremum_updater!(KeyedMaxStateUpdater, KeyedMaxState);

// ---------------------------------------------------------------------------
// KeyedCounterStateUpdater
// ---------------------------------------------------------------------------

pub struct KeyedCounterStateUpdater {
    acc: KeyedCounterState,
}

impl KeyedCounterStateUpdater {
    pub fn new() -> Self {
        Self {
            acc: KeyedCounterState::new(),
        }
    }
}

impl Default for KeyedCounterStateUpdater {
    fn default() -> Self {
        Self::new()
    }
}

impl AccumulatorUpdater for KeyedCounterStateUpdater {
    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
        let measurement = Measurement::new(value);
        match self.acc.increases.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().update(measurement, timestamp_ms);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(IncreaseAccumulator::new(
                    measurement.clone(),
                    timestamp_ms,
                    measurement,
                    timestamp_ms,
                ));
            }
        }
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = KeyedCounterState::new();
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<KeyedCounterState>()
            + self.acc.increases.len()
                * (std::mem::size_of::<KeyByLabelValues>()
                    + std::mem::size_of::<IncreaseAccumulator>())
    }
}

// ---------------------------------------------------------------------------
// CmsAccumulatorUpdater (CountMinSketch)
// ---------------------------------------------------------------------------

/// Keyed weighted-frequency updater.
///
/// A raw Prometheus sample represents the observed metric value, so a bare CMS
/// adds `value` for its key. Counting each received sample as one is a distinct
/// event-count operation and requires an explicit typed plan contract; it must
/// not be inferred from the sketch algorithm alone.
pub struct CmsAccumulatorUpdater {
    acc: CountMinSketchAccumulator,
    row_num: usize,
    col_num: usize,
}

impl CmsAccumulatorUpdater {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            acc: CountMinSketchAccumulator::new(row_num, col_num),
            row_num,
            col_num,
        }
    }
}

impl AccumulatorUpdater for CmsAccumulatorUpdater {
    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
        self.acc.inner.update(&key.to_semicolon_str(), value);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = CountMinSketchAccumulator::new(self.row_num, self.col_num);
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<CountMinSketchAccumulator>()
            + self.row_num * self.col_num * std::mem::size_of::<f64>()
    }
}

// ---------------------------------------------------------------------------
// CmsHeapAccumulatorUpdater — value-weighted / count-weighted top-k
// ---------------------------------------------------------------------------

/// What quantity the top-k heap ranks keys by.
///
/// These are DIFFERENT query semantics and must be chosen explicitly:
///
/// * [`TopkWeight::Value`] — accumulate **Σ of the datapoint value** per key.
///   This answers "top-k <group-by> by total <metric>" (e.g. "top-k hosts by
///   total CPU"). The heap value is the summed metric value, so the read-side
///   reducer's "sort heap descending by value" yields the correct ranking.
///
/// * [`TopkWeight::Count`] — accumulate **+1 per event** per key (occurrence
///   frequency), the textbook heavy-hitter / frequency-top-k semantics
///   ("which keys appear most often").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopkWeight {
    /// Σ datapoint value per key (value-weighted top-k).
    Value,
    /// +1 per event per key (count-weighted / frequency top-k).
    Count,
}

/// Keyed top-k updater backed by a real `CountMinSketchWithHeap` (a CMS
/// matrix PLUS a size-`heap_size` top-k heap). Unlike the heap-LESS
/// `CmsAccumulatorUpdater`, this enumerates top-k keys at read time
/// (`get_topk_keys` / `topk_heap_items`), which is what `topk(...)` queries
/// need.
///
/// The key is the configured group-by (`aggregated_labels`) value vector —
/// e.g. `host` — formed by `extract_aggregated_key_from_series` in the worker,
/// NOT the hardcoded metric label `item`. The accumulated quantity is selected
/// by [`TopkWeight`]:
///   * `Value` → `inner.update(key, value)` adds the datapoint value (Σ value).
///   * `Count` → `inner.update(key, 1.0)` adds one per event (Σ count).
///
/// Both `CountMinSketchWithHeap` and `CountSketchWithHeap` raw-input policies
/// route here; the heap is the shared distinguishing payload.
pub struct CmsHeapAccumulatorUpdater {
    acc: CountMinSketchWithHeapAccumulator,
    row_num: usize,
    col_num: usize,
    heap_size: usize,
    weight: TopkWeight,
    weight_scale: f64,
}

impl CmsHeapAccumulatorUpdater {
    pub fn new(row_num: usize, col_num: usize, heap_size: usize, weight: TopkWeight) -> Self {
        Self::with_weight_scale(row_num, col_num, heap_size, weight, 1.0)
    }

    pub fn with_weight_scale(
        row_num: usize,
        col_num: usize,
        heap_size: usize,
        weight: TopkWeight,
        weight_scale: f64,
    ) -> Self {
        Self {
            acc: CountMinSketchWithHeapAccumulator::new(row_num, col_num, heap_size),
            row_num,
            col_num,
            heap_size,
            weight,
            weight_scale,
        }
    }
}

impl AccumulatorUpdater for CmsHeapAccumulatorUpdater {
    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
        // Heap key = the group-by label-value vector (e.g. `host`), joined the
        // same way the read-side `get_topk_keys` splits it back apart (`;`).
        let weighted = match self.weight {
            // Σ value: feed the datapoint value. sketchlib's CMS-heap
            // `update(key, w)` adds `w.round()` occurrences of `key`, so the
            // heap value accumulates the (rounded) summed metric value.
            TopkWeight::Value => value * self.weight_scale,
            // Σ count: one occurrence per event, regardless of value.
            TopkWeight::Count => 1.0,
        };
        self.acc.inner.update(&key.to_semicolon_str(), weighted);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc =
            CountMinSketchWithHeapAccumulator::new(self.row_num, self.col_num, self.heap_size);
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<CountMinSketchWithHeapAccumulator>()
            + self.row_num * self.col_num * std::mem::size_of::<f64>()
            + self.heap_size * (std::mem::size_of::<asap_sketchlib::CmsHeapItem>() + 32)
    }
}

// ---------------------------------------------------------------------------
// CountSketchAccumulatorUpdater (real median-of-signed-rows CountSketch)
// ---------------------------------------------------------------------------

/// Keyed point-frequency updater backed by a real `asap_sketchlib::CountSketch`
/// (signed rows, median-of-rows estimator) — distinct math from
/// `CmsAccumulatorUpdater`'s CMS (min-of-rows). Closes, on the raw-metric
/// ingest path, the conflation bug where `SketchAlgorithm::CountSketch` silently
/// shared `CmsAccumulatorUpdater` with bare CMS.
///
/// As with bare CMS, each raw Prometheus sample contributes its `value`.
/// Unit event counting must be selected explicitly by a future typed plan
/// contract rather than being implied by `SketchAlgorithm::CountSketch`.
pub struct CountSketchAccumulatorUpdater {
    acc: CountSketchAccumulator,
    row_num: usize,
    col_num: usize,
}

impl CountSketchAccumulatorUpdater {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            acc: CountSketchAccumulator::new(row_num, col_num),
            row_num,
            col_num,
        }
    }
}

impl AccumulatorUpdater for CountSketchAccumulatorUpdater {
    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
        self.acc.inner.update(&key.to_semicolon_str(), value);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = CountSketchAccumulator::new(self.row_num, self.col_num);
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<CountSketchAccumulator>()
            + self.row_num * self.col_num * std::mem::size_of::<f64>()
    }
}

// ---------------------------------------------------------------------------
// CountSketchWithHeapAccumulatorUpdater (real CountSketch + top-k heap)
// ---------------------------------------------------------------------------

/// Keyed top-k updater backed by a real `CountSketchWithHeap` (signed-row
/// CountSketch matrix PLUS a size-`heap_size` top-k heap). Distinct math from
/// `CmsHeapAccumulatorUpdater`'s CMS-with-heap (min-of-rows); shares the same
/// [`TopkWeight`] semantics and heap payload shape.
pub struct CountSketchWithHeapAccumulatorUpdater {
    acc: CountSketchWithHeapAccumulator,
    row_num: usize,
    col_num: usize,
    heap_size: usize,
    weight: TopkWeight,
    weight_scale: f64,
}

impl CountSketchWithHeapAccumulatorUpdater {
    pub fn new(row_num: usize, col_num: usize, heap_size: usize, weight: TopkWeight) -> Self {
        Self::with_weight_scale(row_num, col_num, heap_size, weight, 1.0)
    }

    pub fn with_weight_scale(
        row_num: usize,
        col_num: usize,
        heap_size: usize,
        weight: TopkWeight,
        weight_scale: f64,
    ) -> Self {
        Self {
            acc: CountSketchWithHeapAccumulator::new(row_num, col_num, heap_size),
            row_num,
            col_num,
            heap_size,
            weight,
            weight_scale,
        }
    }
}

impl AccumulatorUpdater for CountSketchWithHeapAccumulatorUpdater {
    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
        let weighted = match self.weight {
            TopkWeight::Value => value * self.weight_scale,
            TopkWeight::Count => 1.0,
        };
        self.acc.inner.update(&key.to_semicolon_str(), weighted);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = CountSketchWithHeapAccumulator::new(self.row_num, self.col_num, self.heap_size);
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        std::mem::size_of::<CountSketchWithHeapAccumulator>()
            + self.row_num * self.col_num * std::mem::size_of::<f64>()
            + self.heap_size * (std::mem::size_of::<asap_sketchlib::CsHeapItem>() + 32)
    }
}

// ---------------------------------------------------------------------------
// HydraKllAccumulatorUpdater
// ---------------------------------------------------------------------------

pub struct HydraKllAccumulatorUpdater {
    acc: HydraKllSketchAccumulator,
    row_num: usize,
    col_num: usize,
    k: u16,
}

impl HydraKllAccumulatorUpdater {
    pub fn new(row_num: usize, col_num: usize, k: u16) -> Self {
        Self {
            acc: HydraKllSketchAccumulator::new(row_num, col_num, k),
            row_num,
            col_num,
            k,
        }
    }
}

impl AccumulatorUpdater for HydraKllAccumulatorUpdater {
    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
        self.acc.update(key, value);
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc = HydraKllSketchAccumulator::new(self.row_num, self.col_num, self.k);
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        // Rough estimate: each cell is a KLL sketch
        std::mem::size_of::<HydraKllSketchAccumulator>() + self.row_num * self.col_num * 4096
    }
}

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

fn cms_dims(params: &SketchParams) -> (usize, usize) {
    match params {
        SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
            (*depth as usize, *width as usize)
        }
        other => unreachable!(
            "accumulator_spec() paired SketchAlgorithm::Cms/CountSketch with unexpected params: {other:?}"
        ),
    }
}

/// Read `(rows = depth, columns = width, heap_size)` out of `SketchParams::CmsWithHeap`
/// or `::CountSketchWithHeap`.
fn cms_heap_dims(params: &SketchParams) -> (usize, usize, usize) {
    match params {
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        }
        | SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => (*depth as usize, *width as usize, *heap_size as usize),
        other => unreachable!(
            "accumulator_spec() paired a WithHeap SketchAlgorithm with unexpected params: {other:?}"
        ),
    }
}

/// Construct the kernel declared by a Planner SummaryAgg. No backend config
/// tags participate in this dispatch and unsupported payloads are errors.
pub fn create_planner_accumulator(
    family: &SummaryFamilyType,
    input: &planner_types::post_asap::SummaryUpdate,
    grouping: &planner_types::post_asap::GroupingStrategy,
) -> Result<Box<dyn AccumulatorUpdater>, String> {
    crate::capability::validate_summary_kernel(family, input, grouping)?;
    use planner_types::post_asap::GroupingStrategy;
    if grouping != &GroupingStrategy::PerSubpopulationInstance {
        return Err("shared summary grouping requires a supported Planner Hydra kernel".into());
    }
    if matches!(family, SummaryFamilyType::ExactAggregate(..)) {
        return Ok(Box::new(PlannerExactUpdater {
            acc: crate::accumulators::exact_accumulator::ExactAccumulator::new(
                family.clone(),
                input.item.is_some(),
            )?,
        }));
    }
    let SummaryFamilyType::Sketch(kind, family_grouping) = family else {
        return Err(format!("unsupported Planner summary family {family:?}"));
    };
    if family_grouping != grouping {
        return Err("Planner family and operator grouping disagree".into());
    }
    // Heap counters use fixed-point storage for fractional counter deltas.
    // This encodes the selected update; it does not choose another family.
    let weight_scale = if matches!(
        input.weight,
        planner_types::post_asap::SummaryInputExpr::ResetAwareCounterDelta { .. }
    ) {
        1_000_000.0
    } else {
        1.0
    };
    let updater: Box<dyn AccumulatorUpdater> = match (kind.algorithm(), kind.params()) {
        (SketchAlgorithm::Kll, SketchParams::Kll { k }) => Box::new(KllAccumulatorUpdater::new(
            u16::try_from(*k).map_err(|_| "KLL k exceeds runtime bound")?,
        )),
        (SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }) => {
            Box::new(DDSketchAccumulatorUpdater::new(*alpha))
        }
        (SketchAlgorithm::Cms, params @ SketchParams::Cms { .. }) => {
            let (r, c) = cms_dims(params);
            Box::new(CmsAccumulatorUpdater::new(r, c))
        }
        (SketchAlgorithm::CountSketch, params @ SketchParams::CountSketch { .. }) => {
            let (r, c) = cms_dims(params);
            Box::new(CountSketchAccumulatorUpdater::new(r, c))
        }
        (SketchAlgorithm::CmsWithHeap, params @ SketchParams::CmsWithHeap { .. }) => {
            let (r, c, h) = cms_heap_dims(params);
            Box::new(CmsHeapAccumulatorUpdater::with_weight_scale(
                r,
                c,
                h,
                TopkWeight::Value,
                weight_scale,
            ))
        }
        (
            SketchAlgorithm::CountSketchWithHeap,
            params @ SketchParams::CountSketchWithHeap { .. },
        ) => {
            let (r, c, h) = cms_heap_dims(params);
            Box::new(CountSketchWithHeapAccumulatorUpdater::with_weight_scale(
                r,
                c,
                h,
                TopkWeight::Value,
                weight_scale,
            ))
        }
        (SketchAlgorithm::Hll, SketchParams::Hll { precision }) => Box::new(HllUpdater {
            acc: HllSketchAccumulator::new(
                asap_sketchlib::HllVariant::Regular,
                u32::from(*precision),
            ),
        }),
        (
            SketchAlgorithm::UnivMon,
            SketchParams::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
            },
        ) => Box::new(UnivMonUpdater {
            acc: UnivMonAccumulator::new(
                *heap_size as usize,
                *sketch_rows as usize,
                *sketch_cols as usize,
                *layers as usize,
            )
            .map_err(|e| e.to_string())?,
        }),
        _ => {
            return Err(format!(
                "unsupported Planner algorithm/parameters: {kind:?}"
            ))
        }
    };
    if updater.is_keyed() != input.item.is_some()
        && !crate::capability::is_unit_sample_frequency(input)
    {
        return Err("Planner item expression does not match the selected kernel layout".into());
    }
    Ok(updater)
}

struct PlannerExactUpdater {
    acc: crate::accumulators::exact_accumulator::ExactAccumulator,
}
impl AccumulatorUpdater for PlannerExactUpdater {
    fn update_single(&mut self, value: f64, timestamp: i64) {
        self.acc.update(None, value, timestamp);
    }
    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, timestamp: i64) {
        self.acc.update(Some(key), value, timestamp);
    }
    impl_clone_accumulator_methods!(acc);
    fn reset(&mut self) {
        self.acc = crate::accumulators::exact_accumulator::ExactAccumulator::new(
            self.acc.family().clone(),
            self.acc.is_keyed(),
        )
        .expect("installed exact family");
    }
    fn is_keyed(&self) -> bool {
        self.acc.is_keyed()
    }
    fn memory_usage_bytes(&self) -> usize {
        self.acc.approx_memory_bytes()
    }
}

#[cfg(test)]
mod planner_parameter_regression {
    use super::*;
    use planner_types::post_asap::{SketchKind, SummaryInputExpr, SummaryUpdate};

    // Planner width is the bucket count; depth is the independent hash-row count.
    #[test]
    fn planner_sketch_dimensions_are_not_transposed() {
        for (algorithm, params) in [
            (
                SketchAlgorithm::Cms,
                SketchParams::Cms {
                    width: 128,
                    depth: 3,
                },
            ),
            (
                SketchAlgorithm::CountSketch,
                SketchParams::CountSketch {
                    width: 128,
                    depth: 3,
                },
            ),
            (
                SketchAlgorithm::CmsWithHeap,
                SketchParams::CmsWithHeap {
                    width: 128,
                    depth: 3,
                    heap_size: 8,
                },
            ),
            (
                SketchAlgorithm::CountSketchWithHeap,
                SketchParams::CountSketchWithHeap {
                    width: 128,
                    depth: 3,
                    heap_size: 8,
                },
            ),
        ] {
            let family = SummaryFamilyType::Sketch(
                SketchKind::new(algorithm.clone(), params),
                Default::default(),
            );
            let update = SummaryUpdate {
                item: Some(SummaryInputExpr::Column(
                    planner_types::pre_asap::ColumnRef::Named("host".into()),
                )),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: Default::default(),
            };
            let state = create_planner_accumulator(&family, &update, &Default::default())
                .unwrap()
                .snapshot_accumulator();
            let dims = match algorithm {
                SketchAlgorithm::Cms => {
                    let s = state
                        .as_any()
                        .downcast_ref::<CountMinSketchAccumulator>()
                        .unwrap();
                    (s.inner.rows(), s.inner.cols())
                }
                SketchAlgorithm::CountSketch => {
                    let s = state
                        .as_any()
                        .downcast_ref::<CountSketchAccumulator>()
                        .unwrap();
                    (s.inner.rows, s.inner.cols)
                }
                SketchAlgorithm::CmsWithHeap => {
                    let s = state
                        .as_any()
                        .downcast_ref::<CountMinSketchWithHeapAccumulator>()
                        .unwrap();
                    (s.inner.rows(), s.inner.cols())
                }
                SketchAlgorithm::CountSketchWithHeap => {
                    let s = state
                        .as_any()
                        .downcast_ref::<CountSketchWithHeapAccumulator>()
                        .unwrap();
                    (s.inner.rows(), s.inner.cols())
                }
                _ => unreachable!(),
            };
            assert_eq!(dims, (3, 128), "{algorithm:?}");
        }
    }
}

struct UnivMonUpdater {
    acc: UnivMonAccumulator,
}

struct HllUpdater {
    acc: HllSketchAccumulator,
}

impl AccumulatorUpdater for HllUpdater {
    fn is_keyed(&self) -> bool {
        false
    }
    fn memory_usage_bytes(&self) -> usize {
        self.acc.approx_memory_bytes()
    }
    fn update_single(&mut self, value: f64, _: i64) {
        if !value.is_nan() {
            let bits = if value == 0.0 { 0 } else { value.to_bits() };
            self.acc.inner.update(&bits.to_le_bytes());
        }
    }
    fn update_keyed(&mut self, _: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
        self.update_single(value, timestamp_ms);
    }
    impl_clone_accumulator_methods!(acc);
    fn reset(&mut self) {
        self.acc.reset_to_empty();
    }
}

impl AccumulatorUpdater for UnivMonUpdater {
    fn is_keyed(&self) -> bool {
        false
    }
    fn memory_usage_bytes(&self) -> usize {
        self.acc.approx_memory_bytes()
    }
    fn update_single(&mut self, value: f64, _: i64) {
        self.acc
            .insert_sample(value)
            .expect("UnivMon sample counter overflow");
    }
    fn update_keyed(&mut self, _: &KeyByLabelValues, value: f64, timestamp_ms: i64) {
        self.update_single(value, timestamp_ms);
    }
    impl_clone_accumulator_methods!(acc);
    fn reset(&mut self) {
        self.acc.reset_to_empty();
    }
}
