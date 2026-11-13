//! The time-window lowering: the native operator of a `TimeRange` or a
//! `TimeShift` over raw data or a tumbling-window chain, and the windows a
//! node is read through, above it ([`window_read_above`]) or below it
//! ([`window_below`]), over the shared walks of
//! `asap_types::plan::time_window`.

use super::*;
use planner_types::plan::time_window::{
    end_offset_below, windows_below, windows_read_above, TimeIndexedRead,
};

/// The native operator of `id` when it is a `TimeRange` or a `TimeShift`
/// without an `@` anchor over a `Scan` of a SQL table or a time series, or
/// over a tumbling-window chain (below any stacked `TimeShift`s); `None`
/// for any other node.
///
/// A `TimeRange(w)` over shifts summing to `s` keeps the rows whose time
/// column lies in `(T - s - w, T - s]` at evaluation time `T`
/// ([`time_range_window`]), the window a PromQL selector and a SQL filter
/// against `now()` read; an ingestion run keeps the window ending at its
/// `window_end_ms` ([`Operator::time_range`]). Over a time series a stale
/// marker is not a row, and an instant window keeps each series' (each
/// label set's) latest row. Over a chain of width `c`, it keeps the windows
/// whose `window_end` lies there, `w / c` of them: `c` must divide `w` and
/// `s`, and the window must be a range. A `TimeShift` only moves the window
/// its `TimeRange` reads (the range above applies its offset), so it passes
/// the rows through. An instant window over a table is refused.
pub(super) fn time_window_operator(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
    input_schemas: &[SchemaRef],
) -> Result<Option<Operator>, Error> {
    let below = match &nodes[&id].payload {
        Payload::NonASAP(NonASAPOp::TimeRange { .. }) => first_input(id, dependencies),
        Payload::NonASAP(NonASAPOp::TimeShift { .. }) => Some(id),
        _ => return Ok(None),
    };
    let Some(below) = node_below_shifts(below, nodes, dependencies) else {
        return Ok(None);
    };
    let chain_width_ms = match &nodes[&below].payload {
        Payload::NonASAP(NonASAPOp::Scan { .. }) => None,
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => Some(
            i64::try_from(*tumbling_window_width_ms)
                .map_err(|_| invalid("tumbling-window width exceeds Int64"))?,
        ),
        _ => return Ok(None),
    };
    let [input] = input_schemas else {
        return Err(invalid("a time range or shift reads one input"));
    };
    let Payload::NonASAP(NonASAPOp::TimeRange { kind, .. }) = &nodes[&id].payload else {
        // A shift passes its rows through.
        let columns = input
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| (field.name.clone(), Expression::Column(i)))
            .collect();
        return Operator::project(input.clone(), columns).map(Some);
    };
    let Some(window) = time_range_window(id, nodes, dependencies) else {
        return Ok(None);
    };
    let millis = |ms: u64| i64::try_from(ms).map_err(|_| invalid("a time window overflows Int64"));
    let (end_offset_ms, width_ms) = (millis(window.end_offset_ms)?, millis(window.width_ms)?);
    let time = input
        .time_index
        .ok_or_else(|| invalid("a time range needs its input's time column"))?;
    let (sample_value, latest_per_series) = match (&nodes[&below].payload, kind) {
        (_, TimeRangeKind::Instant) if chain_width_ms.is_some() => {
            return Err(invalid("an instant window over a tumbling-window chain"))
        }
        (Payload::ASAP(_), TimeRangeKind::Range) => {
            let chain_width_ms = chain_width_ms.expect("a chain");
            if width_ms % chain_width_ms != 0 || end_offset_ms % chain_width_ms != 0 {
                return Err(invalid(format!(
                    "a window ({end_offset_ms} ms back, {width_ms} ms wide) is not whole \
                     tumbling windows of {chain_width_ms} ms"
                )));
            }
            (None, None)
        }
        (
            Payload::NonASAP(NonASAPOp::Scan {
                source: planner_types::ir::operator::Source::Table { .. },
                ..
            }),
            TimeRangeKind::Range,
        ) => (None, None),
        (
            Payload::NonASAP(NonASAPOp::Scan {
                source: planner_types::ir::operator::Source::Table { .. },
                ..
            }),
            TimeRangeKind::Instant,
        ) => return Err(invalid("an instant window over a SQL table is unsupported")),
        (_, kind) => {
            let value = named_column(input, &ColumnRef::SampleValue)?;
            // A series is its label set: every column but its time and value.
            let series = (*kind == TimeRangeKind::Instant).then(|| {
                (0..input.fields.len())
                    .filter(|&column| column != value && column != time)
                    .collect()
            });
            (Some(value), series)
        }
    };
    Operator::time_range(
        input.clone(),
        time,
        end_offset_ms,
        width_ms,
        sample_value,
        latest_per_series,
    )
    .map(Some)
}

/// The window the `TimeRange` `id` reads, its range over the `TimeShift`s
/// right below it ([`end_offset_below`]): `(T - s - w, T - s]`
/// at evaluation time `T`. `None` when `id` is not a `TimeRange`, or a
/// shift below it has an `@` anchor or the offsets are negative.
pub(super) fn time_range_window(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<TimeWindow> {
    let Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) = &nodes.get(&id)?.payload else {
        return None;
    };
    Some(TimeWindow {
        end_offset_ms: end_offset_below(
            id,
            |node| first_input(node, dependencies),
            |node| &nodes[&node].payload,
        )?,
        width_ms: u64::try_from(range.as_millis()).ok()?,
    })
}

