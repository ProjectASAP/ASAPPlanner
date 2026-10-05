use std::rc::Rc;

use asap_logical_optimizer::accuracy::{AccuracyModel, DefaultAccuracyModel};
mod support;
use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::properties::ErrorMetric;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchStatistic, SummaryInputExpr};
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::types::AccuracyTarget;
use support::{lower_promql, post_asap_dag, selected_dag, stage1_candidates};

fn reads_univmon(node: &OperatorNode) -> bool {
    let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator else {
        return false;
    };
    matches!(&summary_input.operator, Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
        if kind.algorithm() == &SketchAlgorithm::UnivMon)
}

/// Stage 1's candidate for `query` whose root reads a UnivMon summary.
fn candidate(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let root = lower_promql(query, accuracy).unwrap();
    stage1_candidates(&root)
        .into_iter()
        .find(|node| reads_univmon(node))
        .expect("UnivMon candidate")
}

#[test]
fn four_evaluations_share_one_value_frequency_state_and_keep_honest_guarantees() {
    // Equal data, grouping, window and requirement produce one state
    // independently of evaluation: UnivMon is sized for L2 whatever it reads.
    let accuracy = AccuracyTarget::Epsilon(0.02);
    let roots: Vec<_> = [
        "distinct_over_time(m[5m])",
        "count_over_time(m[5m])",
        "l2_over_time(m[5m])",
        "entropy_over_time(m[5m])",
    ]
    .into_iter()
    .enumerate()
    .map(|(id, query)| (id, candidate(query, accuracy.clone())))
    .collect();
    let roots = share_common_sub_dags(roots);
    let mut first_state = None;
    for (index, root) in &roots {
        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) = &root.operator
        else {
            panic!()
        };
        if let Some(first) = &first_state {
            assert!(
                Rc::ptr_eq(first, summary_input),
                "state must be shared across evaluations"
            );
        } else {
            first_state = Some(Rc::clone(summary_input));
        }
        let Operator::ASAP(ASAPOp::SummaryAgg { family, input, .. }) = &summary_input.operator
        else {
            panic!()
        };
        assert!(matches!(input.item, Some(SummaryInputExpr::Column(_))));
        assert_eq!(input.weight, SummaryInputExpr::Constant(1.0));
        if *index == 1 {
            assert!(matches!(
                query,
                SketchStatistic::PointCount { value: None, .. }
            ));
        } else {
            // Production certifies L2 from layer 0's F₂, but has no
            // calibrated bound for distinct count or entropy.
            assert_eq!(
                DefaultAccuracyModel
                    .local_guarantee(family, query)
                    .map(|g| g.metric),
                (*index == 2).then_some(ErrorMetric::RelativeValue),
                "{query:?}"
            );
        }
        post_asap_dag(root);
    }
}

#[test]
fn uncalibrated_frequency_evaluations_do_not_bypass_accuracy_targets() {
    // An unmeasured heuristic remains inspectable but is never certified or
    // automatically selected for a caller-visible bounded-error result.
    for query in ["entropy_over_time(m[5m])", "l2_over_time(m[5m])"] {
        for target in [
            AccuracyTarget::Exact,
            AccuracyTarget::Epsilon(0.02),
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.02,
                delta: 0.01,
            },
        ] {
            let root = lower_promql(query, target.clone()).unwrap();
            let offered = stage1_candidates(&root)
                .iter()
                .any(|node| reads_univmon(node));
            if target == AccuracyTarget::Exact {
                assert!(!offered);
            } else {
                assert!(offered);
                // Stage 3 never selects the uncalibrated UnivMon estimate.
                let selected = selected_dag(root, target);
                assert!(!OperatorNode::reachable(&selected)
                    .iter()
                    .any(|node| matches!(
                        &node.operator,
                        Operator::ASAP(ASAPOp::SummaryAgg {
                            family: FieldDataType::Sketch(kind, _),
                            ..
                        }) if kind.algorithm() == &SketchAlgorithm::UnivMon
                    )));
            }
        }
    }
}
