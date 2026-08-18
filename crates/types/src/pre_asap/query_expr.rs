//! The canonical Layer-3 intent algebra IR.
//!
//! Language- and deployment-independent. Box-owned tree; column identity is **positional**
//! (`Aggregate.reduction: Reduction`, wrapping `GroupKeys` for the
//! grouped case), resolved by the [`Binder`](super::binder) against the
//! self-contained [`Schema`] carried on each `Scan`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::agg_intent::AggIntent;
use super::expr_ir::{ArithOp, ColumnRef, CompareOp, L3Scalar};
use super::schema::{Column, ColumnId, DataType, Schema};

/// The column-reference resolution state a [`QueryExpr<C>`] tree carries —
/// [`ColumnId`] (the default, and what the bare `QueryExpr` name has always
/// meant) once the [`Binder`](super::binder::Binder) has resolved every
/// reference positionally, or the front-end-emitted, name-based [`ColumnRef`]
/// before binding. The only place the two states differ in *shape* rather
/// than just in which type fills `C` is [`QueryExpr::Scan`]'s `schema` field:
/// a bound tree's binding schema is always known (the Binder is total, so
/// [`ScanSchema`](Self::ScanSchema) `= Schema`); an unresolved front-end
/// `Scan` knows its schema only when the front end already has it without
/// binding — a SQL leaf, catalog-backed (`Some`) — `None` (PromQL) defers to
/// the Binder, so `ScanSchema = Option<Schema>`.
pub trait ColState:
    Clone + std::fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>
{
    /// What [`QueryExpr::Scan`]'s `schema` field holds for a tree in this state.
    type ScanSchema: Clone + std::fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>;
}

impl ColState for ColumnId {
    type ScanSchema = Schema;
}

impl ColState for ColumnRef {
    type ScanSchema = Option<Schema>;
}

/// Errors from schema derivation over a canonical tree.
#[derive(Debug, Error)]
pub enum QueryExprError {
    #[error("by-column id {0} out of range (input has {1} columns)")]
    InvalidGroupByColumn(ColumnId, usize),
    #[error("Merge requires at least one child")]
    EmptyMerge,
    /// [`QueryExpr::output_schema`] called on (or reached, while recursing, a
    /// child that is) one of the scalar variants (issue #205) — those have no
    /// independent row schema of their own; a scalar expression's *type* only
    /// makes sense against the schema it's embedded in (see `infer_expr_type`,
    /// used by `Project`'s own `output_schema` arm instead).
    #[error("a scalar expression has no row schema of its own")]
    ScalarHasNoRowSchema,
}

// ── Leaf / supporting types ───────────────────────────────────────────────────

/// Positional grouping keys, shared by every "operate per group" L3 operator:
/// `Aggregate.by` (reduce per group), `Sort.partition_by` (rank per group —
/// including generic `topk`/`bottomk`), and `WindowFunc.partition_by` (window
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
/// `Aggregate` ever produces the `without` form; `Sort` / `WindowFunc` /
/// `Sample` groupings are always `by`.
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

/// Operator on the query-level `BinaryOp` node. Reuses the scalar IR's
/// [`ArithOp`] / [`CompareOp`] so every arithmetic/comparison operator has
/// exactly one representation (and one `Display`) across the IR; the remaining
/// variants are PromQL vector-set / power ops with no scalar-IR counterpart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOpKind {
    /// Arithmetic — `Add/Sub/Mul/Div/Mod` (shared with `QueryExpr::Arith`).
    Arith(ArithOp),
    /// Comparison — `Eq/Ne/Lt/Le/Gt/Ge` + `Like/ILike/Regex` family (shared
    /// with `QueryExpr::Compare`).
    Compare(CompareOp),
    /// PromQL logical-set intersection (`and`).
    And,
    /// PromQL logical-set union (`or`).
    Or,
    /// PromQL logical-set complement (`unless`).
    Unless,
    /// Exponentiation (`^`) — PromQL vector op, no scalar-IR counterpart.
    Pow,
    /// `atan2` — PromQL vector op, no scalar-IR counterpart.
    Atan2,
}

impl std::fmt::Display for BinaryOpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryOpKind::Arith(op) => write!(f, "{op}"),
            BinaryOpKind::Compare(op) => write!(f, "{op}"),
            BinaryOpKind::And => f.write_str("AND"),
            BinaryOpKind::Or => f.write_str("OR"),
            BinaryOpKind::Unless => f.write_str("unless"),
            BinaryOpKind::Pow => f.write_str("^"),
            BinaryOpKind::Atan2 => f.write_str("atan2"),
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
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
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

/// A symbolic label matcher on the **info metric** side of an
/// [`QueryExpr::InfoJoin`] (issue #84). Unlike a `Scan` predicate it is not
/// resolved positionally — it references the info metric's labels (`__name__`
/// picks the metric, the rest constrain data labels), which aren't in the input
/// vector's schema; L4 applies it against the info metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoMatcher {
    pub label: String,
    /// One of `Eq` / `Ne` / `Regex` / `NotRegex` (PromQL `=`/`!=`/`=~`/`!~`).
    pub op: CompareOp,
    pub value: String,
}

