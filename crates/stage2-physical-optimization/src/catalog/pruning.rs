//! Physical alternatives no plan can use, removed before Stage 3: one with
//! an input no buildable producer can supply, or one no query output reads.
//! Both follow from the admissible edges alone; no cost is involved.

use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties};
use asap_types::stages::{
    producer_satisfies_physical_requirements, root_groups, Memo, PhysicalCandidate, PruneReason,
};

/// Why each of `alternatives` cannot be used, `None` for a usable one.
/// `admissible_producers_per_input[c][i]` lists the alternatives (by
/// index) on admissible edges into input `i` of alternative `c`.
///
/// - [`PruneReason::InputHasNoBuildableProducer`]: not buildable, where
///   buildable is the least fixed point of "every input has a buildable
///   admissible producer". A candidate without inputs is buildable; a cycle
///   without a buildable way out is not.
/// - [`PruneReason::ReachesNoQueryOutput`]: buildable, but not reached from
///   a query output over admissible edges into buildable producers. A query
///   reads its output at query time, so a query output is an alternative of
///   a query's root group that a query-time consumer may read.
pub(super) fn prune_reasons(
    alternatives: &[PhysicalCandidate],
    admissible_producers_per_input: &[Vec<Vec<usize>>],
    memo: &Memo,
) -> Vec<Option<PruneReason>> {
    let mut buildable = vec![false; alternatives.len()];
    loop {
        let mut changed = false;
        for (candidate, inputs) in admissible_producers_per_input.iter().enumerate() {
            if !buildable[candidate]
                && inputs
                    .iter()
                    .all(|producers| producers.iter().any(|&producer| buildable[producer]))
            {
                buildable[candidate] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let root_groups = root_groups(&memo.query_roots);
    let mut reached = vec![false; alternatives.len()];
    let mut pending: Vec<usize> = alternatives
        .iter()
        .enumerate()
        .filter(|(index, alternative)| {
            buildable[*index]
                && root_groups.contains(&alternative.logical_candidate_id.group)
                && read_by_query_output(alternative)
        })
        .map(|(index, _)| index)
        .collect();
    while let Some(candidate) = pending.pop() {
        if !reached[candidate] {
            reached[candidate] = true;
            pending.extend(
                admissible_producers_per_input[candidate]
                    .iter()
                    .flatten()
                    .copied()
                    .filter(|&producer| buildable[producer]),
            );
        }
    }
    buildable
        .into_iter()
        .zip(reached)
        .map(|(buildable, reached)| {
            if !buildable {
                Some(PruneReason::InputHasNoBuildableProducer)
            } else if !reached {
                Some(PruneReason::ReachesNoQueryOutput)
            } else {
                None
            }
        })
        .collect()
}

/// Whether a query may read `alternative` as its output: as a query-time
/// consumer that is not materialized would ($\alpha^{\mathrm{phys}}$).
fn read_by_query_output(alternative: &PhysicalCandidate) -> bool {
    let query = PhysicalCandidate {
        logical_candidate_id: alternative.logical_candidate_id,
        physical_properties: PhysicalProperties::new(ExecutionTiming::QueryTime, Lifetime::None)
            .expect("query time, not materialized, is valid"),
    };
    producer_satisfies_physical_requirements(alternative, &query)
}
