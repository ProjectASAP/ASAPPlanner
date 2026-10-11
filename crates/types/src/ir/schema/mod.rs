//! Per-edge schema of the operator IR — every edge carries a typed `Schema`.
//!
//! One `Schema` type serves every operator, before and after ASAP
//! optimization: a field's [`FieldDataType`] is either a plain readable
//! [`DataType`] or the summary / exact-accumulator state a `SummaryAgg`
//! produces. The DAG is type-checked: a node's output schema is a function of
//! its inputs and parameters and is verifiable independently of the
//! surrounding context.
//!
//! `Schema::unique_keys` is metadata for reuse-aware planning: a producer's
//! output can only be safely shared across consumers when its row identity
//! is provably stable across reads, which is what this field records.

#![allow(dead_code)]

pub mod aggregate_schema;
pub mod error;
pub mod state_type;

pub use aggregate_schema::aggregate_output_schema;
pub use error::SchemaDerivationError;
pub use state_type::{
    default_hydra_params, hydra_kind_for, EntityIdentity, ExactKind, ExactParams, GroupingStrategy,
    HydraKind, HydraParams, NonNegativeWeightProof, SamplingKind, SamplingParams, SketchAlgorithm,
    SketchCategory, SketchKind, SketchParams, SketchStatistic, StatModelKind, StatModelParams,
    SummaryInputExpr, SummaryUpdate, WaveletKind, WaveletParams, WeightDomain,
};

use serde::{Deserialize, Serialize};

/// Zero-based position of a column in a particular operator's input or output.
///
/// During planning, the position indexes [`Schema::fields`] to obtain metadata;
/// during execution, it identifies the corresponding value in each input row.
/// Expressions, grouping keys, and schema key/time metadata use the same position.
/// It is local to that schema, not a stable field identity across projections or
/// joins, and does not own data or prescribe a row/column-oriented storage layout.
pub type ColumnId = usize;

/// One field of a [`Schema`]: `name + dtype + nullable`, plus an optional
/// table qualifier. The struct describes a column and holds none of its data.
///
/// `T` is the type vocabulary: [`FieldDataType`] on an operator edge (the
/// default, and what [`Schema::fields`] holds), plain [`DataType`] for the
/// nested element fields of [`DataType::List`] / [`DataType::Struct`], which
/// can never carry summary state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Field<T = FieldDataType> {
    /// Field name as it appears in the producer's output. PromQL leaves
    /// produce label-name + the synthetic `value` / `timestamp` columns;
    /// SQL leaves carry their `information_schema` names.
    pub name: String,
    pub dtype: T,
    /// Whether NULL values are allowed in this field. PromQL value
    /// columns are non-nullable; SQL columns inherit their DDL nullability.
    pub nullable: bool,
    /// Optional table/alias qualifier (SQL `t.col` / `t AS a` → `a`). Travels
    /// with the field through joins so a `ColumnRef::Qualified` can pick the
    /// right side when both carry the same `name`. `None` for PromQL labels and
    /// unqualified columns.
    #[serde(default)]
    pub table: Option<String>,
}

impl<T> Field<T> {
    /// An unqualified field (`table = None`).
    pub fn new(name: impl Into<String>, dtype: T, nullable: bool) -> Self {
        Self {
            name: name.into(),
            dtype,
            nullable,
            table: None,
        }
    }

    /// This field re-qualified under `table` (e.g. by a `SubqueryAlias`).
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.table = Some(table.into());
        self
    }
}

impl Field<FieldDataType> {
    /// An unqualified field carrying an ordinary readable value.
    pub fn plain(name: impl Into<String>, dtype: DataType, nullable: bool) -> Self {
        Self::new(name, FieldDataType::Plain(dtype), nullable)
    }

    /// The value type of a plain field; `None` for summary / accumulator state.
    pub fn plain_dtype(&self) -> Option<&DataType> {
        match &self.dtype {
            FieldDataType::Plain(dtype) => Some(dtype),
            _ => None,
        }
    }

