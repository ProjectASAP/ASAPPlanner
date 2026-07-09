//! Sketch-vs-exact boundary — the per-intent accuracy decision (issue #98).
//!
//! The per-node choice of how an [`AggIntent`] is *realised*: by an
//! approximate sketch, by an exact mergeable accumulator, or by an ordinary
//! exact operator (pass-through). This is an L4 concern: L3 carries only the
//! intent + accuracy target, never the realization.
//!
//! The decision consumes three inputs:
//!
//! - the [`AccuracyTarget`] threaded onto the approximate-capable intents
//!   (`Quantile` / `Cardinality` / `Count` / `TopK`) — `Exact` forbids a
//!   sketch; `Epsilon` / `EpsilonDelta` size the sketch parameters;
//! - the [`agg_is_exact`] / [`agg_is_mergeable`] helpers in `asap-ir` —
//!   an exact accumulator exists only for mergeable intents
//!   (`Avg`/`StdDev`/`Variance` need richer partial state, so they
//!   pass through to an ordinary exact operator);
//! - histogram sketchability (#79): [`AggIntent::HistogramQuantile`]
//!   (classic cumulative-`le`-bucket interpolation) is **not** re-sketchable —
//!   pre-aggregated bucket counts can't feed a quantile sketch — while the
//!   generic `Quantile` path (native histograms / raw samples) is.
//!
//! [`realize`] is a single exhaustive match over the intent vocabulary, so a
//! new `AggIntent` variant fails to compile until it is given an explicit
//! realization — there is no silent fall-through. The [`bind`](crate::bind)
//! pass fires it per node over nested trees.

use asap_ir::intent_algebra::agg_intent::{agg_is_mergeable, AggIntent};
use asap_ir::types::AccuracyTarget;
use asap_sketch::{SummaryKind, SummaryParams};

/// How an [`AggIntent`] is realised at L4.
#[derive(Debug, Clone, PartialEq)]
pub enum Realization {
    /// An approximate sketch, sized to the intent's [`AccuracyTarget`].
    Sketch {
        kind: SummaryKind,
        params: SummaryParams,
    },
    /// An exact **mergeable** accumulator (partial state ≡ the value itself:
    /// `Sum` / `Count` / `MinMax` / `Rate` / `Increase`). Still a summary —
    /// it pre-aggregates and merges across stages — just with zero error.
    ExactAccumulator {
        kind: SummaryKind,
        params: SummaryParams,
    },
    /// No summary form — the node stays a logical L3 operator and is executed
    /// exactly (per-series transforms, non-mergeable reducers, exact
    /// quantile/top-k/cardinality, classic-bucket `HistogramQuantile`, …).
    PassThrough,
}

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::Epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch families that can serve an intent, most-preferred first.
/// This is the `AggIntent → SummaryKind` map of issue #98; [`realize`] binds
/// the head of the list. The tail entries are the alternatives a future cost
/// model (#6/#33) may pick instead — listed here so the candidate set has one
/// home.
pub fn sketch_candidates(intent: &AggIntent) -> &'static [SummaryKind] {
    match intent {
        AggIntent::Quantile { .. } => &[SummaryKind::Kll, SummaryKind::DDSketch],
        AggIntent::Cardinality { .. } => {
            &[SummaryKind::Hll, SummaryKind::Theta, SummaryKind::Kmv]
        }
        AggIntent::TopK { .. } => &[SummaryKind::CmsWithHeap],
        AggIntent::Count { .. } => &[SummaryKind::Cms],
        _ => &[],
    }
}

