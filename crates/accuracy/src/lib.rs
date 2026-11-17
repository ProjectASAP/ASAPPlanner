//! `asap-accuracy` — the accuracy rules the planning stages share: the
//! theoretical guarantee of reading a statistic from a summary with given
//! parameters ([`theoretical_guarantee`], from the per-algorithm
//! guarantees in `guarantee`), the deployment's measured accuracy
//! ([`AccuracyEvidence`], in `evidence`, and the measured table
//! [`MeasuredSketchAccuracy`], in `measured`), each operator's error contract
//! ([`ErrorContract`], in `error_contract`), the error metrics of a memo
//! and the metric rule ([`ErrorMetricsOfMemo`], in `metric_rule`, with the
//! metrics each query root allows, [`query_roots_with_target`], and the
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
mod estimate_dependence;
mod evidence;
mod guarantee;
mod logical_edge;
mod measured;
mod metric_rule;
mod readout;
mod readout_caps;

pub use error_contract::ErrorContract;
pub use estimate_dependence::{
    inputs_read_value_by_value, ranks_by_estimate, takes_each_value_from_one_input,
};
pub use evidence::{AccuracyEvidence, NoMeasuredGuarantees};
pub use guarantee::theoretical_guarantee;
pub use logical_edge::{tolerated_limit, within_limit};
pub use measured::{
    ErrorAtFailureProbability, MeasuredAccuracyEntry, MeasuredSketchAccuracy,
    MeasuredSketchAccuracyError,
};
pub use metric_rule::{query_roots_with_target, ErrorMetricsOfMemo};
pub use readout_caps::{readout_levels, readout_value_counts_of_queries, ReadoutRequiredAccuracy};
