//! Helpers that only other crates' tests call, exposed under the
//! `test-support` feature so the normal public API stays as narrow as its
//! production users need. Each forwards to the crate-private function or
//! constant of the same name.
use std::rc::Rc;

use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::properties::guarantee::AccuracyGuarantee;
use asap_types::ir::schema::{FieldDataType, SketchStatistic};
use asap_types::ir::{OperatorNode, ScalarExpr};

use crate::composition::StreamNorm;
pub use crate::evidence::PropagationStats;
use crate::propagation;

/// [`propagation::composed_guarantee`].
pub fn composed_guarantee(node: &OperatorNode) -> Option<AccuracyGuarantee> {
    propagation::composed_guarantee(node)
}

/// `propagation::scalar_guarantee`.
pub fn scalar_guarantee(
    expr: &ScalarExpr,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    propagation::scalar_guarantee(expr, column)
}

/// `propagation::stream_norm`.
pub fn stream_norm(node: &Rc<OperatorNode>) -> Option<StreamNorm> {
    propagation::stream_norm(node)
}

/// `propagation::fold_stats`.
pub fn fold_stats(reduction: &Reduction, input: &Rc<OperatorNode>) -> Option<PropagationStats> {
    propagation::fold_stats(reduction, input)
}

/// `propagation::estimates_raw_data_samples_of`.
pub fn estimates_raw_data_samples_of(node: &OperatorNode) -> Option<String> {
    propagation::estimates_raw_data_samples_of(node)
}

/// `propagation::source_distinct_guarantee`.
pub fn source_distinct_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
    input: &Rc<OperatorNode>,
) -> Option<AccuracyGuarantee> {
    propagation::source_distinct_guarantee(family, query, input)
}

/// `crate::estimators::cms::cms_depth`.
pub fn cms_depth(delta: f64) -> u32 {
    crate::estimators::cms::cms_depth(delta)
}

/// `crate::estimators::kll::kll_rank_error_99`.
pub fn kll_rank_error_99(k: u32) -> f64 {
    crate::estimators::kll::kll_rank_error_99(k)
}

/// `crate::composition::QUOTIENT_OPERAND_DOMAINS`.
pub const QUOTIENT_OPERAND_DOMAINS: &str = crate::composition::QUOTIENT_OPERAND_DOMAINS;

/// `crate::composition::STREAM_ROW_COUNT`.
pub const STREAM_ROW_COUNT: &str = crate::composition::STREAM_ROW_COUNT;

/// `crate::composition::source_norm_statistic`.
pub fn source_norm_statistic(norm: &str, source: &str) -> String {
    crate::composition::source_norm_statistic(norm, source)
}

/// `crate::composition::raw_data_samples_sign_statistic`.
pub fn raw_data_samples_sign_statistic(metric: &str) -> String {
    crate::composition::raw_data_samples_sign_statistic(metric)
}

/// `stats.sign_statistic` (see [`PropagationStats`]).
pub fn sign_statistic(stats: &PropagationStats) -> Option<&str> {
    stats.sign_statistic.as_deref()
}
