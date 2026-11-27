use super::*;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::summary_type::SketchAlgorithm;

/// Values of one sign, which every listed guarantee holds over (DDSketch's
/// needs it).
const ONE_SIGN: Option<ValueSign> = Some(ValueSign::NonNegative);

/// Every listed θ has a theoretical guarantee for the statistic its
/// algorithm is listed for, at failure probability 0.01, in the
/// algorithm's metric; along a list of one structure the error never
/// grows (the lists run weakest first).
#[test]
fn every_listed_parameters_has_a_guarantee_at_one_percent() {
    use asap_types::summary_type::parameter_list;
    let point = SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: Some("1".into()),
    };
    for (algorithm, statistic, metric) in [
        (
            SketchAlgorithm::KLL,
            SketchStatistic::Quantile { q: 0.5 },
            AccuracyMetric::Rank,
        ),
        (
            SketchAlgorithm::DDSketch,
            SketchStatistic::Quantile { q: 0.5 },
            AccuracyMetric::Relative,
        ),
        (
            SketchAlgorithm::KMV,
            SketchStatistic::Cardinality,
            AccuracyMetric::Relative,
        ),
        (
            SketchAlgorithm::CountMinSketch,
            point.clone(),
            AccuracyMetric::ABSOLUTE_STREAM_L1,
        ),
        (
            SketchAlgorithm::CountSketch,
            point,
            AccuracyMetric::ABSOLUTE_STREAM_L2,
        ),
        (
            SketchAlgorithm::CountMinSketchWithHeap,
            SketchStatistic::TopK { k: 10 },
            AccuracyMetric::ABSOLUTE_STREAM_L1,
        ),
    ] {
        let summary_type = FieldDataType::Sketch(algorithm);
        let errors: Vec<f64> = parameter_list(algorithm)
            .iter()
            .map(|parameters| {
                let guarantee = theoretical_guarantee(
                    &summary_type,
                    parameters,
                    &statistic,
                    0.01,
                    ONE_SIGN,
                    None,
                )
                .unwrap_or_else(|| panic!("{algorithm:?} {parameters:?}"));
                assert_eq!(guarantee.spec.accuracy_metric, metric, "{algorithm:?}");
                guarantee.spec.error.evaluate().unwrap()
            })
            .collect();
        assert!(!errors.is_empty(), "{algorithm:?}");
        if matches!(
            algorithm,
            SketchAlgorithm::KLL | SketchAlgorithm::DDSketch | SketchAlgorithm::KMV
        ) {
            assert!(
                errors.windows(2).all(|pair| pair[1] <= pair[0]),
                "{algorithm:?}: {errors:?}"
            );
        }
    }
}

/// One parameter sets both (KLL's k): its rank error grows as the
/// required failure probability shrinks. Separate parameters (CMS width
/// and depth): the bound holds only when the depth fails rarely enough.
#[test]
fn a_guarantee_is_evaluated_at_the_required_failure_probability() {
    let quantile = SketchStatistic::Quantile { q: 0.5 };
    let kll = FieldDataType::Sketch(SketchAlgorithm::KLL);
    let k = SketchParams::Kll { k: 547 };
    let error_at = |delta| {
        theoretical_guarantee(&kll, &k, &quantile, delta, None, None)
            .unwrap()
            .spec
            .error
            .evaluate()
            .unwrap()
    };
    assert_eq!(error_at(1.0), error_at(0.01));
    assert!(error_at(1e-6) > error_at(0.01));
    let cms = FieldDataType::Sketch(SketchAlgorithm::CountMinSketch);
    let point = SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: None,
    };
    let depth = |depth| SketchParams::Cms { width: 272, depth };
    assert!(theoretical_guarantee(&cms, &depth(5), &point, 1e-2, None, None).is_some());
    assert!(theoretical_guarantee(&cms, &depth(5), &point, 1e-4, None, None).is_none());
    assert!(theoretical_guarantee(&cms, &depth(10), &point, 1e-4, None, None).is_some());
    // A statistic the summary type does not answer has no guarantee.
    assert!(
        theoretical_guarantee(&kll, &k, &SketchStatistic::Cardinality, 0.01, None, None).is_none()
    );
}

/// DDSketch's relative error on a quantile holds only over values of one
/// sign: over non-negative or non-positive values at α, at any failure
/// probability; over values of unknown sign, or of no known sign, the
/// guarantee is not certified at any failure probability, though its
/// metric is still relative error.
#[test]
fn a_ddsketch_quantile_is_certified_only_over_values_of_one_sign() {
    let ddsketch = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
    let alpha = SketchParams::DDSketch { alpha: 0.01 };
    let quantile = SketchStatistic::Quantile { q: 0.99 };
    for (sign, certified) in [
        (Some(ValueSign::NonNegative), true),
        (Some(ValueSign::NonPositive), true),
        (Some(ValueSign::Unknown), false),
        (None, false),
    ] {
        let theoretical = theoretical_guarantee(&ddsketch, &alpha, &quantile, 0.01, sign, None);
        assert_eq!(theoretical.is_some(), certified, "{sign:?}");
        let before_check = guarantee_before_failure_probability_check(
            &ddsketch, &alpha, &quantile, 0.01, sign, None,
        )
        .unwrap();
        assert_eq!(before_check.spec.accuracy_metric, AccuracyMetric::Relative);
    }
}
