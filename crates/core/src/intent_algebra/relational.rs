//! The Layer-2 relational IR — the per-language query algebra the parser
//! front ends emit, before [`convert_root`](super::lower::convert_root) lowers
//! it to the canonical L3 [`query_expr::QueryExpr`](super::query_expr::QueryExpr).
//!
//! Leaf / scalar types (`ColumnRef`, `PartitionKeys`, `SortKey`,
//! `BinaryOpKind`, `VectorMatch`) are owned by `query_expr` and re-used here so
//! there is one canonical spelling. Filter / having / project expressions use
//! the shared language-independent [`L3Expr`](super::expr_ir::L3Expr).

use std::time::Duration;

use super::expr_ir::L2Expr;
pub use super::query_expr::{BinaryOpKind, ColumnRef, PartitionKeys, VectorMatch, WindowFuncKind};
use super::schema::Schema;

/// SELECT-list item at Layer 2 — a name-based [`L2Expr`] + optional alias.
/// (`query_expr::ProjectItem` is the positional L3 sibling.)
#[derive(Debug, Clone, PartialEq)]
pub struct L2ProjectItem {
    pub alias: Option<String>,
    pub expr: L2Expr,
}

/// ORDER BY key at Layer 2 — a name-based [`L2Expr`] + direction.
#[derive(Debug, Clone, PartialEq)]
pub struct L2SortKey {
    pub expr: L2Expr,
    pub ascending: bool,
    pub nulls_first: bool,
}

/// Base relation / metric stream source.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSpec {
    /// Metric name (PromQL) or table name (SQL).
    pub name: String,
    /// Front-end-resolved leaf schema. `Some` for SQL tables (DataFusion knows
    /// the columns); `None` for PromQL, where the [`Binder`](super::binder)
    /// synthesises a usage-derived schema (the `(ts, value)` floor + referenced
    /// labels). The presence of a schema also selects the L3 `Source` variant:
    /// `Some` → `Source::Table`, `None` → `Source::TimeSeries`.
    pub schema: Option<Schema>,
}

impl SourceSpec {
    /// A PromQL-style leaf whose schema the Binder synthesises.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            schema: None,
        }
    }

    /// A SQL-style leaf carrying its front-end-resolved schema.
    pub fn with_schema(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            schema: Some(schema),
        }
    }
}

/// One aggregate function in a GROUP BY / AGGREGATE node.
#[derive(Debug, Clone, PartialEq)]
pub struct AggItem {
    pub alias: String,
    pub func: AggFunc,
    pub col: ColumnRef,
    pub distinct: bool,
}

/// Layer-2 aggregate functions. Mapped to canonical [`AggIntent`] by
/// [`super::lower::convert`].
#[derive(Debug, Clone, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    StdDev {
        population: bool,
    },
    Variance {
        population: bool,
    },
    Quantile(f64),
    /// COUNT DISTINCT — maps to `Cardinality`.
    CountDistinct,
    /// Heavy-hitter top-k — maps to `AggIntent::TopK`.
    HeavyHitters {
        k: u64,
    },
    /// PromQL `rate()` / `irate()` — carries the range-vector window so the
    /// canonical `Rate` intent owns it (no separate `Window` node).
    Rate {
        window: Duration,
    },
    /// PromQL `increase()` — see `Rate`.
    Increase {
        window: Duration,
    },
}

