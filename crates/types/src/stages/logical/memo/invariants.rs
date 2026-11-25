//! The memo's invariants (design doc `proposals/memo-invariants.md`; T4,
//! T5 and T25 of the testing-infrastructure design). Each check is one
//! [`InvariantCheck`]; a failed one is an [`InvariantViolation`], a bug in
//! the planner, never a property of the input.
//!
//! Example used below: `q0 = sum by (job) (rate(m[5m]))`,
//! `q1 = sum(rate(m[5m]))`, Stage 0 groups G0 `Scan m` → G1 last 5m → G2
//! `rate` → G3 `sum by (job)` (q0) and G4 `sum` (q1); Pass 2 adds a
//! candidate of G4 that sums G3.

use std::collections::{BTreeSet, HashMap};

use super::hash_consing::{same_candidate, serialized_expression};
use super::Memo;
use crate::ir::schema::Schema;
use crate::ir::{Operator, QueryRoot};
use crate::stages::logical::group::{aggregate_coverage_of, MemoGroupAsInput};
use crate::stages::{
    CandidateForMemoGroupId, GroupProperties, MemoGroupId, OriginOfCandidateForMemoGroup,
};

/// Which invariant failed. Insertion checks (1–4) run in
/// `Memo::find_or_add_group` / `Memo::add_candidate` before the write;
/// the rest after a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantCheck {
    /// 1. A group has one lineage and one sign entry per column: G3 has 2
    ///    columns, so 2 of each.
    ColumnFactsPerColumn,
    /// 2. A new group or candidate reads, and a candidate is added to, only
    ///    existing groups: reading G9 in a 5-group memo fails.
    ReadsMissingGroup,
    /// 3. A new candidate has one input requirement per input group.
    InputRequirementsPerInput,
    /// 4a. A new candidate's expression is valid over its input groups (its
    ///     schema derives, its inputs validate, a merge merges what it may).
    CandidateNotValidOverInputs,
    /// 4b. A new candidate's θ fits its group's output.
    CandidateParametersDoNotFit,
    /// 5. Each group's stored "used by" list is the groups whose expression
    ///    or candidate reads it: G3's includes G4.
    UsedByList,
    /// 6. Each group's stored queries are those whose root reaches it
    ///    through expressions and candidates: G3's are {q0, q1}.
    QueriesList,
    /// 7. Every group is reached by some query (no orphans).
    OrphanGroup,
    /// 8. No two shareable groups have the same expression, and the memo
    ///    finds each by it (adding G3's expression again returns G3).
    DuplicateGroup,
    /// 9. No group holds the same candidate twice.
    DuplicateCandidate,
    /// 10. A stage only adds: no group has fewer candidates than it was
    ///     given, the memo has no fewer groups, and the queries are
    ///     unchanged. Counts suffice: outside this crate the memo changes
    ///     only through methods that append (`find_or_add_group`,
    ///     `add_candidate`, `add_query`) or edit a candidate's input
    ///     requirements, so a stage cannot replace a candidate.
    StageRemoved,
    /// 11. Every Stage 2 physical candidate points to a memo candidate.
    MissingLogicalCandidate,
    /// 12. Each query's plan output has its Stage 0 group's schema: q0's
    ///     plan root outputs (`job`, value), as G3 does.
    PlanOutputSchema,
    /// 13. Every candidate has its group's output schema and kind
    ///     (thorough only).
    CandidateSchema,
    /// 14. Every candidate has its group's `aggregate_coverage`, compared
    ///     exactly (thorough only).
    CandidateCoverage,
}

impl std::fmt::Display for InvariantCheck {
    /// The snake_case name, as in JSON: `orphan_group`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = serde_json::to_value(self).expect("a unit variant serializes");
        f.write_str(
            name.as_str()
                .expect("a unit variant serializes as a string"),
        )
    }
}

/// Who broke an invariant (D13): the rule recorded on the candidate, else
/// the stage after which the check ran, else a group insertion (whose
/// caller the memo does not know).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantOrigin {
    Rule(OriginOfCandidateForMemoGroup),
    Stage(&'static str),
    GroupInsertion,
}

