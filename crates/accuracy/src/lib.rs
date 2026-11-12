//! `asap-accuracy` — the accuracy rules the planning stages share: the
//! theoretical guarantee of reading a statistic from a summary with given
//! parameters ([`theoretical_guarantee`], from the per-algorithm
//! guarantees in `guarantee`), the deployment's measured accuracy
//! ([`AccuracyEvidence`], in `evidence`), each operator's error contract
//! ([`error_propagation`], in `error_propagation`), the metric of a
//! readout's error ([`readout_metric`], in `readout`), the metric rule
//! ([`accepted_metrics`], in `metric_rule`), each readout's required
//! accuracy ([`readout_levels`], in `readout_caps`), and $\alpha^{\log}$ of
//! a logical edge ([`producer_satisfies_logical_requirements`], in
//! `logical_edge`, with a requirement's comparison limit [`target_limit`]).
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.
//!
//! It depends only on `asap-types`, never on a stage crate, so every stage
//! can use it.

mod error_propagation;
mod evidence;
mod guarantee;
mod logical_edge;
mod metric_rule;
mod readout;
mod readout_caps;

pub use error_propagation::{error_propagation, metrics_carried, ErrorPropagation};
pub use evidence::{AccuracyEvidence, NoMeasuredGuarantees};
pub use guarantee::theoretical_guarantee;
pub use logical_edge::{producer_satisfies_logical_requirements, target_limit};
pub use metric_rule::{accepted_metrics, metrics_accepted_on_inputs};
pub use readout::{readout_metric, readout_metric_of_candidate};
pub use readout_caps::readout_levels;
