//! Flat operator/scalar payloads shared by logical transport.
use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ir::operator::agg_intent::AggIntent;
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::maintained_population::{MaintainedPopulation, PopulationStatistic};
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::{BinaryOperator, NonASAPOp, TimeRangeKind};
use crate::ir::operator::operator_properties::{
    ConcatDiscriminatorKey, GroupKeys, InfoMatcher, JoinKind, Reduction, RelationalSetOpKind,
    SampleKind, Source, TimeShift, WindowFrame, WindowFuncKind,
};
use crate::ir::scalar::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use crate::ir::scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};
use crate::ir::schema::state_type::{GroupingStrategy, SketchStatistic, SummaryUpdate};
use crate::ir::schema::{ColumnId, DataType, FieldDataType, Schema};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeRole {
    Input,
    Left,
    Right,
    /// The consumer reads the producer from inside one of its scalar
    /// expressions (`scalar(v)`, a scalar subquery, `EXISTS`, `IN`).
    ScalarRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupingEdgeCompatibility {
    Identical,
    ConsumerCoarsensProducer,
    Incompatible,
    NotApplicable,
}

/// Stable identity of a node within one exported logical ASAP DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LogicalASAPNodeId(pub u32);

// ── Wire mirrors of the scalar language ──────────────────────────────────

/// [`ScalarExpr`] with every operator reference replaced by the id of the
/// exported node (connected to the owner by an [`EdgeRole::ScalarRef`] edge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WireScalarExpr {
    Column(ColumnId),
    Literal(ScalarValue),
    Negative {
        expr: Box<WireScalarExpr>,
        semantics: ExprSemantics,
    },
    Compare {
        left: Box<WireScalarExpr>,
        op: CompareOpKind,
        right: Box<WireScalarExpr>,
        semantics: ExprSemantics,
    },
    BoolAnd(Vec<WireScalarExpr>),
    BoolOr(Vec<WireScalarExpr>),
    Not(Box<WireScalarExpr>),
    IsNull(Box<WireScalarExpr>),
    IsNotNull(Box<WireScalarExpr>),
    Cast {
        expr: Box<WireScalarExpr>,
        to: DataType,
        try_cast: bool,
    },
    InList {
        expr: Box<WireScalarExpr>,
        list: Vec<WireScalarExpr>,
        negated: bool,
    },
    FunctionCall {
        name: String,
        args: Vec<WireScalarExpr>,
    },
    Arithmetic {
        op: ArithmeticOpKind,
        left: Box<WireScalarExpr>,
        right: Box<WireScalarExpr>,
        semantics: ExprSemantics,
    },
    Case {
        operand: Option<Box<WireScalarExpr>>,
        branches: Vec<(WireScalarExpr, WireScalarExpr)>,
        else_expr: Option<Box<WireScalarExpr>>,
    },
    CurrentTimestamp,
    EvalTimestamp,
    PromqlScalarFromVector(LogicalASAPNodeId),
    ScalarSubquery(LogicalASAPNodeId),
    Exists {
        subquery: LogicalASAPNodeId,
        negated: bool,
    },
    InSubquery {
        expr: Box<WireScalarExpr>,
        subquery: LogicalASAPNodeId,
        negated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WirePredicate(pub WireScalarExpr);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireProjectItem {
    pub alias: Option<String>,
    pub expr: WireScalarExpr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireSortKey {
    pub expr: WireScalarExpr,
    pub ascending: bool,
    pub nulls_first: bool,
}

impl WireScalarExpr {
    /// Explicit producer IDs recursively referenced by this scalar tree.
    pub fn operator_refs(&self) -> Vec<LogicalASAPNodeId> {
        fn collect(expr: &WireScalarExpr, out: &mut Vec<LogicalASAPNodeId>) {
            use WireScalarExpr::*;
            match expr {
                PromqlScalarFromVector(id) | ScalarSubquery(id) => out.push(*id),
                Exists { subquery, .. } => out.push(*subquery),
                InSubquery { expr, subquery, .. } => {
                    out.push(*subquery);
                    collect(expr, out);
                }
                Negative { expr, .. }
                | Not(expr)
                | IsNull(expr)
                | IsNotNull(expr)
                | Cast { expr, .. } => collect(expr, out),
                Compare { left, right, .. } | Arithmetic { left, right, .. } => {
                    collect(left, out);
                    collect(right, out);
                }
                BoolAnd(args) | BoolOr(args) | FunctionCall { args, .. } => {
                    for expr in args {
                        collect(expr, out);
                    }
                }
                InList { expr, list, .. } => {
                    collect(expr, out);
                    for expr in list {
                        collect(expr, out);
                    }
                }
                Case {
                    operand,
                    branches,
                    else_expr,
                } => {
                    if let Some(expr) = operand {
                        collect(expr, out);
                    }
                    for (when, then) in branches {
                        collect(when, out);
                        collect(then, out);
                    }
                    if let Some(expr) = else_expr {
                        collect(expr, out);
                    }
                }
                Column(_) | Literal(_) | CurrentTimestamp | EvalTimestamp => {}
            }
        }
        let mut out = Vec::new();
        collect(self, &mut out);
        out
    }

    /// Mirror `expr`, resolving every operator reference through `id_of`.
    pub fn from_expr(
        expr: &ScalarExpr,
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> LogicalASAPNodeId,
    ) -> Self {
        fn boxed(
            e: &ScalarExpr,
            id_of: &mut impl FnMut(&Rc<OperatorNode>) -> LogicalASAPNodeId,
        ) -> Box<WireScalarExpr> {
            Box::new(WireScalarExpr::from_expr(e, id_of))
        }
        fn list(
            es: &[ScalarExpr],
            id_of: &mut impl FnMut(&Rc<OperatorNode>) -> LogicalASAPNodeId,
        ) -> Vec<WireScalarExpr> {
            es.iter()
                .map(|e| WireScalarExpr::from_expr(e, id_of))
                .collect()
        }
        match expr {
            ScalarExpr::Column(id) => WireScalarExpr::Column(*id),
            ScalarExpr::Literal(v) => WireScalarExpr::Literal(v.clone()),
            ScalarExpr::Negative { expr, semantics } => WireScalarExpr::Negative {
                expr: boxed(expr, id_of),
                semantics: *semantics,
            },
            ScalarExpr::Compare {
                left,
                op,
                right,
                semantics,
            } => WireScalarExpr::Compare {
                left: boxed(left, id_of),
                op: op.clone(),
                right: boxed(right, id_of),
                semantics: *semantics,
            },
            ScalarExpr::BoolAnd(parts) => WireScalarExpr::BoolAnd(list(parts, id_of)),
            ScalarExpr::BoolOr(parts) => WireScalarExpr::BoolOr(list(parts, id_of)),
            ScalarExpr::Not(e) => WireScalarExpr::Not(boxed(e, id_of)),
            ScalarExpr::IsNull(e) => WireScalarExpr::IsNull(boxed(e, id_of)),
            ScalarExpr::IsNotNull(e) => WireScalarExpr::IsNotNull(boxed(e, id_of)),
            ScalarExpr::Cast { expr, to, try_cast } => WireScalarExpr::Cast {
                expr: boxed(expr, id_of),
                to: to.clone(),
                try_cast: *try_cast,
            },
            ScalarExpr::InList {
                expr,
                list: items,
                negated,
            } => WireScalarExpr::InList {
                expr: boxed(expr, id_of),
                list: list(items, id_of),
                negated: *negated,
            },
            ScalarExpr::FunctionCall { name, args } => WireScalarExpr::FunctionCall {
                name: name.clone(),
                args: list(args, id_of),
            },
            ScalarExpr::Arithmetic {
                op,
                left,
                right,
                semantics,
            } => WireScalarExpr::Arithmetic {
                op: op.clone(),
                left: boxed(left, id_of),
                right: boxed(right, id_of),
                semantics: *semantics,
            },
            ScalarExpr::Case {
                operand,
                branches,
                else_expr,
            } => WireScalarExpr::Case {
                operand: operand.as_ref().map(|e| boxed(e, id_of)),
                branches: branches
                    .iter()
                    .map(|(w, t)| (Self::from_expr(w, id_of), Self::from_expr(t, id_of)))
                    .collect(),
                else_expr: else_expr.as_ref().map(|e| boxed(e, id_of)),
            },
            ScalarExpr::CurrentTimestamp => WireScalarExpr::CurrentTimestamp,
            ScalarExpr::EvalTimestamp => WireScalarExpr::EvalTimestamp,
            ScalarExpr::PromqlScalarFromVector(node) => {
                WireScalarExpr::PromqlScalarFromVector(id_of(node))
            }
            ScalarExpr::ScalarSubquery(node) => WireScalarExpr::ScalarSubquery(id_of(node)),
            ScalarExpr::Exists { subquery, negated } => WireScalarExpr::Exists {
                subquery: id_of(subquery),
                negated: *negated,
            },
            ScalarExpr::InSubquery {
                expr,
                subquery,
                negated,
            } => WireScalarExpr::InSubquery {
                expr: boxed(expr, id_of),
                subquery: id_of(subquery),
                negated: *negated,
            },
        }
    }
}

impl WirePredicate {
    fn from_pred(
        p: &Predicate,
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> LogicalASAPNodeId,
    ) -> Self {
        WirePredicate(WireScalarExpr::from_expr(&p.0, id_of))
    }
}

impl WireSortKey {
    fn from_keys(
        keys: &[SortKey],
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> LogicalASAPNodeId,
    ) -> Vec<Self> {
        keys.iter()
            .map(|k| WireSortKey {
                expr: WireScalarExpr::from_expr(&k.expr, id_of),
                ascending: k.ascending,
                nulls_first: k.nulls_first,
            })
            .collect()
    }
}

// ── Wire mirror of the non-ASAP operator vocabulary ──────────────────────

/// [`NonASAPOp`] without its child fields (children are edges) and with
/// every scalar expression mirrored as [`WireScalarExpr`]. Fields named
/// `kind` in the IR are renamed so they do not collide with the variant tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NonASAPOpKind {
    Scan {
        source: Source,
        #[serde(default)]
        predicates: Vec<WirePredicate>,
        schema: Schema,
    },
    Values {
        rows: Vec<Vec<WireScalarExpr>>,
        schema: Schema,
    },
    Filter {
        pred: WirePredicate,
    },
    Project {
        cols: Vec<WireProjectItem>,
        #[serde(default)]
        qualifier: Option<String>,
    },
    Aggregate {
        reduction: Reduction,
        measures: Vec<AggIntent>,
        #[serde(default)]
        output_names: Vec<String>,
        #[serde(default)]
        filters: Vec<Option<WirePredicate>>,
        #[serde(default)]
        having: Option<WirePredicate>,
    },
    Join {
        join_kind: JoinKind,
        pred: WirePredicate,
    },
    SetOp {
        set_kind: RelationalSetOpKind,
        all: bool,
    },
    Concat {
        #[serde(default)]
        discriminator_unique_key: Option<ConcatDiscriminatorKey>,
    },
    Dedup {
        cols: Vec<ColumnId>,
    },
    Sort {
        keys: Vec<WireSortKey>,
        #[serde(default)]
        partition_by: GroupKeys,
    },
    Limit {
        n: Option<usize>,
        offset: usize,
        #[serde(default)]
        partition_by: GroupKeys,
    },
    BinaryOp {
        operator: BinaryOperator,
        #[serde(default)]
        return_bool: bool,
    },
    #[serde(rename = "sql_window_func")]
    SQLWindowFunc {
        func: WindowFuncKind,
        args: Vec<WireScalarExpr>,
        partition_by: GroupKeys,
        order_by: Vec<WireSortKey>,
        #[serde(default)]
        frame: Option<WindowFrame>,
        output_name: String,
    },
    TimeRange {
        range: Duration,
        range_kind: TimeRangeKind,
    },
    TimeShift {
        shift: TimeShift,
    },
    PromqlVectorFromScalar {
        expr: WireScalarExpr,
    },
    PromqlRelabel {
        dst: String,
        value: WireScalarExpr,
    },
    PromqlInfoEnrich {
        #[serde(default)]
        selector: Vec<InfoMatcher>,
    },
    PromqlSeriesSample {
        #[serde(default)]
        by: GroupKeys,
        sample_kind: SampleKind,
    },
    PromqlSubquery {
        range: Duration,
        #[serde(default)]
        resolution: Option<Duration>,
    },
}

impl NonASAPOpKind {
    /// Mirror `op`, resolving every operator node its scalar expressions
    /// reference through `id_of`.
    pub fn from_op(
        op: &NonASAPOp,
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> LogicalASAPNodeId,
    ) -> Self {
        use NonASAPOp as Op;
        match op {
            Op::Scan {
                source,
                predicates,
                schema,
            } => NonASAPOpKind::Scan {
                source: source.clone(),
                predicates: predicates
                    .iter()
                    .map(|p| WirePredicate::from_pred(p, id_of))
                    .collect(),
                schema: schema.clone(),
            },
            Op::Values { rows, schema } => NonASAPOpKind::Values {
                rows: rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|e| WireScalarExpr::from_expr(e, id_of))
                            .collect()
                    })
                    .collect(),
                schema: schema.clone(),
            },
            Op::Filter { pred, .. } => NonASAPOpKind::Filter {
                pred: WirePredicate::from_pred(pred, id_of),
            },
            Op::Project {
                cols, qualifier, ..
            } => NonASAPOpKind::Project {
                cols: cols
                    .iter()
                    .map(|ProjectItem { alias, expr }| WireProjectItem {
                        alias: alias.clone(),
                        expr: WireScalarExpr::from_expr(expr, id_of),
                    })
                    .collect(),
                qualifier: qualifier.clone(),
            },
            Op::Aggregate {
                reduction,
                measures,
                output_names,
                filters,
                having,
                ..
            } => NonASAPOpKind::Aggregate {
                reduction: reduction.clone(),
                measures: measures.clone(),
                output_names: output_names.clone(),
                filters: filters
                    .iter()
                    .map(|p| p.as_ref().map(|p| WirePredicate::from_pred(p, id_of)))
                    .collect(),
                having: having.as_ref().map(|p| WirePredicate::from_pred(p, id_of)),
            },
            Op::Join { kind, pred, .. } => NonASAPOpKind::Join {
                join_kind: kind.clone(),
                pred: WirePredicate::from_pred(pred, id_of),
            },
            Op::SetOp { kind, all, .. } => NonASAPOpKind::SetOp {
                set_kind: kind.clone(),
                all: *all,
            },
            Op::Concat {
                discriminator_unique_key,
                ..
            } => NonASAPOpKind::Concat {
                discriminator_unique_key: discriminator_unique_key.clone(),
            },
            Op::Dedup { cols, .. } => NonASAPOpKind::Dedup { cols: cols.clone() },
            Op::Sort {
                keys, partition_by, ..
            } => NonASAPOpKind::Sort {
                keys: WireSortKey::from_keys(keys, id_of),
                partition_by: partition_by.clone(),
            },
            Op::Limit {
                n,
                offset,
                partition_by,
                ..
            } => NonASAPOpKind::Limit {
                n: *n,
                offset: *offset,
                partition_by: partition_by.clone(),
            },
            Op::BinaryOp {
                operator,
                return_bool,
                ..
            } => NonASAPOpKind::BinaryOp {
                operator: operator.clone(),
                return_bool: *return_bool,
            },
            Op::SQLWindowFunc {
                func,
                args,
                partition_by,
                order_by,
                frame,
                output_name,
                ..
            } => NonASAPOpKind::SQLWindowFunc {
                func: func.clone(),
                args: args
                    .iter()
                    .map(|e| WireScalarExpr::from_expr(e, id_of))
                    .collect(),
                partition_by: partition_by.clone(),
                order_by: WireSortKey::from_keys(order_by, id_of),
                frame: frame.clone(),
                output_name: output_name.clone(),
            },
            Op::TimeRange { range, kind, .. } => NonASAPOpKind::TimeRange {
                range: *range,
                range_kind: *kind,
            },
            Op::TimeShift { shift, .. } => NonASAPOpKind::TimeShift { shift: *shift },
            Op::PromqlVectorFromScalar(e) => NonASAPOpKind::PromqlVectorFromScalar {
                expr: WireScalarExpr::from_expr(e, id_of),
            },
            Op::PromqlRelabel { dst, value, .. } => NonASAPOpKind::PromqlRelabel {
                dst: dst.clone(),
                value: WireScalarExpr::from_expr(value, id_of),
            },
            Op::PromqlInfoEnrich { selector, .. } => NonASAPOpKind::PromqlInfoEnrich {
                selector: selector.clone(),
            },
            Op::PromqlSeriesSample { by, kind, .. } => NonASAPOpKind::PromqlSeriesSample {
                by: by.clone(),
                sample_kind: *kind,
            },
            Op::PromqlSubquery {
                range, resolution, ..
            } => NonASAPOpKind::PromqlSubquery {
                range: *range,
                resolution: *resolution,
            },
        }
    }
}

