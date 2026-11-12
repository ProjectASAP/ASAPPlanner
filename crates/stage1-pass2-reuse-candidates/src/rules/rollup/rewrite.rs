//! The candidate computing a coarse group from a finer one of its cluster:
//! a regrouping merge or rollup aggregate over the finer group, under a
//! compensating `Filter` when its selection is wider.

use crate::rules::candidates::{add_candidates_per_input_parameters, add_group_computing_itself};
use crate::Pass2RuleError;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::properties::selection::Constraint;
use asap_types::ir::scalar::{CompareOpKind, ExprSemantics, Predicate, ScalarExpr, ScalarValue};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

use super::member::{column_of, Member};

/// Adds to `coarse` the candidate computing it from `fine` when (concepts
/// doc, section 3, conditions 1–5) its keys are among `fine`'s, its
/// selection lies inside `fine`'s, narrower only on `fine`'s keys, and the
/// cluster's equal definition and time window hold:
/// `Rollup(by coarse's keys)(Filter(compensation, fine))`, without the
/// `Filter` when the selections are equal. States roll up with a
/// regrouping `SummaryMerge`; sums and counts with a sum, minima and maxima
/// with themselves, each under the coarse column's name. The memo checks
/// the result is the coarse group's: a SQL global count (0 over no row)
/// does not roll up from a grouped one (its sum is NULL over no group),
/// while a grouped count does (every group it returns counts a row).
pub(super) fn rollup_from(
    memo: &mut Memo,
    coarse: &Member,
    fine: &Member,
) -> Result<(), Pass2RuleError> {
    if !coarse.keys.iter().all(|key| fine.keys.contains(key)) {
        return Ok(());
    }
    let mut compensation = Vec::new();
    for column in coarse.selection.keys().chain(fine.selection.keys()) {
        match (coarse.selection.get(column), fine.selection.get(column)) {
            (Some(narrow), wide) if Some(narrow) != wide => {
                let within = match (narrow, wide) {
                    (_, None) => true,
                    (Constraint::In(values), Some(Constraint::In(wider))) => {
                        values.iter().all(|value| wider.contains(value))
                    }
                    _ => false,
                };
                if !within || !fine.keys.contains(column) {
                    return Ok(());
                }
                if !compensation.iter().any(|(known, _)| known == column) {
                    compensation.push((column.clone(), narrow.clone()));
                }
            }
            (None, Some(_)) => return Ok(()),
            _ => {}
        }
    }
    if compensation.is_empty() && coarse.keys.len() == fine.keys.len() {
        return Ok(());
    }
    let lineage = &memo.groups()[fine.group.0 as usize]
        .group_properties
        .column_lineage;
    let mut conjuncts = Vec::new();
    for (column, constraint) in &compensation {
        let Some(at) = column_of(lineage, column) else {
            return Ok(());
        };
        // The comparison of the frontend that reads the column's source.
        let semantics = if column.source.is_time_series() {
            ExprSemantics::Promql
        } else {
            ExprSemantics::Sql
        };
        let Some(conjunct) = selecting(at, constraint, semantics) else {
            return Ok(());
        };
        conjuncts.push(conjunct);
    }
    let Some(group_by) = coarse
        .keys
        .iter()
        .map(|key| column_of(lineage, key))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(());
    };
    // The compensating `Filter` stays pending, named by the id it would get
    // as a new group, until the rollup over it derives the coarse group's
    // schema (`Memo::output_schema_of_pending_expression`): a rollup the
    // memo would reject adds no group.
    let filter = (!conjuncts.is_empty()).then(|| {
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(if conjuncts.len() == 1 {
                conjuncts.remove(0)
            } else {
                ScalarExpr::BoolAnd(conjuncts)
            }),
            child: fine.group,
        })
    });
    let mut pending = Vec::new();
    let input = match &filter {
        None => fine.group,
        Some(filter) => {
            let schema = memo.output_schema_of_pending_expression(&[], filter)?;
            pending.push((filter.clone(), schema));
            MemoGroupId(u32::try_from(memo.groups().len()).expect("at most u32::MAX groups"))
        }
    };
    let rollup = match &coarse.expression {
        Operator::ASAP(ASAPOp::SummaryAgg { .. }) => Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![input],
            group_by: Some(group_by),
        }),
        Operator::NonASAP(NonASAPOp::Aggregate {
            measures, filters, ..
        }) => {
            let Some(measures) = measures
                .iter()
                .enumerate()
                .map(|(index, measure)| rolled_up(measure, fine.keys.len() + index))
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(());
            };
            // Each rolled-up measure keeps the coarse column's name: a sum
            // of counts is named `count`, as the count it computes.
            let output_names = memo.groups()[coarse.group.0 as usize]
                .group_properties
                .schema
                .fields
                .iter()
                .skip(coarse.keys.len())
                .map(|field| field.name.clone())
                .collect();
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(group_by),
                measures,
                output_names,
                filters: vec![None; filters.len()],
                having: None,
                child: input,
            })
        }
        _ => unreachable!("a member is a SummaryAgg or an Aggregate"),
    };
    if memo
        .output_schema_of_pending_expression(&pending, &rollup)
        .ok()
        .as_ref()
        != Some(
            &memo.groups()[coarse.group.0 as usize]
                .group_properties
                .schema,
        )
    {
        return Ok(());
    }
    let rollup = match filter {
        None => rollup,
        Some(filter) => {
            let filter =
                add_group_computing_itself(memo, filter, OriginOfCandidateForMemoGroup::Rollup)?;
            rollup.map_children(|_| filter)
        }
    };
    add_candidates_per_input_parameters(
        memo,
        coarse.group,
        rollup,
        OriginOfCandidateForMemoGroup::Rollup,
    )
}

/// The measure rolling `measure`'s finalized values up from column
/// `column` of the finer result: a sum or count by a sum, a minimum or
/// maximum by itself; `None` for any other measure (it rolls up only from
/// states).
pub(super) fn rolled_up(measure: &AggIntent, column: usize) -> Option<AggIntent> {
    let col = Some(column);
    Some(match measure {
        AggIntent::Sum { .. } | AggIntent::Count => AggIntent::Sum { col },
        AggIntent::Min { .. } => AggIntent::Min { col },
        AggIntent::Max { .. } => AggIntent::Max { col },
        _ => return None,
    })
}

/// The predicate selecting the rows whose column `at` meets `constraint`,
/// with the comparison `semantics` of the frontend reading the column's
/// source: `at = v`, `at != v`, or their disjunction (`IN`) or conjunction
/// (`NOT IN`) over several values. A PromQL `!=` keeps a series without the
/// label (its value is the empty string), which only the frontend's
/// comparison says. `None` for an interval (never a compensation: the time
/// window is equal across a cluster).
fn selecting(
    at: usize,
    constraint: &Constraint,
    semantics: ExprSemantics,
) -> Option<ScalarExpr<MemoGroupId>> {
    let compare = |op: CompareOpKind, value: &ScalarValue| ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(at)),
        op,
        right: Box::new(ScalarExpr::Literal(value.clone())),
        semantics,
    };
    let (op, values, combine): (_, _, fn(Vec<_>) -> _) = match constraint {
        Constraint::In(values) => (CompareOpKind::Eq, values, ScalarExpr::BoolOr),
        Constraint::NotIn(values) => (CompareOpKind::Ne, values, ScalarExpr::BoolAnd),
        Constraint::Interval { .. } => return None,
    };
    Some(match &values[..] {
        [value] => compare(op, value),
        values => combine(
            values
                .iter()
                .map(|value| compare(op.clone(), value))
                .collect(),
        ),
    })
}
