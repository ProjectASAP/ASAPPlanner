//! The Layer-2 relational IR — the per-language query algebra the parser
//! front ends emit, before [`convert_root`](crate::lower::convert_root) lowers
//! it to the canonical L3 [`query_expr::QueryExpr`](asap_ir::intent_algebra::query_expr::QueryExpr).
//!
//! Leaf / scalar types (`ColumnRef`, `SortKey`,
//! `BinaryOpKind`, `VectorMatch`) are owned by `query_expr` and re-used here so
//! there is one canonical spelling. Filter / having / project expressions use
//! the shared language-independent [`L3Expr`](asap_ir::intent_algebra::expr_ir::L3Expr).

use std::time::Duration;

pub use asap_ir::intent_algebra::expr_ir::{ColumnRef, L2Expr};
use asap_ir::intent_algebra::agg_intent::{MathFunc, TimeFunc};
pub use asap_ir::intent_algebra::query_expr::{BinaryOpKind, VectorMatch, WindowFuncKind};
use asap_ir::intent_algebra::schema::Schema;

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
    /// the columns); `None` for PromQL, where the [`Binder`](crate::binder)
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
    /// Output alias (`None` = use the intent's conventional name). Matches
    /// `L2ProjectItem.alias`'s convention — no `""` sentinel.
    pub alias: Option<String>,
    pub func: AggFunc,
    pub col: ColumnRef,
}

/// Layer-2 aggregate functions. Mapped to canonical [`AggIntent`] by
/// [`crate::lower::convert`].
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

    // ── Counter-derivative range functions (issue #44) ───────────────────
    // Per-series range reductions; the window rides on the enclosing L2
    // `Window` node (like `*_over_time`), so — unlike `Rate`/`Increase` —
    // these variants do NOT carry it.
    /// PromQL `changes(v[w])` → `AggIntent::Changes`.
    Changes,
    /// PromQL `delta(v[w])` → `AggIntent::Delta`.
    Delta,
    /// PromQL `idelta(v[w])` → `AggIntent::IDelta`.
    IDelta,
    /// PromQL `deriv(v[w])` → `AggIntent::Deriv`.
    Deriv,
    /// PromQL `resets(v[w])` → `AggIntent::Resets`.
    Resets,
    /// PromQL `predict_linear(v[w], t)` → `AggIntent::PredictLinear`.
    PredictLinear {
        seconds: f64,
    },
    /// PromQL `double_exponential_smoothing(v[w], sf, tf)` (a.k.a.
    /// `holt_winters`) → `AggIntent::DoubleExpSmoothing`.
    DoubleExpSmoothing {
        smoothing: f64,
        trend: f64,
    },

    // ── Native-histogram accessors (issue #43) ───────────────────────────
    /// PromQL `histogram_count(v)` → `AggIntent::HistogramCount`.
    HistogramCount,
    /// PromQL `histogram_sum(v)` → `AggIntent::HistogramSum`.
    HistogramSum,
    /// PromQL `histogram_avg(v)` → `AggIntent::HistogramAvg`.
    HistogramAvg,
    /// PromQL `histogram_stddev(v)` → `AggIntent::HistogramStdDev`.
    HistogramStdDev,
    /// PromQL `histogram_stdvar(v)` → `AggIntent::HistogramStdVar`.
    HistogramStdVar,
    /// PromQL `histogram_fraction(lower, upper, v)` → `AggIntent::HistogramFraction`.
    HistogramFraction {
        lower: f64,
        upper: f64,
    },
    /// PromQL classic `histogram_quantile(φ, <le-bucketed vector>)` →
    /// `AggIntent::HistogramQuantile`.
    HistogramQuantile(f64),
    /// PromQL element-wise math / trig transform (`abs`/`sqrt`/`clamp_max`/…) →
    /// `AggIntent::Math` (issue #45).
    Math(MathFunc),
    /// PromQL `absent` → `AggIntent::Absent` (issue #47).
    Absent,
    /// PromQL `absent_over_time` → `AggIntent::AbsentOverTime`.
    AbsentOverTime,
    /// PromQL `present_over_time` → `AggIntent::PresentOverTime`.
    PresentOverTime,
    /// PromQL time / calendar accessor (`timestamp`/`hour`/`day_of_week`/…) →
    /// `AggIntent::TimeFn` (issue #46).
    TimeFn(TimeFunc),
}

