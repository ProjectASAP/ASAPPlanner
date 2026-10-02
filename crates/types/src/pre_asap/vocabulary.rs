//! Shared operator vocabulary: the field types the unified IR's operators
//! ([`crate::ir`]) are built from — grouping keys, reductions, sources, join /
//! set-op / window kinds, PromQL modifiers — plus [`aggregate_output_schema`],
//! the one schema derivation for an `Aggregate`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::agg_intent::AggIntent;
use super::expr_ir::{ArithmeticOpKind, ColumnRef, CompareOpKind, ScalarValue};
use super::schema::{ColumnId, DataType, Field, FieldDataType, Schema};
/// The column-reference type a vocabulary item is generic over:
/// positional [`ColumnId`] once bound, name-based [`ColumnRef`] before.
pub trait ColState:
    Clone + std::fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>
{
}

impl ColState for ColumnId {}

impl ColState for ColumnRef {}

/// Errors from schema derivation over a canonical tree.
#[derive(Debug, Error)]
pub enum QueryExprError {
    #[error("invalid scalar function signature: {0}")]
    InvalidScalarSignature(String),
    #[error("by-column id {0} out of range (input has {1} columns)")]
    InvalidGroupByColumn(ColumnId, usize),
    #[error("Concat requires at least one child")]
    EmptyConcat,
    #[error("invalid per-series sample column: {0}")]
    InvalidSampleColumn(String),
}

// ── Leaf / supporting types ───────────────────────────────────────────────────

/// Positional grouping keys, shared by every "operate per group" operator:
/// `Aggregate.by` (reduce per group), `Sort.partition_by` (rank per group —
/// including generic `topk`/`bottomk`), and `SQLWindowFunc.partition_by` (window
/// per group). One spelling so grouping has a single home to evolve. Empty
/// (and `by`) = no grouping (a global operation).
///
/// Heavy-hitter `AggIntent::TopK` carries its grouping here too, via the
/// enclosing `Aggregate.by` (issue #13) — so reduce, rank, and window groupings
/// all share this one type.
///
/// ## `by` vs `without` (issue #39)
///
/// The stored [`keys`](Self::keys) are **kept** labels for `by(...)` and
/// **excluded** labels for `without(...)`. PromQL's `without(labels)` groups by
/// every label *except* those listed; the complement can't be enumerated at
/// lowering time under an open (usage-derived) schema, so it is deferred to the
/// runtime — the excluded positions are stored, the kept set stays open. Only
/// `Aggregate` ever produces the `without` form; `Sort` / `SQLWindowFunc` /
/// `PromqlSeriesSample` groupings are always `by`.
///
/// Serialises as a bare array for the (overwhelmingly common) `by` case —
/// wire-compatible with the `Vec<ColumnId>` this field held before — and as
/// `{"without": [...]}` for the exclusion case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKeys<C = ColumnId> {
    keys: Vec<C>,
    without: bool,
}

// Not `#[derive(Default)]`: derive would add a `C: Default` bound, but an
// empty key set needs nothing from `C` — `ColumnRef` has no meaningful
// default anyway.
impl<C> Default for GroupKeys<C> {
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            without: false,
        }
    }
}

impl<C> GroupKeys<C> {
    /// An empty key set — a global (ungrouped) operation.
    pub fn none() -> Self {
        Self::default()
    }
    /// `by(keys)` — group by exactly these columns.
    pub fn by(keys: Vec<C>) -> Self {
        Self {
            keys,
            without: false,
        }
    }
    /// `without(keys)` — group by every label *except* these (issue #39). The
    /// kept set is runtime-resolved; only the excluded positions are stored.
    pub fn without(keys: Vec<C>) -> Self {
        Self {
            keys,
            without: true,
        }
    }
    /// Whether this is a `without(...)` exclusion grouping.
    pub fn is_without(&self) -> bool {
        self.without
    }
    /// The named keys — kept labels for `by`, excluded labels for `without`.
    pub fn keys(&self) -> &[C] {
        &self.keys
    }
}

impl<C> std::ops::Deref for GroupKeys<C> {
    type Target = [C];
    fn deref(&self) -> &Self::Target {
        &self.keys
    }
}

