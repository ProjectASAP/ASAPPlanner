//! Summary types: each summary type's parameters $\boldsymbol\theta$ and
//! predefined parameter lists, what a summary is updated with and the
//! statistics read from it, and each summary type's capabilities.

mod capability;
mod family;
mod parameter_lists;
mod parameters;
mod statistic;
mod update;

/// A specific sketch algorithm — bounded error, sized by its
/// [`SketchParams`]. It is `asap_sketchlib`'s family enum; the statistics
/// each answers are its row of the family-capability table
/// ([`SummaryTypeCapability`]). The planner
/// sizes and offers only the families [`SketchParams`] has a variant for.
pub use asap_sketchlib::SketchAlgorithm;
pub use capability::{
    MergeInputRule, MergedError, StatisticKind, SummaryTypeCapability, SupportedSummary,
};
pub use family::SummaryFamily;
pub use parameter_lists::parameter_list;
pub use parameters::{
    CounterReadout, ExactKind, ExactParams, HydraParams, SamplingKind, SamplingParams,
    SketchParameterRange, SketchParams, StatModelKind, StatModelParams, WaveletKind, WaveletParams,
    KLL_MAX_K,
};
pub use statistic::SketchStatistic;
pub use update::{EntityIdentity, SummaryInputExpr, SummaryUpdate};
