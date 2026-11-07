//! Rule 5, finer aggregate: a SQL aggregate read from a finer aggregate's
//! result: filter the finer rows, then merge them into the query's groups
//! with each measure's rollup.

use crate::memo::rules::{add_candidate, expression, Pass2RuleError};
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::scalar::expr_ir::CompareOpKind;
use asap_types::ir::{NonASAPOp, Operator, OperatorResultKind, Predicate, ScalarExpr};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// Rule 5: for two SQL aggregate groups with identical measures over rows
/// of one scan under the same time ranges ([`match_finer`]), the coarser
/// one's candidate `Aggregate(by query keys)` over the finer group's
/// result, through a `Filter` group of the query's extra conjuncts when it
/// has any. Each measure reads the finer value with its rollup
/// ([`value_rollup`]); without a rollup, each query group is one finer row,
/// read with `min`. The memo rejects a candidate whose output is not the
/// group's (a sum of counts of another type).
pub(crate) fn add_finer_aggregate_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let aggregates: Vec<MemoGroupId> = (0..memo.groups().len())
        .map(|index| MemoGroupId(index as u32))
        .filter(|&group| aggregate(memo, group).is_some())
        .collect();
    for &owner in &aggregates {
        for &finer in &aggregates {
            if owner == finer {
                continue;
            }
            let (Some(query), Some(fine)) = (aggregate(memo, owner), aggregate(memo, finer)) else {
                continue;
            };
            if format!("{:?}", query.measures) != format!("{:?}", fine.measures) {
                continue;
            }
            let Some(matched) = match_finer(memo, query.child, &query.keys, fine.child, &fine.keys)
            else {
                continue;
            };
            let global_count = query.keys.is_empty();
            let measures = query
                .measures
                .iter()
                .enumerate()
                .map(|(index, measure)| {
                    let col = Some(fine.keys.len() + index);
                    if !matched.needs_rollup
                        && !(global_count && matches!(measure, AggIntent::Count { .. }))
                    {
                        Some(AggIntent::Min { col })
                    } else {
                        value_rollup(measure, col, !query.keys.is_empty())
                    }
                })
                .collect::<Option<Vec<_>>>();
            let Some(measures) = measures else {
                continue;
            };
            let input = match matched.compensating_filter {
                Some(pred) => {
                    let filter = Operator::NonASAP(NonASAPOp::Filter { pred, child: finer });
                    let Ok(group) = memo.find_or_add_group(filter.clone()) else {
                        continue;
                    };
                    add_candidate(
                        memo,
                        group,
                        filter,
                        None,
                        None,
                        None,
                        OriginOfCandidateForMemoGroup::Rollup,
                    )?;
                    group
                }
                None => finer,
            };
            let filters = vec![None; measures.len()];
            add_candidate(
                memo,
                owner,
                Operator::NonASAP(NonASAPOp::Aggregate {
                    child: input,
                    reduction: Reduction::by(matched.query_keys),
                    measures,
                    output_names: query.output_names,
                    filters,
                    having: None,
                }),
                None,
                None,
                None,
                OriginOfCandidateForMemoGroup::Rollup,
            )?;
        }
    }
    Ok(())
}

/// The parts of a SQL aggregate group rule 5 reads.
struct Aggregate {
    child: MemoGroupId,
    keys: Vec<usize>,
    measures: Vec<AggIntent>,
    output_names: Vec<String>,
}

/// `group` as an aggregate rule 5 may read or answer: a SQL relation
/// grouped `by` keys (not `without`), without measure filters or `HAVING`,
/// with no top-k or `count_values` measure.
fn aggregate(memo: &Memo, group: MemoGroupId) -> Option<Aggregate> {
    if memo.groups()[group.0 as usize].group_properties.result_kind != OperatorResultKind::Relation
    {
        return None;
    }
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction: Reduction::Reduce(by),
        measures,
        output_names,
        filters,
        having: None,
    }) = expression(memo, group)
    else {
        return None;
    };
    (!by.is_without()
        && filters.iter().all(Option::is_none)
        && !measures.iter().any(|measure| {
            matches!(
                measure,
                AggIntent::TopK { .. } | AggIntent::CountValues { .. }
            )
        }))
    .then(|| Aggregate {
        child: *child,
        keys: by.keys().to_vec(),
        measures: measures.clone(),
        output_names: output_names.clone(),
    })
}

/// The measure that merges finer values of `measure` at column `col` into
/// a coarser group: a sum's sum, a count's sum (only into keyed groups: a
/// global count of no rows is 0, a sum of none is null), a minimum's
/// minimum, a maximum's maximum. `None` for every other measure.
fn value_rollup(measure: &AggIntent, col: Option<usize>, keyed: bool) -> Option<AggIntent> {
    Some(match measure {
        AggIntent::Sum { .. } => AggIntent::Sum { col },
        AggIntent::Count { .. } if keyed => AggIntent::Sum { col },
        AggIntent::Min { .. } => AggIntent::Min { col },
        AggIntent::Max { .. } => AggIntent::Max { col },
        _ => return None,
    })
}

/// How a query aggregate is read from a finer one ([`match_finer`]).
struct FinerAggregateMatch {
    /// The query's extra conjuncts over the finer aggregate's output
    /// columns (its keys come first); `None` without extra conjuncts.
    compensating_filter: Option<Predicate<MemoGroupId>>,
    /// The query keys, as the finer aggregate's output columns.
    query_keys: Vec<usize>,
    /// Whether some finer key is neither a query key nor pinned to one
    /// value by an extra conjunct, so finer rows merge.
    needs_rollup: bool,
}

