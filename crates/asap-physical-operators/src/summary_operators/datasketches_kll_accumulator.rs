use crate::{
    AggregateCore, AggregationType, AuxStats, MergeableAccumulator, SerializableToSink,
    SingleSubpopulationAggregate,
};
use asap_sketchlib::{KllSketch, MessagePackCodec};
use base64::{engine::general_purpose, Engine as _};
use serde_json::Value;
use std::collections::HashMap;
#[cfg(feature = "extra_debugging")]
use std::time::Instant;
use tracing::debug;

use crate::Statistic;

/// KLL sketch accumulator — wraps asap_sketchlib::KllSketch.
/// Core struct, update/merge/serde logic live in `asap_sketchlib::sketches`.
/// This file retains QE-specific trait impls and JSON output.
pub struct DatasketchesKLLAccumulator {
    pub inner: KllSketch,
}

impl DatasketchesKLLAccumulator {
    pub fn new(k: u16) -> Self {
        Self {
            inner: KllSketch::new(k),
        }
    }

    pub fn update(&mut self, value: f64) {
        self.inner.update(value);
    }

    pub fn get_quantile(&self, quantile: f64) -> f64 {
        self.inner.quantile(quantile)
    }

    /// Decode from the modified OTLP wire format's
    /// `KLLSketchDataPoint.sketch` bytes when
    /// `encoding = KLL_SKETCH_ENCODING_MSGPACK`. The bytes are the
    /// MessagePack serialization of the cross-language sketch-core
    /// `KllSketch` struct — PR I parity entrypoint. Unlike the
    /// `_ENCODING_PROTO` path (which does lossy statistical
    /// reconstruction via `update()` replay), the msgpack path is a
    /// bit-identical round-trip because sketch-core's `KllSketch`
    /// serializes its full internal state to msgpack.
    pub fn from_msgpack_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: KllSketch::from_msgpack(buffer)
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?,
        })
    }

    /// Decode from the modified OTLP wire format's
    /// `KLLSketchDataPoint.sketch` bytes — the protobuf-encoded
    /// `asap_sketchlib::proto::sketchlib::KllState` message that
    /// DataCollector's `kllprocessor` emits when
    /// `encoding = KLL_SKETCH_ENCODING_PROTO`.
    ///
    /// The neutral codec decodes the sketchlib envelope.
    /// The level-aware constructor below preserves the supplied retained
    /// sample layout without replaying updates.
    pub fn from_sketchlib_proto_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        let state = asap_sketch_codec::kll_state(buffer)?;
        if state.k < 8 {
            return Err(format!("KllState.k must be >= 8 (got {})", state.k).into());
        }
        if state.k > u16::MAX as u32 {
            return Err(format!(
                "KllState.k does not fit in u16 (got {}, max {})",
                state.k,
                u16::MAX
            )
            .into());
        }
        // Validate the levels[] boundary array if it is populated. The
        // proto contract says `levels[0] == 0` and
        // `levels[num_levels] == items.len()`. If the producer left
        // levels empty (common when num_levels is zero), skip.
        if !state.levels.is_empty() {
            if state.levels.len() as u32 != state.num_levels + 1 {
                return Err(format!(
                    "KllState levels length = {}, expected num_levels+1 = {}",
                    state.levels.len(),
                    state.num_levels + 1
                )
                .into());
            }
            if state.levels[0] != 0 {
                return Err(format!("KllState.levels[0] = {}, expected 0", state.levels[0]).into());
            }
            if *state.levels.last().unwrap() as usize != state.items.len() {
                return Err(format!(
                    "KllState.levels[{}] = {}, expected items.len() = {}",
                    state.num_levels,
                    state.levels.last().unwrap(),
                    state.items.len()
                )
                .into());
            }
        }
        let k = state.k as u16;
        // Direct, bit-exact reconstruction from the portable state (no per-item
        // `update()` replay) whenever the producer supplied the `levels[]`
        // boundary array — which it does for any non-empty sketch. Falls back to
        // the statistical replay only when `levels` is absent (empty sketch).
        if !state.levels.is_empty() {
            // KllState is highest-level first; the in-memory constructor
            // expects L0 first. Replaying or copying the wire order changes
            // retained-item weights after the first compaction.
            let mut items = Vec::with_capacity(state.items.len());
            let mut levels = vec![0];
            if state
                .levels
                .windows(2)
                .any(|bounds| bounds[0] > bounds[1] || bounds[1] as usize > state.items.len())
            {
                return Err("KllState levels must be monotonic and within items".into());
            }
            for bounds in state.levels.windows(2).rev() {
                items.extend_from_slice(&state.items[bounds[0] as usize..bounds[1] as usize]);
                levels.push(items.len());
            }
            return Ok(Self {
                inner: KllSketch::from_portable_state(
                    k,
                    &items,
                    &levels,
                    state.num_levels as usize,
                )
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
            });
        }
        let mut acc = Self::new(k);
        for item in &state.items {
            acc.update(*item);
        }
        Ok(acc)
    }

    /// Merge multiple accumulators efficiently without cloning all of them.
    pub fn merge_multiple(
        accumulators: &[Box<dyn crate::AggregateCore>],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }

        let mut kll_accumulators = Vec::with_capacity(accumulators.len());
        for acc in accumulators {
            if acc.get_accumulator_type() != AggregationType::DatasketchesKLL {
                return Err(format!(
                    "Cannot merge DatasketchesKLLAccumulator with {:?}",
                    acc.get_accumulator_type()
                )
                .into());
            }
            let kll_acc = acc
                .as_any()
                .downcast_ref::<DatasketchesKLLAccumulator>()
                .ok_or("Failed to downcast to DatasketchesKLLAccumulator")?;
            kll_accumulators.push(kll_acc);
        }

        let inner_refs: Vec<&KllSketch> = kll_accumulators.iter().map(|acc| &acc.inner).collect();
        let merged_inner = KllSketch::merge_refs(&inner_refs)?;
        Ok(Self {
            inner: merged_inner,
        })
    }
}

