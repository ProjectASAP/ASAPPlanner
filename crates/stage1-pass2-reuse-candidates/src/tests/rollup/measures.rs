//! The measures that roll up: a sum and a count as a sum.
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FieldDataType;

use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, OriginOfCandidateForMemoGroup};
use asap_types::summary_type::ExactKind;
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::range;
use asap_types::test_support::memo_fixtures::{aggregate, state_grouped_by};

use super::instant;

/// Concepts doc Example 2, exact values: `sum by (job)` rolls up the
/// finalized `sum by (job, instance)` of the same per-series rows with a
/// sum over its sum column.
#[test]
fn a_coarse_exact_sum_rolls_up_the_finer_sum() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "x", &["job", "instance"], vec![]);
    let window = group(&mut memo, range(60_000, input));
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

/// A count rolls up as a sum of counts, under the count's own column name:
/// `count by (job)` of PromQL (its measure unnamed, `count`) rolls up the
/// finer `count by (job, instance)` with `sum` over its count column, named
/// `count` as the coarse group's.
#[test]
fn a_coarse_count_rolls_up_the_finer_count_as_a_sum() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "x", &["job", "instance"], vec![]);
    let rows = instant(&mut memo, input);
    let fine = group(
        &mut memo,
        aggregate(Reduction::by(vec![2, 3]), AggIntent::Count, rows),
    );
    let coarse = group(
        &mut memo,
        aggregate(Reduction::by(vec![2]), AggIntent::Count, rows),
    );
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
            output_names: vec!["count".into()],
            filters: vec![],
            having: None,
        })]
    );
}
