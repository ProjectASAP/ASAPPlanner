//! The canonical Layer-3 intent algebra IR.
//!
//! Language- and deployment-independent. Box-owned tree (DAG fan-in is
//! expressed via `LetBinding` / `Ref`); column identity is **positional**
//! (`Aggregate.by: GroupKeys`), resolved by the [`Binder`](super::binder)
//! against the self-contained [`Schema`] carried on each `Scan`.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::agg_intent::AggIntent;
use super::expr_ir::{ArithOp, CompareOp, L3Expr, L3Scalar};
use super::names::BindingName;
use super::schema::{Column, ColumnId, DataType, Schema};

/// Errors from schema derivation over a canonical tree.
#[derive(Debug, Error)]
pub enum QueryExprError {
    #[error("unresolved ref: {0}")]
    UnresolvedRef(String),
    #[error("by-column id {0} out of range (input has {1} columns)")]
    InvalidGroupByColumn(ColumnId, usize),
    #[error("Merge requires at least one child")]
    EmptyMerge,
}

// ── Leaf / supporting types ───────────────────────────────────────────────────

/// Positional grouping keys, shared by every "operate per group" L3 operator:
/// `Aggregate.by` (reduce per group), `Sort.partition_by` (rank per group —
/// including generic `topk`/`bottomk`), and `WindowFunc.partition_by` (window
/// per group). One spelling so grouping has a single home to evolve — e.g. a
/// future qualified-key or `without(...)` representation (issue #12). Empty =
/// no grouping (a global operation).
///
/// Heavy-hitter `AggIntent::TopK` carries its grouping here too, via the
/// enclosing `Aggregate.by` (issue #13) — so reduce, rank, and window groupings
/// all share this one type.
///
/// `#[serde(transparent)]` so it serialises as a bare array — wire-compatible
/// with the `Vec<ColumnId>` these fields held before.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupKeys(pub Vec<ColumnId>);

impl GroupKeys {
    /// An empty key set — a global (ungrouped) operation.
    pub fn none() -> Self {
        Self(Vec::new())
    }
}

impl std::ops::Deref for GroupKeys {
    type Target = [ColumnId];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Vec<ColumnId>> for GroupKeys {
    fn from(keys: Vec<ColumnId>) -> Self {
        Self(keys)
    }
}

impl FromIterator<ColumnId> for GroupKeys {
    fn from_iter<I: IntoIterator<Item = ColumnId>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a GroupKeys {
    type Item = &'a ColumnId;
    type IntoIter = std::slice::Iter<'a, ColumnId>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Compare directly against a `Vec<ColumnId>` so call sites and tests can keep
/// writing `keys == vec![..]` / `assert_eq!(keys, &vec![..])`.
impl PartialEq<Vec<ColumnId>> for GroupKeys {
    fn eq(&self, other: &Vec<ColumnId>) -> bool {
        &self.0 == other
    }
}

/// Lifecycle / flush semantics of a streaming time window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowKind {
    Tumbling,
    Sliding,
    Session,
}

/// Which data model a `Source` / `AggIntent` operates over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataModel {
    TimeSeries,
    Tabular,
    Any,
}

/// The leaf data source of a `Scan`. The schema itself rides on the
/// `Scan.schema` field (Binder-built); `Source` carries only the leaf's
/// identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// Time-series leaf — PromQL / DC lifecycle. Produces `(ts, value, *labels)`.
    TimeSeries { metric: String },
    /// Tabular leaf — asap-fusion / future OLAP. Columns ride on `Scan.schema`.
    Table { table_ref: String },
}

impl Source {
    pub fn data_model(&self) -> DataModel {
        match self {
            Source::TimeSeries { .. } => DataModel::TimeSeries,
            Source::Table { .. } => DataModel::Tabular,
        }
    }
}