/// Series-sampling selection mode (PromQL `limitk` / `limit_ratio`, issue #86).
/// A [`QueryExpr::Sample`] keeps a *subset of whole series*, unchanged — it does
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "C: ColState", deserialize = "C: ColState"))]
pub struct SortKey<C: ColState = ColumnId> {
    pub expr: QueryExpr<C>,
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
/// the shift is a pass-through wrapper ([`QueryExpr::TimeShift`]) over the
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

/// A row-level filter predicate (WHERE clause / PromQL label matcher).
/// Boxed: `Predicate<C>` sits directly (not behind a `Vec`) in
/// `Filter.pred`/`Join.pred`/`Aggregate.having`, and `QueryExpr<C>` is
/// self-recursive without further indirection once the scalar variants are
/// part of it — the box is what makes the recursive type's size finite there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "C: ColState", deserialize = "C: ColState"))]
pub struct Predicate<C: ColState = ColumnId>(pub Box<QueryExpr<C>>);

/// One item in a SELECT projection list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "C: ColState", deserialize = "C: ColState"))]
pub struct ProjectItem<C: ColState = ColumnId> {
    pub alias: Option<String>,
    pub expr: QueryExpr<C>,
}

// ── L3 intent algebra IR ──────────────────────────────────────────────────────

/// What kind of computation an `Aggregate` node performs — orthogonal to
/// *which* columns it groups by (that's still [`GroupKeys`], inside
/// `Reduce`). Explicit, decided once by whichever pass constructs the node
/// (structural, at L2→L3 lowering), rather than inferred downstream from
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "C: ColState", deserialize = "C: ColState"))]
pub enum QueryExpr<C: ColState = ColumnId> {
    /// Outermost leaf. `schema` is the **binding schema** — the resolved column
    /// set every positional `ColumnId` in the tree indexes into, *not* a full
    /// description of the runtime row — once bound (`schema: Schema`, always
    /// present: the [`Binder`](super::binder) is total). Before binding, a
    /// front-end-emitted `Scan` (`C = ColumnRef`) knows it only when the front
    /// end already has it without binding — a catalog-backed SQL leaf — `None`
    /// (PromQL) defers to the Binder; see [`ColState::ScanSchema`]. Complete
    /// when catalog-backed (SQL); for schemaless PromQL the bound schema is
    /// usage-derived (the `(ts, value)` floor + the labels the query
    /// references), since a metric's label set is open and known only at
    /// runtime. That distinction is carried explicitly by
    /// [`Schema::closed`](super::schema::Schema::closed) (SQL leaf → `true`,
    /// PromQL leaf → `false`). `predicates` are leaf-level row filters (PromQL
    /// label matchers, pushed-down `WHERE` conjuncts).
    Scan {
        source: Source,
        #[serde(default)]
        predicates: Vec<Predicate<C>>,
        schema: C::ScanSchema,
    },
    /// A scalar constant leaf — a PromQL number literal or a folded constant
    /// scalar expression (`10*1024*1024`). Appears as a [`BinaryOp`](Self::BinaryOp)
    /// operand for `<vector> op <scalar>` thresholds / unit conversions (#35).
    Scalar(f64),

    /// The query **evaluation time** as a scalar (PromQL `time()`) — a runtime
    /// value, not a constant. Also the implicit input of the no-argument
    /// calendar functions (`hour()`, `day_of_week()`, …). Issue #46.
    EvalTime,

    /// PromQL `vector(s)` — the scalar→instant-vector bridge. Promotes a
    /// scalar-typed child to a single label-less series carrying the scalar's
    /// value at every step. Lets a scalar participate where a vector is required
    /// (`up or vector(0)` dead-man's-switch). Issue #48.
    VectorFromScalar(Box<QueryExpr<C>>),

    /// PromQL `scalar(v)` — the instant-vector→scalar bridge. Collapses a
    /// single-element vector to its value (NaN at runtime if the input is not
    /// exactly one series). Lets a vector feed a scalar position (`vector` /
    /// aggregation `k` args, thresholds). Issue #48.
    ScalarFromVector(Box<QueryExpr<C>>),

    /// ρ — a per-series **label rewrite** (PromQL `label_replace` /
    /// `label_join`). Every input row passes through unchanged except for the
    /// destination label `dst`, whose new value is computed by `value` — a
    /// scalar expression over the child's (source) label columns:
    /// `label_replace` → a `label_replace(src, regex, replacement)` function
    /// call (regex capture-expansion), `label_join` → a `label_join(sep, srcs…)`
    /// concatenation. Sample values and the time axis are untouched. Issue #50.
    Relabel {
        /// The label written by this rewrite (PromQL `dst_label`).
        dst: String,
        value: Box<QueryExpr<C>>,
        child: Box<QueryExpr<C>>,
    },

    /// PromQL `info(v, [selector])` — left-join **label enrichment** (#84). Each
    /// series in `child` is enriched with labels from the matching info metric(s)
    /// (`target_info` by default; `selector`'s `__name__` matchers pick the
    /// metric(s), the rest constrain the data labels), joined on their shared
    /// identifying labels. Those join keys are the info metric's identifying
    /// labels — runtime/metadata-resolved, since an open PromQL schema can't
    /// enumerate them — so they are NOT carried here; L4 resolves them from the
    /// info metric's schema. The output keeps `child`'s (open) schema: the
    /// grafted labels appear at runtime.
    InfoJoin {
        #[serde(default)]
        selector: Vec<InfoMatcher>,
        child: Box<QueryExpr<C>>,
    },

    /// Series-sampling **selection** — PromQL `limitk` / `limit_ratio` (#86).
    /// Keeps a subset of whole series per `by` group (empty = global), passing
    /// each surviving series through unchanged. Not a ranking (`TopK`) and not a
    /// reduction: the output schema equals the child's.
    Sample {
        #[serde(default)]
        by: GroupKeys<C>,
        kind: SampleKind,
        child: Box<QueryExpr<C>>,
    },

    /// σ — row-level filter. Output schema = child schema.
    Filter {
        pred: Predicate<C>,
        child: Box<QueryExpr<C>>,
    },
    /// π — column projection.
    Project {
        cols: Vec<ProjectItem<C>>,
        /// Re-qualifies every output column with this table alias (a derived
        /// table / inline view). `None` for an ordinary SELECT list.
        #[serde(default)]
        qualifier: Option<String>,
        child: Box<QueryExpr<C>>,
    },

    /// γ + α — GROUP BY (positional) + aggregate intents.
    Aggregate {
        reduction: Reduction<C>,
        measures: Vec<AggIntent<C>>,
        /// Output column names parallel to `measures`. A non-empty entry overrides
        /// the synthetic intent-keyed name — SQL threads DataFusion's generated
        /// name (e.g. `"sum(metrics.bytes)"`) here so an enclosing `Project`
        /// resolves the aggregate output by the name it references. An empty
        /// entry (or empty vec) falls back to `AggIntent::output_column`'s name
        /// (PromQL's convention).
        #[serde(default)]
        output_names: Vec<String>,
        #[serde(default)]
        having: Option<Predicate<C>>,
        child: Box<QueryExpr<C>>,
    },