impl<C> From<Vec<C>> for GroupKeys<C> {
    fn from(keys: Vec<C>) -> Self {
        Self::by(keys)
    }
}

impl<C> FromIterator<C> for GroupKeys<C> {
    fn from_iter<I: IntoIterator<Item = C>>(iter: I) -> Self {
        Self::by(iter.into_iter().collect())
    }
}

impl<'a, C> IntoIterator for &'a GroupKeys<C> {
    type Item = &'a C;
    type IntoIter = std::slice::Iter<'a, C>;
    fn into_iter(self) -> Self::IntoIter {
        self.keys.iter()
    }
}

/// Compare directly against a `Vec<C>` so call sites and tests can keep
/// writing `keys == vec![..]` / `assert_eq!(keys, &vec![..])`. A `without`
/// grouping never equals a bare `by` list.
impl<C: PartialEq> PartialEq<Vec<C>> for GroupKeys<C> {
    fn eq(&self, other: &Vec<C>) -> bool {
        !self.without && &self.keys == other
    }
}

/// (De)serialise as a bare array for `by`, or `{"without": [...]}` for the
/// exclusion form — keeping the `by` wire format identical to the old newtype.
/// Borrowed for `Serialize` (no `C: Clone` needed to write one out), owned for
/// `Deserialize` (there's nothing to borrow from).
#[derive(Serialize)]
#[serde(untagged)]
enum GroupKeysReprRef<'a, C> {
    By(&'a [C]),
    Without { without: &'a [C] },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupKeysRepr<C> {
    By(Vec<C>),
    Without { without: Vec<C> },
}

impl<C: Serialize> Serialize for GroupKeys<C> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.without {
            GroupKeysReprRef::Without {
                without: self.keys.as_slice(),
            }
            .serialize(serializer)
        } else {
            GroupKeysReprRef::By(self.keys.as_slice()).serialize(serializer)
        }
    }
}

impl<'de, C: Deserialize<'de>> Deserialize<'de> for GroupKeys<C> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match GroupKeysRepr::deserialize(deserializer)? {
            GroupKeysRepr::By(keys) => Self::by(keys),
            GroupKeysRepr::Without { without } => Self::without(without),
        })
    }
}

/// Which data model a `Source` / `AggIntent` operates over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataModel {
    TimeSeries,
    Tabular,
    Any,
}

/// The leaf data source of a `Scan`. The schema itself rides on the
/// `Scan.schema` field (SchemaResolver-built); `Source` carries only the leaf's
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
/// [`ArithmeticOpKind`] / [`CompareOpKind`] so every arithmetic/comparison
/// operator has exactly one representation (and one `Display`) across the IR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOpKind {
    /// Arithmetic — `Add/Sub/Mul/Div/Mod` (shared with `ScalarExpr::Arithmetic`).
    Arithmetic(ArithmeticOpKind),
    /// Comparison — `Eq/Ne/Lt/Le/Gt/Ge` + `Like/ILike/Regex` family (shared
    /// with `ScalarExpr::Compare`).
    Compare(CompareOpKind),
    /// PromQL vector-set operation.
    Set(PromQLVectorSetOpKind),
}

impl std::fmt::Display for BinaryOpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryOpKind::Arithmetic(op) => write!(f, "{op}"),
            BinaryOpKind::Compare(op) => write!(f, "{op}"),
            BinaryOpKind::Set(PromQLVectorSetOpKind::And) => f.write_str("AND"),
            BinaryOpKind::Set(PromQLVectorSetOpKind::Or) => f.write_str("OR"),
            BinaryOpKind::Set(PromQLVectorSetOpKind::Unless) => f.write_str("unless"),
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
    /// Left semi-join — each left row that has **at least one** match, once.
    /// `WHERE c IN (SELECT …)` / `WHERE EXISTS (…)` (issue #111).
    ///
    /// Output schema is the **left's alone**; the right side is a filter, not a
    /// source of columns. The join predicate still resolves against the
    /// concatenated `left ++ right` schema — its scope is deliberately wider
    /// than the node's output.
    Semi,
    /// Left anti-join — each left row with **no** match. `WHERE NOT EXISTS (…)`.
    /// Same schema rule as [`JoinKind::Semi`].
    ///
    /// Note this is *not* `NOT IN (SELECT …)`: under SQL's three-valued logic a
    /// NULL on the right makes `NOT IN` yield no rows at all, where an anti-join
    /// yields every left row. The SQL front end rejects `NOT IN (subquery)`
    /// rather than lower it here.
    Anti,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationalSetOpKind {
    Union,
    Intersect,
    Except,
}

