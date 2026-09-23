use crate::KeyByLabelValues;
use std::collections::HashMap;

use crate::AggregationType;
use crate::Statistic;

use serde_json::Value;

/// Trait for objects that can be serialized to different formats
pub trait SerializableToSink {
    fn serialize_to_json(&self) -> Value;
    fn serialize_to_bytes(&self) -> Vec<u8>;
}

/// Core trait for all aggregates containing shared functionality
/// This trait provides common operations like serialization, cloning, and type identification
pub trait AggregateCore: SerializableToSink + Send + Sync {
    /// Clone this accumulator into a boxed trait object
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore>;

    /// Get the type name of this accumulator
    fn type_name(&self) -> &'static str;

    /// Downcast to Any for type checking
    fn as_any(&self) -> &dyn std::any::Any;

    /// Mutable downcast to Any. Used by ingest paths that need to
    /// mutate a boxed accumulator in place — e.g. the PROTO_DELTA
    /// delta-merge applier in `drivers::ingest::otel::apply_modified_otlp_delta_bytes`.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Merge this accumulator with another accumulator of the same type
    /// Returns a new merged accumulator, leaving the original unchanged
    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>>;

    /// Get the accumulator type identifier for merge compatibility checking
    fn get_accumulator_type(&self) -> AggregationType;

    /// Get all keys stored in this accumulator
    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>>;

    /// Dispatch a statistic query without downcasting.
    ///
    /// Replaces the 12-arm `match get_accumulator_type()` in the engine.
    /// Single-subpopulation types ignore `key`; multiple-subpopulation types
    /// require it and return `Err` when it is `None`.
    /// Special cases (DeltaSetAggregator, SetAggregator) fall back to a
    /// cardinality value when `key` is `None`.
    fn query_statistic(
        &self,
        statistic: Statistic,
        key: &Option<KeyByLabelValues>,
        query_kwargs: &HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>>;

    /// Approximate in-memory byte footprint of this accumulator.
    ///
    /// Used by the `SketchStore` persistence layer to drive its
    /// memory-pressure trigger. Not required to be exact — the flusher
    /// only needs rough proportionality. The default is a conservative
    /// 4 KiB constant; concrete types should override it with a
    /// type-aware estimate (e.g. KLL: `k * 8` plus overhead).
    ///
    /// Implementors must not call `serialize_to_bytes` here — this is
    /// on the insert hot path.
    fn approx_memory_bytes(&self) -> usize {
        4096
    }

    /// Typed auxiliary statistics — `count`, `sum`, `min`, `max` —
    /// exposed as first-class scalars alongside the sketch payload.
    ///
    /// The overwhelming majority of production queries
    /// (`count_over_time`, `sum_over_time`, `min_over_time`,
    /// `max_over_time`, and the additive aggregations built on
    /// them) only need these scalars. Returning them directly here
    /// lets callers avoid deserialising the full sketch bytes.
    ///
    /// Returning fields as `None` means the accumulator doesn't
    /// track that statistic exactly (e.g. a pure HLL doesn't carry
    /// sum/min/max). Callers then fall back to the sketch's
    /// `query_statistic` method.
    ///
    /// This is the phase-1 piece of the sketch DB design
    /// (docs/design_docs/summary-storage.md).
    fn aux_stats(&self) -> AuxStats {
        AuxStats::empty()
    }

    /// Reset the sketch state to empty **in place**, preserving its
    /// shape / configuration (dimensions, relative accuracy, register
    /// width, …) so a subsequent delta-apply lands on a clean,
    /// same-shape base.
    ///
    /// Used by the OTLP ingest path's per-window base rotation: when a
    /// delta frame opens a new tumbling window for a series, the cached
    /// base is reset here before the new window's delta is applied, so
    /// the reconstructed state reflects that window only rather than an
    /// all-time accumulation across windows (see
    /// `docs/delta-baseline-contract.md` §3).
    ///
    /// The default is a no-op: only the delta-capable, additive families
    /// (DDSketch, CMS, CountSketch, HLL) ever reach the rotation path and
    /// override this. KLL never deltas, and the non-sketch accumulators
    /// are never cached as a delta base.
    fn reset_to_empty(&mut self) {}
}

/// Four typed auxiliary scalars tracked alongside every sketch entry:
/// `count`, `sum`, `min`, `max`. Exposed so the query engine can
/// serve Count / Sum / Min / Max statistics without touching sketch
/// bytes.
///
/// Each field is `Option<…>` because not every accumulator tracks
/// every stat (e.g. HLL has cardinality but no meaningful
/// sum / min / max; DeltaSetAggregator tracks set transitions, not
/// numeric aggregates).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct AuxStats {
    pub count: Option<u64>,
    pub sum: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl AuxStats {
    pub const fn empty() -> Self {
        Self {
            count: None,
            sum: None,
            min: None,
            max: None,
        }
    }

    /// Attempt to fulfil a `Statistic` purely from the typed aux
    /// columns, without needing to deserialise the sketch. Returns
    /// `None` if the requested statistic isn't covered by aux
    /// (e.g. Quantile, Cardinality, TopK) or if the corresponding
    /// aux field is `None`.
    pub fn try_answer(&self, statistic: Statistic) -> Option<f64> {
        match statistic {
            Statistic::Count => self.count.map(|c| c as f64),
            Statistic::Sum => self.sum,
            Statistic::Min => self.min,
            Statistic::Max => self.max,
            // Increase / Rate need two samples; aux columns carry
            // window totals, so one entry's aux is insufficient.
            // Cardinality / Quantile / Topk are sketch-native and
            // must go through query_statistic.
            _ => None,
        }
    }

    /// Merge two aux stats the way the corresponding sketch merge
    /// would. Count / sum add, min / max take the extremum. When
    /// either side is `None` the result is the other side (so a
    /// window that only has partial aux still contributes).
    pub fn merge(self, other: Self) -> Self {
        fn add_opt_u(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.saturating_add(y)),
                (x, None) => x,
                (None, y) => y,
            }
        }
        fn add_opt_f(a: Option<f64>, b: Option<f64>) -> Option<f64> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x + y),
                (x, None) => x,
                (None, y) => y,
            }
        }
        fn min_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, None) => x,
                (None, y) => y,
            }
        }
        fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.max(y)),
                (x, None) => x,
                (None, y) => y,
            }
        }
        Self {
            count: add_opt_u(self.count, other.count),
            sum: add_opt_f(self.sum, other.sum),
            min: min_opt(self.min, other.min),
            max: max_opt(self.max, other.max),
        }
    }
}

