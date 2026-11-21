//! Step 1, widened scans: the scans of one PromQL metric that read
//! different labels become one scan of all of them, so the per-series
//! states over them, and the chains step 5 adds under those states, are
//! one.
//!
//! The front end reads only the labels a query uses:
//! `sum by (region) (increase(c[30m]))` scans `c` with `region`, and
//! `increase(c[6h])` scans it with none, so each per-series `Increase`
//! state reads its own scan group and step 5 gives each its own chain.

use std::collections::BTreeMap;

use crate::rules::candidates::{
    add_candidate_if_output_matches, add_candidates_per_input_parameters,
    add_group_computing_itself, distinct_parameters,
};
use crate::Pass2RuleError;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::Source;
use asap_types::ir::scalar::{ProjectItem, ScalarExpr};
use asap_types::ir::schema::{Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// For each metric with predicate-free scan groups that read different
/// labels, the widened scan W of every label they read; then, for each
/// per-series state group S = `SummaryAgg(TimeRange(W_S, TimeShift*(N)))`
/// with no filter over such a scan N ≠ W, the state group S_W over W
/// (`SummaryAgg(TimeRange(W_S, TimeShift*(W)))`, with S's builds) and the
/// candidate `Project` of S's columns by name over S_W in S.
///
/// Example: `sum by (region) (increase(c[30m]))` and `increase(c[6h])`.
/// The first state already reads W (`c` with `region`); the second gets
/// `Project(series identity, state)` over the 6 h state over W. Step 5
/// then adds one 15 s chain over W, which both states read, and plan
/// selection keeps it when it costs less than the two chains: the wider
/// chain stores each window's `region` too.
///
/// A per-series state copies every label of its input row into its key
/// columns, so its rows over W are its rows over N with more labels, and
/// the `Project` gives back N's.
pub(crate) fn add_widened_scan_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let mut scans_of_metric: BTreeMap<String, Vec<MemoGroupId>> = BTreeMap::new();
    for (index, group) in memo.groups().iter().enumerate() {
        if let Some(metric) = predicate_free_metric_scan(&group.expression) {
            scans_of_metric
                .entry(metric.to_string())
                .or_default()
                .push(MemoGroupId(index as u32));
        }
    }
    let mut widened_scan_of: BTreeMap<MemoGroupId, MemoGroupId> = BTreeMap::new();
    for scans in scans_of_metric.values().filter(|scans| scans.len() > 1) {
        let schemas: Vec<&Schema> = scans
            .iter()
            .map(|scan| &memo.groups()[scan.0 as usize].group_properties.schema)
            .collect();
        let Some(widened_schema) = union_of_labels(&schemas) else {
            continue;
        };
        let Operator::NonASAP(NonASAPOp::Scan { source, .. }) =
            memo.groups()[scans[0].0 as usize].expression.clone()
        else {
            unreachable!("a scan group")
        };
        let widened = Operator::NonASAP(NonASAPOp::Scan {
            source,
            predicates: Vec::new(),
            schema: widened_schema,
        });
        let widened_scan = memo.find_or_add_group(widened.clone())?;
        add_candidate_if_output_matches(
            memo,
            widened_scan,
            widened,
            None,
            OriginOfCandidateForMemoGroup::WidenedScans,
        )?;
        for &scan in scans.iter().filter(|&&scan| scan != widened_scan) {
            widened_scan_of.insert(scan, widened_scan);
        }
    }
    if widened_scan_of.is_empty() {
        return Ok(());
    }
    for state in 0..memo.groups().len() {
        let state = MemoGroupId(state as u32);
        let Some((path, scan)) = per_series_state_over_scan(memo, state) else {
            continue;
        };
        let Some(&widened_scan) = widened_scan_of.get(&scan) else {
            continue;
        };
        let group = &memo.groups()[state.0 as usize];
        let parameters = distinct_parameters(
            group
                .candidates
                .iter()
                .filter(|candidate| candidate.expression.operator == group.expression),
        );
        if parameters.is_empty() {
            continue;
        }
        // The same time path over W, each group computing itself.
        let mut rows = widened_scan;
        for operator in path.iter().rev() {
            rows = add_group_computing_itself(
                memo,
                operator.map_children(|_| rows),
                OriginOfCandidateForMemoGroup::WidenedScans,
            )?;
        }
        let build = memo.groups()[state.0 as usize]
            .expression
            .map_children(|_| rows);
        let widened_state = memo.find_or_add_group(build.clone())?;
        for theta in parameters {
            add_candidate_if_output_matches(
                memo,
                widened_state,
                build.clone(),
                theta,
                OriginOfCandidateForMemoGroup::WidenedScans,
            )?;
        }
        let Some(cols) = columns_by_name(
            &memo.groups()[state.0 as usize].group_properties.schema,
            &memo.groups()[widened_state.0 as usize]
                .group_properties
                .schema,
        ) else {
            continue;
        };
        add_candidates_per_input_parameters(
            memo,
            state,
            Operator::NonASAP(NonASAPOp::Project {
                cols,
                child: widened_state,
            }),
            OriginOfCandidateForMemoGroup::WidenedScans,
        )?;
    }
    Ok(())
}

