//! The front-end-emitted, name-based operator tree: a mirror of the unified
//! IR ([`NonASAPOp`](asap_types::ir::NonASAPOp) / [`ScalarExpr`](asap_types::ir::ScalarExpr))
//! before name resolution.
//!
//! Differences from the resolved IR, and nothing else:
//! - every `ColumnId` is a name-based [`ColumnRef`];
//! - `Scan.schema` is `Option<Schema>` — a front end knows the schema only for
//!   a catalog-backed SQL leaf; `None` (PromQL) defers to the
//!   [`SchemaResolver`](crate::schema_resolver::SchemaResolver);
//! - children are `Rc<UnresolvedOp>` rather than `Rc<OperatorNode>` — no
//!   derived schema exists yet.

use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use asap_types::ir::operator_properties::ConcatDiscriminatorKey;
use asap_types::ir::BinaryOperator;
use asap_types::ir::{ExprSemantics, TimeRangeKind};
use asap_types::pre_asap::{
    AggIntent, ArithmeticOpKind, ColumnRef, CompareOpKind, DataType, GroupKeys, InfoMatcher,
    JoinKind, Reduction, RelationalSetOpKind, SampleKind, ScalarValue, Schema, Source, TimeShift,
    WindowFrame, WindowFuncKind,
};

/// A row-level filter predicate (WHERE clause / PromQL label matcher).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedPredicate(pub UnresolvedScalar);

/// One item in a SELECT projection list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedProjectItem {
    pub alias: Option<String>,
    pub expr: UnresolvedScalar,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedSortKey {
    pub expr: UnresolvedScalar,
    pub ascending: bool,
    pub nulls_first: bool,
}

/// A name-based scalar expression; see
/// [`ScalarExpr`](asap_types::ir::ScalarExpr) for the meaning of each variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnresolvedScalar {
    Column(ColumnRef),
    Literal(ScalarValue),
    Negative {
        expr: Box<UnresolvedScalar>,
        semantics: ExprSemantics,
    },
    Compare {
        left: Box<UnresolvedScalar>,
        op: CompareOpKind,
        right: Box<UnresolvedScalar>,
        semantics: ExprSemantics,
    },
    BoolAnd(Vec<UnresolvedScalar>),
    BoolOr(Vec<UnresolvedScalar>),
    Not(Box<UnresolvedScalar>),
    IsNull(Box<UnresolvedScalar>),
    IsNotNull(Box<UnresolvedScalar>),
    Cast {
        expr: Box<UnresolvedScalar>,
        to: DataType,
        try_cast: bool,
    },
    InList {
        expr: Box<UnresolvedScalar>,
        list: Vec<UnresolvedScalar>,
        negated: bool,
    },
    FunctionCall {
        name: String,
        args: Vec<UnresolvedScalar>,
    },
    Arithmetic {
        op: ArithmeticOpKind,
        left: Box<UnresolvedScalar>,
        right: Box<UnresolvedScalar>,
        semantics: ExprSemantics,
    },
    Case {
        operand: Option<Box<UnresolvedScalar>>,
        branches: Vec<(UnresolvedScalar, UnresolvedScalar)>,
        else_expr: Option<Box<UnresolvedScalar>>,
    },
    CurrentTimestamp,
    EvalTimestamp,
    /// PromQL `scalar(v)`. The operator is resolved as a root in its own scope.
    PromqlScalarFromVector(Rc<UnresolvedOp>),
    ScalarSubquery(Rc<UnresolvedOp>),
    Exists {
        subquery: Rc<UnresolvedOp>,
        negated: bool,
    },
    InSubquery {
        expr: Box<UnresolvedScalar>,
        subquery: Rc<UnresolvedOp>,
        negated: bool,
    },
}

