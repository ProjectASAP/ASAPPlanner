use crate::summary_kernels::hll_sketch::HllSketchAccumulator;
use crate::summary_kernels::univmon::UnivMonAccumulator;
use crate::summary_kernels::weighted_frequency::{FrequencyAlgorithm, WeightedFrequency};
use crate::summary_kernels::{
    CountMinSketchAccumulator, DDSketchAccumulator, DatasketchesKLLAccumulator, HydraCms,
    HydraKllSketchAccumulator,
};
use crate::{AggregateCore, KeyByLabelValues};
use planner_types::post_asap::{FieldDataType as SummaryFamilyType, SketchAlgorithm, SketchParams};

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

    /// Update from one typed row value. Numeric kernels keep their Float64
    /// contract; a frequency kernel may also take non-numeric identities.
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
// WeightedFrequencyUpdater (CountSketch, Count-Min with heap, CountSketch with heap)
// ---------------------------------------------------------------------------

/// Keyed frequency updater backed by sketchlib's per-row-seeded weighted
/// frequency kernel: plain CountSketch (no candidate heap), Count-Min with a
/// top-k heap, or CountSketch with a top-k heap. For the two heap families
/// this is the state the DAG's keyed summary build produces; plain
/// CountSketch uses the same kernel without a heap. Each row hashes with
/// its own seed, so rows stay independent at any depth. Each label value of
/// the key is one `Utf8` identity, as in the DAG's keyed build.
///
/// Each raw Prometheus sample contributes its `value` as a Float64 weight,
/// kept exactly (no rounding). CountSketch accepts signed weights; Count-Min
/// accepts nonnegative ones and rejects a negative input at validation.
pub struct WeightedFrequencyUpdater {
    acc: WeightedFrequency,
}

impl WeightedFrequencyUpdater {
    /// Fails for a shape `asap_sketchlib` refuses: a zero dimension, or an
    /// even CountSketch `depth` (the median needs an odd row count).
    pub fn new(
        algorithm: FrequencyAlgorithm,
        width: usize,
        depth: usize,
        capacity: usize,
    ) -> Result<Self, crate::Error> {
        Ok(Self {
            acc: WeightedFrequency::new(algorithm, width, depth, capacity)?,
        })
    }
}

impl AccumulatorUpdater for WeightedFrequencyUpdater {
    /// The kernel's own weight rule: finite, and nonnegative for Count-Min.
    fn validate_single_input(&self, value: f64) -> Result<(), String> {
        self.acc
            .check_weight(value)
            .map_err(|error| error.to_string())
    }

    fn update_single(&mut self, _value: f64, _timestamp_ms: i64) {
        debug_assert!(
            false,
            "update_single called on keyed updater; use update_keyed"
        );
    }

    fn update_keyed(&mut self, key: &KeyByLabelValues, value: f64, _timestamp_ms: i64) {
        // Inputs pass `validate_single_input` first; only a sum past
        // `f64::MAX` can fail here.
        self.acc
            .update_labels(&key.labels, value)
            .expect("weighted frequency counter overflow");
    }

    impl_clone_accumulator_methods!(acc);

    fn reset(&mut self) {
        self.acc.clear();
    }

    fn is_keyed(&self) -> bool {
        true
    }

