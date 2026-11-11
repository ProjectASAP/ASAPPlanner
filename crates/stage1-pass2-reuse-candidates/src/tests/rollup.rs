//! Step 1: rollup from the finest common group.

use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{ExactKind, SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_fixtures::state_grouped_by;

/// Concepts doc Example 1: F = `quantile by (job, host)(latency[5m])`, C =
/// `quantile by (host)(latency{job="api"}[5m])`. Equal definitions, the
/// narrowing `job ∈ {api}` on a key of F, {host} ⊆ {job, host}: C's state
/// gets `SummaryMerge(by host)(Filter(job = "api", F))`, and F is the
/// finest common group (nothing is synthesized).
#[test]
fn a_filtered_coarse_sketch_state_merges_the_finer_states_through_a_compensating_filter() {
    let mut memo = Memo::new(&DataWorkload::default());
    let labels = ["job", "host"];
    let all = scan(&mut memo, "latency", &labels);
    let api = scan_where(
        &mut memo,
        "latency",
        &labels,
        vec![Predicate(equals(2, "api"))],
    );
    let fine_window = range(&mut memo, all, 300);
    let coarse_window = range(&mut memo, api, 300);
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
            pred: Predicate(equals(0, "api")),
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

/// Concepts doc Example 3: Q1 = `quantile by (host)(latency[5m])`, Q2 =
/// `quantile by (job)(latency{host="xxx"}[5m])`. Neither is finer, so the
/// finest common group G* = `SummaryAgg[KLL, by job, host]` over Q1's rows
/// is synthesized with one build per listed θ, Q1's state merges it by
/// host and Q2's merges `Filter(host = "xxx", G*)` by job.
#[test]
fn a_finest_common_group_no_query_asks_for_serves_both() {
    let mut memo = Memo::new(&DataWorkload::default());
    let labels = ["job", "host"];
    let all = scan(&mut memo, "latency", &labels);
    let xxx = scan_where(
        &mut memo,
        "latency",
        &labels,
        vec![Predicate(equals(3, "xxx"))],
    );
    let all_window = range(&mut memo, all, 300);
    let xxx_window = range(&mut memo, xxx, 300);
    let by_host = sketch_state(
        &mut memo,
        all_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let by_job = sketch_state(
        &mut memo,
        xxx_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2]),
    );
    for state in [by_host, by_job] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.99 });
        query(&mut memo, readout, None);
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let memo = &logical.memo;
    let finest = group_of(
        memo,
        &state_grouped_by(
            FieldDataType::Sketch(SketchAlgorithm::Kll),
            Reduction::by(vec![2, 3]),
            all_window,
        ),
    )
    .expect("G*: KLL by job, host over Q1's rows");
    assert_eq!(
        candidates_of(memo, finest)
            .iter()
            .filter(|c| c.expression.operator == memo.groups()[finest.0 as usize].expression)
            .count(),
        parameter_list(SketchAlgorithm::Kll).len()
    );
    let merges = |state: MemoGroupId| -> Vec<Operator<MemoGroupId>> {
        candidates_of(memo, state)
            .iter()
            .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
            .map(|c| c.expression.operator.clone())
            .collect()
    };
    let listed = parameter_list(SketchAlgorithm::Kll).len();
    // One merge per θ of G*'s builds.
    assert_eq!(
        merges(by_host),
        vec![
            Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![finest],
                group_by: Some(vec![1]),
            });
            listed
        ]
    );
    let filter = group_of(
        memo,
        &Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(equals(1, "xxx")),
            child: finest,
        }),
    )
    .expect("Filter(host = \"xxx\", G*)");
    assert_eq!(candidates_of(memo, filter).len(), listed);
    assert_eq!(
        merges(by_job),
        vec![
            Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![filter],
                group_by: Some(vec![0]),
            });
            listed
        ]
    );
}

/// A `quantile by (job)` over `status = "500"` rows does not roll up from
/// `quantile by (job, host)` over every row: `status` is no key of the
/// finer state, whose states mix both statuses.
#[test]
fn a_narrowing_on_a_column_that_is_no_key_gives_no_rollup() {
    let mut memo = Memo::new(&DataWorkload::default());
    let labels = ["job", "host", "status"];
    let all = scan(&mut memo, "latency", &labels);
    let errors = scan_where(
        &mut memo,
        "latency",
        &labels,
        vec![Predicate(equals(4, "500"))],
    );
    let all_window = range(&mut memo, all, 300);
    let errors_window = range(&mut memo, errors, 300);
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

/// Concepts doc Example 2, exact values: `sum by (job)` rolls up the
/// finalized `sum by (job, instance)` of the same per-series rows with a
/// sum over its sum column.
#[test]
fn a_coarse_exact_sum_rolls_up_the_finer_sum() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "x", &["job", "instance"]);
    let window = range(&mut memo, input, 60);
    let rate = group(
        &mut memo,
        state_grouped_by(
            FieldDataType::exact(ExactKind::Rate),
            Reduction::PerEntity,
            window,
        ),
    );
    let rows = group(
        &mut memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: rate }),
    );
    let sum_by = |memo: &mut Memo, keys: Vec<usize>, name: &str| {
        group(
            memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: rows,
                reduction: Reduction::by(keys),
                measures: vec![AggIntent::Sum { col: None }],
                output_names: vec![name.into()],
                filters: vec![None],
                having: None,
            }),
        )
    };
    let fine = sum_by(&mut memo, vec![2, 3], "value");
    let coarse = sum_by(&mut memo, vec![2], "value");
    query(&mut memo, fine, Some(AccuracyTarget::exact()));
    query(&mut memo, coarse, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let rollups: Vec<_> = candidates_of(&logical.memo, coarse)
        .iter()
        .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .map(|c| c.expression.operator.clone())
        .collect();
    assert_eq!(
        rollups,
        vec![Operator::NonASAP(NonASAPOp::Aggregate {
            child: fine,
            reduction: Reduction::by(vec![0]),
            measures: vec![AggIntent::Sum { col: Some(2) }],
            output_names: vec!["value".into()],
            filters: vec![None],
            having: None,
        })]
    );
}

/// A scan of the SQL table `flows(src, dst, bytes)` with `predicates`.
fn flows(memo: &mut Memo, predicates: Vec<Predicate<MemoGroupId>>) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "flows".into(),
            },
            predicates,
            schema: Schema::new(vec![
                Field::plain("src", DataType::Utf8, false),
                Field::plain("dst", DataType::Utf8, false),
                Field::plain("bytes", DataType::Float64, false),
            ]),
        }),
    )
}

/// `column = value`, SQL semantics.
fn sql_equals(column: usize, value: &str) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.into()))),
        semantics: ExprSemantics::Sql,
    }
}

/// A compensating filter compares with the semantics of the frontend of
/// the source it reads: `SUM(bytes) WHERE dst = 'x' GROUP BY src` over a
/// SQL table rolls up the exact sum state by (src, dst) through
/// `Filter(dst = 'x')` with SQL semantics, not PromQL's.
#[test]
fn a_compensating_filter_over_a_sql_table_has_sql_semantics() {
    let mut memo = Memo::new(&DataWorkload::default());
    let all = flows(&mut memo, vec![]);
    let x = flows(&mut memo, vec![Predicate(sql_equals(1, "x"))]);
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
            pred: Predicate(sql_equals(1, "x")),
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
    let a = flows(&mut memo, vec![Predicate(sql_equals(0, "a"))]);
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
