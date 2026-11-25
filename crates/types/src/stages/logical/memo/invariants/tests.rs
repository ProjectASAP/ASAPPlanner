//! One test per invariant: each builds a memo that breaks it and expects
//! the check's [`InvariantViolation`]. Checks 4a/4b are covered by the
//! memo's own tests (`add_candidate` rejecting an invalid candidate or θ).
//! Memos the API cannot break are broken by editing private fields.

use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::operator::AggIntent;
use crate::ir::schema::DataType;
use crate::stages::{CandidateError, CandidateForMemoGroup, MemoGroupError, ValueSign};
use crate::test_support::memo_builders::{group, query};
use crate::test_support::memo_fixtures::{aggregate, range, scan};
use crate::workload::DataWorkload;

/// G0 `Scan m`, G1 its last 5m, G2 the median per series over G1 (query
/// 0); each with its own expression as its candidate.
fn memo() -> (Memo, [MemoGroupId; 3]) {
    let mut memo = Memo::new(&DataWorkload::default());
    let g0 = group(&mut memo, scan("m", &[("job", DataType::Utf8)], vec![]));
    let g1 = group(&mut memo, range(300_000, g0));
    let g2 = group(&mut memo, median(g1));
    query(&mut memo, g2, None);
    (memo, [g0, g1, g2])
}

fn median(child: MemoGroupId) -> Operator<MemoGroupId> {
    aggregate(
        Reduction::PerEntity,
        AggIntent::Quantile { col: None, q: 0.5 },
        child,
    )
}

fn check_of(result: Result<(), InvariantViolation>) -> InvariantCheck {
    result.expect_err("the memo breaks an invariant").check
}

/// The memo the fixture builds passes every check.
#[test]
fn a_memo_built_through_the_api_passes_every_check() {
    let (memo, _) = memo();
    memo.check_consistency("test", true).unwrap();
}

/// Check 1: Properties with one sign more than the group's columns fail.
#[test]
fn a_group_with_more_signs_than_columns_fails() {
    let (memo, [g0, ..]) = memo();
    let group = &memo.groups[g0.0 as usize];
    let mut properties = group.group_properties.clone();
    properties.column_signs.push(ValueSign::Unknown);
    let violation = check_column_facts(&properties, &group.expression).unwrap_err();
    assert_eq!(violation.check, InvariantCheck::ColumnFactsPerColumn);
}

/// Check 2: A group or candidate reading a group the memo does not have fails,
/// instead of panicking.
#[test]
fn reading_a_missing_group_fails() {
    let (mut memo, [_, _, g2]) = memo();
    let missing = MemoGroupId(9);
    assert!(matches!(
        memo.find_or_add_group(range(60_000, missing)),
        Err(MemoGroupError::Invariant(InvariantViolation {
            check: InvariantCheck::ReadsMissingGroup,
            ..
        }))
    ));
    let candidate = CandidateForMemoGroup::new(
        median(missing),
        None,
        OriginOfCandidateForMemoGroup::Rollup,
        Vec::new(),
    );
    let error = memo.add_candidate(g2, candidate).unwrap_err();
    let violation = error.invariant_violation().unwrap();
    assert_eq!(violation.check, InvariantCheck::ReadsMissingGroup);
    assert_eq!(
        violation.origin,
        InvariantOrigin::Rule(OriginOfCandidateForMemoGroup::Rollup)
    );
}

/// Check 3: A candidate without one input requirement per input fails.
#[test]
fn a_candidate_without_one_requirement_per_input_fails() {
    let (mut memo, [_, g1, g2]) = memo();
    let mut candidate = CandidateForMemoGroup::new(
        median(g1),
        None,
        OriginOfCandidateForMemoGroup::Pass1,
        Vec::new(),
    );
    candidate.input_requirements.clear();
    assert!(matches!(
        memo.add_candidate(g2, candidate),
        Err(CandidateError::Invariant(InvariantViolation {
            check: InvariantCheck::InputRequirementsPerInput,
            ..
        }))
    ));
}

/// Check 5: A group whose "used by" list misses a group reading it fails.
#[test]
fn a_used_by_list_missing_a_reader_fails() {
    let (mut memo, [g0, ..]) = memo();
    memo.groups[g0.0 as usize].parents.clear();
    assert_eq!(
        check_of(memo.check_consistency("test", false)),
        InvariantCheck::UsedByList
    );
}

/// Check 6: A group listing a query that does not reach it fails.
#[test]
fn a_queries_list_naming_an_unreaching_query_fails() {
    let (mut memo, [g0, ..]) = memo();
    memo.groups[g0.0 as usize].queries.push(7);
    assert_eq!(
        check_of(memo.check_consistency("test", false)),
        InvariantCheck::QueriesList
    );
}

