//! The candidate DAGs a memo stands for: each choice of one implementation
//! candidate per needed group, and the complete DAG that choice composes.
//!
//! A group is needed when a query's root is in it or a chosen candidate reads
//! it. A group read by several chosen candidates takes one candidate, so its
//! nodes are built once and shared; which groups are needed depends on the
//! candidates chosen above them. The count is therefore not a product over
//! groups.
//!
//! A choice is counted, not the DAG it composes: two candidates of a group
//! that differ only in what recomposing guarantees re-derives compose equal
//! DAGs. Composing checks only that a choice composes; input contracts,
//! accuracy at the roots, Stage 2 legality and Stage 3 cost stay with the
//! validator and the later stages. The memo's groups are acyclic, as every
//! memo step builds them, but candidates may read each other (a rollup of
//! a finer grouping whose candidate reads the coarser one): a choice taking
//! such candidates together composes no DAG and is not enumerated.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::{OperatorNode, QueryRoot};

use super::rules::reuse::replace;
use crate::pass1::per_computation_candidates::PerComputationCandidateError;
use asap_accuracy::propagation::recompose_guarantees;
use asap_types::stages::MemoCandidates;
use asap_types::stages::MemoGroupId;

/// One candidate (an index into `MemoCandidates::candidates[group]`) for each
/// group it names. [`build_logical_asap_dags_from_chosen_candidates`] gives a group it does not name its first
/// candidate.
pub type ChosenCandidatePerMemoGroup = BTreeMap<MemoGroupId, usize>;

/// The choices [`enumerate`] found, in depth-first order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllChosenCandidatesPerMemoGroup {
    /// Each choice names exactly the groups it needs.
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
    #[error("group {group:?}'s candidate {candidate} reads group {child:?} but has no node of it to replace")]
    ChildNotInCandidate {
        group: MemoGroupId,
        candidate: usize,
        child: MemoGroupId,
    },
    /// The chosen candidates read each other in a cycle (e.g. a coarse
    /// group's rollup reads a finer group whose chosen candidate reads it).
    #[error("group {group:?} reads itself through the chosen candidates")]
    Cycle { group: MemoGroupId },
    /// Re-deriving the composed nodes' schemas and guarantees failed.
    #[error("recomposing the composed DAG: {0}")]
    Recompose(#[from] PerComputationCandidateError),
}

/// Every choice of one candidate per needed group, starting from the query
/// roots, stopping after `limit` choices. A choice whose candidates read
/// each other in a cycle composes no DAG and is left out.
pub fn enumerate(memo: &MemoCandidates, limit: usize) -> AllChosenCandidatesPerMemoGroup {
    fn walk(
        memo: &MemoCandidates,
        mut pending: Vec<MemoGroupId>,
        chosen_candidates: &mut ChosenCandidatePerMemoGroup,
        limit: usize,
        all_chosen_candidates: &mut Vec<ChosenCandidatePerMemoGroup>,
    ) -> bool {
        let group = loop {
            match pending.pop() {
                None => {
                    if all_chosen_candidates.len() == limit {
                        return false;
                    }
                    all_chosen_candidates.push(chosen_candidates.clone());
                    return true;
                }
                Some(group) if !chosen_candidates.contains_key(&group) => break group,
                Some(_) => {}
            }
        };
        for (index, candidate) in memo.candidates[group.0 as usize].iter().enumerate() {
            // A candidate reading a group that already reads this one closes
            // a cycle; every edge is checked as its group is chosen.
            if candidate
                .input_groups
                .iter()
                .any(|&child| reaches(memo, chosen_candidates, child, group))
            {
                continue;
            }
            chosen_candidates.insert(group, index);
            let mut next = pending.clone();
            next.extend(candidate.input_groups.iter().rev());
            let finished = walk(memo, next, chosen_candidates, limit, all_chosen_candidates);
            chosen_candidates.remove(&group);
            if !finished {
                return false;
            }
        }
        true
    }
    let roots = root_groups(memo).rev().collect();
    let mut all_chosen_candidates = Vec::new();
    let complete = walk(
        memo,
        roots,
        &mut ChosenCandidatePerMemoGroup::new(),
        limit,
        &mut all_chosen_candidates,
    );
    AllChosenCandidatesPerMemoGroup {
        all_chosen_candidates,
        complete,
    }
}

