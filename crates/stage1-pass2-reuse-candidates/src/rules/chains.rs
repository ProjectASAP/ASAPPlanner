//! Step 5, chain groups (concepts doc, section 3, "Chain groups"): a
//! `SummaryTumblingWindowChain[w](X)` group per width, and a `SummaryMerge`
//! over the `TimeRange` (and `TimeShift`s) over it as a candidate of the
//! summary state group.

use std::collections::{BTreeMap, BTreeSet};

use crate::edges::CandidateEdge;
use crate::rules::candidates::{
    add_candidate_if_output_matches, add_candidates_per_input_parameters,
    add_group_computing_itself, distinct_parameters,
};
use crate::Pass2RuleError;
use asap_types::ir::operator::arguments::{AtModifier, Reduction, TimeShift};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, Predicate, TimeRangeKind};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
};
use asap_types::summary_type::{SketchParams, SummaryTypeCapability, SummaryUpdate};
use asap_types::workload::greatest_common_divisor;

/// For each summary state group S = `SummaryAgg(TimeRange(W,
/// TimeShift*(X)))` of a summary type that merges, with a build that
/// survived dead-candidate elimination (`pruned`), and each width $w$: the
/// chain group $G_w$ = `SummaryTumblingWindowChain[w](X)` (the shifts
/// stripped, so ranges and offsets over X share it) with one build per θ of
/// S's surviving builds, the shifts and the `TimeRange` re-wrapped around
/// it, and the candidates `SummaryMerge` over the outermost of them in S,
/// each of these one per θ of the group it reads (a chain shared by several
/// state groups has the builds of all of them).
///
/// The widths are the gcd closure of $\gcd(g_q, W, o_1, \ldots)$ over the
/// queries reaching S whose evaluation period at S, $g_q$, is known
/// (`evaluation_period_ms_per_query`: below a subquery, its resolution),
/// with the offsets $o_j$ on S's path; no query with a period, no chain.
/// The edges from a chain whose width does not divide the evaluation grid
/// of the group reading it (the gcd over all its readers) fail with
/// `ChainWidthDoesNotDivideEvaluationGrid` (`candidate_edges`), so the
/// merge over it is pruned: a 15 s chain never serves a reader every 10 s.
/// Under `@ T` S adds no width: once every unanchored state has added its
/// chains, it reads those whose width divides W, the offsets and T, in any
/// memo order. Under `@ end()` or `@ start()` (a `TimeShift` the front end
/// puts above the subtree it anchors) no chain is read.
pub(crate) fn add_chain_groups(
    memo: &mut Memo,
    evaluation_period_ms_per_query: &[BTreeMap<usize, Option<u64>>],
    pruned: &BTreeSet<CandidateForMemoGroupId>,
    edges: &[CandidateEdge],
) -> Result<(), Pass2RuleError> {
    // The chainable states with the θ of their surviving builds, the
    // unanchored ones first: an anchored state reads only the chains they
    // create.
    let mut states: Vec<(MemoGroupId, ChainPath, Vec<Option<SketchParams>>)> = Vec::new();
    for (index, memo_group) in memo.groups().iter().enumerate() {
        let state = MemoGroupId(index as u32);
        let Some(path) = chain_path(memo, state) else {
            continue;
        };
        let parameters = distinct_parameters(
            memo_group
                .candidates
                .iter()
                .enumerate()
                .filter(|&(index_in_group, candidate)| {
                    let survived = !pruned.contains(&CandidateForMemoGroupId {
                        group: state,
                        index_in_group,
                    });
                    survived && candidate.expression.operator == memo_group.expression
                })
                .map(|(_, candidate)| candidate),
        );
        if !parameters.is_empty() {
            states.push((state, path, parameters));
        }
    }
    states.sort_by_key(|(_, path, _)| path.anchored_at_ms.is_some());
    // First every chain group with its builds, then the groups reading
    // them, so a chain shared by state groups offers each every θ it has.
    let mut chains_of_states: Vec<ChainsOfState> = Vec::new();
    for (state, path, parameters) in states {
        let divisor = path.shifts.iter().fold(path.window_ms, |gcd, shift| {
            greatest_common_divisor(gcd, shift.offset_ms.unsigned_abs())
        });
        let widths: BTreeSet<u64> = match path.anchored_at_ms {
            Some(at_ms) => existing_chain_widths(memo, &path)
                .into_iter()
                .filter(|&width| divisor % width == 0 && at_ms.unsigned_abs() % width == 0)
                .collect(),
            None => gcd_closure(
                evaluation_period_ms_per_query[state.0 as usize]
                    .values()
                    .filter_map(|&period_ms| Some(greatest_common_divisor(period_ms?, divisor))),
            ),
        };
        let mut chain_groups = Vec::new();
        for width in widths {
            let chain = path.chain(width);
            let chain_group = memo.find_or_add_group(chain.clone())?;
            for theta in &parameters {
                add_candidate_if_output_matches(
                    memo,
                    chain_group,
                    chain.clone(),
                    theta.clone(),
                    OriginOfCandidateForMemoGroup::TumblingWindows,
                )?;
            }
            chain_groups.push(chain_group);
        }
        chains_of_states.push((state, path, chain_groups, parameters));
    }
    for (state, path, chain_groups, parameters) in chains_of_states {
        let state_grid_ms =
            state_evaluation_grid_ms(&evaluation_period_ms_per_query[state.0 as usize]);
        let level_parameters = &smallest_parameters_per_reader(memo, state, &parameters, edges);
        add_wider_levels(
            memo,
            state,
            &path,
            &chain_groups,
            state_grid_ms,
            level_parameters,
        )?;
        for chain_group in chain_groups {
            let mut top = chain_group;
            for shift in path.shifts.iter().rev() {
                top = add_group_computing_itself(
                    memo,
                    Operator::NonASAP(NonASAPOp::TimeShift {
                        shift: *shift,
                        child: top,
                    }),
                    OriginOfCandidateForMemoGroup::TumblingWindows,
                )?;
            }
            let windows = add_group_computing_itself(
                memo,
                Operator::NonASAP(NonASAPOp::TimeRange {
                    range: std::time::Duration::from_millis(path.window_ms),
                    kind: TimeRangeKind::Range,
                    child: top,
                }),
                OriginOfCandidateForMemoGroup::TumblingWindows,
            )?;
            add_candidates_per_input_parameters(
                memo,
                state,
                Operator::ASAP(ASAPOp::SummaryMerge {
                    children: vec![windows],
                    group_by: None,
                }),
                OriginOfCandidateForMemoGroup::TumblingWindows,
            )?;
        }
    }
    Ok(())
}

