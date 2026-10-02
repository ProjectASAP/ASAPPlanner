use std::rc::Rc;

use asap_aware_mapping::accuracy::{
    AccuracyModel, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
};
use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::replacement::{default_strategies, search_workload_with_targets};
use asap_aware_mapping::{Replacement, ReplacementStrategy, SketchAlgorithmStrategy, TargetSubDAG};
mod support;
use asap_types::ir::cse::share_common_subtrees;
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::post_asap::{
    AccuracyError, BoundExpr, CompositionOperator, ErrorMetric, FieldDataType, ProbabilityExpr,
    ResultGuarantee, SketchAlgorithm, SketchQuery, SummaryInputExpr,
};
use asap_types::types::AccuracyTarget;
use support::{lower_promql, post_asap_dag};

// Synthetic evidence exercises structural sharing, never runtime accuracy.
struct TestEvidence;
impl AccuracyModel for TestEvidence {
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        if matches!(family, FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::UnivMon)
            && !matches!(query, SketchQuery::PointCount { .. })
        {
            let mut guarantee = ResultGuarantee::exact("SYNTHETIC test evidence; not measured");
            guarantee.metric = ErrorMetric::RelativeValue;
            guarantee.bound = BoundExpr::Constant { value: 0.01 };
            guarantee.failure_probability = ProbabilityExpr::Constant { value: 0.01 };
            Some(guarantee)
        } else {
            DefaultAccuracyModel.local_guarantee(family, query)
        }
    }
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        DefaultAccuracyModel.propagate(op, inputs, local, stats)
    }
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
        DefaultAccuracyModel.satisfies(guarantee, target)
    }
}

fn candidate(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let root = lower_promql(query, accuracy).unwrap();
    SketchAlgorithmStrategy::new_with_planning_inputs(&DefaultCostModel, &TestEvidence, &EqualSplitAllocator)
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .find_map(|candidate| {
            let Replacement::Subtree(node) = candidate.replacement else { return None };
            let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator else { return None };
            matches!(&summary_input.operator, Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                if kind.algorithm() == &SketchAlgorithm::UnivMon).then_some(node)
        }).expect("UnivMon candidate")
}

#[test]
fn four_readouts_share_one_value_frequency_state_and_keep_honest_guarantees() {
    // Equal data, grouping and window produce one state independently of readout.
    let accuracy = AccuracyTarget::Epsilon(0.02);
    let roots: Vec<_> = [
        ("distinct_over_time(m[5m])", accuracy.clone()),
        ("count_over_time(m[5m])", AccuracyTarget::Exact),
        ("l2_over_time(m[5m])", accuracy.clone()),
        ("entropy_over_time(m[5m])", accuracy),
    ]
    .into_iter()
    .enumerate()
    .map(|(id, (query, accuracy))| (id, candidate(query, accuracy)))
    .collect();
    let roots = share_common_subtrees(roots);
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
                "state must be shared across readouts"
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
            assert!(matches!(query, SketchQuery::PointCount { value: None, .. }));
            assert!(root.guarantee.as_ref().is_some_and(|g| g.is_exact()));
        } else {
            assert!(!root.guarantee.as_ref().unwrap().is_exact());
            let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &summary_input.operator
            else {
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
fn uncalibrated_frequency_readouts_do_not_bypass_accuracy_targets() {
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
            let candidates = SketchAlgorithmStrategy::default_cost_model()
                .replacements(&TargetSubDAG::new(&root));
            let unknown = candidates
                .iter()
                .filter(|candidate| {
                    matches!(
                        &candidate.replacement,
                        Replacement::Subtree(node)
                            if matches!(&node.operator, Operator::ASAP(ASAPOp::SummaryEstimate { .. }))
                                && node.guarantee.is_none()
                                && candidate.has_missing_accuracy_evidence()
                    )
                })
                .count();
            if target == AccuracyTarget::Exact {
                assert_eq!(unknown, 0);
            } else {
                assert!(unknown > 0);
                let space = search_workload_with_targets(
                    vec![("q", Rc::clone(&root), Some(target))],
                    &default_strategies(),
                    &DefaultAccuracyModel,
                );
                assert!(space
                    .candidates_for_target(&space.roots[0].1)
                    .unwrap()
                    .candidates
                    .iter()
                    .any(|candidate| candidate.has_missing_accuracy_evidence()));
                assert!(!space
                    .global_selection(&DefaultCostModel)
                    .for_target(&space.roots[0].1)
                    .unwrap()
                    .chosen
                    .is_some_and(|candidate| candidate.has_missing_accuracy_evidence()));
            }
        }
    }
}
