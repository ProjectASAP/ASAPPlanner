//! What the rules share: the output contract a candidate must keep.
use asap_types::ir::OperatorNode;

/// Whether `actual` has `expected`'s output: the same result kind, fields,
/// time index and unique keys, and a closed label set where `expected`'s is
/// closed (a projection may enumerate every output of an open schema).
pub(in crate::pass2::memo) fn same_output_contract(
    expected: &OperatorNode,
    actual: &OperatorNode,
) -> bool {
    expected.result_kind == actual.result_kind
        && expected.schema.fields == actual.schema.fields
        && expected.schema.time_index == actual.schema.time_index
        && expected.schema.unique_keys == actual.schema.unique_keys
        && (!expected.schema.closed || actual.schema.closed)
}
