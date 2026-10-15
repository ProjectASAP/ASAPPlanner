//! Dispatch committed estimator parameters to their accuracy models.
use super::*;
use asap_types::pre_asap::AggIntent;

pub mod cardinality;
pub mod cms;
pub mod count_sketch;
pub mod ddsketch;
pub mod hll;
pub mod kll;
pub mod univmon;

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
        SketchParams::Kmv { .. } | SketchParams::Theta { .. } => {
            cardinality::guarantee(algorithm, params, query)
        }
        SketchParams::UnivMon { .. } => univmon::guarantee(algorithm, params, query),
    }
}

fn bounded_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    metric: ErrorMetric,
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

pub(super) fn local_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match family {
        FieldDataType::Plain(_) => Some(AccuracyGuarantee::exact("Plain value")),
        FieldDataType::ExactAggregate(kind, _) => Some(AccuracyGuarantee::exact(format!(
            "ExactAggregate({kind:?})"
        ))),
        FieldDataType::Sketch(kind, _) => sketch_guarantee(kind.algorithm(), kind.params(), query),
        // No error model is registered for these families.
        FieldDataType::Sample(..) | FieldDataType::Wavelet(..) | FieldDataType::StatModel(..) => {
            None
        }
    }
}
/// `params` with the parameters its per-value failure probability depends
/// on enlarged, never reduced, so that each value it answers fails with
/// probability at most `delta` and its error bound is unchanged or smaller
/// (#669b): Count-Min's depth, Count Sketch's depth (the binomial tail of
/// its median), KLL's failure probability (its δ scale) and a distinct
/// count UnivMon's heap. Other parameters are returned as they are:
/// DDSketch never fails; classic HLL's failure probability comes from its
/// source's distinct bound ([`hll::precision_for`]); KMV and Theta are
/// calibrated at a fixed 99%; an L2 or entropy UnivMon is not enlarged.
pub(crate) fn with_failure_probability(params: &SketchParams, delta: f64) -> SketchParams {
    if !(delta > 0.0 && delta < 1.0) {
        return params.clone();
    }
    match params {
        SketchParams::Cms { width, depth } => SketchParams::Cms {
            width: *width,
            depth: (*depth).max(cms::cms_depth(delta)),
        },
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CmsWithHeap {
            width: *width,
            depth: (*depth).max(cms::cms_depth(delta)),
            heap_size: *heap_size,
        },
        SketchParams::CountSketch { width, depth } => SketchParams::CountSketch {
            width: *width,
            depth: (*depth).max(count_sketch::count_sketch_depth(*width, delta)),
        },
        SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CountSketchWithHeap {
            width: *width,
            depth: (*depth).max(count_sketch::count_sketch_depth(*width, delta)),
            heap_size: *heap_size,
        },
        SketchParams::Kll { .. } => kll::with_failure_probability(params, delta),
        SketchParams::UnivMon { .. } => univmon::with_failure_probability(params, delta),
        _ => params.clone(),
    }
}

pub(crate) fn size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        SketchAlgorithm::UnivMon => univmon::size_params(intent, eps, delta),
        SketchAlgorithm::Kll => kll::size_params(eps, delta),
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: cms::cms_width(eps),
            depth: cms::cms_depth(delta),
        },
        SketchAlgorithm::Hll => SketchParams::Hll {
            precision: hll::hll_precision(eps),
        },
        SketchAlgorithm::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: cms::cms_width(eps),
                depth: cms::cms_depth(delta),
                heap_size: topk_capacity(k, eps),
            }
        }
        // Non-preferred candidates (DDSketch / Theta / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchAlgorithm::DDSketch => ddsketch::size_params(eps),
        SketchAlgorithm::Theta => SketchParams::Theta {
            k: cardinality::kmv_k_99(eps),
        },
        SketchAlgorithm::Kmv => SketchParams::Kmv {
            k: cardinality::kmv_k_99(eps),
        },
        SketchAlgorithm::CountSketch => {
            let width = count_sketch::count_sketch_width(eps);
            SketchParams::CountSketch {
                width,
                depth: count_sketch::count_sketch_depth(width, delta),
            }
        }
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            let width = count_sketch::count_sketch_width(eps);
            SketchParams::CountSketchWithHeap {
                width,
                depth: count_sketch::count_sketch_depth(width, delta),
                heap_size: topk_capacity(k, eps),
            }
        }
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