/// Operator on the query-level `BinaryOp` node. Reuses the scalar IR's
/// [`ArithOp`] / [`CompareOp`] so every arithmetic/comparison operator has
/// exactly one representation (and one `Display`) across the IR; the remaining
/// variants are PromQL vector-set / power ops with no scalar-IR counterpart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOpKind {
    /// Arithmetic — `Add/Sub/Mul/Div/Mod` (shared with `L3Expr::Arith`).
    Arith(ArithOp),
    /// Comparison — `Eq/Ne/Lt/Le/Gt/Ge` + `Like/ILike/Regex` family (shared
    /// with `L3Expr::Compare`).
    Compare(CompareOp),
    /// PromQL logical-set intersection (`and`).
    And,
    /// PromQL logical-set union (`or`).
    Or,
    /// PromQL logical-set complement (`unless`).
    Unless,
    /// Exponentiation (`^`) — PromQL vector op, no scalar-IR counterpart.
    Pow,
    /// `atan2` — PromQL vector op, no scalar-IR counterpart.
    Atan2,
}

impl std::fmt::Display for BinaryOpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryOpKind::Arith(op) => write!(f, "{op}"),
            BinaryOpKind::Compare(op) => write!(f, "{op}"),
            BinaryOpKind::And => f.write_str("AND"),
            BinaryOpKind::Or => f.write_str("OR"),
            BinaryOpKind::Unless => f.write_str("unless"),
            BinaryOpKind::Pow => f.write_str("^"),
            BinaryOpKind::Atan2 => f.write_str("atan2"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

/// SQL analytic window function (`fn(...) OVER (…)`). Distinct from a streaming
/// time `Window`: this is an analytic frame over already-materialised rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFuncKind {
    RowNumber,
    Rank,
    DenseRank,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    /// `NTH_VALUE(expr, n)` — `n` is resolved from the (literal) 2nd argument.
    NthValue(Option<u64>),
    Sum,
    Avg,
    Count,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    pub expr: L3Expr,
    pub ascending: bool,
    pub nulls_first: bool,
}

