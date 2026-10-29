use std::rc::Rc;

use crate::support;
use asap_logical_asap_optimization::accuracy::estimators::local_guarantee;
use asap_logical_asap_optimization::accuracy::satisfies;
use asap_logical_asap_optimization::pass2::cse::share_common_sub_dags;
use asap_types::ir::properties::AccuracyMetric;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchStatistic, SummaryInputExpr};
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use support::{logical_candidates, lower_promql, physical_asap_dag, run_stage1_to_stage3};

/// The summary evaluation over a UnivMon state that `root` reads, if any.
fn univmon_estimate(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    OperatorNode::reachable(root).into_iter().find(|node| {
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator else {
            return false;
        };
        matches!(&summary_input.operator, Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
            if kind.algorithm() == &SketchAlgorithm::UnivMon)
    })
}

/// The logical candidate of `query` that evaluates a UnivMon state.
fn candidate(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let root = lower_promql(query, accuracy.clone()).unwrap();
    logical_candidates(root)
        .into_iter()
        .find_map(|node| univmon_estimate(&node))
        .expect("UnivMon candidate")
}

#[test]
fn four_evaluations_size_their_value_frequency_states_and_keep_honest_guarantees() {
    // Pass 1 sizes UnivMon per readout: the distinct count and L2 get their
    // own shapes, while the total and entropy keep the baseline shape and so
    // share one state. Sharing across readouts is the summary-capability
    // rule's job.
    let accuracy = AccuracyTarget::max_error(0.02);
    let roots: Vec<_> = [
        ("distinct_over_time(m[5m])", accuracy.clone()),
        ("count_over_time(m[5m])", accuracy.clone()),
        ("l2_over_time(m[5m])", accuracy.clone()),
        ("entropy_over_time(m[5m])", accuracy),
    ]
    .into_iter()
    .enumerate()
    .map(|(id, (query, accuracy))| (id, candidate(query, accuracy)))
    .collect();
    let roots = share_common_sub_dags(roots);
    let state = |index: usize| {
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
            &roots[index].1.operator
        else {
            panic!()
        };
        Rc::clone(summary_input)
    };
    assert!(Rc::ptr_eq(&state(1), &state(3)), "total and entropy share");
    for (a, b) in [(0, 1), (0, 2), (1, 2)] {
        assert!(
            !Rc::ptr_eq(&state(a), &state(b)),
            "{a} and {b} are sized apart"
        );
    }
    for (index, root) in &roots {
        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) = &root.operator
        else {
            panic!()
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { input, family, .. }) = &summary_input.operator
        else {
            panic!()
        };
        assert!(matches!(input.item, Some(SummaryInputExpr::Column(_))));
        assert_eq!(input.weight, SummaryInputExpr::Constant(1.0));
        let guarantee = root.guarantee.as_ref();
        match index {
            // The distinct count's failure probability waits for the
            // source's distinct bound, which Stage 1 does not read.
            0 => {
                let guarantee = guarantee.expect("a distinct-count guarantee");
                assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Relative);
                assert_eq!(
                    guarantee.spec.error.evaluate().map(|e| e <= 0.02),
                    Some(true)
                );
                assert_eq!(guarantee.spec.failure_probability.evaluate(), None);
                assert!(local_guarantee(family, query).is_none());
            }
            1 => {
                assert!(matches!(
                    query,
                    SketchStatistic::PointCount { value: None, .. }
                ));
                assert!(guarantee.is_some_and(|g| g.is_exact()));
            }
            2 => {
                let guarantee = guarantee.expect("an L2 guarantee");
                assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Relative);
                assert!(satisfies(
                    guarantee,
                    &AccuracyTarget::max_error(0.02).accuracy_metric(AccuracyMetric::Relative)
                ));
                assert!(guarantee.spec.failure_probability.evaluate().unwrap() <= 0.01);
            }
            // Entropy has no certified bound, so composition certifies nothing.
            _ => {
                assert!(guarantee.is_none());
                assert!(local_guarantee(family, query).is_none());
            }
        }
        physical_asap_dag(root);
    }
}

#[test]
fn uncalibrated_frequency_evaluations_do_not_bypass_accuracy_targets() {
    // An unmeasured heuristic remains inspectable but is never certified or
    // automatically selected for a caller-visible bounded-error result.
    // L2 is certified; entropy is not.
    for query in ["entropy_over_time(m[5m])"] {
        for target in [
            AccuracyTarget::exact(),
            AccuracyTarget::max_error(0.02),
            AccuracyTarget::max_error(0.02).failure_probability(0.01),
        ] {
            let root = lower_promql(query, target.clone()).unwrap();
            let generated = logical_candidates(Rc::clone(&root));
            let run = run_stage1_to_stage3(root, target.clone());
            let unknown: Vec<_> = generated
                .iter()
                .filter(|node| univmon_estimate(node).is_some() && node.guarantee.is_none())
                .collect();
            if target.is_exact() {
                assert!(unknown.is_empty(), "{query}");
            } else {
                assert!(!unknown.is_empty(), "{query}");
                let [asap_types::ir::QueryRoot::Operator(selected)] = run
                    .selected_logical()
                    .expect("a certified plan is selected")
                else {
                    panic!("one operator result");
                };
                assert!(selected.guarantee.is_some(), "{query}");
            }
        }
    }
}
