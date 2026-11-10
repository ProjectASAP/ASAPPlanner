//! The one equality that makes two expressions one memo group, and two
//! candidates of a group one candidate: [`same_serialized_expression`] (operator,
//! arguments and input groups). [`Memo::find_or_add_group`] looks a group up
//! by it, Stage 0 included, and [`Memo::add_candidate`] finds a duplicate by
//! it. Parameters θ are not an argument of any operator (they are a
//! candidate's `LogicalProperties`), so builds of one family over one input
//! that differ only in θ are one group.
//!
//! [`Memo::find_or_add_group`]: super::Memo::find_or_add_group
//! [`Memo::add_candidate`]: super::Memo::add_candidate

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::MemoGroupId;
use crate::ir::Operator;
use crate::stages::CandidateForMemoGroup;

/// `expression` with every input group replaced by `()`: the operator and
/// its arguments.
fn operator_and_arguments(expression: &Operator<MemoGroupId>) -> Operator<()> {
    expression.map_children(|_| ())
}

/// The canonical JSON of `expression`'s operator and arguments, serialized
/// once per expression: what [`expression_hash`] hashes and
/// [`same_serialized_expression`] compares; `None` when it does not
/// serialize.
pub(super) fn serialized_operator(expression: &Operator<MemoGroupId>) -> Option<String> {
    serde_json::to_string(&operator_and_arguments(expression)).ok()
}

/// A hash consistent with [`same_serialized_expression`], used only to narrow the
/// groups compared: SipHash over `serialized`, the expression's
/// [`serialized_operator`], then the input groups. A non-finite `f64`
/// serializes as `null`, which only widens one bucket;
/// [`same_serialized_expression`] decides.
pub(super) fn expression_hash(expression: &Operator<MemoGroupId>, serialized: Option<&str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    serialized.unwrap_or_default().hash(&mut hasher);
    expression.children().hash(&mut hasher);
    hasher.finish()
}

/// Whether `left` and `right` are one expression: equal operators and
/// arguments, by typed equality confirmed by equal serializations (each
/// side's [`serialized_operator`], computed once by the caller), over the
/// same input groups at the same positions.
pub(super) fn same_serialized_expression(
    (left, left_serialized): (&Operator<MemoGroupId>, Option<&str>),
    (right, right_serialized): (&Operator<MemoGroupId>, Option<&str>),
) -> bool {
    left.children() == right.children()
        && operator_and_arguments(left) == operator_and_arguments(right)
        && left_serialized.is_some()
        && left_serialized == right_serialized
}

/// [`same_serialized_expression`], serializing both sides.
fn same_expression(left: &Operator<MemoGroupId>, right: &Operator<MemoGroupId>) -> bool {
    same_serialized_expression(
        (left, serialized_operator(left).as_deref()),
        (right, serialized_operator(right).as_deref()),
    )
}

/// Typed equality, confirmed by equal serializations: numeric `PartialEq`
/// alone conflates signed zeros, and `SketchParams`' equality omits the
/// recorded error and failure probability. The serialized check is
/// additional evidence, never a replacement for typed equality (JSON maps
/// non-finite floats to `null`).
pub(super) fn same_value<T: PartialEq + serde::Serialize>(left: &T, right: &T) -> bool {
    left == right
        && match (serde_json::to_string(left), serde_json::to_string(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

/// Whether `left` and `right` are one candidate of a group: the same
/// expression ([`same_expression`]) and logical properties (parameters by
/// [`same_value`]).
pub(super) fn same_expression_and_logical_properties(
    left: &CandidateForMemoGroup,
    right: &CandidateForMemoGroup,
) -> bool {
    same_expression(&left.expression.operator, &right.expression.operator)
        && same_value(
            &left.logical_properties.parameters,
            &right.logical_properties.parameters,
        )
}
