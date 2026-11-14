//! Time windows relative to an evaluation time, and the walks over a plan
//! that find them, shared by the cost model (retention and a scan's read),
//! Stage 3, the installer's checks and the executor. Four layers:
//!
//! 1. [`TimeWindow`]: the rows `(t - ends_before_evaluation_ms -
//!    length_ms, t - ends_before_evaluation_ms]` at evaluation time t.
//! 2. [`InputTimeAccess`]: how one node reads its time-indexed input,
//!    which rows ([`InputRows`]) and how much earlier its output rows are
//!    than those ([`InputTimeAccess::shifts_back_ms`]).
//! 3. [`TimeIndexedPlan`]: a plan the walks run over (plan nodes, Stage 3
//!    candidates or executor nodes): each node's readers, inputs, input
//!    time access and operator, and which nodes are raw data.
//! 4. The walks: up from a time-indexed producer (raw data, a chain,
//!    results stored per evaluation time) through its readers, the window
//!    each needs ([`windows_readers_need`]); down from a node, the windows
//!    its result is computed from ([`windows_computed_from`],
//!    [`shift_back_ms_below`]).
//!
//! Example: `sum_over_time(m[5m] offset 1m)` reads `m` through
//! `TimeShift(1m) -> TimeRange(5m)`. The shift reads what its reader
//! reads, one minute earlier (`WhatItsReadersRead`, `shifts_back_ms`
//! 60,000); the range reads `LastWindow { length_ms: 300_000 }`. So `m` is
//! read through `TimeWindow { ends_before_evaluation_ms: 60_000,
//! length_ms: 300_000 }`, `(t - 6m, t - 1m]`, reaching back 360,000 ms.

use serde::{Deserialize, Serialize};

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::Operator;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::properties::execution_data_state::ExecutionTiming;
use crate::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use crate::stages::Lifetime;

/// The rows `(t - ends_before_evaluation_ms - length_ms, t -
/// ends_before_evaluation_ms]` at evaluation time t.
///
/// Example: a 5-minute read with offset 1m is `TimeWindow {
/// ends_before_evaluation_ms: 60_000, length_ms: 300_000 }`; it reaches
/// back 360,000 ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TimeWindow {
    pub ends_before_evaluation_ms: u64,
    pub length_ms: u64,
}

impl TimeWindow {
    /// How far back from t the window reaches: `ends_before_evaluation_ms
    /// + length_ms` (saturating).
    pub fn reaches_back_ms(&self) -> u64 {
        self.ends_before_evaluation_ms
            .saturating_add(self.length_ms)
    }

    /// The smallest window holding both.
    pub fn union(self, other: Self) -> Self {
        let ends_before_evaluation_ms = self
            .ends_before_evaluation_ms
            .min(other.ends_before_evaluation_ms);
        Self {
            ends_before_evaluation_ms,
            length_ms: self
                .reaches_back_ms()
                .max(other.reaches_back_ms())
                .saturating_sub(ends_before_evaluation_ms),
        }
    }

    /// `(start, end]` of the window at evaluation time `evaluation_time_ms`,
    /// in ms (saturating).
    pub fn bounds_ms(&self, evaluation_time_ms: i64) -> (i64, i64) {
        let millis = |ms: u64| i64::try_from(ms).unwrap_or(i64::MAX);
        let end = evaluation_time_ms.saturating_sub(millis(self.ends_before_evaluation_ms));
        (end.saturating_sub(millis(self.length_ms)), end)
    }
}

/// How one node reads its time-indexed input at its evaluation time t
/// ([`Self::of`]).
///
/// | Node | `reads` | `shifts_back_ms` |
/// |---|---|---|
/// | query-time `TimeShift(1h)` | `WhatItsReadersRead` | 3,600,000 |
/// | `TimeShift(1h)` stored at ingestion time | `CurrentTimeOnly` | 3,600,000 |
/// | query-time chain | `WhatItsReadersRead` | 0 |
/// | ingestion-time chain of width w | `LastWindow { length_ms: w }` | 0 |
/// | `TimeRange(5m)` | `LastWindow { length_ms: 300_000 }` | 0 |
/// | any other operator | `CurrentTimeOnly` | 0 |
/// | `TimeShift` with `@` | `AllRows` | 0 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputTimeAccess {
    /// Which input rows it reads.
    pub reads: InputRows,
    /// How much earlier its output rows are than the rows it read: a
    /// `TimeShift`'s offset, else 0.
    pub shifts_back_ms: i64,
}

