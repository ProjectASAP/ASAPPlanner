//! Rule 1, local window composition: tumbling-window layouts $\omega_v$ as
//! candidates of a summary state group (#1160).

use std::collections::BTreeSet;

use crate::memo::rules::{add_candidate, expression, Pass2RuleError};
use asap_types::ir::schema::FamilyCapability;
use asap_types::ir::TimeRangeKind;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorResultKind};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup, WindowLayout};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

/// Rule 1: to each summary state group whose build can be split into
/// tumbling windows ([`splittable_window_ms`], window $W$), for each of its
/// builds (a candidate whose expression is the group's, with its
/// $\boldsymbol\theta$, not fused, without a layout) and each width $w$ of
/// [`tumbling_window_widths`], the same build with $\omega$ = tumbling
/// windows of width $w$. Every consumer of the state group sees it: no
/// consumer is copied. Stage 3 checks that each consumer's context can read
/// the grid (`PhysicalCandidate::delivers_on`). `entries[i]` is query `i`'s
/// workload entry.
pub(crate) fn add_local_window_composition(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
) -> Result<(), Pass2RuleError> {
    for index in 0..memo.groups().len() {
        let group = MemoGroupId(index as u32);
        let Some(window_ms) = splittable_window_ms(memo, group) else {
            continue;
        };
        let widths = tumbling_window_widths(
            window_ms,
            memo.groups()[index]
                .queries
                .iter()
                .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
        );
        let own = expression(memo, group).clone();
        let builds: Vec<_> = memo.groups()[index]
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.expression.operator == own
                    && candidate.fused_groups.is_empty()
                    && candidate.logical_properties.window_layout().is_none()
            })
            .map(|candidate| {
                (
                    candidate.logical_properties.parameters().cloned(),
                    candidate.input_requirements.clone(),
                )
            })
            .collect();
        for (parameters, input_requirements) in builds {
            for &width_ms in &widths {
                add_candidate(
                    memo,
                    group,
                    own.clone(),
                    parameters.clone(),
                    Some(WindowLayout::Tumbling { width_ms }),
                    Some(input_requirements.clone()),
                    OriginOfCandidateForMemoGroup::TumblingWindows,
                )?;
            }
        }
    }
    Ok(())
}

/// The window $W$, in milliseconds, of a state group whose build can be
/// split into tumbling windows: a `SummaryAgg` whose family merges (the
/// family-capability table), over a range `TimeRange(W)` (`W > 0`) over an
/// instant vector or a SQL table's rows, read at the query's time or a
/// fixed offset from it: no `@` anchor on a time shift below the range.
fn splittable_window_ms(memo: &Memo, group: MemoGroupId) -> Option<u64> {
    let Operator::ASAP(ASAPOp::SummaryAgg { child, family, .. }) = expression(memo, group) else {
        return None;
    };
    FamilyCapability::of(family).merge_inputs?;
    let Operator::NonASAP(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child: input,
    }) = expression(memo, *child)
    else {
        return None;
    };
    if !matches!(
        memo.groups()[input.0 as usize].group_properties.result_kind,
        OperatorResultKind::InstantVector | OperatorResultKind::Relation
    ) {
        return None;
    }
    let mut below = *input;
    while let Operator::NonASAP(NonASAPOp::TimeShift { shift, child }) = expression(memo, below) {
        if shift.at.is_some() {
            return None;
        }
        below = *child;
    }
    u64::try_from(range.as_millis()).ok().filter(|&ms| ms > 0)
}

/// The tumbling-window widths offered for a window of `window_ms`: the gcd
/// closure of the evaluation grids `grids_ms` (each query's interval, range
/// step and phase, `QueryWorkloadEntry::evaluation_grid_ms`), each width
/// then reduced to divide the window: $\gcd(W, c)$ for every $c$ in the
/// closure. Ascending; empty when no query has a grid.
pub(crate) fn tumbling_window_widths(
    window_ms: u64,
    grids_ms: impl IntoIterator<Item = u64>,
) -> Vec<u64> {
    let mut closure: BTreeSet<u64> = BTreeSet::new();
    for grid in grids_ms.into_iter().filter(|&grid| grid > 0) {
        let gcds: Vec<u64> = closure
            .iter()
            .map(|&known| greatest_common_divisor(known, grid))
            .collect();
        closure.insert(grid);
        closure.extend(gcds);
    }
    closure
        .into_iter()
        .map(|grid| greatest_common_divisor(window_ms, grid))
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect()
}
