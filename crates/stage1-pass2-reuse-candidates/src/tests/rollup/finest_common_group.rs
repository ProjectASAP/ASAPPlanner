//! A cluster's finest common group, which no query asks for.
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
use asap_types::ir::schema::FieldDataType;

use asap_types::ir::{ASAPOp, ExprSemantics, NonASAPOp, Operator, Predicate};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::state_grouped_by;
use asap_types::test_support::memo_fixtures::{compare, range};

/// Concepts doc Example 3: Q1 = `quantile by (host)(latency[5m])`, Q2 =
/// `quantile by (job)(latency{host="xxx"}[5m])`. Neither is finer, so the
/// finest common group G* = `SummaryAgg[KLL, by job, host]` over Q1's rows
/// is synthesized with one build per listed θ, Q1's state merges it by
/// host and Q2's merges `Filter(host = "xxx", G*)` by job.
#[test]
fn a_finest_common_group_no_query_asks_for_serves_both() {
    let mut memo = Memo::new(&DataWorkload::default());
    let labels = ["job", "host"];
    let all = scan_group(&mut memo, "latency", &labels, vec![]);
    let xxx = scan_group(
        &mut memo,
        "latency",
        &labels,
        vec![compare(
            3,
            CompareOpKind::Eq,
            ScalarValue::Utf8("xxx".into()),
            ExprSemantics::Promql,
        )],
    );
    let all_window = group(&mut memo, range(300_000, all));
    let xxx_window = group(&mut memo, range(300_000, xxx));
    let by_host = sketch_state(
        &mut memo,
        all_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let by_job = sketch_state(
        &mut memo,
        xxx_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2]),
    );
    for state in [by_host, by_job] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.99 });
        query(&mut memo, readout, None);
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let memo = &logical.memo;
    let finest = group_of(
        memo,
        &state_grouped_by(
            FieldDataType::Sketch(SketchAlgorithm::Kll),
            Reduction::by(vec![2, 3]),
            all_window,
        ),
    )
    .expect("G*: KLL by job, host over Q1's rows");
    assert_eq!(
        candidates_of(memo, finest)
            .iter()
            .filter(|c| c.expression.operator == memo.groups()[finest.0 as usize].expression)
            .count(),
        parameter_list(SketchAlgorithm::Kll).len()
    );
    let merges = |state: MemoGroupId| -> Vec<Operator<MemoGroupId>> {
        candidates_of(memo, state)
            .iter()
            .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
            .map(|c| c.expression.operator.clone())
            .collect()
    };
    let listed = parameter_list(SketchAlgorithm::Kll).len();
    // One merge per θ of G*'s builds.
    assert_eq!(
        merges(by_host),
        vec![
            Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![finest],
                group_by: Some(vec![1]),
            });
            listed
        ]
    );
    let filter = group_of(
        memo,
        &Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(compare(
                1,
                CompareOpKind::Eq,
                ScalarValue::Utf8("xxx".into()),
                ExprSemantics::Promql,
            )),
            child: finest,
        }),
    )
    .expect("Filter(host = \"xxx\", G*)");
    assert_eq!(candidates_of(memo, filter).len(), listed);
    assert_eq!(
        merges(by_job),
        vec![
            Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![filter],
                group_by: Some(vec![0]),
            });
            listed
        ]
    );
}
