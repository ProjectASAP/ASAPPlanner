use std::sync::Arc;
use std::time::Duration;

use super::expr_ir::L3Expr;
use super::schema::{HasSchema, L3Schema, SchemaCatalog};
use crate::types::AccuracyTarget;

// ── Leaf / supporting types ───────────────────────────────────────────────────

/// A row-level filter predicate (WHERE clause / PromQL label matcher).
#[derive(Debug, Clone)]
pub struct Predicate(pub L3Expr);

/// One item in a SELECT projection list.
#[derive(Debug, Clone)]
pub struct ProjectItem {
    pub expr: L3Expr,
    pub alias: Option<String>,
}

/// A GROUP BY key reference (column name).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKey(pub String);
/// A reference to a column by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnRef(pub String);

/// A set of partitioning keys (sharding hint for L5 stage allocator).
#[derive(Debug, Clone)]
pub struct PartitionKeys;

/// One key in an ORDER BY or window OVER clause.
#[derive(Debug, Clone)]
pub struct SortKey {
    pub expr: L3Expr,
    pub ascending: bool,
    pub nulls_first: bool,
}
/// An analytic window frame (ROWS / RANGE BETWEEN …).
#[derive(Debug, Clone)]
pub struct WindowFrame;
/// PromQL vector-match modifiers (`on`/`ignoring` + `group_left`/`group_right`).
#[derive(Debug, Clone)]
pub struct VectorMatch;
/// Reference to a metric by name (PromQL / OTLP).
#[derive(Debug, Clone)]
pub struct MetricRef;
/// Closed time interval [start_ms, end_ms] in milliseconds since Unix epoch.
/// Either bound may be `None`, meaning unbounded on that side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeRange {
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
}
/// Label matchers applied to a time-series scan.
#[derive(Debug, Clone)]
pub struct LabelFilter;
/// Reference to a relational table by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef(pub String);
/// Join key specification (USING / ON column reference).
#[derive(Debug, Clone)]
pub struct JoinKey;

// ── Enum supporting types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOpKind {
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    // Comparison
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    // Boolean / PromQL set operators
    And,
    Or,
    Unless,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowFuncKind {
    RowNumber,
    Rank,
    DenseRank,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue(Option<u64>),
    Sum,
    Avg,
    Count,
    Min,
    Max,
}

/// Which data model a `Source` or `AggIntent` operates over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataModel {
    TimeSeries,
    Tabular,
    /// Agnostic — works over either data model.
    Any,
}

// ── Time window kind ──────────────────────────────────────────────────────────

/// The lifecycle / flush semantics of a streaming time window.
/// Used by `QueryExpr::TimeWindow`; distinct from SQL analytic frames
/// (`QueryExpr::WindowFunc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeWindowKind {
    /// Non-overlapping fixed-size windows.
    Tumbling,
    /// Overlapping windows advancing by `slide` interval.
    Sliding,
    /// Windows that open on activity and close after a gap of inactivity.
    Session,
}

// ── Leaf data source ──────────────────────────────────────────────────────────

/// The leaf data source of a query. Carried by `QueryExpr::Scan` to keep
/// L3 data-model-agnostic: everything above `Scan` (`Filter`, `Aggregate`,
/// etc.) works identically regardless of `Source` variant.
#[derive(Debug, Clone)]
pub enum Source {
    /// Time-series input — deployment-model-asapquery / asaplifecycle shape.
    TimeSeries {
        metric: MetricRef,
        time: TimeRange,
        labels: LabelFilter,
    },
    /// Tabular input — deployment-model-asapfusion / future-OLAP shape.
    Table {
        table_ref: TableRef,
        columns: Vec<ColumnRef>,
        /// Time range extracted from a WHERE predicate on the table's designated
        /// time column. `None` means no time bound (full-history scan).
        time_range: Option<TimeRange>,
    },
    /// Recursive join over sources (multi-table tabular queries).
    Join {
        left: Box<Source>,
        right: Box<Source>,
        on: JoinKey,
    },
}

