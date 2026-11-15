//! Every plan over Stage 2's physical candidates
//! ([`PhysicalASAPMemoCandidates`]), by plain enumeration, for an
//! exhaustive oracle that checks plan selection against every
//! alternative. It is written from Stage 2's output alone and shares no
//! code with Stage 3.
//!
//! A plan picks one physical candidate for each memo group it reaches,
//! starting at the query roots' groups, and one admissible edge into each
//! input of each picked candidate, whose producer is the candidate picked
//! for the producer's group. Example: `sum_over_time(m[5m])` with an
//! exact query-time candidate and an ingestion-time chain candidate in the
//! root group, each reading the scan's single candidate, has two plans.
//!
//! One candidate per group: a plan that reads one group through two of
//! its candidates (say exact for one query and a sketch for another) is
//! not enumerated.

use std::collections::{BTreeMap, BTreeSet};

use crate::stages::{MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId};

/// One plan: the candidate picked per memo group, and the edge feeding
/// each input of each picked candidate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct PhysicalPlanChoice {
    pub candidate_of_group: BTreeMap<MemoGroupId, PhysicalCandidateId>,
    /// Keyed by (consumer, input index of the consumer).
    pub edge_into_input: BTreeMap<(PhysicalCandidateId, usize), PhysicalEdgeId>,
}

impl PhysicalPlanChoice {
    /// The picked candidates.
    pub fn candidates(&self) -> BTreeSet<PhysicalCandidateId> {
        self.candidate_of_group.values().copied().collect()
    }

    /// The picked edges.
    pub fn edges(&self) -> BTreeSet<PhysicalEdgeId> {
        self.edge_into_input.values().copied().collect()
    }
}

/// The enumeration stopped: more than `limit` plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyPlans {
    pub limit: usize,
}

/// What is left to pick in a partial plan.
#[derive(Debug, Clone, Copy)]
enum Pick {
    /// A candidate for this group (a query root's).
    Group(MemoGroupId),
    /// An edge into input `.1` of candidate `.0`.
    Input(PhysicalCandidateId, usize),
}

/// Every plan over `physical`, at most `limit` of them.
pub fn every_physical_plan(
    physical: &PhysicalASAPMemoCandidates,
    limit: usize,
) -> Result<Vec<PhysicalPlanChoice>, TooManyPlans> {
    let mut root_groups = Vec::new();
    for query in physical.logical_candidates.memo.queries() {
        for &group in query.root.operator_refs() {
            if !root_groups.contains(&group) {
                root_groups.push(group);
            }
        }
    }
    let mut candidates_in_group: BTreeMap<MemoGroupId, Vec<PhysicalCandidateId>> = BTreeMap::new();
    for (index, candidate) in physical.physical_candidates.iter().enumerate() {
        candidates_in_group
            .entry(candidate.logical_candidate_id.group)
            .or_default()
            .push(PhysicalCandidateId(index));
    }
    let mut plans = Vec::new();
    let pending: Vec<Pick> = root_groups.into_iter().rev().map(Pick::Group).collect();
    extend(
        physical,
        &candidates_in_group,
        &mut PhysicalPlanChoice::default(),
        pending,
        &mut plans,
        limit,
    )?;
    Ok(plans)
}

/// The group of physical candidate `candidate`.
fn group_of(physical: &PhysicalASAPMemoCandidates, candidate: PhysicalCandidateId) -> MemoGroupId {
    physical.physical_candidates[candidate.0]
        .logical_candidate_id
        .group
}

/// `pending` with a pick for every input of `candidate` on top.
fn with_inputs_of(
    physical: &PhysicalASAPMemoCandidates,
    candidate: PhysicalCandidateId,
    pending: &[Pick],
) -> Vec<Pick> {
    let mut next = pending.to_vec();
    next.extend(
        (0..physical.input_count(candidate))
            .rev()
            .map(|input| Pick::Input(candidate, input)),
    );
    next
}

/// Complete `choice` by every way of making the `pending` picks (the
/// last one first), adding each complete plan to `plans`.
fn extend(
    physical: &PhysicalASAPMemoCandidates,
    candidates_in_group: &BTreeMap<MemoGroupId, Vec<PhysicalCandidateId>>,
    choice: &mut PhysicalPlanChoice,
    mut pending: Vec<Pick>,
    plans: &mut Vec<PhysicalPlanChoice>,
    limit: usize,
) -> Result<(), TooManyPlans> {
    let Some(pick) = pending.pop() else {
        if plans.len() == limit {
            return Err(TooManyPlans { limit });
        }
        plans.push(choice.clone());
        return Ok(());
    };
    match pick {
        Pick::Group(group) => {
            if choice.candidate_of_group.contains_key(&group) {
                return extend(physical, candidates_in_group, choice, pending, plans, limit);
            }
            for &candidate in candidates_in_group.get(&group).into_iter().flatten() {
                choice.candidate_of_group.insert(group, candidate);
                let next = with_inputs_of(physical, candidate, &pending);
                extend(physical, candidates_in_group, choice, next, plans, limit)?;
                choice.candidate_of_group.remove(&group);
            }
            Ok(())
        }
        Pick::Input(consumer, input) => {
            for &edge_id in physical.admissible_edges_into(consumer, input) {
                let producer = physical.edge(edge_id).producer_candidate_id;
                let group = group_of(physical, producer);
                let newly_picked = match choice.candidate_of_group.get(&group) {
                    Some(&picked) if picked != producer => continue,
                    Some(_) => false,
                    None => true,
                };
                choice.edge_into_input.insert((consumer, input), edge_id);
                let next = if newly_picked {
                    choice.candidate_of_group.insert(group, producer);
                    with_inputs_of(physical, producer, &pending)
                } else {
                    pending.clone()
                };
                let result = extend(physical, candidates_in_group, choice, next, plans, limit);
                if newly_picked {
                    choice.candidate_of_group.remove(&group);
                }
                choice.edge_into_input.remove(&(consumer, input));
                result?;
            }
            Ok(())
        }
    }
}
