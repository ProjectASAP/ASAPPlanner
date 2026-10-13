use std::rc::Rc;

use asap_aware_mapping::accuracy::{AccuracyModel, DefaultAccuracyModel};
mod support;
use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::post_asap::{FieldDataType, SketchAlgorithm, SketchStatistic, SummaryInputExpr};
use asap_types::types::AccuracyTarget;
use support::{logical_candidates, lower_promql, post_asap_dag, run_stage1_to_stage3};

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
    logical_candidates(root, accuracy)
        .into_iter()
        .find_map(|node| univmon_estimate(&node))
        .expect("UnivMon candidate")
}

#[test]
fn four_evaluations_share_one_value_frequency_state_and_keep_honest_guarantees() {
    // Equal data, grouping and window produce one state independently of
    // evaluation. Pass 1 offers UnivMon for an approximate count only.
    let accuracy = AccuracyTarget::Epsilon(0.02);
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
        let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) = &summary_input.operator else {
            panic!()
        };
        assert!(matches!(input.item, Some(SummaryInputExpr::Column(_))));
        assert_eq!(input.weight, SummaryInputExpr::Constant(1.0));
        if *index == 1 {
            assert!(matches!(
                query,
                SketchStatistic::PointCount { value: None, .. }
            ));
            assert!(root.guarantee.as_ref().is_some_and(|g| g.is_exact()));
        } else {
            // No calibrated bound, so composition certifies nothing.
            assert!(root.guarantee.is_none());
            let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &summary_input.operator else {
                panic!()
            };
            assert!(
                DefaultAccuracyModel
                    .local_guarantee(family, query)
                    .is_none(),
                "production has no calibrated error bound"
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
            let run = run_stage1_to_stage3(root, target.clone());
            let unknown: Vec<_> = run
                .stage2_physical_asap_optimization
                .iter()
                .filter(|candidate| {
                    let asap_types::ir::QueryRoot::Operator(node) = &candidate.roots[0] else {
                        return false;
                    };
                    univmon_estimate(node).is_some() && node.guarantee.is_none()
                })
                .collect();
            if target == AccuracyTarget::Exact {
                assert!(unknown.is_empty(), "{query}");
            } else {
                assert!(!unknown.is_empty(), "{query}");
                let selected = &run.stage3_plan_selection.as_ref().unwrap().selected;
                assert!(
                    unknown.iter().all(|candidate| &candidate.id != selected),
                    "{query}"
                );
            }
        }
    }
}
