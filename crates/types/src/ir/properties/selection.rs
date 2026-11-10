//! The selection algebra of aggregate coverage (a memo group's coverage
//! `selection`, in `crate::stages`): which output rows of a computation a
//! state took, as a union of boxes ([`SelectionBox`]), each a
//! [`Constraint`] per column ([`ColumnIdentity`]): value sets and intervals,
//! with interval limits relative to the evaluation time
//! ([`IntervalConstraintLimit`]). A predicate conjunct becomes a constraint
//! through `constraint_of`.
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;

use serde::{Deserialize, Serialize};

use crate::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use crate::ir::scalar::ScalarExpr;
use crate::ir::schema::ColumnId;

/// A conjunction of per-column constraints. The timestamp column is one of
/// the columns; there is no separate time field. A column it does not
/// mention is unrestricted. JSON: `columns` is a list of
/// `{column, constraint}` (its keys are not strings), and a bound is
/// `{"included": v}`, `{"excluded": v}` or `"unbounded"`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionBox {
    #[serde(with = "columns_as_list")]
    pub(crate) columns: BTreeMap<ColumnIdentity, Constraint>,
}

/// `SelectionBox::columns` as a list of `{column, constraint}`.
mod columns_as_list {
    use super::{ColumnIdentity, Constraint};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        column: ColumnIdentity,
        constraint: Constraint,
    }

    pub(super) fn serialize<S: Serializer>(
        columns: &BTreeMap<ColumnIdentity, Constraint>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let entries: Vec<Entry> = columns
            .iter()
            .map(|(column, constraint)| Entry {
                column: column.clone(),
                constraint: constraint.clone(),
            })
            .collect();
        entries.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<ColumnIdentity, Constraint>, D::Error> {
        let entries = Vec::<Entry>::deserialize(deserializer)?;
        Ok(entries
            .into_iter()
            .map(|entry| (entry.column, entry.constraint))
            .collect())
    }
}

/// A `Bound` in JSON: `{"included": v}`, `{"excluded": v}` or `"unbounded"`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireBound<T> {
    Included(T),
    Excluded(T),
    Unbounded,
}

impl<T: Clone> From<&Bound<T>> for WireBound<T> {
    fn from(bound: &Bound<T>) -> Self {
        match bound {
            Bound::Included(v) => Self::Included(v.clone()),
            Bound::Excluded(v) => Self::Excluded(v.clone()),
            Bound::Unbounded => Self::Unbounded,
        }
    }
}

impl<T> From<WireBound<T>> for Bound<T> {
    fn from(bound: WireBound<T>) -> Self {
        match bound {
            WireBound::Included(v) => Bound::Included(v),
            WireBound::Excluded(v) => Bound::Excluded(v),
            WireBound::Unbounded => Bound::Unbounded,
        }
    }
}

/// A column of the definition child's output, by `(table, name)`. A
/// `Project` below renames its columns and sets their table to its
/// qualifier (none by default).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnIdentity {
    pub(crate) table: Option<String>,
    pub(crate) name: String,
}

#[cfg(any(test, feature = "test-support"))]
impl ColumnIdentity {
    pub fn new(table: Option<String>, name: impl Into<String>) -> Self {
        Self {
            table,
            name: name.into(),
        }
    }
}

/// A constraint on one column of a selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    /// The value is one of these.
    In(Vec<ScalarValue>),
    /// The value is none of these.
    NotIn(Vec<ScalarValue>),
    /// The value lies between `lower` and `upper`; each end is `Included`,
    /// `Excluded` or `Unbounded`. A PromQL range `m[r]` evaluated at t reads
    /// the samples in (t − r, t]: left-open, right-closed. Examples, on the
    /// timestamp column `ts` (ms) unless stated:
    ///
    /// | Query fragment | `lower` | `upper` |
    /// |---|---|---|
    /// | PromQL `m[5m]` | `Excluded(EvaluationTimePlusMs(-300_000))` | `Included(EvaluationTimePlusMs(0))` |
    /// | PromQL `m[5m] offset 1h` | `Excluded(EvaluationTimePlusMs(-3_900_000))` | `Included(EvaluationTimePlusMs(-3_600_000))` |
    /// | PromQL `m[5m] @ 1700000000` | `Excluded(FixedValue(Timestamp(1_699_999_700_000)))` | `Included(FixedValue(Timestamp(1_700_000_000_000)))` |
    /// | SQL `WHERE ts >= '2026-11-01'` | `Included(FixedValue(Timestamp(..)))` | `Unbounded` |
    /// | SQL `WHERE price > 10` (column `price`) | `Excluded(FixedValue(Float64(10.0)))` | `Unbounded` |
    ///
    /// A full-range interval `(Unbounded, Unbounded)` is still a
    /// constraint, not the absence of one: it keeps every ordered value and
    /// excludes the rows whose value is NULL or NaN. A column with no
    /// constraint in a box keeps every row.
    Interval {
        #[serde(with = "limit_bound")]
        lower: Bound<IntervalConstraintLimit>,
        #[serde(with = "limit_bound")]
        upper: Bound<IntervalConstraintLimit>,
    },
}