/// Which input rows a node reads at its evaluation time t
/// ([`InputTimeAccess::reads`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRows {
    /// Stores nothing itself: it reads exactly what its readers read
    /// (shifted back). A query-time `TimeShift` without `@`, or a chain
    /// built at query time, which builds the windows its readers read.
    WhatItsReadersRead,
    /// The window `(t - length_ms, t]`: a `TimeRange`'s range, or an
    /// ingestion-time chain's tumbling window (it builds its newest window
    /// from it).
    LastWindow { length_ms: u64 },
    /// Only the rows at t: any other operator, and a `TimeShift` stored at
    /// ingestion time, whose refresh reads its input's newest output.
    CurrentTimeOnly,
    /// Every row, because the time it reads does not depend on t: a
    /// `TimeShift` anchored with `@`.
    AllRows,
}

impl InputTimeAccess {
    /// How `operator`, run at `timing` with `lifetime`, reads its input.
    ///
    /// Example: `TimeRange(5m)` reads `LastWindow { length_ms: 300_000 }`,
    /// shifting back 0; `TimeShift(1h)` at query time reads
    /// `WhatItsReadersRead`, shifting back 3,600,000, and stored at
    /// ingestion time reads `CurrentTimeOnly`, shifting back 3,600,000.
    pub fn of<C>(operator: &Operator<C>, timing: ExecutionTiming, lifetime: Lifetime) -> Self {
        let reads = |reads| Self {
            reads,
            shifts_back_ms: 0,
        };
        match operator {
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => match shift.at {
                None => Self {
                    reads: match timing == ExecutionTiming::IngestionTime
                        && matches!(lifetime, Lifetime::AcrossEvaluations(_))
                    {
                        true => InputRows::CurrentTimeOnly,
                        false => InputRows::WhatItsReadersRead,
                    },
                    shifts_back_ms: shift.offset_ms,
                },
                Some(_) => reads(InputRows::AllRows),
            },
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => reads(InputRows::LastWindow {
                length_ms: u64::try_from(range.as_millis()).unwrap_or(u64::MAX),
            }),
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => match timing {
                ExecutionTiming::IngestionTime => reads(InputRows::LastWindow {
                    length_ms: *tumbling_window_width_ms,
                }),
                ExecutionTiming::QueryTime => reads(InputRows::WhatItsReadersRead),
            },
            _ => reads(InputRows::CurrentTimeOnly),
        }
    }
}

/// A plan the time walks run over: plan nodes, Stage 3 candidates or
/// executor nodes.
pub trait TimeIndexedPlan {
    type Node: Copy + Ord;
    /// The child type of the operators it holds.
    type Child;
    /// The nodes that read `node`.
    fn readers_of(&self, node: Self::Node) -> Vec<Self::Node>;
    /// The nodes `node` reads, in input order.
    fn inputs_of(&self, node: Self::Node) -> Vec<Self::Node>;
    /// How `node` reads its time-indexed input ([`InputTimeAccess::of`]).
    fn input_time_access(&self, node: Self::Node) -> InputTimeAccess;
    /// Whether a reader of `node` that reads only the rows at its
    /// evaluation time (`CurrentTimeOnly`) in fact needs every row of
    /// `node`: true for raw data, which is not stored by evaluation time.
    fn current_time_read_needs_every_row(&self, node: Self::Node) -> bool;
    /// `node`'s operator, which the walks down read: a `TimeRange`'s range,
    /// a chain's tumbling window, a `TimeShift`'s offset.
    fn operator_of(&self, node: Self::Node) -> &Operator<Self::Child>;
}

/// The plan as installed and costed: its edges give the readers and
/// inputs, each node's operator, timing and lifetime its input time access
/// ([`InputTimeAccess::of`]), and a current-time read of a `Scan` (raw
/// data) needs every row. O(edges) per node asked.
impl TimeIndexedPlan for PhysicalASAPDAG {
    type Node = PhysicalASAPNodeId;
    type Child = PhysicalASAPNodeId;

