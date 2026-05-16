use std::rc::Rc;
use std::time::Duration;

use crate::types::AccuracyTarget;
use super::schema::{HasSchema, L3Schema, SchemaCatalog};

// ── Stub leaf / supporting types ──────────────────────────────────────────────
// Full definitions will be added as the respective layers are implemented.

/// A row-level filter predicate (WHERE clause / PromQL label matcher).
#[derive(Debug, Clone)] pub struct Predicate;
/// One item in a SELECT projection list.
#[derive(Debug, Clone)] pub struct ProjectItem;
/// A GROUP BY key reference.
#[derive(Debug, Clone)] pub struct GroupKey;
/// A reference to a column by name.
#[derive(Debug, Clone)] pub struct ColumnRef;
/// A set of partitioning keys (sharding hint for L5 stage allocator).
#[derive(Debug, Clone)] pub struct PartitionKeys;
/// One key in an ORDER BY clause.
#[derive(Debug, Clone)] pub struct SortKey;
/// An analytic window frame (ROWS / RANGE BETWEEN …).
#[derive(Debug, Clone)] pub struct WindowFrame;
/// PromQL vector-match modifiers (`on`/`ignoring` + `group_left`/`group_right`).
#[derive(Debug, Clone)] pub struct VectorMatch;
/// Reference to a metric by name (PromQL / OTLP).
#[derive(Debug, Clone)] pub struct MetricRef;
/// Closed time interval for a time-series scan.
#[derive(Debug, Clone)] pub struct TimeRange;
/// Label matchers applied to a time-series scan.
#[derive(Debug, Clone)] pub struct LabelFilter;
/// Reference to a relational table by name.
#[derive(Debug, Clone)] pub struct TableRef;
/// Join key specification (USING / ON column reference).
#[derive(Debug, Clone)] pub struct JoinKey;

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
    Add, Sub, Mul, Div, Mod,
    // Comparison
    Eq, NotEq, Lt, LtEq, Gt, GtEq,
    // Boolean / PromQL set operators
    And, Or, Unless,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowFuncKind {
    RowNumber, Rank, DenseRank,
    Lag, Lead,
    FirstValue, LastValue,
    NthValue(u64),
    Sum, Avg, Count, Min, Max,
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
    Count { accuracy: AccuracyTarget },
    Sum,
    Min,
    Max,
    Quantile { q: f64, accuracy: AccuracyTarget },
    /// Heavy-hitter top-k. Distinct from generic `Sort + Limit` — a
    /// dedicated sketch (SpaceSaving, CMS-with-heap) computes it as a single
    /// primitive. Recognised by L1→L2→L3 lowering on `ORDER BY count DESC
    /// LIMIT k` / PromQL `topk(k, …)`.
    TopK { k: usize, by: Vec<ColumnRef>, accuracy: AccuracyTarget },
    Cardinality { accuracy: AccuracyTarget },

    // ── Time-series streaming derivatives ────────────────────────────────────
    // Include PromQL counter-reset adjustment; not equivalent to Sum/Count
    // over a Window. Kept distinct so delta-set aggregators bind directly.
    Rate { window: Duration },
    Increase { window: Duration },
}

impl AggIntent {
    /// Which data model this intent semantically requires. L4 rules consult
    /// this to skip non-applicable intents (e.g. `Rate` over a `Source::Table`).
    pub fn requires(&self) -> DataModel {
        todo!()
    }

    /// Output column type — used by L3 schema derivation for `Aggregate`.
    pub fn output_type(&self, _input: &super::schema::L3Field) -> super::schema::L3DataType {
        todo!()
    }
}

// ── L3 DAG node ───────────────────────────────────────────────────────────────

