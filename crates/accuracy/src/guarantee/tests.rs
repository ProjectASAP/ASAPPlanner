use super::*;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::summary_type::SketchAlgorithm;

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
            SketchAlgorithm::Kll,
            SketchStatistic::Quantile { q: 0.5 },
            AccuracyMetric::Rank,
        ),
        (
            SketchAlgorithm::DDSketch,
            SketchStatistic::Quantile { q: 0.5 },
            AccuracyMetric::Relative,
        ),
        (
            SketchAlgorithm::Kmv,
            SketchStatistic::Cardinality,
            AccuracyMetric::Relative,
        ),
        (
            SketchAlgorithm::Cms,
            point.clone(),
            AccuracyMetric::ABSOLUTE_STREAM_L1,
        ),
        (
            SketchAlgorithm::CountSketch,
            point,
            AccuracyMetric::ABSOLUTE_STREAM_L2,
        ),
        (
            SketchAlgorithm::CmsWithHeap,
            SketchStatistic::TopK { k: 10 },
            AccuracyMetric::ABSOLUTE_STREAM_L1,
        ),
    ] {
        let summary_type = FieldDataType::Sketch(algorithm);
        let errors: Vec<f64> = parameter_list(algorithm)
            .iter()
            .map(|parameters| {
                let guarantee = theoretical_guarantee(&summary_type, parameters, &statistic, 0.01)
                    .unwrap_or_else(|| panic!("{algorithm:?} {parameters:?}"));
                assert_eq!(guarantee.spec.accuracy_metric, metric, "{algorithm:?}");
                guarantee.spec.error.evaluate().unwrap()
            })
            .collect();
        assert!(!errors.is_empty(), "{algorithm:?}");
        if matches!(
            algorithm,
            SketchAlgorithm::Kll | SketchAlgorithm::DDSketch | SketchAlgorithm::Kmv
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
    let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let k = SketchParams::Kll { k: 547 };
    let error_at = |delta| {
        theoretical_guarantee(&kll, &k, &quantile, delta)
            .unwrap()
            .spec
            .error
            .evaluate()
            .unwrap()
    };
    assert_eq!(error_at(1.0), error_at(0.01));
    assert!(error_at(1e-6) > error_at(0.01));
    let cms = FieldDataType::Sketch(SketchAlgorithm::Cms);
    let point = SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: None,
    };
    let depth = |depth| SketchParams::Cms { width: 272, depth };
    assert!(theoretical_guarantee(&cms, &depth(5), &point, 1e-2).is_some());
    assert!(theoretical_guarantee(&cms, &depth(5), &point, 1e-4).is_none());
    assert!(theoretical_guarantee(&cms, &depth(10), &point, 1e-4).is_some());
    // A statistic the summary type does not answer has no guarantee.
    assert!(theoretical_guarantee(&kll, &k, &SketchStatistic::Cardinality, 0.01).is_none());
}
