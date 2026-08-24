//! Posterior (query-time) sketch error estimation — Chen, Wu, Yang, Jiang,
//! Liu, ["Precise Error Estimation for Sketch-based Flow
//! Measurement"](https://zaoxing.github.io/papers/2021/IMC21_ErrorEstimation.pdf)
//! (IMC '21). Tracks issue #239.
//!
//! ## What this is
//!
//! The traditional CMS/Count-Sketch/CU-Sketch guarantee is an *a priori*,
//! worst-case bound derived before any data is seen: e.g. classic CMS sizing
//! (`w = ⌈e/ε⌉` counters/row, `r = ⌈ln(1/δ)⌉` rows — exactly
//! `crates/asap-aware-mapping/src/implementation.rs`'s `cms_width`/`cms_depth`)
//! guarantees `Pr[error > ε·|F|₁] < δ` obliviously to the real data
//! distribution, by construction assuming the adversarial worst case (§3,
//! §3.3 of the paper).
//!
//! The paper's insight: once a sketch has actually ingested data, its *real*
//! counter values already encode a tighter, still-rigorous bound — no need
//! to fall back to the worst case. [`cms_posterior_error_bound`] implements
//! that estimator (§3.1's Algorithm 1): given one sketch row's `w` counter
//! values and the target confidence `1−δ`, it returns the
//! `⌊w·δ^(1/r)⌋`-th largest counter in that row as the error bound, valid
//! with confidence `1−δ` for *any* flow's estimate (not just the one
//! queried) — proved in §3.2 (Theorems 3.1/3.2/3.4) to closely approximate
//! the true ground-truth error bound (bias `O(1/√w)`, Eq. 5), and in §3.3
//! (Eq. 6) to always be at least as tight as the traditional a priori bound
//! for the same `(w, r)`. [`traditional_a_priori_bound`] and
//! [`classic_cms_sizing`] implement that traditional bound/sizing for
//! comparison — see [`cms_posterior_bound_is_at_most_traditional_bound`]
//! below for the checked property.
//!
//! Appendix A.1 generalizes the technique: [`cu_sketch_posterior_error_bound`]
//! (Algorithm 2) is — per the paper's own text — *identical* to
//! [`cms_posterior_error_bound`], since CU-Sketch shares CM-Sketch's
//! "minimum counter across rows" query paradigm.
//! [`count_sketch_posterior_error_bound`] (Algorithm 3) is Count-Sketch's
//! variant: because Count-Sketch estimates a flow's size as the *median* of
//! its `r` signed counters rather than the minimum, the estimator instead
//! searches for the smallest fractile `p₀` whose two-sided binomial-tail
//! probability clears `δ` (Theorem A.1, `r = 2k+1` odd only — see that
//! function's docs for why even `r` is out of scope here).
//!
//! ## What this is *not* — no runtime sketch exists yet to wire this into
//!
//! This issue names two possible integration points: (1) runtime/readout-time
//! accuracy reporting from a sketch's *actual* counters, and (2) tighter
//! plan-time sizing. As of this module landing, **this repository has no
//! vendored CMS/CountSketch/CU-Sketch runtime and no counter-array data
//! structure anywhere** — confirmed by inspecting
//! `crates/types/src/post_asap/sketch.rs` (`SketchAlgorithm`, `SketchParams`,
//! this module's neighbors) and `crates/asap-aware-mapping/src/implementation.rs`
//! (`default_size_params`, `cms_width`, `cms_depth`): both are purely
//! planning-time sizing metadata. There is no `A[row][col]` counter matrix
//! anywhere in the workspace for these functions to be handed at query
//! time. So integration point (1) — reporting an *actual* query's posterior
//! error from real counters at readout — has nothing to wire into today.
//!
//! The functions here are deliberately **sketch-object-agnostic**: they take
//! plain counter slices (`&[u64]` / `&[i64]`) and numeric parameters, not a
//! concrete sketch type, specifically so that the moment a real CMS/
//! Count-Sketch/CU-Sketch runtime lands in this workspace, its readout path
//! can call these functions directly on its real counter arrays with zero
//! changes needed here. That wiring is out of scope for this module — see
//! issue #239.
//!
//! Integration point (2) — tighter *plan-time* sizing under an expected-case
//! (non-adversarial) assumption — is wired for real, since it touches code
//! that already exists: see
//! `crates/asap-aware-mapping/src/implementation.rs::posterior_aware_size_params`.

