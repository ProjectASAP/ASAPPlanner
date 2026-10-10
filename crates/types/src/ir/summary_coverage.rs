//! What a summary state covers: `definition` (what it computes) and
//! `selection` (which output rows of that computation it took). Design:
//! `docs/design_docs/proposals/asap-primitive-schema.md` §4.2, after
//! Goldstein & Larson's view matching.
//!
//! Coverage is derived from the node, never declared. Walking down from a
//! `SummaryAgg`, a predicate conjunct moves into `selection` when it can be
//! lifted to the `SummaryAgg` (through `Filter`, a range `TimeRange`, a
//! `TimeShift` without `@` and direct-column `Project` items) and it is a
//! value set or an interval on one column. Everything else stays in
//! `definition` as a residual, so two states are merged only when they
//! compute the same thing over disjoint rows.
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::rc::Rc;

use thiserror::Error;

use super::asap::ASAPOp;
use super::node::{Operator, OperatorNode};
use super::non_asap::{NonASAPOp, TimeRangeKind};
use super::scalar::{Predicate, ScalarExpr};
use crate::pre_asap::expr_ir::{CompareOpKind, ScalarValue};
use crate::pre_asap::schema::{ColumnId, Schema};

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryCoverage {
    /// The `SummaryAgg` with the selection removed from its sub-DAG.
    pub definition: Rc<OperatorNode>,
    /// Union of boxes over the output rows of the definition's child.
    pub selection: Vec<SelectionBox>,
}

/// A conjunction of per-column constraints and an optional time window.
/// A column or time it does not mention is unrestricted.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectionBox {
    pub columns: BTreeMap<ColumnIdentity, Constraint>,
    /// Offsets from the evaluation time in milliseconds: a PromQL range
    /// `TimeRange(w)` over `TimeShift(s)` is `(-(s + w), -s]`. Absolute
    /// time will be an ordinary interval on the timestamp column once the IR
    /// has timestamp literals.
    pub relative_time: Option<(Bound<i64>, Bound<i64>)>,
}

