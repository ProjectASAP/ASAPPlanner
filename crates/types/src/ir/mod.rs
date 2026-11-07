//! The operator IR from #511: one operator DAG for every planning stage,
//! arranged by IR section.
pub mod operator;
pub mod properties;
pub mod scalar;
pub mod schema;
pub use operator::asap::{ASAPOp, UNIMPLEMENTED_ASAP_OP};
pub use operator::node::{Operator, OperatorNode, OperatorResultKind};
pub use operator::non_asap::{BinaryOperator, NonASAPOp, TimeRangeKind};
pub use operator::query::QueryRoot;
pub use scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};
pub use schema::error::SchemaDerivationError;

pub mod flat;
pub mod materialized_view;
pub mod physical_export;
pub use properties::timing::{
    apply_materialization_timings, apply_materialization_timings_to_roots, data_state,
    timing_under_consumer, MaterializationAssignment, NodeToTimedNodeCache,
};
#[cfg(any(test, feature = "test-support"))]
pub use properties::timing::{planned_data_state, validate_at_ingestion};
