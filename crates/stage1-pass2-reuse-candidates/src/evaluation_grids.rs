//! The evaluation grid of each group, per query reaching it: the times a
//! group's result is evaluated at, which a chain's width must divide.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use asap_types::ir::{NonASAPOp, Operator};
use asap_types::stages::{EvaluationGrid, Memo};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

/// Per group, by group id: per query reaching it, the period in ms of the
/// grid the query evaluates the group's result on (phase 0), `None` when
/// that grid is unknown. Walking down from each query's root, which is on
/// the query's own grid ([`query_evaluation_grid_ms`]), a group is on its
/// consumer's grid, except below a PromQL subquery: `<expr>[10m:30s]`
/// evaluates `<expr>` at the multiples of 30 s whatever the outer grid, and
/// `<expr>[10m:]` at the server's default interval, unknown to the planner.
/// A group a query reaches through several paths is on the gcd of their
/// grids (unknown when one is). Every candidate's inputs are walked, so a
/// rollup's finer group is on the grids of the queries rolled up.
pub(crate) fn evaluation_grid_per_query(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
) -> Vec<BTreeMap<usize, Option<u64>>> {
    let mut grids: Vec<BTreeMap<usize, Option<u64>>> = vec![BTreeMap::new(); memo.groups().len()];
    let mut pending = Vec::new();
    for (query, memo_query) in memo.queries().iter().enumerate() {
        let grid = query_evaluation_grid_ms(entries, query);
        for &root in memo_query.root.operator_refs() {
            if merge(&mut grids[root.0 as usize], query, grid) {
                pending.push(root);
            }
        }
    }
    while let Some(group) = pending.pop() {
        let grids_here = grids[group.0 as usize].clone();
        for candidate in &memo.groups()[group.0 as usize].candidates {
            let operator = &candidate.expression.operator;
            let subquery_resolution_ms = match operator {
                Operator::NonASAP(NonASAPOp::PromqlSubquery { resolution, .. }) => Some(
                    resolution
                        .map(|resolution| resolution.as_millis() as u64)
                        .filter(|&ms| ms > 0),
                ),
                _ => None,
            };
            for &child in operator.children() {
                for (&query, &grid) in &grids_here {
                    let below = subquery_resolution_ms.unwrap_or(grid);
                    if merge(&mut grids[child.0 as usize], query, below) {
                        pending.push(child);
                    }
                }
            }
        }
    }
    grids
}

/// Per group: the grid its result is evaluated on, the gcd of the
/// evaluation grids of the queries reaching it
/// ([`evaluation_grid_per_query`]) at phase 0; `None` when no query
/// reaches it or one evaluates it on no known grid.
pub(crate) fn evaluation_grids(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
) -> Vec<Option<EvaluationGrid>> {
    evaluation_grid_per_query(memo, entries)
        .into_iter()
        .map(|grids| {
            let mut period_ms = 0;
            for grid in grids.into_values() {
                period_ms = greatest_common_divisor(period_ms, grid?);
            }
            (period_ms > 0).then_some(EvaluationGrid {
                period_ms,
                phase_ms: 0,
            })
        })
        .collect()
}

/// The evaluation grid of query `query` (`entries[query]`'s
/// `QueryRecurrence::evaluation_grid_ms`); `None` when it has no known
/// grid.
fn query_evaluation_grid_ms(entries: &[QueryWorkloadEntry], query: usize) -> Option<u64> {
    let entry = entries.get(query)?;
    entry.recurrence.evaluation_grid_ms(entry.range_evaluation)
}

/// Adds `query`'s `grid` to a group's grids: the gcd with a known one,
/// unknown when either is; whether they changed.
fn merge(grids: &mut BTreeMap<usize, Option<u64>>, query: usize, grid: Option<u64>) -> bool {
    match grids.entry(query) {
        Entry::Vacant(entry) => {
            entry.insert(grid);
            true
        }
        Entry::Occupied(mut entry) => {
            let merged = match (*entry.get(), grid) {
                (Some(known), Some(grid)) => Some(greatest_common_divisor(known, grid)),
                _ => None,
            };
            let changed = merged != *entry.get();
            entry.insert(merged);
            changed
        }
    }
}