/// A column of the definition child's output, by `(table, name)`. A
/// `Project` below renames its columns and sets their table to its
/// qualifier (none by default).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ColumnIdentity {
    pub table: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    In(Vec<ScalarValue>),
    NotIn(Vec<ScalarValue>),
    Interval {
        lower: Bound<ScalarValue>,
        upper: Bound<ScalarValue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoverageError {
    #[error("coverage is derived only for SummaryAgg and SummaryMerge")]
    NotSummary,
    #[error("summary merge requires at least one input")]
    EmptyMerge,
    #[error("summary merge inputs compute different things")]
    DefinitionMismatch,
    #[error("summary merge inputs are not proven disjoint")]
    PossibleOverlap,
}

impl SummaryCoverage {
    /// Coverage of a `SummaryAgg` or `SummaryMerge`. A merge fails unless
    /// every input has the same definition and their selections are
    /// pairwise disjoint.
    pub fn derive(node: &OperatorNode) -> Result<Self, CoverageError> {
        match node.asap() {
            Some(ASAPOp::SummaryAgg { .. }) => Ok(of_summary_agg(node)),
            Some(ASAPOp::SummaryMerge { children }) => {
                let inputs = children
                    .iter()
                    .map(|child| child.coverage().ok_or(CoverageError::NotSummary))
                    .collect::<Result<Vec<_>, _>>()?;
                let first = inputs.first().ok_or(CoverageError::EmptyMerge)?;
                let mut proven = HashSet::new();
                for (i, input) in inputs.iter().enumerate() {
                    if !same_definition(&first.definition, &input.definition, &mut proven) {
                        return Err(CoverageError::DefinitionMismatch);
                    }
                    let overlaps = inputs[..i].iter().any(|other| {
                        input
                            .selection
                            .iter()
                            .any(|a| other.selection.iter().any(|b| !a.disjoint(b)))
                    });
                    if overlaps {
                        return Err(CoverageError::PossibleOverlap);
                    }
                }
                Ok(Self {
                    definition: Rc::clone(&first.definition),
                    selection: union(inputs.iter().flat_map(|c| c.selection.clone()).collect()),
                })
            }
            _ => Err(CoverageError::NotSummary),
        }
    }
}

/// Structural equality ignoring planning metadata (`timing`, `guarantee`).
/// `proven` memoizes node pairs already found equal.
fn same_definition(
    a: &Rc<OperatorNode>,
    b: &Rc<OperatorNode>,
    proven: &mut HashSet<(*const OperatorNode, *const OperatorNode)>,
) -> bool {
    let key = (Rc::as_ptr(a), Rc::as_ptr(b));
    if Rc::ptr_eq(a, b) || proven.contains(&key) {
        return true;
    }
    let (ac, bc) = (a.children(), b.children());
    let equal = a.operator.map_children(|_| ()) == b.operator.map_children(|_| ())
        && a.result_kind == b.result_kind
        && a.schema == b.schema
        && ac.len() == bc.len()
        && ac
            .iter()
            .zip(&bc)
            .all(|(x, y)| same_definition(x, y, proven));
    if equal {
        proven.insert(key);
    }
    equal
}

fn of_summary_agg(node: &OperatorNode) -> SummaryCoverage {
    let Some(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        grouping,
        filter,
    }) = node.asap()
    else {
        unreachable!("called on a SummaryAgg");
    };

    // The chain a predicate can be lifted through, top to bottom, and the
    // first node below it.
    let mut chain: Vec<&Rc<OperatorNode>> = Vec::new();
    let mut base = child;
    while let Some(op) = base.non_asap() {
        let passes = match op {
            NonASAPOp::Filter { .. } | NonASAPOp::Project { .. } => true,
            NonASAPOp::TimeRange { kind, .. } => *kind == TimeRangeKind::Range,
            NonASAPOp::TimeShift { shift, .. } => shift.at.is_none(),
            _ => false,
        };
        if !passes {
            break;
        }
        chain.push(base);
        base = base.children()[0];
    }
    // Time is lifted only from exactly one range window.
    let lift_time = chain
        .iter()
        .filter(|link| matches!(link.non_asap(), Some(NonASAPOp::TimeRange { .. })))
        .count()
        == 1;

    let top = &child.schema;
    // Top-down: decide which conjuncts are lifted. `kept[d]` is the residual
    // of `chain[d]` when it is a `Filter`.
    let mut lifted = BTreeMap::new();
    let agg_filter = filter
        .as_ref()
        .and_then(|pred| residual(pred, top, &[], &mut lifted));
    let kept: Vec<Option<Predicate>> = chain
        .iter()
        .enumerate()
        .map(|(depth, link)| match link.non_asap() {
            Some(NonASAPOp::Filter { pred, .. }) => {
                residual(pred, top, &chain[..depth], &mut lifted)
            }
            _ => None,
        })
        .collect();
    let scan_kept = match base.non_asap() {
        Some(NonASAPOp::Scan { predicates, .. }) => Some(
            predicates
                .iter()
                .filter_map(|pred| residual(pred, top, &chain, &mut lifted))
                .collect::<Vec<_>>(),
        ),
        _ => None,
    };

    // Bottom-up: rebuild without what was lifted. A node whose input and
    // operator are unchanged keeps its `Rc`.
    let mut rebuilt = Rc::clone(base);
    if let (
        Some(kept),
        Some(NonASAPOp::Scan {
            source,
            predicates,
            schema,
        }),
    ) = (scan_kept, base.non_asap())
    {
        if kept != *predicates {
            rebuilt = rebuild(
                base,
                NonASAPOp::Scan {
                    source: source.clone(),
                    predicates: kept,
                    schema: schema.clone(),
                },
            );
        }
    }
    let (mut window_ms, mut shift_ms) = (0i64, 0i64);
    for (depth, link) in chain.iter().enumerate().rev() {
        let child = Rc::clone(&rebuilt);
        let op = match link.non_asap().expect("chain nodes are non-ASAP") {
            NonASAPOp::Filter { pred, .. } => match &kept[depth] {
                None => continue,
                Some(rest) if rest == pred && Rc::ptr_eq(&child, link.children()[0]) => {
                    rebuilt = Rc::clone(link);
                    continue;
                }
                Some(rest) => NonASAPOp::Filter {
                    pred: rest.clone(),
                    child,
                },
            },
            NonASAPOp::TimeRange { range, .. } if lift_time => {
                window_ms = range.as_millis() as i64;
                continue;
            }
            NonASAPOp::TimeShift { shift, .. } if lift_time => {
                shift_ms += shift.offset_ms;
                continue;
            }
            _ if Rc::ptr_eq(&child, link.children()[0]) => {
                rebuilt = Rc::clone(link);
                continue;
            }
            NonASAPOp::TimeRange { range, kind, .. } => NonASAPOp::TimeRange {
                range: *range,
                kind: *kind,
                child,
            },
            NonASAPOp::TimeShift { shift, .. } => NonASAPOp::TimeShift {
                shift: *shift,
                child,
            },
            NonASAPOp::Project {
                cols, qualifier, ..
            } => NonASAPOp::Project {
                cols: cols.clone(),
                qualifier: qualifier.clone(),
                child,
            },
            _ => unreachable!("only liftable operators are on the chain"),
        };
        rebuilt = rebuild(link, op);
    }
    let definition = rebuild_asap(
        node,
        ASAPOp::SummaryAgg {
            child: rebuilt,
            family: family.clone(),
            input: input.clone(),
            reduction: reduction.clone(),
            grouping: grouping.clone(),
            filter: agg_filter,
        },
    );

    let selection = SelectionBox {
        columns: lifted
            .into_iter()
            .map(|(column, constraint)| {
                let field = &top.fields[column];
                let identity = ColumnIdentity {
                    table: field.table.clone(),
                    name: field.name.clone(),
                };
                (identity, constraint)
            })
            .collect(),
        relative_time: lift_time.then_some((
            Bound::Excluded(-(shift_ms + window_ms)),
            Bound::Included(-shift_ms),
        )),
    };
    SummaryCoverage {
        definition,
        selection: vec![selection],
    }
}

