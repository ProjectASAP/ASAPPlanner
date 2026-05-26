//! The canonical Layer-3 intent algebra IR.
//!
//! Language- and deployment-independent. Box-owned tree (DAG fan-in is
//! expressed via `LetBinding` / `Ref`); column identity is **positional**
//! (`Aggregate.by: Vec<ColumnId>`), resolved by the [`Binder`](super::binder)
//! against the self-contained [`Schema`] carried on each `Scan`.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::agg_intent::AggIntent;
use super::expr_ir::{L3Expr, L3Scalar};
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

/// A column reference by name, or one of the two PromQL-conventional
/// synthetic columns.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnRef {
    Named(String),
    /// The implicit metric sample value (PromQL — always the series value).
    SampleValue,
    /// All rows / COUNT(*).
    Wildcard,
}

/// Grouping key set (`by (...)` / `without (...)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionKeys {
    By(Vec<String>),
    Without(Vec<String>),
}

impl PartitionKeys {
    pub fn keys(&self) -> &[String] {
        match self {
            PartitionKeys::By(k) | PartitionKeys::Without(k) => k,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.keys().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Atan2,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Unless,
    Like,
    NotLike,
    Regex,
    NotRegex,
}

impl std::fmt::Display for BinaryOpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BinaryOpKind::Add => "+",
            BinaryOpKind::Sub => "-",
            BinaryOpKind::Mul => "*",
            BinaryOpKind::Div => "/",
            BinaryOpKind::Mod => "%",
            BinaryOpKind::Pow => "^",
            BinaryOpKind::Atan2 => "atan2",
            BinaryOpKind::Eq => "==",
            BinaryOpKind::Ne => "!=",
            BinaryOpKind::Lt => "<",
            BinaryOpKind::Le => "<=",
            BinaryOpKind::Gt => ">",
            BinaryOpKind::Ge => ">=",
            BinaryOpKind::And => "AND",
            BinaryOpKind::Or => "OR",
            BinaryOpKind::Unless => "unless",
            BinaryOpKind::Like => "LIKE",
            BinaryOpKind::NotLike => "NOT LIKE",
            BinaryOpKind::Regex => "=~",
            BinaryOpKind::NotRegex => "!~",
        };
        f.write_str(s)
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
    /// Outermost leaf. `schema` is the authoritative, self-contained output
    /// schema (Binder-built); `predicates` are leaf-level row filters
    /// (PromQL label matchers, pushed-down `WHERE` conjuncts).
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
        child: Box<QueryExpr>,
    },

    /// γ + α — GROUP BY (positional) + aggregate intents.
    Aggregate {
        by: Vec<ColumnId>,
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

    /// Logical-only partitioning marker (sharding hint for L5).
    Partition {
        keys: PartitionKeys,
        child: Box<QueryExpr>,
    },
    /// δ — SQL `DISTINCT` / row deduplication.
    Distinct {
        cols: Vec<ColumnRef>,
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
    Sort {
        keys: Vec<SortKey>,
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

            // ψ — a window reshapes the time axis but not the column set. Over a
            // time-indexed Scan it preserves the time_index; over an Aggregate
            // (the canonical Window-over-Aggregate fused shape) the child has
            // already consumed the time axis, so the child schema passes through.
            QueryExpr::Window { child, .. } => child.output_schema_in(scope),

            QueryExpr::Aggregate {
                by,
                aggs,
                output_names,
                child,
                ..
            } => {
                let in_schema = child.output_schema_in(scope)?;
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
                    .unwrap_or(Column {
                        name: "value".into(),
                        dtype: DataType::Float64,
                        nullable: false,
                    });
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
            | QueryExpr::Partition { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::Subquery { child, .. } => child.output_schema_in(scope),

            // π — one output column per projection item. Each item's type is
            // inferred from its expression against the child schema; the name
            // is the explicit alias or a derived default. Projection may drop
            // the grouping/time columns, so unique_keys reset and time_index
            // is re-found by name.
            QueryExpr::Project { cols, child } => {
                let in_schema = child.output_schema_in(scope)?;
                let columns: Vec<Column> =
                    cols.iter()
                        .enumerate()
                        .map(|(i, item)| {
                            let (dtype, nullable) = infer_expr_type(&item.expr, &in_schema);
                            Column {
                                name: item.alias.clone().unwrap_or_else(|| {
                                    default_proj_name(&item.expr, i, &in_schema)
                                }),
                                dtype,
                                nullable,
                            }
                        })
                        .collect();
                let time_index = columns.iter().position(|c| c.name == "ts");
                Ok(Schema {
                    columns,
                    time_index,
                    unique_keys: Vec::new(),
                })
            }

            QueryExpr::Distinct { cols, child } => {
                let in_schema = child.output_schema_in(scope)?;
                let mut out = in_schema.clone();
                let mut key_ids: Vec<ColumnId> = Vec::with_capacity(cols.len());
                for c in cols {
                    if let ColumnRef::Named(name) = c {
                        if let Some(id) = in_schema.column_id(name) {
                            key_ids.push(id);
                        }
                    }
                }
                if !key_ids.is_empty() {
                    out.add_unique_key(key_ids);
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
                })
            }
            QueryExpr::BinaryOp { lhs, .. } => lhs.output_schema_in(scope),
        }
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
        Column {
            name: name.into(),
            dtype,
            nullable,
        }
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