/// The name-based operator tree; see [`NonASAPOp`](asap_types::ir::NonASAPOp)
/// for the meaning of each variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnresolvedOp {
    Scan {
        source: Source,
        predicates: Vec<UnresolvedPredicate>,
        /// `Some` for a catalog-backed (SQL) leaf; `None` defers to the
        /// usage-derived schema resolver.
        schema: Option<Schema>,
    },
    Values {
        rows: Vec<Vec<UnresolvedScalar>>,
        schema: Schema,
    },
    Filter {
        pred: UnresolvedPredicate,
        child: Rc<UnresolvedOp>,
    },
    Project {
        cols: Vec<UnresolvedProjectItem>,
        qualifier: Option<String>,
        child: Rc<UnresolvedOp>,
    },
    Aggregate {
        reduction: Reduction<ColumnRef>,
        measures: Vec<AggIntent<ColumnRef>>,
        output_names: Vec<String>,
        filters: Vec<Option<UnresolvedPredicate>>,
        having: Option<UnresolvedPredicate>,
        child: Rc<UnresolvedOp>,
    },
    Join {
        kind: JoinKind,
        pred: UnresolvedPredicate,
        left: Rc<UnresolvedOp>,
        right: Rc<UnresolvedOp>,
    },
    SetOp {
        kind: RelationalSetOpKind,
        all: bool,
        left: Rc<UnresolvedOp>,
        right: Rc<UnresolvedOp>,
    },
    Concat {
        children: Vec<Rc<UnresolvedOp>>,
        discriminator_unique_key: Option<ConcatDiscriminatorKey<ColumnRef>>,
    },
    Dedup {
        cols: Vec<ColumnRef>,
        child: Rc<UnresolvedOp>,
    },
    Sort {
        keys: Vec<UnresolvedSortKey>,
        partition_by: GroupKeys<ColumnRef>,
        child: Rc<UnresolvedOp>,
    },
    Limit {
        n: Option<usize>,
        offset: usize,
        partition_by: GroupKeys<ColumnRef>,
        child: Rc<UnresolvedOp>,
    },
    BinaryOp {
        operator: BinaryOperator,
        return_bool: bool,
        lhs: Rc<UnresolvedOp>,
        rhs: Rc<UnresolvedOp>,
    },
    SQLWindowFunc {
        func: WindowFuncKind,
        args: Vec<UnresolvedScalar>,
        partition_by: GroupKeys<ColumnRef>,
        order_by: Vec<UnresolvedSortKey>,
        frame: Option<WindowFrame>,
        output_name: String,
        child: Rc<UnresolvedOp>,
    },
    TimeRange {
        range: Duration,
        kind: TimeRangeKind,
        child: Rc<UnresolvedOp>,
    },
    TimeShift {
        shift: TimeShift,
        child: Rc<UnresolvedOp>,
    },
    PromqlVectorFromScalar(UnresolvedScalar),
    PromqlRelabel {
        dst: String,
        value: UnresolvedScalar,
        child: Rc<UnresolvedOp>,
    },
    PromqlInfoEnrich {
        selector: Vec<InfoMatcher>,
        child: Rc<UnresolvedOp>,
    },
    PromqlSeriesSample {
        by: GroupKeys<ColumnRef>,
        kind: SampleKind,
        child: Rc<UnresolvedOp>,
    },
    PromqlSubquery {
        range: Duration,
        resolution: Option<Duration>,
        child: Rc<UnresolvedOp>,
    },
    /// Bind the complete vector schema before lowering to Project or Filter.
    /// Frontend-only expansion to a projection preserving the complete series identity.
    PromqlMap {
        child: Rc<UnresolvedOp>,
        sample: UnresolvedScalar,
        drop_metric_name: bool,
    },
    PromqlScalarOp {
        child: Rc<UnresolvedOp>,
        scalar: UnresolvedScalar,
        op: asap_types::pre_asap::BinaryOpKind,
        scalar_left: bool,
        return_bool: bool,
    },
}

