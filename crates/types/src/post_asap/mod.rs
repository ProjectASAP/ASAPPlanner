//! The post-ASAP IR: summary-bound types, distinct from
//! [`crate::pre_asap`]'s pre-ASAP IR.
//!
//! Where [`crate::pre_asap`] carries *intent* only ("compute a
//! quantile to ε accuracy"), this module is the summary-bound IR: the
//! summary family, kind/algorithm, and parameters are committed (one
//! `(Kind, Params)` pair per family — [`sketch::ExactKind`]/[`sketch::ExactParams`],
//! [`sketch::SamplingKind`]/[`sketch::SamplingParams`],
//! [`sketch::WaveletKind`]/[`sketch::WaveletParams`],
//! [`sketch::StatModelKind`]/[`sketch::StatModelParams`]), and
//! [`expr::SummaryNode`] / [`expr::SummaryExpr`] describe the summary
//! computation. The `Sketch` family is the one exception to that
//! one-pair-per-family shape: it nests a third level, [`sketch::SketchKind`]
//! (quantile/cardinality/frequency/top-k), which itself carries the
//! committed [`sketch::SketchAlgorithm`] and [`sketch::SketchParams`] —
//! `SummaryFamilyType::Sketch(SketchKind, GroupingStrategy)`, not a flat
//! `(kind, params)` pair
//! — because `Sketch` is the one family with more than one algorithm per
//! purpose today; no other family needs that extra level yet.
//!
//! A second, orthogonal axis lives here too: [`sketch::GroupingStrategy`]
//! (issue #256) — *how many* physical instances of a chosen family/kind
//! exist across a grouped aggregate's `by` subpopulations
//! (`PerSubpopulationInstance`, today's only behavior, vs.
//! `SharedMultiSubpopulation`/Hydra — see [`sketch::HydraKind`]/
//! [`sketch::HydraParams`]), carried on [`expr::SummaryExpr::SummaryAgg`]
//! alongside `reduction` and on sketch-valued edge types
//! — see `asap_aware_mapping::grouping`'s module docs for why.

pub mod cse;
pub mod executable_dag;
pub mod execution_data_state;
pub mod expr;
pub mod guarantee;
pub mod query_time;
pub mod schema;
pub mod sketch;
pub mod summary_maintenance;
pub mod summary_maintenance_lifecycle;
pub mod summary_window;

pub use cse::share_common_summary_subtrees;
pub use executable_dag::{
    compile_executable_dag, compile_executable_dag_with_node_ids, EdgeRole, ExecutableDag,
    ExecutableDagCompilation, ExecutableDagEdge, ExecutableDagNode, ExecutableNodeIdentityMap,
    ExecutableOperator, ExecutableOperatorPayload, GroupingEdgeCompatibility,
    WindowEdgeCompatibility,
};
pub use execution_data_state::{
    assigned_child_data_state, exact_operation_output_schema, produced_data_state,
    validate_execution_data_states, validate_execution_data_states_at, DataPrimitive,
    ExactOperationSchemaError, ExecutionDataState, ExecutionDataStateAssignment,
    ExecutionDataStateError, ExecutionTiming,
};
pub use expr::{
    BinaryOperator, CandidateCompleteness, ExactOperation, SummaryExpr, SummaryNode, ValueOperation,
};
pub use guarantee::{
    AccuracyError, BoundExpr, CompositionOperator, ErrorMetric, GuaranteeSource, ProbabilityExpr,
    ResultGuarantee,
};
pub use query_time::{
    classic_cms_sizing, cms_posterior_error_bound, count_sketch_posterior_error_bound,
    cu_sketch_posterior_error_bound, traditional_a_priori_bound,
};
pub use schema::{SummaryFamilyType, SummaryField, SummarySchema};
pub use sketch::{
    default_hydra_params, hydra_kind_for, EntityIdentity, ExactKind, ExactParams, GroupingStrategy,
    HydraKind, HydraParams, NonNegativeWeightProof, SamplingKind, SamplingParams, SketchAlgorithm,
    SketchCategory, SketchKind, SketchParams, SketchQuery, StatModelKind, StatModelParams,
    SummaryInputExpr, SummaryUpdate, WaveletKind, WaveletParams, WeightDomain,
};
pub use summary_maintenance::SummaryMaintenanceMode;
pub use summary_maintenance_lifecycle::{
    EvaluationSchedule, OutputRepresentation, SummaryMaintenanceLifecycle,
    SummaryMaintenanceLifecycleGuarantee,
};
pub use summary_window::{
    plan_pane_phase, validate_pane_coverage, BoundaryCoverage, PaneCoverageError, PanePhaseBinding,
    SummaryWindowFramework,
};
