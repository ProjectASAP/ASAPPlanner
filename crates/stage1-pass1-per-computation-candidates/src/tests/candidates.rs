use super::{memo_printout, quantile, selector, sum, topk};
use crate::pass1_memo;
use crate::test_fixtures::aggregate_group;
use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{LogicalRequirements, Memo};
use asap_types::summary_type::{ExactKind, SketchStatistic};
use asap_types::test_support::memo_builders::{candidates_of, group, groups_where, query};
use asap_types::workload::DataWorkload;

/// Every group has its own expression as a candidate, with no θ: plain
/// exact execution.
#[test]
fn every_group_offers_its_own_expression() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let s = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, s, None);
    let stage0 = memo.groups().len();
    let memo = pass1_memo(memo, None).unwrap();
    for index in 0..stage0 {
        let group = &memo.groups()[index];
        let own = &group.candidates[0];
        assert_eq!(own.expression.operator, group.expression);
        assert!(own.logical_properties.parameters().is_none());
    }
}

/// An exact accumulator is a state group (`SummaryAgg` of the exact
/// summary type, no θ) read by a `FinalizeExactAccumulator` group, which the
/// aggregate's group reads through a renaming `Project`.
#[test]
fn exact_accumulator_is_a_state_group_and_a_finalize() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let s = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, s, None);
    let memo = pass1_memo(memo, None).unwrap();
    let states = groups_where(&memo, |e| {
        matches!(
            e,
            Operator::ASAP(ASAPOp::SummaryAgg {
                summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, _),
                ..
            })
        )
    });
    let [state] = states[..] else {
        panic!("{}", memo_printout(&memo))
    };
    assert_eq!(candidates_of(&memo, state).len(), 1);
    assert!(candidates_of(&memo, state)[0]
        .logical_properties
        .parameters()
        .is_none());
    // The aggregate reads it through a `FinalizeExactAccumulator`, renamed
    // to the aggregate's columns when its names differ.
    let finalize = Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state });
    let finalize_groups = groups_where(&memo, |e| *e == finalize);
    assert!(candidates_of(&memo, s).iter().any(|c| {
        c.expression.operator == finalize
            || c.expression
                .operator
                .children()
                .iter()
                .any(|g| finalize_groups.contains(g))
    }));
}

/// A multi-measure aggregate's candidate joins the measures' own groups
/// (one single-measure aggregate each, with its own candidates) on the
/// group keys.
#[test]
fn multi_measure_candidate_joins_single_measure_groups() {
    let mut memo = Memo::new(&DataWorkload::default());
    // A SQL table `flows(src Utf8, bytes Float64)`.
    let rows = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "flows".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![
                Field::plain("src", DataType::Utf8, false),
                Field::plain("bytes", DataType::Float64, false),
            ]),
        }),
    );
    let both = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![0]),
        vec![
            AggIntent::Sum { col: Some(1) },
            AggIntent::Quantile {
                col: Some(1),
                q: 0.9,
            },
        ],
        None,
    );
    query(&mut memo, both, None);
    let memo = pass1_memo(memo, None).unwrap();
    let single = groups_where(
        &memo,
        |e| matches!(e, Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) if measures.len() == 1),
    );
    assert_eq!(single.len(), 2, "{}", memo_printout(&memo));
    // The quantile measure's group has its exact aggregate and two readouts.
    assert!(single.iter().any(|g| candidates_of(&memo, *g).len() == 3));
    let joins = groups_where(&memo, |e| {
        matches!(e, Operator::NonASAP(NonASAPOp::Join { .. }))
    });
    assert_eq!(joins.len(), 1);
    assert_eq!(candidates_of(&memo, both).len(), 2);
}

/// Pass 1 sets no input requirement, a top-k readout's included: each
/// candidate has one empty requirement per input group, and Pass 2 sets
/// the readouts' required accuracy (a top-k's `heap_size` ≥ k among it).
#[test]
fn pass1_sets_no_input_requirement() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_job = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![]),
        vec![topk(15)],
        None,
    );
    let samples = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let p = aggregate_group(
        &mut memo,
        samples,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, ranked, None);
    query(&mut memo, p, None);
    let memo = pass1_memo(memo, None).unwrap();
    // The top-k readout is offered, and has no input requirement either.
    assert!(!groups_where(&memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryEstimate {
            query: SketchStatistic::TopK { k: 15 },
            ..
        })
    ))
    .is_empty());
    for group in memo.groups() {
        for candidate in &group.candidates {
            assert_eq!(
                candidate.input_requirements,
                vec![
                    LogicalRequirements::default();
                    candidate.expression.operator.children().len()
                ]
            );
        }
    }
}
