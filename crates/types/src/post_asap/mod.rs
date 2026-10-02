//! Summary-state types and the execution-timing vocabulary of the operator
//! IR ([`crate::ir`]).
//!
//! Where an intent ([`crate::pre_asap::AggIntent`]) says *what* to compute
//! ("a quantile to ε accuracy"), these types say *how* a summary realizes it:
//! the family, kind/algorithm and parameters are committed — one
//! `(Kind, Params)` pair per family ([`sketch::ExactKind`]/[`sketch::ExactParams`],
//! [`sketch::SamplingKind`]/[`sketch::SamplingParams`],
//! [`sketch::WaveletKind`]/[`sketch::WaveletParams`],
//! [`sketch::StatModelKind`]/[`sketch::StatModelParams`]). The `Sketch` family
//! nests a third level, [`sketch::SketchKind`] (quantile/cardinality/
//! frequency/top-k), carrying the committed [`sketch::SketchAlgorithm`] and
//! [`sketch::SketchParams`], because it is the one family with more than one
//! algorithm per purpose.
//!
//! [`sketch::GroupingStrategy`] is a second, orthogonal axis: how many
//! physical instances of a summary exist across a grouped aggregate's `by`
//! subpopulations (per-subpopulation vs. one shared Hydra instance — see
//! `asap_aware_mapping::grouping`). It rides on `ASAPOp::SummaryAgg` and on
//! sketch-valued edge types.
//!
//! The rest: accuracy guarantees ([`guarantee`]), maintained populations,
//! summary windows and maintenance lifecycle, and the execution timing /
//! data-state vocabulary ([`execution_data_state`]).

pub mod execution_data_state;
pub mod guarantee;
pub mod maintained_population;
pub mod query_time;
pub mod sketch;
pub mod summary_maintenance;
pub mod summary_maintenance_lifecycle;
pub mod summary_window;

pub use crate::pre_asap::schema::{Field, FieldDataType, Schema};
pub use execution_data_state::{
    lift_plain, DataPrimitive, ExactOperationSchemaError, ExecutionDataState,
    ExecutionDataStateError, ExecutionTiming,
};
pub use guarantee::{
    AccuracyError, BoundExpr, CompositionOperator, ErrorMetric, GuaranteeSource, ProbabilityExpr,
    ResultGuarantee,
};
pub use query_time::{
    classic_cms_sizing, cms_posterior_error_bound, count_sketch_posterior_error_bound,
    cu_sketch_posterior_error_bound, traditional_a_priori_bound,
};
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
    plan_pane_phase, validate_pane_coverage, PaneCoverageError, PaneLayout, SummaryWindowFramework,
    WindowEdgeCoverage,
};