/// A state group, its chain walk, its chain groups and the θs of its
/// surviving builds, smallest first.
type ChainsOfState = (
    MemoGroupId,
    ChainPath,
    Vec<MemoGroupId>,
    Vec<Option<SketchParams>>,
);

/// The widths a wider chain level may take (1 min, 5 min, 15 min, 1 h,
/// 6 h, 1 d), each a multiple of the one before, so any of them nest.
pub(crate) const WIDER_LEVEL_WIDTHS_MS: [u64; 6] =
    [60_000, 300_000, 900_000, 3_600_000, 21_600_000, 86_400_000];

/// The most wider levels over one chain: of more admitted widths, the
/// narrowest, the widest and the middle one (each further level saves at
/// most a few windows per step, and every level is a group, a merge per θ
/// and a cover). Example: a 1 d range over 15 s: 1 min, 15 min and 6 h of
/// 1 min, 5 min, 15 min, 1 h and 6 h.
pub(crate) const MOST_WIDER_LEVELS: usize = 3;

/// The θs of `state`'s surviving builds (`parameters`) that get wider
/// levels: per reader of `state` (a candidate an edge from its builds
/// reaches, `edges` before the chains) and per θ type, the smallest θ by
/// state size ([`nominal_state_size`], compared only within one type) whose
/// edge meets that reader's accuracy. A larger θ's levels cost more for an
/// accuracy a smaller one meets, so no reader needs a larger one than its
/// own. Example: one state read at relative error 0.05 every 60 s and at
/// 0.005 every 15 s: DDSketch α = 0.05 for the first and the smallest α
/// meeting 0.005 for the second. Exact states (no θ): `[None]`.
fn smallest_parameters_per_reader(
    memo: &Memo,
    state: MemoGroupId,
    parameters: &[Option<SketchParams>],
    edges: &[CandidateEdge],
) -> Vec<Option<SketchParams>> {
    // Every (reader, θ) whose edge meets the reader's accuracy.
    let mut satisfied: Vec<(CandidateForMemoGroupId, Option<SketchParams>)> = Vec::new();
    for edge in edges {
        if edge.producer.group != state || edge.satisfies.is_err() {
            continue;
        }
        let theta = memo
            .candidate(edge.producer)
            .logical_properties
            .parameters()
            .cloned();
        if parameters.contains(&theta) && !satisfied.contains(&(edge.consumer, theta.clone())) {
            satisfied.push((edge.consumer, theta));
        }
    }
    // Per reader and θ type, the smallest θ meeting the reader.
    let mut smallest: Vec<(CandidateForMemoGroupId, Option<SketchParams>)> = Vec::new();
    for (reader, theta) in &satisfied {
        match smallest
            .iter_mut()
            .find(|(known_reader, known)| known_reader == reader && same_type(known, theta))
        {
            Some((_, known)) => {
                if smaller_of_one_type(theta, known) {
                    *known = theta.clone();
                }
            }
            None => smallest.push((*reader, theta.clone())),
        }
    }
    let mut chosen: Vec<Option<SketchParams>> = Vec::new();
    for (_, theta) in smallest {
        if !chosen.contains(&theta) {
            chosen.push(theta);
        }
    }
    chosen
}