/// The metric of a PromQL scan without predicates.
fn predicate_free_metric_scan(expression: &Operator<MemoGroupId>) -> Option<&str> {
    match expression {
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            predicates,
            ..
        }) if predicates.is_empty() => Some(metric),
        _ => None,
    }
}

/// The scan schema with every label of `schemas`: the first one's columns,
/// then each other label, by name, before the series identity, with the
/// time index and unique keys moved with their columns. `None` when the
/// schemas differ in a column of the same name, or the first one has no
/// series identity.
fn union_of_labels(schemas: &[&Schema]) -> Option<Schema> {
    let first = schemas[0];
    let identity = first
        .fields
        .iter()
        .position(|field| field.name == PROMQL_SERIES_IDENTITY)?;
    let mut fields = first.fields.clone();
    for schema in &schemas[1..] {
        for field in &schema.fields {
            match fields.iter().find(|known| known.name == field.name) {
                Some(known) if known != field => return None,
                Some(_) => {}
                None => {
                    let at = fields
                        .iter()
                        .position(|known| known.name == PROMQL_SERIES_IDENTITY)
                        .unwrap_or(identity);
                    fields.insert(at, field.clone());
                }
            }
        }
    }
    let moved = |column: usize| {
        fields
            .iter()
            .position(|field| field.name == first.fields[column].name)
            .expect("the first schema's columns are kept")
    };
    Some(Schema {
        time_index: first.time_index.map(moved),
        unique_keys: first
            .unique_keys
            .iter()
            .map(|key| key.iter().map(|&column| moved(column)).collect())
            .collect(),
        closed: first.closed,
        fields,
    })
}

/// A per-series state group without a filter over a range `TimeRange`
/// over `TimeShift`s over a scan: the `TimeRange` and shifts, top down,
/// and the scan.
fn per_series_state_over_scan(
    memo: &Memo,
    state: MemoGroupId,
) -> Option<(Vec<Operator<MemoGroupId>>, MemoGroupId)> {
    let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        reduction: Reduction::PerEntity,
        filter: None,
        ..
    }) = &memo.groups()[state.0 as usize].expression
    else {
        return None;
    };
    let range = &memo.groups()[child.0 as usize].expression;
    let Operator::NonASAP(NonASAPOp::TimeRange {
        kind: TimeRangeKind::Range,
        child: mut below,
        ..
    }) = range
    else {
        return None;
    };
    let mut path = vec![range.clone()];
    loop {
        let expression = &memo.groups()[below.0 as usize].expression;
        match expression {
            Operator::NonASAP(NonASAPOp::TimeShift { child, .. }) => {
                path.push(expression.clone());
                below = *child;
            }
            Operator::NonASAP(NonASAPOp::Scan { .. }) => return Some((path, below)),
            _ => return None,
        }
    }
}

/// `narrow`'s columns read by name from `wide`; `None` when one is not
/// there.
fn columns_by_name(narrow: &Schema, wide: &Schema) -> Option<Vec<ProjectItem<MemoGroupId>>> {
    narrow
        .fields
        .iter()
        .map(|field| {
            let column = wide
                .fields
                .iter()
                .position(|wide| wide.name == field.name)?;
            Some(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(column),
            })
        })
        .collect()
}
