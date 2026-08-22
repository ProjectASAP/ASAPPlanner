//! Sketch-vs-exact boundary — the per-intent accuracy decision (issue #98).
//!
//! The per-node choice of how an [`AggIntent`] is *realised*: by an
//! approximate summary (sketch, sample, wavelet, statistical model, …), by
//! an exact mergeable accumulator, or by an ordinary exact operator
//! (pass-through). This is a post-ASAP concern: the pre-ASAP IR carries only
//! the intent + accuracy target, never the realization.
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
//! [`implementation_for`] is a single exhaustive match over the intent vocabulary, so a
//! new `AggIntent` variant fails to compile until it is given an explicit
//! realization — there is no silent fall-through. The [`bind`](crate::bind)
//! pass fires it per node over nested trees.
//!
//! Today's core dispatch only ever picks [`Implementation::ExactAggregate`],
//! [`Implementation::Sketch`], or [`Implementation::PassThrough`] — no
//! `AggIntent` variant maps to sampling/wavelet/statistical-model yet.
//! [`Implementation::Sample`]/[`Wavelet`](Implementation::Wavelet)/
//! [`StatModel`](Implementation::StatModel) exist so a deployment's own
//! [`CostModel::realize_extension`](crate::cost_model::CostModel::realize_extension)
//! can choose one of them for an `AggIntent::Extension` node — core has no
//! opinion on when that's the right choice.

use asap_types::post_asap::{
    ExactKind, ExactParams, SamplingKind, SamplingParams, SketchKind, SketchParams, StatModelKind,
    StatModelParams, WaveletKind, WaveletParams,
};
use asap_types::pre_asap::agg_intent::{agg_is_mergeable, AggIntent};
use asap_types::types::AccuracyTarget;

use crate::cost_model::{CostModel, DefaultCostModel};

/// How an [`AggIntent`] is realised at post-ASAP binding time.
#[derive(Debug, Clone, PartialEq)]
pub enum Implementation {
    /// An exact **mergeable** accumulator (partial state ≡ the value
    /// itself: `Sum` / `Count` / `MinMax` / `Rate` / `Increase`). The
    /// built state *is* the answer already — no `SummaryEstimate` readout
    /// step.
    ExactAggregate {
        kind: ExactKind,
        params: ExactParams,
    },
    /// An approximate sketch sized to the intent's [`AccuracyTarget`].
    /// Needs a `SummaryEstimate` readout to recover a value.
    Sketch {
        kind: SketchKind,
        params: SketchParams,
    },
    /// A sampling-based summary (a retained row subset). Needs a
    /// `SummaryEstimate` readout. Not chosen by any core `AggIntent`
    /// dispatch today — see the module docs.
    Sample {
        kind: SamplingKind,
        params: SamplingParams,
    },
    /// A wavelet-transform summary. Needs a `SummaryEstimate` readout. Not
    /// chosen by any core `AggIntent` dispatch today — see the module docs.
    Wavelet {
        kind: WaveletKind,
        params: WaveletParams,
    },
    /// A fitted statistical/parametric-model summary. Needs a
    /// `SummaryEstimate` readout. Not chosen by any core `AggIntent`
    /// dispatch today — see the module docs.
    StatModel {
        kind: StatModelKind,
        params: StatModelParams,
    },
    /// No summary form — the node stays a logical pre-ASAP operator and is
    /// executed exactly (per-series transforms, non-mergeable reducers, exact
    /// quantile/top-k/cardinality, classic-bucket `HistogramQuantile`, …).
    PassThrough,
}