/// Whether two θs (or two exact states) are of one type, whose sizes
/// compare ([`nominal_state_size`]).
fn same_type(a: &Option<SketchParams>, b: &Option<SketchParams>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => std::mem::discriminant(a) == std::mem::discriminant(b),
        (None, None) => true,
        _ => false,
    }
}

/// Whether `a` is a smaller state than `b`, both of one type; `false`
/// across types, whose sizes do not compare.
fn smaller_of_one_type(a: &Option<SketchParams>, b: &Option<SketchParams>) -> bool {
    same_type(a, b) && nominal_state_size(a.as_ref()) < nominal_state_size(b.as_ref())
}

/// A θ's state size, to order θs of one type smallest first: its counters
/// or items (a KLL's `k`, a Count-Min's width × depth, an HLL's 2^p
/// registers, a DDSketch's buckets per unit of log range, `1 / α`). An
/// exact state (`None`) is 0. Sizes of different types do not compare
/// ([`smaller_of_one_type`]); a Hydra structure has no order (every one 0).
fn nominal_state_size(parameters: Option<&SketchParams>) -> f64 {
    let Some(parameters) = parameters else {
        return 0.0;
    };
    match parameters {
        SketchParams::Kll { k } | SketchParams::Kmv { k } => f64::from(*k),
        SketchParams::DDSketch { alpha } => 1.0 / alpha.max(f64::MIN_POSITIVE),
        SketchParams::Hll { precision } => 2f64.powi(i32::from(*precision)),
        SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
            f64::from(*width) * f64::from(*depth)
        }
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        }
        | SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => f64::from(*width) * f64::from(*depth) + f64::from(*heap_size),
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
        } => {
            f64::from(*layers)
                * (f64::from(*sketch_rows) * f64::from(*sketch_cols) + f64::from(*heap_size))
        }
        SketchParams::Hydra(_) => 0.0,
    }
}

/// A state group's evaluation grid, the gcd of its readers' periods
/// (`evaluation_period_ms_per_query`); `None` when one reader's is unknown
/// or none reads it.
fn state_evaluation_grid_ms(periods_ms: &BTreeMap<usize, Option<u64>>) -> Option<u64> {
    let mut grid_ms = 0;
    for period_ms in periods_ms.values() {
        grid_ms = greatest_common_divisor(grid_ms, (*period_ms)?);
    }
    (grid_ms > 0).then_some(grid_ms)
}