/// An end of a [`Constraint::Interval`]. Two limits compare only when they
/// are of the same variant: a fixed value against a fixed value of the same
/// type, an offset from the evaluation time against another such offset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalConstraintLimit {
    /// A constant: `Float64(10.0)` for SQL `price > 10`;
    /// `Timestamp(1_700_000_000_000)` for PromQL `@ 1700000000` or a SQL
    /// timestamp literal.
    FixedValue(ScalarValue),
    /// The evaluation time t of a repeatedly evaluated query plus this many
    /// ms (negative: before t); only on the timestamp column. `m[5m]` is
    /// `(EvaluationTimePlusMs(-300_000), EvaluationTimePlusMs(0)]`.
    EvaluationTimePlusMs(i64),
}

/// A `Constraint::Interval` bound as a [`WireBound`].
mod limit_bound {
    use super::{IntervalConstraintLimit, WireBound};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::ops::Bound;

    pub(super) fn serialize<S: Serializer>(
        bound: &Bound<IntervalConstraintLimit>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        WireBound::from(bound).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Bound<IntervalConstraintLimit>, D::Error> {
        Ok(WireBound::deserialize(deserializer)?.into())
    }
}

/// `column = v`, `!=`, `<`, `<=`, `>`, `>=`, `[NOT] IN (...)` and `OR` of
/// equalities on one column, with non-null literals.
pub(crate) fn constraint_of<C>(expr: &ScalarExpr<C>) -> Option<(ColumnId, Constraint)> {
    let literal = |e: &ScalarExpr<C>| match e {
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

/// An interval between two fixed values.
fn interval(lower: Bound<ScalarValue>, upper: Bound<ScalarValue>) -> Constraint {
    Constraint::Interval {
        lower: lower.map(IntervalConstraintLimit::FixedValue),
        upper: upper.map(IntervalConstraintLimit::FixedValue),
    }
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
        (ScalarValue::Timestamp(a), ScalarValue::Timestamp(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Order of two interval limits; `None` across variants or when the fixed
/// values do not compare.
fn compare_limits(a: &IntervalConstraintLimit, b: &IntervalConstraintLimit) -> Option<Ordering> {
    use IntervalConstraintLimit::*;
    match (a, b) {
        (FixedValue(a), FixedValue(b)) => compare(a, b),
        (EvaluationTimePlusMs(a), EvaluationTimePlusMs(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Whether value `v` lies inside the interval; `None` when unsure.
fn value_inside(
    v: &ScalarValue,
    bounds: (
        &Bound<IntervalConstraintLimit>,
        &Bound<IntervalConstraintLimit>,
    ),
) -> Option<bool> {
    inside(
        &IntervalConstraintLimit::FixedValue(v.clone()),
        bounds,
        compare_limits,
    )
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
    a: &Bound<IntervalConstraintLimit>,
    b: &Bound<IntervalConstraintLimit>,
    want: Ordering,
) -> Option<Bound<IntervalConstraintLimit>> {
    let value = |bound: &Bound<IntervalConstraintLimit>| match bound {
        Bound::Included(v) | Bound::Excluded(v) => Some(v.clone()),
        Bound::Unbounded => None,
    };
    Some(match (value(a), value(b)) {
        (None, _) => b.clone(),
        (_, None) => a.clone(),
        (Some(x), Some(y)) => match compare_limits(&x, &y)? {
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
    pub(crate) fn intersect(&self, other: &Self) -> Option<Self> {
        use Constraint::*;
        Some(match (self, other) {
            (In(a), In(b)) => In(filter_values(a, b, true)?),
            (In(a), NotIn(b)) | (NotIn(b), In(a)) => In(filter_values(a, b, false)?),
            (NotIn(a), NotIn(b)) => NotIn(dedup(a.iter().chain(b).cloned().collect())),
            (In(a), Interval { lower, upper }) | (Interval { lower, upper }, In(a)) => {
                let mut kept = Vec::new();
                for v in a {
                    if value_inside(v, (lower, upper))? {
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
    pub(crate) fn disjoint(&self, other: &Self) -> bool {
        use Constraint::*;
        match (self, other) {
            (In(a), In(b)) => a.iter().all(|v| among(v, b) == Some(false)),
            (In(a), NotIn(b)) | (NotIn(b), In(a)) => a.iter().all(|v| among(v, b) == Some(true)),
            (In(a), Interval { lower, upper }) | (Interval { lower, upper }, In(a)) => a
                .iter()
                .all(|v| value_inside(v, (lower, upper)) == Some(false)),
            (
                Interval {
                    lower: al,
                    upper: au,
                },
                Interval {
                    lower: bl,
                    upper: bu,
                },
            ) => intervals_disjoint((al, au), (bl, bu), compare_limits),
            (NotIn(_), _) | (_, NotIn(_)) => false,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SelectionBox {
    pub fn new(columns: BTreeMap<ColumnIdentity, Constraint>) -> Self {
        Self { columns }
    }

    /// The per-column constraints.
    pub fn columns(&self) -> &BTreeMap<ColumnIdentity, Constraint> {
        &self.columns
    }
}

impl SelectionBox {
    /// Whether no row lies in both boxes: some column is restricted by both
    /// and the restrictions are disjoint.
    pub(crate) fn disjoint(&self, other: &Self) -> bool {
        self.columns
            .iter()
            .any(|(column, a)| other.columns.get(column).is_some_and(|b| a.disjoint(b)))
    }
}

/// Join boxes that differ only in one column whose union is again one
/// constraint: touching value ranges, or the values of one `In` column.
/// Gaps stay separate boxes.
pub(crate) fn union(mut boxes: Vec<SelectionBox>) -> Vec<SelectionBox> {
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
    if a == b {
        return Some(a.clone());
    }
    if a.columns.len() != b.columns.len() {
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
            let (lower, upper) = touching((al, au), (bl, bu), compare_limits)?;
            Constraint::Interval { lower, upper }
        }
        _ => return None,
    };
    let mut columns = a.columns.clone();
    columns.insert(column.clone(), joined);
    Some(SelectionBox { columns })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::scalar::ExprSemantics;

    use IntervalConstraintLimit::{EvaluationTimePlusMs, FixedValue};

    fn relative(lower_ms: i64, upper_ms: i64) -> Constraint {
        Constraint::Interval {
            lower: Bound::Excluded(EvaluationTimePlusMs(lower_ms)),
            upper: Bound::Included(EvaluationTimePlusMs(upper_ms)),
        }
    }

    /// `m[5m]` and `m[5m] offset 5m` are disjoint and touch; neither
    /// compares with an `@ T` interval.
    #[test]
    fn limits_compare_only_within_one_variant() {
        let last_5m = relative(-300_000, 0);
        let offset_5m = relative(-600_000, -300_000);
        assert!(last_5m.disjoint(&offset_5m));
        let at = Constraint::Interval {
            lower: Bound::Excluded(FixedValue(ScalarValue::Timestamp(1_699_999_700_000))),
            upper: Bound::Included(FixedValue(ScalarValue::Timestamp(1_700_000_000_000))),
        };
        assert!(!last_5m.disjoint(&at));
        assert_eq!(last_5m.intersect(&at), None);
        let column = ColumnIdentity::new(None, "ts");
        let one = |constraint: Constraint| SelectionBox {
            columns: BTreeMap::from([(column.clone(), constraint)]),
        };
        assert_eq!(
            union(vec![one(last_5m), one(offset_5m)]),
            vec![one(relative(-600_000, 0))]
        );
    }

    #[test]
    fn an_interval_round_trips_through_json() {
        let constraint = relative(-3_900_000, -3_600_000);
        let json = serde_json::to_value(&constraint).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"interval": {
                "lower": {"excluded": {"evaluation_time_plus_ms": -3_900_000}},
                "upper": {"included": {"evaluation_time_plus_ms": -3_600_000}},
            }})
        );
        assert_eq!(
            serde_json::from_value::<Constraint>(json).unwrap(),
            constraint
        );
    }

    /// SQL `price > 10` lifts to `(FixedValue(10), ∞)`.
    #[test]
    fn a_value_comparison_is_an_interval_between_fixed_values() {
        let price_gt_10: ScalarExpr<()> = ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(0)),
            op: CompareOpKind::Gt,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(10.0))),
            semantics: ExprSemantics::Sql,
        };
        assert_eq!(
            constraint_of(&price_gt_10),
            Some((
                0,
                Constraint::Interval {
                    lower: Bound::Excluded(FixedValue(ScalarValue::Float64(10.0))),
                    upper: Bound::Unbounded,
                }
            ))
        );
    }
}
