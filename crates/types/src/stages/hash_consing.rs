//! The one equality that makes two expressions one memo group, and two
//! candidates of a group one candidate: [`same_expression`] (operator,
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

/// `expression` with every input group replaced by `()`: the operator and
/// its arguments.
fn operator_and_arguments(expression: &Operator<MemoGroupId>) -> Operator<()> {
    expression.map_children(|_| ())
}

/// Whether `left` and `right` are one expression: equal operators and
/// arguments ([`same_value`]) over the same input groups at the same
/// positions.
pub(super) fn same_expression(left: &Operator<MemoGroupId>, right: &Operator<MemoGroupId>) -> bool {
    left.children() == right.children()
        && same_value(
            &operator_and_arguments(left),
            &operator_and_arguments(right),
        )
}

/// A hash consistent with [`same_expression`], used only to narrow the
/// groups compared: SipHash over the canonical JSON of the operator and its
/// arguments, then the input groups. A non-finite `f64` serializes as
/// `null`, which only widens one bucket; [`same_expression`] decides.
pub(super) fn expression_hash(expression: &Operator<MemoGroupId>) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(&operator_and_arguments(expression))
        .unwrap_or_default()
        .hash(&mut hasher);
    expression.children().hash(&mut hasher);
    hasher.finish()
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
