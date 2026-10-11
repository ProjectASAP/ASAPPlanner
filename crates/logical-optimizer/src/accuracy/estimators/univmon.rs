//! UnivMon certifies its unit-update total, which the kernel reads exactly
//! (`calc_l1` returns the update count), and the L2 norm read from layer 0's
//! CountSketch.
//!
//! **L2.** Layer 0 sees the whole stream. Each of its `d` rows keeps
//! `R = Σ_j C[j]²`, the AMS F₂ estimator: with fully random hashing,
//! `E[R] = F₂` and `Var[R] ≤ 2F₂²/w`. Chebyshev puts a row outside
//! `(1 ± ε₂)·F₂` with probability at most `p = 0.1` when
//! `ε₂ = √(2/(w·p)) = √(20/w)`. The kernel reads `√(median of the rows)`,
//! which is outside the interval only if at least `(d+1)/2` of `d` (odd)
//! independent rows are, so `δ = Σ_{i ≥ (d+1)/2} C(d,i) pⁱ (1−p)^(d−i)`.
//! Inside it the relative L2 error is at most `1 − √(1 − ε₂)`, the lower
//! side; the upper side `√(1 + ε₂) − 1` is smaller since `√` is concave.
//! States merge by adding counters, so the bound holds for merged panes.
//!
//! sketchlib slices every row's column and sign from one 128-bit hash (row
//! `r` uses bits `[r·m, (r+1)·m)` for `m = log₂ w` and sign bit `127 − r`),
//! so the rows are independent only when those slices are disjoint:
//! `d·m + d ≤ 128` with `w` a power of two. Otherwise no L2 bound is given.
//! The contract records the idealized-hash assumption.
//!
//! **Distinct count and entropy** are deliberately left uncertified, so
//! Stage 3 rejects them unless a deployment supplies accuracy evidence:
//!
//! - Liu et al., "One Sketch to Rule Them All" (SIGCOMM 2016), rely on
//!   Braverman and Ostrovsky's recursive sketch. With O(log n) layers, each of
//!   which returns a (g, ε)-cover of its sampled substream (every g-heavy item
//!   with a (1 ± ε) frequency), the recursive G-sum is a (1 ± ε)
//!   approximation with probability 1 − δ. The layer sketch's size is only
//!   stated asymptotically (O(ε⁻² log(1/δ)) per CountSketch, times
//!   polylogarithmic factors). No constants are given that could be inverted
//!   into a concrete `(heap_size, sketch_rows, sketch_cols, layers)`.
//! - The kernel (`asap_sketchlib::UnivMon::calc_g_sum_heuristic`, behind
//!   `asap-executor`'s `UnivMonAccumulator`) keeps a fixed top-`heap_size`
//!   heap per layer and estimates its items with the layer's CountSketch.
//!   For distinct counts it also drops items below `L2 / sqrt(heap_size)`.
//!   Nothing bounds the probability that a layer's heap is such a cover, so
//!   the theorem's premise does not hold for these readouts. Even when every
//!   heap holds every item, a CountSketch estimate may be at most zero for a
//!   present item, so the distinct count is not certified in that case
//!   either.
//!
//! A sound guarantee for them needs a per-layer cover guarantee for the
//! heap, a kernel change.
use super::*;

/// The Chebyshev failure probability of one row's F₂ estimate.
const ROW_FAILURE: f64 = 0.1;

/// `asap-executor`'s UnivMon kernel rejects more than 20 rows.
const MAX_ROWS: u32 = 19;

pub(super) fn guarantee(params: &SketchParams, query: &SketchStatistic) -> Option<ResultGuarantee> {
    match query {
        SketchStatistic::PointCount { value: None, .. } => {
            Some(ResultGuarantee::exact("univmon_unit_update_total"))
        }
        SketchStatistic::FrequencyL2 => l2_guarantee(params),
        _ => None,
    }
}

fn l2_guarantee(params: &SketchParams) -> Option<ResultGuarantee> {
    let SketchParams::UnivMon {
        sketch_rows: d,
        sketch_cols: w,
        ..
    } = params
    else {
        return None;
    };
    let hash_bits = u64::from(*d) * (u64::from(w.checked_ilog2()?) + 1);
    if d.is_multiple_of(2) || !w.is_power_of_two() || hash_bits > 128 {
        return None;
    }
    let f2_error = (20.0 / f64::from(*w)).sqrt();
    if f2_error >= 1.0 {
        return None;
    }
    Some(ResultGuarantee {
        metric: ErrorMetric::RelativeValue,
        bound: BoundExpr::Constant {
            value: 1.0 - (1.0 - f2_error).sqrt(),
        },
        failure_probability: ProbabilityExpr::Constant {
            value: median_failure(*d),
        },
        provenance: vec![GuaranteeSource::SketchEvaluation {
            algorithm: "UnivMon".into(),
            contract: "univmon_layer0_f2_median_chebyshev_v1".into(),
            params: serde_json::json!({"sketch_rows": d, "sketch_cols": w,
                "row_failure_probability": ROW_FAILURE,
                "hash_assumption": "independent_uniform_128_bit_hash",
                "readout": "sqrt_median_layer0_row_f2"}),
            query: "FrequencyL2".into(),
        }],
    })
}

