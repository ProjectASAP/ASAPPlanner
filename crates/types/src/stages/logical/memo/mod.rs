//! Memo scope: the memo, Stage 0's and Stage 1's one type, and each query
//! over its groups; the operations over the whole memo
//! (concepts doc `memo-groups-properties-requirements.md`, section 3; #1203).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

mod hash_consing;

use super::group::{
    derive_group_properties, validate_expression_over_input_groups, MemoGroupAsInput,
    MemoGroupError,
};
use crate::accuracy::AccuracyTarget;
use crate::ir::operator::input::InputOrTemporaryBuild;
use crate::ir::schema::Schema;
use crate::ir::Operator;
use crate::ir::QueryRoot;
use crate::stages::{
    CandidateError, CandidateForMemoGroup, CandidateForMemoGroupId, LogicalRequirements, MemoGroup,
    MemoGroupId,
};
use crate::workload::{DataWorkload, SourceFacts};
use hash_consing::{
    expression_hash, same_expression_and_logical_properties, same_serialized_expression,
    serialized_expression,
};

/// The memo, Stage 0's and Stage 1's one type: the memo groups, each with
/// its candidates ([`MemoGroup::candidates`]: empty after Stage 0), and each
/// query's root over them.
#[derive(Debug, Clone)]
pub struct Memo {
    /// The groups, by id ([`Self::groups`]); added only through
    /// [`Self::find_or_add_group`].
    groups: Vec<MemoGroup>,
    /// The queries, by index in the workload ([`Self::queries`]); added only
    /// through [`Self::add_query`].
    queries: Vec<MemoQuery>,
    /// The groups [`Self::find_or_add_group`] may return, by
    /// [`expression_hash`] of their expression: every group whose sharing
    /// is legal ([`GroupProperties::shareable_when_equal`](crate::stages::GroupProperties::shareable_when_equal)).
    groups_by_expression_hash: HashMap<u64, Vec<(MemoGroupId, String)>>,
    /// The data workload's facts per source (`DataWorkload::sources`), fixed
    /// at construction: a `Scan` group's column signs read them.
    pub(super) source_facts: BTreeMap<String, SourceFacts>,
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
    /// A memo without groups over the sources `data` describes: a `Scan`
    /// group's column signs are the ones `data` declares for its source
    /// (`GroupProperties::column_signs`).
    pub fn new(data: &DataWorkload) -> Self {
        Self {
            groups: Vec::new(),
            queries: Vec::new(),
            groups_by_expression_hash: HashMap::new(),
            source_facts: data.sources.clone(),
        }
    }

