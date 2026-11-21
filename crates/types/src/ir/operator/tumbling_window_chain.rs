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

/// The windows that tile `(start_ms, end_ms]` with chains of
/// `level_widths_ms` (widest first, each width a multiple of the next, all
/// anchored at the epoch): the widest level's whole windows inside the
/// interval, then, in each edge left over, the next level's whole windows,
/// down to the narrowest level, which takes what is left. Each as `(level
/// index, window_end_ms)`, oldest first. `start_ms` and `end_ms` must be on
/// the narrowest level's grid; the windows are disjoint and their union is
/// the interval.
///
/// Example: `(99 h + 105 s, 100 h + 105 s]` with levels 5 min and 15 s:
/// 13 windows of 15 s up to 99 h + 300 s, 11 of 5 min up to 100 h, 7 of
/// 15 s up to 100 h + 105 s; 31 instead of 240.
pub fn cover_windows(start_ms: i64, end_ms: i64, level_widths_ms: &[u64]) -> Vec<(usize, i64)> {
    let mut windows = Vec::new();
    cover_into(start_ms, end_ms, level_widths_ms, 0, &mut windows);
    windows
}

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
    let narrowest = level + 1 == level_widths_ms.len();
    // The whole windows of this level inside the interval: ends in `(lo, hi]`.
    let lo = start_ms.div_euclid(width) * width
        + if start_ms.rem_euclid(width) == 0 { 0 } else { width };
    let hi = end_ms.div_euclid(width) * width;
    if narrowest {
        let mut end = start_ms + width;
        while end <= end_ms {
            windows.push((level, end));
            end += width;
        }
        return;
    }
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

/// The mean number of windows [`cover_windows`] selects from each level
/// (index as in `level_widths_ms`) for a range of `range_ms` ending
/// `offset_ms` before each evaluation time `k · step_ms`: the count depends
/// only on the evaluation time modulo the widest width, so this averages
/// over the `lcm(widest, step) / step` evaluation times of one period.
/// Example: 1 h every 15 s over 5 min and 15 s: `[11.05, 19.0]`.
pub fn mean_cover_windows_per_level(
    range_ms: u64,
    offset_ms: i64,
    step_ms: u64,
    level_widths_ms: &[u64],
) -> Vec<f64> {
    let mut per_level = vec![0.0; level_widths_ms.len()];
    let Some(&widest) = level_widths_ms.first() else {
        return per_level;
    };
    let widest = widest.max(1);
    let step = step_ms.max(1);
    let period = widest / crate::workload::greatest_common_divisor(widest, step) * step;
    let evaluations = (period / step).max(1);
    let range = i64::try_from(range_ms).unwrap_or(i64::MAX);
    for k in 0..evaluations {
        let end = i64::try_from(k * step).unwrap_or(i64::MAX) - offset_ms;
        for (level, _) in cover_windows(end - range, end, level_widths_ms) {
            per_level[level] += 1.0;
        }
    }
    for count in &mut per_level {
        *count /= evaluations as f64;
    }
    per_level
}

/// The mean number of windows [`cover_windows`] selects, over every level
/// ([`mean_cover_windows_per_level`]). Example: 1 h every 15 s over 5 min
/// and 15 s: 30.05; over 15 min, 5 min, 1 min and 15 s: 12.0.
pub fn mean_cover_windows(
    range_ms: u64,
    offset_ms: i64,
    step_ms: u64,
    level_widths_ms: &[u64],
) -> f64 {
    mean_cover_windows_per_level(range_ms, offset_ms, step_ms, level_widths_ms)
        .iter()
        .sum()
}
