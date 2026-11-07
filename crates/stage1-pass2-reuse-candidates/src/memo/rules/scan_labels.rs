//! Rule 2, scan widening: scans of one metric that list different labels
//! read the same rows, so a scan of the union of their labels serves each.

use std::collections::BTreeMap;

use crate::memo::rules::{add_candidate, expression, Pass2RuleError};
use asap_types::ir::operator::operator_properties::Source;
use asap_types::ir::schema::{Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, ProjectItem, ScalarExpr};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// Rule 2: for each metric whose scans without predicates list different
/// labels ([`union_schemas`]), the widened scan's group (the union of their
/// labels), and above it the widened image of every group that passes the
/// labels on ([`passes_labels_on`]): the same operator over the widened
/// input, with the original group's own-expression candidates
/// ($\boldsymbol\theta$ and $\omega$ included). Each original group whose
/// rows are not a summary state gets the candidate `Project(widened image)`
/// that lists its own columns, by name, in its order; the memo rejects it
/// when the projection does not compute the group's result (an open schema
/// loses no column it does not list). Two queries whose builds differ only
/// in the labels they list then read one widened build.
pub(crate) fn add_scan_label_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let unions = union_schemas(memo);
    let original_count = memo.groups().len();
    // Each widened group, by the original group it stands for.
    let mut widened: BTreeMap<MemoGroupId, MemoGroupId> = BTreeMap::new();
    for index in 0..original_count {
        let group = MemoGroupId(index as u32);
        let Some((source, schema)) = unfiltered_series_scan(memo, group) else {
            continue;
        };
        let Some(Some(union)) = unions.get(&source) else {
            continue;
        };
        if *union == schema {
            continue;
        }
        let mut union = union.clone();
        union.time_index = union.fields.iter().position(|field| field.name == "ts");
        let scan = Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: source },
            predicates: Vec::new(),
            schema: union,
        });
        let Ok(wide) = memo.find_or_add_group(scan.clone()) else {
            continue;
        };
        add_candidate(
            memo,
            wide,
            scan,
            None,
            None,
            None,
            OriginOfCandidateForMemoGroup::WidenedScans,
        )?;
        widened.insert(group, wide);
    }
    // Widen every group that passes labels on, inputs first (a group's
    // inputs have lower ids than the group in Stage 0's and Pass 1's memo).
    for index in 0..original_count {
        let group = MemoGroupId(index as u32);
        let own = expression(memo, group).clone();
        if widened.contains_key(&group)
            || !passes_labels_on(&own)
            || !own
                .children()
                .into_iter()
                .all(|child| widened.contains_key(child))
        {
            continue;
        }
        let image = own.map_children(|child| widened[child]);
        let Ok(wide) = memo.find_or_add_group(image.clone()) else {
            continue;
        };
        let candidates: Vec<_> = memo.groups()[index]
            .candidates
            .iter()
            .filter(|candidate| candidate.expression.operator == own)
            .map(|candidate| {
                (
                    candidate.logical_properties.parameters().cloned(),
                    candidate.logical_properties.window_layout().copied(),
                )
            })
            .collect();
        for (parameters, window_layout) in candidates {
            add_candidate(
                memo,
                wide,
                image.clone(),
                parameters,
                window_layout,
                None,
                OriginOfCandidateForMemoGroup::WidenedScans,
            )?;
        }
        widened.insert(group, wide);
    }
    for (&group, &wide) in &widened {
        if produces_state(expression(memo, group)) {
            continue;
        }
        let Some(project) = narrowed(memo, wide, group) else {
            continue;
        };
        add_candidate(
            memo,
            group,
            project,
            None,
            None,
            None,
            OriginOfCandidateForMemoGroup::WidenedScans,
        )?;
    }
    Ok(())
}

/// The metric and schema of a time-series `Scan` group without predicates.
fn unfiltered_series_scan(memo: &Memo, group: MemoGroupId) -> Option<(String, Schema)> {
    match expression(memo, group) {
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            predicates,
            schema,
        }) if predicates.is_empty() => Some((metric.clone(), schema.clone())),
        _ => None,
    }
}

/// Each metric's union schema over its scans without predicates, in group
/// order: the first scan's schema, each other label inserted before the
/// series identity column (or appended), unique keys renumbered. `None`
/// for a metric two of whose scans list one name with different fields, or
/// that one scan lists completely (`closed`) and another does not.
fn union_schemas(memo: &Memo) -> BTreeMap<String, Option<Schema>> {
    let mut unions: BTreeMap<String, Option<Schema>> = BTreeMap::new();
    for index in 0..memo.groups().len() {
        let Some((metric, schema)) = unfiltered_series_scan(memo, MemoGroupId(index as u32)) else {
            continue;
        };
        let Some(entry) = unions.get_mut(&metric) else {
            unions.insert(metric, Some(schema));
            continue;
        };
        let Some(union) = entry.as_mut() else {
            continue;
        };
        if union.closed != schema.closed {
            *entry = None;
            continue;
        }
        for field in &schema.fields {
            match union.fields.iter().find(|known| known.name == field.name) {
                Some(known) if known == field => {}
                Some(_) => {
                    *entry = None;
                    break;
                }
                None => {
                    let at = union
                        .fields
                        .iter()
                        .position(|known| known.name == PROMQL_SERIES_IDENTITY)
                        .unwrap_or(union.fields.len());
                    union.fields.insert(at, field.clone());
                    for column in union.unique_keys.iter_mut().flatten() {
                        if *column >= at {
                            *column += 1;
                        }
                    }
                }
            }
        }
    }
    unions
}

/// Whether `operator` passes every label of its input on unchanged, so it
/// may run over the widened scan: a time range, a time shift, a per-series
/// summary build without a filter, a merge, a readout or a finalize.
fn passes_labels_on(operator: &Operator<MemoGroupId>) -> bool {
    match operator {
        Operator::NonASAP(op) => matches!(
            op,
            NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. }
        ),
        Operator::ASAP(op) => match op {
            ASAPOp::SummaryAgg {
                reduction, filter, ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                reduction, filter, ..
            } => reduction.group_keys().is_none() && filter.is_none(),
            ASAPOp::SummaryMerge { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. } => true,
            _ => false,
        },
    }
}

/// Whether `operator`'s rows are summary states, which a `Project` cannot
/// narrow.
fn produces_state(operator: &Operator<MemoGroupId>) -> bool {
    matches!(
        operator,
        Operator::ASAP(
            ASAPOp::SummaryAgg { .. }
                | ASAPOp::SummaryTumblingWindowChain { .. }
                | ASAPOp::SummaryMerge { .. }
        )
    )
}

/// `Project(wide)` listing `group`'s columns, by name, in its order; `None`
/// when `wide` lacks one of them.
fn narrowed(memo: &Memo, wide: MemoGroupId, group: MemoGroupId) -> Option<Operator<MemoGroupId>> {
    let from = &memo.groups()[wide.0 as usize].group_properties.schema;
    let to = &memo.groups()[group.0 as usize].group_properties.schema;
    let cols = to
        .fields
        .iter()
        .map(|field| {
            let column = from
                .fields
                .iter()
                .position(|known| known.name == field.name)?;
            Some(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(column),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Operator::NonASAP(NonASAPOp::Project { cols, child: wide }))
}
