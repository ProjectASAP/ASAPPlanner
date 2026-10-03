//! The operator IR from #511: one operator DAG for every planning stage.
//!
//! - [`operator`] — §1 operators: [`OperatorNode`] and its operator families.
//! - [`scalar`] — §2.2 scalar expressions and column references.
//! - [`schema`] — §2.1 per-edge [`schema::Schema`] and summary state types.
//! - [`properties`] — §2.2–2.3 node properties: accuracy guarantees, execution
//!   timing, and summary coverage.
//! - [`flat`], [`physical_export`], [`cse`], [`canonicalize`] — DAG passes and
//!   transport.
pub mod operator;
pub mod properties;
pub mod query;
pub mod scalar;
pub mod schema;
pub use operator::asap::{ASAPOp, UNIMPLEMENTED_ASAP_OP};
pub use operator::node::{Operator, OperatorNode, OperatorResultKind};
pub use operator::non_asap::{BinaryOperator, NonASAPOp, TimeRangeKind};
pub use query::QueryRoot;
pub use scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};
pub use schema::error::SchemaDerivationError;

pub mod canonicalize;
pub mod cse;
pub mod flat;
/// Physical ASAP DAG: the flattened operators plus execution timing.
pub mod physical_export;
pub use properties::timing::{
    apply_materialization_timings, data_state, planned_data_state, split_shared_by_phase,
    validate_maintained, MaterializationAssignment, TimingMemo,
};

pub mod schema_support;
