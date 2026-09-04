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

pub mod expr;
pub mod guarantee;
pub mod query_time;
pub mod schema;
pub mod sketch;
pub mod summary_maintenance;
pub mod summary_maintenance_lifecycle;
pub mod summary_window;

pub use expr::{SummaryExpr, SummaryNode};
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
    default_hydra_params, hydra_kind_for, ExactKind, ExactParams, GroupingStrategy, HydraKind,
    HydraParams, SamplingKind, SamplingParams, SketchAlgorithm, SketchCategory, SketchKind,
    SketchParams, SketchQuery, StatModelKind, StatModelParams, SummaryInput, SummaryKey,
    SummaryValue, WaveletKind, WaveletParams,
};
pub use summary_maintenance::SummaryMaintenanceMode;
pub use summary_maintenance_lifecycle::{
    EvaluationSchedule, OutputRepresentation, SummaryMaintenanceLifecycle,
    SummaryMaintenanceLifecycleGuarantee,
};
pub use summary_window::SummaryWindowFramework;
