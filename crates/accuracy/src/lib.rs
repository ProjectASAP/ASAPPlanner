//! `asap-accuracy` — the accuracy rules the planning stages share: the
//! theoretical guarantee of reading a statistic from a summary with given
//! parameters ([`theoretical_guarantee`], from the per-algorithm
//! guarantees in `guarantee`), the deployment's measured accuracy
//! ([`AccuracyEvidence`], in `evidence`), a target's comparison limit
//! [`target_limit`], $\alpha^{\log}$ of a logical edge
//! ([`producer_satisfies_logical_requirements`], in `requirements`), and
//! each operator's error contract ([`error_propagation`], in
//! `error_propagation`), applied per edge.
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.
//!
//! It depends only on `asap-types`, never on a stage crate, so every stage
//! can use it.

mod error_propagation;
mod evidence;
pub mod guarantee;
mod requirements;
pub mod sizing;

pub use error_propagation::{error_propagation, ErrorPropagation};
pub use evidence::{AccuracyEvidence, NoMeasuredGuarantees};
pub use guarantee::theoretical_guarantee;
pub use requirements::{
    accepted_metrics, producer_satisfies_logical_requirements, readout_metric, target_limit,
};
