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
use super::expr_ir::L3Expr;
use super::names::BindingName;
use super::schema::{Column, ColumnId, DataType, Schema};

/// Errors from schema derivation over a canonical tree.
#[derive(Debug, Error)]
pub enum QueryExprError {
    #[error("unresolved ref: {0}")]
    UnresolvedRef(String),
    #[error("by-column id {0} out of range (input has {1} columns)")]
    InvalidGroupByColumn(ColumnId, usize),
    #[error("Window requires a time_index on input schema")]
    WindowMissingTimeIndex,
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

            QueryExpr::Window { child, .. } => {
                let in_schema = child.output_schema_in(scope)?;
                if in_schema.time_index.is_none() {
                    return Err(QueryExprError::WindowMissingTimeIndex);
                }
                Ok(in_schema)
            }

            QueryExpr::Aggregate {
                by, aggs, child, ..
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
                for intent in aggs {
                    out_cols.push(intent.output_column(&probe));
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
            | QueryExpr::Subquery { child, .. }
            | QueryExpr::Project { child, .. } => child.output_schema_in(scope),

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
            QueryExpr::SetOp { left, .. } | QueryExpr::Join { left, .. } => {
                left.output_schema_in(scope)
            }
            QueryExpr::BinaryOp { lhs, .. } => lhs.output_schema_in(scope),
        }
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
