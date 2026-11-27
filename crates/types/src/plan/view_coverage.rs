//! A memo group's view coverage ([`view_coverage`]): the plan's content id
//! for the group's aggregate coverage, which names no group. The memo
//! derives aggregate coverage ([`AggregateCoverage`](crate::stages::AggregateCoverage)) over memo
//! groups and stores it per group (`GroupProperties::aggregate_coverage`);
//! this file expands the stored coverage's input groups and hashes the
//! result.

use serde::Serialize;

use super::content_id::content_id;
use crate::ir::Operator;
use crate::plan::physical_asap_dag::ViewCoverage;
use crate::stages::{Memo, MemoGroupId};

/// The plan-scope coverage of `group`: the content id of its expanded
/// definition and its selection; `None` for a group without aggregate
/// coverage. The definition's input groups are expanded to their
/// expressions recursively, down to the `Scan`s, so the id names no memo
/// group: two memos (or processes) that compute the same thing give the
/// same id, and different computations different ids. It is the content id
/// (`plan::content_id`) of the expanded definition. Stage 3 sets it on a materialized
/// view's plan node (`PhysicalASAPDAGNode::view_coverage`).
pub fn view_coverage(memo: &Memo, group: MemoGroupId) -> Option<ViewCoverage> {
    let coverage = memo.groups()[group.0 as usize]
        .group_properties
        .aggregate_coverage
        .as_ref()?;
    let definition = &coverage.definition;
    let expanded_definition = ExpandedDefinition {
        input: definition.input.map(|input| expanded(memo, input)),
        unlifted_operators: definition
            .unlifted_operators
            .iter()
            .map(|operator| operator.map_children(|input| input.map(|input| expanded(memo, input))))
            .collect(),
    };
    Some(ViewCoverage {
        definition_id: content_id(&expanded_definition),
        selection: coverage.selection.clone(),
    })
}

/// An aggregate definition
/// ([`AggregateDefinition`](crate::stages::AggregateDefinition)) with its input
/// groups expanded ([`expanded`]): the fields in the same order, so it
/// serializes as the definition would with each group replaced.
#[derive(Serialize)]
struct ExpandedDefinition {
    input: Option<ExpandedExpression>,
    unlifted_operators: Vec<Operator<Option<ExpandedExpression>>>,
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