    /// Whether this field carries an ordinary readable value.
    pub fn is_plain(&self) -> bool {
        matches!(self.dtype, FieldDataType::Plain(_))
    }

    /// The value type of a plain field; panics on summary / accumulator
    /// state. For code that has already established the field is plain
    /// (front ends, scalar type inference over value columns).
    pub fn expect_plain_dtype(&self) -> &DataType {
        self.plain_dtype().unwrap_or_else(|| {
            panic!(
                "field `{}` carries summary state ({:?}), not a plain value",
                self.name, self.dtype
            )
        })
    }
}

impl From<Field<DataType>> for Field<FieldDataType> {
    fn from(field: Field<DataType>) -> Self {
        Self {
            name: field.name,
            dtype: FieldDataType::Plain(field.dtype),
            nullable: field.nullable,
            table: field.table,
        }
    }
}

/// What a schema field carries: an ordinary readable value, or the summary /
/// exact-accumulator state produced by a `SummaryAgg`.
///
/// Every non-`Plain` variant carries the physical state identity required by
/// that family (`Sketch` additionally carries its grouping layout), so the
/// type system can reject merges of incompatible summaries at plan
/// construction time — a `SummaryMerge` over `Sketch(Kll, …)` and
/// `Sketch(Cms, …)` inputs is a plan-time error, and a `Sketch(…)` can never
/// be confused for a `Sample(…)` even though both are "opaque summary state"
/// at a glance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FieldDataType {
    /// An ordinary, readable value — the closed vocabulary of [`DataType`].
    Plain(DataType),
    /// Exact, mergeable accumulator state (`Sum`/`Count`/`Min`/`Max`/`Rate`/
    /// `Increase`). Value consumers require an explicit finalization boundary.
    ExactAggregate(ExactKind, ExactParams),
    /// Approximate sketch state (KLL/CMS/HLL/…), read out via a
    /// `SummaryEstimate`. A [`SketchKind`] already carries the concrete
    /// algorithm, params, and grouping layout committed to, not just its
    /// category — a bound node needs to know it's specifically independent
    /// KLL or shared Hydra-backed CMS, not merely "some sketch".
    Sketch(SketchKind, GroupingStrategy),
    /// Sampling-based summary state (a retained row subset).
    Sample(SamplingKind, SamplingParams),
    /// Wavelet-transform summary state (a coefficient vector).
    Wavelet(WaveletKind, WaveletParams),
    /// Fitted statistical/parametric-model summary state.
    StatModel(StatModelKind, StatModelParams),
}

impl FieldDataType {
    pub fn is_plain(&self) -> bool {
        matches!(self, FieldDataType::Plain(_))
    }

    pub fn plain(&self) -> Option<&DataType> {
        match self {
            FieldDataType::Plain(dtype) => Some(dtype),
            _ => None,
        }
    }
}

impl From<DataType> for FieldDataType {
    fn from(dtype: DataType) -> Self {
        FieldDataType::Plain(dtype)
    }
}

impl PartialEq<DataType> for FieldDataType {
    fn eq(&self, other: &DataType) -> bool {
        matches!(self, FieldDataType::Plain(dtype) if dtype == other)
    }
}

/// Plain value types. Summary state is a [`FieldDataType`] concern.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// Bottom type for NULL-only values and empty collection elements.
    Null,
    /// 64-bit signed integer. Counter columns, group-cardinality outputs.
    Int64,
    /// 64-bit IEEE-754 float. Quantile / Avg / Sum-over-floats output.
    Float64,
    /// UTF-8 string. PromQL label values, SQL `VARCHAR` / `TEXT`.
    Utf8,
    /// Boolean — predicate output, `unless` / `and` / `or` PromQL ops.
    Bool,
    /// Wall-clock timestamp. PromQL leaves carry exactly one of these
    /// (the `time_index` column); SQL leaves may or may not.
    Timestamp,
    /// The type of a [`ScalarValue::Interval`] — a calendar duration, not an
    /// instant. Carried so `infer_expr_type` can give an interval literal a
    /// type and type `Timestamp ± Interval`; no column is ever declared with it.
    Interval,
    /// Calendar date with no time-of-day — SQL `DATE`, Arrow `Date32`/`Date64`.
    /// Distinct from `Timestamp` because a `CAST(… AS DATE)` is a real type
    /// change DataFusion keeps in the plan: collapsing the two here would make
    /// the bridge lossy in the one direction (`dtype_to_arrow`) that registers
    /// catalog tables.
    Date,
    /// Variable-length sequence. The existing column contract preserves the
    /// element field name, type, and nullability. Nested fields are unqualified.
    List { element: Box<Field<DataType>> },
    /// Ordered named fields, including each field's independent nullability.
    Struct { fields: Vec<Field<DataType>> },
    /// SQL map entries with non-null keys and explicitly nullable values.
    Map {
        key: Box<DataType>,
        value: Box<DataType>,
        value_nullable: bool,
    },
}

