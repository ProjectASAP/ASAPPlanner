use planner_types::summary_type::SketchStatistic;

pub type KernelError = Box<dyn std::error::Error + Send + Sync>;

/// In-memory state of one population's summary.
///
/// Kernels adapt `asap_sketchlib` structures (or exact Planner state) to the
/// operations physical operators need: merge, typed evaluation and memory
/// accounting. Grouping belongs to operators; byte encodings belong to
/// `asap_sketchlib` and deployments.
pub trait AggregateCore: Send + Sync {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore>;

    fn as_any(&self) -> &dyn std::any::Any;

    /// Mutable downcast, so a deployment can apply ingest deltas and merges to
    /// a cached state in place instead of copying it for every frame.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Merge with a state of the same family and shape, leaving both inputs unchanged.
    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError>;

    /// Answer a sketch evaluation. Exact states are read through
    /// [`ExactAccumulator::evaluation`](super::exact::ExactAccumulator::evaluation).
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        Err(format!("{query:?} is not supported by this summary").into())
    }

    /// True when no sample reached this state, so a value readout (a
    /// quantile) has no answer, which SQL reads as NULL. Kernels that do not
    /// track it report false.
    fn is_empty(&self) -> bool {
        false
    }

    /// Approximate in-memory footprint, used for execution memory reservations.
    /// A state shared with other rows (see [`Self::shared_state`]) is not
    /// included. No default: each kernel says what it holds, e.g. a KLL
    /// state its sketch's `memory_bytes()`.
    fn approx_memory_bytes(&self) -> usize;

    /// The identity and footprint of a state this row shares with other
    /// rows (one Hydra grid read by every group of a build). Reservations
    /// count it once per identity, not once per row.
    fn shared_state(&self) -> Option<(usize, usize)> {
        None
    }
}

/// The three [`AggregateCore`] methods every kernel writes the same way, for
/// an `impl AggregateCore` block of a `Clone` state: a boxed copy and the
/// two `Any` views of the state itself.
macro_rules! aggregate_core_object_methods {
    () => {
        fn clone_boxed_core(&self) -> Box<dyn $crate::AggregateCore> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    };
}
pub(crate) use aggregate_core_object_methods;

/// `other` as the state type `T` that a `T` state's
/// [`AggregateCore::merge_with`] requires, or the error `mismatch`.
pub(crate) fn downcast_peer<'a, T: 'static>(
    other: &'a dyn AggregateCore,
    mismatch: &'static str,
) -> Result<&'a T, KernelError> {
    other
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| mismatch.into())
}

impl Clone for Box<dyn AggregateCore> {
    fn clone(&self) -> Self {
        self.clone_boxed_core()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary_kernels::DDSketchAccumulator;

    // A state behind a trait object can be updated in place through the mutable downcast.
    #[test]
    fn mutable_downcast_updates_in_place() {
        let mut state: Box<dyn AggregateCore> = Box::new(DDSketchAccumulator::new(0.01));
        let dd = state
            .as_any_mut()
            .downcast_mut::<DDSketchAccumulator>()
            .unwrap();
        dd.inner.add(&3.0);
        let median = state
            .estimate(&SketchStatistic::Quantile { q: 0.5 })
            .unwrap();
        assert!((median - 3.0).abs() <= 0.03, "{median}");
    }
}
