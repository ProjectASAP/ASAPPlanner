//! Step 5, chain groups (concepts doc, section 3, "Chain groups"): a
//! `SummaryTumblingWindowChain[w](X)` group per width, and a `SummaryMerge`
//! over the `TimeRange` (and `TimeShift`s) over it as a candidate of the
//! summary state group.

use std::collections::BTreeSet;

use crate::rules::{
    add_candidate, add_candidates_per_input_parameters, add_group_computing_itself, Pass2RuleError,
};
use asap_types::ir::operator::operator_properties::{AtModifier, TimeShift};
use asap_types::ir::schema::{SketchParams, SummaryTypeCapability};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

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
/// queries reaching S that repeat on a known evaluation grid $g_q$
/// (`QueryRecurrence::evaluation_grid_ms`), with the offsets $o_j$ on S's
/// path; no query with a grid, no chain. Under `@ T` S adds no width: it
/// reads only chains other states created whose width divides W, the
/// offsets and T. Under `@ end()` or `@ start()` (a `TimeShift` the front
/// end puts above the subtree it anchors) no chain is read.
/// `entries[q]` is query `q`'s workload entry.
pub(crate) fn add_chain_groups(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
    pruned: &BTreeSet<CandidateForMemoGroupId>,
) -> Result<(), Pass2RuleError> {
    // First every chain group with its builds, then the groups reading
    // them, so a chain shared by state groups offers each every θ it has.
    let mut chains_of_states: Vec<(MemoGroupId, ChainPath, Vec<MemoGroupId>)> = Vec::new();
    for index in 0..memo.groups().len() {
        let state = MemoGroupId(index as u32);
        let Some(path) = chain_path(memo, state) else {
            continue;
        };
        let parameters: Vec<Option<SketchParams>> = {
            let mut parameters = Vec::new();
            for (index_in_group, candidate) in memo.groups()[index].candidates.iter().enumerate() {
                let survived = !pruned.contains(&CandidateForMemoGroupId {
                    group: state,
                    index_in_group,
                });
                let own = candidate.expression.operator == memo.groups()[index].expression;
                let theta = candidate.logical_properties.parameters().cloned();
                if survived && own && !parameters.contains(&theta) {
                    parameters.push(theta);
                }
            }
            parameters
        };
        if parameters.is_empty() {
            continue;
        }
        let divisor = path.shifts.iter().fold(path.window_ms, |gcd, shift| {
            greatest_common_divisor(gcd, shift.offset_ms.unsigned_abs())
        });
        let widths: BTreeSet<u64> = match path.anchored_at_ms {
            Some(at_ms) => existing_chain_widths(memo, &path)
                .into_iter()
                .filter(|&width| divisor % width == 0 && at_ms.unsigned_abs() % width == 0)
                .collect(),
            None => gcd_closure(memo.groups()[index].queries.iter().filter_map(|&query| {
                let entry = entries.get(query)?;
                let grid = entry
                    .recurrence
                    .evaluation_grid_ms(entry.range_evaluation)?;
                Some(greatest_common_divisor(grid, divisor))
            })),
        };
        let mut chain_groups = Vec::new();
        for width in widths {
            let chain = path.chain(width);
            let chain_group = memo.find_or_add_group(chain.clone())?;
            for theta in &parameters {
                add_candidate(
                    memo,
                    chain_group,
                    chain.clone(),
                    theta.clone(),
                    OriginOfCandidateForMemoGroup::TumblingWindows,
                )?;
            }
            chain_groups.push(chain_group);
        }
        chains_of_states.push((state, path, chain_groups));
    }
    for (state, path, chain_groups) in chains_of_states {
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

/// A state group's walk down to the rows its chain reads:
/// `SummaryAgg` → `TimeRange(W)` → `TimeShift`s → X.
struct ChainPath {
    /// The `SummaryAgg`, its child to be replaced by X.
    build: ASAPOp<MemoGroupId>,
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
        let ASAPOp::SummaryAgg {
            summary_type,
            input,
            reduction,
            filter,
            ..
        } = &self.build
        else {
            unreachable!("a chain path starts at a SummaryAgg");
        };
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: self.rows,
            summary_type: summary_type.clone(),
            input: input.clone(),
            reduction: reduction.clone(),
            filter: filter.clone(),
            tumbling_window_width_ms: width,
        })
    }
}

/// S's [`ChainPath`]: a `SummaryAgg` of a summary type whose states merge
/// (its capability row), over a range `TimeRange(W)` with `W > 0`, over
/// `TimeShift`s with at most one `@ T` and no `@ end()` or `@ start()`.
fn chain_path(memo: &Memo, state: MemoGroupId) -> Option<ChainPath> {
    let expression = &memo.groups()[state.0 as usize].expression;
    let Operator::ASAP(
        build @ ASAPOp::SummaryAgg {
            child,
            summary_type,
            ..
        },
    ) = expression
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
        build: build.clone(),
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
        .filter_map(|group| match &group.expression {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) if group.expression == path.chain(*tumbling_window_width_ms) => {
                Some(*tumbling_window_width_ms)
            }
            _ => None,
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
