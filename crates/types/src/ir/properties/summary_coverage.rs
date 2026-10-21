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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::{NonASAPOp, TimeRangeKind};
use crate::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use crate::ir::scalar::{Predicate, ScalarExpr};
use crate::ir::schema::family_capability::{FamilyCapability, MergeInputRule};
use crate::ir::schema::{ColumnId, Schema};

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryCoverage {
    /// The `SummaryAgg` with the selection removed from its sub-DAG.
    pub definition: Rc<OperatorNode>,
    /// Union of boxes over the output rows of the definition's child.
    pub selection: Vec<SelectionBox>,
}

/// A conjunction of per-column constraints and an optional time window.
/// A column or time it does not mention is unrestricted. JSON: `columns` is
/// a list of `{column, constraint}` (its keys are not strings), and a bound
/// is `{"included": v}`, `{"excluded": v}` or `"unbounded"`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionBox {
    #[serde(with = "columns_as_list")]
    pub columns: BTreeMap<ColumnIdentity, Constraint>,
    /// Offsets from the evaluation time in milliseconds: a PromQL range
    /// `TimeRange(w)` over `TimeShift(s)` is `(-(s + w), -s]`. Absolute
    /// time will be an ordinary interval on the timestamp column once the IR
    /// has timestamp literals.
    #[serde(with = "relative_time_bounds")]
    pub relative_time: Option<(Bound<i64>, Bound<i64>)>,
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

/// `SelectionBox::relative_time` as `[lower, upper]` wire bounds.
mod relative_time_bounds {
    use super::WireBound;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::ops::Bound;

    /// A relative time window, `(lower, upper)`.
    type Window = (Bound<i64>, Bound<i64>);

    pub(super) fn serialize<S: Serializer>(
        window: &Option<Window>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        window
            .as_ref()
            .map(|(lower, upper)| (WireBound::from(lower), WireBound::from(upper)))
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Window>, D::Error> {
        let window = Option::<(WireBound<i64>, WireBound<i64>)>::deserialize(deserializer)?;
        Ok(window.map(|(lower, upper)| (lower.into(), upper.into())))
    }
}

/// A column of the definition child's output, by `(table, name)`. A
/// `Project` below renames its columns and sets their table to its
/// qualifier (none by default).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnIdentity {
    pub table: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    In(Vec<ScalarValue>),
    NotIn(Vec<ScalarValue>),
    Interval {
        #[serde(with = "value_bound")]
        lower: Bound<ScalarValue>,
        #[serde(with = "value_bound")]
        upper: Bound<ScalarValue>,
    },
}

/// A `Constraint::Interval` bound as a [`WireBound`].
mod value_bound {
    use super::WireBound;
    use crate::ir::scalar::expr_ir::ScalarValue;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::ops::Bound;

