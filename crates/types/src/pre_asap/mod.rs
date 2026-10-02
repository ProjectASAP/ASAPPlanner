//! Shared vocabulary of the operator IR. The operators themselves live in
//! [`crate::ir`]; this module holds the field types they are built from.
//!
//! Operator parameters and schema derivation live in [`crate::ir`].
//! - [`agg_intent`] — the aggregation-intent vocabulary ([`AggIntent`]).
//! - [`expr_ir`] — [`ColumnRef`] and the scalar literal / operator kinds
//!   ([`ScalarValue`], [`CompareOpKind`], [`ArithmeticOpKind`]).
//! - [`schema`] — the per-edge [`Schema`] every node carries.
//! - [`column_resolution`] — turn a name-based `ColumnRef` into a positional
//!   `ColumnId` against a [`Schema`] (used by front-end name resolution).
//! - [`scalar_type_rules`] — type rules of the map scalar functions.

pub mod agg_intent;
pub mod column_resolution;
pub mod expr_ir;
pub mod scalar_type_rules;
pub mod schema;

pub use crate::ir::operator_properties::{
    AtModifier, BinaryOpKind, ColState, ConcatDiscriminatorKey, DataModel, GroupKeys, GroupSide,
    InfoMatcher, JoinKind, PromQLVectorSetOpKind, Reduction, RelationalSetOpKind, SampleKind,
    Source, TimeShift, VectorGrouping, VectorMatch, VectorMatchKind, WindowFrame, WindowFrameBound,
    WindowFrameOffset, WindowFrameUnits, WindowFuncKind,
};
pub use agg_intent::{
    agg_accuracy, agg_is_exact, agg_is_mergeable, default_cardinality, default_quantile, AggIntent,
    MathFunc, TimeFunc,
};
pub use column_resolution::{resolve_column_ref, resolve_column_refs, ResolveError};
pub use expr_ir::{ArithmeticOpKind, ColumnRef, CompareOpKind, ScalarValue};
pub use schema::{ColumnId, DataType, Field, FieldDataType, Schema};

pub use crate::ir::aggregate_schema::aggregate_output_schema;
pub use crate::ir::SchemaDerivationError;
