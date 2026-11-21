//! Tumbling-window chains ([`ASAPOp::SummaryTumblingWindowChain`]): one
//! row per window of width `w`, on a grid anchored at the epoch, a state
//! with a time index. A chain has no range and no window count: a
//! `TimeRange` (and `TimeShift`s) over it select the windows a query reads,
//! a `SummaryMerge` over that selection is the state of the chain's build
//! over the same range, and how many windows it retains is $h_v$, the
//! longest its selected consumers read back (Stage 3's constraint (R)).
use super::asap::ASAPOp;
use super::input::OperatorInput;
use super::node::Operator;
use super::non_asap::NonASAPOp;

/// Whether `state` is a state with a time index: a chain, or a `TimeRange`
/// or `TimeShift` over one.
pub(crate) fn is_time_indexed_state<I: OperatorInput>(state: &I) -> bool {
    match state.operator() {
        Operator::ASAP(
            ASAPOp::SummaryTumblingWindowChain { .. } | ASAPOp::SummaryMergeWindows { .. },
        ) => true,
        Operator::NonASAP(NonASAPOp::TimeRangeCover { .. }) => true,
        Operator::NonASAP(
            NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
        ) => is_time_indexed_state(&child),
        _ => false,
    }
}

/// One level of a `TimeRangeCover`: the levels' widths, widest first, and
/// which of them this is. A reader of level `level` keeps the windows the
/// cover takes from it ([`cover_windows`]).
///
/// Example: `CoverLevel { level_widths_ms: vec![300_000, 15_000], level: 1 }`
/// is the 15 s level of a cover over 5 min and 15 s windows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CoverLevel {
    /// The cover's level widths, widest first, each a multiple of the next.
    pub level_widths_ms: Vec<u64>,
    /// The index of this level in `level_widths_ms`.
    pub level: usize,
}

/// Why a cover cannot select windows: an interval end off its narrowest
/// level's grid, whose windows would hold rows outside the interval.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "({start_ms}, {end_ms}] is not on the grid of the narrowest cover level ({narrowest_ms} ms)"
)]
pub struct CoverOffTheGrid {
    pub start_ms: i64,
    pub end_ms: i64,
    pub narrowest_ms: u64,
}

/// The windows that tile `(start_ms, end_ms]` with chains of
/// `level_widths_ms` (widest first, each width a multiple of the next, all
/// anchored at the epoch): the widest level's whole windows inside the
/// interval, then, in each edge left over, the next level's whole windows,
/// down to the narrowest level, which takes what is left. Each as `(level
/// index, window_end_ms)`, oldest first; disjoint, their union the
/// interval. Err when `start_ms` or `end_ms` is off the narrowest level's
/// grid: no set of whole windows is then exactly the interval.
///
/// Example: `(99 h + 105 s, 100 h + 105 s]` with levels 5 min and 15 s:
/// 13 windows of 15 s up to 99 h + 300 s, 11 of 5 min up to 100 h, 7 of
/// 15 s up to 100 h + 105 s; 31 instead of 240.
pub fn cover_windows(
    start_ms: i64,
    end_ms: i64,
    level_widths_ms: &[u64],
) -> Result<Vec<(usize, i64)>, CoverOffTheGrid> {
    let narrowest_ms = level_widths_ms.last().copied().unwrap_or(1).max(1);
    let narrowest = i64::try_from(narrowest_ms).unwrap_or(i64::MAX);
    if start_ms.rem_euclid(narrowest) != 0 || end_ms.rem_euclid(narrowest) != 0 {
        return Err(CoverOffTheGrid {
            start_ms,
            end_ms,
            narrowest_ms,
        });
    }
    let mut windows = Vec::new();
    cover_into(start_ms, end_ms, level_widths_ms, 0, &mut windows);
    Ok(windows)
}