    pub(super) fn serialize<S: Serializer>(
        bound: &Bound<ScalarValue>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        WireBound::from(bound).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Bound<ScalarValue>, D::Error> {
        Ok(WireBound::deserialize(deserializer)?.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoverageError {
    /// The node is not a summary state (`SummaryAgg`, `PaneChain` or
    /// `SummaryMerge`): a caller bug, or bad input when it is a merge's
    /// input.
    #[error("coverage is derived only for SummaryAgg, PaneChain and SummaryMerge")]
    NotSummary,
    /// A `PaneChain` that does not expand to one build over its whole
    /// window (its window overflows, or the build does not derive): bad
    /// input.
    #[error("pane chain does not expand to one build over its whole window: {reason}")]
    PaneChainExpansion { reason: String },
    /// A `SummaryMerge` without inputs: bad input.
    #[error("summary merge requires at least one input")]
    EmptyMerge,
    /// The inputs of a `SummaryMerge` compute different things: bad input.
    #[error("summary merge inputs compute different things")]
    DefinitionMismatch,
    /// The inputs of a `SummaryMerge` are not proven to relate as the
    /// family's merge requires (disjoint, or disjoint in time for counter
    /// states): bad input.
    #[error(
        "summary merge inputs are not proven {}, which the family's merge requires",
        match required {
            MergeInputRule::DisjointInTime => "disjoint in time",
            _ => "disjoint",
        }
    )]
    InputsOverlap { required: MergeInputRule },
}

/// Coverage already derived during one validation, by node address. The
/// nodes must stay alive while it is in use.
pub(crate) type CoverageMemo = HashMap<*const OperatorNode, SummaryCoverage>;

impl SummaryCoverage {
    /// Coverage of a `SummaryAgg`, `PaneChain` or `SummaryMerge`. A chain
    /// covers what one build over its whole window does, and a merge of its
    /// newest panes what one build over their window does. A merge fails unless
    /// every input has the same definition and their selections are
    /// pairwise disjoint; a family whose merge is idempotent
    /// ([`MergeInputRule::MayOverlap`] in the family-capability
    /// table) may also merge overlapping selections, and a counter family
    /// ([`MergeInputRule::DisjointInTime`]) needs disjoint time windows.
    pub fn derive(node: &OperatorNode) -> Result<Self, CoverageError> {
        Self::derive_memoized(node, &mut CoverageMemo::new())
    }

    /// A stable identity of `definition`: the 128-bit FNV-1a hash, in hex,
    /// of its sub-DAG's operators, output kinds and schemas, children
    /// first, without planning metadata (`timing`, `guarantee`,
    /// `materialization`). Two equal definitions (the equality a merge
    /// requires) get equal ids on any node and in any process, so a summary
    /// store can key states by this id and a concrete selection.
    pub fn definition_id(&self) -> String {
        format!(
            "{:032x}",
            definition_hash(&self.definition, &mut HashMap::new())
        )
    }

    /// [`derive`](Self::derive), reusing and recording the coverage of
    /// every summary state in `memo`, so validating a DAG derives each
    /// state once.
    pub(crate) fn derive_memoized(
        node: &OperatorNode,
        memo: &mut CoverageMemo,
    ) -> Result<Self, CoverageError> {
        let key = node as *const OperatorNode;
        if let Some(coverage) = memo.get(&key) {
            return Ok(coverage.clone());
        }
        let coverage = match node.asap() {
            Some(ASAPOp::SummaryAgg { .. }) => of_summary_agg(node),
            // Back-to-back panes cover the whole window one build over it
            // would.
            Some(ASAPOp::PaneChain { .. }) => of_summary_agg(
                &crate::ir::operator::pane_chain::whole_window_build(node).map_err(|error| {
                    CoverageError::PaneChainExpansion {
                        reason: error.to_string(),
                    }
                })?,
            ),
            // The newest panes of one chain cover what one build over their
            // window does.
            Some(ASAPOp::SummaryMerge {
                newest_panes: Some(_),
                ..
            }) => of_summary_agg(
                &crate::ir::operator::pane_chain::merged_window_build(node).map_err(|error| {
                    CoverageError::PaneChainExpansion {
                        reason: error.to_string(),
                    }
                })?,
            ),
            Some(ASAPOp::SummaryMerge { children, .. }) => {
                let inputs = children
                    .iter()
                    .map(|child| Self::derive_memoized(child, memo))
                    .collect::<Result<Vec<_>, _>>()?;
                of_merge(&inputs)?
            }
            _ => return Err(CoverageError::NotSummary),
        };
        memo.insert(key, coverage.clone());
        Ok(coverage)
    }
}

/// The coverage of a merge of states covering `inputs`.
fn of_merge(inputs: &[SummaryCoverage]) -> Result<SummaryCoverage, CoverageError> {
    let first = inputs.first().ok_or(CoverageError::EmptyMerge)?;
    let mut proven = HashSet::new();
    if !inputs
        .iter()
        .all(|input| same_definition(&first.definition, &input.definition, &mut proven))
    {
        return Err(CoverageError::DefinitionMismatch);
    }
    // A family without a merge is checked as the strictest rule; the
    // executor rejects the merge itself.
    let required = match first.definition.asap() {
        Some(ASAPOp::SummaryAgg { family, .. }) => FamilyCapability::of(family).merge_inputs,
        _ => None,
    }
    .unwrap_or(MergeInputRule::Disjoint);
    let separated: fn(&SelectionBox, &SelectionBox) -> bool = match required {
        MergeInputRule::MayOverlap => |_, _| true,
        MergeInputRule::DisjointInTime => SelectionBox::disjoint_in_time,
        MergeInputRule::Disjoint => SelectionBox::disjoint,
    };
    // Windows in time order, each ending before the next starts, are
    // pairwise disjoint in time, and so pairwise disjoint: O(b log b) for
    // b boxes. Otherwise, every pair of boxes of different inputs.
    if required != MergeInputRule::MayOverlap
        && !windows_ordered_apart(inputs)
        && inputs.iter().enumerate().any(|(i, input)| {
            inputs[..i].iter().any(|other| {
                input
                    .selection
                    .iter()
                    .any(|a| other.selection.iter().any(|b| !separated(a, b)))
            })
        })
    {
        return Err(CoverageError::InputsOverlap { required });
    }
    Ok(SummaryCoverage {
        definition: Rc::clone(&first.definition),
        selection: union(inputs.iter().flat_map(|c| c.selection.clone()).collect()),
    })
}

/// Whether the time windows of every box of `inputs` are pairwise apart:
/// sorted by start, each ends before the next starts. `false` when a box
/// has no time window or two windows overlap, boxes of one input included.
fn windows_ordered_apart(inputs: &[SummaryCoverage]) -> bool {
    let mut windows = Vec::new();
    for selection_box in inputs.iter().flat_map(|input| &input.selection) {
        match &selection_box.relative_time {
            Some(window) => windows.push(window),
            None => return false,
        }
    }
    // A lower bound's position: unbounded first, then by value, an
    // included value before the excluded one.
    let start = |(lower, _): &&(Bound<i64>, Bound<i64>)| match lower {
        Bound::Unbounded => (false, 0, false),
        Bound::Included(at) => (true, *at, false),
        Bound::Excluded(at) => (true, *at, true),
    };
    windows.sort_by_key(start);
    windows.windows(2).all(|pair| {
        let ((_, upper), (lower, _)) = (pair[0], pair[1]);
        ends_before(upper, lower, |a: &i64, b: &i64| Some(a.cmp(b)))
    })
}

/// The hash [`SummaryCoverage::definition_id`] reports: each node's
/// operator without its children, output kind and schema (in their `Debug`
/// form, which every IR type derives and which is fixed for a given IR),
/// then its children's hashes in order. `memo` hashes a shared node once.
fn definition_hash(node: &Rc<OperatorNode>, memo: &mut HashMap<*const OperatorNode, u128>) -> u128 {
    if let Some(hash) = memo.get(&Rc::as_ptr(node)) {
        return *hash;
    }
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut hash = OFFSET;
    let mut write = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    let own = format!(
        "{:?}|{:?}|{:?}",
        node.operator.map_children(|_| ()),
        node.result_kind,
        node.schema
    );
    write(own.as_bytes());
    for child in node.children() {
        let child_hash = definition_hash(child, memo);
        write(&child_hash.to_le_bytes());
    }
    memo.insert(Rc::as_ptr(node), hash);
    hash
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
            // A rename outside a `Project` would be dropped with the node
            // when everything is lifted out of it, so such a node stays.
            NonASAPOp::Project { .. } => true,
            _ if base.operator.output_schema().ok().as_ref() != Some(&base.schema) => false,
            NonASAPOp::Filter { .. } => true,
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
        self.disjoint_in_time(other)
            || self
                .columns
                .iter()
                .any(|(column, a)| other.columns.get(column).is_some_and(|b| a.disjoint(b)))
    }

    /// Whether the boxes' time windows do not overlap.
    fn disjoint_in_time(&self, other: &Self) -> bool {
        match (&self.relative_time, &other.relative_time) {
            (Some((al, au)), Some((bl, bu))) => {
                intervals_disjoint((al, au), (bl, bu), |a: &i64, b: &i64| Some(a.cmp(b)))
            }
            _ => false,
        }
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
    if a == b {
        return Some(a.clone());
    }
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
