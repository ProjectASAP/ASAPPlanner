//! Step 4, dead-candidate elimination (concepts doc, section 3): a
//! candidate that is not a query output is dead when it never had a
//! consumer, or when every edge to a consumer fails or reaches a dead
//! consumer; any candidate is dead when every edge on one of its inputs
//! fails or reads a dead producer. Elimination repeats until nothing
//! changes. Each dead candidate is listed with every root cause, one
//! shortest path per failing edge.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

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
    let graph = EdgeGraph::new(edges);
    let outputs = root_groups(&memo.queries);
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
        .collect();
    // Per dead candidate, why, judged against those dead before it: cut
    // off from an input (`Some(input)`), or without a live consumer.
    let mut dead: BTreeMap<CandidateForMemoGroupId, Option<usize>> = BTreeMap::new();
    loop {
        let newly_dead: Vec<(CandidateForMemoGroupId, Option<usize>)> = all
            .iter()
            .copied()
            .filter(|candidate| !dead.contains_key(candidate))
            .filter_map(|candidate| {
                if let Some(input) = graph.cut_off_input(memo, candidate, &dead) {
                    Some((candidate, Some(input)))
                } else if !outputs.contains(&candidate.group)
                    && !graph.has_live_consumer(candidate, &dead)
                {
                    Some((candidate, None))
                } else {
                    None
                }
            })
            .collect();
        if newly_dead.is_empty() {
            break;
        }
        dead.extend(newly_dead);
    }
    dead.keys()
        .map(|&candidate| PrunedLogicalCandidate {
            candidate,
            causes: graph.causes(candidate, &dead),
        })
        .collect()
}

/// `edges` by producer and by (consumer, input).
struct EdgeGraph<'a> {
    by_producer: HashMap<CandidateForMemoGroupId, Vec<&'a CandidateEdge>>,
    by_consumer_input: HashMap<(CandidateForMemoGroupId, usize), Vec<&'a CandidateEdge>>,
}

impl<'a> EdgeGraph<'a> {
    fn new(edges: &'a [CandidateEdge]) -> Self {
        let mut graph = Self {
            by_producer: HashMap::new(),
            by_consumer_input: HashMap::new(),
        };
        for edge in edges {
            graph
                .by_producer
                .entry(edge.producer)
                .or_default()
                .push(edge);
            graph
                .by_consumer_input
                .entry((edge.consumer, edge.input))
                .or_default()
                .push(edge);
        }
        graph
    }

    /// Whether `candidate` has a satisfied edge into a consumer not in `dead`.
    fn has_live_consumer(
        &self,
        candidate: CandidateForMemoGroupId,
        dead: &BTreeMap<CandidateForMemoGroupId, Option<usize>>,
    ) -> bool {
        self.by_producer.get(&candidate).is_some_and(|edges| {
            edges
                .iter()
                .any(|edge| edge.satisfies.is_ok() && !dead.contains_key(&edge.consumer))
        })
    }

    /// The first input of `candidate` with no satisfied edge from a producer
    /// not in `dead`: every edge on it is cut (the metric rule, a missed
    /// requirement) or reads an eliminated producer.
    fn cut_off_input(
        &self,
        memo: &Memo,
        candidate: CandidateForMemoGroupId,
        dead: &BTreeMap<CandidateForMemoGroupId, Option<usize>>,
    ) -> Option<usize> {
        (0..memo
            .candidate(candidate)
            .expression
            .operator
            .children()
            .len())
            .find(|&input| {
                !self
                    .by_consumer_input
                    .get(&(candidate, input))
                    .is_some_and(|edges| {
                        edges.iter().any(|edge| {
                            edge.satisfies.is_ok() && !dead.contains_key(&edge.producer)
                        })
                    })
            })
    }

    /// Every root cause of dead `candidate`, breadth first, so each failing
    /// edge is reached by a shortest path: a candidate cut off from an
    /// input (`dead`) is traced along that input's edges, through
    /// eliminated producers; any other through its consumer edges and
    /// eliminated consumers, ending in `NoConsumer` at one that never had a
    /// consumer.
    fn causes(
        &self,
        candidate: CandidateForMemoGroupId,
        dead: &BTreeMap<CandidateForMemoGroupId, Option<usize>>,
    ) -> Vec<PruneCause> {
        let mut causes = Vec::new();
        let mut visited = BTreeSet::from([candidate]);
        let mut queue = VecDeque::from([(candidate, Vec::new())]);
        while let Some((at, path)) = queue.pop_front() {
            let (edges, next): (
                &[&CandidateEdge],
                fn(&CandidateEdge) -> CandidateForMemoGroupId,
            ) = match dead[&at] {
                Some(input) => (
                    self.by_consumer_input
                        .get(&(at, input))
                        .map_or(&[], Vec::as_slice),
                    |edge| edge.producer,
                ),
                None => match self.by_producer.get(&at) {
                    Some(edges) => (edges.as_slice(), |edge| edge.consumer),
                    None => {
                        causes.push(PruneCause {
                            through_pruned_candidates: path,
                            root: RootPruneReason::NoConsumer,
                        });
                        continue;
                    }
                },
            };
            for edge in edges {
                match &edge.satisfies {
                    Err(reason) => causes.push(PruneCause {
                        through_pruned_candidates: path.clone(),
                        root: reason.clone(),
                    }),
                    // A satisfied edge of a dead candidate leads to a dead
                    // consumer or producer.
                    Ok(()) => {
                        if visited.insert(next(edge)) {
                            let mut further = path.clone();
                            further.push(next(edge));
                            queue.push_back((next(edge), further));
                        }
                    }
                }
            }
        }
        causes
    }
}