    /// δ — SQL `DISTINCT` / row deduplication. Positional like every other L3
    /// column reference; empty = dedup on all columns (`SELECT DISTINCT *`).
    Distinct {
        cols: Vec<C>,
        child: Box<QueryExpr<C>>,
    },
    /// ⊕ — exact, n-ary `UNION ALL` of independent branches. Rows are
    /// concatenated, never deduplicated; SQL's `UNION`/`INTERSECT`/`EXCEPT` are
    /// [`QueryExpr::SetOp`], not this.
    ///
    /// Used for the branches of one query that a single `Aggregate` cannot
    /// express — PromQL `histogram_quantiles` (one branch per φ, issue #109) and
    /// SQL `ROLLUP`/`CUBE`/`GROUPING SETS` (one branch per grouping level, issue
    /// #118) — as well as for sharded / fan-in plans.
    ///
    /// **The branches must be union-compatible; nothing here enforces it.** The
    /// output schema is the *first* child's, so branches that disagree on a
    /// column name or type leave the merged schema silently misdescribing every
    /// branch but one. A producer that cannot guarantee compatibility must
    /// project the branches into a common shape first.
    ///
    /// A row may appear in several branches, so no branch's unique key survives
    /// the union — `unique_keys` is dropped, as in `SetOp`.
    ///
    /// Empty children is an error ([`QueryExprError::EmptyMerge`]), not an
    /// empty relation: there would be no schema to derive.
    Merge { children: Vec<QueryExpr<C>> },

    /// Logical join. L4 picks the physical alternative.
    Join {
        kind: JoinKind,
        pred: Predicate<C>,
        left: Box<QueryExpr<C>>,
        right: Box<QueryExpr<C>>,
    },
    SetOp {
        kind: SetOpKind,
        all: bool,
        left: Box<QueryExpr<C>>,
        right: Box<QueryExpr<C>>,
    },

    /// Generic order-by for non-heavy-hitter cases.
    ///
    /// `partition_by` makes the ordering **per-group**: a non-empty set means
    /// "rank within each `partition_by` group" — the semantics behind PromQL
    /// `topk by (host) (…)` / SQL `… OVER (PARTITION BY host ORDER BY …)`. It is
    /// row-preserving (schema pass-through) and is where the grouping of a
    /// generic (non-heavy-hitter) ranking lives, so there is no separate
    /// `Partition` node (issue #12: reducing GROUP BY → `Aggregate.by`, per-group
    /// ranking → here, parallel sharding → L5). Empty = a global order-by.
    Sort {
        keys: Vec<SortKey<C>>,
        #[serde(default)]
        partition_by: GroupKeys<C>,
        child: Box<QueryExpr<C>>,
    },
    Limit {
        n: usize,
        offset: usize,
        child: Box<QueryExpr<C>>,
    },

    /// PromQL sub-query (`<expr>[range:resolution]`). Logical pass-through.
    Subquery {
        range: Duration,
        #[serde(default)]
        resolution: Option<Duration>,
        child: Box<QueryExpr<C>>,
    },

    /// Temporal range selection — "look back `range` of history for this
    /// computation." Used for all range-vector functions: `rate`, `increase`,
    /// `*_over_time`. The range is distinct from a row-level `Filter`.
    ///
    /// Structural marker: an `Aggregate` whose direct child is a `TimeRange`
    /// is a *per-series* reduction (label-preserving); one whose child is a
    /// plain `Scan` or another `Aggregate` is a *cross-series* reduction.
    TimeRange {
        range: Duration,
        child: Box<QueryExpr<C>>,
    },

    /// PromQL `offset` / `@` **time shift** on a selector (issue #40). A
    /// pass-through wrapper: it moves *when* `child` is evaluated (the runtime
    /// resolves the `@` anchor and applies the offset) but leaves its schema
    /// unchanged. Wraps the shifted selector directly — `m offset 1h` →
    /// `TimeShift { Scan }`; a ranged selector `m[5m] offset 1h` →
    /// `TimeRange { 5m, TimeShift { Scan } }` (the range is taken at the shifted
    /// time). Never carries the identity shift (the converter emits a bare
    /// selector when neither modifier is present).
    TimeShift {
        shift: TimeShift,
        child: Box<QueryExpr<C>>,
    },

    /// SQL analytic window function: `func(args) OVER (PARTITION BY … ORDER BY …)`.
    /// Output schema = child schema + one column named `output_name` (the name
    /// the enclosing `Project` references). Window frames are not modelled yet.
    WindowFunc {
        func: WindowFuncKind,
        /// Operand expressions (`LAG(value)` → `[Column(value_id)]`); empty for
        /// the rank-only functions (`ROW_NUMBER`/`RANK`/`DENSE_RANK`).
        args: Vec<QueryExpr<C>>,
        partition_by: GroupKeys<C>,
        order_by: Vec<SortKey<C>>,
        /// The output column's name — DataFusion's window-expr field name, so a
        /// `Project` above resolves it (cf. `Aggregate.output_names`).
        output_name: String,
        child: Box<QueryExpr<C>>,
    },

    /// Arithmetic / comparison / boolean composition (PromQL binary ops).
    BinaryOp {
        op: BinaryOpKind,
        lhs: Box<QueryExpr<C>>,
        rhs: Box<QueryExpr<C>>,
        #[serde(default)]
        vector_match: Option<VectorMatch>,
    },

