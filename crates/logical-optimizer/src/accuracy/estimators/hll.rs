//! HLL's relative standard error. Generic HLL has no modeled confidence:
//! its failure probability stays unknown.

use super::*;

pub(super) fn generic_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<ResultGuarantee> {
    let SketchParams::Hll { precision } = params else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        ErrorMetric::Cardinality,
        1.04 / 2f64.powi(i32::from(*precision)).sqrt(),
        ProbabilityExpr::Unknown {
            statistic: "hll_estimator_failure_probability".into(),
        },
        "generic_hll_rse_only_no_confidence_v1",
    ))
}
/// HLL RSE-magnitude inversion. Generic HLL has no modeled confidence target.
pub(crate) fn hll_precision(eps: f64) -> u8 {
    saturating_ceil((1.04 / eps).powi(2).log2(), 4, 18) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_rse_sizing_does_not_certify_confidence() {
        use crate::pass1::realization::default_size_params;
        use asap_types::ir::operator::agg_intent::default_cardinality;
        use asap_types::ir::schema::{GroupingStrategy, SketchKind};
        let c = default_cardinality();
        let params = default_size_params(SketchAlgorithm::Hll, &c, 0.01, 0.01);
        let g = local_guarantee(
            &FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::Hll, params),
                GroupingStrategy::default(),
            ),
            &SketchStatistic::Cardinality,
        )
        .unwrap();
        assert_eq!(g.metric, ErrorMetric::Cardinality);
        assert_eq!(g.failure_probability.evaluate(), None);
        assert!(satisfies(&g, &AccuracyTarget::Epsilon(0.01)));
        assert!(!satisfies(
            &g,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            }
        ));
    }
}
