//! KMV and Theta cardinality bounds using variance and Chebyshev at 99%.
use super::*;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchQuery,
) -> Option<ResultGuarantee> {
    let (SketchParams::Kmv { k } | SketchParams::Theta { k }) = params else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        ErrorMetric::Cardinality,
        10.0 / f64::from(k.saturating_sub(2).max(1)).sqrt(),
        ProbabilityExpr::Constant { value: 0.01 },
        match params {
            SketchParams::Kmv { .. } => "kmv_unbiased_variance_chebyshev_99_v1",
            _ => "theta_variance_chebyshev_99_v1",
        },
    ))
}

/// 99%-confidence KMV/Theta relative bound via Chebyshev, using
/// `RSE <= 1/sqrt(k-2)` and a ten-standard-deviation interval.
pub(crate) fn kmv_k_99(eps: f64) -> u32 {
    saturating_ceil(100.0 / (eps * eps) + 2.0, 16, 1 << 26)
}