/// A node with the same output schema over a new operator. The definition
/// describes what was computed; it is not re-validated as a plan.
fn rebuild(original: &OperatorNode, op: NonASAPOp) -> Rc<OperatorNode> {
    Rc::new(OperatorNode::with_schema(
        Operator::NonASAP(op),
        original.schema.clone(),
    ))
}

fn rebuild_asap(original: &OperatorNode, op: ASAPOp) -> Rc<OperatorNode> {
    Rc::new(OperatorNode::with_schema(
        Operator::ASAP(op),
        original.schema.clone(),
    ))
}

/// Lift what `pred`'s conjuncts can into `lifted` (keyed by the column of
/// `top`, the agg child's schema) and return the rest. `above` are the chain
/// nodes between the predicate and the `SummaryAgg`, top to bottom. A column
/// whose `(table, name)` is not unique in `top` cannot be named in a
/// selection, so its conjuncts stay.
fn residual(
    pred: &Predicate,
    top: &Schema,
    above: &[&Rc<OperatorNode>],
    lifted: &mut BTreeMap<ColumnId, Constraint>,
) -> Option<Predicate> {
    let mut rest: Vec<ScalarExpr> = Vec::new();
    for conjunct in pred.0.conjuncts() {
        let lift = constraint_of(conjunct).and_then(|(column, constraint)| {
            let column = column_at_top(column, above)?;
            let field = &top.fields[column];
            let namesakes = top
                .fields
                .iter()
                .filter(|f| f.name == field.name && f.table == field.table)
                .count();
            if namesakes != 1 {
                return None;
            }
            let combined = match lifted.get(&column) {
                Some(existing) => existing.intersect(&constraint)?,
                None => constraint,
            };
            Some((column, combined))
        });
        match lift {
            Some((column, constraint)) => {
                lifted.insert(column, constraint);
            }
            None => rest.push(conjunct.clone()),
        }
    }
    match rest.len() {
        0 => None,
        1 => rest.pop().map(Predicate),
        _ => Some(Predicate(ScalarExpr::BoolAnd(rest))),
    }
}

/// Map a column of the input of `above.last()` up to the agg child's output:
/// a `Project` passes it only as a direct column item.
fn column_at_top(mut column: ColumnId, above: &[&Rc<OperatorNode>]) -> Option<ColumnId> {
    for link in above.iter().rev() {
        if let Some(NonASAPOp::Project { cols, .. }) = link.non_asap() {
            column = cols
                .iter()
                .position(|item| item.expr == ScalarExpr::Column(column))?;
        }
    }
    Some(column)
}