/// PromQL vector-set operator used by [`BinaryOpKind::Set`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromQLVectorSetOpKind {
    And,
    Or,
    Unless,
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
    /// ClickHouse `lagInFrame`/`leadInFrame`: unlike [`Lag`](Self::Lag)/[`Lead`](Self::Lead),
    /// these respect the window frame bounds (NULL/default past the frame edge)
    /// rather than reaching arbitrarily far back/forward. Kept as distinct
    /// variants so the frame clause is never silently discarded by conflating
    /// them with `Lag`/`Lead` (#267). `WindowFuncKind` still has no frame
    /// representation, so today these lower and behave exactly like
    /// `Lag`/`Lead` — the tag is correct, the frame-respecting behavior isn't
    /// implemented yet. See #231 for modeling window frames properly.
    LagInFrame,
    LeadInFrame,
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

/// A window's frame-spec (`ROWS`/`RANGE BETWEEN … AND …`) — which rows around
/// the current one an analytic window function reads. `GROUPS` is rejected at
/// lowering time (issue #268): every SQL corpus in this repo uses only `ROWS`,
/// and nothing downstream interprets frame semantics yet, so it isn't worth
/// modelling untested.
///
/// Meaningless (but harmless) on the rank-only and navigation functions
/// (`ROW_NUMBER`/`RANK`/`DENSE_RANK`/`LAG`/`LEAD`), which ignore the frame per
/// SQL semantics — DataFusion still attaches one, stored here verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowFrame {
    pub units: WindowFrameUnits,
    pub start_bound: WindowFrameBound,
    pub end_bound: WindowFrameBound,
}

/// A finite window-frame displacement. Intervals are normalized to Arrow's
/// month/day/nanosecond representation so SQL `RANGE INTERVAL ...` bounds
/// survive lowering without leaking DataFusion types into the canonical IR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFrameOffset {
    Scalar(ScalarValue),
    Interval {
        months: i32,
        days: i32,
        nanoseconds: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFrameUnits {
    /// Boundaries count physical rows: `ROWS BETWEEN 2 PRECEDING AND CURRENT ROW`.
    Rows,
    /// Boundaries count by value-distance on the (single) `ORDER BY` column:
    /// `RANGE BETWEEN INTERVAL '1' HOUR PRECEDING AND CURRENT ROW`.
    Range,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFrameBound {
    /// `UNBOUNDED PRECEDING` is
    /// `Preceding(WindowFrameOffset::Scalar(ScalarValue::Null))`.
    Preceding(WindowFrameOffset),
    CurrentRow,
    /// `UNBOUNDED FOLLOWING` is
    /// `Following(WindowFrameOffset::Scalar(ScalarValue::Null))`.
    Following(WindowFrameOffset),
}

/// A symbolic label matcher on the **info metric** side of an
/// [`crate::ir::NonASAPOp::PromqlInfoEnrich`] (issue #84). Unlike a `Scan` predicate it is not
/// resolved positionally — it references the info metric's labels (`__name__`
/// picks the metric, the rest constrain data labels), which aren't in the input
/// vector's schema; the post-ASAP realization pass applies it against the info metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoMatcher {
    pub label: String,
    /// One of `Eq` / `Ne` / `Regex` / `NotRegex` (PromQL `=`/`!=`/`=~`/`!~`).
    pub op: CompareOpKind,
    pub value: String,
}

/// Series-sampling selection mode (PromQL `limitk` / `limit_ratio`, issue #86).
/// A [`crate::ir::NonASAPOp::PromqlSeriesSample`] keeps a *subset of whole series*, unchanged — it does
/// not rank or reduce, so it is distinct from `TopK` and from `Sort → Limit`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleKind {
    /// `limitk(k, v)` — up to `k` series per group. Which series survive is
    /// deterministic across evaluations but otherwise unspecified (no ordering).
    LimitK(usize),
    /// `limit_ratio(r, v)` — a deterministic `r`-fraction of series per group.
    /// `r ∈ [-1, 1]`; a negative `r` selects the complementary fraction.
    LimitRatio(f64),
}