/// The sketch-vs-exact boundary decision for one intent.
///
/// Exhaustive over the [`AggIntent`] vocabulary — adding a variant without an
/// explicit realization is a compile error, and the coverage-matrix test pins
/// each variant's category.
pub fn realize(intent: &AggIntent) -> Realization {
    match intent {
        // ── Approximate-capable intents — the AccuracyTarget decides ────────
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => match accuracy {
            AccuracyTarget::Exact => exact_realization(intent),
            _ => bind_sketch(intent, accuracy),
        },

        // ── Exact mergeable accumulators ─────────────────────────────────────
        AggIntent::Sum { .. } => accumulator(intent, SummaryKind::Sum, SummaryParams::Sum),
        AggIntent::Min { .. } | AggIntent::Max { .. } => {
            accumulator(intent, SummaryKind::MinMax, SummaryParams::MinMax)
        }
        AggIntent::Rate => accumulator(intent, SummaryKind::Rate, SummaryParams::Rate),
        AggIntent::Increase => {
            accumulator(intent, SummaryKind::Increase, SummaryParams::Increase)
        }

        // ── Exact, non-mergeable reducers — richer partial state than a
        //    single value (see `agg_is_mergeable`), so no accumulator form.
        AggIntent::Avg { .. } | AggIntent::StdDev { .. } | AggIntent::Variance { .. } => {
            Realization::PassThrough
        }

        // ── Classic-bucket histogram_quantile (#79): exact `le`-bucket
        //    interpolation over pre-aggregated counts — NOT re-sketchable.
        //    (The native/raw form lowers to the generic `Quantile` above.)
        AggIntent::HistogramQuantile { .. } => Realization::PassThrough,

        // ── Per-series transforms and reductions with no sketch realization:
        //    counter-derivatives (#44), math (#45), time/calendar (#46),
        //    presence (#47), native-histogram accessors (#43), and the
        //    `*OverTime` reducers (#51). All exact by construction.
        AggIntent::Changes
        | AggIntent::Delta
        | AggIntent::IDelta
        | AggIntent::Deriv
        | AggIntent::Resets
        | AggIntent::PredictLinear { .. }
        | AggIntent::DoubleExpSmoothing { .. }
        | AggIntent::HistogramCount
        | AggIntent::HistogramSum
        | AggIntent::HistogramAvg
        | AggIntent::HistogramStdDev
        | AggIntent::HistogramStdVar
        | AggIntent::HistogramFraction { .. }
        | AggIntent::Math(_)
        | AggIntent::Absent
        | AggIntent::AbsentOverTime
        | AggIntent::PresentOverTime
        | AggIntent::TimeFn(_)
        | AggIntent::LastOverTime
        | AggIntent::FirstOverTime
        | AggIntent::MadOverTime
        | AggIntent::TsOfMinOverTime
        | AggIntent::TsOfMaxOverTime
        | AggIntent::TsOfFirstOverTime
        | AggIntent::TsOfLastOverTime => Realization::PassThrough,

        // ── Group / count_values (#49): exact per `agg_is_exact`, but their
        //    output is structural (constant-1 / a synthesized label column),
        //    not a value a summary accumulator carries.
        AggIntent::Group | AggIntent::CountValues { .. } => Realization::PassThrough,
    }
}

/// Exact realization of an approximate-capable intent whose target is
/// `AccuracyTarget::Exact`. `Count` has a mergeable exact accumulator; exact
/// quantile / top-k / cardinality have no single-value summary form (they
/// need the full multiset / heap / set) and pass through.
fn exact_realization(intent: &AggIntent) -> Realization {
    match intent {
        AggIntent::Count { .. } => {
            accumulator(intent, SummaryKind::Count, SummaryParams::Count)
        }
        _ => Realization::PassThrough,
    }
}

fn accumulator(intent: &AggIntent, kind: SummaryKind, params: SummaryParams) -> Realization {
    // An exact accumulator is only sound when partial states merge
    // (`agg(A ∪ B) = combine(agg(A), agg(B))`).
    debug_assert!(agg_is_mergeable(intent), "accumulator for non-mergeable {intent:?}");
    Realization::ExactAccumulator { kind, params }
}