/// `column = v`, `!=`, `<`, `<=`, `>`, `>=`, `[NOT] IN (...)` and `OR` of
/// equalities on one column, with non-null literals.
fn constraint_of(expr: &ScalarExpr) -> Option<(ColumnId, Constraint)> {
    let literal = |e: &ScalarExpr| match e {
        ScalarExpr::Literal(v) if *v != ScalarValue::Null => Some(v.clone()),
        _ => None,
    };
    match expr {
        ScalarExpr::Compare {
            left, op, right, ..
        } => {
            let (column, value, op) = match (left.as_ref(), right.as_ref()) {
                (ScalarExpr::Column(c), r) => (*c, literal(r)?, op.clone()),
                (l, ScalarExpr::Column(c)) => (*c, literal(l)?, flip(op)?),
                _ => return None,
            };
            let constraint = match op {
                CompareOpKind::Eq => Constraint::In(vec![value]),
                CompareOpKind::Ne => Constraint::NotIn(vec![value]),
                CompareOpKind::Lt => interval(Bound::Unbounded, Bound::Excluded(value)),
                CompareOpKind::Le => interval(Bound::Unbounded, Bound::Included(value)),
                CompareOpKind::Gt => interval(Bound::Excluded(value), Bound::Unbounded),
                CompareOpKind::Ge => interval(Bound::Included(value), Bound::Unbounded),
                _ => return None,
            };
            Some((column, constraint))
        }
        ScalarExpr::InList {
            expr,
            list,
            negated,
        } => {
            let ScalarExpr::Column(column) = expr.as_ref() else {
                return None;
            };
            let values = list.iter().map(literal).collect::<Option<Vec<_>>>()?;
            let values = dedup(values);
            Some((
                *column,
                if *negated {
                    Constraint::NotIn(values)
                } else {
                    Constraint::In(values)
                },
            ))
        }
        ScalarExpr::BoolOr(disjuncts) => {
            let mut column = None;
            let mut values = Vec::new();
            for disjunct in disjuncts {
                let (c, Constraint::In(v)) = constraint_of(disjunct)? else {
                    return None;
                };
                if column.replace(c).is_some_and(|prev| prev != c) {
                    return None;
                }
                values.extend(v);
            }
            Some((column?, Constraint::In(dedup(values))))
        }
        _ => None,
    }
}

fn flip(op: &CompareOpKind) -> Option<CompareOpKind> {
    Some(match op {
        CompareOpKind::Eq => CompareOpKind::Eq,
        CompareOpKind::Ne => CompareOpKind::Ne,
        CompareOpKind::Lt => CompareOpKind::Gt,
        CompareOpKind::Le => CompareOpKind::Ge,
        CompareOpKind::Gt => CompareOpKind::Lt,
        CompareOpKind::Ge => CompareOpKind::Le,
        _ => return None,
    })
}

fn interval(lower: Bound<ScalarValue>, upper: Bound<ScalarValue>) -> Constraint {
    Constraint::Interval { lower, upper }
}

