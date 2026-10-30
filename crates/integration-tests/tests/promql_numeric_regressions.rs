//! Numeric regression fixtures: actual PromQL lowering plus numeric update/evaluation checks.
//! The count/sum interpreter below verifies planner update semantics, not a deployed backend.
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::physical_export::physical_asap_dag;
use asap_integration_tests::planning::{explicit_local_candidates, explicit_summary_candidate};
use asap_types::ir::operator::Reduction;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{ExactKind, FieldDataType, SummaryInputExpr, SummaryUpdate};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use std::rc::Rc;

/// The first explicitly generated local-rule fixture containing an ASAP
/// operator, or its exact computation when no summary is applicable.
fn plan(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let pre = lower_promql(query, accuracy.clone()).unwrap();
    let candidates = explicit_local_candidates(pre);
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
    let node = plan("count(up)", AccuracyTarget::exact());
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
    physical_asap_dag(&node);
}

/// Ten samples give count ten, whereas sum retains the signed sample values.
#[test]
fn window_counts_and_sums_distinguish_one_zero_three_and_negative_values() {
    for (query, kind, is_count) in [
        ("count_over_time(up[5m])", ExactKind::Count, true),
        ("sum_over_time(up[5m])", ExactKind::Sum, false),
    ] {
        let node = plan(query, AccuracyTarget::exact());
        let (family, update, reduction) = aggregate(&node);
        assert!(matches!(family, FieldDataType::ExactAggregate(k, _) if *k == kind));
        assert_eq!(*reduction, Reduction::PerEntity);
        for value in [1., 0., 3., -3.] {
            let got: f64 = (0..10).map(|_| contribution(family, update, value)).sum();
            assert_eq!(got, if is_count { 10. } else { value * 10. });
        }
        physical_asap_dag(&node);
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
        let node = plan(query, AccuracyTarget::exact());
        let (family, _, _) = aggregate(&node);
        assert!(matches!(family, FieldDataType::ExactAggregate(k, _) if *k == kind));
        assert!(node.guarantee.as_ref().unwrap().is_exact());
        physical_asap_dag(&node);
    }
}

/// The PromQL quantile over a DDSketch: the linear interpolation at rank
/// `q (n - 1)` between the sketch's two neighbouring order statistics.
fn interpolated_quantile(sketch: &asap_sketchlib::DdSketch, q: f64) -> f64 {
    sketch.quantile_interpolated(q).unwrap().unwrap()
}

/// A finite, nonzero approximate denominator does not certify a ratio bound.
#[test]
fn checked_ratio_must_not_certify_cross_zero_interpolation() {
    let root = lower_promql(
        "quantile_over_time(0.5, data[5m]) / quantile_over_time(0.9, data[5m])",
        AccuracyTarget::max_error(0.01),
    )
    .unwrap();
    let node = explicit_summary_candidate(root, asap_types::ir::schema::SketchAlgorithm::DDSketch);
    assert_eq!(OperatorNode::reachable(&node).iter().filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == asap_types::ir::schema::SketchAlgorithm::DDSketch)).count(), 2,
        "the signed interpolation counterexample requires two explicit DDSketch builds");
    assert!(
        matches!(node.operator, Operator::NonASAP(NonASAPOp::BinaryOp { .. }))
            && node.contains_asap(),
        "direct quantile ratio should remain an available candidate"
    );
    assert!(node.guarantee.is_none(), "{:?}", node.guarantee);
    use asap_accuracy::propagation::bind_guarantees;
    use asap_accuracy::test_support::raw_data_samples_sign_statistic;
    let root = asap_types::ir::QueryRoot::Operator(node.clone());
    assert!(bind_guarantees(&root, &Clone::clone).is_none());
    let proven = bind_guarantees(&root, &|guarantee| {
        guarantee.bind(&|statistic| {
            (statistic == raw_data_samples_sign_statistic("data")).then_some(1.0)
        })
    })
    .expect("a declared nonnegative source recovers the ratio contract");
    assert!(proven.spec.error.evaluate().unwrap() <= 0.01, "{proven:?}");
    assert!(
        node.guarantee.is_none(),
        "conditional sizing must not attach a hypothetical sign proof"
    );
    physical_asap_dag(&node);
    // Keep the actual signed-sketch counterexample: division guards alone pass
    // even though the quantile interpolation does not preserve relative error.
    // The sketch reads its minimum and maximum exactly, so the interpolated
    // order statistics are interior ones.
    let alpha = (0.01 - 8.0 * f64::EPSILON) / 2.01;
    let values = [-2., -1., 1.011, 2.];
    let estimate = |q| {
        let mut sketch = asap_sketchlib::DdSketch::new(alpha);
        for value in values {
            sketch.try_add(&value).unwrap();
        }
        interpolated_quantile(&sketch, q)
    };
    let numerator = estimate(0.5);
    let denominator = estimate(0.9);
    let got = numerator / denominator;
    assert!(
        numerator.is_finite() && denominator.is_finite() && denominator != 0. && got.is_normal()
    );
    let exact_quantile = |q: f64| {
        let rank = q * (values.len() - 1) as f64;
        let (lower, upper) = (rank.floor() as usize, rank.ceil() as usize);
        values[lower] * (1. - rank.fract()) + values[upper] * rank.fract()
    };
    let want = exact_quantile(0.5) / exact_quantile(0.9);
    let error = (got - want).abs() / want.abs();
    assert!(
        error > 0.01,
        "fixture must expose cancellation beyond the requested budget"
    );
}

