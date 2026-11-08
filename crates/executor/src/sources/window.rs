//! The rows a slot of a compiled DAG reads, back from the evaluation time:
//! the executor derives each `Scan`'s and each materialized view's window
//! from the `TimeRange` and `TimeShift` nodes above or below it, and pushes
//! it into the data-store read.

use std::collections::{HashMap, HashSet};

use planner_types::ir::operator::asap::ASAPOp;
use planner_types::ir::operator::node::Operator;
use planner_types::ir::operator::non_asap::NonASAPOp;
use planner_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload,
};

/// Which rows a slot reads, back from an evaluation time `T`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum ScanWindow {
    /// The rows with time in `(T - end_offset_ms - width_ms, T -
    /// end_offset_ms]`.
    Within { end_offset_ms: i64, width_ms: i64 },
    /// Rows at any time.
    Unbounded,
}

impl ScanWindow {
    /// `(start, end]` at evaluation time `evaluation_time_ms`; `None` when
    /// unbounded.
    pub(crate) fn bounds_ms(self, evaluation_time_ms: i64) -> Option<(i64, i64)> {
        match self {
            Self::Within {
                end_offset_ms,
                width_ms,
            } => {
                let end = evaluation_time_ms.saturating_sub(end_offset_ms);
                Some((end.saturating_sub(width_ms), end))
            }
            Self::Unbounded => None,
        }
    }

    /// The smallest window holding both.
    fn union(self, other: Self) -> Self {
        let (
            Self::Within {
                end_offset_ms: a_end,
                width_ms: a_width,
            },
            Self::Within {
                end_offset_ms: b_end,
                width_ms: b_width,
            },
        ) = (self, other)
        else {
            return Self::Unbounded;
        };
        let end_offset_ms = a_end.min(b_end);
        let start_back_ms = a_end
            .saturating_add(a_width)
            .max(b_end.saturating_add(b_width));
        Self::Within {
            end_offset_ms,
            width_ms: start_back_ms.saturating_sub(end_offset_ms),
        }
    }
}

/// The rows the `TimeRange` `time_range` keeps, back from the evaluation
/// time `T`: `(T - s - w, T - s]` for a `TimeRange(w)` over `TimeShift`s
/// whose offsets sum to `s`. `Unbounded` when one of those shifts has an `@`
/// anchor, or `time_range` is not a `TimeRange`.
pub(crate) fn time_range_window<'a>(
    time_range: PhysicalASAPNodeId,
    payload_of: impl Fn(PhysicalASAPNodeId) -> Option<&'a PhysicalASAPOperatorPayload>,
) -> ScanWindow {
    let Some(Operator::NonASAP(NonASAPOp::TimeRange { range, child, .. })) = payload_of(time_range)
    else {
        return ScanWindow::Unbounded;
    };
    let mut end_offset_ms = 0i64;
    let mut below = payload_of(*child);
    while let Some(Operator::NonASAP(NonASAPOp::TimeShift { shift, child })) = below {
        if shift.at.is_some() {
            return ScanWindow::Unbounded;
        }
        end_offset_ms = end_offset_ms.saturating_add(shift.offset_ms);
        below = payload_of(*child);
    }
    ScanWindow::Within {
        end_offset_ms,
        width_ms: i64::try_from(range.as_millis()).unwrap_or(i64::MAX),
    }
}

fn payloads_by_id(
    dag: &PhysicalASAPDAG,
) -> HashMap<PhysicalASAPNodeId, &PhysicalASAPOperatorPayload> {
    dag.nodes
        .iter()
        .map(|node| (node.id, &node.payload))
        .collect()
}

/// Each of `scans`' window, by id: the union of the windows its consumers
/// read through `TimeShift`s without an `@` anchor, each a `TimeRange(w)`
/// (`(T - s - w, T - s]`). `Unbounded` when a root, any other consumer, or
/// a shift with an `@` anchor reads the `Scan`, or nothing reads it.
pub(crate) fn scan_windows(
    dag: &PhysicalASAPDAG,
    scans: impl IntoIterator<Item = PhysicalASAPNodeId>,
) -> HashMap<PhysicalASAPNodeId, ScanWindow> {
    let payloads = payloads_by_id(dag);
    let mut consumers: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for edge in &dag.edges {
        consumers
            .entry(edge.producer)
            .or_default()
            .push(edge.consumer);
    }
    let roots: HashSet<PhysicalASAPNodeId> = dag.roots.iter().copied().collect();
    let window_of = |scan: PhysicalASAPNodeId| {
        let mut window: Option<ScanWindow> = None;
        let mut pending = vec![(scan, 0i64)];
        while let Some((id, end_offset_ms)) = pending.pop() {
            if roots.contains(&id) {
                return ScanWindow::Unbounded;
            }
            for &consumer in consumers.get(&id).into_iter().flatten() {
                let width_ms = match payloads.get(&consumer) {
                    Some(Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }))
                        if shift.at.is_none() =>
                    {
                        pending.push((consumer, end_offset_ms.saturating_add(shift.offset_ms)));
                        continue;
                    }
                    Some(Operator::NonASAP(NonASAPOp::TimeRange { range, .. })) => {
                        range.as_millis()
                    }
                    Some(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })) => todo!(
                        "#1207 PR 6: the scan window of a chain read through a TimeRange over it"
                    ),
                    _ => return ScanWindow::Unbounded,
                };
                let read = ScanWindow::Within {
                    end_offset_ms,
                    width_ms: i64::try_from(width_ms).unwrap_or(i64::MAX),
                };
                window = Some(window.map_or(read, |window| window.union(read)));
            }
        }
        window.unwrap_or(ScanWindow::Unbounded)
    };
    scans
        .into_iter()
        .map(|scan| (scan, window_of(scan)))
        .collect()
}

/// Each of `nodes`' window, by id: the union of the windows of every
/// `TimeRange` below it. `Unbounded` when no `TimeRange` is below it or one
/// of them is unbounded (a whole SQL table, an `@` anchor).
pub(crate) fn view_windows(
    dag: &PhysicalASAPDAG,
    nodes: impl IntoIterator<Item = PhysicalASAPNodeId>,
) -> HashMap<PhysicalASAPNodeId, ScanWindow> {
    let payloads = payloads_by_id(dag);
    let window_of = |node: PhysicalASAPNodeId| {
        let mut seen = HashSet::new();
        let mut pending = vec![node];
        let mut window: Option<ScanWindow> = None;
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let read = match payloads.get(&id) {
                Some(Operator::NonASAP(NonASAPOp::TimeRange { .. })) => {
                    time_range_window(id, |id| payloads.get(&id).copied())
                }
                Some(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })) => todo!(
                    "#1207 PR 6: the window a chain read through a TimeRange over it summarizes"
                ),
                Some(payload) => {
                    pending.extend(payload.children().into_iter().copied());
                    continue;
                }
                None => continue,
            };
            window = Some(window.map_or(read, |window| window.union(read)));
        }
        window.unwrap_or(ScanWindow::Unbounded)
    };
    nodes
        .into_iter()
        .map(|node| (node, window_of(node)))
        .collect()
}