/// PromQL vector-match modifier (`on`/`ignoring` + `group_left`/`group_right`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorMatch {
    pub kind: VectorMatchKind,
    pub labels: Vec<String>,
    pub grouping: Option<VectorGrouping>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VectorMatchKind {
    On,
    Ignoring,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorGrouping {
    pub side: GroupSide,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupSide {
    Left,
    Right,
}

/// A row-level filter predicate (WHERE clause / PromQL label matcher).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predicate(pub L3Expr);

/// One item in a SELECT projection list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectItem {
    pub alias: Option<String>,
    pub expr: L3Expr,
}

// ── L3 intent algebra IR ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueryExpr {
    /// Outermost leaf. `schema` is the **binding schema** — the resolved column
    /// set every positional `ColumnId` in the tree indexes into, *not* a full
    /// description of the runtime row. Complete when catalog-backed (SQL); for
    /// schemaless PromQL it is usage-derived by the [`Binder`](super::binder)
    /// (the `(ts, value)` floor + the labels the query references), since a
    /// metric's label set is open and known only at runtime. That distinction is
    /// carried explicitly by [`Schema::closed`](super::schema::Schema::closed)
    /// (SQL leaf → `true`, PromQL leaf → `false`). `predicates` are leaf-level
    /// row filters (PromQL label matchers, pushed-down `WHERE` conjuncts).
    Scan {
        source: Source,
        #[serde(default)]
        predicates: Vec<Predicate>,
        schema: Schema,
    },
    /// Reference to a `LetBinding` by name; resolved at plan time.
    Ref { name: BindingName },

    /// σ — row-level filter. Output schema = child schema.
    Filter {
        pred: Predicate,
        child: Box<QueryExpr>,
    },
    /// π — column projection.
    Project {
        cols: Vec<ProjectItem>,
        /// Re-qualifies every output column with this table alias (a derived
        /// table / inline view). `None` for an ordinary SELECT list. See
        /// [`relational::QueryExpr::Project`](super::relational).
        #[serde(default)]
        qualifier: Option<String>,
        child: Box<QueryExpr>,
    },

    /// γ + α — GROUP BY (positional) + aggregate intents.
    Aggregate {
        by: GroupKeys,
        aggs: Vec<AggIntent>,
        /// Output column names parallel to `aggs`. A non-empty entry overrides
        /// the synthetic intent-keyed name — SQL threads DataFusion's generated
        /// name (e.g. `"sum(metrics.bytes)"`) here so an enclosing `Project`
        /// resolves the aggregate output by the name it references. An empty
        /// entry (or empty vec) falls back to `AggIntent::output_column`'s name
        /// (PromQL's convention).
        #[serde(default)]
        output_names: Vec<String>,
        #[serde(default)]
        having: Option<Predicate>,
        child: Box<QueryExpr>,
    },

    /// ψ — tumbling / sliding / session window over the time axis. Window
    /// over Aggregate is the canonical windowed-aggregate shape.
    Window {
        kind: WindowKind,
        size: Duration,
        #[serde(default)]
        slide: Option<Duration>,
        child: Box<QueryExpr>,
    },

    /// δ — SQL `DISTINCT` / row deduplication. Positional like every other L3
    /// column reference; empty = dedup on all columns (`SELECT DISTINCT *`).
    Distinct {
        cols: Vec<ColumnId>,
        child: Box<QueryExpr>,
    },
    /// ⊕ — exact union of sub-results from independent stages / shards.
    Merge { children: Vec<QueryExpr> },

    /// Logical join. L4 picks the physical alternative.
    Join {
        kind: JoinKind,
        pred: Predicate,
        left: Box<QueryExpr>,
        right: Box<QueryExpr>,
    },
    SetOp {
        kind: SetOpKind,
        all: bool,
        left: Box<QueryExpr>,
        right: Box<QueryExpr>,
    },

    /// Generic order-by for non-heavy-hitter cases.
    ///
    /// `partition_by` makes the ordering **per-group**: a non-empty set means
    /// "rank within each `partition_by` group" — the semantics behind PromQL
    /// `topk by (host) (…)` / SQL `… OVER (PARTITION BY host ORDER BY …)`. It is
    /// row-preserving (schema pass-through) and is where the grouping of a
    /// generic (non-heavy-hitter) ranking lives, so there is no separate
    /// `Partition` node (issue #12: reducing GROUP BY → `Aggregate.by`, per-group
    /// ranking → here, parallel sharding → L5). Empty = a global order-by.
    Sort {
        keys: Vec<SortKey>,
        #[serde(default)]
        partition_by: GroupKeys,
        child: Box<QueryExpr>,
    },
    Limit {
        n: usize,
        offset: usize,
        child: Box<QueryExpr>,
    },

    /// SQL `WITH name AS (expr) … child`; PromQL recording-rule binding.
    LetBinding {
        name: BindingName,
        expr: Box<QueryExpr>,
        child: Box<QueryExpr>,
    },

    /// PromQL sub-query (`<expr>[range:resolution]`). Logical pass-through.
    Subquery {
        range: Duration,
        #[serde(default)]
        resolution: Option<Duration>,
        child: Box<QueryExpr>,
    },

    /// Temporal range selection — "look back `range` of history for this
    /// computation." Used for all range-vector functions: `rate`, `increase`,
    /// `*_over_time`. The range is distinct from both a streaming `Window`
    /// (which is for query-repetition) and a row-level `Filter`.
    ///
    /// Structural marker: an `Aggregate` whose direct child is a `TimeRange`
    /// is a *per-series* reduction (label-preserving); one whose child is a
    /// plain `Scan` or another `Aggregate` is a *cross-series* reduction.
    TimeRange {
        range: Duration,
        child: Box<QueryExpr>,
    },

    /// SQL analytic window function: `func(args) OVER (PARTITION BY … ORDER BY …)`.
    /// Output schema = child schema + one column named `output_name` (the name
    /// the enclosing `Project` references). Window frames are not modelled yet.
    WindowFunc {
        func: WindowFuncKind,
        /// Operand expressions (`LAG(value)` → `[Column(value_id)]`); empty for
        /// the rank-only functions (`ROW_NUMBER`/`RANK`/`DENSE_RANK`).
        args: Vec<L3Expr>,
        partition_by: GroupKeys,
        order_by: Vec<SortKey>,
        /// The output column's name — DataFusion's window-expr field name, so a
        /// `Project` above resolves it (cf. `Aggregate.output_names`).
        output_name: String,
        child: Box<QueryExpr>,
    },

    /// Arithmetic / comparison / boolean composition (PromQL binary ops).
    BinaryOp {
        op: BinaryOpKind,
        lhs: Box<QueryExpr>,
        rhs: Box<QueryExpr>,
        #[serde(default)]
        vector_match: Option<VectorMatch>,
    },
}

