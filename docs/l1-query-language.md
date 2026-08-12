# L1 — per-language native representation → canonical intent algebra

Per-language front ends, one per supported query language. Each turns a
raw query string all the way into the canonical intent tree that L2
(see [`l2-intent-algebra.md`](./l2-intent-algebra.md)) is the vocabulary
for — there is no separate numbered layer in between. That whole
journey is **two passes**, not one, and this doc is organized around
that split (see `design.md`'s representation/pass table for the
project-wide version of this distinction):

- **Pass 1 — parse + interpret**, entirely per-language. Produces the
  **per-language native representation** — a real intermediate (it
  crosses a real crate boundary: front-end crates hand it to
  `asap-l2`), but not a layer of its own, because nothing outside this
  pipeline ever depends on it as a stable interface. See "Pass 1" below.
- **Pass 2 — lower**, one shared implementation for every language.
  Produces the canonical intent tree — L2's vocabulary. See "Pass 2"
  below.

Front ends never depend on each other: each language's parsing path is
fully independent, so adding or removing a supported language never
touches any other front end.

A single query language may support more than one concrete surface
dialect at this layer — different syntax that still elaborates to the
same native representation. Dialect differences are purely a parsing
concern; they never affect what a later layer can plan.

```
Query string (language A)                Query string (language B)
   │  language A's own parser                │  language B's own parser
   ▼                                          ▼
language A's native representation         language B's native representation
   │  interpret into shared vocabulary        │  interpret into shared vocabulary
   ▼                                          ▼
        per-language native representation — same type for every language
        (spans two real types: the third-party parser's own AST, then
         asap-l2's shared relational tree; columns still named references)
   ═══════════════ pass 1 done; pass 2 (lower) starts ═══════════════
                                        │  one shared pass, for every language:
                                        │    1. bind — name → schema-position resolution
                                        │    2. convert — purely structural translation
                                        │    3. canonicalize — shared cross-language normalization
                                        ▼
                    canonical intent tree  ← L1's output, L2's vocabulary
                                              columns are positions
                                              + a self-contained schema on every scan
```

## Pass 1 — parse + interpret

Different front ends can look nothing alike where they start — one may
begin from a bare AST, another from an already-planned tree — but every
front end must converge on the *same* per-language-native-representation
shape, through the *same* internal entry point, before pass 2 (lower)
canonicalizes into the identical final tree. That convergence, not the
initial parsing itself, is the part that matters at the design level.

Every front end's native representation is interpreted into one shared
relational tree (or DAG) type — not one type per language. Still no
summary names, no positional column identity yet — those come from
pass 2's binding step. The tree covers the standard relational
vocabulary — source scan, filter, projection, aggregation (grouping +
aggregate functions), windowing, deduplication, heavy-hitter ranking,
set/join operators, ordering, and named/referenced sub-plans (for
expressing shared sub-computations) — plus a small number of extension
points for per-language surface features that don't yet have a general
relational equivalent. Column references are still symbolic names at
this point, resolved to positions by binding, in pass 2.

**Why this whole pass's output isn't its own layer.** It's a real
checkpoint — a genuine crate boundary — but it fails the test a layer
has to pass: something *outside* this pipeline treating it as a stable
interface. Nothing does. It only ever appears as a function argument on
the way to canonical, never persisted, tested independently, or
round-tripped. Compare L2, which many things depend on directly (this
pass's own output must conform to it; L3's binding pattern-matches it;
the DAG-export tooling walks it).

### The nesting contract

Nesting — an operator tree inside a source clause or a function
argument, an aggregate over an aggregate, a binary operation over two
subtrees, a range function over a sub-query, and so on — is required to
lower **structurally**: the canonical intent tree is a recursive,
arbitrarily-nestable structure, so "an operation over a sub-query"
needs no special-cased IR shape. Any language whose grammar allows
nesting must be able to express it this way, without a parallel
representation for the nested case.

Not every syntactically-expressible nesting shape has to be supported.
A front end is expected to cleanly reject a shape it can't yet
represent — rather than silently mis-lowering it — since a
resolvable-later gap is one design choice away from becoming a
correctness bug if it's lowered wrong instead of rejected outright.

## Pass 2 — lower

One shared implementation — used by every front end, regardless of
language — does three things in order: bind names to schema positions,
translate the tree structurally into the canonical form, then run a
shared cross-language normalization pass so that semantically
equivalent queries, from any supported language, arrive at the same
canonical shape (the vocabulary that shape belongs to is covered in
[`l2-intent-algebra.md`](./l2-intent-algebra.md)). This ordering
matters: normalization operates on already-resolved, already-translated
trees, so it never has to reason about per-language surface syntax.

This is genuinely more than renaming column references to positions:
`canonicalize`, the third step, rewrites real shape differences away —
e.g. promoting a generic `Limit{Sort{Aggregate([Count])}}` shape (what
SQL's `ORDER BY count DESC LIMIT k`, and PromQL's non-dedicated `topk`
path, both produce) to the explicit heavy-hitter `Aggregate{aggs:
[TopK]}` form (what PromQL's dedicated `topk(k, count_over_time(…))`
already produces directly) — so that both source shapes converge on one
canonical tree regardless of which path produced them.

### Name resolution: binding

Binding walks the per-language native representation and produces one
self-contained schema that every column reference in the resulting
canonical tree indexes into: it seeds columns from whatever catalog is
available (or a minimal always-present floor if the catalog knows
nothing), and accounts for any name the query references that the
catalog didn't already know about. A schema records whether it's a
*complete* enumeration of a row's columns or an *open* superset a
runtime row may exceed — different query languages sit at different
points on that spectrum (a language with no fixed schema, only
label-like references, versus one with a declared catalog) and binding
accommodates both without forcing them into the same shape.

