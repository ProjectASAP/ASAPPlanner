use std::time::Duration;

use super::{memo_printout, quantile, selector};
use crate::pass1_memo;
use crate::test_fixtures::aggregate_group;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::schema::{DataType, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{NonASAPOp, Operator, ScalarExpr, TimeRangeKind};
use asap_types::stages::Memo;
use asap_types::test_support::memo_builders::{candidates_of, group, query};
use asap_types::test_support::memo_fixtures::scan;
use asap_types::workload::DataWorkload;

/// `quantile_over_time(0.9, latency[5m])` over series whose identity is
/// realized as a column (as the planner does before Stage 0): each readout
/// candidate outputs the per-series aggregate's exact schema, the
/// `$promql_series_identity` column included, so every readout keeps the
/// series it answers for.
#[test]
fn per_series_readouts_keep_the_series_identity() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "latency", &["job"]);
    let per_series = aggregate_group(
        &mut memo,
        rows,
        Reduction::PerEntity,
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, per_series, None);
    let memo = pass1_memo(memo, None).unwrap();
    let output = &memo.groups()[per_series.0 as usize].group_properties.schema;
    assert!(output.closed && output.has_promql_series_identity());
    let readouts = &candidates_of(&memo, per_series)[1..];
    assert_eq!(readouts.len(), 2, "{}", memo_printout(&memo));
    for readout in readouts {
        let Operator::NonASAP(NonASAPOp::Project { child, cols, .. }) =
            &readout.expression.operator
        else {
            panic!("a renaming Project: {:?}", readout.expression.operator)
        };
        let estimate = &memo.groups()[child.0 as usize].group_properties.schema;
        assert!(estimate.has_promql_series_identity());
        let identity = output
            .fields
            .iter()
            .position(|field| field.name == PROMQL_SERIES_IDENTITY)
            .unwrap();
        let read = estimate
            .fields
            .iter()
            .position(|field| field.name == PROMQL_SERIES_IDENTITY)
            .unwrap();
        assert_eq!(cols[identity].expr, ScalarExpr::Column(read));
    }
}

/// Without a realized identity the per-series aggregate's schema is open:
/// its rows carry labels it does not list. A renaming `Project` would drop
/// them, so no readout is offered; the group keeps its own expression.
#[test]
fn an_open_per_series_aggregate_offers_no_renamed_readout() {
    let mut memo = Memo::new(&DataWorkload::default());
    let scan = group(
        &mut memo,
        scan("latency", &[("job", DataType::Utf8)], vec![]),
    );
    let rows = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scan,
        }),
    );
    let per_series = aggregate_group(
        &mut memo,
        rows,
        Reduction::PerEntity,
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, per_series, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(
        !memo.groups()[per_series.0 as usize]
            .group_properties
            .schema
            .closed
    );
    assert_eq!(candidates_of(&memo, per_series).len(), 1);
}
