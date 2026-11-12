//! `asap-accuracy` — the accuracy rules the planning stages share: the
//! theoretical guarantee of reading a statistic from a summary with given
//! parameters ([`theoretical_guarantee`], from the per-algorithm
//! guarantees in `guarantee`), the deployment's measured accuracy
//! ([`AccuracyEvidence`], in `evidence`), each operator's error contract
//! ([`ErrorContract`], in `error_contract`), the error metrics of a memo
//! and the metric rule ([`ErrorMetricsOfMemo`], in `metric_rule`, with the
//! metric of a readout's error from `readout`), each readout's required
//! accuracy ([`readout_levels`], in `readout_caps`), and $\alpha^{\log}$ of
//! a logical edge ([`ErrorMetricsOfMemo::edge_meets_logical_requirements`],
//! in `logical_edge`, with a bound's comparison to a limit
//! [`within_limit`]).
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.
//!
//! It depends only on `asap-types`, never on a stage crate, so every stage
//! can use it.

mod error_contract;
mod evidence;
mod guarantee;
mod logical_edge;
mod metric_rule;
mod readout;
mod readout_caps;

pub use error_contract::ErrorContract;
pub use evidence::{AccuracyEvidence, NoMeasuredGuarantees};
pub use guarantee::theoretical_guarantee;
pub use logical_edge::within_limit;
pub use metric_rule::ErrorMetricsOfMemo;
pub use readout_caps::{readout_levels, ReadoutRequiredAccuracy};
