use std::collections::HashMap;

/// Catalog of external data-source metadata. Used by L1→L3 lowering and by
/// `Scan` schema derivation to resolve leaf column / label types; all other
/// nodes derive their output schemas purely from their input schemas.
///
/// Holds both relational tables (SQL / DataFusion sources) and time-series
/// metrics (PromQL / OTLP sources). A deployment model populates only the
/// half it needs.
#[derive(Debug, Clone, Default)]
pub struct SchemaCatalog {
    /// Relational tables, keyed by table name.
    pub tables: HashMap<String, TableSchema>,
    /// Time-series metrics, keyed by metric name.
    pub metrics: HashMap<String, MetricSchema>,
}

/// Schema for a single relational table.
#[derive(Debug, Clone)]
pub struct TableSchema {
    pub columns: Vec<ColumnDef>,
    /// Name of the column that holds the row timestamp, if any.
    pub time_column: Option<String>,
}

/// One column in a `TableSchema`.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: L3DataType,
    pub nullable: bool,
}

/// Schema for a single time-series metric.
///
/// A metric scan always produces a `timestamp` (the time axis) and a sample
/// `value` column; this metadata names the label set carried alongside and the
/// value's type. When a metric is absent from the catalog the lowerer falls
/// back to `value: Float64` with labels discovered from the query's matchers.
#[derive(Debug, Clone)]
pub struct MetricSchema {
    /// Label names exposed by this metric (e.g. `["service", "host", "env"]`).
    pub labels: Vec<String>,
    /// Type of the sample value column. Usually `Float64`.
    pub value_type: L3DataType,
}

impl Default for MetricSchema {
    fn default() -> Self {
        Self {
            labels: Vec::new(),
            value_type: L3DataType::Float64,
        }
    }
}

// ── Data types ────────────────────────────────────────────────────────────────

/// Column types that may appear on an L3 DAG edge.
/// L4 extends this set with `L4DataType::Sketch`; L3 edges never carry
/// sketch-state columns.
#[derive(Debug, Clone, PartialEq)]
pub enum L3DataType {
    Int64,
    Float64,
    Utf8,
    Boolean,
    Timestamp,
    Duration,
    /// Key→Value map (e.g. PromQL label set encoded as a column).
    Map(Box<L3DataType>, Box<L3DataType>),
    List(Box<L3DataType>),
}

// ── Schema ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct L3Field {
    pub name: String,
    pub dtype: L3DataType,
    pub nullable: bool,
}

/// Schema carried on every edge of the L3 DAG. Describes the columns
/// flowing between two operators. Type-checked at plan construction time:
/// a node whose predicate references a column absent from its child's
/// `L3Schema` is a plan-time error.
#[derive(Debug, Clone, PartialEq)]
pub struct L3Schema {
    pub fields: Vec<L3Field>,
    /// Index into `fields` for the time axis, if any.
    /// PromQL `Scan` leaves always carry one; SQL leaves may or may not.
    pub time_index: Option<usize>,
}

// ── Schema derivation trait ───────────────────────────────────────────────────

/// Implemented by `QueryExpr` to compute the output schema of a node given
/// its children's output schemas. The `L3Node` wrapper stores the derived
/// schema so derivation runs once at construction, not on every traversal.
pub trait HasSchema {
    /// # Panics
    /// Panics if a `Scan` over a `Source::Table` references a table absent from
    /// `catalog`. Time-series scans never panic — an unregistered metric falls
    /// back to a `value: Float64` default schema.
    fn output_schema(&self, input_schemas: &[&L3Schema], catalog: &SchemaCatalog) -> L3Schema;
}
