//! The unified operator IR: one operator language before and after ASAP
//! optimization.
//!
//! - [`node`] — [`OperatorNode`] / [`Operator`]: the DAG node and its two
//!   operator categories, with the common planning properties.
//! - [`non_asap`] — [`NonASAPOp`]: ordinary query operators.
//! - [`asap`] — [`ASAPOp`]: summary-state construction, operations and evaluations.
//! - [`scalar`] — [`ScalarExpr`]: value computation owned by operator fields.

pub mod asap;
pub mod canonicalize;
pub mod cse;
pub mod export;
pub mod node;
pub mod query;
pub use query::QueryRoot;
pub mod non_asap;
pub mod scalar;
pub mod timing;

pub use asap::{ASAPOp, UNIMPLEMENTED_ASAP_OP};
pub use node::{Operator, OperatorNode, OperatorResultKind};
pub use non_asap::BinaryOperator;
pub use non_asap::{NonASAPOp, TimeRangeKind};
pub use scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};
pub use timing::{
    apply_lifecycle_timings, data_state, planned_data_state, split_shared_by_phase,
    validate_default, LifecycleAssignment, TimingMemo,
};

pub mod aggregate_schema;
pub mod error;
pub mod operator_properties;
pub use error::QueryExprError;
