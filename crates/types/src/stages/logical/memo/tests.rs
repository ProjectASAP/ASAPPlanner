//! The `Memo` methods: hash-consing groups ([`Memo::find_or_add_group`]) and
//! storing candidates ([`Memo::add_candidate`]), on the concepts doc's KLL
//! example (section 1: G1 the latency scan, G2 its KLL state).

use super::*;
use crate::ir::operator::arguments::{GroupKeys, Reduction, Source};
use crate::ir::scalar::expr_ir::ColumnRef;
use crate::ir::schema::{DataType, Field, FieldDataType, Schema};
use crate::ir::ASAPOp;
use crate::ir::NonASAPOp;
use crate::stages::{
    CandidateExpression, LogicalProperties, MemoGroupAsInput, OriginOfCandidateForMemoGroup,
};
use crate::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic, SummaryUpdate};

/// `Scan metric[1m]`: `[ts, value]`.
fn scan(metric: &str) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    })
}

/// `SummaryAgg[KLL](input)`: no θ in the expression.
fn kll_state(input: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child: input,
        summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    })
}

/// `SummaryEstimate[q](state)`.
fn quantile_readout(state: MemoGroupId, q: f64) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query: SketchStatistic::Quantile { q },
    })
}

/// An unordered `LIMIT 10` over `input`, whose copies may return
/// different rows.
fn unordered_limit(input: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Limit {
        n: Some(10),
        offset: 0,
        partition_by: GroupKeys::default(),
        child: input,
    })
}

/// `Project(input)` keeping its first column.
fn project_first_column(input: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Project {
        cols: vec![crate::ir::ProjectItem {
            alias: None,
            expr: crate::ir::ScalarExpr::Column(0),
        }],
        qualifier: None,
        child: input,
    })
}

/// A candidate computing `operator` with `parameters`.
fn candidate(
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
) -> CandidateForMemoGroup {
    let input_requirements = vec![LogicalRequirements::default(); operator.children().len()];
    CandidateForMemoGroup {
        // Built directly: the doc example's k = 200 and k = 345 are not
        // in today's KLL list, which `LogicalProperties::new` checks.
        logical_properties: LogicalProperties { parameters },
        expression: CandidateExpression { operator },
        input_requirements,
        fused_groups: vec![],
        origin: OriginOfCandidateForMemoGroup::Pass1,
    }
}

/// A query rooted at `group`, without an accuracy target or $n_q$.
fn query(group: MemoGroupId) -> MemoQuery {
    MemoQuery {
        root: QueryRoot::Operator(group),
        accuracy_target: None,
        number_of_values_per_query_evaluation: None,
    }
}

/// KLL `k`.
fn kll_k(k: u32) -> Option<SketchParams> {
    Some(SketchParams::Kll { k })
}

/// An equal expression over the same input groups is found, not added.
#[test]
fn find_or_add_group_returns_the_group_of_an_equal_expression() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
    assert_eq!(memo.find_or_add_group(scan("latency")).unwrap(), g1);
    assert_eq!(memo.find_or_add_group(kll_state(g1)).unwrap(), g2);
    assert_ne!(memo.find_or_add_group(scan("errors")).unwrap(), g1);
    assert_eq!(memo.groups().len(), 3);
    assert_eq!(
        memo.groups()[g2.0 as usize].expression.children(),
        vec![&g1]
    );
}

/// The same operator over different input groups is a different group.
#[test]
fn find_or_add_group_separates_equal_operators_over_different_inputs() {
    let mut memo = Memo::default();
    let latency = memo.find_or_add_group(scan("latency")).unwrap();
    let errors = memo.find_or_add_group(scan("errors")).unwrap();
    assert_ne!(
        memo.find_or_add_group(kll_state(latency)).unwrap(),
        memo.find_or_add_group(kll_state(errors)).unwrap()
    );
}

/// An expression whose copies may return different rows (an unordered
/// `LIMIT`) gets a new group every time.
#[test]
fn find_or_add_group_never_returns_an_unshareable_group() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    assert_ne!(
        memo.find_or_add_group(unordered_limit(g1)).unwrap(),
        memo.find_or_add_group(unordered_limit(g1)).unwrap()
    );
}

/// A new group is a parent of each of its input groups, listed once.
#[test]
fn find_or_add_group_lists_the_new_group_among_its_inputs_parents() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
    memo.find_or_add_group(kll_state(g1)).unwrap();
    assert_eq!(memo.groups()[g1.0 as usize].parent_groups(), &[g2]);
    assert!(memo.groups()[g2.0 as usize].parent_groups().is_empty());
}

/// KLL builds with k = 200 and k = 345 are two candidates of one group
/// G2: θ is a logical property, not part of the expression.
#[test]
fn kll_builds_with_k_200_and_k_345_share_one_group() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let k200_group = memo.find_or_add_group(kll_state(g1)).unwrap();
    let k200 = memo
        .add_candidate(k200_group, candidate(kll_state(g1), kll_k(200)))
        .unwrap();
    let k345_group = memo.find_or_add_group(kll_state(g1)).unwrap();
    let k345 = memo
        .add_candidate(k345_group, candidate(kll_state(g1), kll_k(345)))
        .unwrap();
    assert_eq!(k200_group, k345_group);
    assert_ne!(k200, k345);
    assert_eq!(memo.groups()[k200_group.0 as usize].candidates.len(), 2);
    assert_eq!(
        memo.candidate(k345).logical_properties.parameters(),
        kll_k(345).as_ref()
    );
}

