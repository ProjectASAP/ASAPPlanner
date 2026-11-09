//! Step 4, dead-candidate elimination (concepts doc, section 3): a
//! candidate that is not a query output is dead when it never had a
//! consumer, or when every edge to a consumer fails or reaches a dead
//! consumer; elimination repeats until nothing changes. Each dead candidate
//! is listed with every root cause, one shortest path per failing edge.

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::logical_memo_candidates::CandidateEdge;
use asap_types::stages::{
    root_groups, CandidateForMemoGroupId, Memo, MemoGroupId, PruneCause, PrunedLogicalCandidate,
    RootPruneReason,
};

/// The dead candidates of `memo`, whose possible edges are `edges`, each
/// with its causes, in candidate order.
pub(crate) fn eliminate_dead_candidates(
    memo: &Memo,
    edges: &[CandidateEdge],
) -> Vec<PrunedLogicalCandidate> {
    let mut consumers: HashMap<CandidateForMemoGroupId, Vec<&CandidateEdge>> = HashMap::new();
    for edge in edges {
        consumers.entry(edge.producer).or_default().push(edge);
    }
    let outputs = root_groups(&memo.queries);
    let mut dead: BTreeSet<CandidateForMemoGroupId> = BTreeSet::new();
    let all: Vec<CandidateForMemoGroupId> = memo
        .groups()
        .iter()
        .enumerate()
        .flat_map(|(group, memo_group)| {
            (0..memo_group.candidates.len()).map(move |index_in_group| CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            })
        })
        .filter(|candidate| !outputs.contains(&candidate.group))
        .collect();
    loop {
        let newly_dead: Vec<CandidateForMemoGroupId> = all
            .iter()
            .copied()
            .filter(|candidate| !dead.contains(candidate))
            .filter(|candidate| {
                !consumers.get(candidate).is_some_and(|edges| {
                    edges
                        .iter()
                        .any(|edge| edge.satisfies.is_ok() && !dead.contains(&edge.consumer))
                })
            })
            .collect();
        if newly_dead.is_empty() {
            break;
        }
        dead.extend(newly_dead);
    }
    dead.iter()
        .map(|&candidate| PrunedLogicalCandidate {
            candidate,
            causes: causes(candidate, &consumers),
        })
        .collect()
}

/// Every root cause of dead `candidate`: breadth first from it through
/// dead consumers, so each failing edge is reached by a shortest path; a
/// dead consumer that never had a consumer ends its path with
/// `NoConsumer`.
fn causes(
    candidate: CandidateForMemoGroupId,
    consumers: &HashMap<CandidateForMemoGroupId, Vec<&CandidateEdge>>,
) -> Vec<PruneCause> {
    let mut causes = Vec::new();
    let mut visited = BTreeSet::from([candidate]);
    let mut queue = VecDeque::from([(candidate, Vec::new())]);
    while let Some((at, path)) = queue.pop_front() {
        let Some(edges) = consumers.get(&at) else {
            causes.push(PruneCause {
                through_pruned_consumers: path,
                root: RootPruneReason::NoConsumer,
            });
            continue;
        };
        for edge in edges {
            match &edge.satisfies {
                Err(reason) => causes.push(PruneCause {
                    through_pruned_consumers: path.clone(),
                    root: reason.clone(),
                }),
                // A satisfied edge of a dead candidate leads to a dead consumer.
                Ok(()) => {
                    if visited.insert(edge.consumer) {
                        let mut further = path.clone();
                        further.push(edge.consumer);
                        queue.push_back((edge.consumer, further));
                    }
                }
            }
        }
    }
    causes
}