impl Source {
    pub fn data_model(&self) -> DataModel {
        match self {
            Source::TimeSeries { .. } => DataModel::TimeSeries,
            Source::Table { .. } | Source::Join { .. } => DataModel::Tabular,
        }
    }
}

// ── Aggregation intent ────────────────────────────────────────────────────────

/// What to compute, not how. Sketch type and parameters are chosen by L4
/// rules; `AggIntent` is the L3 statement of intent only.
///
/// Heavy-hitter top-k (`TopK`) is a first-class intent because dedicated
/// sketch primitives (SpaceSaving, CMS-with-heap) compute it in one pass.
/// Generic ordering+limit stays as `QueryExpr::Sort + QueryExpr::Limit`.
#[derive(Debug, Clone)]
pub enum AggIntent {
    // ── Data-model-agnostic ───────────────────────────────────────────────────
    Count {
        accuracy: AccuracyTarget,
    },
    Sum {
        col: Option<ColumnRef>,
    },
    Min {
        col: Option<ColumnRef>,
    },
    Max {
        col: Option<ColumnRef>,
    },
    Avg {
        col: Option<ColumnRef>,
    },
    /// Sample stddev when `population == false`; population stddev otherwise.
    Stddev {
        col: Option<ColumnRef>,
        population: bool,
    },
    Quantile {
        q: f64,
        accuracy: AccuracyTarget,
    },
    /// Heavy-hitter top-k. Distinct from generic `Sort + Limit` — a
    /// dedicated sketch (SpaceSaving, CMS-with-heap) computes it as a single
    /// primitive. Recognised by L1→L2→L3 lowering on `ORDER BY count DESC
    /// LIMIT k` / PromQL `topk(k, …)`.
    TopK {
        k: usize,
        by: Vec<ColumnRef>,
        accuracy: AccuracyTarget,
    },
    Cardinality {
        accuracy: AccuracyTarget,
    },

    // ── Time-series streaming derivatives ────────────────────────────────────
    // Include PromQL counter-reset adjustment; not equivalent to Sum/Count
    // over a Window. Kept distinct so delta-set aggregators bind directly.
    Rate {
        window: Duration,
    },
    Increase {
        window: Duration,
    },
}

impl AggIntent {
    /// Which data model this intent semantically requires. L4 rules consult
    /// this to skip non-applicable intents (e.g. `Rate` over a `Source::Table`).
    pub fn requires(&self) -> DataModel {
        match self {
            Self::Rate { .. } | Self::Increase { .. } => DataModel::TimeSeries,
            _ => DataModel::Any,
        }
    }

    /// The column this intent aggregates, if tracked. Used by schema derivation
    /// to resolve the actual field type (e.g. so `MIN(ts: Int64)` → `Int64`).
    /// Returns `None` for intents with no specific column (`Count`, `Cardinality`,
    /// `TopK`, ...) and for the `"*"` wildcard sentinel produced by `agg_col`
    /// when the aggregate arg is not a named column.
    pub fn col(&self) -> Option<&ColumnRef> {
        match self {
            Self::Sum { col }
            | Self::Min { col }
            | Self::Max { col }
            | Self::Avg { col }
            | Self::Stddev { col, .. } => col.as_ref(),
            _ => None,
        }
    }

    /// Output column type for a single-column aggregate result.
    ///
    /// Returns `None` for `TopK`, which produces multiple output columns.
    /// Use `QueryExpr::output_schema` for TopK schema derivation instead.
    pub fn output_type(&self, input: &super::schema::L3Field) -> Option<super::schema::L3DataType> {
        use super::schema::L3DataType;
        match self {
            Self::Count { .. } | Self::Cardinality { .. } => Some(L3DataType::Int64),
            Self::Min { .. } | Self::Max { .. } => Some(input.dtype.clone()),
            Self::Sum { .. }
            | Self::Avg { .. }
            | Self::Stddev { .. }
            | Self::Quantile { .. }
            | Self::Rate { .. }
            | Self::Increase { .. } => Some(L3DataType::Float64),
            Self::TopK { .. } => None,
        }
    }
}

// ── L3 DAG node ───────────────────────────────────────────────────────────────