/// Per-edge schema. Flowing between any two operators, on every node's
/// input and output.
///
/// `unique_keys` is metadata for reuse-aware planning: each inner `Vec<ColumnId>`
/// is a set of column indices that together uniquely identify rows. The
/// outer `Vec` allows multiple unique-key sets (primary key + another
/// unique constraint). Populated by per-node input/output spec —
/// `Aggregate { by, .. }` emits `unique_keys = [by]`; `Dedup { cols }`
/// adds `cols`; most other nodes pass through.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(try_from = "SchemaWire")]
pub struct Schema {
    /// Fields flowing on this edge, in positional order.
    pub fields: Vec<Field>,
    /// Index into `fields` for the time axis, if any. PromQL leaves
    /// always carry one; SQL leaves may or may not.
    #[serde(default)]
    pub time_index: Option<ColumnId>,
    /// Unique-key sets — each inner vec is a tuple of column indices
    /// that together uniquely identifies a row. Empty `Vec` means
    /// "no provable unique constraint" (the conservative default).
    #[serde(default)]
    pub unique_keys: Vec<Vec<ColumnId>>,
    /// Whether this schema **completely enumerates** the columns at this point.
    ///
    /// - `true` (**closed**): there are no columns beyond these — a catalog-backed
    ///   SQL source, or an output fully determined by an `Aggregate`/`Project`.
    /// - `false` (**open**): a dynamic / superset schema — the runtime row may
    ///   carry more columns than are listed (a schemaless PromQL leaf lists only
    ///   the `(ts, value)` floor + the labels the query references).
    ///
    /// This mirrors Apache Calcite's `DynamicRecordType` (schema-on-read): the
    /// schema starts open at a schemaless leaf and is **frozen to closed** by the
    /// first operator that fully determines its output columns (`Aggregate` /
    /// `Project`). Consumers needing completeness (label validation, full-output
    /// enumeration, cardinality for the cost model) must check this; positional
    /// resolution does not care. **Invariant: open ⇒ do not apply closed-world
    /// validation** (PromQL tolerates unknown labels). Defaults to `false` (open)
    /// — the conservative choice when completeness is unknown.
    #[serde(default)]
    pub closed: bool,
}

/// Deserialization form of [`Schema`]. Also reads the two layouts that
/// predate the unified schema, so plans saved by older builds still load:
///
/// - pre-ASAP `Schema`: `columns` (not `fields`), each `dtype` a bare
///   [`DataType`] (`"float64"`), with `closed` / `unique_keys` present;
/// - post-ASAP `SummarySchema`: `fields` with tagged dtypes
///   (`{"Plain":"float64"}`), but no `closed` / `unique_keys`. Those were the
///   [`Schema::lifted`] shape, so a missing `closed` next to `fields` is closed.
#[derive(Deserialize)]
struct SchemaWire {
    fields: Option<Vec<FieldWire>>,
    columns: Option<Vec<FieldWire>>,
    #[serde(default)]
    time_index: Option<ColumnId>,
    #[serde(default)]
    unique_keys: Vec<Vec<ColumnId>>,
    closed: Option<bool>,
}

#[derive(Deserialize)]
struct FieldWire {
    name: String,
    dtype: FieldDataTypeWire,
    nullable: bool,
    #[serde(default)]
    table: Option<String>,
}

