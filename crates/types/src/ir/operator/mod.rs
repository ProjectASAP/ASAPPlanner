//! #511 §1: the operators of the unified DAG and the parameter types their
//! payloads are built from.
//!
//! - [`node`] — [`OperatorNode`](node::OperatorNode): an operator plus the
//!   planning properties every traversal needs.
//! - [`non_asap`] / [`asap`] — ordinary query operators and ASAP operators.
//! - [`operator_properties`] — operator parameter types ([`GroupKeys`],
//!   [`Reduction`], [`Source`], …).
//! - [`agg_intent`] — the aggregation-intent vocabulary ([`AggIntent`]).
//! - [`maintained_population`] — maintained populations and their evaluations.
//! - [`pane_chain`] — what a tumbling-pane chain computes.
//! - [`query`] — a query root: an operator result or a standalone scalar.
pub mod agg_intent;
pub mod asap;
pub mod maintained_population;
pub mod node;
pub mod non_asap;
pub mod operator_properties;
pub mod pane_chain;
pub mod query;

pub use agg_intent::{
    agg_accuracy, agg_is_exact, agg_is_mergeable, default_cardinality, default_quantile, AggIntent,
    MathFunc, TimeFunc,
};
pub use operator_properties::{
    AtModifier, BinaryOpKind, ColState, ConcatDiscriminatorKey, DataModel, GroupKeys, GroupSide,
    InfoMatcher, JoinKind, PromQLVectorSetOpKind, Reduction, RelationalSetOpKind, SampleKind,
    Source, TimeShift, VectorGrouping, VectorMatch, VectorMatchKind, WindowFrame, WindowFrameBound,
    WindowFrameOffset, WindowFrameUnits, WindowFuncKind,
};
