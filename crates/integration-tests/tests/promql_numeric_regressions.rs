//! Numeric regression fixtures: actual PromQL lowering plus numeric update/readout checks.
//! The count/sum interpreter below verifies planner update semantics, not a deployed backend.
use asap_aware_mapping::{Replacement, ReplacementStrategy, SketchAlgorithmStrategy, TargetSubDAG};
use asap_integration_tests::fixtures::lower_promql;
use asap_types::post_asap::{
    compile_executable_dag, ExactKind, SummaryExpr, SummaryFamilyType, SummaryInputExpr,
    SummaryNode, SummaryUpdate,
};
use asap_types::pre_asap::{ColumnRef, Reduction};
use asap_types::types::AccuracyTarget;
use std::rc::Rc;

fn plan(query: &str, accuracy: AccuracyTarget) -> Rc<SummaryNode> {
    let pre = Rc::new(lower_promql(query, accuracy).unwrap());
    SketchAlgorithmStrategy::default_cost_model()
        .replacements(&TargetSubDAG::new(&pre))
        .into_iter()
        .find_map(|r| match r.replacement {
            Replacement::Summary(n) => Some(n),
            _ => None,
        })
        .unwrap_or_else(|| asap_aware_mapping::replacement::keep_pre_asap(&pre).unwrap())
}
fn aggregate(node: &SummaryNode) -> (&SummaryFamilyType, &SummaryUpdate, &Reduction) {
    match &node.expr {
        SummaryExpr::SummaryAgg {
            family,
            input,
            reduction,
            ..
        } => (family, input, reduction),
        SummaryExpr::SummaryEstimate { summary_input, .. } => aggregate(summary_input),
        SummaryExpr::ValueOperation { child, .. } => aggregate(child),
        other => panic!("not a maintained accumulator: {other:?}"),
    }
}
fn contribution(family: &SummaryFamilyType, update: &SummaryUpdate, value: f64) -> f64 {
    if matches!(
        family,
        SummaryFamilyType::ExactAggregate(ExactKind::Count, _)
    ) {
        return 1.;
    }
    match update.weight {
        SummaryInputExpr::Constant(v) => v,
        SummaryInputExpr::Column(ColumnRef::SampleValue) => value,
        ref other => panic!("unexpected update {other:?}"),
    }
}

/// Target count depends on series multiplicity, never distinct values or health.
#[test]
fn count_up_counts_targets_even_when_values_repeat_or_change_sign() {
    let node = plan("count(up)", AccuracyTarget::Exact);
    let (family, update, _) = aggregate(&node);
    assert!(matches!(
        family,
        SummaryFamilyType::ExactAggregate(ExactKind::Count, _)
    ));
    for values in [[1., 1., 1.], [1., 1., 0.], [0., 0., 0.], [-1., -1., -1.]] {
        assert_eq!(
            values
                .into_iter()
                .map(|v| contribution(family, update, v))
                .sum::<f64>(),
            3.
        );
    }
    compile_executable_dag(&node).unwrap();
}

/// Ten samples give count ten, whereas sum retains the signed sample values.
#[test]
fn window_counts_and_sums_distinguish_one_zero_three_and_negative_values() {
    for (query, kind, is_count) in [
        ("count_over_time(up[5m])", ExactKind::Count, true),
        ("sum_over_time(up[5m])", ExactKind::Sum, false),
    ] {
        let node = plan(query, AccuracyTarget::Exact);
        let (family, update, reduction) = aggregate(&node);
        assert!(matches!(family, SummaryFamilyType::ExactAggregate(k, _) if *k == kind));
        assert_eq!(*reduction, Reduction::PerEntity);
        for value in [1., 0., 3., -3.] {
            let got: f64 = (0..10).map(|_| contribution(family, update, value)).sum();
            assert_eq!(got, if is_count { 10. } else { value * 10. });
        }
        compile_executable_dag(&node).unwrap();
    }
}

/// Exact summary candidates must exist, rather than merely retaining the original query.
#[test]
fn sum_rate_and_increase_have_real_exact_accumulator_nodes() {
    for (query, kind) in [
        ("sum by(job)(up)", ExactKind::Sum),
        ("rate(requests_total[5m])", ExactKind::Rate),
        ("increase(requests_total[5m])", ExactKind::Increase),
    ] {
        let node = plan(query, AccuracyTarget::Exact);
        let (family, _, _) = aggregate(&node);
        assert!(matches!(family, SummaryFamilyType::ExactAggregate(k, _) if *k == kind));
        assert!(node.guarantee.as_ref().unwrap().is_exact());
        compile_executable_dag(&node).unwrap();
    }
}