/// Probability that at least `(d+1)/2` of `d` independent rows fail, each
/// with probability [`ROW_FAILURE`].
fn median_failure(d: u32) -> f64 {
    let (p, n) = (ROW_FAILURE, i32::try_from(d).unwrap_or(i32::MAX));
    let mut binomial = 1.0; // C(d, i)
    let mut tail = 0.0;
    for i in 0..=n {
        if 2 * i > n {
            tail += binomial * p.powi(i) * (1.0 - p).powi(n - i);
        }
        binomial *= f64::from(n - i) / f64::from(i + 1);
    }
    tail
}

/// One shape per `(ε, δ)` for every readout, so every consumer of a Pass 2
/// key reads identical states: the L2 sizing, which the total does not need
/// and distinct count and entropy cannot use. `w` is the smallest power of
/// two with `1 − √(1 − √(20/w)) ≤ ε`, `d` the smallest odd depth with
/// median failure at most δ. Parameters past the hash's bit budget are kept;
/// the guarantee then rejects them. Without a positive ε (an exact count,
/// which reads only the exact total) the shape is a fixed baseline.
pub(super) fn size_params(eps: f64, delta: f64) -> SketchParams {
    if eps.is_nan() || eps <= 0.0 {
        return SketchParams::UnivMon {
            heap_size: 256,
            sketch_rows: 5,
            sketch_cols: 1024,
            layers: 16,
        };
    }
    let eps = eps.min(1.0);
    let f2_error = 2.0 * eps - eps * eps;
    let sketch_cols =
        saturating_ceil(20.0 / (f2_error * f2_error), 32, 1 << 26).next_power_of_two();
    let sketch_rows = (1..=MAX_ROWS)
        .step_by(2)
        .find(|&d| median_failure(d) <= delta)
        .unwrap_or(MAX_ROWS);
    SketchParams::UnivMon {
        heap_size: 256,
        sketch_rows,
        sketch_cols,
        layers: 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::{GroupingStrategy, SketchKind};

    fn family(params: SketchParams) -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::UnivMon, params),
            GroupingStrategy::default(),
        )
    }

    fn l2(params: SketchParams) -> Option<ResultGuarantee> {
        local_guarantee(&family(params), &SketchStatistic::FrequencyL2)
    }

    fn shape(params: &SketchParams) -> (u32, u32) {
        let SketchParams::UnivMon {
            sketch_rows,
            sketch_cols,
            ..
        } = params
        else {
            unreachable!()
        };
        (*sketch_rows, *sketch_cols)
    }

    fn with_shape(sketch_rows: u32, sketch_cols: u32) -> SketchParams {
        SketchParams::UnivMon {
            heap_size: 256,
            sketch_rows,
            sketch_cols,
            layers: 16,
        }
    }

    /// The exact total and L2 are certified; distinct count and entropy have
    /// no sound bound for the shipped kernel and stay uncertified.
    #[test]
    fn the_total_and_l2_are_certified() {
        let params = size_params(0.01, 0.01);
        let total = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        assert!(local_guarantee(&family(params.clone()), &total).is_some_and(|g| g.is_exact()));
        let guarantee = l2(params.clone()).expect("L2 is certified");
        assert_eq!(guarantee.metric, ErrorMetric::RelativeValue);
        assert!(crate::accuracy::satisfies(
            &guarantee,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            }
        ));
        for query in [
            SketchStatistic::Cardinality,
            SketchStatistic::FrequencyEntropy,
        ] {
            assert!(
                local_guarantee(&family(params.clone()), &query).is_none(),
                "{query:?}"
            );
        }
    }

    /// (0.01, 0.01) sizes 5 rows of 2^16 columns: √(20/2^16) ≈ 0.0175 F₂
    /// error, ≈ 0.0088 L2 error, and a median failure of ≈ 0.0086.
    #[test]
    fn sizing_inverts_the_bound() {
        assert_eq!(shape(&size_params(0.01, 0.01)), (5, 1 << 16));
        let (rows, cols) = shape(&size_params(0.01, 0.01));
        let (tighter_rows, _) = shape(&size_params(0.01, 0.001));
        let (_, wider_cols) = shape(&size_params(0.005, 0.01));
        assert!(tighter_rows > rows && wider_cols > cols);
        let guarantee = l2(size_params(0.01, 0.01)).unwrap();
        assert!(guarantee.bound.evaluate().unwrap() <= 0.01);
        assert!(guarantee.failure_probability.evaluate().unwrap() <= 0.01);
    }

    /// Even depth, a non-power-of-two width, or rows whose hash slices
    /// overlap get no L2 bound.
    #[test]
    fn l2_fails_closed_outside_the_hash_contract() {
        assert!(l2(with_shape(5, 1 << 16)).is_some());
        assert!(l2(with_shape(4, 1 << 16)).is_none());
        assert!(l2(with_shape(5, 1000)).is_none());
        // 7 · (16 + 1) = 119 bits fit; 9 · 17 = 153 do not.
        assert!(l2(with_shape(7, 1 << 16)).is_some());
        assert!(l2(with_shape(9, 1 << 16)).is_none());
        // (0.01, 0.001) needs 9 rows of 2^16 columns: sized, but rejected.
        assert_eq!(shape(&size_params(0.01, 0.001)), (9, 1 << 16));
        assert!(l2(size_params(0.01, 0.001)).is_none());
    }
}
