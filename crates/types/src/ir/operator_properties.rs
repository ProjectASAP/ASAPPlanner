//! Parameters shared by ordinary and summary operators: sources, grouping,
//! matching, temporal selection, joins, sets, and windows.
use crate::pre_asap::{ArithmeticOpKind, ColumnId, ColumnRef, CompareOpKind, ScalarValue};
use serde::{Deserialize, Serialize};
/// The column-reference type a vocabulary item is generic over:
/// positional [`ColumnId`] once bound, name-based [`ColumnRef`] before.
pub trait ColState:
    Clone + std::fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>
{
}

impl ColState for ColumnId {}

impl ColState for ColumnRef {}

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
