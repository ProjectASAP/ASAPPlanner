use super::{memo_printout, selector, sum};
use crate::pass1_memo;
use crate::test_fixtures::aggregate_group;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, TimeRangeKind};
use asap_types::stages::Memo;
use asap_types::summary_type::ExactKind;
use asap_types::test_support::memo_builders::{candidates_of, groups_where, query};
use asap_types::workload::DataWorkload;

/// `sum by (job) (sum_over_time(m[5m]))`, and likewise `min` of `min` and
/// `max` of `max`, gets a merged build: one exact accumulator of that kind
/// over the raw samples grouped by `job`, a candidate of the outer
/// aggregate's own accumulator state group, listing the inner aggregate's
/// group in `fused_groups`.
#[test]
fn nested_distributive_aggregate_merges_into_the_outer_state_group() {
    for (intent, kind) in [
        (sum(), ExactKind::Sum),
        (AggIntent::Min { col: None }, ExactKind::Min),
        (AggIntent::Max { col: None }, ExactKind::Max),
    ] {
        let mut memo = Memo::new(&DataWorkload::default());
        let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
        let per_series = aggregate_group(
            &mut memo,
            rows,
            Reduction::PerEntity,
            vec![intent.clone()],
            None,
        );
        let by_job = aggregate_group(
            &mut memo,
            per_series,
            Reduction::by(vec![2]),
            vec![intent],
            None,
        );
        query(&mut memo, by_job, None);
        let memo = pass1_memo(memo, None).unwrap();
        let outer_state = groups_where(
            &memo,
            |e| matches!(e, Operator::ASAP(ASAPOp::SummaryAgg { child, summary_type: FieldDataType::ExactAggregate(state, _), .. }) if *child == per_series && *state == kind),
        );
        let [outer_state] = outer_state[..] else {
            panic!("{kind:?}: {}", memo_printout(&memo))
        };
        let builds = candidates_of(&memo, outer_state);
        assert_eq!(builds.len(), 2, "{kind:?}: {}", memo_printout(&memo));
        let merged = &builds[1];
        assert_eq!(merged.expression.operator.children(), vec![&rows]);
        assert_eq!(merged.fused_groups, vec![per_series]);
    }
}

/// The merge needs the same distributive aggregate twice: a `sum` over a
/// `max`, or a `min` over a `max`, gets none.
#[test]
fn nested_merge_needs_the_same_distributive_aggregate() {
    for outer in [sum(), AggIntent::Min { col: None }] {
        let mut memo = Memo::new(&DataWorkload::default());
        let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
        let per_series = aggregate_group(
            &mut memo,
            rows,
            Reduction::PerEntity,
            vec![AggIntent::Max { col: None }],
            None,
        );
        let by_job = aggregate_group(
            &mut memo,
            per_series,
            Reduction::by(vec![2]),
            vec![outer],
            None,
        );
        query(&mut memo, by_job, None);
        let memo = pass1_memo(memo, None).unwrap();
        assert!(memo
            .groups()
            .iter()
            .flat_map(|g| &g.candidates)
            .all(|c| c.fused_groups.is_empty()));
    }
}
