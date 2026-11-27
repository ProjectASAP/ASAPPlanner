//! A `FinalizeExactAccumulator` covers what the aggregate it finalizes to
//! covers (`docs/design_docs/proposals/asap-primitive-schema.md` §4.2.2):
//! `rate(m[5m])` finalized from a counter state has the coverage of
//! `Aggregate(Rate)` over the same rows, whichever form creates the group.
use asap_types::ir::operator::arguments::{GroupKeys, Reduction};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, ExprSemantics, Operator, Predicate};
use asap_types::stages::{AggregateCoverage, Memo, MemoGroupId};
use asap_types::summary_type::{CounterReadout, ExactKind, ExactParams, SummaryUpdate};
use asap_types::test_support::memo_builders::{candidate, group, scan_group};
use asap_types::test_support::memo_fixtures::{
    aggregate, aggregate_with, compare, range, state, state_grouped_by,
};
use asap_types::workload::DataWorkload;

/// The `value` and `pod` columns of [`scan_group`]`(m, [pod])`.
const VALUE: usize = 1;
const POD: usize = 2;

fn add(memo: &mut Memo, op: Operator<MemoGroupId>) -> MemoGroupId {
    memo.find_or_add_group(op).unwrap()
}
fn coverage(memo: &Memo, g: MemoGroupId) -> Option<AggregateCoverage> {
    memo.groups()[g.0 as usize]
        .group_properties
        .aggregate_coverage
        .clone()
}
fn finalize(child: MemoGroupId, counter_readout: Option<CounterReadout>) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child,
        counter_readout,
    })
}
/// `Scan m` (label `pod`) and `m[5m]`.
fn memo_over_last_5m() -> (Memo, MemoGroupId) {
    let mut memo = Memo::new(&DataWorkload::default());
    let scan = scan_group(&mut memo, "m", &["pod"], vec![]);
    let last_5m = add(&mut memo, range(300_000, scan));
    (memo, last_5m)
}

/// `rate(m[5m])`, computed directly and by finalizing a counter state.
#[test]
fn a_finalized_rate_covers_what_the_rate_covers() {
    let (mut memo, last_5m) = memo_over_last_5m();
    let rate = add(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Rate, last_5m),
    );
    let counter = add(
        &mut memo,
        state(
            FieldDataType::ExactAggregate(ExactKind::Counter, ExactParams::Counter),
            last_5m,
        ),
    );
    let finalized = add(&mut memo, finalize(counter, Some(CounterReadout::Rate)));
    assert!(coverage(&memo, rate).is_some());
    assert_eq!(coverage(&memo, finalized), coverage(&memo, rate));
}

/// Each exact kind's finalize covers what its aggregate over the same rows
/// covers: `increase`, `sum_over_time`, `count_over_time`,
/// `max_over_time`, `min_over_time` and `irate` of `m[5m]`.
#[test]
fn every_finalized_kind_covers_what_its_aggregate_covers() {
    for (kind, counter_readout, measure) in [
        (
            ExactKind::Counter,
            Some(CounterReadout::Increase),
            AggIntent::Increase,
        ),
        (ExactKind::Sum, None, AggIntent::Sum { col: None }),
        (ExactKind::Count, None, AggIntent::Count),
        (ExactKind::Max, None, AggIntent::Max { col: None }),
        (ExactKind::Min, None, AggIntent::Min { col: None }),
        (ExactKind::IRate, None, AggIntent::IRate),
    ] {
        let (mut memo, last_5m) = memo_over_last_5m();
        let direct = add(
            &mut memo,
            aggregate(Reduction::PerEntity, measure.clone(), last_5m),
        );
        let exact_state = add(&mut memo, state(FieldDataType::exact(kind), last_5m));
        let finalized = add(&mut memo, finalize(exact_state, counter_readout));
        assert!(coverage(&memo, direct).is_some(), "{measure:?}");
        assert_eq!(
            coverage(&memo, finalized),
            coverage(&memo, direct),
            "{measure:?}"
        );
    }
}