// Manual trait implementations since the C++ library doesn't provide them
impl Clone for DatasketchesKLLAccumulator {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl std::fmt::Debug for DatasketchesKLLAccumulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatasketchesKLLAccumulator")
            .field("k", &self.inner.k)
            .field("sketch_n", &self.inner.count())
            .finish()
    }
}

// TODO: verify this
// Thread safety: The C++ library is not thread-safe by default, but since we're using it
// in a single-threaded context per accumulator instance and only sharing read-only operations,
// this should be safe.
unsafe impl Send for DatasketchesKLLAccumulator {}
unsafe impl Sync for DatasketchesKLLAccumulator {}

impl SerializableToSink for DatasketchesKLLAccumulator {
    fn serialize_to_json(&self) -> Value {
        // Mirror Python implementation: {"sketch": base64_encoded_string}
        let sketch_bytes = self.inner.sketch_bytes();
        let sketch_b64 = general_purpose::STANDARD.encode(&sketch_bytes);
        serde_json::json!({ "sketch": sketch_b64 })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner.to_msgpack().unwrap_or_default()
    }
}

impl AggregateCore for DatasketchesKLLAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "DatasketchesKLLAccumulator"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(feature = "extra_debugging")]
        let merge_with_start = Instant::now();
        #[cfg(feature = "extra_debugging")]
        debug!(
            "[PERF] DatasketchesKLLAccumulator::merge_with() started - self.k={}, self.n={}",
            self.inner.k,
            self.inner.count()
        );

        if other.get_accumulator_type() != self.get_accumulator_type() {
            return Err(format!(
                "Cannot merge DatasketchesKLLAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        let other_kll = other
            .as_any()
            .downcast_ref::<DatasketchesKLLAccumulator>()
            .ok_or("Failed to downcast to DatasketchesKLLAccumulator")?;

        let merged_inner = KllSketch::merge_refs(&[&self.inner, &other_kll.inner])?;
        let merged = Self {
            inner: merged_inner,
        };

        #[cfg(feature = "extra_debugging")]
        debug!(
            "[PERF] DatasketchesKLLAccumulator::merge_with() TOTAL TIME: {:?}",
            merge_with_start.elapsed()
        );

        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::DatasketchesKLL
    }

    fn approx_memory_bytes(&self) -> usize {
        // KLL with default k=200 holds ~2*k items (~3 KiB). Round up
        // for overhead.
        4 * 1024
    }

    fn aux_stats(&self) -> AuxStats {
        // KLL natively tracks `count` (n, samples observed). min/max
        // are available from the underlying sketch but only via a
        // O(k) quantile extraction at quantile=0/1, which is not
        // a cheap trait-method call. sum is not retained by KLL.
        //
        // Surface only count here; follow-up PR may add min/max via a
        // dedicated accessor on sketch-core. `sum_over_time` queries
        // on KLL fall back to query_statistic as they do today.
        AuxStats {
            count: Some(self.inner.count()),
            ..AuxStats::empty()
        }
    }

    fn get_keys(&self) -> Option<Vec<crate::KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        _key: &Option<crate::KeyByLabelValues>,
        query_kwargs: &std::collections::HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::SingleSubpopulationAggregate;
        self.query(statistic, Some(query_kwargs))
    }
}