/// Interpolated DDS quantiles require a source-domain proof; extrema do not
/// interpolate, and a computed value must not inherit its source's sign.
#[test]
fn ddsketch_interpolation_requires_a_nonnegative_domain() {
    use asap_accuracy::propagation::bind_guarantees;
    use asap_accuracy::test_support::raw_data_samples_sign_statistic;
    use asap_types::ir::{schema::SketchAlgorithm, QueryRoot};
    for (query, endpoint, raw_samples) in [
        ("quantile_over_time(0, data[5m])", true, true),
        ("quantile_over_time(1, data[5m])", true, true),
        ("quantile_over_time(0.5, data[5m])", false, true),
        ("quantile(0.5, data - 100)", false, false),
    ] {
        let node = explicit_summary_candidate(
            lower_promql(query, AccuracyTarget::max_error(0.01)).unwrap(),
            SketchAlgorithm::DDSketch,
        );
        let bound = node
            .guarantee
            .as_ref()
            .and_then(|g| g.spec.error.evaluate());
        assert_eq!(bound.is_some(), endpoint, "{query}");
        let proven = bind_guarantees(&QueryRoot::Operator(node), &|guarantee| {
            guarantee.bind(&|statistic| {
                (statistic == raw_data_samples_sign_statistic("data")).then_some(1.0)
            })
        });
        assert_eq!(
            proven.and_then(|g| g.spec.error.evaluate()).is_some(),
            raw_samples,
            "{query}"
        );
    }
}

/// Rewriting the inner average (its own target, as sum and count
/// accumulators) must not invalidate an otherwise usable outer sketch.
#[test]
fn quantile_over_temporal_average_keeps_a_legal_candidate() {
    for query in [
        "quantile(0.9, avg_over_time(a[5m]))",
        "quantile(0.9, avg_over_time(a[5m]) + avg_over_time(b[5m]))",
    ] {
        let accuracy = AccuracyTarget::max_error(0.01);
        let pre = lower_promql(query, accuracy.clone()).unwrap();
        let node = explicit_local_candidates(pre)
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
        physical_asap_dag(&node);
    }
}

#[test]
fn sketch_counts_use_unit_weights_and_signed_sums_keep_value_weights() {
    use asap_types::ir::schema::{NonNegativeWeightProof, SketchAlgorithm, WeightDomain};
    for is_count in [true, false] {
        let query = if is_count {
            "topk(1, count_over_time(up[5m]))"
        } else {
            "topk(1, sum_over_time(up[5m]))"
        };
        let accuracy = AccuracyTarget::max_error(0.01);
        let pre = lower_promql(query, accuracy.clone()).unwrap();
        let candidates = explicit_local_candidates(pre);
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
            let key = asap_sketchlib::DataInput::Str("a");
            let mut cms: asap_sketchlib::CountMinF64 =
                asap_sketchlib::CountMinSketch::with_dimensions(5, 128);
            let mut cs =
                asap_sketchlib::CountSketch::<asap_sketchlib::Vector2D<f64>>::with_dimensions(
                    5, 128,
                );
            for _ in 0..10 {
                let weight = contribution(family, update, value);
                if is_count {
                    cms.update_weighted(&key, weight);
                } else {
                    cs.update_weighted(&key, weight);
                }
            }
            let got = if is_count {
                cms.estimate(&key)
            } else {
                cs.estimate(&key)
            };
            assert_eq!(got, if is_count { 10. } else { 10. * value });
        }
        physical_asap_dag(node);
    }
}

// A rejected composition must fail the assertion helper without relying on a planner bug.
#[test]
#[should_panic(expected = "does not compose")]
fn logical_candidates_fails_on_a_candidate_that_does_not_compose() {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::PerComputationCandidateError;
    asap_integration_tests::planning::require_composed_operator_candidates(vec![
        asap_plan_selection::search::LogicalCandidate {
            id: "invalid-composition".into(),
            roots: Err(PerComputationCandidateError::InvalidChoice),
        },
    ]);
}
