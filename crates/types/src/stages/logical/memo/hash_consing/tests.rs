//! Hash-consing: equal expressions are one memo group, and a duplicate
//! candidate is the candidate it duplicates, on the concepts doc's KLL example
//! (section 1: G1 the latency scan, G2 its KLL state).

use super::super::tests::kll_k;
use super::super::*;
use crate::ir::operator::arguments::{GroupKeys, Reduction, Source};
use crate::ir::operator::AggIntent;
use crate::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use crate::ir::scalar::{ProjectItem, SortKey};
use crate::ir::schema::{DataType, Field, FieldDataType, Schema};
use crate::ir::{ExprSemantics, NonASAPOp, Predicate, ScalarExpr};
use crate::summary_type::SketchAlgorithm;
use crate::test_support::memo_builders::try_candidate;
use crate::test_support::memo_fixtures::{aggregate_with, scan, state};

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

/// An equal expression over the same input groups is found, not added.
#[test]
fn find_or_add_group_returns_the_group_of_an_equal_expression() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    assert_eq!(
        memo.find_or_add_group(scan("latency", &[], vec![]))
            .unwrap(),
        g1
    );
    assert_eq!(
        memo.find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
            .unwrap(),
        g2
    );
    assert_ne!(
        memo.find_or_add_group(scan("errors", &[], vec![])).unwrap(),
        g1
    );
    assert_eq!(memo.groups().len(), 3);
    assert_eq!(
        memo.groups()[g2.0 as usize].expression.children(),
        vec![&g1]
    );
}

/// The same operator over different input groups is a different group.
#[test]
fn find_or_add_group_separates_equal_operators_over_different_inputs() {
    let mut memo = Memo::new(&DataWorkload::default());
    let latency = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let errors = memo.find_or_add_group(scan("errors", &[], vec![])).unwrap();
    assert_ne!(
        memo.find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), latency))
            .unwrap(),
        memo.find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), errors))
            .unwrap()
    );
}

/// An expression whose copies may return different rows (an unordered
/// `LIMIT`) gets a new group every time.
#[test]
fn find_or_add_group_never_returns_an_unshareable_group() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    assert_ne!(
        memo.find_or_add_group(unordered_limit(g1)).unwrap(),
        memo.find_or_add_group(unordered_limit(g1)).unwrap()
    );
}

/// KLL builds with k = 269 and k = 547 are two candidates of one group
/// G2: θ is a logical property, not part of the expression.
#[test]
fn kll_builds_with_k_269_and_k_547_share_one_group() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let k269_group = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    let k269 = try_candidate(
        &mut memo,
        k269_group,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1),
        kll_k(269),
    )
    .unwrap();
    let k547_group = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    let k547 = try_candidate(
        &mut memo,
        k547_group,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1),
        kll_k(547),
    )
    .unwrap();
    assert_eq!(k269_group, k547_group);
    assert_ne!(k269, k547);
    assert_eq!(memo.groups()[k269_group.0 as usize].candidates.len(), 2);
    assert_eq!(
        memo.candidate(k547).logical_properties.parameters(),
        kll_k(547).as_ref()
    );
}

/// A candidate with the same expression and logical properties as one
/// in the group is not added again; its id is returned.
#[test]
fn add_candidate_returns_the_existing_candidate_for_a_duplicate() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    let first = try_candidate(
        &mut memo,
        g2,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1),
        kll_k(269),
    )
    .unwrap();
    let again = try_candidate(
        &mut memo,
        g2,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1),
        kll_k(269),
    )
    .unwrap();
    assert_eq!(first, again);
    assert_eq!(
        first,
        CandidateForMemoGroupId {
            group: g2,
            index_in_group: 0
        }
    );
    let other_size = try_candidate(
        &mut memo,
        g2,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1),
        kll_k(547),
    );
    assert_ne!(other_size.unwrap(), first);
    assert_eq!(memo.groups()[g2.0 as usize].candidates.len(), 2);
}

/// `Scan m`: `[ts, service, value, latency]`, no unique key.
fn service_scan() -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("service", DataType::Utf8, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("latency", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    })
}

/// Whether `a` and `b`, each built into one memo through
/// `find_or_add_group`, are one group.
fn one_group(
    a: impl FnOnce(&mut Memo) -> MemoGroupId,
    b: impl FnOnce(&mut Memo) -> MemoGroupId,
) -> bool {
    let mut memo = Memo::new(&DataWorkload::default());
    let a = a(&mut memo);
    let b = b(&mut memo);
    a == b
}

/// `operator` over the group of [`service_scan`].
fn over_scan(
    operator: impl FnOnce(MemoGroupId) -> Operator<MemoGroupId>,
) -> impl FnOnce(&mut Memo) -> MemoGroupId {
    |memo| {
        let scan = memo.find_or_add_group(service_scan()).unwrap();
        memo.find_or_add_group(operator(scan)).unwrap()
    }
}

/// `quantile(q, col) by (by)` over [`service_scan`], with `filter`.
fn quantile_agg(
    by: Vec<usize>,
    col: Option<usize>,
    q: f64,
    filter: Option<Predicate<MemoGroupId>>,
) -> impl FnOnce(&mut Memo) -> MemoGroupId {
    over_scan(move |child| {
        aggregate_with(
            Reduction::by(by),
            vec![(AggIntent::Quantile { col, q }, filter)],
            None,
            child,
        )
    })
}