    // ── Scalar expression shapes (issue #205) ───────────────────────────
    //
    // Formerly a separate, self-recursive `Expr<C>` tree, reachable from the
    // operator variants above only through wrapper fields (`Predicate`,
    // `ProjectItem`, `SortKey`). They're variants of this same tree now — a
    // scalar sub-expression is only ever reachable through one of those same
    // wrapper positions (`Filter.pred`, `ProjectItem.expr`, `Aggregate.having`,
    // `Relabel.value`, `WindowFunc.args`, …), which is a *convention* this
    // type no longer enforces at compile time the way the old, closed
    // `Expr<C>` variant set did — nothing stops constructing, say, a `Scan`
    // where a `Compare`'s `left` operand belongs. `output_schema` and every
    // scalar-position consumer (`resolve`, `canonicalize`, `infer_expr_type`)
    // reject a non-scalar variant found there instead (a `QueryExprError` or
    // an `unreachable!`, depending on the call site) — the accepted
    // replacement, since the alternative (a marker-trait/sub-enum bound
    // restricting which variants are constructible in a scalar position) adds
    // real type-level machinery for a distinction every constructor already
    // has to get right structurally anyway (a `Filter` is never built with an
    // operator subtree as its `pred`).
    /// A column reference — unresolved [`ColumnRef`] (front-end-emitted, `C =
    /// ColumnRef`) or positional [`ColumnId`] (once bound, `C = ColumnId`).
    Column(C),
    /// A constant literal value.
    Literal(L3Scalar),
    /// `left op right` — binary comparison.
    Compare {
        left: Box<QueryExpr<C>>,
        op: CompareOp,
        right: Box<QueryExpr<C>>,
    },
    /// Flat conjunction (logical AND). An empty list is vacuously true.
    BoolAnd(Vec<QueryExpr<C>>),
    /// Flat disjunction (logical OR). An empty list is vacuously false.
    BoolOr(Vec<QueryExpr<C>>),
    /// Logical NOT.
    Not(Box<QueryExpr<C>>),
    /// `expr IS NULL`.
    IsNull(Box<QueryExpr<C>>),
    /// `expr IS NOT NULL`.
    IsNotNull(Box<QueryExpr<C>>),
    /// `CAST(expr AS to)`; `try_cast` for SQL `TRY_CAST` (NULL on failure).
    Cast {
        expr: Box<QueryExpr<C>>,
        to: DataType,
        try_cast: bool,
    },
    /// `expr [NOT] IN (v1, v2, …)`.
    InList {
        expr: Box<QueryExpr<C>>,
        list: Vec<QueryExpr<C>>,
        negated: bool,
    },
    /// Scalar function call, e.g. `LOWER(col)`, `ABS(x)`.
    FunctionCall {
        name: String,
        args: Vec<QueryExpr<C>>,
    },
    /// Binary arithmetic: `left op right`.
    Arith {
        op: ArithOp,
        left: Box<QueryExpr<C>>,
        right: Box<QueryExpr<C>>,
    },
    /// SQL `CASE` (both searched and simple forms). `operand` present for the
    /// simple form (`CASE expr WHEN …`), absent for searched.
    Case {
        operand: Option<Box<QueryExpr<C>>>,
        branches: Vec<(QueryExpr<C>, QueryExpr<C>)>,
        else_expr: Option<Box<QueryExpr<C>>>,
    },
}