impl std::fmt::Display for InvariantOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule(rule) => write!(f, "rule {rule:?}"),
            Self::Stage(stage) => write!(f, "stage {stage}"),
            Self::GroupInsertion => write!(f, "group insertion"),
        }
    }
}

/// A broken invariant (D14): a planner bug, returned as an error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("invariant {check} violated at group {group:?} by {origin}: {detail}")]
pub struct InvariantViolation {
    pub check: InvariantCheck,
    /// The group it failed at; `None` when no one group is (the queries).
    pub group: Option<MemoGroupId>,
    pub origin: InvariantOrigin,
    /// What was found, for the person debugging it.
    pub detail: String,
}

/// Check 1: `properties`, derived for a new group computing `expression`,
/// have one lineage and one sign entry per column.
pub(super) fn check_column_facts(
    properties: &GroupProperties,
    expression: &Operator<MemoGroupId>,
) -> Result<(), InvariantViolation> {
    let columns = properties.schema.fields.len();
    if properties.column_lineage.len() == columns && properties.column_signs.len() == columns {
        return Ok(());
    }
    Err(InvariantViolation {
        check: InvariantCheck::ColumnFactsPerColumn,
        group: None,
        origin: InvariantOrigin::GroupInsertion,
        detail: format!(
            "{}: {columns} columns, {} lineage and {} sign entries",
            expression.kind_name(),
            properties.column_lineage.len(),
            properties.column_signs.len()
        ),
    })
}

/// What a stage was given, to check that it only adds (check 10).
#[derive(Debug, Clone)]
pub struct MemoContents {
    candidates_per_group: Vec<usize>,
    queries: usize,
}

impl Memo {
    /// The memo's size, to compare after a stage with
    /// [`Self::check_stage_only_added`].
    pub fn contents(&self) -> MemoContents {
        MemoContents {
            candidates_per_group: self.groups.iter().map(|g| g.candidates.len()).collect(),
            queries: self.queries.len(),
        }
    }

    /// Check 10: after `stage`, no group has fewer candidates than in
    /// `before`, no group is gone, and the query count is unchanged.
    pub fn check_stage_only_added(
        &self,
        before: &MemoContents,
        stage: &'static str,
    ) -> Result<(), InvariantViolation> {
        let violation = |group, detail| InvariantViolation {
            check: InvariantCheck::StageRemoved,
            group,
            origin: InvariantOrigin::Stage(stage),
            detail,
        };
        if self.queries.len() != before.queries {
            return Err(violation(
                None,
                format!(
                    "{} queries before, {} after",
                    before.queries,
                    self.queries.len()
                ),
            ));
        }
        if self.groups.len() < before.candidates_per_group.len() {
            return Err(violation(
                None,
                format!(
                    "{} groups before, {} after",
                    before.candidates_per_group.len(),
                    self.groups.len()
                ),
            ));
        }
        for (g, &count) in before.candidates_per_group.iter().enumerate() {
            let now = self.groups[g].candidates.len();
            if now < count {
                return Err(violation(
                    Some(MemoGroupId(g as u32)),
                    format!("{count} candidates before, {now} after"),
                ));
            }
        }
        Ok(())
    }

