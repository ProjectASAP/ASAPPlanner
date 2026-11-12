//! UnivMon's readouts and the guarantees `asap_sketchlib`'s standard-mode
//! estimators carry (the derivations are in the library's
//! `docs/api/api_univmon.md`, the bounds `estimate_l2` and
//! `estimate_cardinality` cite):
//! - the total (a bare `PointCount`) is the exact unit-update count;
//! - L2 reads layer 0's Count Sketch directly: per row, Chebyshev with
//!   `Var ≤ 2q F2²`, and the median of the rows fails only when at least
//!   `⌈r/2⌉` rows fail, an exact binomial tail;
//! - the distinct count has no guarantee here: it needs the source's
//!   distinct bound;
//! - entropy has no certified bound.
//!
//! Every bound assumes the key hash is uniformly random and the shape keeps
//! the hashes independent (`hash_independent`). Terminal-mode states
//! (OctoSketch aggregators fed promoted deltas) are not covered: the
//! executor rejects them.
use super::*;

/// The per-row failure probability the L2 guarantee fixes: a row misses by
/// more than `ε₂ F2` with probability at most `2q/ε₂² = 0.1`. Fixing it
/// lets a shape state one `(ε, δ)`: the columns give `ε`, the rows `δ`.
const L2_ROW_FAILURE: f64 = 0.1;

pub(super) fn guarantee(
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    let SketchParams::UnivMon {
        sketch_rows,
        sketch_cols,
        layers,
        ..
    } = params
    else {
        return None;
    };
    match query {
        SketchStatistic::PointCount { value: None, .. } => Some(AccuracyGuarantee::exact()),
        SketchStatistic::FrequencyL2 if hash_independent(*sketch_rows, *sketch_cols, *layers) => {
            Some(super::bounded_guarantee(
                AccuracyMetric::Relative,
                l2_relative_error(*sketch_cols)?,
                ProbabilityExpr::Constant {
                    value: l2_failure_probability(*sketch_rows),
                },
            ))
        }
        // The distinct count needs the source's distinct bound; entropy has
        // no certified bound.
        _ => None,
    }
}

/// Whether the executor builds this shape, with independent hashes: at
/// most `UNIVMON_MAX_LAYERS` (the library's) layers and `MATRIX_MAX_ROWS` (20) rows, the rows a
/// UnivMon state encodes, and `rows · (⌈log2 cols⌉ + 1) ≤ 128`
/// ([`asap_sketchlib::UnivMon::check_shape`]; the heap size is not part of
/// the hash rules).
fn hash_independent(rows: u32, cols: u32, layers: u8) -> bool {
    rows as usize <= asap_sketchlib::MATRIX_MAX_ROWS
        && asap_sketchlib::UnivMon::check_shape(
            1,
            rows as usize,
            cols as usize,
            usize::from(layers),
        )
        .is_ok()
}

/// The relative error of the L2 norm at `cols` columns. Per row, the
/// squared norm misses by more than `ε₂ F2` with probability at most
/// `2q/ε₂²` ([`column_collision`]), so fixing that at
/// [`L2_ROW_FAILURE`] gives `ε₂ = √(2q/0.1)`. The norm then misses by at
/// most `1 − √(1 − ε₂)` (the low side; the high side `√(1 + ε₂) − 1` is
/// smaller). `None` when `ε₂ ≥ 1`.
fn l2_relative_error(cols: u32) -> Option<f64> {
    let f2_error = (2.0 * column_collision(cols) / L2_ROW_FAILURE).sqrt();
    (f2_error < 1.0).then(|| 1.0 - (1.0 - f2_error).sqrt())
}

/// `P(c_i = c_j)` for two keys in a row of a UnivMon layer of `cols`
/// columns. `asap_sketchlib` takes a UnivMon column as a `⌈log2 cols⌉`-bit
/// slice of one 128-bit hash modulo `cols` ([`hash_independent`]): uniform
/// for a power of two, and otherwise at most `9/(8 cols)` (the slice values
/// past `cols` fold onto the low columns; the worst case,
/// `cols = ¾ · 2^⌈log2 cols⌉`, doubles a third of them). A standalone
/// Count Sketch hashes modulo `cols` instead (`count_sketch`).
fn column_collision(cols: u32) -> f64 {
    let cols = cols.max(1);
    if cols.is_power_of_two() {
        1.0 / f64::from(cols)
    } else {
        9.0 / (8.0 * f64::from(cols))
    }
}

/// The median of `rows` independent rows misses only when at least
/// `⌈rows/2⌉` of them miss: `P(Binomial(rows, 0.1) ≥ ⌈rows/2⌉)`.
fn l2_failure_probability(rows: u32) -> f64 {
    super::binomial_upper_tail(rows, L2_ROW_FAILURE, rows.div_ceil(2))
}

#[cfg(test)]
mod tests;