/// The Layer-2 relational query IR.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryExpr {
    /// A named metric stream or table — the outermost leaf.
    Source(SourceSpec),
    /// A scalar constant leaf — a PromQL number literal (or a folded constant
    /// scalar expression like `10*1024*1024`). Appears as a `BinaryOp` operand
    /// for `<vector> op <scalar>` thresholds / unit conversions (issue #35).
    Scalar(f64),
    /// The query evaluation time (`time()`), and the implicit input of the
    /// no-arg calendar functions (issue #46).
    EvalTime,
    /// `vector(s)` — scalar→instant-vector bridge (issue #48).
    VectorFromScalar(Box<QueryExpr>),
    /// `scalar(v)` — instant-vector→scalar bridge (issue #48).
    ScalarFromVector(Box<QueryExpr>),
    /// Reference to a CTE / let-binding by name. **Reserved**: no front end
    /// emits `Ref`/`LetBinding` yet (CSE runs on L3); the converter arm exists
    /// for forward-compatibility (e.g. PromQL recording rules).
    Ref(String),

    /// σ — row-level filter (WHERE / PromQL label matchers).
    Filter { pred: L2Expr, input: Box<QueryExpr> },

    /// π — projection / SELECT list (SQL). Column refs in `cols` resolve by
    /// name against the child schema during conversion.
    ///
    /// `qualifier` re-qualifies every output column with a table alias — set for
    /// a derived table / inline view (`FROM (SELECT …) t`), so an outer `t.col`
    /// reference resolves to *this* relation and a join over two derived tables
    /// disambiguates its keys. `None` for an ordinary SELECT list.
    Project {
        cols: Vec<L2ProjectItem>,
        qualifier: Option<String>,
        input: Box<QueryExpr>,
    },

    /// γ + α — GROUP BY (`keys`) followed by aggregate functions. Keys are
    /// `ColumnRef` (not bare strings) so a table-qualified key (`b.k`) resolves
    /// to the correct join side, matching the scalar-predicate path.
    Aggregate {
        keys: Vec<ColumnRef>,
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

    /// δ — deduplicate on `cols`.
    Distinct {
        cols: Vec<ColumnRef>,
        input: Box<QueryExpr>,
    },
    /// τ — heavy-hitter top-k. `by` are the grouping keys (qualified-capable).
    TopK {
        k: u64,
        by: Vec<ColumnRef>,
        input: Box<QueryExpr>,
    },
    /// ⊕ — merge sub-results from independent branches. **Reserved**: SQL
    /// `UNION` lowers to `SetOp`; no front end emits `Merge` yet (reserved for
    /// sharded / fan-in plans).
    Merge { inputs: Vec<QueryExpr> },

    Join {
        kind: asap_ir::intent_algebra::query_expr::JoinKind,
        pred: Option<L2Expr>,
        left: Box<QueryExpr>,
        right: Box<QueryExpr>,
    },
    SetOp {
        kind: asap_ir::intent_algebra::query_expr::SetOpKind,
        all: bool,
        left: Box<QueryExpr>,
        right: Box<QueryExpr>,
    },

    Sort {
        keys: Vec<L2SortKey>,
        /// Per-group ordering keys (`PARTITION BY`): a non-empty set means
        /// "rank within each group", the home for a generic (non-heavy-hitter)
        /// `topk by (…)` / `bottomk` grouping. Empty = a global order-by.
        /// Resolved to positional `Sort.partition_by` ColumnIds at L3.
        partition_by: Vec<ColumnRef>,
        input: Box<QueryExpr>,
    },
    Limit {
        n: u64,
        offset: u64,
        input: Box<QueryExpr>,
    },

    /// **Reserved**: see [`Ref`](Self::Ref) — no front end emits `LetBinding`
    /// yet (DAG fan-in / CSE is expressed on L3).
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
        partition_by: Vec<ColumnRef>,
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
            QueryExpr::Source(_) | QueryExpr::Scalar(_) | QueryExpr::EvalTime | QueryExpr::Ref(_) => {}
            QueryExpr::Filter { input, .. }
            | QueryExpr::Project { input, .. }
            | QueryExpr::Aggregate { input, .. }
            | QueryExpr::Window { input, .. }
            | QueryExpr::Distinct { input, .. }
            | QueryExpr::TopK { input, .. }
            | QueryExpr::Sort { input, .. }
            | QueryExpr::Limit { input, .. }
            | QueryExpr::WindowFunc { input, .. }
            | QueryExpr::VectorFromScalar(input)
            | QueryExpr::ScalarFromVector(input)
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
            | QueryExpr::Distinct { input, .. }
            | QueryExpr::TopK { input, .. }
            | QueryExpr::Sort { input, .. }
            | QueryExpr::Limit { input, .. }
            | QueryExpr::WindowFunc { input, .. }
            | QueryExpr::VectorFromScalar(input)
            | QueryExpr::ScalarFromVector(input)
            | QueryExpr::PromQLSubquery { input, .. } => input.leaf_source(),
            QueryExpr::Merge { inputs } => inputs.first()?.leaf_source(),
            QueryExpr::Join { left, .. }
            | QueryExpr::SetOp { left, .. }
            | QueryExpr::BinaryOp { lhs: left, .. } => left.leaf_source(),
            QueryExpr::LetBinding { body, .. } => body.leaf_source(),
            QueryExpr::Scalar(_) | QueryExpr::EvalTime | QueryExpr::Ref(_) => None,
        }
    }

    /// Outermost metric/table name from the first `Source` leaf.
    pub fn source_name(&self) -> Option<&str> {
        self.leaf_source().map(|s| s.name.as_str())
    }

    /// Whether the leftmost `Source` leaf carries a resolved schema — i.e. it is
    /// a SQL table (`Source::Table`). Time-series (PromQL) leaves return
    /// `false`. The converter uses this to keep the time-series fused per-series
    /// reduction shape for PromQL while routing tabular GROUP BY through a
    /// positional `Aggregate.by` (so group keys land in the output schema).
    pub fn leaf_is_tabular(&self) -> bool {
        self.leaf_source().is_some_and(|s| s.schema.is_some())
    }
}
