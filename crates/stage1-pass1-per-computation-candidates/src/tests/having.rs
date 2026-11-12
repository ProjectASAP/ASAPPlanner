use super::{memo_printout, quantile, selector};
use crate::candidates::key_only_having;
use crate::pass1_memo;
use crate::test_fixtures::aggregate_group;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
use asap_types::ir::{ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::test_support::memo_builders::{candidates_of, group, groups_where, query};
use asap_types::workload::DataWorkload;

/// `column op literal` over an aggregate's output.
fn compare(column: usize, op: CompareOpKind, value: ScalarValue) -> Predicate<MemoGroupId> {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op,
        right: Box::new(ScalarExpr::Literal(value)),
        semantics: ExprSemantics::Promql,
    })
}

/// `Filter(pred)` over `child`, added as Stage 0 adds a consumer `Filter`.
fn filter(memo: &mut Memo, child: MemoGroupId, pred: Predicate<MemoGroupId>) -> MemoGroupId {
    group(memo, Operator::NonASAP(NonASAPOp::Filter { pred, child }))
}

/// The aggregate's input with a key-only predicate pushed below it: a
/// `Filter` group over `rows`.
fn pushed_below(memo: &Memo, rows: MemoGroupId) -> Vec<MemoGroupId> {
    groups_where(
        memo,
        |e| matches!(e, Operator::NonASAP(NonASAPOp::Filter { child, .. }) if *child == rows),
    )
}

/// A key-only `HAVING` (a consumer `Filter` over the group keys) gets
/// candidates in the `Filter`'s group that push it below the aggregate: a
/// `Filter` on the input rows, which the states read. The aggregate's own
/// group keeps its unfiltered candidates for its other consumers.
#[test]
fn key_only_having_is_pushed_below_into_the_filter_group() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        by_job,
        compare(0, CompareOpKind::Eq, ScalarValue::Utf8("a".into())),
    );
    query(&mut memo, having, None);
    query(&mut memo, by_job, None);
    let memo = pass1_memo(memo, None).unwrap();
    let below = pushed_below(&memo, rows);
    assert_eq!(below.len(), 1, "{}", memo_printout(&memo));
    // The Filter group: its own expression, then one readout per algorithm
    // over states built from the filtered rows.
    assert_eq!(candidates_of(&memo, having).len(), 3);
    assert_eq!(candidates_of(&memo, by_job).len(), 3);
}

/// A `HAVING` over an aggregate value stays above the output: nothing is
/// pushed below, and the `Filter` group keeps its own expression only.
#[test]
fn value_having_stays_above() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        by_job,
        compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0)),
    );
    query(&mut memo, having, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(pushed_below(&memo, rows).is_empty());
    assert_eq!(candidates_of(&memo, having).len(), 1);
}

/// A global aggregate's `HAVING` stays above even when it is constant: the
/// one output row exists with no input row.
#[test]
fn global_aggregate_having_stays_above() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        p,
        Predicate(ScalarExpr::Literal(ScalarValue::Boolean(false))),
    );
    query(&mut memo, having, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(pushed_below(&memo, rows).is_empty());
}

/// Which `HAVING` conjuncts of a grouped aggregate (`by (job)`) are pushed
/// below it: a key conjunct, and a column-free one built of literals and
/// comparison and boolean operators (`1 = 0`); not a column-free one with a
/// function call, which may be volatile (`random() < 0.5`, built in the IR
/// directly: the frontends and schema derivation reject `random`), nor
/// one over the aggregate's value, nor any of a global aggregate's.
#[test]
fn key_only_having_pushes_keys_and_constants_but_no_function_call() {
    let literal = |value| Box::new(ScalarExpr::Literal(value));
    let one_equals_zero = ScalarExpr::Compare {
        left: literal(ScalarValue::Int64(1)),
        op: CompareOpKind::Eq,
        right: literal(ScalarValue::Int64(0)),
        semantics: ExprSemantics::Sql,
    };
    let random_below_half = ScalarExpr::Compare {
        left: Box::new(ScalarExpr::FunctionCall {
            name: "random".into(),
            args: vec![],
        }),
        op: CompareOpKind::Lt,
        right: literal(ScalarValue::Float64(0.5)),
        semantics: ExprSemantics::Sql,
    };
    let job_is_a = compare(0, CompareOpKind::Eq, ScalarValue::Utf8("a".into())).0;
    let value_above_ten = compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0)).0;
    let by_job = Reduction::by(vec![2]);
    assert!(key_only_having(&by_job, &job_is_a));
    assert!(key_only_having(&by_job, &one_equals_zero));
    assert!(key_only_having(
        &by_job,
        &ScalarExpr::Not(Box::new(one_equals_zero.clone()))
    ));
    assert!(!key_only_having(&by_job, &random_below_half));
    assert!(!key_only_having(&by_job, &value_above_ten));
    let global = Reduction::by(vec![]);
    assert!(!key_only_having(&global, &one_equals_zero));
}

/// A grouped aggregate's constant `HAVING 1 = 0` is pushed below it.
#[test]
fn a_constant_having_of_a_grouped_aggregate_is_pushed_below() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        by_job,
        Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Literal(ScalarValue::Int64(1))),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
            semantics: ExprSemantics::Sql,
        }),
    );
    query(&mut memo, having, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert_eq!(
        pushed_below(&memo, rows).len(),
        1,
        "{}",
        memo_printout(&memo)
    );
    assert_eq!(candidates_of(&memo, having).len(), 3);
}

/// The aggregate's own value `having` is a `Filter` above each readout:
/// the aggregate group's summary candidates are `Filter`s over a readout.
#[test]
fn own_value_having_filters_each_readout() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        Some(compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0))),
    );
    query(&mut memo, by_job, None);
    let memo = pass1_memo(memo, None).unwrap();
    let kinds: Vec<&str> = candidates_of(&memo, by_job)
        .iter()
        .map(|c| c.expression.operator.kind_name())
        .collect();
    assert_eq!(kinds, ["Aggregate", "Filter", "Filter"]);
}
