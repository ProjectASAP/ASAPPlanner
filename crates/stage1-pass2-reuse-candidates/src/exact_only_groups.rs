//! The groups each query may read only exactly
//! (`LogicalASAPMemoCandidates::exact_only_groups_of_query`): the metric
//! rule per query, where dead-candidate elimination applies it per group.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use asap_accuracy::error_propagation;
use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::QueryRoot;
use asap_types::stages::{Memo, MemoGroupId};

/// Per query: the groups below its root where an error in no metric of its
/// target reaches the root bounded, with every group below them. Walking
/// down from each root with a target, which accepts the target's metric
/// (every metric when it names none), a candidate passes to its inputs the
/// metrics its error contract carries (`error_propagation`); a group
/// keeps, per query, the metrics every path to it carries, so one path
/// through a consumer without a contract (a comparison, a sum) makes it
/// exact-only for that query. `read_only_exactly` adds, per query, groups
/// listed by the readout requirements (a readout whose level another query
/// gave, for a query whose failure probability has no per-value one).
/// `order` is the memo's topological order (`Memo::topological_order`).
pub(crate) fn exact_only_groups_of_query(
    memo: &Memo,
    order: &[MemoGroupId],
    read_only_exactly: &BTreeMap<usize, BTreeSet<MemoGroupId>>,
) -> Vec<BTreeSet<MemoGroupId>> {
    let mut accepted: Vec<BTreeMap<usize, Vec<AccuracyMetric>>> =
        vec![BTreeMap::new(); memo.groups().len()];
    for (query, memo_query) in memo.queries().iter().enumerate() {
        let (Some(target), QueryRoot::Operator(root)) =
            (&memo_query.accuracy_target, &memo_query.root)
        else {
            continue;
        };
        let metrics = AccuracyMetric::ALL
            .into_iter()
            .filter(|&metric| target.accuracy_metric.is_none_or(|named| named == metric))
            .collect();
        accepted[root.0 as usize].insert(query, metrics);
    }
    let mut exact_only = vec![BTreeSet::new(); memo.queries().len()];
    for (&query, groups) in read_only_exactly {
        exact_only[query].extend(groups.iter().copied());
    }
    for &group in order.iter().rev() {
        let accepted_here = accepted[group.0 as usize].clone();
        for (&query, metrics) in &accepted_here {
            if metrics.is_empty() {
                exact_only[query].insert(group);
            }
        }
        for candidate in &memo.groups()[group.0 as usize].candidates {
            let operator = &candidate.expression.operator;
            let carried: Vec<AccuracyMetric> = AccuracyMetric::ALL
                .into_iter()
                .filter(|&metric| error_propagation(memo, operator, metric).is_some())
                .collect();
            for &child in operator.children() {
                for (&query, metrics) in &accepted_here {
                    let passed: Vec<AccuracyMetric> = metrics
                        .iter()
                        .copied()
                        .filter(|metric| carried.contains(metric))
                        .collect();
                    match accepted[child.0 as usize].entry(query) {
                        Entry::Vacant(entry) => {
                            entry.insert(passed);
                        }
                        Entry::Occupied(mut entry) => {
                            entry.get_mut().retain(|metric| passed.contains(metric))
                        }
                    }
                }
            }
        }
    }
    // Every group below an exact-only group is read exactly too.
    for &group in order.iter().rev() {
        for groups in &mut exact_only {
            if groups.contains(&group) {
                for candidate in &memo.groups()[group.0 as usize].candidates {
                    groups.extend(
                        candidate
                            .expression
                            .operator
                            .children()
                            .into_iter()
                            .copied(),
                    );
                }
            }
        }
    }
    exact_only
}