/// A node in the L3 DAG. Wraps the expression and its derived output schema
/// so that every edge implicitly carries a typed schema: holding an
/// `Arc<L3Node>` gives you both the child expression and the schema of the
/// data flowing on that edge.
#[derive(Debug, Clone)]
pub struct L3Node {
    pub expr: QueryExpr,
    /// Output schema of `expr` — the schema of the data flowing on the edge
    /// leading *from* this node to its parent(s).
    pub schema: L3Schema,
}

// ── L3 intent algebra IR ──────────────────────────────────────────────────────

/// Language- and deployment-independent intent-only IR. No sketch types,
/// no sketch parameters, no language-specific operators. Traversing from
/// the root node yields a DAG; shared sub-expressions appear as multiple
/// `Rc` references to the same `L3Node`.
#[derive(Debug, Clone)]
pub enum QueryExpr {
    // ── Base relations ────────────────────────────────────────────────────────
    /// Outermost leaf. `source` carries the data-model-specific leaf shape.
    Scan {
        source: Source,
        predicates: Vec<Predicate>,
    },
    /// Reference to a named `LetBinding` sub-expression; resolved at plan time.
    Ref(String),

    // ── Filtering & projection ────────────────────────────────────────────────
    /// σ — row-level filter. Output schema = child schema (unchanged).
    Filter {
        child: Arc<L3Node>,
        pred: Predicate,
    },
    /// π — column projection. Output schema = child schema projected to `cols`.
    Project {
        child: Arc<L3Node>,
        cols: Vec<ProjectItem>,
    },

    // ── Aggregation ───────────────────────────────────────────────────────────
    /// γ + α — GROUP BY + aggregate intents. Concrete operator (HashAgg /
    /// SortAgg / SketchAgg) chosen by L4; `aggs` carry intent only.
    Aggregate {
        child: Arc<L3Node>,
        by: Vec<GroupKey>,
        aggs: Vec<AggIntent>,
        having: Option<Predicate>,
        /// Output column names parallel to `aggs`. When non-empty (populated by
        /// the SQL lowerer from DataFusion's aggregate schema), `output_schema`
        /// uses these names so that an enclosing `Project` can resolve aggregate
        /// outputs by the names DataFusion assigned them (e.g. `"MIN(metrics.ts)"`).
        /// Empty = fall back to synthetic `"agg_{i}"` names.
        output_names: Vec<String>,
    },

    // ── Time / streaming windows ──────────────────────────────────────────────
    /// ψ — tumbling / sliding / session window over the time axis. Defines
    /// the flush / reset lifecycle for aggregates in its sub-DAG. SQL analytic
    /// frames are a different node (`WindowFunc`).
    TimeWindow {
        child: Arc<L3Node>,
        kind: TimeWindowKind,
        size: Duration,
        slide: Option<Duration>,
    },

    // ── Distributed-execution structure ───────────────────────────────────────
    /// Logical-only partitioning marker. Output schema = child schema.
    /// Carries a sharding hint for the L5 stage allocator.
    Partition {
        child: Arc<L3Node>,
        keys: PartitionKeys,
    },
    /// δ — SQL `DISTINCT` / row deduplication.
    Distinct {
        child: Arc<L3Node>,
        cols: Vec<ColumnRef>,
    },
    /// ⊕ — exact union of sub-results from independent stages or shards.
    /// Sketch unions are a separate node in `SummaryExpr` because they carry
    /// sketch-family / params type constraints.
    Merge {
        children: Vec<Arc<L3Node>>,
    },

    // ── Joins ─────────────────────────────────────────────────────────────────
    /// Logical join. L4 picks the physical alternative (HashJoin /
    /// SortMergeJoin / SketchJoin) based on selectivity, memory budget, and
    /// accuracy target.
    Join {
        kind: JoinKind,
        left: Arc<L3Node>,
        right: Arc<L3Node>,
        pred: Option<Predicate>,
    },

    // ── Set operators ─────────────────────────────────────────────────────────
    SetOp {
        kind: SetOpKind,
        all: bool,
        left: Arc<L3Node>,
        right: Arc<L3Node>,
    },