// ── Posterior (query-time) estimators ───────────────────────────────────────

/// Count-Min Sketch posterior error estimator — §3.1, Algorithm 1.
///
/// `row` is one sketch row's `w` raw counter values (any of the sketch's `r`
/// rows — the algorithm is defined per-row and every row gives an
/// independent, equally valid estimate). `rows` is the sketch's total row
/// count `r` (used only to convert the target confidence into the
/// per-row fractile `p = δ^(1/r)`, per the union-bound argument in §3.1:
/// "the probability δ that all r corresponding counters are not bounded by
/// g(δ) is p^r"). `delta` is the target failure probability `δ` (so the
/// returned bound holds with confidence `1−δ`).
///
/// Returns the `⌊w·p⌋`-th largest value in `row` (1-indexed, clamped to
/// `[1, w]`), matching the paper's own descriptive prose in §3.1 ("report
/// the ⌊wp⌋-th largest counter as our estimation for g(δ)"). Note: the
/// paper's Algorithm 1 pseudocode box instead prints `⌈wp⌉` (ceiling) — the
/// paper is internally inconsistent between its prose and its pseudocode
/// box on this rounding direction. This implementation follows the prose
/// (and issue #239's own paraphrase of it); the two conventions differ by
/// at most one rank, well within the paper's own `O(1/√w)` bias bound
/// (Eq. 5), so the choice does not affect any of this module's correctness
/// properties.
///
/// Returns `None` for degenerate inputs: an empty row, zero rows, or `delta`
/// outside `(0, 1]`.
pub fn cms_posterior_error_bound(row: &[u64], rows: u32, delta: f64) -> Option<u64> {
    let idx = posterior_rank(row.len(), rows, delta)?;
    Some(kth_largest(row, idx))
}

/// CU-Sketch posterior error estimator — Appendix A.1, Algorithm 2.
///
/// Per the paper: "the algorithm for CU-Sketch is exactly the same as that
/// for CM-Sketch due to their similar properties in generating sketch: they
/// share a common query paradigm that returns the minimum counter value
/// among the `r` rows as the estimated flow size." This is a distinctly
/// named entry point (matching the paper's own Algorithm 2 naming) that
/// delegates to [`cms_posterior_error_bound`] rather than duplicating its
/// logic — see that function's docs for the full parameter/behavior
/// contract, which applies verbatim here.
pub fn cu_sketch_posterior_error_bound(row: &[u64], rows: u32, delta: f64) -> Option<u64> {
    cms_posterior_error_bound(row, rows, delta)
}

