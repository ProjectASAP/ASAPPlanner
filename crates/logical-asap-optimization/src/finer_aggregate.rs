//! Whether a query aggregate can be computed from a finer aggregate over the
//! same source and window, and with what on top (view matching for
//! aggregation queries, as in *Optimizing Queries Using Materialized Views:
//! A Practical, Scalable Solution*, SIGMOD 2001, §3.3, and Apache Calcite's
//! `SubstitutionVisitor.unifyAggregates`).
//!
//! The finer result holds at least the rows the query needs. A
//! *compensating* operation closes the gap: a compensating filter removes
//! the extra rows, and a rollup merges finer groups into the query's. A
//! filter that reads only finer group-by columns is constant within a finer
//! group, so filtering finer groups equals filtering input rows.
use asap_types::ir::scalar::CompareOpKind;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr};

/// How a query aggregate is computed from a finer one: keep the finer rows
/// that pass `compensating_filter`, then, when `needs_rollup`, merge them
/// into the query's groups.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FinerAggregateMatch {
    /// The query's conjuncts that the finer aggregate lacks, over the finer
    /// group-by (column `i` is the finer aggregate's `i`-th key); `None`
    /// when the finer aggregate has every conjunct.
    pub compensating_filter: Option<Predicate>,
    /// Each query group key, as an index into the finer group-by.
    pub query_keys: Vec<usize>,
    /// Whether finer groups merge into query groups: a finer key the query
    /// drops is not pinned by an equality in `compensating_filter`. When
    /// `false`, each query group is at most one finer row.
    pub needs_rollup: bool,
}

/// How a query aggregate over `query_rows` grouped by `query_keys` is
/// computed from a finer aggregate over `finer_rows` grouped by
/// `finer_keys` (keys are columns of the rows); `None` when it cannot be.
///
/// Both rows must be the same scan (source and schema) under the same time
/// ranges; the finer conjuncts (scan predicates and filters) must be among
/// the query's; every other query conjunct must read only finer group-by
/// columns; and the query keys must be finer keys.
pub(crate) fn match_finer(
    query_rows: &OperatorNode,
    query_keys: &[usize],
    finer_rows: &OperatorNode,
    finer_keys: &[usize],
) -> Option<FinerAggregateMatch> {
    let query = Rows::of(query_rows)?;
    let finer = Rows::of(finer_rows)?;
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
    let identical = extra.is_empty()
        && !needs_rollup
        && query_key_positions.iter().copied().eq(0..finer_keys.len());
    if identical {
        return None;
    }
    let compensating_filter = (!extra.is_empty()).then(|| {
        let remapped: Vec<ScalarExpr> = extra
            .iter()
            .map(|conjunct| conjunct.map_columns(&mut |column| finer_key(column).unwrap()))
            .collect();
        Predicate(match <[ScalarExpr; 1]>::try_from(remapped) {
            Ok([one]) => one,
            Err(many) => ScalarExpr::BoolAnd(many),
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
    conjuncts: Vec<ScalarExpr>,
}

impl Rows {
    /// The rows of `node`, a chain of `TimeRange`s and `Filter`s over a
    /// `Scan`, none changing the scan's columns; `None` for anything else.
    fn of(node: &OperatorNode) -> Option<Rows> {
        let mut rows = Rows {
            spine: Vec::new(),
            conjuncts: Vec::new(),
        };
        let mut node = node;
        loop {
            match node.non_asap()? {
                NonASAPOp::TimeRange { child, .. } if child.schema == node.schema => {
                    rows.spine.push(node.operator.map_children(|_| ()));
                    node = child;
                }
                NonASAPOp::Filter { pred, child } if child.schema == node.schema => {
                    split_and(&pred.0, &mut rows.conjuncts);
                    node = child;
                }
                NonASAPOp::Scan {
                    source,
                    schema,
                    predicates,
                } => {
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

fn split_and(expr: &ScalarExpr, out: &mut Vec<ScalarExpr>) {
    match expr {
        ScalarExpr::BoolAnd(items) => items.iter().for_each(|item| split_and(item, out)),
        other => out.push(other.clone()),
    }
}

/// The columns `pred` pins to one literal value each: its conjuncts
/// `c = v` and `c IN (v)`.
pub(crate) fn pinned_columns(pred: &ScalarExpr) -> Vec<usize> {
    let mut conjuncts = Vec::new();
    split_and(pred, &mut conjuncts);
    conjuncts.iter().filter_map(pinned_column).collect()
}

/// The column `conjunct` pins to one literal value (`c = v`, `c IN (v)`).
fn pinned_column(conjunct: &ScalarExpr) -> Option<usize> {
    match conjunct {
        ScalarExpr::Compare {
            left,
            op: CompareOpKind::Eq,
            right,
            ..
        } => match (&**left, &**right) {
            (ScalarExpr::Column(column), ScalarExpr::Literal(_))
            | (ScalarExpr::Literal(_), ScalarExpr::Column(column)) => Some(*column),
            _ => None,
        },
        ScalarExpr::InList {
            expr,
            list,
            negated: false,
        } => match (&**expr, list.as_slice()) {
            (ScalarExpr::Column(column), [ScalarExpr::Literal(_)]) => Some(*column),
            _ => None,
        },
        _ => None,
    }
}