impl QueryExpr {
    /// Output schema of the root of a single query (empty binding scope).
    pub fn output_schema(&self) -> Result<Schema, QueryExprError> {
        self.output_schema_in(&BindingScope::default())
    }

    /// Output schema given an explicit `LetBinding` scope.
    pub fn output_schema_in(&self, scope: &BindingScope) -> Result<Schema, QueryExprError> {
        match self {
            QueryExpr::Scan { schema, .. } => Ok(schema.clone()),

            // ψ — streaming window (tumbling / sliding / session) for query
            // repetition. Does not change the column schema; passes through.
            // Per-series range reductions (`rate`, `*_over_time`) now use the
            // `TimeRange` node instead, so this arm is a simple pass-through.
            QueryExpr::Window { child, .. } => child.output_schema_in(scope),

            QueryExpr::Aggregate {
                by,
                aggs,
                output_names,
                child,
                ..
            } => {
                let in_schema = child.output_schema_in(scope)?;

                // Per-series range reduction: `rate`/`increase` (is_per_series)
                // OR any single aggregate whose direct child is a `TimeRange`
                // (`*_over_time` functions) or a `Subquery` (`*_over_time` over a
                // sub-query, e.g. `max_over_time(rate(m[5m])[1h:])`). All produce
                // one value per series and are label-preserving — the range child
                // is the structural marker that confers per-series semantics on
                // otherwise cross-series intents like `Avg`/`Sum`/`Count`. A
                // cross-series aggregation operator over a range vector is a
                // PromQL type error the parser rejects, so an `Aggregate` over a
                // `Subquery` is only ever this per-series `*_over_time` shape.
                let is_range_child = matches!(
                    child.as_ref(),
                    QueryExpr::TimeRange { .. } | QueryExpr::Subquery { .. }
                );
                if by.is_empty() && aggs.len() == 1 && (aggs[0].is_per_series() || is_range_child) {
                    return Ok(per_series_reduction_schema(&in_schema, &aggs[0]));
                }

                let mut out_cols: Vec<Column> = Vec::with_capacity(by.len() + aggs.len());
                for &id in by {
                    let c =
                        in_schema
                            .columns
                            .get(id)
                            .ok_or(QueryExprError::InvalidGroupByColumn(
                                id,
                                in_schema.columns.len(),
                            ))?;
                    out_cols.push(c.clone());
                }
                let value_col_idx = in_schema
                    .column_id("value")
                    .or_else(|| (0..in_schema.columns.len()).find(|i| !by.contains(i)));
                let probe = value_col_idx
                    .and_then(|i| in_schema.columns.get(i))
                    .cloned()
                    .unwrap_or_else(|| Column::new("value", DataType::Float64, false));
                // Each reducer types off its own input column (`SUM(bytes)` vs
                // `AVG(latency)` in one node); `None` falls back to the sample-
                // value probe (PromQL's single-column convention). A non-empty
                // `output_names[i]` overrides the synthetic output column name.
                for (i, intent) in aggs.iter().enumerate() {
                    let in_col = intent
                        .input_col()
                        .and_then(|id| in_schema.columns.get(id))
                        .unwrap_or(&probe);
                    let mut out = intent.output_column(in_col);
                    if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
                        out.name = name.clone();
                    }
                    out_cols.push(out);
                }
                let unique_keys = if by.is_empty() {
                    Vec::new()
                } else {
                    vec![(0..by.len()).collect()]
                };
                Ok(Schema {
                    columns: out_cols,
                    time_index: None,
                    unique_keys,
                    // A cross-series aggregate enumerates exactly `by ++ aggs`,
                    // so its output is closed even over an open input — this is
                    // where an open schema freezes to closed.
                    closed: true,
                })
            }

            QueryExpr::LetBinding { name, expr, child } => {
                let bound = expr.output_schema_in(scope)?;
                let extended = scope.with(name.clone(), bound);
                child.output_schema_in(&extended)
            }
            QueryExpr::Ref { name } => scope
                .lookup(name)
                .cloned()
                .ok_or_else(|| QueryExprError::UnresolvedRef(name.as_str().into())),

            QueryExpr::Filter { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::Subquery { child, .. }
            | QueryExpr::TimeRange { child, .. } => child.output_schema_in(scope),

            // π — one output column per projection item. Each item's type is
            // inferred from its expression against the child schema; the name
            // is the explicit alias or a derived default. Projection may drop
            // the grouping/time columns, so unique_keys reset and time_index
            // is re-found by name.
            QueryExpr::Project { cols, qualifier, child } => {
                let in_schema = child.output_schema_in(scope)?;
                let columns: Vec<Column> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let (dtype, nullable) = infer_expr_type(&item.expr, &in_schema);
                        let name = item
                            .alias
                            .clone()
                            .unwrap_or_else(|| default_proj_name(&item.expr, i, &in_schema));
                        let c = Column::new(name, dtype, nullable);
                        // A derived table re-qualifies its output columns with
                        // its alias, so `t.col` (and a join over two derived
                        // tables) resolves to the right relation.
                        match qualifier {
                            Some(q) => c.with_table(q),
                            None => c,
                        }
                    })
                    .collect();
                let time_index = columns.iter().position(|c| c.name == "ts");
                Ok(Schema {
                    columns,
                    time_index,
                    unique_keys: Vec::new(),
                    // Projection enumerates exactly its items → closed.
                    closed: true,
                })
            }

