//! How Stage 1, Pass 1 computes an aggregate measure.

use crate::ir::schema::{ExactKind, SketchAlgorithm};

/// How one Pass 1 target is computed: exact or a summary.
///
/// Each target lists one exact option: `ExactAccumulator` when the intent
/// has an exact mergeable state and it can be built, otherwise `Exact`.
/// The plain exact computation is always offered too, as the aggregate
/// group's own expression.
#[derive(Debug, Clone, PartialEq)]
pub enum HowToComputeAggregateMeasure {
    /// Plain exact execution: the aggregate group's own expression; the
    /// listed exact option only when the intent has no exact accumulator
    /// form or that form is pruned.
    Exact,
    /// One or more exact mergeable accumulator states, by accumulator kind,
    /// finalized and combined into the measure: one state for `sum`,
    /// `count`, `min`, `max`, `rate`, `irate` and `increase`; `[Sum, Count]`,
    /// divided, for `avg`.
    ExactAccumulator(Vec<ExactKind>),
    /// A summary family: its state group gets one build candidate per
    /// configuration of the family's predefined list (`parameter_list`).
    Summary(SketchAlgorithm),
}