/// Trait for accumulators that support a single subpopulation
/// These accumulators store a single aggregate value (e.g., Sum, Increase)
pub trait SingleSubpopulationAggregate: AggregateCore {
    /// Query the accumulator for a specific statistic
    fn query(
        &self,
        statistic: Statistic,
        query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>>;

    /// Clone this accumulator into a boxed trait object
    fn clone_boxed(&self) -> Box<dyn SingleSubpopulationAggregate>;
}

/// Trait for accumulators that support multiple subpopulations identified by keys
/// These accumulators store separate values for different label combinations
pub trait MultipleSubpopulationAggregate: AggregateCore {
    /// Query the accumulator for a specific statistic and key
    fn query(
        &self,
        statistic: Statistic,
        key: &KeyByLabelValues,
        query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>>;

    /// Clone this accumulator into a boxed trait object
    fn clone_boxed(&self) -> Box<dyn MultipleSubpopulationAggregate>;
}

/// Trait for merging multiple accumulators of the same type
pub trait MergeableAccumulator<T> {
    fn merge_accumulators(
        accumulators: Vec<T>,
    ) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
    where
        T: Sized;
}

// Implement Clone for the new trait objects
impl Clone for Box<dyn AggregateCore> {
    fn clone(&self) -> Self {
        self.clone_boxed_core()
    }
}

impl Clone for Box<dyn SingleSubpopulationAggregate> {
    fn clone(&self) -> Self {
        self.clone_boxed()
    }
}

impl Clone for Box<dyn MultipleSubpopulationAggregate> {
    fn clone(&self) -> Self {
        self.clone_boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aux_stats_empty_answers_nothing() {
        let e = AuxStats::empty();
        assert_eq!(e.try_answer(Statistic::Count), None);
        assert_eq!(e.try_answer(Statistic::Sum), None);
        assert_eq!(e.try_answer(Statistic::Min), None);
        assert_eq!(e.try_answer(Statistic::Max), None);
    }

    #[test]
    fn aux_stats_try_answer_covers_typed_stats() {
        let a = AuxStats {
            count: Some(7),
            sum: Some(42.0),
            min: Some(1.5),
            max: Some(9.25),
        };
        assert_eq!(a.try_answer(Statistic::Count), Some(7.0));
        assert_eq!(a.try_answer(Statistic::Sum), Some(42.0));
        assert_eq!(a.try_answer(Statistic::Min), Some(1.5));
        assert_eq!(a.try_answer(Statistic::Max), Some(9.25));
    }

    #[test]
    fn aux_stats_try_answer_skips_sketch_native_stats() {
        let a = AuxStats {
            count: Some(100),
            sum: Some(500.0),
            min: Some(1.0),
            max: Some(10.0),
        };
        assert_eq!(a.try_answer(Statistic::Quantile), None);
        assert_eq!(a.try_answer(Statistic::Cardinality), None);
        assert_eq!(a.try_answer(Statistic::Topk), None);
        assert_eq!(a.try_answer(Statistic::Increase), None);
        assert_eq!(a.try_answer(Statistic::Rate), None);
    }

    #[test]
    fn aux_stats_merge_adds_count_and_sum_takes_extrema() {
        let a = AuxStats {
            count: Some(10),
            sum: Some(50.0),
            min: Some(1.0),
            max: Some(9.0),
        };
        let b = AuxStats {
            count: Some(5),
            sum: Some(20.0),
            min: Some(0.5),
            max: Some(12.0),
        };
        let merged = a.merge(b);
        assert_eq!(merged.count, Some(15));
        assert_eq!(merged.sum, Some(70.0));
        assert_eq!(merged.min, Some(0.5));
        assert_eq!(merged.max, Some(12.0));
    }

    #[test]
    fn aux_stats_merge_handles_partial_sides() {
        // HLL-like (count only) merged with Sum-only side.
        let hll_like = AuxStats {
            count: Some(100),
            ..AuxStats::empty()
        };
        let sum_like = AuxStats {
            sum: Some(500.0),
            ..AuxStats::empty()
        };
        let merged = hll_like.merge(sum_like);
        assert_eq!(merged.count, Some(100));
        assert_eq!(merged.sum, Some(500.0));
        assert_eq!(merged.min, None);
        assert_eq!(merged.max, None);
    }

    #[test]
    fn aux_stats_merge_is_empty_plus_empty() {
        let merged = AuxStats::empty().merge(AuxStats::empty());
        assert_eq!(merged, AuxStats::empty());
    }

    #[test]
    fn aux_stats_count_saturates_on_overflow() {
        let a = AuxStats {
            count: Some(u64::MAX - 1),
            ..AuxStats::empty()
        };
        let b = AuxStats {
            count: Some(100),
            ..AuxStats::empty()
        };
        let merged = a.merge(b);
        assert_eq!(merged.count, Some(u64::MAX));
    }
}
