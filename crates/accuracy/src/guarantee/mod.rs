//! Dispatch committed estimator parameters to their accuracy models.
use asap_types::accuracy::{
    AccuracyGuarantee, AccuracyMetric, BoundExpr, GuaranteeSource, ProbabilityExpr,
};
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};

#[cfg(test)]
use crate::requirements::meets;
use asap_types::summary_type::SummaryTypeCapability;

pub(crate) mod cardinality;
pub(crate) mod cms;
pub(crate) mod count_sketch;
pub(crate) mod ddsketch;
pub mod hll;
pub(crate) mod kll;
#[cfg(test)]
mod sizing_oracle;
pub mod univmon;

#[allow(dead_code)] // Read again for the error bound of each listed θ (`parameter_list`), in #1207 PRs 3–4.
pub(super) fn sketch_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match params {
        SketchParams::Kll { .. } => kll::guarantee(algorithm, params, query),
        SketchParams::DDSketch { .. } => ddsketch::guarantee(algorithm, params, query),
        SketchParams::Hll { .. } => hll::generic_guarantee(algorithm, params, query),
        SketchParams::Cms { .. } | SketchParams::CmsWithHeap { .. } => {
            cms::guarantee(algorithm, params, query)
        }
        SketchParams::CountSketch { .. } | SketchParams::CountSketchWithHeap { .. } => {
            count_sketch::guarantee(algorithm, params, query)
        }
        SketchParams::Kmv { .. } => cardinality::guarantee(algorithm, params, query),
        SketchParams::UnivMon { .. } => univmon::guarantee(algorithm, params, query),
        SketchParams::Hydra(hydra) => {
            sketch_guarantee(algorithm, &hydra.per_subpopulation(), query)
        }
    }
}

fn bounded_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    metric: AccuracyMetric,
    bound: f64,
    delta: ProbabilityExpr,
    contract: &str,
) -> AccuracyGuarantee {
    AccuracyGuarantee::new(
        metric,
        BoundExpr::Constant { value: bound },
        delta,
        vec![GuaranteeSource::SketchEvaluation {
            algorithm: format!("{algorithm:?}"),
            contract: contract.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            query: format!("{query:?}"),
        }],
    )
}

/// The theoretical error bound of reading `statistic` from a state of
/// `summary_type` with parameters `parameters` at failure probability
/// `failure_probability` (the guarantee's own failure probability): the
/// algorithm's error formula (concepts doc, section 2, "Where the guarantee
/// comes from"). `None` when this θ cannot fail that rarely, when the
/// summary type does not answer `statistic` (its capability row) or has no
/// error model (`SummarySample`, `Wavelet`, `StatModel`, and sketch
/// readouts without a certified bound).
pub fn theoretical_guarantee(
    summary_type: &FieldDataType,
    parameters: &SketchParams,
    statistic: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    match summary_type {
        FieldDataType::Sketch(_) if SummaryTypeCapability::of(summary_type).answers(statistic) => {
            let _ = (parameters, failure_probability);
            todo!("#1207 PR 4: the algorithm's error formula at θ, at failure_probability")
        }
        _ => None,
    }
}

/// `P(Binomial(n, p) ≥ k)`, summed term by term in log space and rounded
/// up by a relative 1e-12 so rounding never certifies a smaller tail.
pub(crate) fn binomial_upper_tail(n: u32, p: f64, k: u32) -> f64 {
    if k == 0 || p >= 1.0 {
        return 1.0;
    }
    if k > n || p <= 0.0 {
        return 0.0;
    }
    let (ln_p, ln_q) = (p.ln(), (-p).ln_1p());
    let mut ln_choose = 0.0;
    let mut tail = 0.0;
    for i in 0..=n {
        if i > 0 {
            ln_choose += f64::from(n - i + 1).ln() - f64::from(i).ln();
        }
        if i >= k {
            tail += (ln_choose + f64::from(i) * ln_p + f64::from(n - i) * ln_q).exp();
        }
    }
    (tail * (1.0 + 1e-12)).min(1.0)
}