// ── The exported DAG ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LogicalASAPOperatorPayload {
    Relational {
        operator: NonASAPOpKind,
    },
    SummaryAgg {
        family: FieldDataType,
        input: SummaryUpdate,
        reduction: Reduction,
        grouping: GroupingStrategy,
        #[serde(default)]
        filter: Option<WirePredicate>,
    },
    SummaryEstimate {
        query: SketchStatistic,
    },
    FinalizeExactAccumulator,
    MaintainPopulation {
        population: MaintainedPopulation<OperatorNode>,
    },
    EvaluatePopulation {
        evaluation: PopulationStatistic,
    },
    SummaryMerge,
    SummarySubtract,
    SummaryDelete {
        key: ColumnId,
    },
    SummaryJoin {
        key: ColumnId,
        family: FieldDataType,
    },
    Extension {
        name: String,
    },
}

/// The operator's own inputs with their edge roles, in field order.
pub(super) fn input_edges(operator: &Operator) -> Vec<(&Rc<OperatorNode>, EdgeRole)> {
    match operator {
        Operator::NonASAP(op) => match op {
            NonASAPOp::Join { left, right, .. }
            | NonASAPOp::SetOp { left, right, .. }
            | NonASAPOp::BinaryOp {
                lhs: left,
                rhs: right,
                ..
            } => vec![(left, EdgeRole::Left), (right, EdgeRole::Right)],
            NonASAPOp::Concat { children, .. } => {
                children.iter().map(|c| (c, EdgeRole::Input)).collect()
            }
            NonASAPOp::Filter { child, .. }
            | NonASAPOp::Project { child, .. }
            | NonASAPOp::Aggregate { child, .. }
            | NonASAPOp::Dedup { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::SQLWindowFunc { child, .. }
            | NonASAPOp::TimeRange { child, .. }
            | NonASAPOp::TimeShift { child, .. }
            | NonASAPOp::PromqlRelabel { child, .. }
            | NonASAPOp::PromqlInfoEnrich { child, .. }
            | NonASAPOp::PromqlSeriesSample { child, .. }
            | NonASAPOp::PromqlSubquery { child, .. } => vec![(child, EdgeRole::Input)],
            NonASAPOp::Scan { .. }
            | NonASAPOp::Values { .. }
            | NonASAPOp::PromqlVectorFromScalar(_) => vec![],
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummarySubtract { left, right }
            | ASAPOp::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => vec![(left, EdgeRole::Left), (right, EdgeRole::Right)],
            ASAPOp::SummaryMerge { children } => {
                children.iter().map(|c| (c, EdgeRole::Input)).collect()
            }
            ASAPOp::SummaryAgg { child, .. }
            | ASAPOp::FinalizeExactAccumulator { child }
            | ASAPOp::MaintainPopulation { child, .. }
            | ASAPOp::EvaluatePopulation { child, .. }
            | ASAPOp::Extension { child, .. } => vec![(child, EdgeRole::Input)],
            ASAPOp::SummaryEstimate { summary_input, .. }
            | ASAPOp::SummaryDelete { summary_input, .. } => {
                vec![(summary_input, EdgeRole::Input)]
            }
        },
    }
}

pub(super) fn payload_of(
    operator: &Operator,
    id_of: &mut impl FnMut(&Rc<OperatorNode>) -> LogicalASAPNodeId,
) -> LogicalASAPOperatorPayload {
    match operator {
        Operator::NonASAP(op) => LogicalASAPOperatorPayload::Relational {
            operator: NonASAPOpKind::from_op(op, id_of),
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummaryAgg {
                family,
                input,
                reduction,
                grouping,
                filter,
                ..
            } => LogicalASAPOperatorPayload::SummaryAgg {
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: filter.as_ref().map(|p| WirePredicate::from_pred(p, id_of)),
            },
            ASAPOp::SummaryEstimate { query, .. } => LogicalASAPOperatorPayload::SummaryEstimate {
                query: query.clone(),
            },
            ASAPOp::FinalizeExactAccumulator { .. } => {
                LogicalASAPOperatorPayload::FinalizeExactAccumulator
            }
            ASAPOp::MaintainPopulation { population, .. } => {
                LogicalASAPOperatorPayload::MaintainPopulation {
                    population: population.clone(),
                }
            }
            ASAPOp::EvaluatePopulation { evaluation, .. } => {
                LogicalASAPOperatorPayload::EvaluatePopulation {
                    evaluation: evaluation.clone(),
                }
            }
            ASAPOp::SummaryMerge { .. } => LogicalASAPOperatorPayload::SummaryMerge,
            ASAPOp::SummarySubtract { .. } => LogicalASAPOperatorPayload::SummarySubtract,
            ASAPOp::SummaryDelete { key, .. } => {
                LogicalASAPOperatorPayload::SummaryDelete { key: *key }
            }
            ASAPOp::SummaryJoin { key, family, .. } => LogicalASAPOperatorPayload::SummaryJoin {
                key: *key,
                family: family.clone(),
            },
            ASAPOp::Extension { name, .. } => {
                LogicalASAPOperatorPayload::Extension { name: name.clone() }
            }
        },
    }
}

/// Grouping compatibility between two `SummaryAgg`s by their reductions.
pub(super) fn grouping_compatibility(
    producer: &Operator,
    consumer: &Operator,
) -> GroupingEdgeCompatibility {
    let (
        Operator::ASAP(ASAPOp::SummaryAgg {
            reduction: producer,
            ..
        }),
        Operator::ASAP(ASAPOp::SummaryAgg {
            reduction: consumer,
            ..
        }),
    ) = (producer, consumer)
    else {
        return GroupingEdgeCompatibility::NotApplicable;
    };
    match (producer, consumer) {
        (p, c) if p == c => GroupingEdgeCompatibility::Identical,
        (Reduction::PerEntity, Reduction::Reduce(_)) => {
            GroupingEdgeCompatibility::ConsumerCoarsensProducer
        }
        (Reduction::Reduce(p), Reduction::Reduce(c))
            if !p.is_without() && !c.is_without() && c.iter().all(|key| p.contains(key)) =>
        {
            GroupingEdgeCompatibility::ConsumerCoarsensProducer
        }
        _ => GroupingEdgeCompatibility::Incompatible,
    }
}
