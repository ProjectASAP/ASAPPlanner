//! The group-scope helpers over a memo group: the column a top-k ranks
//! ([`ranked_column_of_group`]).

use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::operator::AggIntent;
use crate::ir::schema::DataType;
use crate::test_support::memo_fixtures::{aggregate, scan};
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
