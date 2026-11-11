//! `asap-accuracy` — the accuracy rules the planning stages share: the
//! theoretical guarantee of reading a statistic from a summary with given
//! parameters ([`theoretical_guarantee`], from the per-algorithm
//! guarantees in `guarantee`) and a target's comparison limit
//! ([`target_limit`]), the deployment's measured accuracy
//! ([`AccuracyEvidence`], in `evidence`), each operator's error contract
//! ([`error_propagation`], in `error_propagation`), the metric rule
//! ([`accepted_metrics`], in `metric_rule`), each readout's required
//! accuracy ([`readout_levels`], in `readout_caps`), and $\alpha^{\log}$ of
//! a logical edge ([`producer_satisfies_logical_requirements`], in
//! `logical_edge`).
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.
//!
//! It depends only on `asap-types`, never on a stage crate, so every stage
//! can use it.

mod error_propagation;
mod evidence;
pub mod guarantee;
mod logical_edge;
mod metric_rule;
mod readout_caps;

pub use error_propagation::{error_propagation, ErrorPropagation};
pub use evidence::{AccuracyEvidence, NoMeasuredGuarantees};
pub use guarantee::{target_limit, theoretical_guarantee};
pub use logical_edge::producer_satisfies_logical_requirements;
pub use metric_rule::{accepted_metrics, readout_metric, sketch_readout_metric};
pub use readout_caps::readout_levels;
