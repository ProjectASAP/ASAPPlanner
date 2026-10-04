//! Pass 2's window-composition rule (#509), tumbling windows (#580 W1–W4).
//!
//! A query that reads the last `lookback` every `cadence` can answer each
//! evaluation by merging back-to-back panes of width
//! `gcd(lookback, cadence)`: the pane width divides both, so every window
//! starts and ends on a pane boundary (#509, "Tumbling window"). The window
//! form is an axis on Pass 1 alternatives: every alternative whose summary
//! family merges ([`FieldDataType::family_merges`]) also gets a tumbling
//! form next to its whole-window one. Panes of one width over one input are
//! structurally identical, so the identical-expression rule shares them
//! across queries.
//!
//! Pane `i` of width `w` is `SummaryAgg` over `TimeRange(w)` over
//! `TimeShift(o + i·w)` over the `Scan`, where `o` is the query's own
//! offset. Its coverage is derived from that sub-DAG: the selection's
//! relative time is `(-(o + (i + 1)·w), -(o + i·w)]` (W2). A `SummaryMerge`
//! combines the panes (W1), which derivation accepts only for disjoint
//! panes; a tumbling form is used only when the merged coverage is exactly
//! the whole-window state's. Stage 2 still
//! runs every node at query time, so until it plans materialization each
//! evaluation rebuilds all panes.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::{Source, TimeShift};
use asap_types::ir::schema::{FieldDataType, GroupingStrategy};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
use asap_types::workload::{QueryRecurrence, RepeatedDemand, RootDemand};

use crate::pass1::logical_candidates::{LocalLogicalCandidates, LogicalCandidateError};
use crate::pass1::replacement::Realization;

/// How a summary alternative covers its query's window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowForm {
    /// One state built over the whole window.
    #[default]
    Whole,
    /// Back-to-back panes of `pane_ms`, merged at each evaluation.
    Tumbling { pane_ms: u64 },
}

impl WindowForm {
    /// E.g. "tumbling 1m panes"; `None` for the whole window.
    pub fn label(self) -> Option<String> {
        match self {
            WindowForm::Whole => None,
            WindowForm::Tumbling { pane_ms } => {
                Some(format!("tumbling {} panes", duration_label(pane_ms)))
            }
        }
    }
}

fn duration_label(ms: u64) -> String {
    for (unit, size) in [("h", 3_600_000), ("m", 60_000), ("s", 1_000)] {
        if ms.is_multiple_of(size) {
            return format!("{}{unit}", ms / size);
        }
    }
    format!("{ms}ms")
}

/// Most panes one window is split into. A finer split multiplies pane
/// builds and merge inputs for no reuse the cadence can exploit.
pub const MAX_PANES: u64 = 64;

/// The interval between evaluations: a fixed interval, or the gcd of the gaps
/// between scheduled times. `None` for one-off, estimated or unknown demand.
pub fn evaluation_cadence_ms(recurrence: &QueryRecurrence) -> Option<u64> {
    let cadence = match recurrence {
        QueryRecurrence::Repeated(
            RepeatedDemand::FixedInterval(interval)
            | RepeatedDemand::FixedIntervalAt { interval, .. },
        ) => u64::from(interval.0),
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => times
            .windows(2)
            .filter_map(|pair| pair[1].0.checked_sub(pair[0].0))
            .fold(0, gcd),
        _ => 0,
    };
    (cadence > 0).then_some(cadence)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The pane width for a window of `lookback_ms` evaluated every
/// `cadence_ms`: their gcd, when it splits the window into 2 to
/// [`MAX_PANES`] panes.
pub fn pane_width_ms(lookback_ms: u64, cadence_ms: u64) -> Option<u64> {
    if lookback_ms == 0 || cadence_ms == 0 {
        return None;
    }
    let width = gcd(lookback_ms, cadence_ms);
    (2..=MAX_PANES)
        .contains(&(lookback_ms / width))
        .then_some(width)
}

/// The window an aggregate's input reads: a range selector over a time
/// series, optionally offset (an `@` anchor is absolute time and excluded).
struct Window<'a> {
    lookback_ms: u64,
    offset_ms: i64,
    scan: &'a Rc<OperatorNode>,
}