/// Does an already-**available** [`Implementation`] — e.g. a summary
/// instance a downstream deployment already materialized somewhere, found
/// via whatever inventory/index that deployment keeps — satisfy a
/// **required** [`Implementation`] (what [`implementation_for`]/
/// [`implementation_for_with`] computed for some [`AggIntent`])?
///
/// This is the query-optimization-literature "materialized view matching"
/// / "answering queries using views" question, narrowed to this crate's
/// summary vocabulary: not "can I build this from scratch" (that's what
/// `implementation_for` answers) but "does something that already exists
/// answer this".
///
/// `asap-plan` deliberately ships no implementation of this trait and no
/// default method body — unlike [`implementation_for`], which decision an
/// available `Implementation` satisfies a required one is not a fact this
/// crate can settle on its own. Two real, reasonable answers already
/// diverge outside this crate:
///
/// - A **pure sketch-algebra** answer would say a `Sketch{kind: Kll, ..}`
///   requirement is satisfied by an available `DDSketch` (both quantile
///   sketches), and that a heap-bearing top-k sketch also answers a bare
///   frequency point-query (the heap is additional info on the same
///   underlying matrix) — but not the reverse.
/// - A **deployment with its own storage-layout rules** may need more:
///   e.g. whether a multi-population accumulator can serve a
///   single-population query via re-aggregation is a fact about that
///   deployment's storage layout, not about any summary family's kind at
///   all — a family's own kind doesn't encode grouping (grouping lives on
///   the post-ASAP node's `by` instead), so there is nothing in this
///   crate's own vocabulary to subsume.
///
/// Implementations are expected to consult `required`/`available`'s
/// `kind` (and whatever grouping/placement context the deployment tracks
/// alongside `Implementation`, which this trait's signature doesn't carry
/// because this crate has no inventory concept to carry it in).
pub trait Matcher {
    fn is_satisfied_by(&self, required: &Implementation, available: &Implementation) -> bool;
}

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::Epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch families that can serve an intent, most-preferred first.
/// This is the `AggIntent → SketchKind` map of issue #98; [`implementation_for`] binds
/// the head of the list. The tail entries are the alternatives a future cost
/// model (#6/#33) may pick instead — listed here so the candidate set has one
/// home.
pub fn summary_candidates(intent: &AggIntent) -> &'static [SketchKind] {
    match intent {
        AggIntent::Quantile { .. } => &[SketchKind::Kll, SketchKind::DDSketch],
        AggIntent::Cardinality { .. } => &[SketchKind::Hll, SketchKind::Theta, SketchKind::Kmv],
        // Count-Sketch-with-heap is CMS-with-heap's balanced/zero-mean-error
        // alternative for the same heavy-hitter shape.
        AggIntent::TopK { .. } => &[SketchKind::CmsWithHeap, SketchKind::CountSketchWithHeap],
        AggIntent::Count { .. } => &[SketchKind::Cms, SketchKind::CountSketch],
        _ => &[],
    }
}

/// The sketch-vs-exact boundary decision for one intent.
///
/// Exhaustive over the [`AggIntent`] vocabulary — adding a variant without an
/// explicit realization is a compile error, and the coverage-matrix test pins
/// each variant's category. Ranks candidate summaries via
/// [`DefaultCostModel`] (`asap-plan`'s built-in static preference order,
/// unchanged); use [`implementation_for_with`] to plug in a deployment-specific
/// [`CostModel`] instead.
pub fn implementation_for(intent: &AggIntent) -> Implementation {
    implementation_for_with(intent, &DefaultCostModel)
}