/// A tagged [`FieldDataType`], or a bare [`DataType`] from a pre-ASAP column.
#[derive(Deserialize)]
#[serde(untagged)]
enum FieldDataTypeWire {
    Current(FieldDataType),
    Legacy(DataType),
}

impl TryFrom<SchemaWire> for Schema {
    type Error = String;

    fn try_from(wire: SchemaWire) -> Result<Self, String> {
        let legacy_summary = wire.fields.is_some();
        let fields = match (wire.fields, wire.columns) {
            (Some(fields), None) | (None, Some(fields)) => fields,
            (Some(_), Some(_)) => return Err("schema has both `fields` and `columns`".into()),
            (None, None) => return Err("missing field `fields`".into()),
        };
        let fields = fields
            .into_iter()
            .map(|field| Field {
                name: field.name,
                dtype: match field.dtype {
                    FieldDataTypeWire::Current(dtype) => dtype,
                    FieldDataTypeWire::Legacy(dtype) => FieldDataType::Plain(dtype),
                },
                nullable: field.nullable,
                table: field.table,
            })
            .collect();
        Ok(Self {
            fields,
            time_index: wire.time_index,
            unique_keys: wire.unique_keys,
            closed: wire.closed.unwrap_or(legacy_summary),
        })
    }
}

/// Reserved physical row column carrying canonical JSON of a complete PromQL
/// label map. `$` cannot occur in a user PromQL label name.
pub const PROMQL_SERIES_IDENTITY: &str = "$promql_series_identity";

impl Schema {
    pub fn has_promql_series_identity(&self) -> bool {
        self.closed
            && self.fields.iter().any(|field| {
                field.name == PROMQL_SERIES_IDENTITY
                    && field.dtype == DataType::Utf8
                    && !field.nullable
                    && field.table.is_none()
            })
    }

    /// Construct a `Schema` from fields alone — no time index, no
    /// unique-key constraint. Used by `Scan` over a tabular source
    /// when the catalog supplies no primary-key metadata.
    pub fn new(fields: Vec<Field>) -> Self {
        Self {
            fields,
            time_index: None,
            unique_keys: Vec::new(),
            closed: false,
        }
    }

    /// Construct a `Scan`-style schema with explicit `time_index` +
    /// inferred unique keys (e.g. PromQL leaves: `[time_index, label_set]`).
    pub fn with_time_index(
        fields: Vec<Field>,
        time_index: ColumnId,
        unique_keys: Vec<Vec<ColumnId>>,
    ) -> Self {
        Self {
            fields,
            time_index: Some(time_index),
            unique_keys,
            closed: false,
        }
    }

    /// The schema of a summary-planning node: `fields` and a time axis, no
    /// unique-key claim, closed. The shape every post-ASAP operator output
    /// carried before pre- and post-ASAP schemas were one type.
    pub fn lifted(fields: Vec<Field>, time_index: Option<ColumnId>) -> Self {
        Self {
            fields,
            time_index,
            unique_keys: Vec::new(),
            closed: true,
        }
    }

    /// Whether every field carries an ordinary readable value.
    pub fn is_all_plain(&self) -> bool {
        self.fields.iter().all(Field::is_plain)
    }

    /// Look up a field by name (first match). `None` if not present.
    pub fn column_id(&self, name: &str) -> Option<ColumnId> {
        self.fields.iter().position(|c| c.name == name)
    }

    /// Look up a field by `(table, name)` qualifier — disambiguates columns
    /// that share a `name` across a join (`a.k` vs `b.k`). `None` if no field
    /// has both that qualifier and name.
    pub fn column_id_qualified(&self, table: &str, name: &str) -> Option<ColumnId> {
        self.fields
            .iter()
            .position(|c| c.name == name && c.table.as_deref() == Some(table))
    }

    /// Whether this schema has *any* provable unique key — the signal a
    /// reuse-aware planning pass needs to decide whether a producer's output
    /// can be safely shared across consumers.
    pub fn has_unique_key(&self) -> bool {
        !self.unique_keys.is_empty()
    }