fn window(input: &Rc<OperatorNode>) -> Option<Window<'_>> {
    let Some(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child,
    }) = input.non_asap()
    else {
        return None;
    };
    let lookback_ms = u64::try_from(range.as_millis()).ok()?;
    if *range != Duration::from_millis(lookback_ms) || i64::try_from(lookback_ms).is_err() {
        return None;
    }
    let (scan, offset_ms) = match child.non_asap() {
        Some(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms,
                at: None,
            },
            child,
        }) => (child, *offset_ms),
        _ => (child, 0),
    };
    matches!(
        scan.non_asap(),
        Some(NonASAPOp::Scan {
            source: Source::TimeSeries { .. },
            ..
        })
    )
    .then_some(Window {
        lookback_ms,
        offset_ms,
        scan,
    })
}

/// The scan a tumbling form of `target` would read its panes from: equal
/// scans make panes of equal width identical across targets.
pub fn pane_source(target: &OperatorNode) -> Option<&Rc<OperatorNode>> {
    match target.non_asap() {
        Some(NonASAPOp::Aggregate { child, .. }) => window(child).map(|w| w.scan),
        _ => None,
    }
}

/// Whether panes of `pane_ms` tile the window exactly. That the panes'
/// derived coverage is the whole window is checked again when a tumbling
/// form is realized ([`tumbling_state`]).
fn panes_tile_window(window: &Window<'_>, pane_ms: u64) -> bool {
    pane_ms > 0 && window.lookback_ms.is_multiple_of(pane_ms)
}

/// Add the tumbling form of every mergeable alternative whose target reads
/// a range window, with the pane width of the target's queries
/// (`demand[i]` is the demand of `inventory.roots[i]`). A target read by
/// several queries uses the gcd of their cadences; one read by a query
/// without a cadence gets no tumbling form.
pub fn add_window_forms<Id>(inventory: &mut LocalLogicalCandidates<Id>, demand: &[RootDemand]) {
    let mut cadence: HashMap<*const OperatorNode, Option<u64>> = HashMap::new();
    for (index, (_, root)) in inventory.roots.iter().enumerate() {
        let root_cadence = demand
            .get(index)
            .and_then(|d| evaluation_cadence_ms(&d.recurrence));
        let operators = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for node in operators.into_iter().flat_map(OperatorNode::reachable) {
            let entry = cadence.entry(Rc::as_ptr(&node)).or_insert(root_cadence);
            *entry = match (*entry, root_cadence) {
                (Some(a), Some(b)) => Some(gcd(a, b)),
                _ => None,
            };
        }
    }
    for target in &mut inventory.targets {
        let Some(NonASAPOp::Aggregate { child, .. }) = target.target.non_asap() else {
            continue;
        };
        let Some(window) = window(child) else {
            continue;
        };
        let Some(pane_ms) = cadence
            .get(&Rc::as_ptr(&target.target))
            .copied()
            .flatten()
            .and_then(|cadence| pane_width_ms(window.lookback_ms, cadence))
        else {
            continue;
        };
        if !panes_tile_window(&window, pane_ms) {
            continue;
        }
        let tumbling: Vec<(Realization, GroupingStrategy)> = target
            .alternatives
            .iter()
            .zip(&target.absorbs)
            .zip(&target.groupings)
            .filter(|((alternative, absorbs), grouping)| {
                absorbs.is_none()
                    && family(alternative, grouping).is_some_and(|f| f.family_merges())
            })
            .map(|((alternative, _), grouping)| (alternative.clone(), grouping.clone()))
            .collect();
        for (alternative, grouping) in tumbling {
            target.alternatives.push(alternative);
            target.absorbs.push(None);
            target.windows.push(WindowForm::Tumbling { pane_ms });
            target.groupings.push(grouping);
        }
    }
}

fn family(realization: &Realization, grouping: &GroupingStrategy) -> Option<FieldDataType> {
    match realization {
        Realization::ExactAggregate { kind, params } => {
            Some(FieldDataType::ExactAggregate(kind.clone(), params.clone()))
        }
        Realization::Sketch(kind) => Some(FieldDataType::Sketch(kind.clone(), grouping.clone())),
        _ => None,
    }
}

