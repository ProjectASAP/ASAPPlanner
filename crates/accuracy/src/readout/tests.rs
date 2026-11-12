use super::*;
use asap_types::summary_type::{ExactKind, SketchAlgorithm};

/// A readout's metric is its algorithm's: KLL quantiles read a rank
/// error, DDSketch quantiles and HLL distinct counts a relative one
/// (HLL's although its failure probability is unknown); an exact state
/// reads exactly.
#[test]
fn a_readouts_metric_is_its_algorithms() {
    let quantile = SketchStatistic::Quantile { q: 0.9 };
    let sketch = |algorithm| FieldDataType::Sketch(algorithm);
    assert_eq!(
        readout_error_metric(&sketch(SketchAlgorithm::Kll), &quantile),
        Some(AccuracyMetric::Rank)
    );
    assert_eq!(
        readout_error_metric(&sketch(SketchAlgorithm::DDSketch), &quantile),
        Some(AccuracyMetric::Relative)
    );
    assert_eq!(
        readout_error_metric(&sketch(SketchAlgorithm::Hll), &SketchStatistic::Cardinality),
        Some(AccuracyMetric::Relative)
    );
    assert_eq!(
        readout_error_metric(&FieldDataType::exact(ExactKind::Sum), &quantile),
        None
    );
}
