//! Compensating filters and matchers: a coarse group over fewer rows
//! rolls up a finer one through a `Filter` on its keys.
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::FieldDataType;

use asap_types::ir::{ASAPOp, ExprSemantics, NonASAPOp, Operator, Predicate};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use asap_types::summary_type::{ExactKind, SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::aggregate;
use asap_types::test_support::memo_fixtures::{compare, range};

use super::{flows, instant};

/// Concepts doc Example 1: F = `quantile by (job, host)(latency[5m])`, C =
/// `quantile by (host)(latency{job="api"}[5m])`. Equal definitions, the
/// narrowing `job ∈ {api}` on a key of F, {host} ⊆ {job, host}: C's state
/// gets `SummaryMerge(by host)(Filter(job = "api", F))`, and F is the
/// finest common group (nothing is synthesized).
#[test]
fn a_filtered_coarse_sketch_state_merges_the_finer_states_through_a_compensating_filter() {
    let mut memo = Memo::new(&DataWorkload::default());
    let labels = ["job", "host"];
    let all = scan_group(&mut memo, "latency", &labels, vec![]);
    let api = scan_group(
        &mut memo,
        "latency",
        &labels,
        vec![compare(
            2,
            CompareOpKind::Eq,
            ScalarValue::Utf8("api".into()),
            ExprSemantics::Promql,
        )],
    );
    let fine_window = group(&mut memo, range(300_000, all));
    let coarse_window = group(&mut memo, range(300_000, api));
    let fine = sketch_state(
        &mut memo,
        fine_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2, 3]),
    );
    let coarse = sketch_state(
        &mut memo,
        coarse_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let groups_before = memo.groups().len();
    for state in [fine, coarse] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.99 });
        query(&mut memo, readout, None);
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let memo = &logical.memo;
    let filter = group_of(
        memo,
        &Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(compare(
                0,
                CompareOpKind::Eq,
                ScalarValue::Utf8("api".into()),
                ExprSemantics::Promql,
            )),
            child: fine,
        }),
    )
    .expect("Filter(job = \"api\", F)");
    assert!(candidates_of(memo, coarse).iter().any(|c| c.origin
        == OriginOfCandidateForMemoGroup::Rollup
        && c.expression.operator
            == Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![filter],
                group_by: Some(vec![1]),
            })));
    // F reads nothing from C, and no finest common group was added: the
    // memo grew by the readouts, the filter, and the chains only.
    assert!(candidates_of(memo, fine)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::Rollup));
    assert!(groups_where(memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryAgg { .. })
    ))
    .iter()
    .all(|&g| g == fine || g == coarse || g.0 as usize >= groups_before));
}

/// A `quantile by (job)` over `status = "500"` rows does not roll up from
/// `quantile by (job, host)` over every row: `status` is no key of the
/// finer state, whose states mix both statuses.
#[test]
fn a_narrowing_on_a_column_that_is_no_key_gives_no_rollup() {
    let mut memo = Memo::new(&DataWorkload::default());
    let labels = ["job", "host", "status"];
    let all = scan_group(&mut memo, "latency", &labels, vec![]);
    let errors = scan_group(
        &mut memo,
        "latency",
        &labels,
        vec![compare(
            4,
            CompareOpKind::Eq,
            ScalarValue::Utf8("500".into()),
            ExprSemantics::Promql,
        )],
    );
    let all_window = group(&mut memo, range(300_000, all));
    let errors_window = group(&mut memo, range(300_000, errors));
    let fine = sketch_state(
        &mut memo,
        all_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2, 3]),
    );
    let coarse = sketch_state(
        &mut memo,
        errors_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2]),
    );
    for state in [fine, coarse] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.99 });
        query(&mut memo, readout, None);
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    assert!(candidates_of(&logical.memo, coarse).iter().all(|c| !c
        .expression
        .operator
        .children()
        .contains(&&fine)));
}