/// Like [`implementation_for`], but ranks candidate summaries via `cost_model` (see
/// [`crate::cost_model`]) instead of the built-in static preference order.
pub fn implementation_for_with(intent: &AggIntent, cost_model: &dyn CostModel) -> Implementation {
    match intent {
        // ── Approximate-capable intents — the AccuracyTarget decides ────────
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => match accuracy {
            AccuracyTarget::Exact => exact_realization(intent),
            _ => bind_summary_with(intent, accuracy, cost_model),
        },

        // ── Exact mergeable accumulators ─────────────────────────────────────
        AggIntent::Sum { .. } => exact_accumulator(intent, ExactKind::Sum, ExactParams::Sum),
        AggIntent::Min { .. } | AggIntent::Max { .. } => {
            exact_accumulator(intent, ExactKind::MinMax, ExactParams::MinMax)
        }
        AggIntent::Rate => exact_accumulator(intent, ExactKind::Rate, ExactParams::Rate),
        AggIntent::Increase => {
            exact_accumulator(intent, ExactKind::Increase, ExactParams::Increase)
        }

        // ── Exact, non-mergeable reducers — richer partial state than a
        //    single value (see `agg_is_mergeable`), so no accumulator form.
        AggIntent::Avg { .. } | AggIntent::StdDev { .. } | AggIntent::Variance { .. } => {
            Implementation::PassThrough
        }

        // ── Classic-bucket histogram_quantile (#79): exact `le`-bucket
        //    interpolation over pre-aggregated counts — NOT re-sketchable.
        //    (The native/raw form lowers to the generic `Quantile` above.)
        AggIntent::HistogramQuantile { .. } => Implementation::PassThrough,

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
        | AggIntent::TsOfLastOverTime => Implementation::PassThrough,

        // ── Group / count_values (#49): exact per `agg_is_exact`, but their
        //    output is structural (constant-1 / a synthesized label column),
        //    not a value a summary accumulator carries.
        AggIntent::Group | AggIntent::CountValues { .. } => Implementation::PassThrough,

        // ── Extension (deployment-model-specific, issue #131) — core has no
        //    realization opinion for a shape it doesn't know, so it defers
        //    entirely to the `CostModel` (issue #150): `realize_extension`
        //    defaults to `PassThrough`, preserving today's behavior for
        //    every deployment that doesn't override it. This is also the
        //    only path that can currently produce `Implementation::Sample`/
        //    `Wavelet`/`StatModel` — see the module docs.
        AggIntent::Extension { ext_kind, payload } => {
            cost_model.realize_extension(ext_kind, payload)
        }
    }
}

/// Exact realization of an approximate-capable intent whose target is
/// `AccuracyTarget::Exact`. `Count` has a mergeable exact accumulator; exact
/// quantile / top-k / cardinality have no single-value summary form (they
/// need the full multiset / heap / set) and pass through.
fn exact_realization(intent: &AggIntent) -> Implementation {
    match intent {
        AggIntent::Count { .. } => exact_accumulator(intent, ExactKind::Count, ExactParams::Count),
        _ => Implementation::PassThrough,
    }
}

fn exact_accumulator(intent: &AggIntent, kind: ExactKind, params: ExactParams) -> Implementation {
    // An exact accumulator is only sound when partial states merge
    // (`agg(A ∪ B) = combine(agg(A), agg(B))`).
    debug_assert!(
        agg_is_mergeable(intent),
        "accumulator for non-mergeable {intent:?}"
    );
    Implementation::ExactAggregate { kind, params }
}

/// Bind the preferred candidate sketch, with parameters sized to the
/// target, ranking [`summary_candidates`] via `cost_model` (see
/// [`crate::cost_model`]) instead of taking the static-order head
/// unconditionally.
fn bind_summary_with(
    intent: &AggIntent,
    accuracy: &AccuracyTarget,
    cost_model: &dyn CostModel,
) -> Implementation {
    let (eps, delta) = match accuracy {
        // Unreachable via `implementation_for` (Exact routes to `exact_realization`);
        // degrade to the tightest parameters if called directly.
        AccuracyTarget::Exact => (f64::MIN_POSITIVE, DEFAULT_DELTA),
        AccuracyTarget::Epsilon(e) => (*e, DEFAULT_DELTA),
        AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, *delta),
    };
    let ranked = cost_model.rank_candidates(intent, summary_candidates(intent));
    let kind = ranked
        .into_iter()
        .next()
        .expect("approximate intent has at least one candidate summary");
    let params = cost_model.size_params(kind.clone(), intent, eps, delta);
    Implementation::Sketch { kind, params }
}