    fn readers_of(&self, node: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId> {
        let edges = self.edges.iter().filter(|edge| edge.producer == node);
        edges.map(|edge| edge.consumer).collect()
    }

    fn inputs_of(&self, node: PhysicalASAPNodeId) -> Vec<PhysicalASAPNodeId> {
        let edges = self.edges.iter().filter(|edge| edge.consumer == node);
        edges.map(|edge| edge.producer).collect()
    }

    fn input_time_access(&self, node: PhysicalASAPNodeId) -> InputTimeAccess {
        let plan_node = plan_node(self, node);
        InputTimeAccess::of(
            &plan_node.payload,
            plan_node.output_state.timing,
            plan_node.lifetime,
        )
    }

    fn current_time_read_needs_every_row(&self, node: PhysicalASAPNodeId) -> bool {
        matches!(
            plan_node(self, node).payload,
            Operator::NonASAP(NonASAPOp::Scan { .. })
        )
    }

    fn operator_of(&self, node: PhysicalASAPNodeId) -> &Operator<PhysicalASAPNodeId> {
        &plan_node(self, node).payload
    }
}

/// The node of `dag` with id `id`.
fn plan_node(
    dag: &PhysicalASAPDAG,
    id: PhysicalASAPNodeId,
) -> &crate::plan::physical_asap_dag::PhysicalASAPDAGNode {
    dag.nodes
        .iter()
        .find(|node| node.id == id)
        .expect("a node of the plan")
}

/// Up: the window each reader of `producer` needs from its rows, walking
/// up through the readers that read `WhatItsReadersRead`. `None`,
/// unbounded, when one reads `AllRows`, a `CurrentTimeOnly` reader reads
/// a producer whose current-time read needs every row (raw data,
/// [`TimeIndexedPlan::current_time_read_needs_every_row`]), or a path's
/// offsets are negative or overflow. The retention `producer` needs is the largest
/// [`TimeWindow::reaches_back_ms`].
///
/// The walk starts at the producer's own `shifts_back_ms` (a stored
/// `TimeShift(1h)` keeps each row an hour before its readers read it).
/// Each reader that reads `WhatItsReadersRead` adds its `shifts_back_ms`
/// and the walk goes on above it; a `LastWindow` reader ends the path with
/// `{offset, length}`; a `CurrentTimeOnly` reader with `{offset, 0}` (raw
/// data: unbounded).
///
/// Example: a `Scan` read by `TimeRange(5m)` and by `TimeShift(1h) ->
/// TimeRange(5m)` is read through `{0, 5m}` and `{1h, 5m}`; the
/// `TimeShift(1h)` itself through `{1h, 5m}`. A chain read by a
/// `TimeShift(1h)` stored at ingestion time is read through `{0, 0}`, its
/// newest window.
pub fn windows_readers_need<P: TimeIndexedPlan>(
    plan: &P,
    producer: P::Node,
) -> Option<Vec<(P::Node, TimeWindow)>> {
    let needs_every_row = plan.current_time_read_needs_every_row(producer);
    let mut windows = Vec::new();
    // (node whose readers are next, the offsets walked through below them)
    let mut pending = vec![(producer, plan.input_time_access(producer).shifts_back_ms)];
    let mut seen = std::collections::BTreeSet::new();
    while let Some((node, offset_ms)) = pending.pop() {
        if !seen.insert((node, offset_ms)) {
            continue;
        }
        for reader in plan.readers_of(node) {
            let access = plan.input_time_access(reader);
            let length_ms = match access.reads {
                InputRows::WhatItsReadersRead => {
                    pending.push((reader, offset_ms.checked_add(access.shifts_back_ms)?));
                    continue;
                }
                InputRows::LastWindow { length_ms } => length_ms,
                InputRows::CurrentTimeOnly if !needs_every_row => 0,
                InputRows::CurrentTimeOnly | InputRows::AllRows => return None,
            };
            windows.push((
                reader,
                TimeWindow {
                    ends_before_evaluation_ms: u64::try_from(offset_ms).ok()?,
                    length_ms,
                },
            ));
        }
    }
    Some(windows)
}

/// The sum of the offsets of the `TimeShift`s directly below `node` (each
/// the only input of the node above it). `None` when one has an `@`
/// anchor, or the sum is negative or overflows.
///
/// Example: `TimeRange(5m)` over `TimeShift(1m)` over `TimeShift(1h)`:
/// 3,660,000.
pub fn shift_back_ms_below<P: TimeIndexedPlan>(plan: &P, node: P::Node) -> Option<u64> {
    let only_input = |node| match plan.inputs_of(node).as_slice() {
        [input] => Some(*input),
        _ => None,
    };
    let mut shift_back_ms = 0i64;
    let mut current = node;
    while let Some(input) = only_input(current) {
        let Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) = plan.operator_of(input) else {
            break;
        };
        if shift.at.is_some() {
            return None;
        }
        shift_back_ms = shift_back_ms.checked_add(shift.offset_ms)?;
        current = input;
    }
    u64::try_from(shift_back_ms).ok()
}

