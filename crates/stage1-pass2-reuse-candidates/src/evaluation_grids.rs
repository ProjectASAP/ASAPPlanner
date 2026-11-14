//! Steps 4 to 6: the evaluation grid of each group, per query reaching
//! it: the times a group's result is evaluated at, which a chain's width
//! must divide.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use asap_types::ir::{NonASAPOp, Operator};
use asap_types::stages::{EvaluationGrid, Memo, MemoGroupId};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

/// Per group, by group id: per query reaching it, the period in ms of the
/// grid the query evaluates the group's result on (phase 0), `None` when
/// that period is unknown. Walking down from each query's root, which is on
/// the query's own grid ([`query_evaluation_period_ms`]), a group is on its
/// consumer's grid, except below a PromQL subquery: `<expr>[10m:30s]`
/// evaluates `<expr>` at the multiples of 30 s whatever the outer grid, and
/// `<expr>[10m:]` at the server's default interval, unknown to the planner.
/// A group a query reaches through several paths is on the gcd of their
/// grids (unknown when one is). Every candidate's inputs are walked, so a
/// rollup's finer group is on the grids of the queries rolled up.
pub(crate) fn evaluation_period_ms_per_query(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
) -> Vec<BTreeMap<usize, Option<u64>>> {
    let mut periods_ms: Vec<BTreeMap<usize, Option<u64>>> =
        vec![BTreeMap::new(); memo.groups().len()];
    let mut pending = Vec::new();
    for (query, memo_query) in memo.queries().iter().enumerate() {
        let period_ms = query_evaluation_period_ms(entries, query);
        for &root in memo_query.root.operator_refs() {
            if merge(&mut periods_ms[root.0 as usize], query, period_ms) {
                pending.push(root);
            }
        }
    }
    while let Some(group) = pending.pop() {
        let periods_here_ms = periods_ms[group.0 as usize].clone();
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
                for (&query, &period_ms) in &periods_here_ms {
                    let below_ms = subquery_resolution_ms.unwrap_or(period_ms);
                    if merge(&mut periods_ms[child.0 as usize], query, below_ms) {
                        pending.push(child);
                    }
                }
            }
        }
    }
    periods_ms
}

/// Per group: the grid its result is evaluated on, the gcd of the
/// evaluation periods of the queries reaching it
/// (`evaluation_period_ms_per_query`) at phase 0; `None` when no query
/// reaches it or one evaluates it on no known grid.
pub(crate) fn evaluation_grids(
    evaluation_period_ms_per_query: &[BTreeMap<usize, Option<u64>>],
) -> Vec<Option<EvaluationGrid>> {
    evaluation_period_ms_per_query
        .iter()
        .map(|periods_ms| {
            let mut period_ms = 0;
            for &query_period_ms in periods_ms.values() {
                period_ms = greatest_common_divisor(period_ms, query_period_ms?);
            }
            (period_ms > 0).then_some(EvaluationGrid {
                period_ms,
                phase_ms: 0,
            })
        })
        .collect()
}

/// When `input_group` is a chain `SummaryTumblingWindowChain[w]` read by
/// `group`, whose result depends on the evaluation time, and $w$ does not
/// divide `group`'s evaluation period (`evaluation_grid_of_group`, from
/// [`evaluation_grids`]) or that period is unknown: the chain's width and
/// the period in ms. Its windows do not end at `group`'s evaluation times.
pub(crate) fn chain_off_the_evaluation_grid(
    memo: &Memo,
    evaluation_grid_of_group: &[Option<EvaluationGrid>],
    group: MemoGroupId,
    input_group: MemoGroupId,
) -> Option<(u64, Option<u64>)> {
    let tumbling_window_width_ms = memo.groups()[input_group.0 as usize]
        .expression
        .chain_tumbling_window_width_ms()?;
    if !memo.groups()[group.0 as usize]
        .group_properties
        .depends_on_evaluation_time
    {
        return None;
    }
    let evaluation_period_ms =
        evaluation_grid_of_group[group.0 as usize].map(|grid| grid.period_ms);
    (!evaluation_period_ms
        .is_some_and(|period_ms| period_ms.is_multiple_of(tumbling_window_width_ms)))
    .then_some((tumbling_window_width_ms, evaluation_period_ms))
}

/// The evaluation period in ms of query `query` (`entries[query]`'s
/// `QueryRecurrence::evaluation_grid_ms`); `None` when it has no known
/// grid.
fn query_evaluation_period_ms(entries: &[QueryWorkloadEntry], query: usize) -> Option<u64> {
    let entry = entries.get(query)?;
    entry.recurrence.evaluation_grid_ms(entry.range_evaluation)
}

/// Adds `query`'s evaluation period `period_ms` to a group's periods: the
/// gcd with a known one, unknown when either is; whether they changed.
fn merge(
    periods_ms: &mut BTreeMap<usize, Option<u64>>,
    query: usize,
    period_ms: Option<u64>,
) -> bool {
    match periods_ms.entry(query) {
        Entry::Vacant(entry) => {
            entry.insert(period_ms);
            true
        }
        Entry::Occupied(mut entry) => {
            let merged = match (*entry.get(), period_ms) {
                (Some(known_ms), Some(period_ms)) => {
                    Some(greatest_common_divisor(known_ms, period_ms))
                }
                _ => None,
            };
            let changed = merged != *entry.get();
            entry.insert(merged);
            changed
        }
    }
}
