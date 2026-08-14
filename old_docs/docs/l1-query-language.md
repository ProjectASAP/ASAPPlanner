# L1 — query string → canonical intent algebra

Per-language front ends, one per supported query language. Each turns a
raw query string all the way into the canonical intent tree that L2
(see [`l2-intent-algebra.md`](./l2-intent-algebra.md)) is the vocabulary
for. That journey is **two passes**, and this doc is organized around
that split (see `design.md`'s representation/pass table for the
project-wide version of this distinction):

- **Pass 1 — interpret**, entirely per-language. Each front end
  translates its own third-party parser's AST directly into the
  canonical shape, with column references left named (unresolved). See
  "Pass 1" below.
- **Pass 2 — resolve, then canonicalize**, one shared implementation
  for every language. Binds names to schema positions, substitutes them
  throughout, then normalizes cross-language/cross-phrasing shape
  differences into L2's one canonical form. See "Pass 2" below.

**Status.** This describes the design target; implementation tracked in
[issue #179](https://github.com/ProjectASAP/ASAPController/issues/179).

Front ends never depend on each other: each language's parsing path is
fully independent, so adding or removing a supported language leaves
every other front end untouched.

A single query language may support more than one concrete surface
dialect at this layer — different syntax that still elaborates to the
same canonical shape. Dialect differences are purely a parsing concern,
resolved entirely within pass 1.

```
Query string (language A)                Query string (language B)
   │  language A's own parser                │  language B's own parser
   ▼                                          ▼
language A's native AST                    language B's native AST
   │  interpret directly into                 │  interpret directly into
   │  the canonical shape                     │  the canonical shape
   ▼                                          ▼
        canonical-shaped tree, named column references
   ═══════════════ pass 1 done; pass 2 starts ═══════════════
                                        │  one shared pass, for every language:
                                        │    1. resolve — bind names to schema
                                        │       positions, substitute throughout
                                        │    2. canonicalize — cross-language /
                                        │       cross-phrasing normalization
                                        ▼
                    canonical intent tree  ← L1's output, L2's vocabulary
                                              columns are positions
                                              + a self-contained schema on every scan
```

## Pass 1 — interpret

Different front ends can look nothing alike where they start — one may
begin from a bare AST, another from an already-planned tree — but every
front end produces the same canonical shape as its output, through the
same node vocabulary, regardless of source language.

Every language-construct-specific structural decision belongs here, in
the front end that has full context on what it's looking at, at parse
time: a dedicated `topk()` call becomes `Aggregate{aggs:
[AggIntent::TopK]}` directly, a range selector `m[5m]` becomes
`TimeRange` directly, `WHERE`/label matchers become `Scan.predicates`
directly. Column references stay named at this point — resolving them
to positions is pass 2's job, once a schema exists to resolve against.

### The nesting contract

Nesting — an operator tree inside a source clause or a function
argument, an aggregate over an aggregate, a binary operation over two
subtrees, a range function over a sub-query, and so on — is required to
lower **structurally**: the canonical intent tree is a recursive,
arbitrarily-nestable structure, so "an operation over a sub-query"
needs no special-cased IR shape. Any language whose grammar allows
nesting must be able to express it this way, using the same recursive
structure for the nested case as for the top level.

Not every syntactically-expressible nesting shape has to be supported.
A front end is expected to cleanly reject a shape it can't yet
represent — rather than silently mis-lowering it — since a
resolvable-later gap is one design choice away from becoming a
correctness bug if it's lowered wrong instead of rejected outright.

## Pass 2 — resolve, then canonicalize

One shared implementation — used by every front end, regardless of
language — does two things in order:

1. **Resolve.** Bind names to schema positions (see "Name resolution:
   binding" below), then substitute every column reference throughout
   the tree — a generic walk over the already-canonical-shaped tree
   pass 1 produced.
2. **Canonicalize.** Run a cross-language normalization pass so that
   semantically equivalent queries, from any supported language or
   differently phrased within the same language, converge on the
   identical canonical shape. This step matters precisely because pass
   1 is per-language: a language with no dedicated syntax for an intent
   still needs its generic phrasing recognized as that intent — e.g.
   SQL's `ORDER BY count DESC LIMIT k` has no `topk()`-shaped AST node
   for the SQL front end to translate from directly; `canonicalize`
   recognizes it as the same `Aggregate{aggs:[TopK]}` shape PromQL's
   dedicated `topk(k, count_over_time(…))` produces directly in pass 1.

This ordering matters: canonicalization operates on an already-resolved
tree, so its pattern-matching rules work over stable schema positions,
not per-language surface syntax.

### Name resolution: binding

Binding walks the canonical-shaped, named-reference tree pass 1
produced and derives one self-contained schema that every column
reference indexes into: it seeds columns from whatever catalog is
available (or a minimal always-present floor if the catalog knows
nothing), and accounts for any name the query references that the
catalog didn't already know about.

Why isolate name resolution as its own step, rather than resolving
inline while interpreting, in pass 1:

- **Everything downstream becomes purely structural and total.** Take
  those two words literally: *structural* means later steps match on
  tree shape and column position, never on a name string again — the
  string comparisons all happened once, here. *Total* is the
  computer-science sense — a function defined for every input, with no
  case left unhandled — applied to column resolution: once binding has
  run, a `ColumnId` is guaranteed to resolve, so "column not found" is
  a binding-time error, never a surprise several passes downstream.
- **Schema/catalog policy is swappable independently of interpretation.**
  Binding is generic over where schema information comes from; a
  language with no catalog and a language with a real one both flow
  through the same binding step, differing only in what the catalog
  supplies.
- **It's the natural home for resolution-policy questions.** A
  reference to "every column except these" can only be resolved once a
  schema is known — and for an open schema (see below), the full
  complement is represented and deferred to serving time, rather than
  resolved eagerly.

Each independent sub-tree (e.g. either side of a binary operation)
binds against its own schema, since the two sides may reference
entirely different sources — but a side must still see names
referenced by an *enclosing* operation (a grouping key mentioned above,
but not inside, either branch), so an enclosing scope's referenced
names are threaded down into each side's own binding pass.

### Open vs. closed schemas

Take `sum by (job) (http_requests_total)`. The query tells us
`http_requests_total` carries a `job` label — but a real series for
that metric might also carry `instance`, `region`, or any other label
a target happened to attach, none of which this query mentions.
Nothing anywhere enumerates PromQL's full label set for a metric ahead
of time; it's **open** — a superset the runtime data may exceed. Now
compare `SELECT host, bytes FROM requests` — SQL's `requests` table has
a declared, complete column list in the catalog: `host` and `bytes`
are the whole row, guaranteed, nothing more can show up at runtime.
That's **closed**.

Binding resolves this per source, not per language, since which one
applies depends on whether a catalog exists for that particular scan:

- A **schema-less** source (PromQL's `http_requests_total`, no
  catalog available) falls back to binding's own usage-derived schema —
  open, seeded from exactly the labels the query references.
- An **already schema-carrying** source (SQL's `requests`, resolved
  against `SqlCatalog`) keeps its own declared schema as-is — closed —
  and binding's usage-derived fallback goes unused for that source.

Either way, every column reference downstream resolves to a position
into *some* schema, uniformly; the difference is only where that
schema came from. This asymmetry also carries into whether a source
declares a **unique key** (a set of columns that together identify a
row): an already-cataloged source can carry a real one through
unchanged, while a usage-derived schema proves one only when the query
itself establishes it. Why a unique key matters is covered in
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
PromQL's schema is entirely usage-derived, so it has no equivalent
parameter.

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
`Binder` itself stays the same either way.

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

`canonicalize` — pass 2's second step, unaffected by the pass 1/interpret
migration tracked in #179, since it already operates on the canonical
type either way:

```rust
pub fn canonicalize(tree: QueryExpr) -> QueryExpr;
```

Idempotent, bottom-up: rewrites a tree already in canonical shape into
its normal form (e.g. promoting a generic `Limit{Sort{Aggregate([Count])}}`
shape to the explicit heavy-hitter `Aggregate{aggs:[TopK]}` form) —
same type in, same type out.