/// Down: the windows of input rows that `node`'s result is computed from:
/// each `TimeRange` at or below it, and each `SummaryTumblingWindowChain`
/// at or below it reached without passing one (its tumbling window), each
/// over the shifts directly below it ([`shift_back_ms_below`]). `None`
/// when one is read through an `@` anchor or a negative offset.
///
/// Example: a merge of `TimeRange(5m)` over a scan and of `TimeShift(1h)
/// -> TimeRange(5m)` over it: `{0, 5m}` and `{1h, 5m}`.
pub fn windows_computed_from<P: TimeIndexedPlan>(
    plan: &P,
    node: P::Node,
) -> Option<Vec<TimeWindow>> {
    let mut windows = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut pending = vec![node];
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let length_ms = match plan.operator_of(id) {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                u64::try_from(range.as_millis()).ok()?
            }
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => *tumbling_window_width_ms,
            _ => {
                pending.extend(plan.inputs_of(id));
                continue;
            }
        };
        windows.push(TimeWindow {
            ends_before_evaluation_ms: shift_back_ms_below(plan, id)?,
            length_ms,
        });
    }
    Some(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIVE_MINUTES: TimeWindow = TimeWindow {
        ends_before_evaluation_ms: 0,
        length_ms: 300_000,
    };

    /// `(t - 6m, t - 1m]` and `(t - 5m, t]` are both held by `(t - 6m, t]`.
    #[test]
    fn the_union_of_two_windows_holds_both() {
        let shifted = TimeWindow {
            ends_before_evaluation_ms: 60_000,
            length_ms: 300_000,
        };
        assert_eq!(shifted.reaches_back_ms(), 360_000);
        assert_eq!(
            shifted.union(FIVE_MINUTES),
            TimeWindow {
                ends_before_evaluation_ms: 0,
                length_ms: 360_000
            }
        );
        assert_eq!(shifted.bounds_ms(1_000_000), (640_000, 940_000));
    }

    /// A hand-built plan: each node's readers and input time access, and
    /// whether node 0 is raw data. The walks up read only these.
    struct Plan<R, A> {
        readers: R,
        access: A,
        raw_data: bool,
        operator: Operator<u32>,
    }

    impl<R: Fn(u32) -> Vec<u32>, A: Fn(u32) -> InputTimeAccess> TimeIndexedPlan for Plan<R, A> {
        type Node = u32;
        type Child = u32;
        fn readers_of(&self, node: u32) -> Vec<u32> {
            (self.readers)(node)
        }
        fn inputs_of(&self, _: u32) -> Vec<u32> {
            Vec::new()
        }
        fn input_time_access(&self, node: u32) -> InputTimeAccess {
            (self.access)(node)
        }
        fn current_time_read_needs_every_row(&self, node: u32) -> bool {
            self.raw_data && node == 0
        }
        fn operator_of(&self, _: u32) -> &Operator<u32> {
            &self.operator
        }
    }

    /// The plan of `readers` and `access`, node 0 raw data when `raw_data`.
    fn plan<R, A>(readers: R, access: A, raw_data: bool) -> Plan<R, A> {
        Plan {
            readers,
            access,
            raw_data,
            // Read only by the walks down, which these tests do not run.
            operator: Operator::NonASAP(NonASAPOp::TimeRange {
                range: std::time::Duration::from_secs(300),
                kind: crate::ir::TimeRangeKind::Range,
                child: 0,
            }),
        }
    }

    /// Nodes 0 (a scan) ← 1 `TimeRange(5m)`, and 0 ← 2 `TimeShift(1h)` ← 3
    /// `TimeRange(5m)`; node 4, a reader at the evaluation time, reads 0
    /// too. The scan is read through `{0, 5m}` and `{1h, 5m}`, the shift
    /// through `{1h, 5m}`; the reader at the evaluation time reads raw data
    /// whole. A shift stored at ingestion time ([`InputTimeAccess::of`]) is
    /// read through `{1h, 5m}` too, and reads its input through `{0, 0}`,
    /// its newest output, or, raw data, unbounded.
    #[test]
    fn readers_read_a_producer_through_the_shifts_above_it() {
        let readers = |node: u32| -> Vec<u32> {
            match node {
                0 => vec![1, 2],
                2 => vec![3],
                _ => vec![],
            }
        };
        let reads = |reads| InputTimeAccess {
            reads,
            shifts_back_ms: 0,
        };
        let access = |node: u32| match node {
            1 | 3 => reads(InputRows::LastWindow { length_ms: 300_000 }),
            2 => InputTimeAccess {
                reads: InputRows::WhatItsReadersRead,
                shifts_back_ms: 3_600_000,
            },
            _ => reads(InputRows::CurrentTimeOnly),
        };
        let mut windows = windows_readers_need(&plan(readers, access, true), 0).unwrap();
        // The shift itself is read an hour back too: what it stores is read
        // an hour after it is stored.
        assert_eq!(
            windows_readers_need(&plan(readers, access, false), 2),
            Some(vec![(
                3,
                TimeWindow {
                    ends_before_evaluation_ms: 3_600_000,
                    length_ms: 300_000
                }
            )])
        );
        windows.sort();
        assert_eq!(
            windows,
            vec![
                (1, FIVE_MINUTES),
                (
                    3,
                    TimeWindow {
                        ends_before_evaluation_ms: 3_600_000,
                        length_ms: 300_000
                    }
                )
            ]
        );
        // Stored at ingestion time, the shift keeps the hour itself: it is
        // still read through `{1h, 5m}`, and the producer below it (here a
        // chain, not raw data) only through its newest output, `{0, 0}`.
        let shift = Operator::<u32>::NonASAP(NonASAPOp::TimeShift {
            shift: crate::ir::operator::arguments::TimeShift {
                offset_ms: 3_600_000,
                at: None,
            },
            child: 0,
        });
        let stored_shift = InputTimeAccess::of(
            &shift,
            ExecutionTiming::IngestionTime,
            Lifetime::AcrossEvaluations(crate::stages::StorageMedium::Memory),
        );
        assert_eq!(
            stored_shift,
            InputTimeAccess {
                reads: InputRows::CurrentTimeOnly,
                shifts_back_ms: 3_600_000
            }
        );
        let stored = |node: u32| match node {
            2 => stored_shift,
            node => access(node),
        };
        assert_eq!(
            windows_readers_need(&plan(readers, stored, false), 2),
            windows_readers_need(&plan(readers, access, false), 2)
        );
        // Over raw data, which has no newest output, it reads unbounded.
        assert_eq!(windows_readers_need(&plan(readers, stored, true), 0), None);
        let mut windows = windows_readers_need(&plan(readers, stored, false), 0).unwrap();
        windows.sort();
        assert_eq!(
            windows,
            vec![
                (1, FIVE_MINUTES),
                (
                    2,
                    TimeWindow {
                        ends_before_evaluation_ms: 0,
                        length_ms: 0
                    }
                )
            ]
        );
        let with_reader_at_evaluation = |node: u32| match node {
            0 => vec![1, 4],
            _ => vec![],
        };
        assert_eq!(
            windows_readers_need(&plan(with_reader_at_evaluation, access, true), 0),
            None
        );
        assert_eq!(
            windows_readers_need(&plan(with_reader_at_evaluation, access, false), 0)
                .map(|windows| windows.len()),
            Some(2)
        );
    }
}