/// The window back from the evaluation time that the compiled consumers of
/// `id` read ([`windows_read_above`]): through the
/// `TimeShift`s (without an `@` anchor) above it, a `TimeRange(w)` or a
/// chain of width `w` built at ingestion time (its window ending at the
/// evaluation time) over shifts summing to `s` reads `{s, w}`; several read
/// the smallest window holding all of theirs. A chain built at query time
/// builds the windows its own readers read, so the walk passes up through
/// it. `None` when `id` or a shift above it is a compile root, another
/// consumer reads it outside a window, or nothing reads it.
///
/// Example: a `Scan` read by a query-time chain of 1-minute windows, which
/// `TimeRange(5m)` and `TimeShift(1m) -> TimeRange(5m)` read, is read for
/// `{0, 6m}`.
pub(super) fn window_read_above(
    id: NodeId,
    roots: &BTreeSet<NodeId>,
    consumers: &BTreeMap<NodeId, Vec<NodeId>>,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
) -> Option<TimeWindow> {
    if roots.contains(&id) {
        return None;
    }
    windows_read_above(
        id,
        |node| consumers.get(&node).into_iter().flatten().copied(),
        |node| match TimeIndexedRead::of(
            &nodes[&node].payload,
            nodes[&node].output_state.timing,
            nodes[&node].lifetime,
        ) {
            TimeIndexedRead::Through { .. } if roots.contains(&node) => TimeIndexedRead::Whole,
            read => read,
        },
        true,
    )?
    .into_iter()
    .map(|(_, window)| window)
    .reduce(TimeWindow::union)
}

/// The window back from the evaluation time of the data `id`'s state
/// summarizes: the smallest window holding every window below it
/// ([`windows_below`]). `None` when none is below it, or one
/// is read through an `@` anchor or a negative offset.
pub(super) fn window_below(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<TimeWindow> {
    windows_below(
        id,
        |node| dependencies.get(&node).into_iter().flatten().copied(),
        |node| first_input(node, dependencies),
        |node| &nodes[&node].payload,
    )?
    .into_iter()
    .reduce(TimeWindow::union)
}

pub(super) fn first_input(
    id: NodeId,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<NodeId> {
    dependencies.get(&id).and_then(|d| d.first()).copied()
}

/// The first node at or below `start` that is not a `TimeShift`; `None`
/// when `start` is `None`, a shift has an `@` anchor, or a shift lacks its
/// child.
pub(super) fn node_below_shifts(
    start: Option<NodeId>,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<NodeId> {
    let mut below = start;
    while let Some(node) = below {
        let Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) = &nodes[&node].payload else {
            return Some(node);
        };
        if shift.at.is_some() {
            return None;
        }
        below = first_input(node, dependencies);
    }
    None
}

/// The first node at or below `node` that is not a `TimeShift`, following
/// each shift's child: a range reads its raw source through any number of
/// stacked shifts. `operator` gives a node's non-ASAP operator (`None` for an
/// ASAP one) and `resolve` a child's node; an error when a child is missing.
pub(crate) fn below_time_shifts<'a, N, C: 'a>(
    mut node: &'a N,
    operator: impl Fn(&'a N) -> Option<&'a NonASAPOp<C>>,
    resolve: impl Fn(&'a C) -> Result<&'a N, Error>,
) -> Result<&'a N, Error> {
    while let Some(NonASAPOp::TimeShift { child, .. }) = operator(node) {
        node = resolve(child)?;
    }
    Ok(node)
}

/// The logical window of the counter evaluation `id`, as `(offset,
/// lookback)`: it ends `offset` ms before the evaluation time and spans
/// `lookback` ms. It is the union of the range windows the evaluation
/// reads, each moved back by the time shifts below its range: one range for
/// a single build or for the windows of a chain it merges. `None`
/// without a range; an error when the ranges do not form one window.
pub(super) fn counter_window(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Result<Option<(i64, i64)>, Error> {
    let millis =
        |ms: u128| i64::try_from(ms).map_err(|_| invalid("counter lookback exceeds Int64"));
    let mut pending = vec![id];
    let mut visited = BTreeSet::new();
    // (offset, end of the window back from the evaluation time) per range.
    let mut windows = BTreeSet::new();
    while let Some(ancestor) = pending.pop() {
        if !visited.insert(ancestor) {
            continue;
        }
        if let Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) = &nodes[&ancestor].payload {
            let mut offset = 0i64;
            let mut below = dependencies.get(&ancestor).and_then(|d| d.first()).copied();
            while let Some(input) = below {
                let Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) = &nodes[&input].payload
                else {
                    break;
                };
                if shift.at.is_none() {
                    offset = offset
                        .checked_add(shift.offset_ms)
                        .ok_or_else(|| invalid("counter window overflows Int64"))?;
                }
                below = dependencies.get(&input).and_then(|d| d.first()).copied();
            }
            let back = offset
                .checked_add(millis(range.as_millis())?)
                .ok_or_else(|| invalid("counter window overflows Int64"))?;
            windows.insert((offset, back));
            continue;
        }
        pending.extend(dependencies.get(&ancestor).into_iter().flatten().copied());
    }
    let mut windows = windows.into_iter();
    let Some((offset, mut back)) = windows.next() else {
        return Ok(None);
    };
    // Back-to-back windows, newest first, form one window.
    for (start, end) in windows {
        if start != back {
            return Err(invalid("counter evaluation has ambiguous logical windows"));
        }
        back = end;
    }
    Ok(Some((offset, back - offset)))
}
