//! The operator IR from #511: one operator DAG for every planning stage.
pub mod aggregate_schema;
pub mod asap;
pub mod error;
pub mod node;
pub mod non_asap;
pub mod operator_properties;
pub mod query;
pub mod scalar;
pub use asap::{ASAPOp, UNIMPLEMENTED_ASAP_OP};
pub use error::SchemaDerivationError;
pub use node::{Operator, OperatorNode, OperatorResultKind};
pub use non_asap::{BinaryOperator, NonASAPOp, TimeRangeKind};
pub use query::QueryRoot;
pub use scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};

pub mod canonicalize;
pub mod cse;
pub mod flat;
/// Physical ASAP DAG: the flattened operators plus execution timing.
pub mod physical_export;
/// Execution timing for physical plans: a lifecycle assignment expanded onto every node.
pub mod timing;
pub use timing::{
    apply_lifecycle_timings, data_state, planned_data_state, split_shared_by_phase,
    validate_default, LifecycleAssignment, TimingMemo,
};
pub mod schema_support;
/// Semantic observation coverage, separate from field layout and physical timing.
pub mod summary_coverage;
