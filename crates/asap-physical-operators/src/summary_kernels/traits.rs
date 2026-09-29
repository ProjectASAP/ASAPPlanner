use planner_types::post_asap::SketchQuery;

pub type KernelError = Box<dyn std::error::Error + Send + Sync>;

/// In-memory state of one population's summary.
///
/// Kernels adapt `asap_sketchlib` structures (or exact Planner state) to the
/// operations physical operators need: merge, typed readout and memory
/// accounting. Grouping belongs to operators; byte encodings belong to
/// `asap_sketchlib` and deployments.
pub trait AggregateCore: Send + Sync {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore>;

    fn as_any(&self) -> &dyn std::any::Any;

    /// Merge with a state of the same family and shape, leaving both inputs unchanged.
    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError>;

    /// Answer a sketch readout. Exact states are read through
    /// [`ExactAccumulator::readout`](super::exact::ExactAccumulator::readout).
    fn estimate(&self, query: &SketchQuery) -> Result<f64, KernelError> {
        Err(format!("{query:?} is not supported by this summary").into())
    }

    /// Approximate in-memory footprint, used for execution memory reservations.
    fn approx_memory_bytes(&self) -> usize {
        4096
    }
}

impl Clone for Box<dyn AggregateCore> {
    fn clone(&self) -> Self {
        self.clone_boxed_core()
    }
}