/// `P(c_i = c_j)` for two keys in a Count Sketch row of `cols` columns.
/// `asap_sketchlib` takes a column as a `⌈log2 cols⌉`-bit hash slice modulo
/// `cols`: uniform for a power of two, and otherwise at most `9/(8 cols)`
/// (the slice values past `cols` fold onto the low columns; the worst case,
/// `cols = ¾ · 2^⌈log2 cols⌉`, doubles a third of them).
pub(crate) fn column_collision(cols: u32) -> f64 {
    let cols = cols.max(1);
    if cols.is_power_of_two() {
        1.0 / f64::from(cols)
    } else {
        9.0 / (8.0 * f64::from(cols))
    }
}

/// The parameters of one state shared by consumers of the same algorithm,
/// each sized alone with one of `params` (Pass 2's summary-capability
/// rule): each parameter's largest value, except where a smaller value is
/// stricter, where it takes the smallest (a KLL's failure probability,
/// DDSketch's α). Not every bound is monotone in its parameters (a Count
/// Sketch's collision bound is tighter at a power-of-two width than one
/// column wider), so the shared state is not assumed to meet each
/// consumer's target: the summary-capability rule composes every guarantee
/// above it again, and plan selection checks each consumer against the
/// shared state. `None` for no parameters,
/// parameters of different algorithms, or a shared UnivMon the library
/// would not build ([`univmon::hash_independent`]).
pub(crate) fn shared_size_params(params: &[&SketchParams]) -> Option<SketchParams> {
    use SketchParams as P;
    let (first, rest) = params.split_first()?;
    let shared = rest.iter().try_fold((*first).clone(), |a, b| {
        Some(match (a, *b) {
            (
                P::UnivMon {
                    heap_size,
                    sketch_rows,
                    sketch_cols,
                    layers,
                    distinct_failure_probability,
                },
                P::UnivMon {
                    heap_size: h,
                    sketch_rows: r,
                    sketch_cols: c,
                    layers: l,
                    distinct_failure_probability: f,
                },
            ) => P::UnivMon {
                heap_size: heap_size.max(*h),
                sketch_rows: sketch_rows.max(*r),
                sketch_cols: sketch_cols.max(*c),
                layers: layers.max(*l),
                // `None`: not sized for a distinct count.
                distinct_failure_probability: min_recorded(distinct_failure_probability, *f),
            },
            (
                P::Kll {
                    k,
                    failure_probability,
                },
                P::Kll {
                    k: k2,
                    failure_probability: f2,
                },
            ) => P::Kll {
                k: k.max(*k2),
                // `None` is the calibration's δ, above any recorded one.
                failure_probability: min_recorded(failure_probability, *f2),
            },
            (P::Cms { width, depth }, P::Cms { width: w, depth: d }) => P::Cms {
                width: width.max(*w),
                depth: depth.max(*d),
            },
            (P::CountSketch { width, depth }, P::CountSketch { width: w, depth: d }) => {
                P::CountSketch {
                    width: width.max(*w),
                    depth: depth.max(*d),
                }
            }
            (
                P::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                },
                P::CmsWithHeap {
                    width: w,
                    depth: d,
                    heap_size: h,
                },
            ) => P::CmsWithHeap {
                width: width.max(*w),
                depth: depth.max(*d),
                heap_size: heap_size.max(*h),
            },
            (
                P::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
                P::CountSketchWithHeap {
                    width: w,
                    depth: d,
                    heap_size: h,
                },
            ) => P::CountSketchWithHeap {
                width: width.max(*w),
                depth: depth.max(*d),
                heap_size: heap_size.max(*h),
            },
            (P::Hll { precision }, P::Hll { precision: p }) => P::Hll {
                precision: precision.max(*p),
            },
            (P::DDSketch { alpha }, P::DDSketch { alpha: a }) => P::DDSketch {
                alpha: alpha.min(*a),
            },
            (P::Kmv { k }, P::Kmv { k: k2 }) => P::Kmv { k: k.max(*k2) },
            (P::Theta { k }, P::Theta { k: k2 }) => P::Theta { k: k.max(*k2) },
            _ => return None,
        })
    })?;
    // The largest rows of one UnivMon with the largest columns of another
    // can pass the hash budget `asap_sketchlib` builds within.
    match shared {
        P::UnivMon {
            sketch_rows,
            sketch_cols,
            layers,
            ..
        } if !univmon::hash_independent(sketch_rows, sketch_cols, layers) => None,
        shared => Some(shared),
    }
}