/// Wider chain levels for state group `state` (design doc
/// `multi-level-window-chains.md`): for each of its chain groups $G_w$, the
/// widths $C$ of [`WIDER_LEVEL_WIDTHS_MS`] that $w$ divides with $2C \le W$
/// (W the state's range), narrowest first. Each is the chain group
/// `SummaryTumblingWindowChain[C](X)` with one `SummaryMergeWindows[C]`
/// candidate over the next narrower level per θ (built only from the
/// narrower level, never from the rows), and two `TimeRangeCover(W)`
/// groups, over the first wider level and over all of them (widest first),
/// each with a `SummaryMerge` over it in the state group, each for the θs of `level_parameters` (the smallest each
/// reader accepts). At most [`MOST_WIDER_LEVELS`] levels;
/// only over a chain $G_w$ whose width divides the state's evaluation grid
/// `state_grid_ms` (a cover over any other would fail Pass 2's grid check:
/// its edges are read from $G_w$). Under `TimeShift`s without `@` adding up
/// to an offset $o \ge 0$, the covers read the levels $o$ earlier
/// (`TimeRangeCover::offset_ms`), and only the widths that divide $o$, so the
/// shifted range ends on every level's grid; under `@` none.
/// Example: `quantile_over_time(0.99, m[1h])` every 15 s: levels 1 min,
/// 5 min and 15 min over the 15 s chain (not 1 h: 2 h > 1 h), and covers
/// over (1 min, 15 s) and over the four; read every 15 s and every 60 s, none over the 60 s
/// chain. `max_over_time(m[1h] offset 1h)`: the same levels (1 h is a
/// multiple of each), the covers at offset 1 h; `offset 90s`: none.
fn add_wider_levels(
    memo: &mut Memo,
    state: MemoGroupId,
    path: &ChainPath,
    chain_groups: &[MemoGroupId],
    state_grid_ms: Option<u64>,
    level_parameters: &[Option<SketchParams>],
) -> Result<(), Pass2RuleError> {
    // Under `offset`s (no `@`), the covers read the levels that much
    // earlier: only levels whose width divides the offset, so the shifted
    // range still ends on each level's grid.
    if path.anchored_at_ms.is_some() || path.shifts.iter().any(|shift| shift.at.is_some()) {
        return Ok(());
    }
    let Ok(offset_ms) = u64::try_from(
        path.shifts
            .iter()
            .try_fold(0_i64, |total, shift| total.checked_add(shift.offset_ms))
            .unwrap_or(-1),
    ) else {
        return Ok(());
    };
    let Some(state_grid_ms) = state_grid_ms else {
        return Ok(());
    };
    for &narrowest in chain_groups {
        let Some(narrowest_width_ms) = memo.groups()[narrowest.0 as usize]
            .expression
            .chain_tumbling_window_width_ms()
        else {
            continue;
        };
        if !state_grid_ms.is_multiple_of(narrowest_width_ms) {
            continue;
        }
        let mut widths: Vec<u64> = WIDER_LEVEL_WIDTHS_MS
            .into_iter()
            .filter(|&width| {
                width > narrowest_width_ms
                    && width.is_multiple_of(narrowest_width_ms)
                    && width.saturating_mul(2) <= path.window_ms
                    && offset_ms.is_multiple_of(width)
            })
            .collect();
        if widths.len() > MOST_WIDER_LEVELS {
            // The narrowest, the widest and one between: the edges stay
            // short and the middle stays few windows.
            widths = vec![
                widths[0],
                widths[widths.len() / 2],
                widths[widths.len() - 1],
            ];
        }
        let widths_count = widths.len();
        // Levels narrowest first; each built from the one before.
        let mut levels = vec![narrowest];
        for width in widths {
            let narrower = *levels.last().expect("the narrowest level");
            let level = memo.find_or_add_group(path.chain(width))?;
            let merge_windows = Operator::ASAP(ASAPOp::SummaryMergeWindows {
                child: narrower,
                tumbling_window_width_ms: width,
            });
            levels.push(level);
            // Covers over the first wider level and over all of them: the
            // shortest keeps the least memory, the longest the fewest
            // merges per step; a middle one (with three levels) rarely
            // wins over both and grows the selection per context and θ.
            let widest_so_far = levels.len() - 1;
            if widest_so_far != 1 && widest_so_far != widths_count {
                for theta in level_parameters {
                    add_candidate_if_output_matches(
                        memo,
                        level,
                        merge_windows.clone(),
                        theta.clone(),
                        OriginOfCandidateForMemoGroup::MultiLevelWindows,
                    )?;
                }
                continue;
            }
            let cover_expression = Operator::NonASAP(NonASAPOp::TimeRangeCover {
                range: std::time::Duration::from_millis(path.window_ms),
                offset_ms,
                levels: levels.iter().rev().copied().collect(),
            });
            let cover = memo.find_or_add_group(cover_expression.clone())?;
            let merge = Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![cover],
                group_by: None,
            });
            for theta in level_parameters {
                for (group, operator) in [
                    (level, merge_windows.clone()),
                    (cover, cover_expression.clone()),
                    (state, merge.clone()),
                ] {
                    add_candidate_if_output_matches(
                        memo,
                        group,
                        operator,
                        theta.clone(),
                        OriginOfCandidateForMemoGroup::MultiLevelWindows,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// A state group's walk down to the rows its chain reads:
/// `SummaryAgg` → `TimeRange(W)` → `TimeShift`s → X, with the build's
/// arguments the chain keeps.
struct ChainPath {
    summary_type: FieldDataType,
    input: SummaryUpdate,
    reduction: Reduction,
    filter: Option<Predicate<MemoGroupId>>,
    window_ms: u64,
    /// Top down.
    shifts: Vec<TimeShift>,
    /// T of an `@ T` among the shifts.
    anchored_at_ms: Option<i64>,
    rows: MemoGroupId,
}

impl ChainPath {
    /// `SummaryTumblingWindowChain[width](X)` with the build's arguments.
    fn chain(&self, width: u64) -> Operator<MemoGroupId> {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: self.rows,
            summary_type: self.summary_type.clone(),
            input: self.input.clone(),
            reduction: self.reduction.clone(),
            filter: self.filter.clone(),
            tumbling_window_width_ms: width,
        })
    }
}

/// S's [`ChainPath`]: a `SummaryAgg` of a summary type whose states merge
/// (its capability row), over a range `TimeRange(W)` with `W > 0`, over
/// `TimeShift`s with at most one `@ T` and no `@ end()` or `@ start()`.
fn chain_path(memo: &Memo, state: MemoGroupId) -> Option<ChainPath> {
    let expression = &memo.groups()[state.0 as usize].expression;
    let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type,
        input,
        reduction,
        filter,
    }) = expression
    else {
        return None;
    };
    SummaryTypeCapability::of(summary_type).merge_inputs?;
    let Operator::NonASAP(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child: below,
    }) = &memo.groups()[child.0 as usize].expression
    else {
        return None;
    };
    let window_ms = u64::try_from(range.as_millis()).ok().filter(|&ms| ms > 0)?;
    let (mut shifts, mut anchored_at_ms, mut rows) = (Vec::new(), None, *below);
    while let Operator::NonASAP(NonASAPOp::TimeShift { shift, child }) =
        &memo.groups()[rows.0 as usize].expression
    {
        match shift.at {
            None => {}
            Some(AtModifier::Timestamp(at_ms)) if anchored_at_ms.is_none() => {
                anchored_at_ms = Some(at_ms)
            }
            Some(_) => return None,
        }
        shifts.push(*shift);
        rows = *child;
    }
    Some(ChainPath {
        summary_type: summary_type.clone(),
        input: input.clone(),
        reduction: reduction.clone(),
        filter: filter.clone(),
        window_ms,
        shifts,
        anchored_at_ms,
        rows,
    })
}

