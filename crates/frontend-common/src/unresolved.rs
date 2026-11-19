//! The front-end-emitted, name-based operator tree: the unified IR
//! ([`NonASAPOp`](asap_types::ir::NonASAPOp) / [`ScalarExpr`]) before name
//! resolution. Scalar expressions are the IR's own generic types with
//! [`ColumnRef`] columns; the operator tree is a mirror of `NonASAPOp`.
//!
//! Differences of [`UnresolvedOp`] from `NonASAPOp`:
//! - every `ColumnId` is a name-based [`ColumnRef`];
//! - `Scan.schema` is `Option<Schema>` — a front end knows the schema only for
//!   a catalog-backed SQL leaf; `None` (PromQL) defers to the usage-derived
//!   [`resolve_schema_with_inherited`](crate::schema_resolver::resolve_schema_with_inherited);
//! - children are `Rc<UnresolvedOp>` rather than `Rc<OperatorNode>` — no
//!   derived schema exists yet;
//! - `Scan` and `Project` carry the SQL `qualifier` qualified references
//!   resolve by;
//! - `PromqlMap` and `PromqlScalarOp` exist only here: resolution lowers them
//!   to `Project` / `Filter` once the vector schema is known.

use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use asap_types::ir::operator::{
    AggIntent, BinaryOpKind, GroupKeys, InfoMatcher, JoinKind, Reduction, RelationalSetOpKind,
    SampleKind, Source, TimeShift, VectorMatch, WindowFrame, WindowFuncKind,
};
use asap_types::ir::scalar::{ColumnRef, Predicate, ProjectItem, ScalarExpr, SortKey};
use asap_types::ir::schema::Schema;
use asap_types::ir::BinaryOperator;
use asap_types::ir::TimeRangeKind;

/// A name-based scalar expression: the resolved IR's [`ScalarExpr`] with
/// [`ColumnRef`] columns and operator reads that point into the unresolved
/// tree.
pub type UnresolvedScalar = ScalarExpr<Rc<UnresolvedOp>, ColumnRef>;

/// A row-level filter predicate (WHERE clause / PromQL label matcher).
pub type UnresolvedPredicate = Predicate<Rc<UnresolvedOp>, ColumnRef>;

/// One item in a SELECT projection list.
pub type UnresolvedProjectItem = ProjectItem<Rc<UnresolvedOp>, ColumnRef>;

pub type UnresolvedSortKey = SortKey<Rc<UnresolvedOp>, ColumnRef>;

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
        /// The SQL table or alias qualifying the leaf's columns, which
        /// qualified references (`t.col`) above resolve by; `None` for a
        /// PromQL leaf.
        qualifier: Option<String>,
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
        /// The alias of a derived table (`FROM (SELECT …) t`) qualifying
        /// every output column, which qualified references above resolve
        /// by; `None` for an ordinary SELECT list.
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
        op: asap_types::ir::operator::BinaryOpKind,
        scalar_left: bool,
        return_bool: bool,
    },
}

impl UnresolvedOp {
    /// A binary operation over two vectors, matched by `vector_match`
    /// (`None`: one-to-one on every label but the metric name); `return_bool`
    /// makes a comparison answer 0 or 1 instead of filtering. The PromQL
    /// and MetricsQL frontends both lower `a / on(job) group_left b` here.
    pub fn vector_binary(
        kind: BinaryOpKind,
        vector_match: Option<VectorMatch>,
        return_bool: bool,
        lhs: UnresolvedOp,
        rhs: UnresolvedOp,
    ) -> Self {
        UnresolvedOp::BinaryOp {
            operator: BinaryOperator {
                kind,
                vector_match,
                checked_relative_division: false,
                checked_finite_division: false,
            },
            return_bool,
            lhs: Rc::new(lhs),
            rhs: Rc::new(rhs),
        }
    }

    /// A `Concat` of `children`.
    pub fn concat(children: Vec<UnresolvedOp>) -> Self {
        UnresolvedOp::Concat {
            children: children.into_iter().map(Rc::new).collect(),
        }
    }
}