/// PromQL vector-match modifier (`on`/`ignoring` + `group_left`/`group_right`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorMatch {
    pub kind: VectorMatchKind,
    pub labels: Vec<String>,
    pub grouping: Option<VectorGrouping>,
}

/// PromQL `@` modifier — pins a selector's evaluation time to an anchor instead
/// of the query evaluation time (issue #40).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AtModifier {
    /// `@ start()` — the query range's start instant.
    Start,
    /// `@ end()` — the query range's end instant.
    End,
    /// `@ <ts>` — an absolute instant, milliseconds since the Unix epoch (may be
    /// negative). PromQL writes the timestamp in seconds; the front end scales it.
    Timestamp(i64),
}

/// PromQL per-selector **time-shift** modifiers — `offset` and `@` (issue #40).
/// Neither changes a selector's *schema*; both move *when* it is evaluated, so
/// the shift is a pass-through wrapper ([`crate::ir::NonASAPOp::TimeShift`]) over the
/// selector rather than a new leaf shape. The runtime resolves the anchor and
/// applies the offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TimeShift {
    /// `offset <d>` as signed milliseconds — a positive value shifts the
    /// lookback *back* in time (`offset 5m`), a negative value shifts it
    /// *forward* (`offset -5m`). `0` = no offset.
    pub offset_ms: i64,
    /// `@` anchor; `None` = evaluate at the query time.
    pub at: Option<AtModifier>,
}

impl TimeShift {
    /// Whether this shift is the identity (no `offset`, no `@`) — the state of
    /// every selector that carries neither modifier.
    pub fn is_identity(&self) -> bool {
        self.offset_ms == 0 && self.at.is_none()
    }
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

// ── Intent algebra IR ────────────────────────────────────────────────────────

/// What kind of computation an `Aggregate` node performs — orthogonal to
/// *which* columns it groups by (that's still [`GroupKeys`], inside
/// `Reduce`). Explicit, decided once by whichever pass constructs the node
/// (structural, at front-end lowering time), rather than inferred downstream from
/// whether a grouping-key list happens to be empty or from a neighboring
/// node's shape. See design proposal #165.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reduction<C = ColumnId> {
    /// Collapses input rows via `by` — `by`/`without` semantics are exactly
    /// [`GroupKeys`]'s. May still collapse every row into one (an empty,
    /// non-`without` `by`) — that's a genuine reduction with zero grouping
    /// columns, not "no grouping concept."
    Reduce(GroupKeys<C>),
    /// No grouping concept at all: preserves one output row per input
    /// entity (e.g. a per-series windowed computation with no `by(...)`
    /// clause to begin with, because there's no aggregation operator here
    /// for such a clause to attach to). Never merges across entities, and
    /// never collapses an entity's own row structure (e.g. a time axis) —
    /// unlike `Reduce(GroupKeys::without(vec![]))` ("group by every
    /// label"), which is still a genuine reduction and does collapse it.
    PerEntity,
}

impl<C> Reduction<C> {
    /// Shorthand for the common case — group by these (possibly empty)
    /// keys, kept rather than excluded.
    pub fn by(keys: Vec<C>) -> Self {
        Self::Reduce(GroupKeys::by(keys))
    }

    /// The grouping keys, if this is a genuine reduction — `None` for
    /// `PerEntity`, which has no grouping-keys concept to report.
    pub fn group_keys(&self) -> Option<&GroupKeys<C>> {
        match self {
            Self::Reduce(by) => Some(by),
            Self::PerEntity => None,
        }
    }

