//! Numeric regression fixtures: actual PromQL lowering plus numeric update/evaluation checks.
//! The count/sum interpreter below verifies planner update semantics, not a deployed backend.
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::logical_candidates;
use asap_integration_tests::post_asap::post_asap_dag;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::post_asap::{ExactKind, FieldDataType, SummaryInputExpr, SummaryUpdate};
use asap_types::pre_asap::{ColumnRef, Reduction};
use asap_types::types::AccuracyTarget;
use std::rc::Rc;

/// The first logical candidate (in enumeration order) with an ASAP operator,
/// else the exact one.
fn plan(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let pre = lower_promql(query, accuracy.clone()).unwrap();
    let candidates = logical_candidates(pre, accuracy);
    candidates
        .iter()
        .find(|node| node.contains_asap())
        .unwrap_or(&candidates[0])
        .clone()
}
fn aggregate(node: &OperatorNode) -> (&FieldDataType, &SummaryUpdate, &Reduction) {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryAgg {
            family,
            input,
            reduction,
            ..
        }) => (family, input, reduction),
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => aggregate(summary_input),
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => aggregate(child),
        // A value operation (Project/Filter/Sort/Limit/...) over the state.
        Operator::NonASAP(op) if op.children().len() == 1 && node.contains_asap() => {
            aggregate(op.children()[0])
        }
        other => panic!("not a maintained accumulator: {other:?}"),
    }
}
fn contribution(family: &FieldDataType, update: &SummaryUpdate, value: f64) -> f64 {
    if matches!(family, FieldDataType::ExactAggregate(ExactKind::Count, _)) {
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
        FieldDataType::ExactAggregate(ExactKind::Count, _)
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
    post_asap_dag(&node);
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
        assert!(matches!(family, FieldDataType::ExactAggregate(k, _) if *k == kind));
        assert_eq!(*reduction, Reduction::PerEntity);
        for value in [1., 0., 3., -3.] {
            let got: f64 = (0..10).map(|_| contribution(family, update, value)).sum();
            assert_eq!(got, if is_count { 10. } else { value * 10. });
        }
        post_asap_dag(&node);
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
        assert!(matches!(family, FieldDataType::ExactAggregate(k, _) if *k == kind));
        assert!(node.guarantee.as_ref().unwrap().is_exact());
        post_asap_dag(&node);
    }
}

/// A finite, nonzero approximate denominator does not certify a ratio bound.
#[test]
fn checked_ratio_must_not_certify_cross_zero_interpolation() {
    let node = plan(
        "quantile_over_time(0.5, data[5m]) / quantile_over_time(0.9, data[5m])",
        AccuracyTarget::Epsilon(0.01),
    );
    assert!(
        matches!(node.operator, Operator::NonASAP(NonASAPOp::BinaryOp { .. }))
            && node.contains_asap(),
        "direct quantile ratio should remain an available candidate"
    );
    assert!(node.guarantee.is_none());
    post_asap_dag(&node);
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

/// Rewriting the inner average (its own target, as sum and count
/// accumulators) must not invalidate an otherwise usable outer sketch.
#[test]
fn quantile_over_temporal_average_keeps_a_legal_candidate() {
    for query in [
        "quantile(0.9, avg_over_time(a[5m]))",
        "quantile(0.9, avg_over_time(a[5m]) + avg_over_time(b[5m]))",
    ] {
        let accuracy = AccuracyTarget::Epsilon(0.01);
        let pre = lower_promql(query, accuracy.clone()).unwrap();
        let node = logical_candidates(pre, accuracy)
            .into_iter()
            .find(|node| {
                OperatorNode::reachable(node).iter().any(|n| {
                    matches!(
                        n.asap(),
                        Some(ASAPOp::SummaryAgg {
                            family: FieldDataType::Sketch(..),
                            ..
                        })
                    )
                })
            })
            .unwrap_or_else(|| panic!("outer sketch candidate must survive: {query}"));
        post_asap_dag(&node);
    }
}

#[test]
fn sketch_counts_use_unit_weights_and_signed_sums_keep_value_weights() {
    use asap_types::post_asap::{NonNegativeWeightProof, SketchAlgorithm, WeightDomain};
    for is_count in [true, false] {
        let query = if is_count {
            "topk(1, count_over_time(up[5m]))"
        } else {
            "topk(1, sum_over_time(up[5m]))"
        };
        let accuracy = AccuracyTarget::Epsilon(0.01);
        let pre = lower_promql(query, accuracy.clone()).unwrap();
        let candidates = logical_candidates(pre, accuracy);
        // The heap state each summarized candidate builds.
        let heap = |node: &Rc<OperatorNode>| {
            OperatorNode::reachable(node).into_iter().find(|n| {
                matches!(
                    n.asap(),
                    Some(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(..),
                        ..
                    })
                )
            })
        };
        let wanted = if is_count {
            SketchAlgorithm::CmsWithHeap
        } else {
            SketchAlgorithm::CountSketchWithHeap
        };
        let (node, state) = candidates
            .iter()
            .find_map(|node| {
                let state = heap(node)?;
                let (family, _, _) = aggregate(&state);
                matches!(family, FieldDataType::Sketch(kind, _) if kind.algorithm() == &wanted)
                    .then_some((node, state))
            })
            .expect("weighted sketch candidate");
        let (family, update, _) = aggregate(&state);
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
            // A signed sum's Count-Min heap carries no non-negative proof,
            // so plan selection rejects it.
            for state in candidates.iter().filter_map(heap) {
                let (family, update, _) = aggregate(&state);
                if matches!(family, FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::CmsWithHeap)
                {
                    assert!(!matches!(
                        update.weight_domain,
                        WeightDomain::NonNegative { .. }
                    ));
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
        post_asap_dag(node);
    }
}