/// `asap-plan`'s built-in `SketchParams` sizing, keyed off the resolved
/// `(eps, delta)` accuracy budget. [`CostModel::size_params`]'s default
/// body — factored out to a free function so a deployment's own
/// `CostModel` impl can still delegate to it for the candidates it
/// doesn't want to resize itself.
///
/// Each formula inverts the sketch family's standard error bound to the
/// smallest parameter satisfying the target, clamped to the family's sane
/// range. A non-positive ε saturates to the clamp maximum (tightest
/// allowed).
pub fn default_size_params(
    kind: SketchKind,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        SketchKind::Kll => SketchParams::Kll { k: kll_k(eps) },
        SketchKind::Cms => SketchParams::Cms {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::Hll => SketchParams::Hll {
            precision: hll_precision(eps),
        },
        SketchKind::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: cms_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
        // Non-preferred candidates (DDSketch / Theta / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchKind::DDSketch => SketchParams::DDSketch { alpha: eps },
        SketchKind::Theta => SketchParams::Theta { k: kmv_k(eps) },
        SketchKind::Kmv => SketchParams::Kmv { k: kmv_k(eps) },
        // Count-Sketch is CMS's balanced/zero-mean-error alternative —
        // same (width, depth) shape, sized the same way for now (a
        // Count-Sketch-specific bound uses an L2-norm error guarantee
        // rather than CMS's L1-norm one; this is a placeholder pending
        // that refinement, same status as the other non-preferred
        // candidates above).
        SketchKind::CountSketch => SketchParams::CountSketch {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            SketchParams::CountSketchWithHeap {
                width: cms_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
    }
}

/// A deployment's explicit bet about how "typical" (non-adversarial) its
/// workload's collision pattern is expected to be, consumed only by
/// [`posterior_aware_size_params`].
///
/// This is **not** derived from Chen et al.'s posterior-error-estimation
/// technique (issue #239, `asap_types::post_asap::error_estimation`) — that
/// technique computes a tighter bound *at query time* from a sketch's real
/// counter values, and this repo has no sketch runtime yet for a real
/// counter array to size against (see that module's docs). This struct is
/// this crate's own *plan-time* analogue of the same underlying intuition
/// — an expected-case (skewed / non-adversarial) workload needs a smaller
/// sketch than the adversarial worst case — expressed as an explicit,
/// caller-supplied assumption rather than anything observed or proven.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpectedCaseSizing {
    /// Fraction, in `(0, 1]`, of the traditional worst-case width
    /// ([`cms_width`]) the caller is betting is enough. `1.0` (or any
    /// value outside `(0, 1)`) reproduces the worst-case width exactly —
    /// no risk taken. A smaller value shrinks the sketch proportionally,
    /// at the cost documented on [`posterior_aware_size_params`].
    pub width_relaxation: f64,
}

/// Opt-in alternative to [`default_size_params`] for the CMS-family kinds
/// (`Cms` / `CmsWithHeap` / `CountSketch` / `CountSketchWithHeap`): sizes
/// width to `assumption.width_relaxation` of the worst-case [`cms_width`],
/// trading the unconditional worst-case `(ε,δ)` guarantee for a smaller
/// sketch under an explicit, caller-stated non-adversarial-workload bet —
/// see [`ExpectedCaseSizing`].
///
/// **The tradeoff, spelled out:** [`default_size_params`]'s width guarantees
/// `Pr[error > ε·|F|₁] < δ` for *any* input, including an adversarial one
/// built to maximize collisions (§3.3 of the posterior-error-estimation
/// paper this issue is about — see
/// `asap_types::post_asap::error_estimation`'s module docs). Shrinking
/// width below that only keeps the same `(ε,δ)` guarantee if the real
/// workload's collision load stays within `width_relaxation` of the
/// worst-case assumption — this function does not check that, cannot check
/// it (no data exists at plan time), and does not change the formal
/// guarantee's statement; it only changes how much hardware is spent
/// chasing it. Callers accept that gap explicitly by choosing
/// `width_relaxation < 1.0`.
///
/// Depth ([`cms_depth`]) is left unchanged from [`default_size_params`]:
/// depth trades away confidence *exponentially* (`Pr[all r rows bad] =
/// p^r` — each extra row multiplies the failure probability down), a
/// differently-shaped and materially riskier tradeoff than width's linear
/// relaxation. Issue #239 asks for *a* tighter-sizing option under a
/// stated assumption, not a full redesign of the depth/width tradeoff
/// space, so depth relaxation is left as explicit future scope.
///
/// For every `SketchKind` outside the CMS family, this is identical to
/// [`default_size_params`] — `width_relaxation` only ever touches the
/// [`cms_width`]-sized formulas this issue is about.
///
/// [`default_size_params`]'s own behavior is completely unchanged by this
/// function's existence — this is a separate, additive entry point, never
/// called from [`default_size_params`] or [`implementation_for`].
pub fn posterior_aware_size_params(
    kind: SketchKind,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
    assumption: ExpectedCaseSizing,
) -> SketchParams {
    let relaxed_width = |eps: f64| -> u32 {
        let base = cms_width(eps);
        let f = assumption.width_relaxation;
        if !(f.is_finite() && f > 0.0 && f < 1.0) {
            return base; // out-of-range bet: no relaxation, fall back to worst case
        }
        ((base as f64 * f).ceil() as u32).clamp(2, base)
    };
    match kind {
        SketchKind::Cms => SketchParams::Cms {
            width: relaxed_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: relaxed_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
        SketchKind::CountSketch => SketchParams::CountSketch {
            width: relaxed_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            SketchParams::CountSketchWithHeap {
                width: relaxed_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
        // Every other kind is untouched by this issue's CMS-specific
        // relaxation — defer to the existing formula verbatim.
        other => default_size_params(other, intent, eps, delta),
    }
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
    use asap_types::pre_asap::agg_intent::{
        agg_is_exact, default_cardinality, default_quantile, MathFunc, TimeFunc,
    };

    fn eps(e: f64) -> AccuracyTarget {
        AccuracyTarget::Epsilon(e)
    }

    /// Shorthand for asserting the realization *category*.
    #[derive(Debug, PartialEq)]
    enum Cat {
        Sketch(SketchKind),
        Acc(ExactKind),
        Pass,
    }

    fn cat(intent: &AggIntent) -> Cat {
        match implementation_for(intent) {
            Implementation::ExactAggregate { kind, .. } => Cat::Acc(kind),
            Implementation::Sketch { kind, .. } => Cat::Sketch(kind),
            Implementation::PassThrough => Cat::Pass,
            other => {
                panic!("this coverage matrix expects only Exact/Sketch/PassThrough, got {other:?}")
            }
        }
    }

    /// The `AggIntent → SummaryKind` coverage matrix (issue #98): every intent
    /// variant maps to a sketch, an exact accumulator, or an explicit
    /// pass-through. `implementation_for`'s match is exhaustive, so a new variant cannot
    /// compile without a decision; this matrix pins what each decision *is*.
    #[test]
    fn agg_intent_to_summary_kind_coverage_matrix() {
        use AggIntent as A;
        use Cat::*;
        use ExactKind as E;
        use SketchKind as K;
        let matrix: Vec<(A, Cat)> = vec![
            // approximate-capable, at an ε target → sketch
            (default_quantile(0.99), Sketch(K::Kll)),
            (default_cardinality(), Sketch(K::Hll)),
            (
                A::Count {
                    accuracy: eps(0.01),
                },
                Sketch(K::Cms),
            ),
            (
                A::TopK {
                    k: 10,
                    accuracy: eps(0.01),
                },
                Sketch(K::CmsWithHeap),
            ),
            // the same intents at Exact → exact realization
            (
                A::Quantile {
                    col: None,
                    q: 0.5,
                    accuracy: AccuracyTarget::Exact,
                },
                Pass,
            ),
            (
                A::Cardinality {
                    col: None,
                    accuracy: AccuracyTarget::Exact,
                },
                Pass,
            ),
            (
                A::Count {
                    accuracy: AccuracyTarget::Exact,
                },
                Acc(E::Count),
            ),
            (
                A::TopK {
                    k: 10,
                    accuracy: AccuracyTarget::Exact,
                },
                Pass,
            ),
            // exact mergeable accumulators
            (A::Sum { col: None }, Acc(E::Sum)),
            (A::Min { col: None }, Acc(E::MinMax)),
            (A::Max { col: None }, Acc(E::MinMax)),
            (A::Rate, Acc(E::Rate)),
            (A::Increase, Acc(E::Increase)),
            // exact but non-mergeable → pass-through
            (A::Avg { col: None }, Pass),
            (
                A::StdDev {
                    col: None,
                    population: false,
                },
                Pass,
            ),
            (
                A::Variance {
                    col: None,
                    population: true,
                },
                Pass,
            ),
            // classic-bucket histogram_quantile is not re-sketchable (#79)
            (A::HistogramQuantile { q: 0.99 }, Pass),
            // counter-derivative / range-vector functions (#44)
            (A::Changes, Pass),
            (A::Delta, Pass),
            (A::IDelta, Pass),
            (A::Deriv, Pass),
            (A::Resets, Pass),
            (A::PredictLinear { seconds: 60.0 }, Pass),
            (
                A::DoubleExpSmoothing {
                    smoothing: 0.5,
                    trend: 0.5,
                },
                Pass,
            ),
            // native-histogram accessors (#43)
            (A::HistogramCount, Pass),
            (A::HistogramSum, Pass),
            (A::HistogramAvg, Pass),
            (A::HistogramStdDev, Pass),
            (A::HistogramStdVar, Pass),
            (
                A::HistogramFraction {
                    lower: 0.0,
                    upper: 1.0,
                },
                Pass,
            ),
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
        let exact = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::Exact,
        };
        assert_eq!(implementation_for(&exact), Implementation::PassThrough);

        let approx = default_quantile(0.99); // ε = 0.01
        assert_eq!(
            implementation_for(&approx),
            Implementation::Sketch {
                kind: SketchKind::Kll,
                params: SketchParams::Kll { k: 200 }, // design.md worked example
            }
        );

        let looser = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: eps(0.05),
        };
        assert_eq!(
            implementation_for(&looser),
            Implementation::Sketch {
                kind: SketchKind::Kll,
                params: SketchParams::Kll { k: 40 }, // ⌈2/0.05⌉
            }
        );
    }

    #[test]
    fn default_cardinality_inverts_to_hll_precision_14() {
        // `default_cardinality` encodes HLL's standard error at p=14; the
        // sizing must invert it back exactly.
        assert_eq!(
            implementation_for(&default_cardinality()),
            Implementation::Sketch {
                kind: SketchKind::Hll,
                params: SketchParams::Hll { precision: 14 },
            }
        );
    }

    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.001,
                delta: 0.001,
            },
        };
        assert_eq!(
            implementation_for(&intent),
            Implementation::Sketch {
                kind: SketchKind::Cms,
                params: SketchParams::Cms {
                    width: 2719,
                    depth: 7
                }, // ⌈e/0.001⌉, ⌈ln 1000⌉
            }
        );
        // Epsilon-only falls back to DEFAULT_DELTA → depth 5.
        let intent = AggIntent::Count {
            accuracy: eps(0.001),
        };
        assert_eq!(
            implementation_for(&intent),
            Implementation::Sketch {
                kind: SketchKind::Cms,
                params: SketchParams::Cms {
                    width: 2719,
                    depth: 5
                },
            }
        );
    }

    #[test]
    fn topk_heap_size_tracks_k() {
        let intent = AggIntent::TopK {
            k: 25,
            accuracy: eps(0.01),
        };
        match implementation_for(&intent) {
            Implementation::Sketch {
                kind: SketchKind::CmsWithHeap,
                params:
                    SketchParams::CmsWithHeap {
                        width,
                        depth,
                        heap_size,
                    },
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
            summary_candidates(&default_quantile(0.5)),
            &[SketchKind::Kll, SketchKind::DDSketch]
        );
        assert_eq!(
            summary_candidates(&default_cardinality()),
            &[SketchKind::Hll, SketchKind::Theta, SketchKind::Kmv]
        );
        assert_eq!(
            summary_candidates(&AggIntent::TopK {
                k: 5,
                accuracy: eps(0.01)
            }),
            &[SketchKind::CmsWithHeap, SketchKind::CountSketchWithHeap]
        );
        assert_eq!(
            summary_candidates(&AggIntent::Count {
                accuracy: eps(0.01)
            }),
            &[SketchKind::Cms, SketchKind::CountSketch]
        );
        assert!(summary_candidates(&AggIntent::Rate).is_empty());
    }

    #[test]
    fn degenerate_epsilon_saturates_to_tightest_params() {
        let intent = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: eps(0.0),
        };
        assert_eq!(
            implementation_for(&intent),
            Implementation::Sketch {
                kind: SketchKind::Kll,
                params: SketchParams::Kll { k: 65_535 },
            }
        );
    }

    // ── posterior_aware_size_params (issue #239, integration point 2) ──────

    fn count_intent(e: f64) -> AggIntent {
        AggIntent::Count { accuracy: eps(e) }
    }

    #[test]
    fn posterior_aware_sizing_shrinks_width_under_stated_assumption() {
        let intent = count_intent(0.01);
        let worst_case = default_size_params(SketchKind::Cms, &intent, 0.01, 0.01);
        let relaxed = posterior_aware_size_params(
            SketchKind::Cms,
            &intent,
            0.01,
            0.01,
            ExpectedCaseSizing {
                width_relaxation: 0.5,
            },
        );
        match (worst_case, relaxed) {
            (
                SketchParams::Cms {
                    width: w0,
                    depth: d0,
                },
                SketchParams::Cms {
                    width: w1,
                    depth: d1,
                },
            ) => {
                assert!(
                    w1 < w0,
                    "expected relaxed width {w1} to be strictly smaller than worst-case {w0}"
                );
                assert_eq!(d0, d1, "depth must be unaffected by width_relaxation");
            }
            other => panic!("expected Cms/Cms pair, got {other:?}"),
        }
    }

    #[test]
    fn posterior_aware_sizing_at_full_relaxation_matches_worst_case() {
        // width_relaxation = 1.0 must reproduce default_size_params exactly
        // — the "no risk taken" boundary.
        let intent = count_intent(0.01);
        let worst_case = default_size_params(SketchKind::Cms, &intent, 0.01, 0.01);
        let relaxed = posterior_aware_size_params(
            SketchKind::Cms,
            &intent,
            0.01,
            0.01,
            ExpectedCaseSizing {
                width_relaxation: 1.0,
            },
        );
        assert_eq!(worst_case, relaxed);
    }

    #[test]
    fn posterior_aware_sizing_invalid_relaxation_falls_back_to_worst_case() {
        let intent = count_intent(0.01);
        let worst_case = default_size_params(SketchKind::Cms, &intent, 0.01, 0.01);
        for bad in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
            let relaxed = posterior_aware_size_params(
                SketchKind::Cms,
                &intent,
                0.01,
                0.01,
                ExpectedCaseSizing {
                    width_relaxation: bad,
                },
            );
            assert_eq!(
                worst_case, relaxed,
                "width_relaxation={bad} should fall back to the worst-case width"
            );
        }
    }

    #[test]
    fn posterior_aware_sizing_applies_to_every_cms_family_kind() {
        let cms_heap_intent = AggIntent::TopK {
            k: 7,
            accuracy: eps(0.01),
        };
        let assumption = ExpectedCaseSizing {
            width_relaxation: 0.25,
        };
        // CountSketch
        assert_eq!(
            posterior_aware_size_params(
                SketchKind::CountSketch,
                &count_intent(0.01),
                0.01,
                0.01,
                assumption
            ),
            SketchParams::CountSketch {
                width: 68,
                depth: 5
            }, // ceil(272 * 0.25)
        );
        // CmsWithHeap / CountSketchWithHeap carry k through untouched.
        match posterior_aware_size_params(
            SketchKind::CmsWithHeap,
            &cms_heap_intent,
            0.01,
            0.01,
            assumption,
        ) {
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            } => {
                assert_eq!(width, 68);
                assert_eq!(depth, 5);
                assert_eq!(heap_size, 7);
            }
            other => panic!("expected CmsWithHeap, got {other:?}"),
        }
    }

    #[test]
    fn posterior_aware_sizing_leaves_non_cms_kinds_unchanged() {
        // Kll/Hll/etc. have no width_relaxation concept — must be byte-for-
        // byte identical to default_size_params.
        let intent = default_quantile(0.99);
        let assumption = ExpectedCaseSizing {
            width_relaxation: 0.1,
        };
        assert_eq!(
            posterior_aware_size_params(SketchKind::Kll, &intent, 0.01, 0.01, assumption),
            default_size_params(SketchKind::Kll, &intent, 0.01, 0.01),
        );
    }

    #[test]
    fn default_size_params_unchanged_by_new_function_existing() {
        // Regression pin: default_size_params's own worst-case behavior for
        // existing callers must be untouched by adding
        // posterior_aware_size_params alongside it.
        assert_eq!(
            default_size_params(SketchKind::Cms, &count_intent(0.001), 0.001, 0.001),
            SketchParams::Cms {
                width: 2719,
                depth: 7
            },
        );
    }
}
