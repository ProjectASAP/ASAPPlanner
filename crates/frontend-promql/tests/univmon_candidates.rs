use std::rc::Rc;

use asap_aware_mapping::accuracy::{
    AccuracyModel, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
};
use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::{Replacement, ReplacementStrategy, SketchAlgorithmStrategy, TargetSubDAG};
use asap_frontend_promql::lower_promql;
use asap_types::post_asap::{
    compile_executable_dag, cse::share_common_summary_subtrees, AccuracyError, BoundExpr,
    CompositionOperator, ErrorMetric, ProbabilityExpr, ResultGuarantee, SketchAlgorithm,
    SketchQuery, SummaryExpr, SummaryFamilyType, SummaryInputExpr, SummaryNode,
};
use asap_types::types::AccuracyTarget;

// Synthetic evidence exercises structural sharing, never runtime accuracy.
struct TestEvidence;
impl AccuracyModel for TestEvidence {
    fn local_guarantee(
        &self,
        family: &SummaryFamilyType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        if matches!(family, SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::UnivMon)
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

fn candidate(query: &str, accuracy: AccuracyTarget) -> Rc<SummaryNode> {
    let root = lower_promql(query, accuracy).unwrap();
    SketchAlgorithmStrategy::with_models(&DefaultCostModel, &TestEvidence, &EqualSplitAllocator)
        .replacements(&TargetSubDAG::new(&Rc::new(root)))
        .into_iter()
        .find_map(|candidate| {
            let Replacement::Summary(node) = candidate.replacement else { return None };
            let SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr else { return None };
            matches!(&summary_input.expr, SummaryExpr::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. }
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
    let roots = share_common_summary_subtrees(roots);
    let mut first_state = None;
    for (index, root) in &roots {
        let SummaryExpr::SummaryEstimate {
            summary_input,
            query,
            ..
        } = &root.expr
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
        let SummaryExpr::SummaryAgg { input, .. } = &summary_input.expr else {
            panic!()
        };
        assert!(matches!(input.item, Some(SummaryInputExpr::Column(_))));
        assert_eq!(input.weight, SummaryInputExpr::Constant(1.0));
        if *index == 1 {
            assert!(matches!(query, SketchQuery::PointCount { value: None, .. }));
            assert!(root.guarantee.as_ref().is_some_and(|g| g.is_exact()));
        } else {
            assert!(!root.guarantee.as_ref().unwrap().is_exact());
            let SummaryExpr::SummaryAgg { family, .. } = &summary_input.expr else {
                panic!()
            };
            assert!(
                DefaultAccuracyModel
                    .local_guarantee(family, query)
                    .is_none(),
                "production has no calibrated error bound"
            );
        }
        compile_executable_dag(root).unwrap();
    }
}

#[test]
fn uncalibrated_frequency_readouts_do_not_bypass_accuracy_targets() {
    // Parser support must not make an unmeasured heuristic eligible for a
    // caller-visible bounded-error result under the production default model.
    for query in ["entropy_over_time(m[5m])", "l2_over_time(m[5m])"] {
        for target in [
            AccuracyTarget::Exact,
            AccuracyTarget::Epsilon(0.02),
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.02,
                delta: 0.01,
            },
        ] {
            let root = Rc::new(lower_promql(query, target).unwrap());
            let candidates = SketchAlgorithmStrategy::default_cost_model()
                .replacements(&TargetSubDAG::new(&root));
            assert!(candidates.iter().all(|candidate| !matches!(&candidate.replacement,
                Replacement::Summary(node) if matches!(&node.expr, SummaryExpr::SummaryEstimate { .. }))));
        }
    }
}
