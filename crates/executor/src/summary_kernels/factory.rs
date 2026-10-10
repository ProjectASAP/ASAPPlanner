use crate::summary_kernels::hll_sketch::HllSketchAccumulator;
use crate::summary_kernels::univmon::UnivMonAccumulator;
use crate::summary_kernels::{
    CountMinSketchAccumulator, CountMinSketchWithHeapAccumulator, CountSketchAccumulator,
    CountSketchWithHeapAccumulator, DDSketchAccumulator, DatasketchesKLLAccumulator,
    HydraKllSketchAccumulator,
};
use crate::{AggregateCore, KeyByLabelValues};
use planner_types::ir::schema::{
    FieldDataType as SummaryFamilyType, SketchAlgorithm, SketchParams,
};

/// Generate the clone-based `AccumulatorUpdater` methods for updaters whose
/// inner `acc` field implements `Clone + AggregateCore`.
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
            // Avoids a clone when a pane is evicted at window close.
            let this = *self;
            Box::new(this.$acc_field)
        }
    };
}

/// Shared update interface for query-time and precompute-time accumulation.
///
/// This provides a uniform interface over all accumulator types so that the
/// operators don't need to know which concrete type they're dealing with.
pub trait AccumulatorUpdater: Send {
    /// Validate an immutable precompute input before an updater can silently
    /// discard a value outside its representable domain.
    fn validate_single_input(&self, value: f64) -> Result<(), String> {
        if value.is_finite() {
            Ok(())
        } else {
            Err("accumulator input must be finite".into())
        }
    }

    /// Update from the typed native row. Numeric kernels retain their existing
    /// Float64 contract; frequency kernels may accept nonnumeric identities.
    fn update_value(
        &mut self,
        value: &crate::values::Value,
        timestamp_ms: i64,
    ) -> Result<(), String> {
        let crate::values::Value::Float64(value) = value else {
            return Err("summary update requires Float64".into());
        };
        self.validate_single_input(*value)?;
        self.update_single(*value, timestamp_ms);
        Ok(())
    }

    /// Feed a single (value, timestamp_ms) pair — for SingleSubpopulation types.
    fn update_single(&mut self, value: f64, timestamp_ms: i64);

    /// Feed a keyed (key, value, timestamp_ms) triple, e.g. a frequency item or an exact keyed state.
    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, timestamp_ms: i64);

    /// Extract the final accumulator as a boxed `AggregateCore`.
    fn take_accumulator(&mut self) -> Box<dyn AggregateCore>;

    /// Non-destructive read of the current accumulator state (clone without reset).
    /// Used by pane-based sliding windows to read shared panes.
    fn snapshot_accumulator(&self) -> Box<dyn AggregateCore>;

    /// Consume the updater and return its accumulator by move, avoiding the
    /// clone that `take_accumulator`/`snapshot_accumulator` pay. Default falls
    /// back to a clone for updaters that can't move their inner state out.
    fn into_accumulator(self: Box<Self>) -> Box<dyn AggregateCore> {
        self.snapshot_accumulator()
    }

    /// Reset internal state for reuse (avoids re-allocation).
    fn reset(&mut self);

    /// Whether this updater consumes keyed updates.
    fn is_keyed(&self) -> bool;

    /// Estimated memory usage in bytes.
    fn memory_usage_bytes(&self) -> usize;
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
// DDSketchAccumulatorUpdater
// ---------------------------------------------------------------------------
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
/// The key is the frequency item supplied by the operator (e.g. a `host`
/// value), not a group-by population. The accumulated quantity is selected
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
}

impl CmsHeapAccumulatorUpdater {
    pub fn new(row_num: usize, col_num: usize, heap_size: usize, weight: TopkWeight) -> Self {
        Self {
            acc: CountMinSketchWithHeapAccumulator::new(row_num, col_num, heap_size),
            row_num,
            col_num,
            heap_size,
            weight,
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
            TopkWeight::Value => value,
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
/// `CmsAccumulatorUpdater`'s CMS (min-of-rows).
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
}

impl CountSketchWithHeapAccumulatorUpdater {
    pub fn new(row_num: usize, col_num: usize, heap_size: usize, weight: TopkWeight) -> Self {
        Self {
            acc: CountSketchWithHeapAccumulator::new(row_num, col_num, heap_size),
            row_num,
            col_num,
            heap_size,
            weight,
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
            TopkWeight::Value => value,
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

/// Construct the kernel declared by a Planner SummaryAgg. No deployment config
/// tags participate in this dispatch and unsupported payloads are errors.
pub fn create_planner_accumulator(
    family: &SummaryFamilyType,
    input: &planner_types::ir::schema::SummaryUpdate,
    grouping: &planner_types::ir::schema::GroupingStrategy,
) -> Result<Box<dyn AccumulatorUpdater>, String> {
    if input.item.is_some()
        && matches!(
            input.weight_domain,
            planner_types::ir::schema::WeightDomain::NonNegative {
                proof:
                    planner_types::ir::schema::NonNegativeWeightProof::ResetAwareCounterDerivative
            }
        )
    {
        return Err("window-weighted summaries require typed DAG binding; integer heap updaters cannot consume rates".into());
    }

    crate::capability::validate_summary_kernel(family, input, grouping)?;
    use planner_types::ir::schema::GroupingStrategy;
    if grouping != &GroupingStrategy::PerSubpopulationInstance {
        return Err("shared summary grouping requires a supported Planner Hydra kernel".into());
    }
    if matches!(family, SummaryFamilyType::ExactAggregate(..)) {
        return Ok(Box::new(PlannerExactUpdater {
            acc: crate::summary_kernels::exact::ExactAccumulator::new(
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
            Box::new(CmsHeapAccumulatorUpdater::new(r, c, h, TopkWeight::Value))
        }
        (
            SketchAlgorithm::CountSketchWithHeap,
            params @ SketchParams::CountSketchWithHeap { .. },
        ) => {
            let (r, c, h) = cms_heap_dims(params);
            Box::new(CountSketchWithHeapAccumulatorUpdater::new(
                r,
                c,
                h,
                TopkWeight::Value,
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
    acc: crate::summary_kernels::exact::ExactAccumulator,
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
        self.acc = crate::summary_kernels::exact::ExactAccumulator::new(
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
        self.acc.inner =
            asap_sketchlib::HllSketch::new(self.acc.inner.variant, self.acc.inner.precision);
    }
}

impl AccumulatorUpdater for UnivMonUpdater {
    fn update_value(&mut self, value: &crate::values::Value, _: i64) -> Result<(), String> {
        self.acc
            .insert_value(value)
            .map_err(|error| error.to_string())
    }
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
        self.acc.clear();
    }
}

#[cfg(test)]
mod planner_parameter_regression {
    use super::*;
    use planner_types::ir::schema::{SketchKind, SummaryInputExpr, SummaryUpdate};

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
                    planner_types::ir::scalar::ColumnRef::Named("host".into()),
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
