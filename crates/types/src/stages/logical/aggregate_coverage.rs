//! Memo scope of aggregate coverage: the IR derives it
//! ([`crate::ir::properties::aggregate_coverage`]) over memo groups
//! (`R` = [`MemoGroupId`]) and the memo stores it per group
//! (`GroupProperties::aggregate_coverage`); here the plan's view of it, a
//! content id that names no group ([`view_coverage`]).

use serde::Serialize;

use crate::ir::Operator;
use crate::plan::physical_asap_dag::ViewCoverage;
use crate::stages::{Memo, MemoGroupId};

/// The plan-scope coverage of `group`: the content id of its expanded
/// definition and its selection; `None` for a group without aggregate
/// coverage. The definition's input groups are expanded to their
/// expressions recursively, down to the `Scan`s, so the id names no memo
/// group: two memos (or processes) that compute the same thing give the
/// same id, and different computations different ids. It is the 128-bit
/// FNV-1a hash, in hex, of the expanded definition's JSON (non-finite
/// floats serialized by `serde_f64`). Stage 3 sets it on a materialized
/// view's plan node (`PhysicalASAPDAGNode::view_coverage`).
pub fn view_coverage(memo: &Memo, group: MemoGroupId) -> Option<ViewCoverage> {
    let coverage = memo.groups()[group.0 as usize]
        .group_properties
        .aggregate_coverage
        .clone()?;
    let expanded = coverage.map_references(|input| expanded(memo, input));
    let serialized =
        serde_json::to_string(&expanded.definition).expect("an expanded definition serializes");
    Some(ViewCoverage {
        definition_id: fnv1a_128_hex(&serialized),
        selection: expanded.selection,
    })
}

/// What a memo group computes with no ids: its expression over its input
/// groups' expressions, expanded the same way down to the `Scan`s.
#[derive(Serialize)]
#[serde(transparent)]
struct ExpandedExpression(Box<Operator<ExpandedExpression>>);

fn expanded(memo: &Memo, group: MemoGroupId) -> ExpandedExpression {
    ExpandedExpression(Box::new(
        memo.groups()[group.0 as usize]
            .expression
            .map_children(|&input| expanded(memo, input)),
    ))
}

/// The 128-bit FNV-1a hash of `text`, in hex: stable across processes and
/// Rust versions.
fn fnv1a_128_hex(text: &str) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let hash = text.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u128::from(byte)).wrapping_mul(PRIME)
    });
    format!("{hash:032x}")
}
