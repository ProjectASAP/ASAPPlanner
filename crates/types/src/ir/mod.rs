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

pub mod canonicalize;
pub mod cse;
pub mod flat;
/// Physical ASAP DAG: the flattened operators plus execution timing.
pub mod physical_export;
pub use properties::timing::{
    apply_materialization_timings, apply_materialization_timings_to_roots, data_state,
    is_filtered_build, planned_data_state, split_shared_by_timing, validate_at_ingestion,
    MaterializationAssignment, TimingMemo,
};
