/// Opaque handle to the external data-source catalog (Prometheus metric
/// metadata, SQL `information_schema`, DataFusion catalog). Used only by
/// `Scan` schema derivation to resolve leaf column types; all other nodes
/// derive their output schemas purely from their input schemas.
pub struct SchemaCatalog;

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

#[derive(Debug, Clone)]
pub struct L3Field {
    pub name: String,
    pub dtype: L3DataType,
    pub nullable: bool,
}

/// Schema carried on every edge of the L3 DAG. Describes the columns
/// flowing between two operators. Type-checked at plan construction time:
/// a node whose predicate references a column absent from its child's
/// `L3Schema` is a plan-time error.
#[derive(Debug, Clone)]
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
    fn output_schema(&self, input_schemas: &[&L3Schema], catalog: &SchemaCatalog) -> L3Schema;
}