/// The smaller of two recorded failure probabilities; an unrecorded one
/// (`None`) yields to a recorded one.
fn min_recorded(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Accuracy-dependent candidate budget, not a completeness theorem. The
/// membership model must still certify the selected set independently.
pub(crate) fn topk_capacity(k: usize, eps: f64) -> u32 {
    u32::try_from(k)
        .unwrap_or(u32::MAX)
        .max(saturating_ceil(1.0 / eps, 1, 1 << 26))
}

/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive x saturate to `hi`
/// (a degenerate ε means "as accurate as this family goes").
pub(crate) fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}

#[cfg(test)]
mod shared_size_tests {
    use super::*;

    /// The shared size takes each parameter's largest value, and the
    /// smallest where smaller is stricter (KLL δ, DDSketch α); sizes of
    /// different algorithms have none.
    #[test]
    fn shared_size_takes_each_parameters_strictest_value() {
        assert_eq!(
            shared_size_params(&[&univmon(256, 5, 1024, 16), &univmon(1, 7, 4096, 1)]),
            Some(univmon(256, 7, 4096, 16))
        );
        // The distinct count's failure probability, where one was sized
        // for, takes the smallest.
        let distinct = |heap_size, delta| SketchParams::UnivMon {
            heap_size,
            sketch_rows: 1,
            sketch_cols: 1,
            layers: 19,
            distinct_failure_probability: Some(delta),
        };
        let Some(SketchParams::UnivMon {
            heap_size,
            distinct_failure_probability,
            ..
        }) = shared_size_params(&[
            &univmon(256, 5, 1024, 16),
            &distinct(4096, 0.01),
            &distinct(2048, 0.001),
        ])
        else {
            unreachable!()
        };
        assert_eq!(
            (heap_size, distinct_failure_probability),
            (4096, Some(0.001))
        );
        let kll = |k, failure_probability| SketchParams::Kll {
            k,
            failure_probability,
        };
        let SketchParams::Kll {
            k,
            failure_probability,
        } = shared_size_params(&[
            &kll(200, None),
            &kll(100, Some(0.001)),
            &kll(150, Some(0.005)),
        ])
        .unwrap()
        else {
            unreachable!()
        };
        assert_eq!((k, failure_probability), (200, Some(0.001)));
        assert_eq!(
            shared_size_params(&[
                &SketchParams::DDSketch { alpha: 0.01 },
                &SketchParams::DDSketch { alpha: 0.001 }
            ]),
            Some(SketchParams::DDSketch { alpha: 0.001 })
        );
        assert_eq!(
            shared_size_params(&[&kll(200, None), &SketchParams::Hll { precision: 12 }]),
            None
        );
        assert_eq!(shared_size_params(&[]), None);
    }