            QueryExpr::Distinct { cols, child } => {
                let mut out = child.output_schema_in(scope)?;
                // Deduplicating on `cols` makes them a unique key of the result.
                if !cols.is_empty() {
                    out.add_unique_key(cols.clone());
                }
                Ok(out)
            }

            QueryExpr::Merge { children } => children
                .first()
                .ok_or(QueryExprError::EmptyMerge)
                .and_then(|c| c.output_schema_in(scope)),
            // Set operations are union-compatible: both sides share the left's
            // column shape, so the output schema is the left's. (Row identity
            // is not preserved across a UNION, so unique_keys are dropped.)
            QueryExpr::SetOp { left, .. } => {
                let mut s = left.output_schema_in(scope)?;
                s.unique_keys.clear();
                Ok(s)
            }
            // ⋈ — output is the concatenation of both inputs' columns. Outer
            // joins make the non-preserved side nullable. Post-join row
            // identity isn't provable in general, so unique_keys reset.
            QueryExpr::Join {
                kind, left, right, ..
            } => {
                let l = left.output_schema_in(scope)?;
                let r = right.output_schema_in(scope)?;
                let (left_null, right_null) = match kind {
                    JoinKind::Left => (false, true),
                    JoinKind::Right => (true, false),
                    JoinKind::Full => (true, true),
                    JoinKind::Inner | JoinKind::Cross => (false, false),
                };
                let l_len = l.columns.len();
                let mut columns = Vec::with_capacity(l_len + r.columns.len());
                columns.extend(l.columns.iter().cloned().map(|mut c| {
                    c.nullable |= left_null;
                    c
                }));
                columns.extend(r.columns.iter().cloned().map(|mut c| {
                    c.nullable |= right_null;
                    c
                }));
                let time_index = l.time_index.or(r.time_index.map(|i| i + l_len));
                Ok(Schema {
                    columns,
                    time_index,
                    unique_keys: Vec::new(),
                    // The concatenation is complete only if both sides are.
                    closed: l.closed && r.closed,
                })
            }
            // ψ-analytic — child schema + one appended window-output column.
            QueryExpr::WindowFunc {
                func,
                args,
                output_name,
                child,
                ..
            } => {
                let mut out = child.output_schema_in(scope)?;
                // First operand's (dtype, nullable) from the child schema, owned
                // so the borrow ends before we append.
                let arg = args.first().and_then(|a| match a {
                    L3Expr::Column(id) => out.columns.get(*id),
                    _ => None,
                });
                let arg_dtype = || arg.map_or(DataType::Float64, |c| c.dtype.clone());
                let (dtype, nullable) = match func {
                    WindowFuncKind::RowNumber
                    | WindowFuncKind::Rank
                    | WindowFuncKind::DenseRank
                    | WindowFuncKind::Count => (DataType::Int64, false),
                    WindowFuncKind::Sum | WindowFuncKind::Avg => (DataType::Float64, true),
                    // Navigation funcs: arg type, nullable (boundary rows are NULL).
                    WindowFuncKind::Lag
                    | WindowFuncKind::Lead
                    | WindowFuncKind::FirstValue
                    | WindowFuncKind::LastValue
                    | WindowFuncKind::NthValue(_) => (arg_dtype(), true),
                    WindowFuncKind::Min | WindowFuncKind::Max => {
                        (arg_dtype(), arg.is_none_or(|c| c.nullable))
                    }
                };
                out.columns
                    .push(Column::new(output_name.clone(), dtype, nullable));
                Ok(out)
            }