/// Check 7: A group no query reaches fails: a rule added it and then nothing
/// read it.
#[test]
fn a_group_no_query_reaches_fails() {
    let (mut memo, [_, g1, _]) = memo();
    let orphan = group(
        &mut memo,
        aggregate(
            Reduction::PerEntity,
            AggIntent::Quantile { col: None, q: 0.9 },
            g1,
        ),
    );
    let violation = memo.check_consistency("stage1_pass2", false).unwrap_err();
    assert_eq!(violation.check, InvariantCheck::OrphanGroup);
    assert_eq!(violation.group, Some(orphan));
    assert_eq!(violation.origin, InvariantOrigin::Stage("stage1_pass2"));
}

/// Check 8: Two groups with the same expression fail.
#[test]
fn two_groups_with_one_expression_fail() {
    let (mut memo, [g0, ..]) = memo();
    let mut copy = memo.groups[g0.0 as usize].clone();
    copy.parents.clear();
    copy.queries.clear();
    let copy_id = MemoGroupId(memo.groups.len() as u32);
    memo.groups.push(copy);
    query(&mut memo, copy_id, None);
    let violation = memo.check_consistency("test", false).unwrap_err();
    assert_eq!(violation.check, InvariantCheck::DuplicateGroup);
    assert_eq!(violation.group, Some(copy_id));
}

/// Check 9: A group holding the same candidate twice fails, naming its rule.
#[test]
fn a_candidate_held_twice_fails() {
    let (mut memo, [_, _, g2]) = memo();
    let candidates = &mut memo.groups[g2.0 as usize].candidates;
    candidates.push(candidates[0].clone());
    let violation = memo.check_consistency("test", false).unwrap_err();
    assert_eq!(violation.check, InvariantCheck::DuplicateCandidate);
    assert_eq!(
        violation.origin,
        InvariantOrigin::Rule(OriginOfCandidateForMemoGroup::Pass1)
    );
}

/// Check 10: A stage that drops a candidate, a group or a query fails.
#[test]
fn a_stage_that_removes_fails() {
    let (mut memo, [_, _, g2]) = memo();
    let before = memo.contents();
    memo.groups[g2.0 as usize].candidates.pop();
    assert_eq!(
        check_of(memo.check_stage_only_added(&before, "test")),
        InvariantCheck::StageRemoved
    );
    let empty = Memo::new(&DataWorkload::default());
    assert_eq!(
        check_of(empty.check_stage_only_added(&before, "test")),
        InvariantCheck::StageRemoved
    );
}

/// Check 11: A physical candidate pointing past its group's candidates fails.
#[test]
fn a_physical_candidate_of_a_missing_memo_candidate_fails() {
    let (memo, [_, _, g2]) = memo();
    let id = |index_in_group| CandidateForMemoGroupId {
        group: g2,
        index_in_group,
    };
    memo.check_candidates_exist([id(0)], "stage2").unwrap();
    assert_eq!(
        check_of(memo.check_candidates_exist([id(1)], "stage2")),
        InvariantCheck::MissingLogicalCandidate
    );
}

/// Check 12: A plan root whose output schema differs from its query's Stage 0
/// group fails.
#[test]
fn a_plan_root_with_another_schema_fails() {
    let (memo, [_, _, g2]) = memo();
    let own = memo.groups[g2.0 as usize].group_properties.schema.clone();
    let other = crate::ir::schema::Schema::with_time_index(
        vec![crate::ir::schema::Field::plain(
            "ts",
            DataType::Timestamp,
            false,
        )],
        0,
        vec![],
    );
    let plan_root = [QueryRoot::Operator("n0")];
    memo.check_plan_output_schemas(&plan_root, |_| Some(&own), "stage3")
        .unwrap();
    assert_eq!(
        check_of(memo.check_plan_output_schemas(&plan_root, |_| Some(&other), "stage3")),
        InvariantCheck::PlanOutputSchema
    );
}

/// Check 13: A candidate whose output schema differs from its group's fails,
/// only in the thorough check.
#[test]
fn a_candidate_with_another_schema_fails_when_thorough() {
    let (mut memo, [g0, g1, _]) = memo();
    // G1's candidate still reads G0, but keeps only its first column.
    memo.groups[g1.0 as usize].candidates[0].expression.operator = first_column_of(g0);
    memo.check_consistency("test", false).unwrap();
    assert_eq!(
        check_of(memo.check_consistency("test", true)),
        InvariantCheck::CandidateSchema
    );
}

/// `Project(child)` keeping its first column.
fn first_column_of(child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(crate::ir::NonASAPOp::Project {
        cols: vec![crate::ir::ProjectItem {
            alias: None,
            expr: crate::ir::ScalarExpr::Column(0),
        }],
        child,
    })
}

/// Check 14: A candidate covering other rows than its group fails, only in the
/// thorough check.
#[test]
fn a_candidate_with_another_coverage_fails_when_thorough() {
    let (mut memo, [_, _, g2]) = memo();
    assert!(memo.groups[g2.0 as usize]
        .group_properties
        .aggregate_coverage
        .is_some());
    memo.groups[g2.0 as usize]
        .group_properties
        .aggregate_coverage = None;
    memo.check_consistency("test", false).unwrap();
    assert_eq!(
        check_of(memo.check_consistency("test", true)),
        InvariantCheck::CandidateCoverage
    );
}