/// The windows of `level` and narrower tiling `(start_ms, end_ms]`, both on
/// the narrowest level's grid.
fn cover_into(
    start_ms: i64,
    end_ms: i64,
    level_widths_ms: &[u64],
    level: usize,
    windows: &mut Vec<(usize, i64)>,
) {
    if start_ms >= end_ms || level >= level_widths_ms.len() {
        return;
    }
    let width = i64::try_from(level_widths_ms[level].max(1)).unwrap_or(i64::MAX);
    // The whole windows of this level inside the interval end in `(lo, hi]`.
    let lo = start_ms.div_euclid(width) * width
        + if start_ms.rem_euclid(width) == 0 {
            0
        } else {
            width
        };
    let hi = end_ms.div_euclid(width) * width;
    if lo >= hi {
        cover_into(start_ms, end_ms, level_widths_ms, level + 1, windows);
        return;
    }
    cover_into(start_ms, lo, level_widths_ms, level + 1, windows);
    let mut end = lo + width;
    while end <= hi {
        windows.push((level, end));
        end += width;
    }
    cover_into(hi, end_ms, level_widths_ms, level + 1, windows);
}

/// The mean number of windows [`cover_windows`] takes from each level
/// (index as in `level_widths_ms`, widest first) for a range of `range_ms`
/// at the evaluation times `grid.phase_ms + k · grid.period_ms`.
///
/// Closed form: the levels from the widest down to `i` cover the whole
/// windows of `c_i` inside the range, `U_i = max(0, ⌊T⌋_{c_i} −
/// ⌈T − W⌉_{c_i})`, so level `i` takes `(U_i − U_{i−1}) / c_i` windows. The
/// residues of `T` modulo `c_i` on the grid are `r + j·g`, `g =
/// gcd(period, c_i)`, `j < c_i / g`, each equally often, so `E[T mod c_i]`
/// is their mean, and the same for `T − W`; when `W < 2·c_i` the `max`
/// may bind and the residues are summed instead. Example: 1 h every 15 s
/// over 5 min and 15 s: `[11.05, 19.0]`; every 60 s: `[11.2, 16.0]`.
pub fn mean_cover_windows_per_level(
    range_ms: u64,
    grid: crate::stages::EvaluationGrid,
    level_widths_ms: &[u64],
) -> Vec<f64> {
    let period = grid.period_ms.max(1);
    // E[U_i] in ms per level.
    let covered: Vec<f64> = level_widths_ms
        .iter()
        .map(|&width| {
            let width = width.max(1);
            let g = crate::workload::greatest_common_divisor(period, width);
            let residues = width / g;
            // The residues of T and of T − W modulo `width`.
            let first_t = grid.phase_ms % g;
            let first_start =
                (grid.phase_ms + width * (range_ms / width + 1) - range_ms % width) % g;
            let mean_residue = |first: u64| first as f64 + g as f64 * (residues - 1) as f64 / 2.0;
            if range_ms >= 2 * width {
                // ⌈T − W⌉ − (T − W) = (width − (T − W) mod width) mod width.
                let zero_start = if first_start == 0 { 1.0 } else { 0.0 } / residues as f64;
                let ceil_gap = width as f64 - mean_residue(first_start) - width as f64 * zero_start;
                range_ms as f64 - mean_residue(first_t) - ceil_gap
            } else {
                // Few residues and the max may bind: sum them.
                (0..residues)
                    .map(|j| {
                        let t = first_t + j * g;
                        // T ≡ t (mod width), T − W ≡ t − W.
                        let start_residue = (t + width - range_ms % width) % width;
                        let floor_t = range_ms as i64 - t as i64;
                        let ceil_gap = (width - start_residue) % width;
                        (floor_t - ceil_gap as i64).max(0) as f64
                    })
                    .sum::<f64>()
                    / residues as f64
            }
        })
        .collect();
    let mut previous = 0.0;
    covered
        .iter()
        .zip(level_widths_ms)
        .map(|(&covered, &width)| {
            let taken = (covered - previous).max(0.0) / width.max(1) as f64;
            previous = covered.max(previous);
            taken
        })
        .collect()
}
