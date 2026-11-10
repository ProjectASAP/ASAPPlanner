//! Memo scope: the memo, Stage 0's and Stage 1's one type, and each query
//! over its groups; the operations over the whole memo
//! (concepts doc `memo-groups-properties-requirements.md`, section 3; #1203).

use std::collections::{BTreeSet, HashSet};

use super::group::derive_group_properties;
use crate::accuracy::AccuracyTarget;
use crate::ir::Operator;
use crate::ir::QueryRoot;
use crate::ir::SchemaDerivationError;
use crate::stages::{
    CandidateError, CandidateForMemoGroup, CandidateForMemoGroupId, LogicalRequirements, MemoGroup,
    MemoGroupId,
};

/// The memo, Stage 0's and Stage 1's one type: the memo groups, each with
/// its candidates ([`MemoGroup::candidates`]: empty after Stage 0), and each
/// query's root over them.
#[derive(Debug, Clone, Default)]
pub struct Memo {
    /// The groups, by id ([`Self::groups`]); added only through
    /// [`Self::find_or_add_group`].
    groups: Vec<MemoGroup>,
    /// The queries, by index in the workload.
    pub queries: Vec<MemoQuery>,
}

/// One query of the memo: its root over groups, its accuracy target and
/// $n_q$.
#[derive(Debug, Clone)]
pub struct MemoQuery {
    pub root: QueryRoot<MemoGroupId>,
    /// $A_q$; `None` for a query without one (every query when accuracy is
    /// not enforced).
    pub accuracy_target: Option<AccuracyTarget>,
    /// $n_q$, the number of values one evaluation returns; `None` when the
    /// data workload does not bound it. Pass 2 turns a whole-result failure
    /// probability into a per-value one with it.
    pub number_of_values_per_query_evaluation: Option<u64>,
}

impl Memo {
    /// The groups, by id.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
    }

    /// Adds `query` and returns its index: its root's groups and every
    /// group below them list it ([`MemoGroup::reached_by_queries`]). Stage 0
    /// adds each query once.
    pub fn add_query(&mut self, query: MemoQuery) -> usize {
        let _ = query;
        todo!("#1207 PR 2")
    }

    /// Hash-consing: the group with this expression, added if new, with the
    /// `GroupProperties` [`derive_group_properties`] derives. Fails when the
    /// expression breaks the node rules (schema derivation, input
    /// validation, merge check).
    pub fn find_or_add_group(
        &mut self,
        expression: Operator<MemoGroupId>,
    ) -> Result<MemoGroupId, SchemaDerivationError> {
        let _group_properties = derive_group_properties(self, &expression)?;
        todo!("#1207 PR 2: hash-consing over groups")
    }

    /// Adds `candidate` to `group`'s candidates. Fails when its expression
    /// breaks the node rules ([`derive_group_properties`]) or its output
    /// schema or kind differs from the group's.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> Result<CandidateForMemoGroupId, CandidateError> {
        let _output = derive_group_properties(self, &candidate.expression.operator)
            .map_err(CandidateError::Schema)?;
        let _ = group;
        todo!("#1207 PR 2")
    }

    /// The candidate `id`.
    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup {
        let _ = id;
        todo!("#1207 PR 2")
    }

    /// Replaces candidate `id`'s input requirements $r^{\log}_{c,i}$, one
    /// per input group. The only way to change a candidate's $r^{\log}$:
    /// Pass 2 sets a readout's required accuracy after the rules have run.
    /// Panics when the vector does not have one entry per input group.
    pub fn set_candidate_input_requirements(
        &mut self,
        id: CandidateForMemoGroupId,
        input_requirements: Vec<LogicalRequirements>,
    ) {
        let candidate = &mut self.groups[id.group.0 as usize].candidates[id.index_in_group];
        assert_eq!(
            input_requirements.len(),
            candidate.expression.operator.children().len(),
            "one input requirement per input group of candidate {id:?}"
        );
        candidate.input_requirements = input_requirements;
    }

    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: a dependency order for
    /// candidate traversal, whatever order rules appended groups in. `None`
    /// when those reads form a cycle, a group reading itself included.
    pub fn topological_order(&self) -> Option<Vec<MemoGroupId>> {
        let count = self.groups.len();
        let mut reads: Vec<HashSet<usize>> = vec![HashSet::new(); count];
        for (g, group) in self.groups.iter().enumerate() {
            for child in group.expression.children().into_iter().chain(
                group
                    .candidates
                    .iter()
                    .flat_map(|c| c.expression.operator.children()),
            ) {
                // A group reading itself never becomes ready: a cycle.
                reads[g].insert(child.0 as usize);
            }
        }
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut waiting: Vec<usize> = reads.iter().map(HashSet::len).collect();
        for (g, read) in reads.iter().enumerate() {
            for &child in read {
                consumers[child].push(g);
            }
        }
        let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
            .filter(|&g| waiting[g] == 0)
            .map(std::cmp::Reverse)
            .collect();
        let mut order = Vec::with_capacity(count);
        while let Some(std::cmp::Reverse(g)) = ready.pop() {
            order.push(MemoGroupId(g as u32));
            for &consumer in &consumers[g] {
                waiting[consumer] -= 1;
                if waiting[consumer] == 0 {
                    ready.push(std::cmp::Reverse(consumer));
                }
            }
        }
        (order.len() == count).then_some(order)
    }
}

/// The groups some query's root reads, ascending: the groups of the
/// `queries`' roots that are a query's result or a scalar query's operand.
pub fn root_groups(queries: &[MemoQuery]) -> BTreeSet<MemoGroupId> {
    queries
        .iter()
        .flat_map(|query| query.root.operator_refs())
        .copied()
        .collect()
}

/// The queries whose root reads `group`, ascending.
pub fn queries_rooted_at(queries: &[MemoQuery], group: MemoGroupId) -> Vec<usize> {
    (0..queries.len())
        .filter(|&query| queries[query].root.operator_refs().contains(&&group))
        .collect()
}