    /// The groups, by id.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
    }

    /// The queries, by index in the workload.
    pub fn queries(&self) -> &[MemoQuery] {
        &self.queries
    }

    /// Adds `query` and returns its index: its root's groups and every
    /// group below them list it ([`MemoGroup::reached_by_queries`]). Stage 0
    /// adds each query once.
    pub fn add_query(&mut self, query: MemoQuery) -> usize {
        let index = self.queries.len();
        let groups = query.root.operator_refs().into_iter().copied().collect();
        self.queries.push(query);
        self.add_queries_below(groups, &[index]);
        index
    }

    /// Hash-consing: the group whose expression is `expression` (operator,
    /// arguments and input groups by typed equality, confirmed by equal
    /// serializations: `same_serialized_expression`), else a
    /// new group for it, with the `GroupProperties` `derive_group_properties`
    /// derives from its input groups, listed as a parent of each input
    /// group. A group whose copies may return different rows (an unordered
    /// `LIMIT`, [`GroupProperties::shareable_when_equal`](crate::stages::GroupProperties::shareable_when_equal)) is never returned
    /// for another expression, so such an expression always gets a new
    /// group. Fails, leaving the memo unchanged, when the expression is not
    /// valid over its input groups (its output schema does not derive, an
    /// input fails validation, or a merge reads states it may not merge).
    pub fn find_or_add_group(
        &mut self,
        expression: Operator<MemoGroupId>,
    ) -> Result<MemoGroupId, MemoGroupError> {
        let serialized = serialized_expression(&expression);
        let hash = expression_hash(&serialized);
        // Only shareable groups are listed by hash, and an equal expression
        // is as shareable as the group it equals, which was validated.
        if let Some((group, _)) = self
            .groups_by_expression_hash
            .get(&hash)
            .and_then(|groups| {
                groups.iter().find(|(group, known)| {
                    same_serialized_expression(
                        (&self.groups[group.0 as usize].expression, known),
                        (&expression, &serialized),
                    )
                })
            })
        {
            return Ok(*group);
        }
        let id = MemoGroupId(
            u32::try_from(self.groups.len()).expect("a memo has at most u32::MAX groups"),
        );
        let group_properties = derive_group_properties(self, id, &expression)?;
        let shareable = group_properties.shareable_when_equal;
        for &input in expression.children() {
            add_parent(&mut self.groups[input.0 as usize], id);
        }
        self.groups.push(MemoGroup {
            expression,
            group_properties,
            parents: Vec::new(),
            queries: Vec::new(),
            candidates: Vec::new(),
        });
        if shareable {
            self.groups_by_expression_hash
                .entry(hash)
                .or_default()
                .push((id, serialized));
        }
        Ok(id)
    }

    /// The output schema `expression` would have as a new group, without
    /// adding anything: the dry run of [`Self::find_or_add_group`], which a
    /// rule calls to decide whether a way to compute a group can be built
    /// before it adds any of the way's groups. `expression` reads memo
    /// groups or pending expressions, not yet in the memo: the input
    /// `MemoGroupId(n + k)`, `n` the memo's group count, is
    /// `pending_expressions[k]`, the id it would get as a new group, with
    /// output schema `pending_schemas[k]` (from earlier calls). The
    /// expression's output schema must derive and its inputs pass
    /// validation; a merge's coverage is checked only when it is added.
    ///
    /// # Panics
    ///
    /// When the two pending lists differ in length, or a pending
    /// expression reads one at or after it.
    pub fn output_schema_of_pending_expression(
        &self,
        pending_expressions: &[Operator<MemoGroupId>],
        pending_schemas: &[Schema],
        expression: &Operator<MemoGroupId>,
    ) -> Result<Schema, MemoGroupError> {
        assert_eq!(
            pending_expressions.len(),
            pending_schemas.len(),
            "one schema per pending expression"
        );
        fn input<'m>(
            memo: &'m Memo,
            built: &[InputOrTemporaryBuild<MemoGroupAsInput<'m>>],
            group_id: MemoGroupId,
        ) -> InputOrTemporaryBuild<MemoGroupAsInput<'m>> {
            match (group_id.0 as usize).checked_sub(memo.groups.len()) {
                None => InputOrTemporaryBuild::Input(MemoGroupAsInput { memo, group_id }),
                Some(index) => built
                    .get(index)
                    .expect("a pending expression reads only earlier ones")
                    .clone(),
            }
        }
        let mut built = Vec::with_capacity(pending_expressions.len());
        for (pending, schema) in pending_expressions.iter().zip(pending_schemas) {
            let operator = pending.map_children(|&group_id| input(self, &built, group_id));
            built.push(InputOrTemporaryBuild::with_schema(operator, schema.clone()));
        }
        let operator = expression.map_children(|&group_id| input(self, &built, group_id));
        let schema = operator.output_schema()?;
        operator.validate_inputs()?;
        Ok(schema)
    }

    /// Adds `candidate` to `group`'s candidates and returns its id, unless
    /// the group already has a candidate with the same expression
    /// (`same_expression`) and logical properties: then that candidate's
    /// id, and `candidate` is dropped. A new candidate makes `group` a parent
    /// of each of its input groups, and `group`'s queries reach each input
    /// group and every group below it.
    ///
    /// Fails, leaving the memo unchanged, when the candidate's expression
    /// is not valid over its input groups
    /// (`validate_expression_over_input_groups`),
    /// or when its output schema or kind differs from the group's: then it
    /// does not compute the group's result.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> Result<CandidateForMemoGroupId, CandidateError> {
        if let Some(index_in_group) = self.groups[group.0 as usize]
            .candidates
            .iter()
            .position(|known| same_expression_and_logical_properties(known, &candidate))
        {
            return Ok(CandidateForMemoGroupId {
                group,
                index_in_group,
            });
        }
        let operator = candidate
            .expression
            .operator
            .map_children(|&group_id| MemoGroupAsInput {
                memo: self,
                group_id,
            });
        let (schema, _) = validate_expression_over_input_groups(&operator)
            .map_err(CandidateError::NotValidOverInputGroups)?;
        let result_kind = operator.output_kind();
        let group_properties = &self.groups[group.0 as usize].group_properties;
        if schema != group_properties.schema || result_kind != group_properties.result_kind {
            return Err(CandidateError::OutputDiffersFromGroup { group });
        }
        let candidates = &mut self.groups[group.0 as usize].candidates;
        let input_groups: Vec<MemoGroupId> = candidate
            .expression
            .operator
            .children()
            .into_iter()
            .copied()
            .collect();
        candidates.push(candidate);
        let index_in_group = candidates.len() - 1;
        for &input in &input_groups {
            add_parent(&mut self.groups[input.0 as usize], group);
        }
        let queries = self.groups[group.0 as usize].queries.clone();
        self.add_queries_below(input_groups, &queries);
        Ok(CandidateForMemoGroupId {
            group,
            index_in_group,
        })
    }

    /// Adds `queries` to each of `groups` and every group below them, through
    /// the groups' expressions and their candidates' input groups. A group
    /// that has every query already is not walked: every group below it has
    /// them too.
    fn add_queries_below(&mut self, groups: Vec<MemoGroupId>, queries: &[usize]) {
        let mut to_visit = groups;
        while let Some(group) = to_visit.pop() {
            let group = &mut self.groups[group.0 as usize];
            let mut added = false;
            for &query in queries {
                if let Err(position) = group.queries.binary_search(&query) {
                    group.queries.insert(position, query);
                    added = true;
                }
            }
            if added {
                to_visit.extend(
                    group
                        .expression
                        .children()
                        .into_iter()
                        .chain(
                            group
                                .candidates
                                .iter()
                                .flat_map(|candidate| candidate.expression.operator.children()),
                        )
                        .copied(),
                );
            }
        }
    }

    /// The candidate `id`.
    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup {
        &self.groups[id.group.0 as usize].candidates[id.index_in_group]
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

/// Lists `parent` among `group`'s parents, once.
fn add_parent(group: &mut MemoGroup, parent: MemoGroupId) {
    if !group.parents.contains(&parent) {
        group.parents.push(parent);
    }
}

impl Memo {
    /// The groups some query's root reads, ascending: the groups of the
    /// queries' roots that are a query's result or a scalar query's operand.
    pub fn root_groups(&self) -> BTreeSet<MemoGroupId> {
        self.queries
            .iter()
            .flat_map(|query| query.root.operator_refs())
            .copied()
            .collect()
    }

    /// The queries whose root reads `group`, ascending.
    pub fn queries_rooted_at(&self, group: MemoGroupId) -> Vec<usize> {
        (0..self.queries.len())
            .filter(|&query| self.queries[query].root.operator_refs().contains(&&group))
            .collect()
    }
}

#[cfg(test)]
mod tests;