            QueryExpr::BinaryOp { lhs, .. } => lhs.output_schema_in(scope),
        }
    }
}

/// Output schema of a *per-series* window/range reduction (`rate`/`increase`,
/// or an `*_over_time` reducer under a time `Window`). Such a reduction emits
/// one value per series, so every label column of `input` is preserved and only
/// the sample value is replaced — kept named `value` so the PromQL sample-value
/// convention (and any outer `SampleValue` reference) still resolves it by name.
fn per_series_reduction_schema(input: &Schema, agg: &AggIntent) -> Schema {
    let value_idx = input
        .column_id("value")
        .or_else(|| (0..input.columns.len()).find(|&i| Some(i) != input.time_index));
    let mut columns = input.columns.clone();
    if let Some(vi) = value_idx {
        let mut out = agg.output_column(&columns[vi]);
        out.name = "value".into();
        columns[vi] = out;
    }
    Schema {
        columns,
        time_index: input.time_index,
        unique_keys: input.unique_keys.clone(),
        // Per-series reduction is label-preserving: it inherits its input's
        // completeness (an open scan stays open; a closed one stays closed).
        closed: input.closed,
    }
}

/// Infer the `(DataType, nullable)` a scalar [`L3Expr`] produces against an
/// input [`Schema`]. Used by `Project` schema derivation. Approximate at L3:
/// unknown columns and bare `FunctionCall`s fall back to a permissive default
/// (the L4/emit layer refines with a real function/type registry).
fn infer_expr_type(expr: &L3Expr, schema: &Schema) -> (DataType, bool) {
    match expr {
        L3Expr::Column(id) => schema
            .columns
            .get(*id)
            .map(|c| (c.dtype.clone(), c.nullable))
            .unwrap_or((DataType::Float64, true)),
        L3Expr::Literal(s) => match s {
            L3Scalar::Int64(_) => (DataType::Int64, false),
            L3Scalar::Float64(_) => (DataType::Float64, false),
            L3Scalar::Utf8(_) => (DataType::Utf8, false),
            L3Scalar::Boolean(_) => (DataType::Bool, false),
            L3Scalar::Null => (DataType::Float64, true),
        },
        // Boolean-valued expressions (SQL three-valued logic → nullable).
        L3Expr::Compare { .. }
        | L3Expr::BoolAnd(_)
        | L3Expr::BoolOr(_)
        | L3Expr::Not(_)
        | L3Expr::IsNull(_)
        | L3Expr::IsNotNull(_)
        | L3Expr::InList { .. } => (DataType::Bool, true),
        L3Expr::Arith { left, right, .. } => {
            let (lt, ln) = infer_expr_type(left, schema);
            let (rt, rn) = infer_expr_type(right, schema);
            let dtype = if matches!(lt, DataType::Int64) && matches!(rt, DataType::Int64) {
                DataType::Int64
            } else {
                DataType::Float64
            };
            (dtype, ln || rn)
        }
        L3Expr::Cast { to, try_cast, expr } => {
            let (_, nullable) = infer_expr_type(expr, schema);
            (to.clone(), *try_cast || nullable)
        }
        // No function/type registry at L3 — default permissive.
        L3Expr::FunctionCall { .. } => (DataType::Float64, true),
        L3Expr::Case {
            branches,
            else_expr,
            ..
        } => branches
            .first()
            .map(|(_, then)| (infer_expr_type(then, schema).0, true))
            .or_else(|| else_expr.as_ref().map(|e| infer_expr_type(e, schema)))
            .unwrap_or((DataType::Float64, true)),
    }
}

/// Default output-column name for a projection item with no explicit alias:
/// a bare column keeps its (schema) name; anything else gets `col_{i}`.
fn default_proj_name(expr: &L3Expr, idx: usize, schema: &Schema) -> String {
    match expr {
        L3Expr::Column(id) => schema
            .columns
            .get(*id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("col_{idx}")),
        _ => format!("col_{idx}"),
    }
}

