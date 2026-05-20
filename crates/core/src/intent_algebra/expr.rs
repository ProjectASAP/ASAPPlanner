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

/// A GROUP BY key reference (column / label name).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKey(pub String);

/// A reference to a column / label by name. For time-series sources this is
/// a label name or the synthetic sample-value / timestamp column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnRef(pub String);

/// A set of partitioning keys (sharding hint for L5 stage allocator).
#[derive(Debug, Clone)]
pub struct PartitionKeys;

/// One key in an ORDER BY clause.
#[derive(Debug, Clone)]
pub struct SortKey {
    pub expr: L3Expr,
    pub ascending: bool,
    pub nulls_first: bool,
}

/// An analytic window frame (ROWS / RANGE BETWEEN …).
#[derive(Debug, Clone)]
pub struct WindowFrame;

/// PromQL vector-match modifiers: `on(...)` / `ignoring(...)` for label-set
/// matching, plus `group_left(...)` / `group_right(...)` for many-to-one and
/// one-to-many cardinality. Carried by `QueryExpr::BinaryOp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorMatch {
    /// `true` for `on(labels)`, `false` for `ignoring(labels)`.
    pub on: bool,
    /// The labels named in the `on` / `ignoring` clause.
    pub labels: Vec<String>,
    /// Grouping side for many-to-one / one-to-many matches, if any.
    pub grouping: Option<VectorGrouping>,
}

/// `group_left(labels)` / `group_right(labels)` modifier on a vector match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorGrouping {
    pub side: GroupSide,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupSide {
    Left,
    Right,
}

/// Reference to a metric by name (PromQL / OTLP).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MetricRef(pub String);

