//! Stage 2's output: every operator node of every implementation candidate,
//! each with the timings and materializations Stage 2 allows, and which
//! candidates may produce each input. Stage 3 selects among them; Stage 2
//! prices nothing.
use std::rc::Rc;

use super::graph::{PhysicalCandidateId, ReaderConstraint};
use super::producers::ProducerCandidates;
use crate::ir::properties::ExecutionTiming;
use crate::ir::{OperatorNode, QueryRoot};
use crate::physical::Materialization;
use crate::stages::{MemoGroupId, PrunedCandidate};

/// One implementation candidate: index `index` in group `group`'s
/// candidates ([`MemoCandidates::candidates`](crate::stages::MemoCandidates::candidates)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateForMemoGroupId {
    pub group: MemoGroupId,
    pub index: usize,
}

/// Index into
/// [`PhysicalASAPMemoCandidates::operator_nodes_of_candidates_for_memo_groups`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperatorNodeOfCandidateForMemoGroupId(pub usize);

/// One operator node of one or more candidates for a memo group, before
/// Stage 2 chose its timing and materialization.
#[derive(Debug, Clone)]
pub struct OperatorNodeOfCandidateForMemoGroup {
    /// Every candidate of the group this node can implement, ascending:
    /// identical terminal readers of different candidates are factored into
    /// one node, and the selected producers decide which one it implements.
    /// A private node belongs to the one candidate it was lowered from.
    pub candidates_for_memo_group: Vec<CandidateForMemoGroupId>,
    /// Whether it is its candidates' root; false for an implementation's
    /// private nodes.
    pub is_implementation_root: bool,
    /// How many equal shares of a query's accuracy budget this node's sketch sizes meet
    /// (`CandidateForMemoGroup::accuracy_budget_parts`). For a factored node, the largest among its
    /// candidates: equal nodes have equal sizes. 1 for an exact node.
    pub accuracy_budget_parts: usize,
    /// The operator node, with Stage 2's physical implementation; its
    /// children are the expected inputs, which the selected producers replace.
    pub physical_node: Rc<OperatorNode>,
}

impl OperatorNodeOfCandidateForMemoGroup {
    /// The memo group whose candidates this node implements.
    /// `candidates_for_memo_group` is never empty, and all its entries are
    /// candidates of this one group.
    pub fn group(&self) -> MemoGroupId {
        let group = self.candidates_for_memo_group[0].group;
        debug_assert!(self
            .candidates_for_memo_group
            .iter()
            .all(|candidate| candidate.group == group));
        group
    }
}

/// One operator node with one timing and materialization: what Stage 3
/// selects. A selection may run it more than once, each run a node of the
/// [`DagOfSelectedCandidates`](super::DagOfSelectedCandidates).
#[derive(Debug, Clone)]
pub struct PhysicalCandidate {
    /// The operator node it runs.
    pub operator_node: OperatorNodeOfCandidateForMemoGroupId,
    /// When it runs. A node that is not materialized under an ingestion-time
    /// build still runs at ingestion time, so both are kept.
    pub timing: ExecutionTiming,
    /// Whether and where its result is kept.
    pub materialization: Materialization,
    /// Per input of its operator node (in `map_children` order), the
    /// candidates that may produce it. Depends on its timing.
    pub producer_candidates_per_input: Vec<ProducerCandidates>,
    /// Whether it may produce a query's output: an implementation root at
    /// query time whose output matches its group's logical node, in a group
    /// some query's root reads.
    pub may_produce_query_output: bool,
    /// One entry per workload query: whether this pane chain may be kept
    /// across evaluations when that query is its only reader. Filled only
    /// for a `MatAtQueryKeptAcrossEvaluations(Memory)` pane chain in a
    /// deployment that keeps query-time results; empty otherwise.
    pub may_keep_across_evaluations_per_sole_reader_query: Vec<bool>,
}

/// A physical candidate Stage 2 pruned, named by what it would have been:
/// candidates are renumbered after pruning, so a pruned one has no
/// [`PhysicalCandidateId`]. Unique among a [`PhysicalASAPMemoCandidates`]'
/// `pruned_candidates`: an operator node has at most one candidate per
/// timing and materialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalCandidateKey {
    /// Its operator node: an index into the output's
    /// `operator_nodes_of_candidates_for_memo_groups`, which pruning keeps
    /// whole.
    pub operator_node: OperatorNodeOfCandidateForMemoGroupId,
    pub timing: ExecutionTiming,
    pub materialization: Materialization,
}