impl SingleSubpopulationAggregate for DatasketchesKLLAccumulator {
    fn query(
        &self,
        statistic: Statistic,
        query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        match statistic {
            Statistic::Quantile => {
                debug!(
                    "Querying DatasketchesKLLAccumulator for quantile with kwargs: {:?}",
                    query_kwargs
                );
                let quantile = query_kwargs
                    .and_then(|kwargs| kwargs.get("quantile"))
                    .ok_or("Missing quantile parameter for quantile query")?
                    .parse::<f64>()
                    .map_err(|_| "Invalid quantile parameter format")?;

                if !(0.0..=1.0).contains(&quantile) {
                    return Err("Quantile must be between 0.0 and 1.0".into());
                }

                Ok(self.get_quantile(quantile))
            }
            _ => Err(
                format!("Unsupported statistic in DatasketchesKLLAccumulator: {statistic:?}")
                    .into(),
            ),
        }
    }

    fn clone_boxed(&self) -> Box<dyn SingleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}

impl MergeableAccumulator<DatasketchesKLLAccumulator> for DatasketchesKLLAccumulator {
    fn merge_accumulators(
        accumulators: Vec<DatasketchesKLLAccumulator>,
    ) -> Result<DatasketchesKLLAccumulator, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }
        let mut iter = accumulators.into_iter();
        let mut merged = iter.next().unwrap();
        for acc in iter {
            merged.inner.merge(&acc.inner)?;
        }
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn encode_state(state: asap_sketchlib::proto::sketchlib::KllState) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, SketchEnvelope};
        SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Kll(state)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn test_datasketches_kll_creation() {
        let kll = DatasketchesKLLAccumulator::new(200);
        assert!(kll.inner.count() == 0);
        assert_eq!(kll.inner.k, 200);
    }

    #[test]
    fn test_datasketches_kll_update() {
        let mut kll = DatasketchesKLLAccumulator::new(200);
        kll.update(10.0);
        kll.update(20.0);
        kll.update(15.0);
        assert_eq!(kll.inner.count(), 3);
    }

    #[test]
    fn test_datasketches_kll_quantile() {
        let mut kll = DatasketchesKLLAccumulator::new(200);
        for i in 1..=10 {
            kll.update(i as f64);
        }
        assert_eq!(kll.get_quantile(0.0), 1.0);
        assert_eq!(kll.get_quantile(1.0), 10.0);
        // Sketchlib KLL is approximate; 0.5 quantile of 1..10 may be 5, 6, or 7.
        let q50 = kll.get_quantile(0.5);
        assert!((q50 - 6.0).abs() <= 1.0, "expected median ~6, got {q50}");
    }

    #[test]
    fn test_datasketches_kll_query() {
        let mut kll = DatasketchesKLLAccumulator::new(200);
        for i in 1..=10 {
            kll.update(i as f64);
        }

        let mut query_kwargs = HashMap::new();
        query_kwargs.insert("quantile".to_string(), "0.5".to_string());
        let result = kll.query(Statistic::Quantile, Some(&query_kwargs)).unwrap();
        // Sketchlib KLL is approximate; 0.5 quantile of 1..10 may be 5, 6, or 7.
        assert!(
            (result - 6.0).abs() <= 1.0,
            "expected median ~6, got {result}"
        );

        assert!(kll.query(Statistic::Sum, Some(&query_kwargs)).is_err());
    }

    #[test]
    fn test_datasketches_kll_merge() {
        let mut kll1 = DatasketchesKLLAccumulator::new(200);
        let mut kll2 = DatasketchesKLLAccumulator::new(200);

        for i in 1..=5 {
            kll1.update(i as f64);
        }
        for i in 6..=10 {
            kll2.update(i as f64);
        }

        let merged = DatasketchesKLLAccumulator::merge_accumulators(vec![kll1, kll2]).unwrap();
        assert_eq!(merged.inner.count(), 10);
        assert_eq!(merged.get_quantile(0.0), 1.0);
        assert_eq!(merged.get_quantile(1.0), 10.0);
    }

    #[test]
    fn test_datasketches_kll_get_keys() {
        let kll = DatasketchesKLLAccumulator::new(200);
        assert_eq!(kll.type_name(), "DatasketchesKLLAccumulator");
    }

    #[test]
    fn test_trait_object() {
        let mut kll = DatasketchesKLLAccumulator::new(200);
        kll.update(5.0);
        let trait_obj: Box<dyn AggregateCore> = Box::new(kll);
        assert_eq!(trait_obj.type_name(), "DatasketchesKLLAccumulator");
    }

    #[test]
    fn test_datasketches_kll_query_with_kwargs() {
        let mut kll = DatasketchesKLLAccumulator::new(200);
        for i in 1..=10 {
            kll.update(i as f64);
        }

        let mut query_kwargs = HashMap::new();
        query_kwargs.insert("quantile".to_string(), "0.5".to_string());
        let result = kll.query(Statistic::Quantile, Some(&query_kwargs)).unwrap();
        // Sketchlib KLL is approximate; 0.5 quantile of 1..10 may be 5, 6, or 7.
        assert!(
            (result - 6.0).abs() <= 1.0,
            "expected median ~6, got {result}"
        );

        query_kwargs.insert("quantile".to_string(), "0.9".to_string());
        let result = kll.query(Statistic::Quantile, Some(&query_kwargs)).unwrap();
        // Sketchlib KLL is approximate; 0.9 quantile of 1..10 may be 9 or 10.
        assert!(
            (9.0..=10.0).contains(&result),
            "expected 0.9 quantile in [9,10], got {result}"
        );

        query_kwargs.insert("quantile".to_string(), "0.0".to_string());
        assert_eq!(
            kll.query(Statistic::Quantile, Some(&query_kwargs)).unwrap(),
            1.0
        );

        query_kwargs.insert("quantile".to_string(), "1.0".to_string());
        assert_eq!(
            kll.query(Statistic::Quantile, Some(&query_kwargs)).unwrap(),
            10.0
        );

        assert!(kll.query(Statistic::Quantile, None).is_err());

        query_kwargs.insert("quantile".to_string(), "invalid".to_string());
        assert!(kll.query(Statistic::Quantile, Some(&query_kwargs)).is_err());

        query_kwargs.insert("quantile".to_string(), "1.5".to_string());
        assert!(kll.query(Statistic::Quantile, Some(&query_kwargs)).is_err());

        query_kwargs.insert("quantile".to_string(), "-0.1".to_string());
        assert!(kll.query(Statistic::Quantile, Some(&query_kwargs)).is_err());

        query_kwargs.insert("quantile".to_string(), "0.5".to_string());
        assert!(kll.query(Statistic::Sum, Some(&query_kwargs)).is_err());
    }

    #[test]
    fn test_datasketches_kll_merge_multiple() {
        let mut kll1 = DatasketchesKLLAccumulator::new(200);
        let mut kll2 = DatasketchesKLLAccumulator::new(200);
        let mut kll3 = DatasketchesKLLAccumulator::new(200);

        for i in 1..=5 {
            kll1.update(i as f64);
        }
        for i in 6..=10 {
            kll2.update(i as f64);
        }
        for i in 11..=15 {
            kll3.update(i as f64);
        }

        let boxed_accs: Vec<Box<dyn AggregateCore>> =
            vec![Box::new(kll1), Box::new(kll2), Box::new(kll3)];

        let merged = DatasketchesKLLAccumulator::merge_multiple(&boxed_accs).unwrap();
        assert_eq!(merged.inner.count(), 15);
        assert_eq!(merged.get_quantile(0.0), 1.0);
        assert_eq!(merged.get_quantile(1.0), 15.0);
        assert_eq!(merged.get_quantile(0.5), 8.0);
    }

    #[test]
    fn test_datasketches_kll_merge_multiple_error_cases() {
        let empty: Vec<Box<dyn AggregateCore>> = vec![];
        assert!(DatasketchesKLLAccumulator::merge_multiple(&empty).is_err());

        let kll1 = DatasketchesKLLAccumulator::new(200);
        let kll2 = DatasketchesKLLAccumulator::new(100);
        let boxed_accs: Vec<Box<dyn AggregateCore>> = vec![Box::new(kll1), Box::new(kll2)];
        assert!(DatasketchesKLLAccumulator::merge_multiple(&boxed_accs).is_err());

        use crate::summary_operators::sum_accumulator::SumAccumulator;
        let kll = DatasketchesKLLAccumulator::new(200);
        let sum = SumAccumulator::new();
        let mixed_accs: Vec<Box<dyn AggregateCore>> = vec![Box::new(kll), Box::new(sum)];
        assert!(DatasketchesKLLAccumulator::merge_multiple(&mixed_accs).is_err());
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_reconstructs_quantiles() {
        // Build a KllState with 64 items in level order; the decoder
        // replays every item through `update()` so the reconstructed
        // sketch is statistically equivalent — quantile estimates
        // match the ground truth (sorted items) within KLL's own
        // rank-error bound for k=200.
        use asap_sketchlib::proto::sketchlib::KllState;

        let items: Vec<f64> = (0..64).map(|i| i as f64).collect();
        let state = KllState {
            k: 200,
            m: 8,
            num_levels: 1,
            levels: vec![0, 64],
            items: items.clone(),
            coin: None,
            offset: 0.0,
            value_scale: 0,
            residuals: Vec::new(),
        };
        let bytes = encode_state(state);

        let acc =
            DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.inner.count(), 64);
        // For 64 values 0..63, the true median is 31.5 and quantile
        // error is ~1% × range = 0.63. KLL's own point query can
        // legally be off by up to ε × N ~= 0.01 × 64 = 0.64. Allow a
        // generous tolerance since the important invariant is "the
        // decoded sketch is queryable and returns a sensible value".
        let median = acc.get_quantile(0.5);
        assert!(
            (median - 31.5).abs() <= 10.0,
            "reconstructed median {median} is outside tolerance of true median 31.5"
        );
        let q01 = acc.get_quantile(0.01);
        let q99 = acc.get_quantile(0.99);
        assert!(
            q01 <= q99,
            "quantile monotonicity violated: q01={q01}, q99={q99}"
        );
    }

    // Compacted portable state is highest-level first, unlike the runtime buffer.
    #[test]
    fn compacted_wire_state_preserves_count_and_quantiles() {
        use asap_sketchlib::{proto::sketchlib::KllState, sketches::KLL};
        let mut source = KLL::<f64>::init_kll_with_seed(32, 123);
        for i in 0..1000 {
            source.update(&(((i * 7919 + 17) % 1009) as f64 / 1009.0));
        }
        assert!(source.wire_num_levels() > 1);
        let state = KllState {
            k: 32,
            m: source.wire_m(),
            num_levels: source.wire_num_levels(),
            levels: source.wire_levels(),
            items: source.wire_items(),
            coin: None,
            offset: 0.0,
            value_scale: 0,
            residuals: vec![],
        };
        let decoded =
            DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(&encode_state(state)).unwrap();
        assert_eq!(decoded.inner.count(), source.count() as u64);
        for q in [0.0, 0.1, 0.5, 0.9, 1.0] {
            assert_eq!(decoded.inner.quantile(q), source.quantile(q), "q={q}");
        }
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrapped() {
        // Mirrors what DataCollector's kllprocessor emits: the state
        // wrapped in a `SketchEnvelope{kll: ...}` via sketchlib-go's
        // `SerializePortableFO` + `proto.Marshal`.
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, KllState, SketchEnvelope};

        let items: Vec<f64> = (0..64).map(|i| i as f64).collect();
        let state = KllState {
            k: 200,
            m: 8,
            num_levels: 1,
            levels: vec![0, 64],
            items,
            coin: None,
            offset: 0.0,
            value_scale: 0,
            residuals: Vec::new(),
        };
        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Kll(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let acc = DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("envelope-wrapped decode should succeed");
        assert_eq!(acc.inner.count(), 64);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrong_sketch_type() {
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, CountMinState, SketchEnvelope};

        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::CountMin(
                CountMinState::default(),
            )),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let result = DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "wrong-sketch envelope should error");
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_rejects_small_k() {
        use asap_sketchlib::proto::sketchlib::KllState;
        let state = KllState {
            k: 4, // < minimum of 8
            m: 2,
            num_levels: 0,
            levels: Vec::new(),
            items: Vec::new(),
            coin: None,
            offset: 0.0,
            value_scale: 0,
            residuals: Vec::new(),
        };
        let bytes = encode_state(state);
        let result = DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("k must be >= 8"));
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_rejects_inconsistent_levels() {
        use asap_sketchlib::proto::sketchlib::KllState;
        // num_levels=1 but levels array has 3 entries instead of 2
        let state = KllState {
            k: 200,
            m: 8,
            num_levels: 1,
            levels: vec![0, 5, 10],
            items: vec![1.0, 2.0, 3.0, 4.0, 5.0],
            coin: None,
            offset: 0.0,
            value_scale: 0,
            residuals: Vec::new(),
        };
        let bytes = encode_state(state);
        let result = DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("levels length"));
    }

    #[test]
    fn aux_stats_exposes_count_via_kll_n() {
        let mut acc = DatasketchesKLLAccumulator::new(200);
        for i in 0..50 {
            acc.update(i as f64);
        }
        let aux = acc.aux_stats();
        assert_eq!(aux.count, Some(50));
        // KLL doesn't natively expose min/max cheaply and doesn't
        // track sum at all — those fields must be None so callers
        // fall through to query_statistic.
        assert_eq!(aux.sum, None);
        assert_eq!(aux.min, None);
        assert_eq!(aux.max, None);
    }

    #[test]
    fn aux_stats_empty_kll_has_zero_count() {
        let acc = DatasketchesKLLAccumulator::new(200);
        assert_eq!(acc.aux_stats().count, Some(0));
    }
}
