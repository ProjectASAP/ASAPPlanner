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
//! validator and the later stages. The memo must be acyclic, as every memo
//! step builds it.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use asap_types::ir::{OperatorNode, QueryRoot};

use super::candidates::CandidateMemo;
use super::rules::reuse::replace;
use super::MemoGroupId;
use crate::per_computation_candidates::{recompose_guarantees, PerComputationCandidateError};

/// One candidate (an index into `CandidateMemo::candidates[group]`) for each
/// group it names. [`compose`] gives a group it does not name its first
/// candidate.
pub type MemoChoice = BTreeMap<MemoGroupId, usize>;

/// The choices [`enumerate`] found, in depth-first order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoEnumeration {
    /// Each choice names exactly the groups it needs.
    pub choices: Vec<MemoChoice>,
    /// False when the limit stopped the enumeration before every choice was found.
    pub complete: bool,
}

/// A choice that does not compose.
#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
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
    /// Re-deriving the composed nodes' schemas and guarantees failed.
    #[error("recomposing the composed DAG: {0}")]
    Recompose(#[from] PerComputationCandidateError),
}

/// Every choice of one candidate per needed group, starting from the query
/// roots, stopping after `limit` choices.
pub fn enumerate(memo: &CandidateMemo, limit: usize) -> MemoEnumeration {
    fn walk(
        memo: &CandidateMemo,
        mut pending: Vec<MemoGroupId>,
        choice: &mut MemoChoice,
        limit: usize,
        choices: &mut Vec<MemoChoice>,
    ) -> bool {
        let group = loop {
            match pending.pop() {
                None => {
                    if choices.len() == limit {
                        return false;
                    }
                    choices.push(choice.clone());
                    return true;
                }
                Some(group) if !choice.contains_key(&group) => break group,
                Some(_) => {}
            }
        };
        for (index, candidate) in memo.candidates[group.0 as usize].iter().enumerate() {
            choice.insert(group, index);
            let mut next = pending.clone();
            next.extend(candidate.children.iter().rev());
            let finished = walk(memo, next, choice, limit, choices);
            choice.remove(&group);
            if !finished {
                return false;
            }
        }
        true
    }
    let roots = root_groups(memo).rev().collect();
    let mut choices = Vec::new();
    let complete = walk(memo, roots, &mut MemoChoice::new(), limit, &mut choices);
    MemoEnumeration { choices, complete }
}

/// Each root group (one with a query's root) and its DAG under `choice`:
/// each needed group implemented by its chosen candidate over its children's
/// composed DAGs, a group read several times composed once and shared. Every
/// node is then rebuilt with its derived schema and composed guarantee, as
/// in plan reconstruction. A scalar query's expression is not rebuilt: its
/// operator roots are root groups like any other query's.
pub fn compose(
    memo: &CandidateMemo,
    choice: &MemoChoice,
) -> Result<Vec<(MemoGroupId, Rc<OperatorNode>)>, ComposeError> {
    fn composed(
        memo: &CandidateMemo,
        group: MemoGroupId,
        choice: &MemoChoice,
        done: &mut HashMap<MemoGroupId, Rc<OperatorNode>>,
    ) -> Result<Rc<OperatorNode>, ComposeError> {
        if let Some(found) = done.get(&group) {
            return Ok(Rc::clone(found));
        }
        let index = choice.get(&group).copied().unwrap_or(0);
        let candidate =
            memo.candidates[group.0 as usize]
                .get(index)
                .ok_or(ComposeError::NoSuchCandidate {
                    group,
                    candidate: index,
                })?;
        let own_nodes: std::collections::HashSet<_> = OperatorNode::reachable(&candidate.node)
            .iter()
            .map(Rc::as_ptr)
            .collect();
        let mut replacements = HashMap::new();
        for &child in &candidate.children {
            let old = &memo.groups[child.0 as usize].node;
            if !own_nodes.contains(&Rc::as_ptr(old)) {
                return Err(ComposeError::ChildNotInCandidate {
                    group,
                    candidate: index,
                    child,
                });
            }
            replacements.insert(Rc::as_ptr(old), composed(memo, child, choice, done)?);
        }
        let node = replace(&candidate.node, &replacements);
        done.insert(group, Rc::clone(&node));
        Ok(node)
    }
    let mut done = HashMap::new();
    let (groups, roots): (Vec<_>, Vec<_>) = root_groups(memo)
        .map(|group| {
            Ok((
                group,
                QueryRoot::Operator(composed(memo, group, choice, &mut done)?),
            ))
        })
        .collect::<Result<Vec<_>, ComposeError>>()?
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

fn root_groups(memo: &CandidateMemo) -> impl DoubleEndedIterator<Item = MemoGroupId> + '_ {
    memo.groups
        .iter()
        .enumerate()
        .filter(|(_, group)| !group.roots_of.is_empty())
        .map(|(index, _)| MemoGroupId(index as u32))
}