/// A node in the L3 DAG. Wraps the expression and its derived output schema
/// so that every edge implicitly carries a typed schema: holding an
/// `Rc<L3Node>` gives you both the child expression and the schema of the
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
    Scan { source: Source, predicates: Vec<Predicate> },
    /// Reference to a named `LetBinding` sub-expression; resolved at plan time.
    Ref(String),

    // ── Filtering & projection ────────────────────────────────────────────────
    /// σ — row-level filter. Output schema = child schema (unchanged).
    Filter { child: Rc<L3Node>, pred: Predicate },
    /// π — column projection. Output schema = child schema projected to `cols`.
    Project { child: Rc<L3Node>, cols: Vec<ProjectItem> },

    // ── Aggregation ───────────────────────────────────────────────────────────
    /// γ + α — GROUP BY + aggregate intents. Concrete operator (HashAgg /
    /// SortAgg / SketchAgg) chosen by L4; `aggs` carry intent only.
    Aggregate {
        child: Rc<L3Node>,
        by: Vec<GroupKey>,
        aggs: Vec<AggIntent>,
        having: Option<Predicate>,
    },

    // ── Time / streaming windows ──────────────────────────────────────────────
    /// ψ — tumbling / sliding / session window over the time axis. Defines
    /// the flush / reset lifecycle for aggregates in its sub-DAG. SQL analytic
    /// frames are a different node (`WindowFunc`).
    TimeWindow {
        child: Rc<L3Node>,
        kind: TimeWindowKind,
        size: Duration,
        slide: Option<Duration>,
    },

    // ── Distributed-execution structure ───────────────────────────────────────
    /// Logical-only partitioning marker. Output schema = child schema.
    /// Carries a sharding hint for the L5 stage allocator.
    Partition { child: Rc<L3Node>, keys: PartitionKeys },
    /// δ — SQL `DISTINCT` / row deduplication.
    Distinct { child: Rc<L3Node>, cols: Vec<ColumnRef> },
    /// ⊕ — exact union of sub-results from independent stages or shards.
    /// Sketch unions are a separate node in `SummaryExpr` because they carry
    /// sketch-family / params type constraints.
    Merge { children: Vec<Rc<L3Node>> },

    // ── Joins ─────────────────────────────────────────────────────────────────
    /// Logical join. L4 picks the physical alternative (HashJoin /
    /// SortMergeJoin / SketchJoin) based on selectivity, memory budget, and
    /// accuracy target.
    Join {
        kind: JoinKind,
        left: Rc<L3Node>,
        right: Rc<L3Node>,
        pred: Option<Predicate>,
    },

    // ── Set operators ─────────────────────────────────────────────────────────
    SetOp {
        kind: SetOpKind,
        all: bool,
        left: Rc<L3Node>,
        right: Rc<L3Node>,
    },

    // ── Ordering & limiting ───────────────────────────────────────────────────
    /// Generic order-by for non-heavy-hitter cases (`ORDER BY name LIMIT 10`).
    /// Heavy-hitter shapes lower to `AggIntent::TopK` instead.
    Sort { child: Rc<L3Node>, keys: Vec<SortKey> },
    Limit { child: Rc<L3Node>, n: u64, offset: u64 },

    // ── Subquery / CTE ────────────────────────────────────────────────────────
    Subquery { child: Rc<L3Node>, alias: String },
    /// SQL `WITH name AS (expr) … body`; lowering target for PromQL
    /// recording-rule bindings. The `expr` sub-DAG may be referenced N times
    /// via `Ref(name)` in `body`, giving the DAG its fan-in.
    LetBinding {
        name: String,
        expr: Rc<L3Node>,
        body: Rc<L3Node>,
    },

    // ── Analytic window functions ─────────────────────────────────────────────
    /// SQL `OVER (PARTITION BY … ORDER BY … ROWS BETWEEN …)`.
    /// Distinct from `TimeWindow` — that is a streaming window over the time
    /// axis; this is an analytic frame over already-grouped rows.
    WindowFunc {
        child: Rc<L3Node>,
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
        lhs: Rc<L3Node>,
        rhs: Rc<L3Node>,
        vector_match: Option<VectorMatch>,
    },
}

impl HasSchema for QueryExpr {
    fn output_schema(&self, _input_schemas: &[&L3Schema], _catalog: &SchemaCatalog) -> L3Schema {
        todo!()
    }
}