    /// The grouping keys, panicking if this is `PerEntity` — for call sites
    /// (tests, mostly) that already know, from the shape they built or are
    /// asserting on, that this must be a genuine reduction. Prefer
    /// [`group_keys`](Self::group_keys) wherever the caller can't assume that.
    pub fn expect_reduce(&self) -> &GroupKeys<C> {
        match self {
            Self::Reduce(by) => by,
            Self::PerEntity => panic!("expected Reduction::Reduce, got PerEntity"),
        }
    }
}

/// A caller-proven compound unique key for a [`crate::ir::NonASAPOp::Concat`] (issue
/// #228) — built only via [`ConcatDiscriminatorKey::new`], never by naming `discriminator` directly
/// in a struct literal (both fields are private): from *other Rust code*,
/// the only way to end up with one of these is to hand over a specific
/// column as the discriminator, by name, at the call site.
///
/// Caveat: this is a Rust-API-level guarantee, not a data-level one. The
/// derived `Deserialize` impl below builds a `ConcatDiscriminatorKey`
/// directly from field values, bypassing `new()`. Deserialization is therefore
/// equivalent to a caller supplying the assertion directly; it does not prove
/// either fact below. An external boundary accepting IR data must
/// reject this field or validate both obligations before treating it as
/// uniqueness evidence.
///
/// # Soundness
///
/// `Concat`'s default (see its own doc) is to drop `unique_keys`
/// unconditionally, because a key unique **within** one branch is not unique
/// **across** the concatenation unless the branches' value sets for that key
/// are provably disjoint — nothing about matching schemas or matching
/// per-branch keys establishes that on its own. Two different branches can
/// trivially emit the same `inner_key` value (e.g. two PromQL
/// `histogram_quantiles` branches keyed on `(host, le)` can both produce a
/// `(host, le)` pair for different φ).
///
/// Prepending `discriminator` restores a compound key only when two facts
/// hold: `inner_key` uniquely identifies rows **within every branch**, and
/// `discriminator`'s value is **guaranteed to differ between branches** — a
/// literal the producer just tagged the branch with (PromQL φ riding along via
/// [`crate::ir::NonASAPOp::PromqlRelabel`], a Postgres-style synthetic `GROUPING()` id
/// for `ROLLUP`/`CUBE`, …), never something inferred structurally from the
/// branches' own data — then `discriminator` alone partitions rows into
/// disjoint sets independent of what the branches actually contain, so
/// `(discriminator, inner_key)` is sound even when otherwise-identical
/// `inner_key` values occur in different branches. Neither fact is verified
/// here; both are part of the caller-proven claim.
///
/// This is a **caller-proven claim, not something `Concat` can verify**:
/// nothing stops a caller from asserting a discriminator that in fact
/// repeats across branches, in which case the resulting `unique_keys` claim
/// is simply wrong — `output_schema` trusts it without checking. The
/// obligation is on the constructor call site, exactly as it is on
/// [`crate::ir::NonASAPOp::Dedup`]'s `cols` or any other unverified `unique_keys`
/// producer in this module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(bound(serialize = "C: ColState", deserialize = "C: ColState"))]
pub struct ConcatDiscriminatorKey<C: ColState = ColumnId> {
    discriminator: C,
    inner_key: Vec<C>,
}

impl<C: ColState> ConcatDiscriminatorKey<C> {
    /// The only constructor — `discriminator` must be named explicitly by
    /// the caller. See the type's doc for the soundness obligation this
    /// puts on that caller.
    pub fn new(discriminator: C, inner_key: Vec<C>) -> Self {
        Self {
            discriminator,
            inner_key,
        }
    }

    pub fn discriminator(&self) -> &C {
        &self.discriminator
    }

    pub fn inner_key(&self) -> &[C] {
        &self.inner_key
    }
}