/// A PromQL `!=` matcher compensates with PromQL's comparison:
/// `sum by (job)` over `x{instance!="i1"}` rolls up `sum by (job,
/// instance)` over `x` through `Filter(instance != "i1")` with PromQL
/// semantics (a series without the label matches), not a negated SQL
/// `IN` list.
#[test]
fn a_not_equal_matcher_compensates_with_promql_semantics() {
    let mut memo = Memo::new(&DataWorkload::default());
    let not_equal = |column: usize| {
        compare(
            column,
            CompareOpKind::Ne,
            ScalarValue::Utf8("i1".into()),
            ExprSemantics::Promql,
        )
    };
    let all = scan_group(&mut memo, "x", &["job", "instance"], vec![]);
    let not_i1 = scan_group(&mut memo, "x", &["job", "instance"], vec![not_equal(3)]);
    let all_rows = instant(&mut memo, all);
    let not_i1_rows = instant(&mut memo, not_i1);
    let fine = group(
        &mut memo,
        aggregate(
            Reduction::by(vec![2, 3]),
            AggIntent::Sum { col: None },
            all_rows,
        ),
    );
    let coarse = group(
        &mut memo,
        aggregate(
            Reduction::by(vec![2]),
            AggIntent::Sum { col: None },
            not_i1_rows,
        ),
    );
    query(&mut memo, fine, Some(AccuracyTarget::exact()));
    query(&mut memo, coarse, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let filters = groups_where(
        &logical.memo,
        |e| matches!(e, Operator::NonASAP(NonASAPOp::Filter { child, .. }) if *child == fine),
    );
    let [filter] = filters[..] else {
        panic!("one compensating filter: {filters:?}");
    };
    assert_eq!(
        logical.memo.groups()[filter.0 as usize].expression,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(not_equal(1)),
            child: fine,
        })
    );
}

/// A compensating filter compares with the semantics of the frontend of
/// the source it reads: `SUM(bytes) WHERE dst = 'x' GROUP BY src` over a
/// SQL table rolls up the exact sum state by (src, dst) through
/// `Filter(dst = 'x')` with SQL semantics, not PromQL's.
#[test]
fn a_compensating_filter_over_a_sql_table_has_sql_semantics() {
    let mut memo = Memo::new(&DataWorkload::default());
    let all = flows(&mut memo, vec![]);
    let x = flows(
        &mut memo,
        vec![Predicate(compare(
            1,
            CompareOpKind::Eq,
            ScalarValue::Utf8("x".into()),
            ExprSemantics::Sql,
        ))],
    );
    let sum_state = |memo: &mut Memo, rows: MemoGroupId, keys: Vec<usize>| {
        group(
            memo,
            Operator::ASAP(ASAPOp::SummaryAgg {
                child: rows,
                summary_type: FieldDataType::exact(ExactKind::Sum),
                input: SummaryUpdate::column(ColumnRef::Named("bytes".into())),
                reduction: Reduction::by(keys),
                filter: None,
            }),
        )
    };
    let fine = sum_state(&mut memo, all, vec![0, 1]);
    let coarse = sum_state(&mut memo, x, vec![0]);
    for state in [fine, coarse] {
        let root = group(
            &mut memo,
            Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state }),
        );
        query(&mut memo, root, Some(AccuracyTarget::exact()));
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let filters = groups_where(
        &logical.memo,
        |e| matches!(e, Operator::NonASAP(NonASAPOp::Filter { child, .. }) if *child == fine),
    );
    let [filter] = filters[..] else {
        panic!("one compensating filter: {filters:?}");
    };
    assert_eq!(
        logical.memo.groups()[filter.0 as usize].expression,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(compare(
                1,
                CompareOpKind::Eq,
                ScalarValue::Utf8("x".into()),
                ExprSemantics::Sql
            )),
            child: fine,
        })
    );
}

/// A rollup the memo rejects adds nothing: `COUNT(*) WHERE src = 'a'` (one
/// row, 0 when no row matches) does not roll up from `COUNT(*) GROUP BY
/// src` (a sum of counts, NULL over no row), and no compensating
/// `Filter(src = 'a')` group is left without a reader.
#[test]
fn a_rejected_rollup_leaves_no_compensating_filter() {
    let mut memo = Memo::new(&DataWorkload::default());
    let all = flows(&mut memo, vec![]);
    let a = flows(
        &mut memo,
        vec![Predicate(compare(
            0,
            CompareOpKind::Eq,
            ScalarValue::Utf8("a".into()),
            ExprSemantics::Sql,
        ))],
    );
    let count_by = |memo: &mut Memo, rows: MemoGroupId, keys: Vec<usize>| {
        group(
            memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: rows,
                reduction: Reduction::by(keys),
                measures: vec![AggIntent::Count],
                output_names: vec!["count".into()],
                filters: vec![None],
                having: None,
            }),
        )
    };
    let by_src = count_by(&mut memo, all, vec![0]);
    let only_a = count_by(&mut memo, a, vec![]);
    query(&mut memo, by_src, Some(AccuracyTarget::exact()));
    query(&mut memo, only_a, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let memo = &logical.memo;
    assert!(candidates_of(memo, only_a)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::Rollup));
    let roots = memo.root_groups();
    let orphans: Vec<MemoGroupId> = (0..memo.groups().len())
        .map(|index| MemoGroupId(index as u32))
        .filter(|g| !roots.contains(g) && memo.groups()[g.0 as usize].parent_groups().is_empty())
        .collect();
    assert!(orphans.is_empty(), "groups without a reader: {orphans:?}");
}
