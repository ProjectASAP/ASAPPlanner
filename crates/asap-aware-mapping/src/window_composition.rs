//! Exact window composition over mergeable summary states.
//!
//! Candidate generation is independent of costing. Relative pane selectors keep
//! the original source, predicates and anchor; stored outputs must still be
//! bound to the evaluation window and revision by the physical runtime.

use std::{rc::Rc, time::Duration};

use asap_types::{
    ir::{operator_properties::TimeShift, ASAPOp, NonASAPOp, Operator, OperatorNode},
    workload::{QueryRecurrence, QueryWorkloadEntry, RepeatedDemand},
};

#[derive(Debug, thiserror::Error)]
pub enum WindowCompositionError {
    #[error("window composition exceeds candidate budget; no partial inventory returned")]
    BudgetExceeded,
    #[error("window composition: {0}")]
    Schema(#[from] asap_types::ir::SchemaDerivationError),
}

/// Enumerate retaining each state and composing it from disjoint cadence-sized
/// panes. A fixed recurrence without a phase remains executable by rebuilding
/// relative panes; it does not certify reuse of a catalog's pane layout.
/// Every combination of eligible states is returned, including the original.
pub fn enumerate_window_compositions(
    root: &Rc<OperatorNode>,
    entry: &QueryWorkloadEntry,
    limit: usize,
) -> Result<Vec<Rc<OperatorNode>>, WindowCompositionError> {
    let cadence = match &entry.recurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval))
        | QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt { interval, .. }) => {
            u64::from(interval.0)
        }
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => times
            .windows(2)
            .filter_map(|pair| pair[1].0.checked_sub(pair[0].0))
            .fold(0, gcd),
        _ => 0,
    };
    if limit == 0 {
        return Err(WindowCompositionError::BudgetExceeded);
    }
    if cadence == 0 {
        return Ok(vec![Rc::clone(root)]);
    }
    let mut sites = Vec::new();
    for node in OperatorNode::reachable(root) {
        if let Some(composed) = compose(&node, cadence, limit)? {
            sites.push((node, composed));
        }
    }
    let count = 1usize
        .checked_shl(u32::try_from(sites.len()).unwrap_or(u32::MAX))
        .filter(|count| *count <= limit)
        .ok_or(WindowCompositionError::BudgetExceeded)?;
    let mut candidates = Vec::with_capacity(count);
    for mask in 0..count {
        fn rebuild(
            node: &Rc<OperatorNode>,
            sites: &[(Rc<OperatorNode>, Rc<OperatorNode>)],
            mask: usize,
        ) -> Result<Rc<OperatorNode>, WindowCompositionError> {
            if let Some((index, (_, composed))) = sites
                .iter()
                .enumerate()
                .find(|(_, (original, _))| Rc::ptr_eq(original, node))
            {
                if mask & (1 << index) != 0 {
                    return Ok(Rc::clone(composed));
                }
            }
            let children = node.children();
            let rebuilt = children
                .iter()
                .map(|child| rebuild(child, sites, mask))
                .collect::<Result<Vec<_>, _>>()?;
            if children.iter().zip(&rebuilt).all(|(a, b)| Rc::ptr_eq(a, b)) {
                return Ok(Rc::clone(node));
            }
            let replacements = children.iter().zip(&rebuilt);
            let mut result = node.map_children(|child| {
                // map_children also visits scalar plan references.
                replacements
                    .clone()
                    .find(|(original, _)| Rc::ptr_eq(original, child))
                    .map_or_else(|| Rc::clone(child), |(_, rebuilt)| Rc::clone(rebuilt))
            })?;
            result.guarantee = node.guarantee.clone();
            Ok(Rc::new(result))
        }
        candidates.push(rebuild(root, &sites, mask)?);
    }
    Ok(candidates)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn compose(
    state: &Rc<OperatorNode>,
    cadence: u64,
    limit: usize,
) -> Result<Option<Rc<OperatorNode>>, WindowCompositionError> {
    let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &state.operator else {
        return Ok(None);
    };
    let original_input = child;
    let Some(NonASAPOp::TimeRange { range, kind, child }) = child.non_asap() else {
        return Ok(None);
    };
    let Ok(lookback) = u64::try_from(range.as_millis()) else {
        return Ok(None);
    };
    if *range != Duration::from_millis(lookback) || lookback == 0 {
        return Ok(None);
    }
    let width = gcd(lookback, cadence);
    let count = lookback / width;
    if count <= 1 {
        return Ok(None);
    }
    if count > limit as u64 || lookback > i64::MAX as u64 {
        return Err(WindowCompositionError::BudgetExceeded);
    }
    let (source, shift) = match child.non_asap() {
        Some(NonASAPOp::Scan { .. }) => (child, TimeShift::default()),
        Some(NonASAPOp::TimeShift { child, shift })
            if matches!(child.non_asap(), Some(NonASAPOp::Scan { .. })) =>
        {
            (child, *shift)
        }
        _ => return Ok(None),
    };
    if !matches!(
        source.non_asap(),
        Some(NonASAPOp::Scan {
            source: asap_types::ir::operator_properties::Source::TimeSeries { .. },
            ..
        })
    ) {
        return Ok(None);
    }
    let mut panes = Vec::new();
    for pane in 0..count {
        let Some(offset_ms) = shift.offset_ms.checked_add((pane * width) as i64) else {
            return Ok(None);
        };
        let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
            child: Rc::clone(source),
            shift: TimeShift { offset_ms, ..shift },
        }))?;
        let input = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_millis(width),
            kind: *kind,
            child: shifted,
        }))?;
        panes.push(Rc::new(state.map_children(|original| {
            if Rc::ptr_eq(original, original_input) {
                Rc::clone(&input)
            } else {
                Rc::clone(original)
            }
        })?));
    }
    // Derivation checks identical state schemas; downstream physical compilation
    // remains responsible for the concrete family's merge capability.
    let Ok(mut merged) =
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge { children: panes }))
    else {
        return Ok(None);
    };
    merged.schema = state.schema.clone();
    merged.guarantee = state.guarantee.clone();
    Ok(Some(Rc::new(merged)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::{
        ir::operator_properties::{Reduction, Source},
        ir::TimeRangeKind,
        post_asap::{GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate},
        pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema},
        workload::{Predictability, Query, QueryRequirements, RepetitionInterval, TimeSelection},
    };

    fn fixture() -> (Rc<OperatorNode>, QueryWorkloadEntry) {
        let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
        }))
        .unwrap();
        let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scan,
        }))
        .unwrap();
        let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
            child: range,
            family: FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
                Default::default(),
            ),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::by(vec![]),
            grouping: GroupingStrategy::default(),
            filter: None,
        }))
        .unwrap();
        let entry = QueryWorkloadEntry {
            query: Query("quantile_over_time(0.99, events[5m])".into()),
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            requirements: QueryRequirements::default(),
            predictability: Predictability::AdHoc,
            time_selection: TimeSelection::default(),
        };
        (state, entry)
    }

    /// Automatic composition covers the original interval once, without gaps or overlap.
    #[test]
    fn five_minute_window_has_original_and_five_exact_panes() {
        let (state, entry) = fixture();
        let variants = enumerate_window_compositions(&state, &entry, 16).unwrap();
        assert_eq!(variants.len(), 2);
        assert!(Rc::ptr_eq(&variants[0], &state));
        let Operator::ASAP(ASAPOp::SummaryMerge { children }) = &variants[1].operator else {
            panic!("expected merge")
        };
        assert_eq!(children.len(), 5);
        for (index, pane) in children.iter().enumerate() {
            let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &pane.operator else {
                panic!()
            };
            let Some(NonASAPOp::TimeRange { range, child, .. }) = child.non_asap() else {
                panic!()
            };
            assert_eq!(*range, Duration::from_secs(60));
            let Some(NonASAPOp::TimeShift { shift, .. }) = child.non_asap() else {
                panic!()
            };
            assert_eq!(shift.offset_ms, index as i64 * 60_000);
        }
        variants[1].validate_structure().unwrap();
    }

    /// Unknown cadence retains the original; an exhausted budget never returns a partial search.
    #[test]
    fn unknown_cadence_and_budget_are_explicit() {
        let (state, mut entry) = fixture();
        assert!(matches!(
            enumerate_window_compositions(&state, &entry, 4),
            Err(WindowCompositionError::BudgetExceeded)
        ));
        entry.recurrence = QueryRecurrence::OneTime {
            invocations: 1,
            execute_at: None,
        };
        assert_eq!(
            enumerate_window_compositions(&state, &entry, 4)
                .unwrap()
                .len(),
            1
        );
    }
}