    /// Append `cols` as an additional unique-key set if not already present.
    /// Used by `Dedup { cols }`: "the input schema with `unique_keys`
    /// tightened to include `cols`".
    pub fn add_unique_key(&mut self, cols: Vec<ColumnId>) {
        if !self.unique_keys.contains(&cols) {
            self.unique_keys.push(cols);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, dtype: DataType) -> Field {
        Field::plain(name, dtype, false)
    }

    #[test]
    fn schema_new_has_no_time_or_unique_key() {
        let s = Schema::new(vec![col("k", DataType::Utf8), col("v", DataType::Float64)]);
        assert!(s.time_index.is_none());
        assert!(!s.has_unique_key());
        assert_eq!(s.column_id("k"), Some(0));
        assert_eq!(s.column_id("v"), Some(1));
        assert_eq!(s.column_id("missing"), None);
    }

    #[test]
    fn schema_with_time_index_populates_metadata() {
        let s = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("value", DataType::Float64),
            ],
            0,
            vec![vec![0, 1]],
        );
        assert_eq!(s.time_index, Some(0));
        assert!(s.has_unique_key());
        assert_eq!(s.unique_keys, vec![vec![0, 1]]);
    }

    #[test]
    fn add_unique_key_dedupes() {
        let mut s = Schema::new(vec![col("a", DataType::Utf8), col("b", DataType::Utf8)]);
        s.add_unique_key(vec![0]);
        s.add_unique_key(vec![0]);
        s.add_unique_key(vec![0, 1]);
        assert_eq!(s.unique_keys, vec![vec![0], vec![0, 1]]);
    }

    #[test]
    fn schema_serde_roundtrip() {
        let s = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("value", DataType::Float64),
            ],
            0,
            vec![vec![0]],
        );
        let json = serde_json::to_string(&s).unwrap();
        let back: Schema = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn legacy_pre_asap_schema_deserializes() {
        // Pre-unification `Schema`: `columns`, bare dtypes, explicit `closed`.
        let json = r#"{"columns":[
            {"name":"ts","dtype":"timestamp","nullable":false,"table":null},
            {"name":"value","dtype":"float64","nullable":true,"table":"t"}
        ],"time_index":0,"unique_keys":[[0]],"closed":true}"#;
        let back: Schema = serde_json::from_str(json).unwrap();
        let mut expected = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                Field::plain("value", DataType::Float64, true).with_table("t"),
            ],
            0,
            vec![vec![0]],
        );
        expected.closed = true;
        assert_eq!(back, expected);
    }

    #[test]
    fn legacy_pre_asap_schema_without_closed_or_table_is_open() {
        // Older still: written before `closed` and `Field.table` existed.
        let json = r#"{"columns":[{"name":"a","dtype":"utf8","nullable":false}]}"#;
        let back: Schema = serde_json::from_str(json).unwrap();
        assert_eq!(back, Schema::new(vec![col("a", DataType::Utf8)]));
        assert!(!back.closed, "absent `closed` ⇒ open");
    }

    #[test]
    fn legacy_summary_schema_deserializes_as_lifted() {
        // Pre-unification post-ASAP `SummarySchema`: no `closed`/`unique_keys`.
        let json = r#"{"fields":[
            {"name":"ts","dtype":{"Plain":"timestamp"},"nullable":false},
            {"name":"v","dtype":{"Plain":"float64"},"nullable":false}
        ],"time_index":0}"#;
        let back: Schema = serde_json::from_str(json).unwrap();
        assert_eq!(
            back,
            Schema::lifted(
                vec![col("ts", DataType::Timestamp), col("v", DataType::Float64)],
                Some(0)
            )
        );
    }

    #[test]
    fn schema_without_fields_is_rejected() {
        assert!(serde_json::from_str::<Schema>(r#"{"closed":true}"#).is_err());
    }

    #[test]
    fn qualified_field_serde_roundtrip() {
        let c = col("service", DataType::Utf8).with_table("hosts");
        let back: Field = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.table.as_deref(), Some("hosts"));
    }
}
