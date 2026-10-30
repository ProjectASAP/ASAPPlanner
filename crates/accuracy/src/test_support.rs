//! Propagation helpers that only other crates' unit tests call, exposed
//! under the `test-support` feature so the normal public API stays as
//! narrow as its production users need. Each forwards to the
//! `crate::propagation` function of the same name.
use std::rc::Rc;

use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::properties::guarantee::AccuracyGuarantee;
use asap_types::ir::schema::{FieldDataType, SketchStatistic, SummaryUpdate};
use asap_types::ir::{OperatorNode, ScalarExpr};

use crate::composition::StreamNorm;
use crate::propagation;
use crate::PropagationStats;

/// [`propagation::composed_guarantee`].
pub fn composed_guarantee(node: &OperatorNode) -> Option<AccuracyGuarantee> {
    propagation::composed_guarantee(node)
}

/// [`propagation::scalar_guarantee`].
pub fn scalar_guarantee(
    expr: &ScalarExpr,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    propagation::scalar_guarantee(expr, column)
}

/// [`propagation::stream_norm`].
pub fn stream_norm(node: &Rc<OperatorNode>) -> Option<StreamNorm> {
    propagation::stream_norm(node)
}

/// [`propagation::fold_stats`].
pub fn fold_stats(reduction: &Reduction, input: &Rc<OperatorNode>) -> Option<PropagationStats> {
    propagation::fold_stats(reduction, input)
}

/// [`propagation::estimates_raw_data_samples_of`].
pub fn estimates_raw_data_samples_of(node: &OperatorNode) -> Option<String> {
    propagation::estimates_raw_data_samples_of(node)
}

/// [`propagation::estimate_has_bound`].
pub fn estimate_has_bound(
    family: &FieldDataType,
    update: &SummaryUpdate,
    query: &SketchStatistic,
) -> bool {
    propagation::estimate_has_bound(family, update, query)
}

/// [`propagation::estimate_bound_over_raw_data_samples`].
pub fn estimate_bound_over_raw_data_samples(
    family: &FieldDataType,
    update: &SummaryUpdate,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    propagation::estimate_bound_over_raw_data_samples(family, update, query)
}

/// [`propagation::source_distinct_guarantee`].
pub fn source_distinct_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
    input: &Rc<OperatorNode>,
) -> Option<AccuracyGuarantee> {
    propagation::source_distinct_guarantee(family, query, input)
}