impl<C: ColState> QueryExpr<C> {
    /// If this expression is a `BoolAnd`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn conjuncts(&self) -> &[QueryExpr<C>] {
        match self {
            QueryExpr::BoolAnd(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// If this expression is a `BoolOr`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn disjuncts(&self) -> &[QueryExpr<C>] {
        match self {
            QueryExpr::BoolOr(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// Recursively collect every column reference in a **scalar** subtree —
    /// used by the [`Binder`](super::binder::Binder) to seed usage-derived
    /// leaf schemas, and available to L4 for column-lineage / selectivity.
    /// `self` must be one of the scalar variants (see the module doc on
    /// [`QueryExpr`]'s scalar shapes) — every caller already only reaches
    /// this through a scalar-typed position (`Predicate`, `ProjectItem.expr`,
    /// …), so an operator variant here indicates a construction bug, not a
    /// shape this needs to handle silently.
    pub fn columns_referenced(&self) -> Vec<&C> {
        match self {
            QueryExpr::Column(c) => vec![c],
            QueryExpr::Literal(_) => vec![],
            QueryExpr::Compare { left, right, .. } | QueryExpr::Arith { left, right, .. } => {
                let mut v = left.columns_referenced();
                v.extend(right.columns_referenced());
                v
            }
            QueryExpr::BoolAnd(parts) | QueryExpr::BoolOr(parts) => {
                parts.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            QueryExpr::Not(e) | QueryExpr::IsNull(e) | QueryExpr::IsNotNull(e) => {
                e.columns_referenced()
            }
            QueryExpr::Cast { expr, .. } => expr.columns_referenced(),
            QueryExpr::InList { expr, list, .. } => {
                let mut v = expr.columns_referenced();
                v.extend(list.iter().flat_map(|e| e.columns_referenced()));
                v
            }
            QueryExpr::FunctionCall { args, .. } => {
                args.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            QueryExpr::Case {
                operand,
                branches,
                else_expr,
            } => {
                let mut v = vec![];
                if let Some(op) = operand {
                    v.extend(op.columns_referenced());
                }
                for (when, then) in branches {
                    v.extend(when.columns_referenced());
                    v.extend(then.columns_referenced());
                }
                if let Some(e) = else_expr {
                    v.extend(e.columns_referenced());
                }
                v
            }
            other => unreachable!(
                "columns_referenced called on a non-scalar QueryExpr variant: {other:?}"
            ),
        }
    }
}

/// The canonical, positional Layer-3 tree — what the bare `QueryExpr` name has
/// always meant (the default `C = ColumnId`). Every existing consumer keeps
/// using `QueryExpr` unparameterized; this alias exists only to name the
/// resolved state explicitly at a use site that also wants to name
/// [`L2QueryExpr`] nearby.
pub type L3QueryExpr = QueryExpr<ColumnId>;

/// The front-end-emitted, name-based Layer-2 tree — `QueryExpr<ColumnRef>`,
/// unresolved: front ends construct this directly during their own `interpret`
/// step (issue #179), and the [`Binder`](super::binder) resolves it into
/// [`L3QueryExpr`].
pub type L2QueryExpr = QueryExpr<ColumnRef>;

// `output_schema` needs a fully bound tree — it reads `Scan.schema` as a plain
// `Schema` and resolves every scalar `Expr::Column` positionally — so it lives
// only on the resolved instantiation, not `impl<C: ColState> QueryExpr<C>`.
// Same reasoning as `AggIntent`'s `output_column`/`requires`/`is_per_series`
// (#205): a schema-shaped property that is only meaningful post-binding.
impl QueryExpr<ColumnId> {
    /// Output schema of the root of a canonical tree.
    pub fn output_schema(&self) -> Result<Schema, QueryExprError> {
        match self {
            QueryExpr::Scan { schema, .. } => Ok(schema.clone()),

            QueryExpr::Aggregate {
                reduction,
                measures,
                output_names,
                child,
                ..
            } => {
                let in_schema = child.output_schema()?;
                aggregate_output_schema(&in_schema, reduction, measures, output_names)
            }

            QueryExpr::Filter { child, .. }
            | QueryExpr::Sort { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::Subquery { child, .. }
            // Series sampling keeps a subset of whole series unchanged, so the
            // output schema (and row-uniqueness) is exactly the child's (#86).
            | QueryExpr::Sample { child, .. }
            // Info enrichment adds runtime info labels — the statically-known
            // schema is the child's (open), so it passes through (#84).
            | QueryExpr::InfoJoin { child, .. }
            | QueryExpr::TimeRange { child, .. }
            // A time shift (`offset`/`@`) moves *when* the child is evaluated,
            // never its columns — schema passes through (#40).
            | QueryExpr::TimeShift { child, .. } => child.output_schema(),

            // ρ — relabel preserves every input column and writes one label
            // `dst` (Utf8): overwritten in place if it already exists, else
            // appended (nullable — a `label_replace` regex non-match leaves it
            // unset). The schema stays open (other labels remain runtime-only).
            // A rewrite can collapse two label sets into one, so row-uniqueness
            // is no longer provable — drop unique_keys.
            QueryExpr::Relabel { dst, child, .. } => {
                let mut out = child.output_schema()?;
                if let Some(existing) = out.columns.iter_mut().find(|c| c.name == *dst) {
                    existing.dtype = DataType::Utf8;
                    existing.nullable = true;
                } else {
                    out.columns.push(Column::new(dst.clone(), DataType::Utf8, true));
                }
                out.unique_keys.clear();
                Ok(out)
            }

            // π — one output column per projection item. Each item's type is
            // inferred from its expression against the child schema; the name
            // is the explicit alias or a derived default. Projection may drop
            // the grouping/time columns, so unique_keys reset and time_index
            // is re-found by name.
            QueryExpr::Project { cols, qualifier, child } => {
                let in_schema = child.output_schema()?;
                let columns: Vec<Column> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let (dtype, nullable) = infer_expr_type(&item.expr, &in_schema);
                        let name = item
                            .alias
                            .clone()
                            .unwrap_or_else(|| default_proj_name(&item.expr, i, &in_schema));
                        let c = Column::new(name, dtype, nullable);
                        // A derived table re-qualifies its output columns with
                        // its alias, so `t.col` (and a join over two derived
                        // tables) resolves to the right relation.
                        match qualifier {
                            Some(q) => c.with_table(q),
                            None => c,
                        }
                    })
                    .collect();
                let time_index = columns.iter().position(|c| c.name == "ts");
                Ok(Schema {
                    columns,
                    time_index,
                    unique_keys: Vec::new(),
                    // Projection enumerates exactly its items → closed.
                    closed: true,
                })
            }

            QueryExpr::Distinct { cols, child } => {
                let mut out = child.output_schema()?;
                // Deduplicating on `cols` makes them a unique key of the result.
                if !cols.is_empty() {
                    out.add_unique_key(cols.clone());
                }
                Ok(out)
            }

            // ⊕ — the branches are union-compatible by construction, so the
            // output shape is the first child's. A row can appear in more than
            // one branch, so no key of one branch is a key of the union: drop
            // unique_keys, exactly as `SetOp` does.
            QueryExpr::Merge { children } => {
                let mut s = children
                    .first()
                    .ok_or(QueryExprError::EmptyMerge)
                    .and_then(|c| c.output_schema())?;
                s.unique_keys.clear();
                Ok(s)
            }
            // Set operations are union-compatible: both sides share the left's
            // column shape, so the output schema is the left's. (Row identity
            // is not preserved across a UNION, so unique_keys are dropped.)
            QueryExpr::SetOp { left, .. } => {
                let mut s = left.output_schema()?;
                s.unique_keys.clear();
                Ok(s)
            }
            // ⋈ — output is the concatenation of both inputs' columns. Outer
            // joins make the non-preserved side nullable. Post-join row
            // identity isn't provable in general, so unique_keys reset.
            QueryExpr::Join {
                kind, left, right, ..
            } => {
                let l = left.output_schema()?;
                let r = right.output_schema()?;
                // Semi / anti joins filter the left side; the right contributes
                // no columns, so the output is the left's schema unchanged. Row
                // identity *is* preserved (each left row appears at most once),
                // but a left row can be dropped, so unique_keys still reset.
                if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    return Ok(Schema {
                        unique_keys: Vec::new(),
                        ..l
                    });
                }
                let (left_null, right_null) = match kind {
                    JoinKind::Left => (false, true),
                    JoinKind::Right => (true, false),
                    JoinKind::Full => (true, true),
                    JoinKind::Inner | JoinKind::Cross => (false, false),
                    JoinKind::Semi | JoinKind::Anti => unreachable!("handled above"),
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
                    // The concatenation is complete only if both sides are.
                    closed: l.closed && r.closed,
                })
            }
            // ψ-analytic — child schema + one appended window-output column.
            QueryExpr::WindowFunc {
                func,
                args,
                output_name,
                child,
                ..
            } => {
                let mut out = child.output_schema()?;
                // First operand's (dtype, nullable) from the child schema, owned
                // so the borrow ends before we append.
                let arg = args.first().and_then(|a| match a {
                    QueryExpr::Column(id) => out.columns.get(*id),
                    _ => None,
                });
                let arg_dtype = || arg.map_or(DataType::Float64, |c| c.dtype.clone());
                let (dtype, nullable) = match func {
                    WindowFuncKind::RowNumber
                    | WindowFuncKind::Rank
                    | WindowFuncKind::DenseRank
                    | WindowFuncKind::Count => (DataType::Int64, false),
                    WindowFuncKind::Sum | WindowFuncKind::Avg => (DataType::Float64, true),
                    // Navigation funcs: arg type, nullable (boundary rows are NULL).
                    WindowFuncKind::Lag
                    | WindowFuncKind::Lead
                    | WindowFuncKind::FirstValue
                    | WindowFuncKind::LastValue
                    | WindowFuncKind::NthValue(_) => (arg_dtype(), true),
                    WindowFuncKind::Min | WindowFuncKind::Max => {
                        (arg_dtype(), arg.is_none_or(|c| c.nullable))
                    }
                };
                out.columns
                    .push(Column::new(output_name.clone(), dtype, nullable));
                Ok(out)
            }

            // A scalar constant has no series — model it as a single `value`
            // column so it can sit as a `BinaryOp` operand.
            // Both scalar leaves — a constant and the eval time — are a single
            // `value` column with no labels.
            QueryExpr::Scalar(_) | QueryExpr::EvalTime => Ok(Schema {
                columns: vec![Column::new("value", DataType::Float64, false)],
                time_index: None,
                unique_keys: Vec::new(),
                closed: true,
            }),

            // `vector(s)` yields a label-less instant vector: the (ts, value)
            // floor and nothing else. `closed` — its full label set (empty) is
            // known statically (#48).
            QueryExpr::VectorFromScalar(_) => Ok(Schema {
                columns: vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                ],
                time_index: Some(0),
                unique_keys: Vec::new(),
                closed: true,
            }),

            // `scalar(v)` collapses to a single `value`, no time index — the same
            // scalar shape as a constant or `time()` (#48).
            QueryExpr::ScalarFromVector(_) => Ok(Schema {
                columns: vec![Column::new("value", DataType::Float64, false)],
                time_index: None,
                unique_keys: Vec::new(),
                closed: true,
            }),

            // The output shape of `<vector> op <scalar>` (or `<scalar> op
            // <vector>`) is the vector side's — a scalar operand (a constant or
            // `time()`) contributes only its value, no labels. Prefer the
            // non-scalar side.
            QueryExpr::BinaryOp { lhs, rhs, .. } => match (lhs.as_ref(), rhs.as_ref()) {
                (
                    QueryExpr::Scalar(_) | QueryExpr::EvalTime | QueryExpr::ScalarFromVector(_),
                    r,
                ) => r.output_schema(),
                (l, _) => l.output_schema(),
            },

            // The scalar variants (issue #205) — see `QueryExprError::ScalarHasNoRowSchema`.
            QueryExpr::Column(_)
            | QueryExpr::Literal(_)
            | QueryExpr::Compare { .. }
            | QueryExpr::BoolAnd(_)
            | QueryExpr::BoolOr(_)
            | QueryExpr::Not(_)
            | QueryExpr::IsNull(_)
            | QueryExpr::IsNotNull(_)
            | QueryExpr::Cast { .. }
            | QueryExpr::InList { .. }
            | QueryExpr::FunctionCall { .. }
            | QueryExpr::Arith { .. }
            | QueryExpr::Case { .. } => Err(QueryExprError::ScalarHasNoRowSchema),
        }
    }
}

/// Output schema of a *per-series* window/range reduction (`rate`/`increase`,
/// or an `*_over_time` reducer under a time `Window`). Such a reduction emits
/// one value per series, so every label column of `input` is preserved and only
/// the sample value is replaced — kept named `value` so the PromQL sample-value
/// convention (and any outer `SampleValue` reference) still resolves it by name.
fn per_series_reduction_schema(input: &Schema, agg: &AggIntent) -> Schema {
    let value_idx = input
        .column_id("value")
        .or_else(|| (0..input.columns.len()).find(|&i| Some(i) != input.time_index));
    let mut columns = input.columns.clone();
    if let Some(vi) = value_idx {
        let mut out = agg.output_column(&columns[vi]);
        out.name = "value".into();
        // A per-series range reduction produces a PromQL sample value, which is
        // always `float64` — override the reducer's own output dtype so
        // `count_over_time` (whose `Count` intent types `Int64`) matches every
        // other range reducer instead of leaking an `Int64` value column (#69).
        out.dtype = DataType::Float64;
        columns[vi] = out;
    }
    Schema {
        columns,
        time_index: input.time_index,
        unique_keys: input.unique_keys.clone(),
        // Per-series reduction is label-preserving: it inherits its input's
        // completeness (an open scan stays open; a closed one stays closed).
        closed: input.closed,
    }
}

/// The output schema of an `Aggregate { reduction, measures }` over `in_schema` —
/// the **single** canonical derivation shared by
/// [`QueryExpr::output_schema`]'s `Aggregate` arm and the converter's
/// HAVING-resolution path (`column_resolution::output_schema_for_aggregate`),
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
            return Ok(per_series_reduction_schema(in_schema, &measures[0]));
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

    let mut out_cols: Vec<Column> = Vec::with_capacity(by.len() + measures.len());
    for &id in by.keys() {
        let c = in_schema
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
        .unwrap_or_else(|| Column::new("value", DataType::Float64, false));
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
                out_cols.push(Column::new(label.clone(), DataType::Utf8, false));
            }
            let mut cnt = intent.output_column(&probe);
            if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
                cnt.name = name.clone();
            }
            out_cols.push(cnt);
            continue;
        }
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
        columns: out_cols,
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
        if id >= in_schema.columns.len() {
            return Err(QueryExprError::InvalidGroupByColumn(
                id,
                in_schema.columns.len(),
            ));
        }
    }
    let mut out_cols: Vec<Column> = Vec::new();
    for (i, col) in in_schema.columns.iter().enumerate() {
        let is_time = in_schema.time_index == Some(i);
        let is_value = col.name == "value";
        if !is_time && !is_value && !excluded.contains(&i) {
            out_cols.push(col.clone());
        }
    }
    let probe = in_schema
        .column_id("value")
        .and_then(|i| in_schema.columns.get(i))
        .cloned()
        .unwrap_or_else(|| Column::new("value", DataType::Float64, false));
    for (i, intent) in measures.iter().enumerate() {
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
    Ok(Schema {
        columns: out_cols,
        time_index: None,
        unique_keys: Vec::new(),
        // The kept label set is runtime-only, so — unlike `by` — this does not
        // freeze the open schema to closed.
        closed: false,
    })
}

