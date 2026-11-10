//! The one equality that makes two expressions one memo group, and two
//! candidates of a group one candidate: typed equality of the operator, its
//! arguments and input groups, confirmed by equal serializations
//! ([`same_value`]; [`same_serialized_expression`] with each side's
//! serialization computed once). [`Memo::find_or_add_group`] looks a group up
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

/// The canonical JSON of `expression` (operator, arguments and input
/// groups), serialized once per expression: what [`expression_hash`] hashes
/// and [`same_serialized_expression`] compares.
pub(super) fn serialized_expression(expression: &Operator<MemoGroupId>) -> String {
    serde_json::to_string(expression).expect("an expression over memo groups serializes")
}

/// A hash consistent with [`same_serialized_expression`], used only to
/// narrow the groups compared: SipHash over the expression's
/// [`serialized_expression`]. A non-finite `f64` serializes as `null`,
/// which only widens one bucket; [`same_serialized_expression`] decides.
pub(super) fn expression_hash(serialized: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    hasher.finish()
}

/// Whether `left` and `right` are one expression: typed equality (operator,
/// arguments and input groups) confirmed by equal serializations (each
/// side's [`serialized_expression`], computed once by the caller), as
/// [`same_value`] decides it.
pub(super) fn same_serialized_expression(
    (left, left_serialized): (&Operator<MemoGroupId>, &str),
    (right, right_serialized): (&Operator<MemoGroupId>, &str),
) -> bool {
    left == right && left_serialized == right_serialized
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
/// expression and logical properties (parameters), each by [`same_value`]:
/// typed equality first, serialized only when typed-equal.
pub(super) fn same_expression_and_logical_properties(
    left: &CandidateForMemoGroup,
    right: &CandidateForMemoGroup,
) -> bool {
    same_value(&left.expression.operator, &right.expression.operator)
        && same_value(
            &left.logical_properties.parameters,
            &right.logical_properties.parameters,
        )
}