/// Lexical scope for `LetBinding` / `Ref` resolution.
#[derive(Debug, Default, Clone)]
pub struct BindingScope {
    bindings: HashMap<String, Schema>,
}

impl BindingScope {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(&self, name: BindingName, schema: Schema) -> Self {
        let mut bindings = self.bindings.clone();
        bindings.insert(name.as_str().into(), schema);
        Self { bindings }
    }
    pub fn lookup(&self, name: &BindingName) -> Option<&Schema> {
        self.bindings.get(name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent_algebra::expr_ir::{ArithOp, CompareOp};

    fn col(name: &str, dtype: DataType, nullable: bool) -> Column {
        Column::new(name, dtype, nullable)
    }

    fn scan(
        columns: Vec<Column>,
        time_index: Option<ColumnId>,
        uk: Vec<Vec<ColumnId>>,
    ) -> QueryExpr {
        QueryExpr::Scan {
            source: Source::Table {
                table_ref: "t".into(),
            },
            predicates: vec![],
            schema: Schema {
                columns,
                time_index,
                unique_keys: uk,
                closed: true,
            },
        }
    }

    #[test]
    fn project_retypes_and_renames_per_item() {
        let child = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("host", DataType::Utf8, false),
                col("value", DataType::Float64, false),
            ],
            Some(0),
            vec![vec![0, 1]],
        );
        let q = QueryExpr::Project {
            qualifier: None,
            cols: vec![
                // bare column passthrough keeps its (schema) name + type: host=col 1
                ProjectItem {
                    alias: None,
                    expr: L3Expr::Column(1),
                },
                // arithmetic over value (col 2) → Float64
                ProjectItem {
                    alias: Some("dbl".into()),
                    expr: L3Expr::Arith {
                        op: ArithOp::Add,
                        left: Box::new(L3Expr::Column(2)),
                        right: Box::new(L3Expr::Column(2)),
                    },
                },
                // comparison → Bool (nullable under 3-valued logic)
                ProjectItem {
                    alias: Some("flag".into()),
                    expr: L3Expr::Compare {
                        left: Box::new(L3Expr::Column(2)),
                        op: CompareOp::Gt,
                        right: Box::new(L3Expr::Literal(L3Scalar::Float64(0.0))),
                    },
                },
            ],
            child: Box::new(child),
        };
        let s = q.output_schema().unwrap();
        assert_eq!(s.columns.len(), 3);
        assert_eq!(s.columns[0], col("host", DataType::Utf8, false));
        assert_eq!(s.columns[1], col("dbl", DataType::Float64, false));
        assert_eq!(s.columns[2], col("flag", DataType::Bool, true));
        // projection drops the time axis + unique keys (ts not retained)
        assert!(s.time_index.is_none());
        assert!(s.unique_keys.is_empty());
    }