/// How a query aggregate over `query_rows` grouped by `query_keys` is
/// computed from a finer aggregate over `finer_rows` grouped by
/// `finer_keys` (keys are columns of the rows); `None` when it cannot be.
/// Both rows must be the same scan under the same time ranges ([`Rows`]);
/// the finer conjuncts must be among the query's; every other query
/// conjunct must read only finer keys and no subquery; the query keys must
/// be finer keys. Two aggregates with the same groups are one computation,
/// not one read from the other: `None`.
fn match_finer(
    memo: &Memo,
    query_rows: MemoGroupId,
    query_keys: &[usize],
    finer_rows: MemoGroupId,
    finer_keys: &[usize],
) -> Option<FinerAggregateMatch> {
    let query = Rows::of(memo, query_rows)?;
    let finer = Rows::of(memo, finer_rows)?;
    if query.spine != finer.spine {
        return None;
    }
    let mut extra = query.conjuncts;
    for conjunct in finer.conjuncts {
        let position = extra.iter().position(|other| *other == conjunct)?;
        extra.remove(position);
    }
    let finer_key = |column: usize| finer_keys.iter().position(|&key| key == column);
    let query_key_positions = query_keys
        .iter()
        .map(|&key| finer_key(key))
        .collect::<Option<Vec<_>>>()?;
    if !extra.iter().all(|conjunct| {
        conjunct.operator_refs().is_empty()
            && conjunct
                .columns_referenced()
                .into_iter()
                .all(|column| finer_key(column).is_some())
    }) {
        return None;
    }
    let pinned: Vec<usize> = extra.iter().filter_map(pinned_column).collect();
    let needs_rollup = finer_keys
        .iter()
        .any(|key| !query_keys.contains(key) && !pinned.contains(key));
    if extra.is_empty() && !needs_rollup {
        return None;
    }
    let compensating_filter = (!extra.is_empty()).then(|| {
        let mut remapped: Vec<ScalarExpr<MemoGroupId>> = extra
            .iter()
            .map(|conjunct| conjunct.map_columns(&mut |column| finer_key(column).unwrap_or(column)))
            .collect();
        Predicate(if remapped.len() == 1 {
            remapped.remove(0)
        } else {
            ScalarExpr::BoolAnd(remapped)
        })
    });
    Some(FinerAggregateMatch {
        compensating_filter,
        query_keys: query_key_positions,
        needs_rollup,
    })
}

/// An aggregate's input rows: the scan and time ranges they come from, and
/// the conjuncts that select them.
struct Rows {
    /// Each time range above the scan, outermost first, then the scan
    /// without its predicates.
    spine: Vec<Operator<()>>,
    /// Scan predicates and filter predicates, split at `AND`.
    conjuncts: Vec<ScalarExpr<MemoGroupId>>,
}

impl Rows {
    /// The rows of `group`, a chain of `TimeRange` and `Filter` groups over
    /// a `Scan` group, none changing the scan's columns; `None` for any
    /// other group.
    fn of(memo: &Memo, group: MemoGroupId) -> Option<Rows> {
        let schema = |group: MemoGroupId| &memo.groups()[group.0 as usize].group_properties.schema;
        let mut rows = Rows {
            spine: Vec::new(),
            conjuncts: Vec::new(),
        };
        let mut group = group;
        loop {
            match expression(memo, group) {
                Operator::NonASAP(NonASAPOp::TimeRange { child, .. })
                    if schema(*child) == schema(group) =>
                {
                    rows.spine
                        .push(expression(memo, group).map_children(|_| ()));
                    group = *child;
                }
                Operator::NonASAP(NonASAPOp::Filter { pred, child })
                    if schema(*child) == schema(group) =>
                {
                    split_and(&pred.0, &mut rows.conjuncts);
                    group = *child;
                }
                Operator::NonASAP(NonASAPOp::Scan {
                    source,
                    schema,
                    predicates,
                }) => {
                    for predicate in predicates {
                        split_and(&predicate.0, &mut rows.conjuncts);
                    }
                    rows.spine.push(Operator::NonASAP(NonASAPOp::Scan {
                        source: source.clone(),
                        schema: schema.clone(),
                        predicates: Vec::new(),
                    }));
                    return Some(rows);
                }
                _ => return None,
            }
        }
    }
}

/// `expr`'s conjuncts, nested `AND`s flattened, appended to `out`.
fn split_and(expr: &ScalarExpr<MemoGroupId>, out: &mut Vec<ScalarExpr<MemoGroupId>>) {
    match expr {
        ScalarExpr::BoolAnd(conjuncts) => {
            for conjunct in conjuncts {
                split_and(conjunct, out);
            }
        }
        other => out.push(other.clone()),
    }
}

/// The column an equality conjunct pins to one literal value
/// (`column = literal`, either side); `None` for any other conjunct.
fn pinned_column(conjunct: &ScalarExpr<MemoGroupId>) -> Option<usize> {
    let ScalarExpr::Compare {
        left,
        op: CompareOpKind::Eq,
        right,
        ..
    } = conjunct
    else {
        return None;
    };
    match (left.as_ref(), right.as_ref()) {
        (ScalarExpr::Column(column), ScalarExpr::Literal(_))
        | (ScalarExpr::Literal(_), ScalarExpr::Column(column)) => Some(*column),
        _ => None,
    }
}
