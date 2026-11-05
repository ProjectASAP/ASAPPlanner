//! The candidate DAGs a memo stands for: each choice of the candidate
//! computing each query's root group and of one producer per input of each
//! chosen candidate, and the complete DAG that choice composes.
//!
//! A candidate is needed when it computes a query's root group or a chosen
//! candidate reads it. Two consumers of one group may read different
//! candidates of it (`y_{u,c,i}`), and a candidate several consumers read is
//! one node, built once and shared; which candidates are needed depends on
//! the producers chosen above them. The count is therefore not a product
//! over groups.
//!
//! A producer must satisfy its consumer input's logical requirement
//! ([`satisfies`]). Composing checks only that a choice composes; accuracy
//! at the roots, Stage 2 legality and Stage 3 cost stay with the validator
//! and the later stages. Candidates may read each other through their
//! groups (a rollup of a finer grouping whose candidate reads the coarser
//! one): a choice closing such a cycle composes no DAG and is not
//! enumerated.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_accuracy::propagation::recompose_guarantees;
use asap_types::ir::{OperatorNode, QueryRoot, SchemaDerivationError};
use asap_types::stages::{satisfies, CandidateForMemoGroupId, Memo, MemoGroupId};

/// One memo choice: the candidate computing each query's root group
/// (`z_{q,v}`), and the producer each input of each chosen candidate reads
/// (`y_{u,c,i}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChosenCandidatePerMemoGroup {
    /// Each root group's candidate, by index in its group.
    pub root_candidates: BTreeMap<MemoGroupId, usize>,
    /// Each chosen candidate's input (in operator-child order) to the
    /// candidate it reads.
    pub producer_of_input: BTreeMap<(CandidateForMemoGroupId, usize), CandidateForMemoGroupId>,
}

/// The choices [`enumerate`] found, in depth-first order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllChosenCandidatesPerMemoGroup {
    /// Each choice names exactly the candidates and inputs it needs.
    pub all_chosen_candidates: Vec<ChosenCandidatePerMemoGroup>,
    /// False when the limit stopped the enumeration before every choice was found.
    pub complete: bool,
}

/// A choice that does not compose.
#[derive(Debug, thiserror::Error)]
pub enum ErrorBuildingLogicalASAPDagsFromChosenCandidates {
    #[error("group {group:?} has no candidate {candidate}")]
    NoSuchCandidate {
        group: MemoGroupId,
        candidate: usize,
    },
    #[error("input {input} of candidate {candidate:?} has no chosen producer")]
    NoProducer {
        candidate: CandidateForMemoGroupId,
        input: usize,
    },
    /// The chosen candidates read each other in a cycle.
    #[error("candidate {candidate:?} reads itself through the chosen producers")]
    Cycle { candidate: CandidateForMemoGroupId },
    /// Re-deriving the composed nodes' schemas and guarantees failed.
    #[error("recomposing the composed DAG: {0}")]
    Recompose(#[from] SchemaDerivationError),
}

/// The input groups of `candidate`, in operator-child order.
fn inputs_of(memo: &Memo, candidate: CandidateForMemoGroupId) -> Vec<MemoGroupId> {
    let mut inputs = Vec::new();
    memo.candidate(candidate)
        .expression
        .operator
        .map_children(|&group| inputs.push(group));
    inputs
}

/// Every choice of a candidate per root group and a producer per needed
/// input, starting from the query roots, stopping after `limit` choices. A
/// producer must satisfy the input's requirement, and a choice whose
/// candidates read each other in a cycle is left out.
pub fn enumerate(memo: &Memo, limit: usize) -> AllChosenCandidatesPerMemoGroup {
    struct Walk<'a> {
        memo: &'a Memo,
        limit: usize,
        found: Vec<ChosenCandidatePerMemoGroup>,
    }
    impl Walk<'_> {
        /// Chooses the next undecided root or input; false when the limit
        /// stopped the walk.
        fn walk(
            &mut self,
            roots: &[MemoGroupId],
            mut pending: Vec<(CandidateForMemoGroupId, usize)>,
            expanded: &BTreeSet<CandidateForMemoGroupId>,
            chosen: &mut ChosenCandidatePerMemoGroup,
        ) -> bool {
            if let Some((&root, rest)) = roots.split_first() {
                for index in 0..self.memo.candidates[root.0 as usize].len() {
                    let candidate = CandidateForMemoGroupId { group: root, index };
                    chosen.root_candidates.insert(root, index);
                    let (pending, expanded) = self.expand(candidate, &pending, expanded);
                    let finished = self.walk(rest, pending, &expanded, chosen);
                    chosen.root_candidates.remove(&root);
                    if !finished {
                        return false;
                    }
                }
                return true;
            }
            let slot = loop {
                match pending.pop() {
                    None => {
                        if self.found.len() == self.limit {
                            return false;
                        }
                        self.found.push(chosen.clone());
                        return true;
                    }
                    Some(slot) if !chosen.producer_of_input.contains_key(&slot) => break slot,
                    Some(_) => {}
                }
            };
            let (consumer, input) = slot;
            let group = inputs_of(self.memo, consumer)[input];
            let requirement = &self.memo.candidate(consumer).input_requirements[input];
            for (index, producer) in self.memo.candidates[group.0 as usize].iter().enumerate() {
                let producer_id = CandidateForMemoGroupId { group, index };
                if !satisfies(&producer.logical_properties, requirement)
                    || reaches(self.memo, chosen, producer_id, consumer)
                {
                    continue;
                }
                chosen.producer_of_input.insert(slot, producer_id);
                let (next, next_expanded) = self.expand(producer_id, &pending, expanded);
                let finished = self.walk(&[], next, &next_expanded, chosen);
                chosen.producer_of_input.remove(&slot);
                if !finished {
                    return false;
                }
            }
            true
        }