/// Output schema of a *per-series* window/range reduction (`rate`/`increase`,
/// or an `*_over_time` reducer under a time `Window`). Such a reduction emits
/// one value per series, so every label column of `input` is preserved and only
/// the sample value is replaced — kept named `value` so the PromQL sample-value
/// convention (and any outer `SampleValue` reference) still resolves it by name.
fn per_series_reduction_schema(input: &Schema, agg: &AggIntent) -> Result<Schema, QueryExprError> {
    let vi = if let Some(index) = agg.input_cols().first() {
        *index
    } else {
        super::column_resolution::resolve_column_ref(&ColumnRef::SampleValue, input)
            .map_err(|error| QueryExprError::InvalidSampleColumn(error.to_string()))?
    };
    if !matches!(
        input.fields.get(vi).map(|column| &column.dtype),
        Some(FieldDataType::Plain(DataType::Float64 | DataType::Int64))
    ) {
        return Err(QueryExprError::InvalidSampleColumn(format!(
            "column {vi} is not numeric"
        )));
    }
    let mut columns = input.fields.clone();
    {
        let mut out = agg.output_column(&columns[vi]);
        out.name = "value".into();
        // A per-series range reduction produces a PromQL sample value, which is
        // always `float64` — override the reducer's own output dtype so
        // `count_over_time` (whose `Count` intent types `Int64`) matches every
        // other range reducer instead of leaking an `Int64` value column (#69).
        out.dtype = FieldDataType::Plain(DataType::Float64);
        columns[vi] = out;
    }
    Ok(Schema {
        fields: columns,
        time_index: input.time_index,
        unique_keys: input.unique_keys.clone(),
        // Per-series reduction is label-preserving: it inherits its input's
        // completeness (an open scan stays open; a closed one stays closed).
        closed: input.closed,
    })
}

/// The output schema of an `Aggregate { reduction, measures }` over `in_schema` —
/// the **single** canonical derivation shared by
/// [`NonASAPOp::output_schema`](crate::ir::NonASAPOp::output_schema)'s
/// `Aggregate` arm and the HAVING-resolution path (`column_resolution::output_schema_for_aggregate`),
/// so the two can never drift (issue #41).
///
/// `Reduction::PerEntity` selects the label-preserving
/// [`per_series_reduction_schema`] (`rate`/`increase`/`*_over_time`) instead
/// of the cross-series `by ++ measures` shape. Which one applies is read directly
/// off `reduction` — decided once, at construction, by whoever built the
/// `Aggregate` node (issue #165) — not re-derived here from `by`/child shape.
pub fn aggregate_output_schema(
    in_schema: &Schema,
    reduction: &Reduction,
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<Schema, QueryExprError> {
    let by = match reduction {
        Reduction::PerEntity => {
            debug_assert_eq!(
                measures.len(),
                1,
                "a per-entity reduction is single-aggregate"
            );
            return per_series_reduction_schema(in_schema, &measures[0]);
        }
        Reduction::Reduce(by) => by,
    };

    // `without(excluded)` groups by every label *except* those listed: the kept
    // labels are the input's label columns minus the excluded positions (and the
    // ts / sample-value columns), and the schema stays **open** because the full
    // runtime label set isn't known. The `by(...)` path instead enumerates its
    // kept columns and freezes to closed (issue #39).
    if by.is_without() {
        return without_output_schema(in_schema, by.keys(), measures, output_names);
    }

    let mut out_cols: Vec<Field> = Vec::with_capacity(by.len() + measures.len());
    for &id in by.keys() {
        let c = in_schema
            .fields
            .get(id)
            .ok_or(QueryExprError::InvalidGroupByColumn(
                id,
                in_schema.fields.len(),
            ))?;
        out_cols.push(c.clone());
    }
    let value_col_idx =
        super::column_resolution::resolve_column_ref(&ColumnRef::SampleValue, in_schema)
            .ok()
            .or_else(|| (0..in_schema.fields.len()).find(|i| !by.contains(i)));
    let probe = value_col_idx
        .and_then(|i| in_schema.fields.get(i))
        .cloned()
        .unwrap_or_else(|| Field::plain("value", DataType::Float64, false));
    // Each reducer types off its own input column (`SUM(bytes)` vs `AVG(latency)`
    // in one node); `None` falls back to the sample-value probe (PromQL's
    // single-column convention). A non-empty `output_names[i]` overrides the
    // synthetic output column name.
    for (i, intent) in measures.iter().enumerate() {
        // `count_values("l", v)` emits TWO columns: the synthesized `Utf8` label
        // `l` (the stringified sample value it groups by) and the per-value
        // count. If `l` collides with a group-by key of the same name, PromQL's
        // synthesized label takes precedence — emit a single column, never a
        // duplicate.
        if let AggIntent::CountValues { label } = intent {
            if !out_cols.iter().any(|c| c.name == *label) {
                out_cols.push(Field::plain(label.clone(), DataType::Utf8, false));
            }
            let mut cnt = intent.output_column(&probe);
            if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
                cnt.name = name.clone();
            }
            out_cols.push(cnt);
            continue;
        }
        // Only the output *type* is read from here, so the leading column is
        // enough for the multi-column intents: `Cardinality` and `PearsonCorr`
        // both have a fixed output type that ignores it.
        let in_col = intent
            .input_cols()
            .first()
            .and_then(|id| in_schema.fields.get(*id))
            .unwrap_or(&probe);
        let mut out = intent.output_column(in_col);
        // A global extremum emits NULL for an empty input, even if its input
        // column is non-nullable. Grouped extrema only emit existing groups.
        if by.is_empty() && matches!(intent, AggIntent::Min { .. } | AggIntent::Max { .. }) {
            out.nullable = true;
        }
        if let Some((arg, _)) = intent
            .arg_selector_columns(in_schema)
            .map_err(QueryExprError::InvalidScalarSignature)?
        {
            out.dtype = in_schema.fields[arg].dtype.clone();
            out.nullable = in_schema.fields[arg].nullable;
        }
        if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
            out.name = name.clone();
        }
        out_cols.push(out);
    }
    // `count_values` groups by (by-keys ∪ the synthesized value label), so the
    // by-keys alone are not a unique key — be conservative and claim none.
    let has_count_values = measures
        .iter()
        .any(|a| matches!(a, AggIntent::CountValues { .. }));
    let unique_keys = if by.is_empty() || has_count_values {
        Vec::new()
    } else {
        vec![(0..by.len()).collect()]
    };
    Ok(Schema {
        fields: out_cols,
        time_index: None,
        unique_keys,
        // A cross-series aggregate enumerates exactly `by ++ measures`, so its output
        // is closed even over an open input — this is where an open schema
        // freezes to closed.
        closed: true,
    })
}

