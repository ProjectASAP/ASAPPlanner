//! Step 3: the groups each query may read only exactly.
use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
use asap_types::ir::{ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr};
use asap_types::stages::{ColumnLineage, Memo};
use asap_types::summary_type::SketchAlgorithm;
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::arithmetic;

/// The metric rule is per query, not per group: Q0 =
/// `quantile_over_time(0.99, latency[5m]) / quantile_over_time(0.5,
/// latency[5m])` and Q1 = the same ratio `> 1`, both relative 0.05, share
/// the ratio group. Q0 may read the DDSketch readouts below it (the
/// division carries relative error); Q1's comparison has no contract, so
/// Q1 reads the ratio and every group below it only exactly. (Both read
/// the rows below a sketch build exactly: a build has no contract.)
#[test]
fn a_query_comparing_a_shared_ratio_reads_it_only_exactly() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "latency", &["job"], vec![]);
    let window = range(&mut memo, input, 300);
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &[SketchAlgorithm::DDSketch]);
    let p50 = quantile_over_time_readouts(&mut memo, window, 0.5, &[SketchAlgorithm::DDSketch]);
    let ratio = group(
        &mut memo,
        arithmetic(ArithmeticOpKind::Div, p99.result, p50.result),
    );
    let value = memo.groups()[ratio.0 as usize]
        .group_properties
        .column_lineage
        .iter()
        .position(|lineage| matches!(lineage, ColumnLineage::Computed { .. }))
        .expect("the ratio computes its value");
    let above_one = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(value)),
                op: CompareOpKind::Gt,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
                semantics: ExprSemantics::Promql,
            }),
            child: ratio,
        }),
    );
    let target = || AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let q0 = query(&mut memo, ratio, Some(target()));
    let q1 = query(&mut memo, above_one, Some(target()));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let exact_only = &logical.exact_only_groups_of_query;
    for group in [ratio, p99.result, p50.result, p99.by_algorithm[0].1] {
        assert!(!exact_only[q0].contains(&group), "G{}", group.0);
    }
    for group in [ratio, p99.result, p50.result, p99.by_algorithm[0].1, window] {
        assert!(exact_only[q1].contains(&group), "G{}", group.0);
    }
    assert!(!exact_only[q1].contains(&above_one));
    let ddsketch_p99 = readout_of(&logical.memo, p99.by_algorithm[0].2);
    assert!(
        pruned_entry(&logical, ddsketch_p99).is_none(),
        "Q0 still reads the DDSketch readout"
    );
}