    fn univmon(heap_size: u32, sketch_rows: u32, sketch_cols: u32, layers: u8) -> SketchParams {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
            distinct_failure_probability: None,
        }
    }

    /// Each UnivMon fits `asap_sketchlib`'s hash budget, but the largest
    /// rows of one with the largest columns of the other do not
    /// (`12 · (⌈log2 2^16⌉ + 1) = 204 > 128`): the library would not build
    /// the shared shape, so there is none.
    #[test]
    fn no_shared_univmon_past_the_hash_budget() {
        assert!(univmon::hash_independent(12, 512, 1));
        assert!(univmon::hash_independent(5, 1 << 16, 1));
        assert_eq!(
            shared_size_params(&[&univmon(1, 12, 512, 1), &univmon(1, 5, 1 << 16, 1)]),
            None
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::agg_intent::{default_cardinality, default_quantile};

    fn delta_and_error(params: &SketchParams, query: &SketchStatistic) -> (f64, f64) {
        let algorithm = match params {
            SketchParams::Cms { .. } => SketchAlgorithm::Cms,
            SketchParams::CountSketch { .. } => SketchAlgorithm::CountSketch,
            SketchParams::Kll { .. } => SketchAlgorithm::Kll,
            other => panic!("{other:?}"),
        };
        let g = sketch_guarantee(&algorithm, params, query).unwrap();
        (
            g.spec.failure_probability.evaluate().unwrap(),
            g.spec.error.evaluate().unwrap(),
        )
    }

    /// Enlarging for a smaller failure probability deepens Count-Min and
    /// Count Sketch and grows KLL until each meets it, at no larger error;
    /// it never shrinks a sketch that already meets it. (Count Sketch's
    /// depth is capped at 255 rows, which a non-power-of-two width reaches
    /// before 1e-6.)
    #[test]
    fn enlarging_meets_the_failure_probability_at_no_larger_error() {
        let point = SketchStatistic::PointCount {
            key: asap_types::pre_asap::expr_ir::ColumnRef::SampleValue,
            value: None,
        };
        let quantile = SketchStatistic::Quantile { q: 0.99 };
        for (params, query, delta) in [
            (
                size_params(SketchAlgorithm::Cms, &default_cardinality(), 0.01, 0.01),
                &point,
                1e-6,
            ),
            (
                size_params(
                    SketchAlgorithm::CountSketch,
                    &default_cardinality(),
                    0.01,
                    0.01,
                ),
                &point,
                1e-3,
            ),
            (
                size_params(SketchAlgorithm::Kll, &default_quantile(0.99), 0.01, 0.01),
                &quantile,
                1e-6,
            ),
        ] {
            let (_, error) = delta_and_error(&params, query);
            let enlarged = with_failure_probability(&params, delta);
            let (new_delta, new_error) = delta_and_error(&enlarged, query);
            assert!(new_delta <= delta, "{enlarged:?}");
            assert!(new_error <= error * (1.0 + 1e-9), "{enlarged:?}");
            assert_ne!(enlarged, params);
            assert_eq!(with_failure_probability(&enlarged, 0.01), enlarged);
        }
        let ddsketch = SketchParams::DDSketch { alpha: 0.01 };
        assert_eq!(with_failure_probability(&ddsketch, 1e-6), ddsketch);
    }

    /// A distinct-count UnivMon grows its heap for a smaller failure
    /// probability at no larger relative error.
    #[test]
    fn a_distinct_count_univmon_grows_its_heap() {
        let params = size_params(SketchAlgorithm::UnivMon, &default_cardinality(), 0.05, 0.01);
        let enlarged = with_failure_probability(&params, 1e-4);
        let error = |p: &SketchParams| {
            univmon::distinct_guarantee(p, "m")
                .unwrap()
                .spec
                .error
                .evaluate()
                .unwrap()
        };
        let (
            SketchParams::UnivMon { heap_size: k, .. },
            SketchParams::UnivMon {
                heap_size: grown,
                distinct_failure_probability,
                ..
            },
        ) = (&params, &enlarged)
        else {
            panic!("{enlarged:?}")
        };
        assert!(grown > k);
        assert_eq!(*distinct_failure_probability, Some(1e-4));
        assert!(error(&enlarged) <= error(&params) * (1.0 + 1e-9));
    }

    /// Classic HLL's precision grows until its failure probability over the
    /// source's distinct bound meets the target, up to 18.
    #[test]
    fn classic_hll_precision_grows_for_its_failure_probability() {
        let p = hll::precision_for(4, 64, 0.01);
        assert!(p > 4);
        assert!(hll::classic_failure_probability(p, 64) <= 0.01);
        assert!(hll::classic_failure_probability(p - 1, 64) > 0.01);
        assert_eq!(hll::precision_for(12, 4096, 1e-30), 12);
    }
}