    #[test]
    fn per_series_rate_preserves_labels() {
        // A per-series range reduction (`rate`) is label-preserving: it produces
        // one value per series, so every label survives and only the sample
        // value is replaced (kept named `value`). The TimeRange child is the
        // structural marker; the outer Aggregate carries the Rate intent.
        let scan_node = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("job", DataType::Utf8, true),
            ],
            Some(0),
            vec![],
        );
        let rate = QueryExpr::Aggregate {
            by: vec![].into(),
            aggs: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: Box::new(QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Box::new(scan_node),
            }),
        };
        let s = rate.output_schema().unwrap();
        assert_eq!(
            s.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ts", "value", "job"],
            "rate preserves all labels; only the sample value is replaced"
        );
        assert_eq!(s.time_index, Some(0));
        assert!(s.column_id("job").is_some(), "label survives the reduction");
    }

    #[test]
    fn over_time_reduction_preserves_labels() {
        // `*_over_time` lowers to `Aggregate { by:[], [reducer], TimeRange { Scan } }`:
        // a per-series time-range reduction. The TimeRange child confers per-series
        // semantics on otherwise cross-series intents like `Avg`, so an outer
        // `sum by(job)(avg_over_time(...))` resolves its key positionally.
        let scan_node = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("job", DataType::Utf8, true),
            ],
            Some(0),
            vec![],
        );
        let avg_over_time = QueryExpr::Aggregate {
            by: vec![].into(),
            aggs: vec![AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Box::new(scan_node),
            }),
        };
        let s = avg_over_time.output_schema().unwrap();
        assert_eq!(
            s.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ts", "value", "job"],
            "TimeRange-child marks per-series: labels preserved, value renamed"
        );
        assert!(
            s.column_id("job").is_some(),
            "outer Aggregate.by can resolve it"
        );
    }

    #[test]
    fn completeness_open_leaf_freezes_to_closed_at_cross_series_aggregate() {
        // A schemaless (PromQL-style) leaf is *open*; it stays open through a
        // per-series reduction (`rate`), then is **frozen to closed** by a
        // cross-series aggregate (which enumerates exactly its output columns).
        let open_leaf = QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            // `with_time_index` defaults to `closed: false` (open).
            schema: Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp, false),
                    col("value", DataType::Float64, false),
                    col("job", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        };
        assert!(
            !open_leaf.output_schema().unwrap().closed,
            "schemaless leaf is open"
        );

        let rate = QueryExpr::Aggregate {
            by: vec![].into(),
            aggs: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: Box::new(open_leaf),
        };
        assert!(
            !rate.output_schema().unwrap().closed,
            "per-series rate is label-preserving → stays open"
        );

        let sum_by_job = QueryExpr::Aggregate {
            by: vec![2].into(), // `job`
            aggs: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(rate),
        };
        assert!(
            sum_by_job.output_schema().unwrap().closed,
            "cross-series aggregate enumerates `by ++ aggs` → frozen to closed"
        );
    }

    #[test]
    fn project_keeps_time_index_when_ts_passed_through() {
        let child = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
            ],
            Some(0),
            vec![],
        );
        let q = QueryExpr::Project {
            qualifier: None,
            cols: vec![
                // value=col 1, ts=col 0
                ProjectItem {
                    alias: None,
                    expr: L3Expr::Column(1),
                },
                ProjectItem {
                    alias: None,
                    expr: L3Expr::Column(0),
                },
            ],
            child: Box::new(child),
        };
        let s = q.output_schema().unwrap();
        assert_eq!(s.columns[0].name, "value");
        assert_eq!(s.columns[1].name, "ts");
        assert_eq!(s.time_index, Some(1));
    }

    fn join(kind: JoinKind) -> QueryExpr {
        let left = scan(vec![col("a", DataType::Int64, false)], None, vec![vec![0]]);
        let right = scan(vec![col("b", DataType::Utf8, false)], None, vec![]);
        QueryExpr::Join {
            kind,
            pred: Predicate(L3Expr::Literal(L3Scalar::Boolean(true))),
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    #[test]
    fn inner_join_concatenates_both_sides() {
        let s = join(JoinKind::Inner).output_schema().unwrap();
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.columns[0], col("a", DataType::Int64, false));
        assert_eq!(s.columns[1], col("b", DataType::Utf8, false));
        // post-join row identity not provable → no unique keys
        assert!(s.unique_keys.is_empty());
    }

    #[test]
    fn left_join_makes_right_side_nullable() {
        let s = join(JoinKind::Left).output_schema().unwrap();
        assert!(!s.columns[0].nullable, "preserved left side stays non-null");
        assert!(s.columns[1].nullable, "right side nullable under LEFT JOIN");
    }

    #[test]
    fn full_join_makes_both_sides_nullable() {
        let s = join(JoinKind::Full).output_schema().unwrap();
        assert!(s.columns[0].nullable);
        assert!(s.columns[1].nullable);
    }

    #[test]
    fn setop_takes_left_shape_and_drops_unique_keys() {
        let left = scan(
            vec![
                col("k", DataType::Utf8, false),
                col("v", DataType::Int64, false),
            ],
            None,
            vec![vec![0]],
        );
        let right = scan(
            vec![
                col("k", DataType::Utf8, false),
                col("v", DataType::Int64, false),
            ],
            None,
            vec![vec![0]],
        );
        let q = QueryExpr::SetOp {
            kind: SetOpKind::Union,
            all: false,
            left: Box::new(left),
            right: Box::new(right),
        };
        let s = q.output_schema().unwrap();
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.columns[0].name, "k");
        assert!(
            s.unique_keys.is_empty(),
            "UNION does not preserve row identity"
        );
    }
}
