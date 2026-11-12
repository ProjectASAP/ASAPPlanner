//! Step 2: top-k reuse.

use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::stages::{Memo, OriginOfCandidateForMemoGroup};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;

/// `topk(5, x)` reads `topk(10, x)`'s ranked rows: the 5 highest of the 10
/// highest are the 5 highest.
#[test]
fn a_smaller_topk_reads_the_largest_topk_of_the_same_input() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "x", &["job"], vec![]);
    let topk = |memo: &mut Memo, k: usize| {
        group(
            memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: input,
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::TopK { k }],
                output_names: vec![String::new()],
                filters: vec![None],
                having: None,
            }),
        )
    };
    let top5 = topk(&mut memo, 5);
    let top10 = topk(&mut memo, 10);
    query(&mut memo, top5, Some(AccuracyTarget::exact()));
    query(&mut memo, top10, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let reuse: Vec<_> = candidates_of(&logical.memo, top5)
        .iter()
        .filter(|c| c.origin == OriginOfCandidateForMemoGroup::TopKReuse)
        .collect();
    assert_eq!(reuse.len(), 1);
    assert_eq!(reuse[0].expression.operator.children(), vec![&top10]);
    assert!(candidates_of(&logical.memo, top10)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::TopKReuse));
}