/// Stage 2's output, which Stage 3 selects from.
#[derive(Debug, Clone)]
pub struct PhysicalASAPMemoCandidates {
    /// One per operator node of an implementation, before Stage 2 chose its
    /// timing and materialization.
    pub operator_nodes_of_candidates_for_memo_groups: Vec<OperatorNodeOfCandidateForMemoGroup>,
    /// One per (operator node, timing, materialization): what Stage 3
    /// selects. Indexed by [`PhysicalCandidateId`].
    pub candidates: Vec<PhysicalCandidate>,
    /// Each query's root over groups, by index in the workload, passed
    /// through from Pass 2. Its group references, in order, are the output
    /// references of [`Self::query_output_candidates`].
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// Legality that depends on which readers are selected.
    pub reader_constraints: Vec<ReaderConstraint>,
    /// The candidates Stage 2 pruned, each on its operator node's memo
    /// group, in pruning order: configurations it does not generate, then
    /// candidates no buildable input, query output or reader count can use.
    /// Empty when built by [`Self::new`].
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidateKey>>,
    /// By group index: the candidates whose operator node is an
    /// implementation root of that group, ascending.
    implementation_roots_of_group: Vec<Vec<PhysicalCandidateId>>,
    /// Each output reference's group: [`output_references`] of
    /// `query_roots`, built once.
    output_reference_groups: Vec<MemoGroupId>,
}

/// The output references of `query_roots`: each operator reference, in
/// order (a scalar root's in `map_operator_refs` order), with the index of
/// the query whose root it is.
pub fn output_references<C: Clone>(query_roots: &[QueryRoot<C>]) -> Vec<(usize, C)> {
    let mut references = Vec::new();
    for (query, root) in query_roots.iter().enumerate() {
        match root {
            QueryRoot::Operator(reference) => references.push((query, reference.clone())),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |reference| {
                    references.push((query, reference.clone()))
                });
            }
        }
    }
    references
}

impl PhysicalASAPMemoCandidates {
    /// Builds the private per-group index over `candidates`. Stage 3's
    /// component search uses it to build one smaller catalog per component.
    pub fn new(
        operator_nodes_of_candidates_for_memo_groups: Vec<OperatorNodeOfCandidateForMemoGroup>,
        candidates: Vec<PhysicalCandidate>,
        query_roots: Vec<QueryRoot<MemoGroupId>>,
        reader_constraints: Vec<ReaderConstraint>,
    ) -> Self {
        let mut implementation_roots_of_group: Vec<Vec<PhysicalCandidateId>> = Vec::new();
        for (id, candidate) in candidates.iter().enumerate() {
            let node = &operator_nodes_of_candidates_for_memo_groups[candidate.operator_node.0];
            if node.is_implementation_root {
                let group = node.group().0 as usize;
                if implementation_roots_of_group.len() <= group {
                    implementation_roots_of_group.resize(group + 1, Vec::new());
                }
                implementation_roots_of_group[group].push(PhysicalCandidateId(id));
            }
        }
        let output_reference_groups = output_references(&query_roots)
            .into_iter()
            .map(|(_, group)| group)
            .collect();
        Self {
            operator_nodes_of_candidates_for_memo_groups,
            candidates,
            query_roots,
            reader_constraints,
            pruned_candidates: Vec::new(),
            implementation_roots_of_group,
            output_reference_groups,
        }
    }

    /// The implementation roots of `group`, ascending.
    pub fn implementation_roots(&self, group: MemoGroupId) -> &[PhysicalCandidateId] {
        self.implementation_roots_of_group
            .get(group.0 as usize)
            .map_or(&[], Vec::as_slice)
    }

    /// The candidates that may produce output reference `output_reference`
    /// (each group reference of `query_roots`, in order; a scalar query has
    /// several): its group's roots with `may_produce_query_output`.
    pub fn query_output_candidates(
        &self,
        output_reference: usize,
    ) -> impl Iterator<Item = PhysicalCandidateId> + '_ {
        let roots = self
            .output_reference_groups
            .get(output_reference)
            .map_or(&[][..], |&group| self.implementation_roots(group));
        roots
            .iter()
            .copied()
            .filter(|id| self.candidates[id.0].may_produce_query_output)
    }

    /// The number of output references: group references of `query_roots`.
    pub fn output_reference_count(&self) -> usize {
        self.output_reference_groups.len()
    }

    /// The operator node candidate `candidate` runs.
    pub fn physical_node(&self, candidate: PhysicalCandidateId) -> &Rc<OperatorNode> {
        &self.operator_nodes_of_candidates_for_memo_groups
            [self.candidates[candidate.0].operator_node.0]
            .physical_node
    }

    /// The memo group whose candidates candidate `candidate` implements.
    pub fn memo_group_of(&self, candidate: PhysicalCandidateId) -> MemoGroupId {
        self.operator_nodes_of_candidates_for_memo_groups
            [self.candidates[candidate.0].operator_node.0]
            .group()
    }
}