        /// `pending` with `candidate`'s inputs, when it is not expanded yet.
        fn expand(
            &self,
            candidate: CandidateForMemoGroupId,
            pending: &[(CandidateForMemoGroupId, usize)],
            expanded: &BTreeSet<CandidateForMemoGroupId>,
        ) -> (
            Vec<(CandidateForMemoGroupId, usize)>,
            BTreeSet<CandidateForMemoGroupId>,
        ) {
            let mut pending = pending.to_vec();
            let mut expanded = expanded.clone();
            if expanded.insert(candidate) {
                let inputs = inputs_of(self.memo, candidate).len();
                pending.extend((0..inputs).rev().map(|input| (candidate, input)));
            }
            (pending, expanded)
        }
    }
    let roots: Vec<MemoGroupId> = root_groups(memo).collect();
    let mut walk = Walk {
        memo,
        limit,
        found: Vec::new(),
    };
    let complete = walk.walk(
        &roots,
        Vec::new(),
        &BTreeSet::new(),
        &mut ChosenCandidatePerMemoGroup::default(),
    );
    AllChosenCandidatesPerMemoGroup {
        all_chosen_candidates: walk.found,
        complete,
    }
}

/// Each root group (one with a query's root) and its DAG under
/// `chosen_candidates`: each needed candidate's operator over its chosen
/// producers' composed DAGs, a candidate read several times composed once
/// and shared. Every node is then rebuilt with its derived schema and
/// composed guarantee, as in plan reconstruction.
pub fn build_logical_asap_dags_from_chosen_candidates(
    memo: &Memo,
    chosen_candidates: &ChosenCandidatePerMemoGroup,
) -> Result<Vec<(MemoGroupId, Rc<OperatorNode>)>, ErrorBuildingLogicalASAPDagsFromChosenCandidates>
{
    fn composed(
        memo: &Memo,
        candidate: CandidateForMemoGroupId,
        chosen_candidates: &ChosenCandidatePerMemoGroup,
        done: &mut HashMap<CandidateForMemoGroupId, Rc<OperatorNode>>,
        composing: &mut HashSet<CandidateForMemoGroupId>,
    ) -> Result<Rc<OperatorNode>, ErrorBuildingLogicalASAPDagsFromChosenCandidates> {
        if let Some(found) = done.get(&candidate) {
            return Ok(Rc::clone(found));
        }
        if !composing.insert(candidate) {
            return Err(ErrorBuildingLogicalASAPDagsFromChosenCandidates::Cycle { candidate });
        }
        let template = memo.operator_node_of(candidate)?;
        let mut producers = Vec::new();
        for input in 0..inputs_of(memo, candidate).len() {
            let producer = *chosen_candidates
                .producer_of_input
                .get(&(candidate, input))
                .ok_or(ErrorBuildingLogicalASAPDagsFromChosenCandidates::NoProducer {
                    candidate,
                    input,
                })?;
            producers.push(composed(memo, producer, chosen_candidates, done, composing)?);
        }
        let mut next = producers.into_iter();
        let node = Rc::new(
            template
                .with_new_children(|_| next.next().expect("one producer per input"))?
                .with_guarantee(template.guarantee.clone()),
        );
        done.insert(candidate, Rc::clone(&node));
        Ok(node)
    }
    let mut done = HashMap::new();
    let mut composing = HashSet::new();
    let (groups, roots): (Vec<_>, Vec<_>) = root_groups(memo)
        .map(|group| {
            let index = chosen_candidates
                .root_candidates
                .get(&group)
                .copied()
                .unwrap_or(0);
            if index >= memo.candidates[group.0 as usize].len() {
                return Err(ErrorBuildingLogicalASAPDagsFromChosenCandidates::NoSuchCandidate {
                    group,
                    candidate: index,
                });
            }
            let candidate = CandidateForMemoGroupId { group, index };
            Ok((
                group,
                QueryRoot::Operator(composed(
                    memo,
                    candidate,
                    chosen_candidates,
                    &mut done,
                    &mut composing,
                )?),
            ))
        })
        .collect::<Result<Vec<_>, ErrorBuildingLogicalASAPDagsFromChosenCandidates>>()?
        .into_iter()
        .unzip();
    let roots = recompose_guarantees(&roots)?;
    Ok(groups
        .into_iter()
        .zip(roots)
        .map(|(group, root)| match root {
            QueryRoot::Operator(node) => (group, node),
            QueryRoot::Scalar(_) => unreachable!("composed from operator roots"),
        })
        .collect())
}

/// Whether `from` is `to` or reads it through the producers `chosen` names.
fn reaches(
    memo: &Memo,
    chosen: &ChosenCandidatePerMemoGroup,
    from: CandidateForMemoGroupId,
    to: CandidateForMemoGroupId,
) -> bool {
    let mut pending = vec![from];
    let mut seen = HashSet::new();
    while let Some(candidate) = pending.pop() {
        if candidate == to {
            return true;
        }
        if !seen.insert(candidate) {
            continue;
        }
        for input in 0..inputs_of(memo, candidate).len() {
            if let Some(&producer) = chosen.producer_of_input.get(&(candidate, input)) {
                pending.push(producer);
            }
        }
    }
    false
}

fn root_groups(memo: &Memo) -> impl DoubleEndedIterator<Item = MemoGroupId> {
    asap_types::stages::root_groups(&memo.query_roots).into_iter()
}