/// Closed time interval `[start_ms, end_ms]` in milliseconds since the Unix
/// epoch. Either bound may be `None`, meaning unbounded on that side.
///
/// For PromQL this is the query's absolute evaluation range, which is supplied
/// by the query API (`/query_range` `start`/`end`), **not** by the query
/// string — so a PromQL `Scan` carries `time = None` until that context is
/// threaded in. The range-vector duration (`m[5m]`) is a *window* and lives on
/// `QueryExpr::TimeWindow`, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeRange {
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
}

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
    Pow,
    Atan2,
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
    NthValue(u64),
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
    ///
    /// Label matchers are **not** carried here: they are language-independent
    /// row filters and live in `QueryExpr::Scan.predicates` (the same place SQL
    /// `WHERE` conjuncts on a table scan would go), so every node above the leaf
    /// stays data-model-agnostic.
    TimeSeries {
        metric: MetricRef,
        /// Absolute evaluation range; `None` when not yet bound (see `TimeRange`).
        time: Option<TimeRange>,
    },
    /// Tabular input — deployment-model-asapfusion / future-OLAP shape.
    Table {
        table_ref: TableRef,
        columns: Vec<ColumnRef>,
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
    Sum,
    Min,
    Max,
    Avg,
    /// Sample standard deviation when `population == false`; population stddev
    /// otherwise. PromQL `stddev` / `stddev_over_time`.
    StdDev {
        population: bool,
    },
    /// Variance — PromQL `stdvar` / `stdvar_over_time`.
    Variance {
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

    /// Output column type for a single-column aggregate result.
    ///
    /// `input` is the field this intent reduces (the sample-value field for a
    /// time-series window). Returns `None` for `TopK`, which produces multiple
    /// output columns — use `QueryExpr::output_schema` for its schema.
    pub fn output_type(&self, input: &super::schema::L3Field) -> Option<super::schema::L3DataType> {
        use super::schema::L3DataType;
        match self {
            Self::Count { .. } | Self::Cardinality { .. } => Some(L3DataType::Int64),
            Self::Min | Self::Max => Some(input.dtype.clone()),
            Self::Sum
            | Self::Avg
            | Self::StdDev { .. }
            | Self::Variance { .. }
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
/// `Arc` references to the same `L3Node`.
#[derive(Debug, Clone)]
pub enum QueryExpr {
    // ── Base relations ────────────────────────────────────────────────────────
    /// Outermost leaf. `source` carries the data-model-specific leaf shape;
    /// `predicates` are leaf-level row filters (PromQL label matchers, SQL
    /// pushed-down `WHERE` conjuncts).
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
    },

    // ── Time / streaming windows ──────────────────────────────────────────────
    /// ψ — tumbling / sliding / session window over the time axis. Defines
    /// the flush / reset lifecycle for aggregates in its sub-DAG. PromQL range
    /// vectors (`m[5m]`) and subqueries (`m[5m:1m]`) lower here. SQL analytic
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
        use super::schema::{L3DataType, L3Field};

        // Shorthand: first child's schema (most nodes have exactly one child).
        let child = || input_schemas[0];

        match self {
            // ── Leaf: schema comes from the catalog ───────────────────────────
            QueryExpr::Scan { source, predicates } => match source {
                // Time-series leaf: synthesize `timestamp` (the time axis) and
                // the sample-`value` column, then one `Utf8` label column per
                // label known from the catalog, unioned with any label
                // referenced by the scan's predicates (so a filter on an
                // unregistered label still type-checks).
                Source::TimeSeries { metric, .. } => {
                    let meta = catalog.metrics.get(&metric.0);
                    let value_dtype = meta
                        .map(|m| m.value_type.clone())
                        .unwrap_or(L3DataType::Float64);

                    let mut fields = vec![
                        L3Field {
                            name: "timestamp".to_string(),
                            dtype: L3DataType::Timestamp,
                            nullable: false,
                        },
                        L3Field {
                            name: "value".to_string(),
                            dtype: value_dtype,
                            nullable: false,
                        },
                    ];

                    let mut label_names: Vec<String> =
                        meta.map(|m| m.labels.clone()).unwrap_or_default();
                    for p in predicates {
                        for c in p.0.columns_referenced() {
                            if c.0 != "value" && c.0 != "timestamp" && !label_names.contains(&c.0) {
                                label_names.push(c.0.clone());
                            }
                        }
                    }
                    for name in label_names {
                        fields.push(L3Field {
                            name,
                            dtype: L3DataType::Utf8,
                            nullable: true,
                        });
                    }
                    L3Schema {
                        fields,
                        time_index: Some(0),
                    }
                }
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
                Source::Join { .. } => {
                    todo!("schema derivation for Join sources")
                }
            },

            // ── Pass-through: output schema == child schema ───────────────────
            QueryExpr::Filter { .. }
            | QueryExpr::Sort { .. }
            | QueryExpr::Limit { .. }
            | QueryExpr::Distinct { .. }
            | QueryExpr::Partition { .. }
            | QueryExpr::TimeWindow { .. } => child().clone(),

            // ── Aggregate: GROUP BY cols + one output col per AggIntent ───────
            QueryExpr::Aggregate { by, aggs, .. } => {
                let cs = child();

                // TopK is the only multi-column AggIntent: produces the TopK
                // by-columns looked up from the child schema, followed by a
                // synthetic "count" Int64 column.
                if let [AggIntent::TopK { by: topk_by, .. }] = aggs.as_slice() {
                    let mut fields: Vec<L3Field> = topk_by
                        .iter()
                        .map(|col| {
                            cs.fields
                                .iter()
                                .find(|f| f.name == col.0)
                                .cloned()
                                .unwrap_or(L3Field {
                                    name: col.0.clone(),
                                    dtype: L3DataType::Utf8,
                                    nullable: true,
                                })
                        })
                        .collect();
                    fields.push(L3Field {
                        name: "count".to_string(),
                        dtype: L3DataType::Int64,
                        nullable: false,
                    });
                    return L3Schema {
                        fields,
                        time_index: None,
                    };
                }

                // General case: GROUP BY fields (preserving child type) followed
                // by one output field per AggIntent. The sample-`value` field is
                // the canonical reduction input for a time-series window.
                let value_field = cs
                    .fields
                    .iter()
                    .find(|f| f.name == "value")
                    .cloned()
                    .unwrap_or(L3Field {
                        name: "value".to_string(),
                        dtype: L3DataType::Float64,
                        nullable: true,
                    });
                let by_fields: Vec<L3Field> = by
                    .iter()
                    .filter_map(|key| cs.fields.iter().find(|f| f.name == key.0).cloned())
                    .collect();
                let agg_fields: Vec<L3Field> = aggs
                    .iter()
                    .enumerate()
                    .map(|(i, agg)| {
                        let name = if aggs.len() == 1 {
                            "value".to_string()
                        } else {
                            format!("value_{i}")
                        };
                        L3Field {
                            name,
                            dtype: agg.output_type(&value_field).unwrap_or(L3DataType::Float64),
                            nullable: true,
                        }
                    })
                    .collect();
                let all_fields: Vec<L3Field> = by_fields.into_iter().chain(agg_fields).collect();
                L3Schema {
                    fields: all_fields,
                    time_index: None,
                }
            }

            // ── BinaryOp: left operand shape, value column re-typed ───────────
            // Arithmetic/comparison between two vectors yields the left vector's
            // shape (label set + value); set ops (`and`/`or`/`unless`) likewise.
            QueryExpr::BinaryOp { .. } => input_schemas[0].clone(),

            // ── Merge / SetOp: union-compatible; representative is the first ──
            QueryExpr::Merge { .. } | QueryExpr::SetOp { .. } => input_schemas[0].clone(),

            // ── Everything else: not yet implemented ──────────────────────────
            _ => todo!(
                "output_schema not yet implemented for {:?}",
                std::mem::discriminant(self)
            ),
        }
    }
}