/// The widths of the chain groups over `path`'s rows with its build's
/// arguments that the memo has already.
fn existing_chain_widths(memo: &Memo, path: &ChainPath) -> Vec<u64> {
    memo.groups()
        .iter()
        .filter_map(|group| {
            group
                .expression
                .chain_tumbling_window_width_ms()
                .filter(|&width_ms| group.expression == path.chain(width_ms))
        })
        .collect()
}

/// The gcd closure of `widths`: each, and the gcd of every two in it.
fn gcd_closure(widths: impl IntoIterator<Item = u64>) -> BTreeSet<u64> {
    let mut closure: BTreeSet<u64> = BTreeSet::new();
    for width in widths.into_iter().filter(|&width| width > 0) {
        let gcds: Vec<u64> = closure
            .iter()
            .map(|&known| greatest_common_divisor(known, width))
            .collect();
        closure.insert(width);
        closure.extend(gcds);
    }
    closure
}

#[cfg(test)]
mod tests {
    use super::*;

    /// θs of different types never compare by size, whatever their counts:
    /// a Count-Min of 100 × 5 is not "smaller" than a Count Sketch of
    /// 1,000 × 5 or a Hydra; within one type the smaller counts order first.
    #[test]
    fn state_sizes_compare_only_within_one_type() {
        let cms = |width| Some(SketchParams::Cms { width, depth: 5 });
        let count_sketch = |width| Some(SketchParams::CountSketch { width, depth: 5 });
        let hydra = Some(SketchParams::Hydra(
            asap_types::summary_type::HydraParams::HydraCms {
                width: 100,
                depth: 5,
                shared_rows: 5,
                shared_columns: 1_000,
            },
        ));
        assert!(smaller_of_one_type(&cms(100), &cms(1_000)));
        assert!(!smaller_of_one_type(&cms(1_000), &cms(100)));
        assert!(!smaller_of_one_type(&cms(100), &count_sketch(1_000)));
        assert!(!smaller_of_one_type(&count_sketch(100), &cms(1_000)));
        assert!(!smaller_of_one_type(&cms(100), &hydra));
        assert!(!smaller_of_one_type(&hydra, &cms(100)));
        assert!(!smaller_of_one_type(&None, &cms(100)));
        assert!(same_type(&cms(1), &cms(2)) && !same_type(&cms(1), &count_sketch(1)));
    }
}
