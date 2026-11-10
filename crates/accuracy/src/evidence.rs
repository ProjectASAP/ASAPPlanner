//! The deployment's measured accuracy, read before each summary's
//! theoretical guarantee.

use asap_types::accuracy::AccuracyGuarantee;
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchParams, SketchStatistic};

/// The deployment's measured accuracy, per summary configuration. A
/// producer's guarantee for a readout is its measurement when there is one,
/// else [`theoretical_guarantee`](crate::guarantee::theoretical_guarantee);
/// the source is decided before Pass 2 builds the logical edges (concepts
/// doc, section 2, "Where the guarantee comes from").
pub trait AccuracyEvidence {
    /// The measured error bound of reading `statistic` from a state of
    /// `summary_type` with parameters `parameters` at failure probability
    /// `failure_probability` (the guarantee's own failure probability);
    /// `None` when the deployment has no measurement for it, or when this θ
    /// cannot fail that rarely.
    fn measured_guarantee(
        &self,
        summary_type: &FieldDataType,
        parameters: &SketchParams,
        statistic: &SketchStatistic,
        failure_probability: f64,
    ) -> Option<AccuracyGuarantee>;
}

/// Evidence with no measurement: every guarantee is the theoretical one.
#[derive(Debug, Clone, Copy)]
pub struct NoMeasuredGuarantees;

impl AccuracyEvidence for NoMeasuredGuarantees {
    fn measured_guarantee(
        &self,
        _summary_type: &FieldDataType,
        _parameters: &SketchParams,
        _statistic: &SketchStatistic,
        _failure_probability: f64,
    ) -> Option<AccuracyGuarantee> {
        None
    }
}
