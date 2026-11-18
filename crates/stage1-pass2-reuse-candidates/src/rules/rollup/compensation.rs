//! The compensating `Filter` between two members of a cluster: the
//! constraints that narrow the wider member's selection to the narrower
//! one's, as a predicate over a result's columns.

use asap_types::ir::operator::arguments::SourceColumn;
use asap_types::ir::properties::selection::Constraint;
use asap_types::ir::scalar::{CompareOpKind, ExprSemantics, Predicate, ScalarExpr, ScalarValue};
use asap_types::stages::{ColumnLineage, MemoGroupId};

use super::member::{column_of, Member};

/// The per-column constraints that narrow `fine`'s selection to
/// `coarse`'s: `coarse`'s constraint on each column where it selects fewer
/// rows. `None` when `coarse`'s selection is not inside `fine`'s or
/// narrows a column that is not one of `fine`'s keys (a `Filter` over
/// `fine`'s result could not apply it).
pub(super) fn compensation(
    coarse: &Member,
    fine: &Member,
) -> Option<Vec<(SourceColumn, Constraint)>> {
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
                    return None;
                }
                if !compensation.iter().any(|(known, _)| known == column) {
                    compensation.push((column.clone(), narrow.clone()));
                }
            }
            (None, Some(_)) => return None,
            _ => {}
        }
    }
    Some(compensation)
}

/// The conjuncts selecting `compensation` from rows whose columns have
/// `lineage`, one per column; `None` when a column is not exactly one of
/// theirs or its constraint is no value set.
pub(super) fn selecting_columns(
    lineage: &[ColumnLineage],
    compensation: &[(SourceColumn, Constraint)],
) -> Option<Vec<ScalarExpr<MemoGroupId>>> {
    compensation
        .iter()
        .map(|(column, constraint)| {
            // The comparison of the frontend that reads the column's source.
            let semantics = if column.source.is_time_series() {
                ExprSemantics::Promql
            } else {
                ExprSemantics::Sql
            };
            selecting(column_of(lineage, column)?, constraint, semantics)
        })
        .collect()
}

/// The AND of `conjuncts` (at least one).
pub(super) fn conjunction(mut conjuncts: Vec<ScalarExpr<MemoGroupId>>) -> Predicate<MemoGroupId> {
    Predicate(if conjuncts.len() == 1 {
        conjuncts.remove(0)
    } else {
        ScalarExpr::BoolAnd(conjuncts)
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