Why isolate name resolution as its own step, rather than resolving
inline while translating:

- **Everything downstream becomes purely structural and total.** Once
  a schema exists, later steps work over positions, not names — a
  column reference can't fail to resolve once binding has already run,
  so every "column not found"-shaped failure is concentrated in one
  place.
- **Schema/catalog policy is swappable independently of translation.**
  Binding is generic over where schema information comes from; a
  language with no catalog and a language with a real one both flow
  through the same binding step, differing only in what the catalog
  supplies.
- **It's the natural home for resolution-policy questions.** A
  reference to "every column except these" can only be resolved once a
  schema is known — and for an open schema, the full complement can't
  be enumerated at all, so that has to be represented and deferred
  rather than resolved eagerly.

Each independent sub-tree (e.g. either side of a binary operation)
binds against its own schema, since the two sides may reference
entirely different sources — but a side must still see names
referenced by an *enclosing* operation (a grouping key mentioned above,
but not inside, either branch), so an enclosing scope's referenced
names are threaded down into each side's own binding pass.

### Column identity: two sources of truth

A source may arrive at the per-language native representation in one
of two shapes: schema-less (known only by whatever columns a query
happens to reference) or already schema-carrying (arriving with a
declared, complete column set from an external catalog). Binding
accommodates both:

- A **schema-less** source falls back to binding's own usage-derived
  schema — open, since a runtime row may carry more than what the
  query referenced.
- An **already schema-carrying** source keeps its own declared schema
  as-is — closed, since the catalog is a complete enumeration — and
  binding's own fallback computation for that source goes unused.

Either way, every column reference downstream resolves to a position
into *some* schema, uniformly; the difference is only where that
schema came from. This asymmetry also carries into whether a source
declares a **unique key** (a set of columns that together identify a
row): an already-cataloged source can carry a real one through
unchanged, while a usage-derived schema has no way to prove one, and so
never gets one. Why a unique key matters is covered in
[`l2-intent-algebra.md`](./l2-intent-algebra.md#unique-keys-and-cross-query-sharing).

## Interface

There is no shared trait for a front end's top-level entry point — each
exposes its own free functions, differing in language-specific
arguments (SQL takes a catalog + dialect; PromQL doesn't) but converging
on the same return type — the canonical `QueryExpr` — once both passes
finish:

```rust
// PromQL
pub fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Result<QueryExpr, PromqlError>;
pub fn lower_promql_batch(workload: &QueryWorkload) -> Vec<Result<QueryExpr, PromqlError>>;

// SQL
pub async fn lower_sql(query: &str, catalog: &SqlCatalog, accuracy: AccuracyTarget) -> Result<QueryExpr, SqlError>;
pub async fn lower_sql_dialect(query: &str, catalog: &SqlCatalog, dialect: SqlDialect, accuracy: AccuracyTarget) -> Result<QueryExpr, SqlError>;
pub async fn lower_sql_batch(workload: &QueryWorkload, catalog: &SqlCatalog) -> Vec<Result<QueryExpr, SqlError>>;
```

SQL's extra `catalog`/`dialect` parameters are exactly the schema-source
asymmetry covered above: SQL supplies its own resolved schema up front;
PromQL doesn't have one to supply, so it has no equivalent parameter.

Underneath both, the one real extension point is the schema source
pass 2's binding step resolves against:

```rust
pub trait SchemaCatalog {
    fn columns_for(&self, source: &str) -> Option<Vec<Column>>;
}

pub struct Binder<C: SchemaCatalog = UsageDerivedCatalog> { .. }
impl<C: SchemaCatalog> Binder<C> {
    pub fn bind(&self, tree: &QueryExpr) -> Schema;
}
```

The default `Binder` (`UsageDerivedCatalog`) implements this by always
returning `None` — a schema built purely from what the query happens to
reference. A catalog-backed language (or a future registry-backed one)
implements `columns_for` to return a real, closed column set instead;
nothing about `Binder` itself has to change for that swap.

For example, PromQL has no real catalog — a metric's label set is only
knowable from what the query itself references — so its front end binds
with the default:

```rust
// PromQL: `sum by (job) (http_requests_total)`, no catalog available.
let schema = Binder::default().bind(&tree);
// -> Schema { columns: [ts, value, job], time_index: Some(0), closed: false }
//    ("job" was seeded because the query references it; anything the
//    query never mentions is simply absent from this schema)
```

SQL has a real, declared catalog, so its front end supplies one instead:

```rust
struct SqlCatalog { /* wraps a table registry */ }
impl SchemaCatalog for SqlCatalog {
    fn columns_for(&self, source: &str) -> Option<Vec<Column>> {
        // "requests" -> its full declared column list, looked up from
        // whatever table registry this deployment has, e.g.:
        // Some(vec![Column::new("host", Utf8, false), Column::new("bytes", Int64, false)])
        ..
    }
}
let schema = Binder::with_catalog(SqlCatalog { .. }).bind(&tree);
// -> Schema { columns: [host, bytes], time_index: None, closed: true }
//    (the catalog's declared columns are used verbatim, regardless of
//    which ones the query actually references)
```

And pass 2 (lower) itself — wired inside `lower_promql`/`lower_sql`
above, so no front end can reach the canonical form through any other
path:

```rust
pub fn convert_root(native: &QueryExpr, accuracy: &AccuracyTarget) -> Result<QueryExpr, ConvertError>;
```

Takes the per-language native representation in, returns the canonical
tree out — binding, structural conversion, and canonicalization in one
call.