/// Output schema of a `without(excluded)` aggregate: the kept labels (every
/// input label column except the `excluded` positions, the time axis, and the
/// sample-value column) followed by the aggregate output column(s). Unlike the
/// `by` path this stays **open** — the excluded set is enumerable but the kept
/// set is not (the runtime carries labels the usage-derived schema never saw),
/// so the schema can't freeze to closed and claims no unique key (issue #39).
fn without_output_schema(
    in_schema: &Schema,
    excluded: &[ColumnId],
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<Schema, QueryExprError> {
    for &id in excluded {
        if id >= in_schema.fields.len() {
            return Err(QueryExprError::InvalidGroupByColumn(
                id,
                in_schema.fields.len(),
            ));
        }
    }
    let mut out_cols: Vec<Field> = Vec::new();
    for (i, col) in in_schema.fields.iter().enumerate() {
        let is_time = in_schema.time_index == Some(i);
        let is_value =
            super::column_resolution::resolve_column_ref(&ColumnRef::SampleValue, in_schema).ok()
                == Some(i);
        if !is_time && !is_value && !excluded.contains(&i) {
            out_cols.push(col.clone());
        }
    }
    let probe = in_schema
        .column_id("value")
        .and_then(|i| in_schema.fields.get(i))
        .cloned()
        .unwrap_or_else(|| Field::plain("value", DataType::Float64, false));
    for (i, intent) in measures.iter().enumerate() {
        // Only the output *type* is read from here, so the leading column is
        // enough for the multi-column intents: `Cardinality` and `PearsonCorr`
        // both have a fixed output type that ignores it.
        let in_col = intent
            .input_cols()
            .first()
            .and_then(|id| in_schema.fields.get(*id))
            .unwrap_or(&probe);
        let mut out = intent.output_column(in_col);
        if let Some((arg, _)) = intent
            .arg_selector_columns(in_schema)
            .map_err(QueryExprError::InvalidScalarSignature)?
        {
            out.dtype = in_schema.fields[arg].dtype.clone();
            out.nullable = in_schema.fields[arg].nullable;
        }
        if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
            out.name = name.clone();
        }
        out_cols.push(out);
    }
    Ok(Schema {
        fields: out_cols,
        time_index: None,
        unique_keys: Vec::new(),
        // The kept label set is runtime-only, so — unlike `by` — this does not
        // freeze the open schema to closed.
        closed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field::plain(name, dtype, nullable)
    }

    #[test]
    fn group_keys_by_vs_without_semantics() {
        let by = GroupKeys::by(vec![1, 2]);
        let without = GroupKeys::without(vec![1, 2]);
        assert!(!by.is_without());
        assert!(without.is_without());
        // Deref / iteration expose the stored keys regardless of mode.
        assert_eq!(by.len(), 2);
        assert_eq!(without.keys(), &[1, 2]);
        // A `by` compares equal to its bare vec; a `without` never does.
        assert_eq!(by, vec![1, 2]);
        assert_ne!(without, vec![1, 2]);
        assert_ne!(by, without);
    }

    #[test]
    fn group_keys_serde_by_is_bare_array_without_is_tagged() {
        // `by` keeps the pre-#39 bare-array wire format; `without` uses an object.
        let by = serde_json::to_string(&GroupKeys::by(vec![2, 3])).unwrap();
        assert_eq!(by, "[2,3]");
        let without = serde_json::to_string(&GroupKeys::without(vec![2])).unwrap();
        assert_eq!(without, r#"{"without":[2]}"#);
        // Round-trip both.
        for g in [GroupKeys::by(vec![2, 3]), GroupKeys::without(vec![2])] {
            let json = serde_json::to_string(&g).unwrap();
            let back: GroupKeys = serde_json::from_str(&json).unwrap();
            assert_eq!(back, g);
        }
    }

    #[test]
    fn time_shift_identity_and_serde() {
        let offset_only = TimeShift {
            offset_ms: 1,
            at: None,
        };
        let at_only = TimeShift {
            offset_ms: 0,
            at: Some(AtModifier::End),
        };
        assert!(TimeShift::default().is_identity());
        assert!(!offset_only.is_identity());
        assert!(!at_only.is_identity());
        // Round-trip the shift + anchor.
        let s = TimeShift {
            offset_ms: -300_000,
            at: Some(AtModifier::Timestamp(60_000)),
        };
        let back: TimeShift = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    // Nested temporal aggregation must replace the sample, never the grouping label.
    #[test]
    fn temporal_reduction_of_grouped_sum_preserves_job() {
        let input = Schema::new(vec![
            col("job", DataType::Utf8, true),
            col("sum", DataType::Float64, false),
        ]);
        for aggregate in [
            AggIntent::Avg { col: None },
            AggIntent::Avg { col: Some(1) },
            AggIntent::Rate,
        ] {
            let output =
                aggregate_output_schema(&input, &Reduction::PerEntity, &[aggregate], &[]).unwrap();
            assert_eq!(output.fields[0], input.fields[0]);
            assert_eq!(output.fields[1].name, "value");
            assert_eq!(output.fields[1].dtype, DataType::Float64);
        }
    }

    #[test]
    fn discriminator_assertion_rejects_unknown_wire_fields() {
        let json = r#"{"discriminator":1,"inner_key":[0],"unverified":true}"#;
        assert!(serde_json::from_str::<ConcatDiscriminatorKey>(json).is_err());
    }

    #[test]
    fn aggregate_strips_time_and_keeps_unique_keys() {
        let input = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("host", DataType::Utf8, false),
            ],
            0,
            Vec::new(),
        );
        let out = aggregate_output_schema(
            &input,
            &Reduction::by(vec![2]),
            &[AggIntent::Sum { col: None }],
            &[],
        )
        .expect("valid group-by column");
        let names: Vec<_> = out.fields.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["host", "sum"]);
        assert!(out.time_index.is_none());
        assert_eq!(out.unique_keys, vec![vec![0]]);
    }
}