/// Bind the preferred candidate sketch, with parameters sized to the target.
fn bind_sketch(intent: &AggIntent, accuracy: &AccuracyTarget) -> Realization {
    let (eps, delta) = match accuracy {
        // Unreachable via `realize` (Exact routes to `exact_realization`);
        // degrade to the tightest parameters if called directly.
        AccuracyTarget::Exact => (f64::MIN_POSITIVE, DEFAULT_DELTA),
        AccuracyTarget::Epsilon(e) => (*e, DEFAULT_DELTA),
        AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, *delta),
    };
    let kind = sketch_candidates(intent)
        .first()
        .expect("approximate intent has at least one candidate sketch")
        .clone();
    let params = match kind {
        SummaryKind::Kll => SummaryParams::Kll { k: kll_k(eps) },
        SummaryKind::Cms => SummaryParams::Cms {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SummaryKind::Hll => SummaryParams::Hll {
            precision: hll_precision(eps),
        },
        SummaryKind::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SummaryParams::CmsWithHeap {
                width: cms_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
        // Non-preferred candidates (DDSketch / Theta / Kmv) are only reachable
        // once a cost model picks them; sized here so that wiring is local.
        SummaryKind::DDSketch => SummaryParams::DDSketch { alpha: eps },
        SummaryKind::Theta => SummaryParams::Theta { k: kmv_k(eps) },
        SummaryKind::Kmv => SummaryParams::Kmv { k: kmv_k(eps) },
        SummaryKind::Sum
        | SummaryKind::Count
        | SummaryKind::MinMax
        | SummaryKind::Increase
        | SummaryKind::Rate => unreachable!("exact accumulators are not sketch candidates"),
    };
    Realization::Sketch { kind, params }
}

// ── Parameter sizing ──────────────────────────────────────────────────────────
//
// Each function inverts the sketch family's standard error bound to the
// smallest parameter satisfying the target, clamped to the family's sane
// range. A non-positive ε saturates to the clamp maximum (tightest allowed).

/// KLL: rank error ε ≈ 2/k ⇒ `k = ⌈2/ε⌉`. ε = 0.01 → k = 200, matching the
/// design doc's worked example (`KLL{k=200}` satisfies ε=0.01).
fn kll_k(eps: f64) -> u32 {
    saturating_ceil(2.0 / eps, 8, 65_535)
}

/// HLL: standard error ≈ 1.04/√(2^p) ⇒ `p = ⌈log2((1.04/ε)²)⌉`. The default
/// `Cardinality` target (`asap-ir::default_cardinality`) inverts to p = 14.
fn hll_precision(eps: f64) -> u8 {
    saturating_ceil((1.04 / eps).powi(2).log2(), 4, 18) as u8
}

/// CMS: over-count ≤ ε·N with width `w = ⌈e/ε⌉` columns.
fn cms_width(eps: f64) -> u32 {
    saturating_ceil(std::f64::consts::E / eps, 2, 1 << 26)
}

/// CMS: failure probability ≤ δ with depth `d = ⌈ln(1/δ)⌉` rows.
/// δ = 0.01 → depth 5.
fn cms_depth(delta: f64) -> u32 {
    saturating_ceil((1.0 / delta).ln(), 1, 32)
}

/// KMV / theta: relative error ≈ 1/√k ⇒ `k = ⌈1/ε²⌉`.
fn kmv_k(eps: f64) -> u32 {
    saturating_ceil(1.0 / (eps * eps), 16, 1 << 26)
}

/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive x saturate to `hi`
/// (a degenerate ε means "as accurate as this family goes").
fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_ir::intent_algebra::agg_intent::{
        agg_is_exact, default_cardinality, default_quantile, MathFunc, TimeFunc,
    };

    fn eps(e: f64) -> AccuracyTarget {
        AccuracyTarget::Epsilon(e)
    }

    /// Shorthand for asserting the realization *category*.
    #[derive(Debug, PartialEq)]
    enum Cat {
        Sketch(SummaryKind),
        Acc(SummaryKind),
        Pass,
    }

    fn cat(intent: &AggIntent) -> Cat {
        match realize(intent) {
            Realization::Sketch { kind, .. } => Cat::Sketch(kind),
            Realization::ExactAccumulator { kind, .. } => Cat::Acc(kind),
            Realization::PassThrough => Cat::Pass,
        }
    }

    /// The `AggIntent → SummaryKind` coverage matrix (issue #98): every intent
    /// variant maps to a sketch, an exact accumulator, or an explicit
    /// pass-through. `realize`'s match is exhaustive, so a new variant cannot
    /// compile without a decision; this matrix pins what each decision *is*.
    #[test]
    fn agg_intent_to_summary_kind_coverage_matrix() {
        use AggIntent as A;
        use Cat::*;
        use SummaryKind as K;
        let matrix: Vec<(A, Cat)> = vec![
            // approximate-capable, at an ε target → sketch
            (default_quantile(0.99), Sketch(K::Kll)),
            (default_cardinality(), Sketch(K::Hll)),
            (A::Count { accuracy: eps(0.01) }, Sketch(K::Cms)),
            (A::TopK { k: 10, accuracy: eps(0.01) }, Sketch(K::CmsWithHeap)),
            // the same intents at Exact → exact realization
            (A::Quantile { col: None, q: 0.5, accuracy: AccuracyTarget::Exact }, Pass),
            (A::Cardinality { col: None, accuracy: AccuracyTarget::Exact }, Pass),
            (A::Count { accuracy: AccuracyTarget::Exact }, Acc(K::Count)),
            (A::TopK { k: 10, accuracy: AccuracyTarget::Exact }, Pass),
            // exact mergeable accumulators
            (A::Sum { col: None }, Acc(K::Sum)),
            (A::Min { col: None }, Acc(K::MinMax)),
            (A::Max { col: None }, Acc(K::MinMax)),
            (A::Rate, Acc(K::Rate)),
            (A::Increase, Acc(K::Increase)),
            // exact but non-mergeable → pass-through
            (A::Avg { col: None }, Pass),
            (A::StdDev { col: None, population: false }, Pass),
            (A::Variance { col: None, population: true }, Pass),
            // classic-bucket histogram_quantile is not re-sketchable (#79)
            (A::HistogramQuantile { q: 0.99 }, Pass),
            // counter-derivative / range-vector functions (#44)
            (A::Changes, Pass),
            (A::Delta, Pass),
            (A::IDelta, Pass),
            (A::Deriv, Pass),
            (A::Resets, Pass),
            (A::PredictLinear { seconds: 60.0 }, Pass),
            (A::DoubleExpSmoothing { smoothing: 0.5, trend: 0.5 }, Pass),
            // native-histogram accessors (#43)
            (A::HistogramCount, Pass),
            (A::HistogramSum, Pass),
            (A::HistogramAvg, Pass),
            (A::HistogramStdDev, Pass),
            (A::HistogramStdVar, Pass),
            (A::HistogramFraction { lower: 0.0, upper: 1.0 }, Pass),
            // per-sample transforms (#45, #46) + presence (#47)
            (A::Math(MathFunc::Abs), Pass),
            (A::TimeFn(TimeFunc::Hour), Pass),
            (A::Absent, Pass),
            (A::AbsentOverTime, Pass),
            (A::PresentOverTime, Pass),
            // extended aggregations (#49)
            (A::Group, Pass),
            (A::CountValues { label: "v".into() }, Pass),
            // additional range reducers (#51)
            (A::LastOverTime, Pass),
            (A::FirstOverTime, Pass),
            (A::MadOverTime, Pass),
            (A::TsOfMinOverTime, Pass),
            (A::TsOfMaxOverTime, Pass),
            (A::TsOfFirstOverTime, Pass),
            (A::TsOfLastOverTime, Pass),
        ];
        for (intent, expected) in &matrix {
            assert_eq!(&cat(intent), expected, "realization for {intent:?}");
        }
        // Every accumulator pick is mergeable; every sketch pick is on a
        // genuinely approximate target (the `agg_is_*` helpers stay truthful).
        for (intent, expected) in &matrix {
            if let Cat::Acc(_) = expected {
                assert!(agg_is_mergeable(intent), "{intent:?}");
            }
            if let Cat::Sketch(_) = expected {
                assert!(
                    !agg_is_exact(intent) || matches!(intent, AggIntent::Count { .. }),
                    "{intent:?} sketches only under an approximate target"
                );
            }
        }
    }

    #[test]
    fn accuracy_target_drives_the_boundary() {
        // Same intent, three targets → three different decisions.
        let exact = AggIntent::Quantile { col: None, q: 0.99, accuracy: AccuracyTarget::Exact };
        assert_eq!(realize(&exact), Realization::PassThrough);

        let approx = default_quantile(0.99); // ε = 0.01
        assert_eq!(
            realize(&approx),
            Realization::Sketch {
                kind: SummaryKind::Kll,
                params: SummaryParams::Kll { k: 200 }, // design.md worked example
            }
        );

        let looser = AggIntent::Quantile { col: None, q: 0.99, accuracy: eps(0.05) };
        assert_eq!(
            realize(&looser),
            Realization::Sketch {
                kind: SummaryKind::Kll,
                params: SummaryParams::Kll { k: 40 }, // ⌈2/0.05⌉
            }
        );
    }

    #[test]
    fn default_cardinality_inverts_to_hll_precision_14() {
        // `default_cardinality` encodes HLL's standard error at p=14; the
        // sizing must invert it back exactly.
        assert_eq!(
            realize(&default_cardinality()),
            Realization::Sketch {
                kind: SummaryKind::Hll,
                params: SummaryParams::Hll { precision: 14 },
            }
        );
    }

    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta { epsilon: 0.001, delta: 0.001 },
        };
        assert_eq!(
            realize(&intent),
            Realization::Sketch {
                kind: SummaryKind::Cms,
                params: SummaryParams::Cms { width: 2719, depth: 7 }, // ⌈e/0.001⌉, ⌈ln 1000⌉
            }
        );
        // Epsilon-only falls back to DEFAULT_DELTA → depth 5.
        let intent = AggIntent::Count { accuracy: eps(0.001) };
        assert_eq!(
            realize(&intent),
            Realization::Sketch {
                kind: SummaryKind::Cms,
                params: SummaryParams::Cms { width: 2719, depth: 5 },
            }
        );
    }

    #[test]
    fn topk_heap_size_tracks_k() {
        let intent = AggIntent::TopK { k: 25, accuracy: eps(0.01) };
        match realize(&intent) {
            Realization::Sketch {
                kind: SummaryKind::CmsWithHeap,
                params: SummaryParams::CmsWithHeap { width, depth, heap_size },
            } => {
                assert_eq!(heap_size, 25);
                assert_eq!(width, 272); // ⌈e/0.01⌉
                assert_eq!(depth, 5);
            }
            other => panic!("expected CmsWithHeap, got {other:?}"),
        }
    }

    #[test]
    fn candidate_lists_match_the_issue_map() {
        assert_eq!(
            sketch_candidates(&default_quantile(0.5)),
            &[SummaryKind::Kll, SummaryKind::DDSketch]
        );
        assert_eq!(
            sketch_candidates(&default_cardinality()),
            &[SummaryKind::Hll, SummaryKind::Theta, SummaryKind::Kmv]
        );
        assert_eq!(
            sketch_candidates(&AggIntent::TopK { k: 5, accuracy: eps(0.01) }),
            &[SummaryKind::CmsWithHeap]
        );
        assert_eq!(
            sketch_candidates(&AggIntent::Count { accuracy: eps(0.01) }),
            &[SummaryKind::Cms]
        );
        assert!(sketch_candidates(&AggIntent::Rate).is_empty());
    }

    #[test]
    fn degenerate_epsilon_saturates_to_tightest_params() {
        let intent = AggIntent::Quantile { col: None, q: 0.99, accuracy: eps(0.0) };
        assert_eq!(
            realize(&intent),
            Realization::Sketch {
                kind: SummaryKind::Kll,
                params: SummaryParams::Kll { k: 65_535 },
            }
        );
    }
}