    /// Checks 5–9 after `stage`, and 13–14 when `thorough` (they re-derive
    /// every candidate's schema and coverage; `--check-invariants`).
    pub fn check_consistency(
        &self,
        stage: &'static str,
        thorough: bool,
    ) -> Result<(), InvariantViolation> {
        let at_stage = |check, group: usize, detail| InvariantViolation {
            check,
            group: Some(MemoGroupId(group as u32)),
            origin: InvariantOrigin::Stage(stage),
            detail,
        };
        let count = self.groups.len();
        // Every group read is one of the memo's: the walks below index by it.
        for (g, group) in self.groups.iter().enumerate() {
            let reads = group.expression.children().into_iter().chain(
                group
                    .candidates
                    .iter()
                    .flat_map(|c| c.expression.operator.children()),
            );
            for read in reads {
                if read.0 as usize >= count {
                    return Err(at_stage(
                        InvariantCheck::ReadsMissingGroup,
                        g,
                        format!("reads {read:?} of {count} groups"),
                    ));
                }
            }
        }
        // 5. The readers of each group, from expressions and candidates.
        let mut readers: Vec<BTreeSet<MemoGroupId>> = vec![BTreeSet::new(); count];
        for (g, group) in self.groups.iter().enumerate() {
            for read in group.expression.children().into_iter().chain(
                group
                    .candidates
                    .iter()
                    .flat_map(|c| c.expression.operator.children()),
            ) {
                readers[read.0 as usize].insert(MemoGroupId(g as u32));
            }
        }
        for (g, group) in self.groups.iter().enumerate() {
            let stored: BTreeSet<MemoGroupId> = group.parents.iter().copied().collect();
            if stored != readers[g] || stored.len() != group.parents.len() {
                return Err(at_stage(
                    InvariantCheck::UsedByList,
                    g,
                    format!("stored {:?}, read by {:?}", group.parents, readers[g]),
                ));
            }
        }
        // 6. The queries reaching each group, walked down from the roots.
        let mut reached: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
        for (query, memo_query) in self.queries.iter().enumerate() {
            let mut to_visit: Vec<MemoGroupId> = memo_query
                .root
                .operator_refs()
                .into_iter()
                .copied()
                .collect();
            while let Some(group) = to_visit.pop() {
                let Some(queries) = reached.get_mut(group.0 as usize) else {
                    return Err(InvariantViolation {
                        check: InvariantCheck::ReadsMissingGroup,
                        group: Some(group),
                        origin: InvariantOrigin::Stage(stage),
                        detail: format!("query {query}'s root reads it, of {count} groups"),
                    });
                };
                if queries.insert(query) {
                    let group = &self.groups[group.0 as usize];
                    to_visit.extend(
                        group.expression.children().into_iter().chain(
                            group
                                .candidates
                                .iter()
                                .flat_map(|c| c.expression.operator.children()),
                        ),
                    );
                }
            }
        }
        for (g, group) in self.groups.iter().enumerate() {
            if !group.queries.iter().copied().eq(reached[g].iter().copied()) {
                return Err(at_stage(
                    InvariantCheck::QueriesList,
                    g,
                    format!("stored {:?}, reached by {:?}", group.queries, reached[g]),
                ));
            }
            // 7.
            if reached[g].is_empty() {
                return Err(at_stage(
                    InvariantCheck::OrphanGroup,
                    g,
                    format!("no query reaches {}", group.expression.kind_name()),
                ));
            }
        }
        // 8. Shareable groups are unique by expression and listed by it.
        let mut by_expression: HashMap<String, usize> = HashMap::new();
        for (g, group) in self.groups.iter().enumerate() {
            if !group.group_properties.shareable_when_equal {
                continue;
            }
            let serialized = serialized_expression(&group.expression);
            if let Some(first) = by_expression.insert(serialized.clone(), g) {
                return Err(at_stage(
                    InvariantCheck::DuplicateGroup,
                    g,
                    format!("same expression as group {first}"),
                ));
            }
            let listed = self
                .groups_by_expression_hash
                .get(&super::hash_consing::expression_hash(&serialized))
                .is_some_and(|groups| {
                    groups
                        .iter()
                        .any(|(id, known)| id.0 as usize == g && *known == serialized)
                });
            if !listed {
                return Err(at_stage(
                    InvariantCheck::DuplicateGroup,
                    g,
                    "not found by its expression".to_string(),
                ));
            }
        }
        // 9, and 13–14 when thorough.
        for (g, group) in self.groups.iter().enumerate() {
            for (i, candidate) in group.candidates.iter().enumerate() {
                let by_rule = |check, detail| InvariantViolation {
                    check,
                    group: Some(MemoGroupId(g as u32)),
                    origin: InvariantOrigin::Rule(candidate.origin.clone()),
                    detail,
                };
                if let Some(j) = group.candidates[..i]
                    .iter()
                    .position(|earlier| same_candidate(earlier, candidate))
                {
                    return Err(by_rule(
                        InvariantCheck::DuplicateCandidate,
                        format!("candidates {j} and {i} are one"),
                    ));
                }
                if !thorough {
                    continue;
                }
                let operator =
                    candidate
                        .expression
                        .operator
                        .map_children(|&group_id| MemoGroupAsInput {
                            memo: self,
                            group_id,
                        });
                let properties = &group.group_properties;
                match operator.output_schema() {
                    Ok(schema)
                        if schema == properties.schema
                            && operator.output_kind() == properties.result_kind => {}
                    other => {
                        return Err(by_rule(
                            InvariantCheck::CandidateSchema,
                            format!("candidate {i} outputs {other:?}"),
                        ))
                    }
                }
                let coverage = aggregate_coverage_of(&operator);
                if coverage != properties.aggregate_coverage {
                    return Err(by_rule(
                        InvariantCheck::CandidateCoverage,
                        format!(
                            "candidate {i} covers {coverage:?}, group {:?}",
                            properties.aggregate_coverage
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Check 11 over Stage 2's output: each memo candidate a physical
    /// candidate (kept or pruned) points to exists.
    pub fn check_candidates_exist(
        &self,
        ids: impl IntoIterator<Item = CandidateForMemoGroupId>,
        stage: &'static str,
    ) -> Result<(), InvariantViolation> {
        for id in ids {
            let exists = self
                .groups
                .get(id.group.0 as usize)
                .is_some_and(|group| id.index_in_group < group.candidates.len());
            if !exists {
                return Err(InvariantViolation {
                    check: InvariantCheck::MissingLogicalCandidate,
                    group: Some(id.group),
                    origin: InvariantOrigin::Stage(stage),
                    detail: format!("no memo candidate {id:?}"),
                });
            }
        }
        Ok(())
    }

    /// Check 12 over Stage 3's plan: query `i`'s plan root `plan_roots[i]`
    /// has its memo root's shape, and each node in it outputs the schema of
    /// the memo root group in its place (`output_schema`; `None`: no such
    /// node).
    pub fn check_plan_output_schemas<'s, N: std::fmt::Debug>(
        &self,
        plan_roots: &[QueryRoot<N>],
        output_schema: impl Fn(&N) -> Option<&'s Schema>,
        stage: &'static str,
    ) -> Result<(), InvariantViolation> {
        let violation = |group, detail| InvariantViolation {
            check: InvariantCheck::PlanOutputSchema,
            group,
            origin: InvariantOrigin::Stage(stage),
            detail,
        };
        if plan_roots.len() != self.queries.len() {
            return Err(violation(
                None,
                format!(
                    "{} plan roots for {} queries",
                    plan_roots.len(),
                    self.queries.len()
                ),
            ));
        }
        for (query, (memo_query, plan_root)) in self.queries.iter().zip(plan_roots).enumerate() {
            let groups = memo_query.root.operator_refs();
            let nodes = plan_root.operator_refs();
            let same_shape = matches!(
                (&memo_query.root, plan_root),
                (QueryRoot::Operator(_), QueryRoot::Operator(_))
                    | (QueryRoot::Scalar(_), QueryRoot::Scalar(_))
            ) && groups.len() == nodes.len();
            if !same_shape {
                return Err(violation(
                    groups.first().copied().copied(),
                    format!("query {query}'s plan root has another shape"),
                ));
            }
            for (&group, node) in groups.into_iter().zip(nodes) {
                let schema = &self.groups[group.0 as usize].group_properties.schema;
                let output = output_schema(node);
                if output != Some(schema) {
                    return Err(violation(
                        Some(group),
                        format!(
                            "query {query}'s plan node {node:?} outputs {output:?}, group {schema:?}"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