    // ── Ordering & limiting ───────────────────────────────────────────────────
    /// Generic order-by for non-heavy-hitter cases (`ORDER BY name LIMIT 10`).
    /// Heavy-hitter shapes lower to `AggIntent::TopK` instead.
    Sort {
        child: Arc<L3Node>,
        keys: Vec<SortKey>,
    },
    Limit {
        child: Arc<L3Node>,
        /// `None` means no upper bound (only an offset applies).
        n: Option<u64>,
        offset: u64,
    },

    // ── Subquery / CTE ────────────────────────────────────────────────────────
    Subquery {
        child: Arc<L3Node>,
        alias: String,
    },
    /// SQL `WITH name AS (expr) … body`; lowering target for PromQL
    /// recording-rule bindings. The `expr` sub-DAG may be referenced N times
    /// via `Ref(name)` in `body`, giving the DAG its fan-in.
    LetBinding {
        name: String,
        expr: Arc<L3Node>,
        body: Arc<L3Node>,
    },

    // ── Analytic window functions ─────────────────────────────────────────────
    /// SQL `OVER (PARTITION BY … ORDER BY … ROWS BETWEEN …)`.
    /// Distinct from `TimeWindow` — that is a streaming window over the time
    /// axis; this is an analytic frame over already-grouped rows.
    WindowFunc {
        child: Arc<L3Node>,
        func: WindowFuncKind,
        /// Expressions the function operates on (e.g. `LAG(value)` → `[Column("value")]`).
        /// Empty for rank-only funcs (`ROW_NUMBER`, `RANK`, `DENSE_RANK`).
        args: Vec<L3Expr>,
        partition_by: Vec<GroupKey>,
        order_by: Vec<SortKey>,
        frame: Option<WindowFrame>,
    },

    // ── Binary composition ────────────────────────────────────────────────────
    /// Arithmetic / comparison / boolean composition (PromQL binary ops
    /// including `and` / `or` / `unless`, SQL boolean composition).
    BinaryOp {
        op: BinaryOpKind,
        lhs: Arc<L3Node>,
        rhs: Arc<L3Node>,
        vector_match: Option<VectorMatch>,
    },
}