/// Count-Sketch posterior error estimator — Appendix A.1, Algorithm 3.
///
/// Count-Sketch counters are signed (each row's hash also picks a random
/// sign, so a flow's contribution can subtract as well as add — "balanced/
/// zero-mean-error" per `SketchAlgorithm::CountSketch`'s own doc), and a flow's
/// size is estimated as the *median*, not the minimum, of its `r` per-row
/// counters. That breaks Algorithm 1/2's simple `p = δ^(1/r)` derivation (a
/// union bound over "any row is bad"): the median needs *more than half* the
/// rows to be bad, so Algorithm 3 instead searches for the smallest
/// fractile `p₀ ∈ {2/w, 3/w, …}` whose two-sided binomial-tail probability —
/// the chance that more than half of `r` independent `Bernoulli(p₀/2)`
/// trials succeed — first reaches `δ`, then reports the `⌈w·p₀⌉`-th largest
/// *absolute* counter value at that `p₀` (`row` here is `A[1][1..w]`,
/// unsigned by absolute value, matching Algorithm 3's
/// `SortToDescendingOrder(|A[1][1]| … |A[1][w]|)`).
///
/// Theorem A.1 states the optimal bound only for `r = 2k+1` (odd); the
/// paper's own footnote for the even case ("Similar equation for an even
/// r") does not spell out the formula. Per issue #239's instruction to
/// "only implement what you can read and verify" rather than guess, this
/// function requires odd `rows` and returns `None` for even `rows` — a
/// documented scope boundary, not an oversight.
///
/// Returns `None` for: an empty `row`, `rows == 0`, even `rows`, or `delta`
/// outside `(0, 1]`.
pub fn count_sketch_posterior_error_bound(row: &[i64], rows: u32, delta: f64) -> Option<u64> {
    let w = row.len();
    if w == 0 || rows == 0 || rows.is_multiple_of(2) || !(delta > 0.0 && delta <= 1.0) {
        return None;
    }
    let r = rows as u64;
    // Majority threshold: for r = 2k+1, this is k+1 — the smallest j for
    // which "j of r rows" is a strict majority. Also correct (by the same
    // "more than half" reading) for the even-r case this function declines
    // to handle, but we never reach here with even r.
    let j_start = r.div_ceil(2);

    // `C(r, j)` for every `j` in `j_start..=r`, computed once via the
    // incremental recurrence below (O(r) total) instead of letting
    // `binomial_tail` recompute `binomial_coeff` (itself O(r)) from
    // scratch for every `j` on every fractile tried — the fractile search
    // just below evaluates the tail up to `log2(w)` times, so hoisting
    // this out turns each evaluation's cost from O(r²) into O(r).
    let coeffs = binomial_coeffs_from(r, j_start);

    // `2 * binomial_tail(p0/2)` is monotonically non-decreasing in `p0`
    // (raising each row's bad-probability can only raise the chance that a
    // majority of rows are bad), so the smallest qualifying fractile
    // `p0 = (i+1)/w` is a binary search over `i`, not a linear scan —
    // O(log w) tail evaluations instead of O(w).
    let satisfies = |i: usize| -> bool {
        let p0 = ((i + 1) as f64 / w as f64).min(1.0);
        2.0 * binomial_tail_with_coeffs(&coeffs, r, j_start, p0 / 2.0) >= delta
    };
    let mut lo = 1usize;
    let mut hi = w;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if satisfies(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    // `lo == hi`: either `satisfies(lo)`, or `lo == w` and the search
    // never found one — matching the original linear scan's fallback,
    // which left `chosen_p0` at its pre-loop-initialized `1.0` in that
    // case (the same value `p0(w)` evaluates to below).
    let chosen_p0 = ((lo + 1) as f64 / w as f64).min(1.0);

    let idx = ((w as f64) * chosen_p0).ceil() as usize;
    let idx = idx.clamp(1, w);
    let abs_row: Vec<u64> = row.iter().map(|v| v.unsigned_abs()).collect();
    Some(kth_largest(&abs_row, idx))
}

// ── Traditional a priori bound (§3.3), for comparison ──────────────────────

/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive `x` saturate to `hi` (a
/// degenerate accuracy target means "as accurate as this family goes").
/// Byte-for-byte the same policy as `implementation.rs`'s private
/// `saturating_ceil` — duplicated here, not imported, for the same
/// layering reason [`classic_cms_sizing`] itself is duplicated rather than
/// calling `implementation.rs` directly.
fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}

/// The classic CMS `(ε, δ)` sizing formula (§3.3, restated just before
/// Eq. 6; identical — formula *and* clamping — to
/// `crates/asap-aware-mapping/src/implementation.rs`'s private
/// `cms_width`/`cms_depth`, reimplemented here — deliberately, not by
/// accident — because `asap-types` sits below `asap-aware-mapping` in the
/// workspace's dependency layering and cannot import from it): `w = ⌈e/ε⌉`
/// counters/row, clamped to `[2, 2²⁶]`; `r = ⌈ln(1/δ)⌉` rows, clamped to
/// `[1, 32]`.
///
/// Returns `(width, depth)`. Matching `implementation.rs`'s own clamp semantics
/// exactly (not just its in-range formula) matters here specifically:
/// this function's whole purpose is giving comparison/test code (and,
/// per issue #250, a future replan) the traditional bound to compare
/// this module's posterior bound against — an un-clamped reimplementation
/// would silently diverge from `implementation.rs`'s real sizing outside a
/// narrow "nothing saturates" range of `(eps, delta)`, exactly the range
/// most tests default to, making the divergence easy to miss.
pub fn classic_cms_sizing(eps: f64, delta: f64) -> (u32, u32) {
    let width = saturating_ceil(std::f64::consts::E / eps, 2, 1 << 26);
    let depth = saturating_ceil((1.0 / delta).ln(), 1, 32);
    (width, depth)
}

/// The traditional a priori bound value itself: `ε·|F|₁`, the classic CMS
/// guarantee's right-hand side (§3.3: "the original CM bound … guarantees
/// `Pr[𝕏ₑᵢ > ε|F|₁] < δ`"). Exists so a posterior bound (in the same
/// counter units as `total_flow_size`) can be compared directly against the
/// traditional worst-case bound it is meant to improve on — see
/// [`cms_posterior_bound_is_at_most_traditional_bound`]'s test below.
pub fn traditional_a_priori_bound(total_flow_size: u64, eps: f64) -> f64 {
    eps * total_flow_size as f64
}

// ── Shared internals ─────────────────────────────────────────────────────────

/// `⌊w·δ^(1/r)⌋`, clamped to `[1, w]` (1-indexed rank into a
/// descending-sorted row of `w` counters). `None` for degenerate inputs.
fn posterior_rank(w: usize, rows: u32, delta: f64) -> Option<usize> {
    if w == 0 || rows == 0 || !(delta > 0.0 && delta <= 1.0) {
        return None;
    }
    let p = delta.powf(1.0 / rows as f64);
    let idx = ((w as f64) * p).floor() as i64;
    Some((idx.max(1) as usize).min(w))
}

/// The `k`-th largest value in `values` (1-indexed): sorts a copy in
/// descending order and returns `sorted[k-1]`. `k` is expected already
/// clamped to `[1, values.len()]` by the caller.
/// The `k`-th largest value in `values` (1-indexed: `k=1` is the max).
/// `select_nth_unstable_by` partitions in O(w) average instead of fully
/// sorting in O(w log w) — this only ever needs one rank, not a total
/// order, and both call sites (this module's per-query readout math) are
/// documented as meant to run on a future runtime's hot readout path.
fn kth_largest(values: &[u64], k: usize) -> u64 {
    let mut buf: Vec<u64> = values.to_vec();
    let idx = k - 1;
    let (_, &mut kth, _) = buf.select_nth_unstable_by(idx, |a, b| b.cmp(a));
    kth
}

/// `Σ_{j=j_start}^{r} C(r, j) · p^j · (1-p)^(r-j)` — the upper binomial tail
/// probability, given `coeffs[i] = C(r, j_start + i)` already computed by
/// [`binomial_coeffs_from`]. Split out from the coefficient computation so
/// a caller trying several `p` values against the same `(r, j_start)` (as
/// [`count_sketch_posterior_error_bound`]'s fractile search does) pays for
/// the coefficients once, not once per `p`.
fn binomial_tail_with_coeffs(coeffs: &[f64], r: u64, j_start: u64, p: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    coeffs
        .iter()
        .enumerate()
        .map(|(offset, &c)| {
            let j = j_start + offset as u64;
            c * p.powi(j as i32) * (1.0 - p).powi((r - j) as i32)
        })
        .sum()
}

/// `C(r, j)` for every `j` in `j_start..=r`, in one O(r) pass via the
/// incremental recurrence `C(r,j) = C(r,j-1) · (r-j+1)/j` — instead of
/// calling [`binomial_coeff`] (itself O(r)) fresh for every `j`, which is
/// what made evaluating a single tail probability O(r²).
fn binomial_coeffs_from(r: u64, j_start: u64) -> Vec<f64> {
    let mut coeffs = Vec::with_capacity((r - j_start + 1) as usize);
    let mut c = binomial_coeff(r, j_start);
    coeffs.push(c);
    for j in (j_start + 1)..=r {
        c = c * (r - j + 1) as f64 / j as f64;
        coeffs.push(c);
    }
    coeffs
}

/// `C(n, k)`, computed as an iterative running product in `f64` (avoids
/// factorial overflow; exact for the small `n` realistic sketch depths use,
/// and only ever used as a probability-mass weight so `f64` rounding is
/// immaterial).
fn binomial_coeff(n: u64, k: u64) -> f64 {
    let k = k.min(n - k);
    let mut result = 1.0_f64;
    for i in 0..k {
        result = result * (n - i) as f64 / (i + 1) as f64;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Algorithm 1 (CM-Sketch) — hand-computable examples ──────────────────

    #[test]
    fn cms_single_row_confidence_one_picks_the_largest_counter() {
        // rows=1, delta=1 ⇒ p = 1^(1/1) = 1 ⇒ index = floor(w*1) = w ⇒ the
        // w-th largest = the *smallest* counter (delta=1 means "no
        // confidence required", so the loosest possible bound is fine —
        // this pins the boundary behavior, not a meaningful confidence).
        let row = [10u64, 4, 7, 1];
        assert_eq!(cms_posterior_error_bound(&row, 1, 1.0), Some(1));
    }

    #[test]
    fn cms_hand_computed_example() {
        // r=2, delta=0.25 ⇒ p = 0.25^(1/2) = 0.5. w=10 ⇒ index = floor(10*0.5) = 5.
        // Descending-sorted row: [10,9,8,7,6,5,4,3,2,1] ⇒ 5th largest = 6.
        let row: Vec<u64> = (1..=10).collect(); // [1..10]
        assert_eq!(cms_posterior_error_bound(&row, 2, 0.25), Some(6));
    }

    #[test]
    fn cms_rank_clamped_to_at_least_one() {
        // Very small p (large r, small delta) floors to 0 — must clamp to
        // rank 1 (the single largest counter), not panic / index -1.
        let row = [5u64, 100, 1];
        let bound = cms_posterior_error_bound(&row, 50, 1e-9).unwrap();
        assert_eq!(bound, 100); // the largest counter
    }

    #[test]
    fn cms_all_zero_counters_bound_is_zero() {
        let row = [0u64; 8];
        assert_eq!(cms_posterior_error_bound(&row, 3, 0.05), Some(0));
    }

    #[test]
    fn cms_degenerate_inputs_return_none() {
        assert_eq!(cms_posterior_error_bound(&[], 3, 0.05), None);
        assert_eq!(cms_posterior_error_bound(&[1, 2, 3], 0, 0.05), None);
        assert_eq!(cms_posterior_error_bound(&[1, 2, 3], 3, 0.0), None);
        assert_eq!(cms_posterior_error_bound(&[1, 2, 3], 3, -0.1), None);
        assert_eq!(cms_posterior_error_bound(&[1, 2, 3], 3, 1.5), None);
    }

    #[test]
    fn cms_monotonic_in_width_tighter_bound_as_w_grows() {
        // Hold the *total* collided mass fixed and spread it over more
        // counters: a wider sketch means less collision per counter for
        // the same workload, so the reported bound should not increase.
        // (Using row = 1..=w instead — growing *with* w — would grow the
        // total mass too, which is a different, unrelated effect; the
        // fixed-total-uniform-spread row below isolates width alone.)
        let total = 20_160u64; // divisible by every width below
        let mut previous = u64::MAX;
        for w in [10usize, 20, 40, 80, 160] {
            let row = vec![total / w as u64; w];
            let bound = cms_posterior_error_bound(&row, 4, 0.1).unwrap();
            assert!(
                bound <= previous,
                "bound grew from {previous} to {bound} as w increased to {w}"
            );
            previous = bound;
        }
    }

    #[test]
    fn cms_monotonic_in_rows_tighter_bound_as_r_grows() {
        // Algorithm 1 computes its bound from *one* row's counters; `rows`
        // (r) only feeds into p = delta^(1/r). For delta < 1, delta^(1/r)
        // increases toward 1 as r grows (more rows ⇒ the union bound over
        // "any row is bad" needs a looser per-row p to keep the same
        // overall delta), which raises the rank index ⌊w·p⌋ — i.e. r is a
        // confidence dial that monotonically raises the rank (and, on a
        // descending-sorted row, that means an equal-or-tighter reported
        // bound). We assert the rank itself grows monotonically with r,
        // which is the exact, documented relationship.
        let row: Vec<u64> = (1..=100).rev().collect();
        let mut previous_idx = 0usize;
        for r in [1u32, 2, 4, 8, 16, 32] {
            let idx = posterior_rank(row.len(), r, 0.1).unwrap();
            assert!(
                idx >= previous_idx,
                "rank shrank from {previous_idx} to {idx} as r grew to {r}"
            );
            previous_idx = idx;
        }
    }

    // ── Algorithm 2 (CU-Sketch) — identical to Algorithm 1 ─────────────────

    #[test]
    fn cu_sketch_matches_cms_exactly() {
        let row: Vec<u64> = (1..=50).collect();
        for (r, delta) in [(1u32, 0.5), (3, 0.1), (7, 0.01)] {
            assert_eq!(
                cu_sketch_posterior_error_bound(&row, r, delta),
                cms_posterior_error_bound(&row, r, delta)
            );
        }
    }

    // ── Algorithm 3 (Count-Sketch) ───────────────────────────────────────────

    #[test]
    fn count_sketch_rejects_even_rows() {
        let row = [3i64, -5, 2, 8];
        assert_eq!(count_sketch_posterior_error_bound(&row, 4, 0.05), None);
    }

    #[test]
    fn count_sketch_uses_absolute_values() {
        // Symmetric-magnitude row: signs shouldn't change the bound.
        let row_pos: Vec<i64> = (1..=21).collect();
        let row_neg: Vec<i64> = (1..=21).map(|v| -v).collect();
        let a = count_sketch_posterior_error_bound(&row_pos, 5, 0.05);
        let b = count_sketch_posterior_error_bound(&row_neg, 5, 0.05);
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn count_sketch_degenerate_inputs_return_none() {
        assert_eq!(count_sketch_posterior_error_bound(&[], 3, 0.05), None);
        assert_eq!(
            count_sketch_posterior_error_bound(&[1, 2, 3], 0, 0.05),
            None
        );
        assert_eq!(count_sketch_posterior_error_bound(&[1, 2, 3], 3, 0.0), None);
        assert_eq!(count_sketch_posterior_error_bound(&[1, 2, 3], 3, 1.5), None);
    }

    #[test]
    fn count_sketch_single_row_extreme_confidence() {
        // r=1 (odd), delta=1: the loosest possible ask — should resolve to
        // *some* valid rank in range without panicking.
        let row: Vec<i64> = vec![9, -4, 7, -1, 3];
        let bound = count_sketch_posterior_error_bound(&row, 1, 1.0).unwrap();
        assert!(row.iter().any(|v| v.unsigned_abs() == bound));
    }

    #[test]
    fn count_sketch_monotonic_in_width() {
        // Same fixed-total-uniform-spread construction as the CMS
        // monotonicity test above — isolates width's effect from total
        // mass's.
        let total = 20_160i64;
        let mut previous = u64::MAX;
        for w in [10usize, 20, 40, 80, 160] {
            let row = vec![total / w as i64; w];
            let bound = count_sketch_posterior_error_bound(&row, 5, 0.1).unwrap();
            assert!(
                bound <= previous,
                "bound grew from {previous} to {bound} as w increased to {w}"
            );
            previous = bound;
        }
    }

    // ── Traditional bound / classic sizing ──────────────────────────────────

    #[test]
    fn classic_cms_sizing_matches_implementation_rs_formula() {
        // Same worked example as
        // `asap-aware-mapping::implementation::tests::epsilon_delta_sizes_cms_depth`
        // (eps=0.001, delta=0.001 ⇒ width=2719, depth=7), pinned here too so
        // the two independent (layering-forced) reimplementations can't
        // silently drift apart undetected.
        assert_eq!(classic_cms_sizing(0.001, 0.001), (2719, 7));
        assert_eq!(classic_cms_sizing(0.01, 0.01), (272, 5));
    }

    #[test]
    fn classic_cms_sizing_degenerate_inputs_saturate_like_implementation_rs() {
        // Degenerate width/depth saturate to their hi clamp (2^26 / 32),
        // matching `implementation.rs`'s `saturating_ceil` exactly — not `0` (see
        // the correctness fix on `classic_cms_sizing`'s doc comment: an
        // earlier version returned `(0, 0)` here, silently diverging from
        // `implementation.rs`'s real degenerate-input behavior).
        assert_eq!(classic_cms_sizing(0.0, 0.01), (1 << 26, 5));
        assert_eq!(classic_cms_sizing(0.01, 0.0), (272, 32));
        assert_eq!(classic_cms_sizing(0.01, 1.0), (272, 32));
        assert_eq!(classic_cms_sizing(f64::NAN, 0.01), (1 << 26, 5));
    }

    /// Pinned extreme-range values — the clamp actually engaging, not just
    /// the in-range formula — computed by hand against the same
    /// `[2, 2²⁶]` / `[1, 32]` bounds `implementation.rs`'s `cms_width`/`cms_depth`
    /// use, so a future edit that reintroduces the un-clamped bug (an
    /// earlier version of this function silently diverged from
    /// `implementation.rs` outside the narrow range most other tests exercise)
    /// gets caught here.
    #[test]
    fn classic_cms_sizing_clamps_extreme_ranges_like_implementation_rs() {
        // eps=1e-10: raw width e/eps ≈ 2.7e10, hi-clamped to 2^26.
        assert_eq!(classic_cms_sizing(1e-10, 0.01), (1 << 26, 5));
        // eps=3.0: raw width e/3 < 1, lo-clamped to 2.
        assert_eq!(classic_cms_sizing(3.0, 0.01), (2, 5));
        // delta=1e-20: raw depth ln(1e20) ≈ 46.05 → ⌈⌉ 47, hi-clamped to 32.
        assert_eq!(classic_cms_sizing(0.01, 1e-20), (272, 32));
    }

    #[test]
    fn traditional_bound_is_linear_in_total_and_eps() {
        assert_eq!(traditional_a_priori_bound(1_000_000, 0.01), 10_000.0);
        assert_eq!(traditional_a_priori_bound(0, 0.01), 0.0);
    }

    // ── The correctness property this issue is actually about: the ────────
    // ── posterior bound (Algorithm 1) is always ≤ the traditional a ───────
    // ── priori bound (Eq. 6), for the same worst-case (w, r) sizing. ──────

    /// A tiny deterministic PRNG (xorshift64*) — enough to generate varied
    /// synthetic counter distributions without adding a `rand` dependency
    /// to this crate.
    struct Xorshift64(u64);
    impl Xorshift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        /// A weight in `[1, max]`, skewed low (many small draws, occasional
        /// large one) by squaring a uniform fraction — cheap stand-in for a
        /// heavy-tailed/Zipf-like distribution shape.
        fn skewed_weight(&mut self, max: u64) -> u64 {
            let u = (self.next() % 1_000_000) as f64 / 1_000_000.0;
            let skewed = u * u; // biases toward 0
            1 + (skewed * max as f64) as u64
        }
    }

    /// Distributes `total` across `w` non-negative counters honoring a CMS
    /// row's real structure: each unit of `total` lands in exactly one of
    /// the `w` counters (a counter is a sum of colliding flow weights), so
    /// `Σ counters == total` always — the property Eq. 6's proof relies on.
    fn synthetic_row(rng: &mut Xorshift64, w: usize, total: u64, skewed: bool) -> Vec<u64> {
        let mut counters = vec![0u64; w];
        let mut remaining = total;
        while remaining > 0 {
            let bucket = (rng.next() as usize) % w;
            let chunk = if skewed {
                rng.skewed_weight(remaining.min(1000))
            } else {
                1 + rng.next() % remaining.clamp(1, 37)
            }
            .min(remaining);
            counters[bucket] += chunk;
            remaining -= chunk;
        }
        counters
    }

    #[test]
    fn cms_posterior_bound_is_at_most_traditional_bound() {
        // The paper's Eq. 6 claim: at the standard worst-case sizing
        // (w=classic width, r=classic depth for a target (eps,delta)), the
        // posterior bound computed from real counters never exceeds the
        // traditional a priori bound eps*|F|1 — regardless of how the
        // total mass is actually distributed across counters.
        let mut rng = Xorshift64(0x243F6A8885A308D3);
        for (eps, delta) in [(0.05, 0.05), (0.02, 0.1), (0.1, 0.01), (0.03, 0.2)] {
            let (w, r) = classic_cms_sizing(eps, delta);
            let traditional = traditional_a_priori_bound(1_000_000, eps);
            for skewed in [false, true] {
                for trial in 0..20 {
                    rng.0 = rng.0.wrapping_add(0x9E3779B97F4A7C15).wrapping_add(trial);
                    let row = synthetic_row(&mut rng, w as usize, 1_000_000, skewed);
                    let posterior =
                        cms_posterior_error_bound(&row, r, delta).expect("valid inputs");
                    assert!(
                        (posterior as f64) <= traditional + 1e-6,
                        "posterior bound {posterior} exceeded traditional bound \
                         {traditional} for eps={eps} delta={delta} w={w} r={r} \
                         skewed={skewed} trial={trial}"
                    );
                }
            }
        }
    }

    #[test]
    fn cms_posterior_bound_at_most_traditional_bound_worst_case_single_counter() {
        // Degenerate worst case: all mass in one counter (maximum possible
        // collision) — the pigeonhole argument underlying Eq. 6 still must
        // hold: a single counter holding the entire total is itself only
        // ever picked as the estimate when the rank lands on it, and
        // whenever it isn't, the picked counter is 0 <= traditional bound.
        for (eps, delta) in [(0.05, 0.05), (0.01, 0.01)] {
            let (w, r) = classic_cms_sizing(eps, delta);
            let traditional = traditional_a_priori_bound(1_000_000, eps);
            let mut row = vec![0u64; w as usize];
            row[0] = 1_000_000;
            let posterior = cms_posterior_error_bound(&row, r, delta).expect("valid inputs");
            assert!(
                (posterior as f64) <= traditional + 1e-6,
                "posterior bound {posterior} exceeded traditional bound {traditional}"
            );
        }
    }

    #[test]
    fn kth_largest_pigeonhole_property() {
        // The elementary fact Eq. 6's proof leans on: for w non-negative
        // counters summing to T, the k-th largest is at most T/k. This is
        // the general-purpose invariant behind the paper-specific test
        // above, checked directly and unconditionally (no sketch sizing
        // formula involved).
        let mut rng = Xorshift64(0xD1B54A32D192ED03);
        for _ in 0..50 {
            let w = 5 + (rng.next() % 50) as usize;
            let total = 1 + rng.next() % 100_000;
            let row = synthetic_row(&mut rng, w, total, true);
            let sum: u64 = row.iter().sum();
            assert_eq!(sum, total);
            for k in 1..=w {
                let kth = kth_largest(&row, k);
                assert!(
                    (kth as f64) * (k as f64) <= (total as f64) + 1e-6,
                    "k={k}-th largest {kth} violates k*kth <= total ({total})"
                );
            }
        }
    }
}