/// A candidate with the same expression and logical properties as one
/// in the group is not added again; its id is returned.
#[test]
fn add_candidate_returns_the_existing_candidate_for_a_duplicate() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
    let first = memo
        .add_candidate(g2, candidate(kll_state(g1), kll_k(200)))
        .unwrap();
    let again = memo
        .add_candidate(g2, candidate(kll_state(g1), kll_k(200)))
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(
        first,
        CandidateForMemoGroupId {
            group: g2,
            index_in_group: 0
        }
    );
    let other_size = candidate(kll_state(g1), kll_k(345));
    assert_ne!(memo.add_candidate(g2, other_size).unwrap(), first);
    assert_eq!(memo.groups()[g2.0 as usize].candidates.len(), 2);
}

/// A new candidate makes its group a parent of each group it reads, and
/// the group's queries reach those groups and every group below them.
#[test]
fn add_candidate_maintains_parents_and_queries() {
    // Query 0 is G3 = SummaryEstimate[q=0.95](G2) over G2 = KLL(G1).
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
    let g3 = memo.find_or_add_group(quantile_readout(g2, 0.95)).unwrap();
    assert_eq!(memo.add_query(query(g3)), 0);
    // A rule adds a state group G5 over another scan G4: no query
    // reaches them yet.
    let g4 = memo.find_or_add_group(scan("latency_copy")).unwrap();
    let g5 = memo.find_or_add_group(kll_state(g4)).unwrap();
    assert!(memo.groups()[g5.0 as usize].reached_by_queries().is_empty());
    // G3 gains a candidate reading G5: G5 and G4 now serve query 0.
    memo.add_candidate(g3, candidate(quantile_readout(g5, 0.95), None))
        .unwrap();
    assert_eq!(memo.groups()[g5.0 as usize].parent_groups(), &[g3]);
    assert_eq!(memo.groups()[g4.0 as usize].parent_groups(), &[g5]);
    assert_eq!(memo.groups()[g5.0 as usize].reached_by_queries(), &[0]);
    assert_eq!(memo.groups()[g4.0 as usize].reached_by_queries(), &[0]);
    // Query 1 is G6 = Project(G3): it reaches G3 and everything below
    // it, through expressions and candidates, in ascending order.
    let g6 = memo.find_or_add_group(project_first_column(g3)).unwrap();
    assert_eq!(memo.add_query(query(g6)), 1);
    for group in [g1, g2, g3, g4, g5] {
        assert_eq!(
            memo.groups()[group.0 as usize].reached_by_queries(),
            &[0, 1],
            "group {group:?}"
        );
    }
    assert_eq!(memo.groups()[g3.0 as usize].parent_groups(), &[g6]);
}

/// A candidate that computes another result than its group's (a
/// readout offered for a state group) is rejected and changes nothing.
#[test]
fn add_candidate_rejects_a_candidate_whose_output_differs_from_its_group() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
    let before = format!("{memo:?}");
    assert!(matches!(
        memo.add_candidate(g2, candidate(quantile_readout(g2, 0.5), None)),
        Err(CandidateError::OutputDiffersFromGroup { group }) if group == g2
    ));
    assert_eq!(
        format!("{memo:?}"),
        before,
        "a rejected candidate changes nothing"
    );
}

/// A candidate whose expression fails input validation (a readout of
/// raw rows instead of summary state) is rejected and changes nothing.
#[test]
fn add_candidate_rejects_a_candidate_whose_inputs_fail_validation() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
    let g3 = memo.find_or_add_group(quantile_readout(g2, 0.95)).unwrap();
    let over_raw_rows = quantile_readout(g1, 0.95).map_children(|&group_id| MemoGroupAsInput {
        memo: &memo,
        group_id,
    });
    assert!(
        over_raw_rows.output_schema().is_ok(),
        "fixture: the schema derives; only input validation rejects it"
    );
    let before = format!("{memo:?}");
    assert!(matches!(
        memo.add_candidate(g3, candidate(quantile_readout(g1, 0.95), None)),
        Err(CandidateError::Schema(_))
    ));
    assert_eq!(
        format!("{memo:?}"),
        before,
        "a rejected candidate changes nothing"
    );
}

/// An expression with no output schema (a group-by column out of range)
/// is an error, and no group is added.
#[test]
fn find_or_add_group_returns_an_error_for_an_expression_with_no_schema() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan("latency")).unwrap();
    let out_of_range = Operator::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::by(vec![9]),
        measures: vec![crate::ir::operator::agg_intent::AggIntent::Sum { col: Some(1) }],
        output_names: vec![],
        filters: vec![],
        having: None,
        child: g1,
    });
    assert!(memo.find_or_add_group(out_of_range).is_err());
    assert_eq!(memo.groups().len(), 1);
    assert!(memo.groups()[g1.0 as usize].parent_groups().is_empty());
}
