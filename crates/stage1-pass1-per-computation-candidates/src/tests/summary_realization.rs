use crate::computation_targets::ways_to_compute_intent;
use crate::summary_realization::{
    realize_keyed_additive_summary_input, summarised_input, summary_candidates,
};
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, FieldDataType};
use asap_types::ir::{ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{
    SketchAlgorithm, SketchStatistic, SummaryInputExpr, SummaryTypeCapability,
};
use asap_types::test_support::memo_builders::{group, scan_group};
use asap_types::test_support::memo_fixtures::scan;
use asap_types::workload::DataWorkload;

/// A quantile outside [0, 1] is answered -Inf or +Inf: it gets no summary,
/// so no way besides the plain exact aggregate.
#[test]
fn a_quantile_outside_the_unit_interval_offers_no_summary() {
    let choices = ways_to_compute_intent(&AggIntent::Quantile { col: None, q: 1.5 });
    assert!(choices.is_empty());
}

/// A heap sketch consumes every raw row, so the keyed input rule refuses
/// a TopK over a filtered count.
#[test]
fn keyed_heap_input_refuses_a_filtered_count() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = scan_group(&mut memo, "m", &["job"], vec![]);
    let mut count_by_job = |filter: Option<Predicate<MemoGroupId>>| {
        group(
            &mut memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![2]),
                measures: vec![AggIntent::Count],
                output_names: vec![String::new()],
                filters: vec![filter],
                having: None,
                child: rows,
            }),
        )
    };
    let unfiltered = count_by_job(None);
    let value_above_one = Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(1)),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
        semantics: ExprSemantics::Promql,
    });
    let filtered = count_by_job(Some(value_above_one));
    let output = Reduction::Reduce(asap_types::ir::operator::arguments::GroupKeys::none());
    assert!(realize_keyed_additive_summary_input(&memo, &output, unfiltered).is_some());
    assert!(realize_keyed_additive_summary_input(&memo, &output, filtered).is_none());
}

/// A summary reads the intent's own input columns: one column by name,
/// none as the sample value, several as one hashed tuple.
#[test]
fn summarised_input_reads_the_intents_columns() {
    // Columns [ts=0, value=1, job=2].
    let Operator::NonASAP(NonASAPOp::Scan { schema, .. }) =
        scan("m", &[("job", DataType::Utf8)], vec![])
    else {
        unreachable!("a scan")
    };
    let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
    let cases = [
        (vec![2], column("job")),
        (vec![1], column("value")),
        (vec![], SummaryInputExpr::Column(ColumnRef::SampleValue)),
        (
            vec![1, 2],
            SummaryInputExpr::Tuple(vec![column("value"), column("job")]),
        ),
    ];
    for (cols, want) in cases {
        let intent = AggIntent::Cardinality { cols: cols.clone() };
        assert_eq!(
            summarised_input(&intent, &schema),
            Some(want),
            "wrong summarised input for cols={cols:?}"
        );
    }
}

/// Each approximable intent is offered the algorithms it was offered when
/// the list was written by hand, in the same order; a non-sketch intent
/// is offered none.
#[test]
fn summary_candidates_keep_the_hand_written_lists() {
    use SketchAlgorithm as S;
    let cardinality = |cols: Vec<usize>| AggIntent::Cardinality { cols };
    let cases = [
        (
            AggIntent::Quantile { col: None, q: 0.99 },
            vec![S::Kll, S::DDSketch],
        ),
        (cardinality(vec![]), vec![S::Hll, S::Kmv, S::UnivMon]),
        (cardinality(vec![1]), vec![S::Hll, S::Kmv, S::UnivMon]),
        (cardinality(vec![1, 2]), vec![S::Hll, S::Kmv]),
        (AggIntent::FrequencyL2 { col: None }, vec![S::UnivMon]),
        (AggIntent::FrequencyEntropy { col: None }, vec![S::UnivMon]),
        (
            AggIntent::TopK { k: 10 },
            vec![S::CmsWithHeap, S::CountSketchWithHeap],
        ),
        (AggIntent::Count, vec![S::Cms, S::UnivMon]),
        (AggIntent::Sum { col: None }, vec![]),
    ];
    for (intent, want) in cases {
        assert_eq!(summary_candidates(&intent), want, "{intent:?}");
    }
}

/// An algorithm is offered for an intent exactly when its capability table
/// row answers the intent's statistic: Count-Min's row has no top-k readout,
/// so a top-k is offered only the heap variants, whose rows have one.
#[test]
fn an_algorithm_without_the_readout_is_not_offered() {
    let top_k = AggIntent::TopK { k: 10 };
    let statistic = SketchStatistic::TopK { k: 10 };
    let answers = |algorithm| {
        SummaryTypeCapability::of(&FieldDataType::Sketch(algorithm)).answers(&statistic)
    };
    assert!(!answers(SketchAlgorithm::Cms));
    assert!(answers(SketchAlgorithm::CmsWithHeap));
    let offered = summary_candidates(&top_k);
    assert!(!offered.contains(&SketchAlgorithm::Cms));
    assert!(offered.iter().all(|algorithm| answers(*algorithm)));
}
