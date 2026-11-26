//! Search-space pruning rule 3 (`SearchSpacePruning::mandatory_group_cover`):
//! a cover row per memo group every plan executes, $\sum_{e \in G} x_e \ge 1$
//! over its kept executions. Every integer solution meets it, so the optimum
//! is unchanged; the LP relaxation may not (it can split a group's flow
//! over fractions of executions whose sum is below 1), so the rows raise
//! its bound towards the optimum.
//!
//! A group is mandatory when it is a query's output group (every execution
//! that may answer the query is in it), or when every kept execution of a
//! mandatory group reads one input only from producers in it: (A) gives
//! that input a producer whenever the consumer runs.
//!
//! Example: `quantile_over_time(0.9, m[10m])` and
//! `quantile_over_time(0.5, m[5m])` at rank 0.01, raw data not retained:
//! each readout group and the merges and chains below them every plan
//! needs are mandatory (9 groups), whose least execution costs add up to
//! 1.70/s against an LP bound of 1.14/s without the rows (optimum 2.72/s).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use good_lp::{constraint, Constraint, Expression, Variable};

use asap_types::stages::MemoGroupId;

use super::model::SelectionInputs;

/// The cover rows of the mandatory groups (module doc) over the kept
/// executions' $x$ (`x`, by execution; a copy of an equivalent context
/// shares its original's variable, counted once).
pub(crate) fn mandatory_group_rows(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
    x: &[Variable],
) -> Vec<Constraint> {
    let selectable = inputs.selectable;
    let executions = selectable.executions();
    let group_of = |index: usize| {
        inputs.physical.physical_candidates[executions[index].candidate.0]
            .logical_candidate_id
            .group
    };
    let mut kept_of_group: BTreeMap<MemoGroupId, Vec<usize>> = BTreeMap::new();
    for index in (0..executions.len()).filter(|&index| kept[index]) {
        kept_of_group
            .entry(group_of(index))
            .or_default()
            .push(index);
    }
    let mut mandatory: BTreeSet<MemoGroupId> = BTreeSet::new();
    for (_, answering) in selectable.answering() {
        let groups: BTreeSet<MemoGroupId> =
            answering.iter().map(|&index| group_of(index)).collect();
        if let [group] = groups.into_iter().collect::<Vec<_>>()[..] {
            mandatory.insert(group);
        }
    }
    let mut pending: Vec<MemoGroupId> = mandatory.iter().copied().collect();
    while let Some(group) = pending.pop() {
        let Some(members) = kept_of_group.get(&group) else {
            continue;
        };
        // The groups every kept execution of `group` reads one input from
        // alone.
        let mut read_by_all: Option<BTreeSet<MemoGroupId>> = None;
        for &consumer in members {
            let read: BTreeSet<MemoGroupId> = (0..selectable.input_count(consumer))
                .filter_map(|input| {
                    let producers: BTreeSet<MemoGroupId> = selectable
                        .edges_into(consumer, input)
                        .iter()
                        .map(|&position| selectable.edges()[position].producer)
                        .filter(|&producer| kept[producer])
                        .map(group_of)
                        .collect();
                    let mut producers = producers.into_iter();
                    match (producers.next(), producers.next()) {
                        (Some(only), None) => Some(only),
                        _ => None,
                    }
                })
                .collect();
            read_by_all = Some(match read_by_all {
                None => read,
                Some(all) => all.intersection(&read).copied().collect(),
            });
        }
        for read in read_by_all.unwrap_or_default() {
            if mandatory.insert(read) {
                pending.push(read);
            }
        }
    }
    mandatory
        .iter()
        .filter_map(|group| kept_of_group.get(group))
        .map(|members| {
            let mut seen = HashSet::new();
            let sum: Expression = members
                .iter()
                .map(|&index| x[index])
                .filter(|variable| seen.insert(*variable))
                .sum();
            constraint!(sum >= 1)
        })
        .collect()
}