/// A finite, nonzero approximate denominator does not prove a valid relative quantile bound.
#[test]
fn checked_ratio_must_not_certify_cross_zero_interpolation() {
    let node = plan(
        "quantile_over_time(0.5, data[5m]) / quantile_over_time(0.9, data[5m])",
        AccuracyTarget::Epsilon(0.01),
    );
    assert!(
        matches!(node.expr, SummaryExpr::KeepPreAsap(_)),
        "unproved ratio must retain native execution"
    );
    compile_executable_dag(&node).unwrap();
    // Keep the actual signed-sketch counterexample: division guards alone pass
    // even though the quantile interpolation does not preserve relative error.
    let alpha = (0.01 - 8.0 * f64::EPSILON) / 2.01;
    let estimate = |q| {
        let mut sketch = asap_sketchlib::DdSketch::new(alpha);
        for value in [-1., 1.011] {
            sketch.try_update(value).unwrap();
        }
        sketch.quantile_interpolated(q).unwrap()
    };
    let numerator = estimate(0.5);
    let denominator = estimate(0.9);
    let got = numerator / denominator;
    assert!(
        numerator.is_finite() && denominator.is_finite() && denominator != 0. && got.is_normal()
    );
    let exact_quantile = |q: f64| -(1. - q) + 1.011 * q;
    let want = exact_quantile(0.5) / exact_quantile(0.9);
    let error = (got - want).abs() / want.abs();
    assert!(
        error > 0.01,
        "fixture must expose cancellation beyond the requested budget"
    );
}

/// A new conditional average rewrite must not invalidate an otherwise usable outer sketch.
#[test]
fn quantile_over_temporal_average_keeps_a_legal_candidate() {
    for query in [
        "quantile(0.9, avg_over_time(a[5m]))",
        "quantile(0.9, avg_over_time(a[5m]) + avg_over_time(b[5m]))",
    ] {
        let node = plan(query, AccuracyTarget::Epsilon(0.01));
        assert!(
            !matches!(node.expr, SummaryExpr::KeepPreAsap(_)),
            "outer sketch candidate must survive: {query}"
        );
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr else {
            panic!("outer sketch readout")
        };
        let SummaryExpr::SummaryAgg { child, .. } = &summary_input.expr else {
            panic!("outer sketch state")
        };
        assert!(
            matches!(child.expr, SummaryExpr::KeepPreAsap(_)),
            "guarded expression must retain native maintenance input"
        );
        compile_executable_dag(&node).unwrap();
    }
}

struct OneKeyTopKEvidence;
impl asap_aware_mapping::accuracy::AccuracyEvidenceProvider for OneKeyTopKEvidence {
    fn propagation_stats(
        &self,
        op: &asap_types::post_asap::CompositionOperator,
        _family: &SummaryFamilyType,
        _query: Option<&asap_types::post_asap::SketchQuery>,
    ) -> asap_aware_mapping::accuracy::PropagationStats {
        // Single-key fixture: no excluded keys; bounds cover every value below.
        if matches!(
            op,
            asap_types::post_asap::CompositionOperator::TopKSelection
        ) {
            asap_aware_mapping::accuracy::PropagationStats {
                topk_selected_lower_bound: Some(-1000.),
                topk_excluded_upper_bound: Some(-1001.),
                topk_interval_failure_probability: Some(0.001),
                ..Default::default()
            }
        } else {
            Default::default()
        }
    }
}

#[test]
fn sketch_counts_use_unit_weights_and_signed_sums_keep_value_weights() {
    use asap_aware_mapping::accuracy::{DefaultAccuracyModel, EqualSplitAllocator};
    use asap_aware_mapping::cost_model::DefaultCostModel;
    use asap_types::post_asap::{NonNegativeWeightProof, SketchAlgorithm, WeightDomain};
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &OneKeyTopKEvidence,
    );
    for is_count in [true, false] {
        let query = if is_count {
            "topk(1, count_over_time(up[5m]))"
        } else {
            "topk(1, sum_over_time(up[5m]))"
        };
        let pre = Rc::new(lower_promql(query, AccuracyTarget::Epsilon(0.01)).unwrap());
        let candidates = strategy.replacements(&TargetSubDAG::new(&pre));
        let wanted = if is_count {
            SketchAlgorithm::CmsWithHeap
        } else {
            SketchAlgorithm::CountSketchWithHeap
        };
        let node = candidates
            .iter()
            .find_map(|c| {
                let Replacement::Summary(node) = &c.replacement else {
                    return None;
                };
                let (family, _, _) = aggregate(node);
                matches!(family, SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &wanted)
                    .then_some(node)
            })
            .expect("weighted sketch candidate");
        let (family, update, _) = aggregate(node);
        if is_count {
            assert_eq!(update.weight, SummaryInputExpr::Constant(1.));
            assert_eq!(
                update.weight_domain,
                WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount
                }
            );
        } else {
            assert_eq!(
                update.weight,
                SummaryInputExpr::Column(ColumnRef::SampleValue)
            );
            for c in &candidates {
                if let Replacement::Summary(n) = &c.replacement {
                    assert!(
                        !matches!(aggregate(n).0, SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::CmsWithHeap)
                    );
                }
            }
        }
        for value in [1., 0., 3., -3.] {
            let mut cms =
                asap_sketchlib::message_pack_format::portable::countminsketch::new_sketchlib_cms(
                    5, 128,
                );
            let mut cs =
                asap_sketchlib::message_pack_format::portable::countsketch::CountSketch::new(
                    5, 128,
                );
            for _ in 0..10 {
                let weight = contribution(family, update, value);
                if is_count {
                    cms.insert_many(&asap_sketchlib::DataInput::String("a".into()), weight);
                } else {
                    cs.update("a", weight);
                }
            }
            let got = if is_count {
                cms.estimate(&asap_sketchlib::DataInput::String("a".into()))
            } else {
                cs.estimate("a")
            };
            assert_eq!(got, if is_count { 10. } else { 10. * value });
        }
        compile_executable_dag(node).unwrap();
    }
}