/// The Layer-2 relational query IR.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryExpr {
    /// A named metric stream or table — the outermost leaf.
    Source(SourceSpec),
    /// Reference to a CTE / let-binding by name.
    Ref(String),

    /// σ — row-level filter (WHERE / PromQL label matchers).
    Filter { pred: L2Expr, input: Box<QueryExpr> },

    /// π — projection / SELECT list (SQL). Column refs in `cols` resolve by
    /// name against the child schema during conversion.
    Project {
        cols: Vec<L2ProjectItem>,
        input: Box<QueryExpr>,
    },

    /// γ + α — GROUP BY (`keys`) followed by aggregate functions.
    Aggregate {
        keys: Vec<String>,
        aggs: Vec<AggItem>,
        having: Option<L2Expr>,
        input: Box<QueryExpr>,
    },

    /// ψ — time window (PromQL `[5m]`).
    Window {
        duration: Duration,
        slide: Option<Duration>,
        input: Box<QueryExpr>,
    },

    /// Partition the stream by key-tuple (`by (dims)` / `without (dims)`).
    Partition {
        keys: PartitionKeys,
        input: Box<QueryExpr>,
    },
    /// δ — deduplicate on `cols`.
    Distinct {
        cols: Vec<ColumnRef>,
        input: Box<QueryExpr>,
    },
    /// τ — heavy-hitter top-k. `by` are the grouping keys.
    TopK {
        k: u64,
        by: Vec<String>,
        input: Box<QueryExpr>,
    },
    /// ⊕ — merge sub-results from independent branches.
    Merge { inputs: Vec<QueryExpr> },

    Join {
        kind: super::query_expr::JoinKind,
        pred: Option<L2Expr>,
        left: Box<QueryExpr>,
        right: Box<QueryExpr>,
    },
    SetOp {
        kind: super::query_expr::SetOpKind,
        all: bool,
        left: Box<QueryExpr>,
        right: Box<QueryExpr>,
    },

    Sort {
        keys: Vec<L2SortKey>,
        input: Box<QueryExpr>,
    },
    Limit {
        n: u64,
        offset: u64,
        input: Box<QueryExpr>,
    },

    LetBinding {
        name: String,
        expr: Box<QueryExpr>,
        body: Box<QueryExpr>,
    },

    /// PromQL sub-query syntax: `<expr>[range:resolution]`.
    PromQLSubquery {
        range: Duration,
        resolution: Option<Duration>,
        input: Box<QueryExpr>,
    },

    /// SQL analytic window function `func(args) OVER (PARTITION BY … ORDER BY …)`.
    WindowFunc {
        func: WindowFuncKind,
        args: Vec<L2Expr>,
        partition_by: Vec<String>,
        order_by: Vec<L2SortKey>,
        output_name: String,
        input: Box<QueryExpr>,
    },

    /// Binary op between two instant-vector expressions (PromQL `+`, `/`, …).
    BinaryOp {
        op: BinaryOpKind,
        lhs: Box<QueryExpr>,
        rhs: Box<QueryExpr>,
        vector_match: Option<VectorMatch>,
    },
}

impl QueryExpr {
    /// Walk the tree depth-first, calling `f` on every node.
    pub fn walk<F: FnMut(&QueryExpr)>(&self, f: &mut F) {
        f(self);
        match self {
            QueryExpr::Source(_) | QueryExpr::Ref(_) => {}
            QueryExpr::Filter { input, .. }
            | QueryExpr::Project { input, .. }
            | QueryExpr::Aggregate { input, .. }
            | QueryExpr::Window { input, .. }
            | QueryExpr::Partition { input, .. }
            | QueryExpr::Distinct { input, .. }
            | QueryExpr::TopK { input, .. }
            | QueryExpr::Sort { input, .. }
            | QueryExpr::Limit { input, .. }
            | QueryExpr::WindowFunc { input, .. }
            | QueryExpr::PromQLSubquery { input, .. } => input.walk(f),
            QueryExpr::Merge { inputs } => {
                for i in inputs {
                    i.walk(f);
                }
            }
            QueryExpr::Join { left, right, .. }
            | QueryExpr::SetOp { left, right, .. }
            | QueryExpr::BinaryOp {
                lhs: left,
                rhs: right,
                ..
            } => {
                left.walk(f);
                right.walk(f);
            }
            QueryExpr::LetBinding { expr, body, .. } => {
                expr.walk(f);
                body.walk(f);
            }
        }
    }

    /// The leftmost `Source` leaf.
    pub fn leaf_source(&self) -> Option<&SourceSpec> {
        match self {
            QueryExpr::Source(s) => Some(s),
            QueryExpr::Filter { input, .. }
            | QueryExpr::Project { input, .. }
            | QueryExpr::Aggregate { input, .. }
            | QueryExpr::Window { input, .. }
            | QueryExpr::Partition { input, .. }
            | QueryExpr::Distinct { input, .. }
            | QueryExpr::TopK { input, .. }
            | QueryExpr::Sort { input, .. }
            | QueryExpr::Limit { input, .. }
            | QueryExpr::WindowFunc { input, .. }
            | QueryExpr::PromQLSubquery { input, .. } => input.leaf_source(),
            QueryExpr::Merge { inputs } => inputs.first()?.leaf_source(),
            QueryExpr::Join { left, .. }
            | QueryExpr::SetOp { left, .. }
            | QueryExpr::BinaryOp { lhs: left, .. } => left.leaf_source(),
            QueryExpr::LetBinding { body, .. } => body.leaf_source(),
            QueryExpr::Ref(_) => None,
        }
    }

    /// Outermost metric/table name from the first `Source` leaf.
    pub fn source_name(&self) -> Option<&str> {
        self.leaf_source().map(|s| s.name.as_str())
    }

    /// Whether the leftmost `Source` leaf carries a resolved schema — i.e. it is
    /// a SQL table (`Source::Table`). Time-series (PromQL) leaves return
    /// `false`. The converter uses this to keep the time-series fused-Partition
    /// canonical shape for PromQL while routing tabular GROUP BY through a
    /// positional `Aggregate.by` (so group keys land in the output schema).
    pub fn leaf_is_tabular(&self) -> bool {
        self.leaf_source().is_some_and(|s| s.schema.is_some())
    }
}