/// `max by (pod) (m[5m])`-shaped: a grouped exact state finalizes to the
/// grouped aggregate.
#[test]
fn a_finalized_grouped_max_covers_what_the_grouped_max_covers() {
    let (mut memo, last_5m) = memo_over_last_5m();
    let by_pod = Reduction::Reduce(GroupKeys::by(vec![POD]));
    let direct = add(
        &mut memo,
        aggregate(by_pod.clone(), AggIntent::Max { col: None }, last_5m),
    );
    let max_state = add(
        &mut memo,
        state_grouped_by(FieldDataType::exact(ExactKind::Max), by_pod, last_5m),
    );
    let finalized = add(&mut memo, finalize(max_state, None));
    assert!(coverage(&memo, direct).is_some());
    assert_eq!(coverage(&memo, finalized), coverage(&memo, direct));
}

/// SQL-shaped `SUM(value) FILTER (WHERE pod = 'a')`: the state reads a
/// named column and filters its rows as the aggregate's measure does.
#[test]
fn a_finalized_filtered_sum_of_a_named_column_covers_what_the_sum_covers() {
    let (mut memo, last_5m) = memo_over_last_5m();
    let pod_is_a = Predicate(compare(
        POD,
        CompareOpKind::Eq,
        ScalarValue::Utf8("a".into()),
        ExprSemantics::Sql,
    ));
    let direct = add(
        &mut memo,
        aggregate_with(
            Reduction::PerEntity,
            vec![(AggIntent::Sum { col: Some(VALUE) }, Some(pod_is_a.clone()))],
            None,
            last_5m,
        ),
    );
    let sum_state = add(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: last_5m,
            summary_type: FieldDataType::exact(ExactKind::Sum),
            input: SummaryUpdate::column(ColumnRef::Named("value".into())),
            reduction: Reduction::PerEntity,
            filter: Some(pod_is_a),
        }),
    );
    let finalized = add(&mut memo, finalize(sum_state, None));
    assert!(coverage(&memo, direct).is_some());
    assert_eq!(coverage(&memo, finalized), coverage(&memo, direct));
}

/// The group of `rate(m[5m])` has one coverage whichever form creates it:
/// created by the `Aggregate` with the finalize added as a candidate (as
/// Pass 1 adds it), or created by the finalize with the `Aggregate` added.
#[test]
fn a_rate_group_has_one_coverage_whichever_form_creates_it() {
    let counter_type = FieldDataType::ExactAggregate(ExactKind::Counter, ExactParams::Counter);
    let rate_of = |last_5m| aggregate(Reduction::PerEntity, AggIntent::Rate, last_5m);

    let (mut aggregate_first, last_5m) = memo_over_last_5m();
    let rate = group(&mut aggregate_first, rate_of(last_5m));
    let counter = group(&mut aggregate_first, state(counter_type.clone(), last_5m));
    candidate(
        &mut aggregate_first,
        rate,
        finalize(counter, Some(CounterReadout::Rate)),
        None,
    );

    let (mut finalize_first, last_5m) = memo_over_last_5m();
    let counter = group(&mut finalize_first, state(counter_type, last_5m));
    let finalized = group(
        &mut finalize_first,
        finalize(counter, Some(CounterReadout::Rate)),
    );
    candidate(&mut finalize_first, finalized, rate_of(last_5m), None);

    assert!(coverage(&aggregate_first, rate).is_some());
    assert_eq!(
        coverage(&finalize_first, finalized),
        coverage(&aggregate_first, rate)
    );
}

/// `rate` and `increase` finalized from one counter state compute different
/// things, so their coverages differ.
#[test]
fn a_finalized_rate_and_increase_of_one_counter_differ() {
    let (mut memo, last_5m) = memo_over_last_5m();
    let counter = add(
        &mut memo,
        state(FieldDataType::exact(ExactKind::Counter), last_5m),
    );
    let rate = add(&mut memo, finalize(counter, Some(CounterReadout::Rate)));
    let increase = add(&mut memo, finalize(counter, Some(CounterReadout::Increase)));
    assert!(coverage(&memo, rate).is_some());
    assert_ne!(coverage(&memo, rate), coverage(&memo, increase));
}