/// Infer the `(DataType, nullable)` a scalar [`QueryExpr`] produces against an
/// input [`Schema`]. Used by `Project` schema derivation. Approximate at L3:
/// unknown columns and bare `FunctionCall`s fall back to a permissive default
/// (the L4/emit layer refines with a real function/type registry). `expr`
/// must be one of the scalar variants (issue #205) — an operator variant here
/// is a construction bug, not a shape this needs to handle silently.
fn infer_expr_type(expr: &QueryExpr<ColumnId>, schema: &Schema) -> (DataType, bool) {
    match expr {
        QueryExpr::Column(id) => schema
            .columns
            .get(*id)
            .map(|c| (c.dtype.clone(), c.nullable))
            .unwrap_or((DataType::Float64, true)),
        QueryExpr::Literal(s) => match s {
            L3Scalar::Int64(_) => (DataType::Int64, false),
            L3Scalar::Float64(_) => (DataType::Float64, false),
            L3Scalar::Utf8(_) => (DataType::Utf8, false),
            L3Scalar::Boolean(_) => (DataType::Bool, false),
            L3Scalar::Null => (DataType::Float64, true),
        },
        // Boolean-valued expressions (SQL three-valued logic → nullable).
        QueryExpr::Compare { .. }
        | QueryExpr::BoolAnd(_)
        | QueryExpr::BoolOr(_)
        | QueryExpr::Not(_)
        | QueryExpr::IsNull(_)
        | QueryExpr::IsNotNull(_)
        | QueryExpr::InList { .. } => (DataType::Bool, true),
        QueryExpr::Arith { left, right, .. } => {
            let (lt, ln) = infer_expr_type(left, schema);
            let (rt, rn) = infer_expr_type(right, schema);
            let dtype = if matches!(lt, DataType::Int64) && matches!(rt, DataType::Int64) {
                DataType::Int64
            } else {
                DataType::Float64
            };
            (dtype, ln || rn)
        }
        QueryExpr::Cast { to, try_cast, expr } => {
            let (_, nullable) = infer_expr_type(expr, schema);
            (to.clone(), *try_cast || nullable)
        }
        // No function/type registry at L3 — default permissive.
        QueryExpr::FunctionCall { .. } => (DataType::Float64, true),
        QueryExpr::Case {
            branches,
            else_expr,
            ..
        } => branches
            .first()
            .map(|(_, then)| (infer_expr_type(then, schema).0, true))
            .or_else(|| else_expr.as_ref().map(|e| infer_expr_type(e, schema)))
            .unwrap_or((DataType::Float64, true)),
        other => {
            unreachable!("infer_expr_type called on a non-scalar QueryExpr variant: {other:?}")
        }
    }
}