/// Realize a tumbling form over `input`, the target's window: pane `i`
/// reads `TimeRange(w)` over `TimeShift(o + i·w)` over the scan, `pane`
/// builds its state, and a `SummaryMerge` combines them. Fails when the
/// merged coverage is not the coverage of `pane` over the whole window.
pub(crate) fn tumbling_state(
    input: &Rc<OperatorNode>,
    pane_ms: u64,
    pane: impl Fn(Rc<OperatorNode>) -> Result<Rc<OperatorNode>, LogicalCandidateError>,
) -> Result<Rc<OperatorNode>, LogicalCandidateError> {
    let unsupported = || LogicalCandidateError::Unsupported("tumbling panes over this window");
    let window = window(input).ok_or_else(unsupported)?;
    let Some(NonASAPOp::TimeRange { kind, .. }) = input.non_asap() else {
        return Err(unsupported());
    };
    let width = i64::try_from(pane_ms).map_err(|_| unsupported())?;
    let mut panes = Vec::new();
    for i in 0..(window.lookback_ms / pane_ms) as i64 {
        let offset_ms = window
            .offset_ms
            .checked_add(i * width)
            .ok_or_else(unsupported)?;
        let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
            child: Rc::clone(window.scan),
            shift: TimeShift {
                offset_ms,
                at: None,
            },
        }))?;
        let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_millis(pane_ms),
            kind: *kind,
            child: shifted,
        }))?;
        panes.push(pane(range)?);
    }
    let merged =
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children: panes }))?;
    if merged.coverage() != pane(Rc::clone(input))?.coverage() {
        return Err(LogicalCandidateError::Unsupported(
            "tumbling panes do not cover the whole window",
        ));
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::logical_candidates::enumerate_local_logical_candidates;
    use crate::test_support::lower_promql;
    use asap_types::ir::schema::SketchAlgorithm;
    use asap_types::types::AccuracyTarget;
    use asap_types::workload::{Predictability, RepetitionInterval};
    use std::collections::BTreeMap;
    use std::ops::Bound;

    fn every(ms: u32) -> RootDemand {
        RootDemand {
            accuracy: None,
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(ms),
            )),
            predictability: Predictability::Unknown,
            latency_ms: None,
        }
    }

    fn inventory(query: &str, demand: &[RootDemand]) -> LocalLogicalCandidates<usize> {
        let root = lower_promql(
            query,
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        );
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
        let mut inventory = enumerate_local_logical_candidates(
            vec![(0, QueryRoot::Operator(root))],
            &BTreeMap::new(),
        )
        .unwrap();
        add_window_forms(&mut inventory, demand);
        inventory
    }

    /// The pane width is the gcd of the window and the cadence, within
    /// 2..=MAX_PANES panes.
    #[test]
    fn pane_width_is_the_gcd_of_window_and_cadence() {
        assert_eq!(pane_width_ms(300_000, 60_000), Some(60_000));
        assert_eq!(pane_width_ms(60_000, 10_000), Some(10_000));
        assert_eq!(pane_width_ms(300_000, 120_000), Some(60_000));
        // One pane is the whole window; too many panes are not offered.
        assert_eq!(pane_width_ms(60_000, 60_000), None);
        assert_eq!(pane_width_ms(3_600_000, 7_000), None);
        assert_eq!(pane_width_ms(300_000, 0), None);
        let scheduled = QueryRecurrence::Repeated(RepeatedDemand::Scheduled(vec![
            asap_types::workload::TimestampMs(0),
            asap_types::workload::TimestampMs(60_000),
            asap_types::workload::TimestampMs(150_000),
        ]));
        assert_eq!(evaluation_cadence_ms(&scheduled), Some(30_000));
        let once = QueryRecurrence::OneTime {
            invocations: 1,
            execute_at: None,
        };
        assert_eq!(evaluation_cadence_ms(&once), None);
    }

    /// A 5-min p99 every minute: KLL and DDSketch each get a 1-min tumbling
    /// form, labeled as such; a one-off query gets none.
    #[test]
    fn mergeable_sketches_get_a_tumbling_form() {
        let query = "quantile_over_time(0.99, m[5m])";
        let inventory = inventory(query, &[every(60_000)]);
        let [target] = inventory.targets.as_slice() else {
            panic!("one target")
        };
        let tumbling: Vec<_> = target
            .alternatives
            .iter()
            .zip(&target.windows)
            .filter(|(_, w)| **w != WindowForm::Whole)
            .map(|(a, w)| match a {
                Realization::Sketch(kind) => (kind.algorithm().clone(), *w),
                other => panic!("{other:?}"),
            })
            .collect();
        let form = WindowForm::Tumbling { pane_ms: 60_000 };
        assert_eq!(
            tumbling,
            [
                (SketchAlgorithm::Kll, form),
                (SketchAlgorithm::DDSketch, form)
            ]
        );
        assert_eq!(form.label().as_deref(), Some("tumbling 1m panes"));
        let once = self::inventory(query, &[]);
        assert!(once.targets[0]
            .windows
            .iter()
            .all(|w| *w == WindowForm::Whole));
    }

    /// Rate and increase depend on their window's edges, so their
    /// accumulators do not merge and get no tumbling form.
    #[test]
    fn non_mergeable_family_gets_no_tumbling_form() {
        for query in ["rate(m[1m])", "increase(m[1m])"] {
            let inventory = inventory(query, &[every(10_000)]);
            assert!(
                inventory
                    .targets
                    .iter()
                    .flat_map(|t| &t.windows)
                    .all(|w| *w == WindowForm::Whole),
                "{query}"
            );
        }
        // An exact sum merges.
        let inventory = inventory("sum_over_time(m[1m])", &[every(10_000)]);
        assert!(inventory.targets[0]
            .windows
            .contains(&WindowForm::Tumbling { pane_ms: 10_000 }));
    }

    /// Pane `i` reads `TimeRange(w)` over `TimeShift(o + i·w)` over the one
    /// scan and derives the relative time `(-(o + (i + 1)·w), -(o + i·w)]`;
    /// the merge covers the whole window.
    #[test]
    fn panes_partition_the_window() {
        for (query, offset) in [
            ("quantile_over_time(0.99, m[5m])", 0),
            ("quantile_over_time(0.99, m[5m] offset 1h)", 3_600_000),
        ] {
            let inventory = inventory(query, &[every(60_000)]);
            let target = &inventory.targets[0];
            let Some(NonASAPOp::Aggregate { child, .. }) = target.target.non_asap() else {
                panic!("aggregate")
            };
            let merged = tumbling_state(child, 60_000, kll_state(target)).unwrap();
            let Operator::ASAP(ASAPOp::SummaryMerge { children }) = &merged.operator else {
                panic!("merge")
            };
            assert_eq!(children.len(), 5);
            let scans: std::collections::HashSet<_> =
                children.iter().map(|pane| pane_source_of(pane)).collect();
            assert_eq!(scans.len(), 1, "every pane reads one scan");
            for (i, pane) in children.iter().enumerate() {
                let i = i as i64;
                let Some(NonASAPOp::TimeRange { range, child, .. }) = pane.children()[0].non_asap()
                else {
                    panic!("range")
                };
                assert_eq!(*range, Duration::from_secs(60));
                let Some(NonASAPOp::TimeShift { shift, .. }) = child.non_asap() else {
                    panic!("shift")
                };
                assert_eq!(shift.offset_ms, offset + i * 60_000);
                assert_eq!(
                    pane.coverage().unwrap().selection[0].relative_time,
                    Some((
                        Bound::Excluded(-(offset + (i + 1) * 60_000)),
                        Bound::Included(-(offset + i * 60_000))
                    ))
                );
            }
            let selection = &merged.coverage().unwrap().selection;
            assert_eq!(selection.len(), 1, "adjacent panes join");
            assert_eq!(
                selection[0].relative_time,
                Some((
                    Bound::Excluded(-(offset + 300_000)),
                    Bound::Included(-offset)
                ))
            );
        }
    }

    /// A tumbling form answers with the same schema as its whole-window form,
    /// for exact accumulators (finalized through the merge) and sketches.
    #[test]
    fn tumbling_and_whole_forms_have_one_output_schema() {
        use crate::pass1::logical_candidates::compose_logical_candidate;
        for (query, cadence) in [
            ("sum_over_time(m[1m])", 10_000),
            ("topk by (job) (10, sum_over_time(m[1m]))", 10_000),
            ("quantile_over_time(0.99, m[5m])", 60_000),
        ] {
            let inventory = inventory(query, &[every(cadence)]);
            let (t, target) = inventory
                .targets
                .iter()
                .enumerate()
                .find(|(_, t)| t.windows.iter().any(|w| *w != WindowForm::Whole))
                .expect("a tumbling form");
            let schema = |c: usize| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[t] = c;
                let roots = compose_logical_candidate(&inventory, &choice).unwrap();
                let QueryRoot::Operator(root) = &roots[0].1 else {
                    panic!("operator root")
                };
                root.schema.clone()
            };
            for (c, window) in target.windows.iter().enumerate() {
                if *window == WindowForm::Whole {
                    continue;
                }
                let whole = target.alternatives[..c]
                    .iter()
                    .position(|a| *a == target.alternatives[c])
                    .expect("the whole-window form");
                assert_eq!(schema(c), schema(whole), "{query}");
            }
        }
    }

    fn pane_source_of(pane: &OperatorNode) -> *const OperatorNode {
        let range = pane.children()[0];
        let shift = range.children()[0];
        Rc::as_ptr(shift.children()[0])
    }

    /// A KLL state over `input`, per series, for `target`'s alternative 1.
    fn kll_state(
        target: &crate::pass1::logical_candidates::LocalLogicalTarget,
    ) -> impl Fn(Rc<OperatorNode>) -> Result<Rc<OperatorNode>, LogicalCandidateError> + '_ {
        move |input| {
            Ok(OperatorNode::new_shared(Operator::ASAP(
                ASAPOp::SummaryAgg {
                    child: input,
                    family: family(&target.alternatives[1], &GroupingStrategy::default()).unwrap(),
                    input: asap_types::ir::schema::SummaryUpdate::column(
                        asap_types::ir::scalar::ColumnRef::SampleValue,
                    ),
                    reduction: asap_types::ir::operator::Reduction::PerEntity,
                    grouping: GroupingStrategy::default(),
                    filter: None,
                },
            ))?)
        }
    }

    /// Panes that would not tile the window are not offered, and panes
    /// that overlap do not merge: their derived selections share rows.
    #[test]
    fn overlapping_or_partial_panes_are_rejected() {
        let scan = OperatorNode::reachable(&lower_promql("m", AccuracyTarget::Exact))
            .into_iter()
            .find(|n| matches!(n.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap();
        let window = Window {
            lookback_ms: 300_000,
            offset_ms: 0,
            scan: &scan,
        };
        assert!(panes_tile_window(&window, 60_000));
        assert!(!panes_tile_window(&window, 70_000));

        let inventory = inventory("quantile_over_time(0.99, m[5m])", &[every(60_000)]);
        let target = &inventory.targets[0];
        let Some(NonASAPOp::Aggregate { child, .. }) = target.target.non_asap() else {
            panic!("aggregate")
        };
        let Some(NonASAPOp::TimeRange {
            kind, child: scan, ..
        }) = child.non_asap()
        else {
            panic!("range")
        };
        let pane = |shift_ms: i64, width_ms: u64| {
            let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
                child: Rc::clone(scan),
                shift: TimeShift {
                    offset_ms: shift_ms,
                    at: None,
                },
            }))
            .unwrap();
            let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
                range: Duration::from_millis(width_ms),
                kind: *kind,
                child: shifted,
            }))
            .unwrap();
            kll_state(target)(range).unwrap()
        };
        let merge =
            |children| OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children }));
        merge(vec![pane(0, 60_000), pane(60_000, 60_000)]).unwrap();
        assert!(merge(vec![pane(0, 120_000), pane(60_000, 60_000)]).is_err());
        // Panes of 2m over a 5m window leave the oldest minute uncovered.
        assert!(tumbling_state(child, 120_000, kll_state(target)).is_err());
    }
}