    fn memory_usage_bytes(&self) -> usize {
        self.acc.approx_memory_bytes()
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

/// Construct the kernel declared by a Planner SummaryAgg. No deployment config
/// tags participate in this dispatch and unsupported payloads are errors.
pub fn create_planner_accumulator(
    family: &SummaryFamilyType,
    input: &planner_types::post_asap::SummaryUpdate,
    grouping: &planner_types::post_asap::GroupingStrategy,
) -> Result<Box<dyn AccumulatorUpdater>, String> {
    if input.item.is_some()
        && matches!(
            input.weight_domain,
            planner_types::post_asap::WeightDomain::NonNegative {
                proof:
                    planner_types::post_asap::NonNegativeWeightProof::ResetAwareCounterDerivative
            }
        )
    {
        return Err("window-weighted summaries require typed DAG binding; integer heap updaters cannot consume rates".into());
    }

    crate::capability::validate_summary_kernel(family, input, grouping)?;
    use planner_types::post_asap::GroupingStrategy;
    if grouping != &GroupingStrategy::PerSubpopulationInstance {
        return Err(
            "shared grouping has one state for all groups; build it with `create_hydra_cms`".into(),
        );
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
        (SketchAlgorithm::Kll, SketchParams::Kll { k, .. }) => {
            if *k > planner_types::post_asap::sketch::KLL_MAX_K {
                return Err("KLL k exceeds runtime bound".into());
            }
            Box::new(KllAccumulatorUpdater::new(
                u16::try_from(*k).map_err(|_| "KLL k exceeds runtime bound")?,
            ))
        }
        (SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }) => {
            Box::new(DDSketchAccumulatorUpdater::new(*alpha))
        }
        (SketchAlgorithm::Cms, params @ SketchParams::Cms { .. }) => {
            let (r, c) = cms_dims(params);
            Box::new(CmsAccumulatorUpdater::new(r, c))
        }
        (SketchAlgorithm::CountSketch, params @ SketchParams::CountSketch { .. }) => {
            let (r, c) = cms_dims(params);
            Box::new(
                WeightedFrequencyUpdater::new(FrequencyAlgorithm::CountSketch, c, r, 0)
                    .map_err(|e| e.to_string())?,
            )
        }
        (SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap, _) => {
            let (algorithm, width, depth, capacity) =
                WeightedFrequency::configuration(kind).map_err(|e| e.to_string())?;
            Box::new(
                WeightedFrequencyUpdater::new(algorithm, width, depth, capacity)
                    .map_err(|e| e.to_string())?,
            )
        }
        (SketchAlgorithm::Hll, SketchParams::Hll { precision, .. }) => Box::new(HllUpdater {
            acc: HllSketchAccumulator::new(
                asap_sketchlib::HllVariant::Regular,
                u32::from(*precision),
            ),
        }),
        (SketchAlgorithm::Kmv, SketchParams::Kmv { k, .. }) => {
            Box::new(crate::summary_kernels::kmv::KmvAccumulator::new(*k))
        }
        (
            SketchAlgorithm::UnivMon,
            SketchParams::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
                ..
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
        && crate::capability::unit_frequency_column(input).is_none()
    {
        return Err("Planner item expression does not match the selected kernel layout".into());
    }
    Ok(updater)
}

/// Construct the one state a `SharedMultiSubpopulation { kind: HydraCms }`
/// family shares across its groups. The update contract is checked with the
/// plan by `validate_summary_kernel`.
pub fn create_hydra_cms(family: &SummaryFamilyType) -> Result<HydraCms, String> {
    let (shared_rows, shared_columns, width, depth) = crate::capability::hydra_cms_shape(family)?;
    HydraCms::new(shared_rows, shared_columns, width, depth).map_err(|e| e.to_string())
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
                SketchAlgorithm::CountSketch
                | SketchAlgorithm::CmsWithHeap
                | SketchAlgorithm::CountSketchWithHeap => {
                    let (width, depth, _) = state
                        .as_any()
                        .downcast_ref::<WeightedFrequency>()
                        .unwrap()
                        .shape();
                    (depth, width)
                }
                _ => unreachable!(),
            };
            assert_eq!(dims, (3, 128), "{algorithm:?}");
        }
    }

    // HydraCms builds one shared grid shaped by its Hydra params; it is never
    // a per-group updater, and its family must be the matching Count-Min.
    #[test]
    fn hydra_cms_builds_one_shared_grid() {
        use planner_types::post_asap::{
            GroupingStrategy, HydraKind, HydraParams, NonNegativeWeightProof, WeightDomain,
        };
        let grouping = |width| GroupingStrategy::SharedMultiSubpopulation {
            kind: HydraKind::HydraCms,
            params: HydraParams::HydraCms {
                width,
                depth: 3,
                shared_rows: 5,
                shared_columns: 32,
            },
        };
        let family = |width| {
            SummaryFamilyType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Cms,
                    SketchParams::Cms {
                        width: 128,
                        depth: 3,
                    },
                ),
                grouping(width),
            )
        };
        assert_eq!(
            create_hydra_cms(&family(128)).unwrap().shape(),
            (5, 32, 128, 3)
        );
        assert!(create_hydra_cms(&family(64)).is_err());
        let update = SummaryUpdate {
            item: Some(SummaryInputExpr::Column(
                planner_types::pre_asap::ColumnRef::Named("host".into()),
            )),
            weight: SummaryInputExpr::Constant(1.0),
            weight_domain: WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            },
        };
        assert!(
            crate::capability::validate_summary_kernel(&family(128), &update, &grouping(128))
                .is_ok()
        );
        assert!(create_planner_accumulator(&family(128), &update, &grouping(128)).is_err());
    }
}
