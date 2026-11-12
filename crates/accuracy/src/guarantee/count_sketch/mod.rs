//! CountSketch L2 frequency error and odd-depth median concentration: the
//! median fails only when a majority of rows fail, an exact binomial tail.
use super::*;

pub(super) fn guarantee(params: &SketchParams) -> Option<AccuracyGuarantee> {
    let (SketchParams::CountSketch { width, depth }
    | SketchParams::CountSketchWithHeap { width, depth, .. }) = params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        AccuracyMetric::ABSOLUTE_STREAM_L2,
        (3.0 / f64::from(*width)).sqrt(),
        ProbabilityExpr::Constant {
            value: count_sketch_failure_probability(*width, *depth)?,
        },
    ))
}
/// One row misses a point query by more than `ε ‖f‖₂ = √(3/w) ‖f‖₂` with
/// probability at most `q/ε² = q w/3` (Chebyshev, `Var ≤ q F2`), with `q`
/// the column collision probability ([`column_collision`]): `1/3` to
/// within `w/(3 · 2^32)`.
fn row_failure_probability(width: u32) -> f64 {
    column_collision(width) * f64::from(width.max(1)) / 3.0
}

/// `P(c_i = c_j)` for two keys in a row of `cols` columns, at most
/// `1/cols + 2^-32`. The executor builds `CountSketch<_, RegularPath>` and
/// `CountSketchWithHeap<_, RegularPath>`, whose rows take a key's column as
/// `(hash & (2^32 − 1)) % cols`: each column holds `⌊2^32/cols⌋` or
/// `⌈2^32/cols⌉` of the `2^32` hash values, so a key lands in any one with
/// probability at most `1/cols + 2^-32`, and two keys collide no more
/// often than that.
fn column_collision(cols: u32) -> f64 {
    1.0 / f64::from(cols.max(1)) + 2f64.powi(-32)
}

/// The median of an odd `depth` of independent rows misses only when at
/// least `(depth + 1)/2` rows miss: `P(Binomial(depth, p) ≥ (depth+1)/2)`.
fn count_sketch_failure_probability(width: u32, depth: u32) -> Option<f64> {
    if depth == 0 || depth.is_multiple_of(2) {
        return None;
    }
    Some(super::binomial_upper_tail(
        depth,
        row_failure_probability(width),
        depth.div_ceil(2),
    ))
}

#[cfg(test)]
mod tests;