/// Default output-column name for a projection item with no explicit alias:
/// a bare column keeps its (schema) name; anything else gets `col_{i}`.
fn default_proj_name(expr: &QueryExpr<ColumnId>, idx: usize, schema: &Schema) -> String {
    match expr {
        QueryExpr::Column(id) => schema
            .columns
            .get(*id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("col_{idx}")),
        _ => format!("col_{idx}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_asap::expr_ir::{ArithOp, CompareOp};

    fn col(name: &str, dtype: DataType, nullable: bool) -> Column {
        Column::new(name, dtype, nullable)
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
                closed: true,
            },
        }
    }

    /// A row can appear in more than one branch, so no branch's unique key is a
    /// key of the union. `Merge` took the first child's schema verbatim, which
    /// let a `Distinct`'s key leak out and claim a uniqueness the merged rows do
    /// not have — `unique_keys` feeds CSE's producer-sharing legality check.
    #[test]
    fn merge_drops_the_branches_unique_keys() {
        let branch = || QueryExpr::Distinct {
            cols: vec![0],
            child: Box::new(scan(
                vec![
                    col("k", DataType::Utf8, false),
                    col("v", DataType::Int64, false),
                ],
                None,
                vec![],
            )),
        };
        assert_eq!(
            branch().output_schema().unwrap().unique_keys,
            vec![vec![0]],
            "a Distinct branch does have a unique key on its own"
        );

        let merged = QueryExpr::Merge {
            children: vec![branch(), branch()],
        };
        let schema = merged.output_schema().unwrap();
        assert!(
            schema.unique_keys.is_empty(),
            "the union of two deduplicated branches is not deduplicated"
        );
        // The column shape is still the first branch's.
        assert_eq!(schema.columns.len(), 2);
    }

    /// Same rule as `SetOp`, which already dropped them.
    #[test]
    fn merge_and_setop_agree_on_unique_keys() {
        let branch = || QueryExpr::Distinct {
            cols: vec![0],
            child: Box::new(scan(vec![col("k", DataType::Utf8, false)], None, vec![])),
        };
        let merged = QueryExpr::Merge {
            children: vec![branch(), branch()],
        };
        let setop = QueryExpr::SetOp {
            kind: SetOpKind::Union,
            all: true,
            left: Box::new(branch()),
            right: Box::new(branch()),
        };
        assert_eq!(
            merged.output_schema().unwrap().unique_keys,
            setop.output_schema().unwrap().unique_keys,
        );
    }

    #[test]
    fn an_empty_merge_has_no_schema() {
        assert!(matches!(
            QueryExpr::Merge { children: vec![] }.output_schema(),
            Err(QueryExprError::EmptyMerge)
        ));
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
            qualifier: None,
            cols: vec![
                // bare column passthrough keeps its (schema) name + type: host=col 1
                ProjectItem {
                    alias: None,
                    expr: QueryExpr::Column(1),
                },
                // arithmetic over value (col 2) → Float64
                ProjectItem {
                    alias: Some("dbl".into()),
                    expr: QueryExpr::Arith {
                        op: ArithOp::Add,
                        left: Box::new(QueryExpr::Column(2)),
                        right: Box::new(QueryExpr::Column(2)),
                    },
                },
                // comparison → Bool (nullable under 3-valued logic)
                ProjectItem {
                    alias: Some("flag".into()),
                    expr: QueryExpr::Compare {
                        left: Box::new(QueryExpr::Column(2)),
                        op: CompareOp::Gt,
                        right: Box::new(QueryExpr::Literal(L3Scalar::Float64(0.0))),
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
    fn without_aggregate_keeps_open_schema_minus_excluded() {
        // `sum without (instance) (m)` over `[ts, value, instance, job]`: the
        // kept labels are the input labels minus the excluded `instance` (and ts
        // / value), followed by the `sum` column, and the schema stays OPEN
        // (issue #39). `job` survives; `instance` is dropped.
        let scan_node = QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp, false),
                    col("value", DataType::Float64, false),
                    col("instance", DataType::Utf8, true),
                    col("job", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        };
        let agg = QueryExpr::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::without(vec![2])), // exclude `instance`
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(scan_node),
        };
        let s = agg.output_schema().unwrap();
        let names: Vec<_> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["job", "sum"], "kept `job`, dropped `instance`");
        assert!(!s.closed, "a `without` result stays open");
        assert!(s.time_index.is_none());
        assert!(s.unique_keys.is_empty(), "kept set unknown → no unique key");
    }

    #[test]
    fn time_shift_is_schema_pass_through() {
        // `offset`/`@` move *when* a selector is evaluated, never its columns —
        // a `TimeShift` output schema equals its child's (issue #40).
        let scan_node = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("job", DataType::Utf8, true),
            ],
            Some(0),
            vec![],
        );
        let shifted = QueryExpr::TimeShift {
            shift: TimeShift {
                offset_ms: 3_600_000,
                at: Some(AtModifier::Timestamp(1_609_746_000_000)),
            },
            child: Box::new(scan_node.clone()),
        };
        assert_eq!(
            shifted.output_schema().unwrap(),
            scan_node.output_schema().unwrap(),
        );
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

    #[test]
    fn per_series_rate_preserves_labels() {
        // A per-series range reduction (`rate`) is label-preserving: it produces
        // one value per series, so every label survives and only the sample
        // value is replaced (kept named `value`). The TimeRange child is the
        // structural marker; the outer Aggregate carries the Rate intent.
        let scan_node = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("job", DataType::Utf8, true),
            ],
            Some(0),
            vec![],
        );
        let rate = QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: Box::new(QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Box::new(scan_node),
            }),
        };
        let s = rate.output_schema().unwrap();
        assert_eq!(
            s.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ts", "value", "job"],
            "rate preserves all labels; only the sample value is replaced"
        );
        assert_eq!(s.time_index, Some(0));
        assert!(s.column_id("job").is_some(), "label survives the reduction");
    }

    #[test]
    fn over_time_reduction_preserves_labels() {
        // `*_over_time` lowers to `Aggregate { by:[], [reducer], TimeRange { Scan } }`:
        // a per-series time-range reduction. The TimeRange child confers per-series
        // semantics on otherwise cross-series intents like `Avg`, so an outer
        // `sum by(job)(avg_over_time(...))` resolves its key positionally.
        let scan_node = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("job", DataType::Utf8, true),
            ],
            Some(0),
            vec![],
        );
        let avg_over_time = QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Box::new(scan_node),
            }),
        };
        let s = avg_over_time.output_schema().unwrap();
        assert_eq!(
            s.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ts", "value", "job"],
            "TimeRange-child marks per-series: labels preserved, value renamed"
        );
        assert!(
            s.column_id("job").is_some(),
            "outer Aggregate.by can resolve it"
        );
    }

    #[test]
    fn completeness_open_leaf_freezes_to_closed_at_cross_series_aggregate() {
        // A schemaless (PromQL-style) leaf is *open*; it stays open through a
        // per-series reduction (`rate`), then is **frozen to closed** by a
        // cross-series aggregate (which enumerates exactly its output columns).
        let open_leaf = QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            // `with_time_index` defaults to `closed: false` (open).
            schema: Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp, false),
                    col("value", DataType::Float64, false),
                    col("job", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        };
        assert!(
            !open_leaf.output_schema().unwrap().closed,
            "schemaless leaf is open"
        );

        let rate = QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: Box::new(open_leaf),
        };
        assert!(
            !rate.output_schema().unwrap().closed,
            "per-series rate is label-preserving → stays open"
        );

        let sum_by_job = QueryExpr::Aggregate {
            reduction: Reduction::by(vec![2]), // `job`
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(rate),
        };
        assert!(
            sum_by_job.output_schema().unwrap().closed,
            "cross-series aggregate enumerates `by ++ measures` → frozen to closed"
        );
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
            qualifier: None,
            cols: vec![
                // value=col 1, ts=col 0
                ProjectItem {
                    alias: None,
                    expr: QueryExpr::Column(1),
                },
                ProjectItem {
                    alias: None,
                    expr: QueryExpr::Column(0),
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
            pred: Predicate(Box::new(QueryExpr::Literal(L3Scalar::Boolean(true)))),
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