impl HasSchema for QueryExpr {
    fn output_schema(&self, input_schemas: &[&L3Schema], catalog: &SchemaCatalog) -> L3Schema {
        use super::schema::{L3Field, L3Schema};

        // Shorthand: first child's schema (most nodes have exactly one child).
        let child = || input_schemas[0];

        match self {
            // ── Leaf: Table scan — schema comes from the catalog ──────────────
            QueryExpr::Scan { source, .. } => match source {
                Source::Table { table_ref, .. } => {
                    let table = catalog
                        .tables
                        .get(&table_ref.0)
                        .unwrap_or_else(|| panic!("table '{}' not in catalog", table_ref.0));
                    let fields: Vec<L3Field> = table
                        .columns
                        .iter()
                        .map(|c| L3Field {
                            name: c.name.clone(),
                            dtype: c.data_type.clone(),
                            nullable: c.nullable,
                        })
                        .collect();
                    let time_index = table
                        .time_column
                        .as_ref()
                        .and_then(|tc| fields.iter().position(|f| &f.name == tc));
                    L3Schema { fields, time_index }
                }
                Source::TimeSeries { .. } | Source::Join { .. } => {
                    todo!("schema derivation for TimeSeries and Join sources (PromQL path)")
                }
            },

            // ── Project: one output field per ProjectItem ─────────────────────
            QueryExpr::Project { cols, .. } => {
                use super::expr_ir::L3Scalar;
                use super::schema::L3DataType;

                let cs = child();
                let time_col_src = cs.time_index.map(|ti| cs.fields[ti].name.clone());

                let pairs: Vec<(L3Field, bool)> = cols
                    .iter()
                    .map(|item| match &item.expr {
                        L3Expr::Column(col_ref) => {
                            // TODO: DataFusion may qualify column names with the table name
                            // (e.g. "metrics.value") while child schema stores bare names
                            // ("value"). Strip the qualifier before lookup so the fallback
                            // Float64 default is not silently applied to real columns.
                            let child_f = cs.fields.iter().find(|f| f.name == col_ref.0);
                            let (dtype, nullable) = child_f
                                .map(|f| (f.dtype.clone(), f.nullable))
                                .unwrap_or((L3DataType::Float64, true));
                            let out_name = item.alias.as_deref().unwrap_or(&col_ref.0).to_string();
                            let is_time = time_col_src.as_deref() == Some(col_ref.0.as_str());
                            (
                                L3Field {
                                    name: out_name,
                                    dtype,
                                    nullable,
                                },
                                is_time,
                            )
                        }
                        // CAST: output type is the cast target.
                        L3Expr::Cast { to, .. } => {
                            let name = item.alias.as_deref().unwrap_or("cast").to_string();
                            (
                                L3Field {
                                    name,
                                    dtype: to.clone(),
                                    nullable: true,
                                },
                                false,
                            )
                        }
                        // Literal: infer type from the scalar variant.
                        L3Expr::Literal(scalar) => {
                            let (dtype, nullable) = match scalar {
                                L3Scalar::Int64(_) => (L3DataType::Int64, false),
                                L3Scalar::Float64(_) => (L3DataType::Float64, false),
                                L3Scalar::Utf8(_) => (L3DataType::Utf8, false),
                                L3Scalar::Boolean(_) => (L3DataType::Boolean, false),
                                L3Scalar::Null => (L3DataType::Float64, true),
                            };
                            let name = item.alias.as_deref().unwrap_or("literal").to_string();
                            (
                                L3Field {
                                    name,
                                    dtype,
                                    nullable,
                                },
                                false,
                            )
                        }
                        // Arithmetic, CASE, function calls, boolean exprs:
                        // default to Float64 (full type inference is future work).
                        _ => {
                            let name = item.alias.as_deref().unwrap_or("expr").to_string();
                            (
                                L3Field {
                                    name,
                                    dtype: L3DataType::Float64,
                                    nullable: true,
                                },
                                false,
                            )
                        }
                    })
                    .collect();

                let time_index = pairs.iter().position(|(_, is_time)| *is_time);
                let fields = pairs.into_iter().map(|(f, _)| f).collect();
                L3Schema { fields, time_index }
            }

            // ── Pass-through: output schema == child schema ───────────────────
            QueryExpr::Filter { .. }
            | QueryExpr::Sort { .. }
            | QueryExpr::Limit { .. }
            | QueryExpr::Distinct { .. }
            | QueryExpr::Partition { .. }
            | QueryExpr::TimeWindow { .. } => child().clone(),

            // ── Aggregate: GROUP BY cols + one output col per AggIntent ───────
            QueryExpr::Aggregate {
                by,
                aggs,
                output_names,
                ..
            } => {
                let cs = child();

                // TopK is the only multi-column AggIntent: produces the TopK
                // by-columns looked up from the child schema, followed by a
                // synthetic "count" Int64 column.
                if let [AggIntent::TopK { by: topk_by, .. }] = aggs.as_slice() {
                    let mut fields: Vec<L3Field> = topk_by
                        .iter()
                        .filter_map(|col| cs.fields.iter().find(|f| f.name == col.0).cloned())
                        .collect();
                    fields.push(L3Field {
                        name: "count".to_string(),
                        dtype: super::schema::L3DataType::Int64,
                        nullable: false,
                    });
                    return L3Schema {
                        fields,
                        time_index: None,
                    };
                }

                // General case: GROUP BY fields (preserving child type) followed
                // by one output field per AggIntent. Use the tracked col to look
                // up the real field type; fall back to Float64 for untracked intents.
                let float64_dummy = L3Field {
                    name: String::new(),
                    dtype: super::schema::L3DataType::Float64,
                    nullable: true,
                };
                let by_fields: Vec<L3Field> = by
                    .iter()
                    .filter_map(|key| cs.fields.iter().find(|f| f.name == key.0).cloned())
                    .collect();
                let agg_fields: Vec<L3Field> = aggs
                    .iter()
                    .enumerate()
                    .map(|(i, agg)| {
                        let col_field = agg
                            .col()
                            .and_then(|c| cs.fields.iter().find(|f| f.name == c.0))
                            .cloned()
                            .unwrap_or_else(|| float64_dummy.clone());
                        let name = output_names
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| format!("agg_{i}"));
                        L3Field {
                            name,
                            dtype: agg
                                .output_type(&col_field)
                                .unwrap_or(super::schema::L3DataType::Float64),
                            nullable: true,
                        }
                    })
                    .collect();
                let time_col_name = cs.time_index.map(|ti| cs.fields[ti].name.clone());
                let all_fields: Vec<L3Field> = by_fields.into_iter().chain(agg_fields).collect();
                // Propagate time_index if a GROUP BY key preserved the time column in
                // the output. Agg output columns are never the time axis.
                let time_index = time_col_name.and_then(|tc| {
                    if by.iter().any(|key| key.0 == tc) {
                        all_fields.iter().position(|f| f.name == tc)
                    } else {
                        None
                    }
                });
                L3Schema {
                    fields: all_fields,
                    time_index,
                }
            }

            // ── WindowFunc: child schema + one new column ─────────────────────
            QueryExpr::WindowFunc { func, args, .. } => {
                use super::schema::{L3DataType, L3Field};
                let cs = child();

                // Resolve the first arg's type from the child schema.
                // Falls back to Float64 for non-column exprs or unknown columns.
                let arg_field = args.first().and_then(|a| match a {
                    L3Expr::Column(col_ref) => cs.fields.iter().find(|f| f.name == col_ref.0),
                    _ => None,
                });
                let arg_dtype = || arg_field.map_or(L3DataType::Float64, |f| f.dtype.clone());

                // TODO: the output column name should come from DataFusion's Window plan
                // node schema (the actual name the enclosing Projection was built against,
                // e.g. "row_number_1"), not a hardcoded string. An enclosing Project that
                // references DataFusion's real name won't resolve it from this schema.
                // Fix: thread the Window node's output field name through lower_window and
                // store it on WindowFunc, then use it here instead of the hardcoded literals.
                let (win_name, win_dtype, win_nullable) = match func {
                    WindowFuncKind::RowNumber => ("row_number", L3DataType::Int64, false),
                    WindowFuncKind::Rank => ("rank", L3DataType::Int64, false),
                    WindowFuncKind::DenseRank => ("dense_rank", L3DataType::Int64, false),
                    WindowFuncKind::Count => ("count", L3DataType::Int64, false),
                    WindowFuncKind::Sum => ("sum", L3DataType::Float64, true),
                    WindowFuncKind::Avg => ("avg", L3DataType::Float64, true),
                    // Navigation funcs: same type as arg, always nullable (boundary rows)
                    WindowFuncKind::Lag => ("lag", arg_dtype(), true),
                    WindowFuncKind::Lead => ("lead", arg_dtype(), true),
                    WindowFuncKind::FirstValue => ("first_value", arg_dtype(), true),
                    WindowFuncKind::LastValue => ("last_value", arg_dtype(), true),
                    WindowFuncKind::NthValue(_) => ("nth_value", arg_dtype(), true),
                    // Min/Max: preserve input type and nullability
                    WindowFuncKind::Min => {
                        ("min", arg_dtype(), arg_field.is_none_or(|f| f.nullable))
                    }
                    WindowFuncKind::Max => {
                        ("max", arg_dtype(), arg_field.is_none_or(|f| f.nullable))
                    }
                };

                let mut fields = cs.fields.clone();
                fields.push(L3Field {
                    name: win_name.to_string(),
                    dtype: win_dtype,
                    nullable: win_nullable,
                });
                L3Schema {
                    fields,
                    time_index: cs.time_index,
                }
            }

            // ── Merge: all shards have identical schemas; use first ────────────
            QueryExpr::Merge { .. } => input_schemas[0].clone(),

            // ── SetOp: output is left-shaped (SQL semantics) ──────────────────
            QueryExpr::SetOp { .. } => input_schemas[0].clone(),

            // ── Everything else: not yet implemented ──────────────────────────
            _ => todo!(
                "output_schema not yet implemented for {:?}",
                std::mem::discriminant(self)
            ),
        }
    }
}
