//! Pass 2's identical-expression rule (#509): structurally identical sub-DAGs
//! across queries may be computed once. Sharing changes cost
//! non-additively (a shared node is priced once), so Stage 1 keeps both the
//! independent and the shared form, and Stage 3 chooses by cost.
//!
//! Each form is a Pass 1 inventory: Pass 1 over the queries as written, and
//! Pass 1 over the queries with identical sub-DAGs merged by
//! [`share_common_sub_dags`]. A target that sharing merges is one target in
//! the shared form, so its queries take the same alternative.

use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{MetricType, RootDemand};

use super::summary_capability::share_summary_capability;
use super::window_composition::{add_window_forms, share_window_segments};
use crate::pass1::logical_candidates::{
    enumerate_local_logical_candidates, LocalLogicalCandidates, LogicalCandidateError,
};

/// Which Pass 2 sharing a Stage 1 variant applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    /// Pass 1 over the queries as written.
    Independent,
    /// The identical-expression rule: identical sub-DAGs across queries are
    /// merged, before Pass 1 and again after composition.
    IdenticalExpressions,
    /// The summary-capability rule on top of the identical-expression rule
    /// ([`super::summary_capability`]): targets that can share one summary
    /// are sized for their strictest consumer, so composition builds
    /// identical producers, which are merged.
    SummaryCapability,
    /// The shared-segment rule on top of the identical-expression rule
    /// ([`share_window_segments`]): queries over windows of one scan merge
    /// one shared summary per segment, and the identical segments are merged
    /// after composition. One variant per segmentation, by its number of
    /// distinct segments (Q67).
    WindowSegments { segments: usize },
}

impl Sharing {
    /// Whether identical sub-DAGs are merged after composition.
    pub fn merges_after_composition(self) -> bool {
        self != Sharing::Independent
    }
}

/// One sharing form of the workload, with its Pass 1 alternatives.
#[derive(Debug, Clone)]
pub struct SharingVariant<Id> {
    pub sharing: Sharing,
    pub inventory: LocalLogicalCandidates<Id>,
}

/// Stage 1 = Pass 1 + Pass 2: the independent variant first, then the
/// identical-expression variant when sharing merges at least one node, then
/// the summary-capability variant when two targets can share a summary. The
/// last is skipped when it would repeat the identical-expression variant.
/// In every variant, the window-composition rule adds tumbling forms of
/// mergeable alternatives for repeating queries (`demand[i]` is the demand
/// of `roots[i]`; a root without one gets none). Last, the shared-segment
/// variants when windows of one scan can share segments, one per
/// segmentation (Q67): only that form, not the tumbling forms, for the
/// targets it groups.
pub fn stage1_logical_candidates<Id: Clone>(
    roots: Vec<(Id, QueryRoot)>,
    metric_types: &BTreeMap<String, MetricType>,
    demand: &[RootDemand],
) -> Result<Vec<SharingVariant<Id>>, LogicalCandidateError> {
    let shared = share_identical_expressions(&roots);
    let mut variants = vec![SharingVariant {
        sharing: Sharing::Independent,
        inventory: enumerate_local_logical_candidates(roots, metric_types)?,
    }];
    if let Some(roots) = shared {
        variants.push(SharingVariant {
            sharing: Sharing::IdenticalExpressions,
            inventory: enumerate_local_logical_candidates(roots, metric_types)?,
        });
    }
    let base = &variants.last().expect("the independent variant").inventory;
    let segments = share_window_segments(base, demand)?;
    if let Some(capability) = share_summary_capability(base)? {
        if capability.resized || variants.len() == 1 {
            variants.push(SharingVariant {
                sharing: Sharing::SummaryCapability,
                inventory: capability.inventory,
            });
        }
    }
    for variant in &mut variants {
        add_window_forms(&mut variant.inventory, demand);
    }
    for (segments, inventory) in segments {
        variants.push(SharingVariant {
            sharing: Sharing::WindowSegments { segments },
            inventory,
        });
    }
    Ok(variants)
}

/// `roots` with identical operator sub-DAGs merged across queries, or `None`
/// when that merges nothing. Scalar roots are kept as written.
pub fn share_identical_expressions<Id: Clone>(
    roots: &[(Id, QueryRoot)],
) -> Option<Vec<(Id, QueryRoot)>> {
    let operators: Vec<(usize, Rc<OperatorNode>)> = roots
        .iter()
        .enumerate()
        .filter_map(|(i, (_, root))| match root {
            QueryRoot::Operator(node) => Some((i, node.clone())),
            QueryRoot::Scalar(_) => None,
        })
        .collect();
    let before = distinct_nodes(operators.iter().map(|(_, node)| node));
    let merged = share_common_sub_dags(operators);
    if distinct_nodes(merged.iter().map(|(_, node)| node)) == before {
        return None;
    }
    let mut out = roots.to_vec();
    for (i, node) in merged {
        out[i].1 = QueryRoot::Operator(node);
    }
    Some(out)
}

/// Distinct nodes (by identity) reachable from `roots`.
pub fn distinct_nodes<'a>(roots: impl IntoIterator<Item = &'a Rc<OperatorNode>>) -> usize {
    roots
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .map(|node| Rc::as_ptr(&node))
        .collect::<HashSet<_>>()
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::types::AccuracyTarget;

    fn roots(queries: &[&str]) -> Vec<(usize, QueryRoot)> {
        queries
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let root = lower_promql(q, AccuracyTarget::Exact);
                let root =
                    asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
                (i, QueryRoot::Operator(root))
            })
            .collect()
    }

    /// Two queries over the same range selector get an independent and a
    /// shared variant, with the same targets in each.
    #[test]
    fn identical_input_adds_a_shared_variant() {
        let variants = stage1_logical_candidates(
            roots(&[
                "sum by (job) (rate(m[1m]))",
                "topk by (job) (10, sum_over_time(m[1m]))",
            ]),
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        assert_eq!(
            variants.iter().map(|v| v.sharing).collect::<Vec<_>>(),
            [Sharing::Independent, Sharing::IdenticalExpressions]
        );
        assert_eq!(
            variants[0].inventory.targets.len(),
            variants[1].inventory.targets.len()
        );
        let operators = |v: &SharingVariant<usize>| -> Vec<Rc<OperatorNode>> {
            v.inventory
                .roots
                .iter()
                .map(|(_, r)| match r {
                    QueryRoot::Operator(n) => n.clone(),
                    QueryRoot::Scalar(_) => unreachable!(),
                })
                .collect()
        };
        assert!(
            distinct_nodes(&operators(&variants[1])) < distinct_nodes(&operators(&variants[0]))
        );
    }

    /// Queries with nothing in common have no shared variant.
    #[test]
    fn nothing_identical_adds_no_variant() {
        let variants = stage1_logical_candidates(
            roots(&["sum(rate(a[1m]))", "sum(rate(b[1m]))"]),
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].sharing, Sharing::Independent);
    }
}
