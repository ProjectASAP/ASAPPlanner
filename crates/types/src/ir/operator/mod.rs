//! The operators of the unified DAG and the parameter types their
//! payloads are built from.
//!
//! - [`node`] — [`OperatorNode`](node::OperatorNode): an operator plus the
//!   planning properties every traversal needs.
//! - [`non_asap`] / [`asap`] — ordinary query operators and ASAP operators.
//! - [`input`] — how the derivation rules read an operator's inputs
//!   ([`OperatorInput`](input::OperatorInput)).
//! - [`arguments`] — operator argument types ([`GroupKeys`],
//!   [`Reduction`], [`Source`], …).
//! - [`agg_intent`] — the aggregation-intent vocabulary ([`AggIntent`]).
//! - [`maintained_population`] — maintained populations and their evaluations.
//! - [`tumbling_window_chain`] — what a tumbling-window chain computes.
//! - [`query`] — a query root: an operator result or a standalone scalar.
pub mod agg_intent;
pub mod arguments;
pub mod asap;
pub mod input;
pub mod maintained_population;
pub mod node;
pub mod non_asap;
pub mod query;
pub mod tumbling_window_chain;

pub use agg_intent::{
    agg_has_exact_per_entity_kernel, default_cardinality, default_quantile, AggIntent, MathFunc,
    TimeFunc,
};
pub use arguments::{
    AtModifier, BinaryOpKind, ColState, GroupKeys, GroupSide, InfoMatcher, JoinKind,
    PromQLVectorSetOpKind, Reduction, RelationalSetOpKind, SampleKind, Source, TimeShift,
    VectorGrouping, VectorMatch, VectorMatchKind, WindowFrame, WindowFrameBound, WindowFrameOffset,
    WindowFrameUnits, WindowFuncKind,
};