impl UnresolvedScalar {
    /// The direct scalar sub-expressions (not the operators this expression
    /// reads — see [`operator_refs`](Self::operator_refs)).
    pub fn children(&self) -> Vec<&UnresolvedScalar> {
        use UnresolvedScalar::*;
        match self {
            Column(_)
            | Literal(_)
            | CurrentTimestamp
            | EvalTimestamp
            | PromqlScalarFromVector(_)
            | ScalarSubquery(_)
            | Exists { .. } => vec![],
            Negative { expr, .. }
            | Not(expr)
            | IsNull(expr)
            | IsNotNull(expr)
            | Cast { expr, .. }
            | InSubquery { expr, .. } => vec![expr],
            Compare { left, right, .. } | Arithmetic { left, right, .. } => vec![left, right],
            BoolAnd(parts) | BoolOr(parts) => parts.iter().collect(),
            InList { expr, list, .. } => {
                let mut v = vec![expr.as_ref()];
                v.extend(list.iter());
                v
            }
            FunctionCall { args, .. } => args.iter().collect(),
            Case {
                operand,
                branches,
                else_expr,
            } => {
                let mut v = Vec::new();
                if let Some(op) = operand {
                    v.push(op.as_ref());
                }
                for (when, then) in branches {
                    v.push(when);
                    v.push(then);
                }
                if let Some(e) = else_expr {
                    v.push(e.as_ref());
                }
                v
            }
        }
    }

    /// Every column referenced in this expression, not inside the operators
    /// it reads (those have their own scope).
    pub fn columns_referenced(&self) -> Vec<&ColumnRef> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a ColumnRef>) {
        if let UnresolvedScalar::Column(c) = self {
            out.push(c);
        }
        for child in self.children() {
            child.collect_columns(out);
        }
    }

    /// The operators this expression (transitively) reads.
    pub fn operator_refs(&self) -> Vec<&Rc<UnresolvedOp>> {
        let mut out = Vec::new();
        self.collect_operator_refs(&mut out);
        out
    }

    fn collect_operator_refs<'a>(&'a self, out: &mut Vec<&'a Rc<UnresolvedOp>>) {
        use UnresolvedScalar::*;
        match self {
            PromqlScalarFromVector(op) | ScalarSubquery(op) => out.push(op),
            Exists { subquery, .. } | InSubquery { subquery, .. } => out.push(subquery),
            _ => {}
        }
        for child in self.children() {
            child.collect_operator_refs(out);
        }
    }
}

impl UnresolvedOp {
    /// An ordinary `Concat` (no unique-key claim).
    pub fn concat(children: Vec<UnresolvedOp>) -> Self {
        UnresolvedOp::Concat {
            children: children.into_iter().map(Rc::new).collect(),
            discriminator_unique_key: None,
        }
    }

    /// A `Concat` whose output carries the caller-proven compound unique key
    /// `(discriminator, inner_key)`. Nothing verifies the claim.
    pub fn concat_with_discriminator(
        children: Vec<UnresolvedOp>,
        discriminator: ColumnRef,
        inner_key: Vec<ColumnRef>,
    ) -> Self {
        UnresolvedOp::Concat {
            children: children.into_iter().map(Rc::new).collect(),
            discriminator_unique_key: Some(ConcatDiscriminatorKey::new(discriminator, inner_key)),
        }
    }

    /// Every scalar expression this operator owns.
    pub fn scalar_exprs(&self) -> Vec<&UnresolvedScalar> {
        use UnresolvedOp::*;
        match self {
            Scan { predicates, .. } => predicates.iter().map(|p| &p.0).collect(),
            Values { rows, .. } => rows.iter().flatten().collect(),
            Filter { pred, .. } | Join { pred, .. } => vec![&pred.0],
            Project { cols, .. } => cols.iter().map(|c| &c.expr).collect(),
            Aggregate {
                filters, having, ..
            } => filters
                .iter()
                .flatten()
                .chain(having.iter())
                .map(|p| &p.0)
                .collect(),
            Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
            SQLWindowFunc { args, order_by, .. } => args
                .iter()
                .chain(order_by.iter().map(|k| &k.expr))
                .collect(),
            PromqlVectorFromScalar(e) => vec![e],
            PromqlScalarOp { scalar, .. } => vec![scalar],
            PromqlMap { sample, .. } => vec![sample],
            PromqlRelabel { value, .. } => vec![value],
            SetOp { .. }
            | Concat { .. }
            | Dedup { .. }
            | Limit { .. }
            | BinaryOp { .. }
            | TimeRange { .. }
            | TimeShift { .. }
            | PromqlInfoEnrich { .. }
            | PromqlSeriesSample { .. }
            | PromqlSubquery { .. } => vec![],
        }
    }
}