/// Each root group (one with a query's root) and its DAG under `chosen_candidates`:
/// each needed group implemented by its chosen candidate over its children's
/// composed DAGs, a group read several times composed once and shared. Every
/// node is then rebuilt with its derived schema and composed guarantee, as
/// in plan reconstruction. A scalar query's expression is not rebuilt: its
/// operator roots are root groups like any other query's.
pub fn build_logical_asap_dags_from_chosen_candidates(
    memo: &MemoCandidates,
    chosen_candidates: &ChosenCandidatePerMemoGroup,
) -> Result<Vec<(MemoGroupId, Rc<OperatorNode>)>, ErrorBuildingLogicalASAPDagsFromChosenCandidates>
{
    fn composed(
        memo: &MemoCandidates,
        group: MemoGroupId,
        chosen_candidates: &ChosenCandidatePerMemoGroup,
        done: &mut HashMap<MemoGroupId, Rc<OperatorNode>>,
        composing: &mut HashSet<MemoGroupId>,
    ) -> Result<Rc<OperatorNode>, ErrorBuildingLogicalASAPDagsFromChosenCandidates> {
        if let Some(found) = done.get(&group) {
            return Ok(Rc::clone(found));
        }
        if !composing.insert(group) {
            return Err(ErrorBuildingLogicalASAPDagsFromChosenCandidates::Cycle { group });
        }
        let index = chosen_candidates.get(&group).copied().unwrap_or(0);
        let candidate = memo.candidates[group.0 as usize].get(index).ok_or(
            ErrorBuildingLogicalASAPDagsFromChosenCandidates::NoSuchCandidate {
                group,
                candidate: index,
            },
        )?;
        let own_nodes: HashSet<_> = OperatorNode::reachable(&candidate.root_node)
            .iter()
            .map(Rc::as_ptr)
            .collect();
        let mut replacements = HashMap::new();
        for &child in &candidate.input_groups {
            let old = &memo.groups[child.0 as usize].logical_node;
            if !own_nodes.contains(&Rc::as_ptr(old)) {
                return Err(
                    ErrorBuildingLogicalASAPDagsFromChosenCandidates::ChildNotInCandidate {
                        group,
                        candidate: index,
                        child,
                    },
                );
            }
            replacements.insert(
                Rc::as_ptr(old),
                composed(memo, child, chosen_candidates, done, composing)?,
            );
        }
        let node = replace(&candidate.root_node, &replacements);
        done.insert(group, Rc::clone(&node));
        Ok(node)
    }
    let mut done = HashMap::new();
    let mut composing = HashSet::new();
    let (groups, roots): (Vec<_>, Vec<_>) = root_groups(memo)
        .map(|group| {
            Ok((
                group,
                QueryRoot::Operator(composed(
                    memo,
                    group,
                    chosen_candidates,
                    &mut done,
                    &mut composing,
                )?),
            ))
        })
        .collect::<Result<Vec<_>, ErrorBuildingLogicalASAPDagsFromChosenCandidates>>()?
        .into_iter()
        .unzip();
    let roots = recompose_guarantees(&roots).map_err(PerComputationCandidateError::from)?;
    Ok(groups
        .into_iter()
        .zip(roots)
        .map(|(group, root)| match root {
            QueryRoot::Operator(node) => (group, node),
            QueryRoot::Scalar(_) => unreachable!("composed from operator roots"),
        })
        .collect())
}

/// Whether `from` is `to` or reads it through the candidates `chosen_candidates` names.
fn reaches(
    memo: &MemoCandidates,
    chosen_candidates: &ChosenCandidatePerMemoGroup,
    from: MemoGroupId,
    to: MemoGroupId,
) -> bool {
    let mut pending = vec![from];
    let mut seen = HashSet::new();
    while let Some(group) = pending.pop() {
        if group == to {
            return true;
        }
        if !seen.insert(group) {
            continue;
        }
        if let Some(&index) = chosen_candidates.get(&group) {
            pending.extend(&memo.candidates[group.0 as usize][index].input_groups);
        }
    }
    false
}

fn root_groups(memo: &MemoCandidates) -> impl DoubleEndedIterator<Item = MemoGroupId> {
    asap_types::stages::root_groups(&memo.query_roots).into_iter()
}
