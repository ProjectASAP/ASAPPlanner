//! The group-scope helpers over a memo group: the column a top-k ranks
//! ([`ranked_column_of_group`]) and the metric whose raw data samples a
//! group outputs ([`raw_data_samples_of_metric_of_group`]).

use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::operator::AggIntent;
use crate::ir::schema::DataType;
use crate::test_support::memo_fixtures::{aggregate, range, scan};
use crate::workload::DataWorkload;

/// A top-k over a one-measure grouped aggregate ranks its value, the last
/// column; over per-series rows, the sample value.
#[test]
fn ranked_column_of_group_is_the_measure_or_the_sample_value() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = memo
        .find_or_add_group(scan("m", &[("job", DataType::Utf8)], vec![]))
        .unwrap();
    let by_job = memo
        .find_or_add_group(aggregate(
            Reduction::by(vec![2]),
            AggIntent::Sum { col: None },
            rows,
        ))
        .unwrap();
    assert_eq!(ranked_column_of_group(&memo, by_job), Ok(1));
    assert_eq!(ranked_column_of_group(&memo, rows), Ok(1));
}

/// The raw data samples of `m` reach a group through a range and a plain
/// sum, not through a rate.
#[test]
fn raw_data_samples_of_metric_of_group_follows_ranges_and_plain_sums() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = memo.find_or_add_group(scan("m", &[], vec![])).unwrap();
    let window = memo.find_or_add_group(range(60_000, rows)).unwrap();
    let summed = memo
        .find_or_add_group(aggregate(
            Reduction::PerEntity,
            AggIntent::Sum { col: None },
            window,
        ))
        .unwrap();
    let rated = memo
        .find_or_add_group(aggregate(Reduction::PerEntity, AggIntent::Rate, window))
        .unwrap();
    let metric = |group| raw_data_samples_of_metric_of_group(&memo, group);
    assert_eq!(metric(summed), Some("m".to_string()));
    assert_eq!(metric(rated), None);
}
