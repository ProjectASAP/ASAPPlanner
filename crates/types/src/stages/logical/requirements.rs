//! Candidate-input scope: each candidate's input requirements
//! $r^{\log}_{c,i}$ (concepts doc `memo-groups-properties-requirements.md`,
//! section 2).

use crate::accuracy::AccuracyMetric;

/// $r^{\log}_{c,i}$: only what $G_{c,i}$'s identity does not fix. Only a
/// readout's input (`SummaryEstimate`, `FinalizeExactAccumulator`,
/// `EvaluatePopulation` over an approximate state) carries one (concepts
/// doc, section 2, "Logical requirements").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogicalRequirements {
    /// `None` on every input that is not a readout's, and on a readout's
    /// input when no accuracy target reaches it.
    pub required_accuracy: Option<RequiredAccuracy>,
}

/// The accuracy a readout's input needs from its producer: the producer's
/// guarantee for the readout's statistic, evaluated at
/// `failure_probability`, is at most `max_error` in `metric`. It is also
/// the readout's own output error.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct RequiredAccuracy {
    pub metric: AccuracyMetric,
    /// $\varepsilon_r$, in the metric's units.
    pub max_error: f64,
    /// $\delta_r$, per value; 1 when the target sets none.
    pub failure_probability: f64,
}