/// Aggregates that differ in a column, a quantile or a filter are
/// different groups.
#[test]
fn aggregates_that_differ_in_an_argument_are_different_groups() {
    assert!(!one_group(
        quantile_agg(vec![1], Some(2), 0.5, None),
        quantile_agg(vec![1], Some(3), 0.5, None)
    ));
    assert!(!one_group(
        quantile_agg(vec![], Some(2), 0.5, None),
        quantile_agg(vec![], Some(2), 0.9, None)
    ));
    let latency_above_1 = Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(3)),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
        semantics: ExprSemantics::Sql,
    });
    assert!(!one_group(
        quantile_agg(vec![1], Some(2), 0.5, None),
        quantile_agg(vec![1], Some(2), 0.5, Some(latency_above_1))
    ));
}

/// Equal expressions without a unique key are one group when they return
/// the same rows each time: a keyless scan, a global aggregate, a
/// `without` aggregate.
#[test]
fn equal_keyless_expressions_that_return_the_same_rows_are_one_group() {
    let table_scan = |memo: &mut Memo| {
        memo.find_or_add_group(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "metrics".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, true),
                ],
                0,
                vec![],
            ),
        }))
        .unwrap()
    };
    assert!(one_group(table_scan, table_scan));
    assert!(one_group(
        quantile_agg(vec![], Some(2), 0.9, None),
        quantile_agg(vec![], Some(2), 0.9, None)
    ));
    let without = || {
        over_scan(|child| {
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(GroupKeys::without(vec![0])),
                measures: vec![AggIntent::Count],
                output_names: vec![],
                filters: vec![],
                having: None,
                child,
            })
        })
    };
    assert!(one_group(without(), without()));
}

/// `DISTINCT` on every column returns the same rows each time and is one
/// group; a `Dedup` on some columns keeps an unspecified row per key.
#[test]
fn dedup_is_one_group_only_on_every_column() {
    let dedup = |cols: Vec<usize>| {
        over_scan(move |child| Operator::NonASAP(NonASAPOp::Dedup { cols, child }))
    };
    assert!(one_group(dedup(vec![]), dedup(vec![])));
    assert!(!one_group(dedup(vec![1]), dedup(vec![1])));
}

/// Two unordered `LIMIT 10`s may return different rows: two groups, and
/// so are equal parents over them.
#[test]
fn unordered_limits_and_their_parents_are_different_groups() {
    assert!(!one_group(
        over_scan(unordered_limit),
        over_scan(unordered_limit)
    ));
    let distinct_over_limit = |memo: &mut Memo| {
        let limit = over_scan(unordered_limit)(memo);
        memo.find_or_add_group(Operator::NonASAP(NonASAPOp::Dedup {
            cols: vec![],
            child: limit,
        }))
        .unwrap()
    };
    assert!(!one_group(distinct_over_limit, distinct_over_limit));
}

/// A `LIMIT` over a `Sort` with the same partitions keeps the first rows
/// of a fixed order (one group); over a `Sort` partitioned differently,
/// which rows come first depends on input order (two groups).
#[test]
fn limits_over_a_sort_with_their_partitions_are_one_group() {
    let limit = |sort_partitions: GroupKeys| {
        move |memo: &mut Memo| {
            let sorted = over_scan(|child| {
                Operator::NonASAP(NonASAPOp::Sort {
                    keys: vec![SortKey {
                        expr: ScalarExpr::Column(2),
                        ascending: false,
                        nulls_first: false,
                    }],
                    partition_by: sort_partitions,
                    child,
                })
            })(memo);
            memo.find_or_add_group(Operator::NonASAP(NonASAPOp::Limit {
                n: Some(10),
                offset: 0,
                partition_by: GroupKeys::default(),
                child: sorted,
            }))
            .unwrap()
        }
    };
    assert!(one_group(
        limit(GroupKeys::default()),
        limit(GroupKeys::default())
    ));
    assert!(!one_group(
        limit(GroupKeys::by(vec![1])),
        limit(GroupKeys::by(vec![1]))
    ));
}

/// A projection emitting the literal `value`.
fn literal(value: f64) -> impl FnOnce(&mut Memo) -> MemoGroupId {
    over_scan(move |child| {
        Operator::NonASAP(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: Some("v".into()),
                expr: ScalarExpr::Literal(ScalarValue::Float64(value)),
            }],
            child,
        })
    })
}

/// Signed zeros and different non-finite literals are different groups;
/// JSON's `null` for non-finite floats never decides equality.
#[test]
fn signed_zero_and_non_finite_literals_are_different_groups() {
    for (a, b) in [
        (0.0, -0.0),
        (-0.0, 0.0),
        (f64::INFINITY, f64::NEG_INFINITY),
        (f64::NAN, f64::NAN),
    ] {
        assert!(!one_group(literal(a), literal(b)), "{a} and {b}");
    }
    assert!(one_group(literal(f64::INFINITY), literal(f64::INFINITY)));
}
