//! Query text through summary selection: counts use observations, never value weights.
use std::rc::Rc;

use asap_aware_mapping::{Replacement, ReplacementStrategy, SketchAlgorithmStrategy, TargetSubDAG};
use asap_frontend_promql::lower_promql;
use asap_types::post_asap::{
    compile_executable_dag, ExactKind, ExecutableOperatorPayload, NonNegativeWeightProof,
    SketchAlgorithm, SummaryExpr, SummaryFamilyType, SummaryInputExpr, WeightDomain,
};
use asap_types::types::AccuracyTarget;

// Exact series and temporal counts must select a count accumulator, not distinct or sum.
#[test]
fn exact_counts_select_count_accumulators() {
    for query in ["count(up)", "count by(job)(up)", "count_over_time(up[5m])"] {
        let root = Rc::new(lower_promql(query, AccuracyTarget::Exact).unwrap());
        let candidates =
            SketchAlgorithmStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
        assert!(
            candidates.iter().any(|candidate| {
                matches!(&candidate.replacement, Replacement::Summary(node)
                if matches!(&node.expr, SummaryExpr::SummaryAgg {
                    family: SummaryFamilyType::ExactAggregate(ExactKind::Count, _), .. }))
            }),
            "{query}: {candidates:?}"
        );
    }
}

// CMS/CountSketch count updates must stay +1 even for zero or negative samples.
#[test]
fn frequency_count_candidates_use_unit_weights() {
    for query in ["count_over_time(up[5m])", "count(up)"] {
        let root = Rc::new(lower_promql(query, AccuracyTarget::Epsilon(0.02)).unwrap());
        let candidates =
            SketchAlgorithmStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
        let mut algorithms = Vec::new();
        for candidate in &candidates {
            let Replacement::Summary(node) = &candidate.replacement else {
                continue;
            };
            let SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr else {
                continue;
            };
            let SummaryExpr::SummaryAgg {
                family: SummaryFamilyType::Sketch(kind, _),
                input,
                ..
            } = &summary_input.expr
            else {
                continue;
            };
            assert!(
                !matches!(kind.algorithm(), SketchAlgorithm::Hll),
                "{query}: {node:?}"
            );
            if !matches!(
                kind.algorithm(),
                SketchAlgorithm::Cms | SketchAlgorithm::CountSketch
            ) {
                continue;
            }
            let dag = compile_executable_dag(node).unwrap();
            assert!(
                dag.nodes.iter().any(|node| matches!(
                    &node.payload,
                    ExecutableOperatorPayload::SummaryAgg { input: actual, .. } if actual == input
                )),
                "executable DAG must preserve the count update contract"
            );
            algorithms.push(kind.algorithm().clone());
            assert!(input.item.is_some(), "frequency keys must be explicit");
            assert_eq!(
                input.weight,
                SummaryInputExpr::Constant(1.0),
                "{query}: {node:?}"
            );
            assert_eq!(
                input.weight_domain,
                WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount
                }
            );
        }
        assert!(
            algorithms.contains(&SketchAlgorithm::Cms),
            "{query}: {candidates:?}"
        );
        assert!(
            algorithms.contains(&SketchAlgorithm::CountSketch),
            "{query}: {candidates:?}"
        );
    }
}