fn dedup(values: Vec<ScalarValue>) -> Vec<ScalarValue> {
    let mut out: Vec<ScalarValue> = Vec::new();
    for v in values {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Whether `v` equals one of `values`; `None` when some pair cannot be
/// compared (different types, NaN), since `1` and `1.0` may select the same
/// rows.
fn among(v: &ScalarValue, values: &[ScalarValue]) -> Option<bool> {
    for w in values {
        if compare(v, w)? == Ordering::Equal {
            return Some(true);
        }
    }
    Some(false)
}

/// The values of `a` that are (`keep = true`) or are not in `b`.
fn filter_values(a: &[ScalarValue], b: &[ScalarValue], keep: bool) -> Option<Vec<ScalarValue>> {
    let mut out = Vec::new();
    for v in a {
        if among(v, b)? == keep {
            out.push(v.clone());
        }
    }
    Some(out)
}

/// Order of two literals of the same type; `None` across types or for NaN.
fn compare(a: &ScalarValue, b: &ScalarValue) -> Option<Ordering> {
    match (a, b) {
        (ScalarValue::Int64(a), ScalarValue::Int64(b)) => Some(a.cmp(b)),
        (ScalarValue::Float64(a), ScalarValue::Float64(b)) => a.partial_cmp(b),
        (ScalarValue::Utf8(a), ScalarValue::Utf8(b)) => Some(a.cmp(b)),
        (ScalarValue::Boolean(a), ScalarValue::Boolean(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Whether `v` lies inside the interval; `None` when it cannot be compared.
fn inside<T>(
    v: &T,
    (lower, upper): (&Bound<T>, &Bound<T>),
    cmp: impl Fn(&T, &T) -> Option<Ordering>,
) -> Option<bool> {
    let above = match lower {
        Bound::Unbounded => true,
        Bound::Included(l) => cmp(v, l)? != Ordering::Less,
        Bound::Excluded(l) => cmp(v, l)? == Ordering::Greater,
    };
    let below = match upper {
        Bound::Unbounded => true,
        Bound::Included(u) => cmp(v, u)? != Ordering::Greater,
        Bound::Excluded(u) => cmp(v, u)? == Ordering::Less,
    };
    Some(above && below)
}

/// Whether no value is both at most `upper` and at least `lower`.
fn ends_before<T>(
    upper: &Bound<T>,
    lower: &Bound<T>,
    cmp: impl Fn(&T, &T) -> Option<Ordering>,
) -> bool {
    match (upper, lower) {
        (Bound::Included(u), Bound::Included(l)) => cmp(u, l) == Some(Ordering::Less),
        (Bound::Included(u) | Bound::Excluded(u), Bound::Included(l) | Bound::Excluded(l)) => {
            matches!(cmp(u, l), Some(Ordering::Less | Ordering::Equal))
        }
        _ => false,
    }
}

fn intervals_disjoint<T>(
    (al, au): (&Bound<T>, &Bound<T>),
    (bl, bu): (&Bound<T>, &Bound<T>),
    cmp: impl Fn(&T, &T) -> Option<Ordering> + Copy,
) -> bool {
    ends_before(au, bl, cmp) || ends_before(bu, al, cmp)
}

/// The tighter of two lower (`want = Greater`) or upper (`Less`) bounds.
fn tighter(
    a: &Bound<ScalarValue>,
    b: &Bound<ScalarValue>,
    want: Ordering,
) -> Option<Bound<ScalarValue>> {
    let value = |bound: &Bound<ScalarValue>| match bound {
        Bound::Included(v) | Bound::Excluded(v) => Some(v.clone()),
        Bound::Unbounded => None,
    };
    Some(match (value(a), value(b)) {
        (None, _) => b.clone(),
        (_, None) => a.clone(),
        (Some(x), Some(y)) => match compare(&x, &y)? {
            Ordering::Equal if matches!(a, Bound::Excluded(_)) => a.clone(),
            Ordering::Equal => b.clone(),
            order if order == want => a.clone(),
            _ => b.clone(),
        },
    })
}

impl Constraint {
    /// The conjunction of two constraints on one column, when it is again a
    /// single constraint.
    fn intersect(&self, other: &Self) -> Option<Self> {
        use Constraint::*;
        Some(match (self, other) {
            (In(a), In(b)) => In(filter_values(a, b, true)?),
            (In(a), NotIn(b)) | (NotIn(b), In(a)) => In(filter_values(a, b, false)?),
            (NotIn(a), NotIn(b)) => NotIn(dedup(a.iter().chain(b).cloned().collect())),
            (In(a), Interval { lower, upper }) | (Interval { lower, upper }, In(a)) => {
                let mut kept = Vec::new();
                for v in a {
                    if inside(v, (lower, upper), compare)? {
                        kept.push(v.clone());
                    }
                }
                In(kept)
            }
            (
                Interval {
                    lower: al,
                    upper: au,
                },
                Interval {
                    lower: bl,
                    upper: bu,
                },
            ) => Interval {
                lower: tighter(al, bl, Ordering::Greater)?,
                upper: tighter(au, bu, Ordering::Less)?,
            },
            (NotIn(_), Interval { .. }) | (Interval { .. }, NotIn(_)) => return None,
        })
    }

    /// Whether no value satisfies both; `false` when unsure.
    fn disjoint(&self, other: &Self) -> bool {
        use Constraint::*;
        match (self, other) {
            (In(a), In(b)) => a.iter().all(|v| among(v, b) == Some(false)),
            (In(a), NotIn(b)) | (NotIn(b), In(a)) => a.iter().all(|v| among(v, b) == Some(true)),
            (In(a), Interval { lower, upper }) | (Interval { lower, upper }, In(a)) => a
                .iter()
                .all(|v| inside(v, (lower, upper), compare) == Some(false)),
            (
                Interval {
                    lower: al,
                    upper: au,
                },
                Interval {
                    lower: bl,
                    upper: bu,
                },
            ) => intervals_disjoint((al, au), (bl, bu), compare),
            (NotIn(_), _) | (_, NotIn(_)) => false,
        }
    }
}

impl SelectionBox {
    /// Whether no row lies in both boxes: some column, or the time window,
    /// is restricted by both and the restrictions are disjoint.
    fn disjoint(&self, other: &Self) -> bool {
        let time = match (&self.relative_time, &other.relative_time) {
            (Some((al, au)), Some((bl, bu))) => {
                intervals_disjoint((al, au), (bl, bu), |a: &i64, b: &i64| Some(a.cmp(b)))
            }
            _ => false,
        };
        time || self
            .columns
            .iter()
            .any(|(column, a)| other.columns.get(column).is_some_and(|b| a.disjoint(b)))
    }
}

/// Join boxes that differ only in one dimension whose union is again one
/// constraint: touching time windows or value ranges, or the values of one
/// `In` column. Gaps stay separate boxes.
fn union(mut boxes: Vec<SelectionBox>) -> Vec<SelectionBox> {
    let mut i = 0;
    while i < boxes.len() {
        let joined = (i + 1..boxes.len()).find_map(|j| join(&boxes[i], &boxes[j]).map(|b| (j, b)));
        match joined {
            Some((j, joined)) => {
                boxes.remove(j);
                boxes[i] = joined;
                i = 0;
            }
            None => i += 1,
        }
    }
    boxes
}

fn join(a: &SelectionBox, b: &SelectionBox) -> Option<SelectionBox> {
    if a.columns == b.columns {
        let ((al, au), (bl, bu)) = (a.relative_time.as_ref()?, b.relative_time.as_ref()?);
        let time = touching((al, au), (bl, bu), |x: &i64, y: &i64| Some(x.cmp(y)))?;
        return Some(SelectionBox {
            columns: a.columns.clone(),
            relative_time: Some(time),
        });
    }
    if a.relative_time != b.relative_time || a.columns.len() != b.columns.len() {
        return None;
    }
    let mut differing = a
        .columns
        .iter()
        .filter(|(column, constraint)| b.columns.get(*column) != Some(*constraint));
    let (column, constraint) = differing.next()?;
    if differing.next().is_some() {
        return None;
    }
    let joined = match (constraint, b.columns.get(column)?) {
        (Constraint::In(values), Constraint::In(more)) => {
            Constraint::In(dedup(values.iter().chain(more).cloned().collect()))
        }
        (
            Constraint::Interval {
                lower: al,
                upper: au,
            },
            Constraint::Interval {
                lower: bl,
                upper: bu,
            },
        ) => {
            let (lower, upper) = touching((al, au), (bl, bu), compare)?;
            Constraint::Interval { lower, upper }
        }
        _ => return None,
    };
    let mut columns = a.columns.clone();
    columns.insert(column.clone(), joined);
    Some(SelectionBox {
        columns,
        relative_time: a.relative_time,
    })
}

/// The union of two intervals when one ends exactly where the other starts,
/// with the shared end point in exactly one of them.
fn touching<T: Clone>(
    (al, au): (&Bound<T>, &Bound<T>),
    (bl, bu): (&Bound<T>, &Bound<T>),
    cmp: impl Fn(&T, &T) -> Option<Ordering>,
) -> Option<(Bound<T>, Bound<T>)> {
    let meets = |upper: &Bound<T>, lower: &Bound<T>| match (upper, lower) {
        (Bound::Included(u), Bound::Excluded(l)) | (Bound::Excluded(u), Bound::Included(l)) => {
            cmp(u, l) == Some(Ordering::Equal)
        }
        _ => false,
    };
    if meets(au, bl) {
        Some((al.clone(), bu.clone()))
    } else if meets(bu, al) {
        Some((bl.clone(), au.clone()))
    } else {
        None
    }
}
