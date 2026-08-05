# L2 — logical plan

Every front end's output converges on one shared relational tree (or
DAG) type at this layer — not one type per language. Still no summary
names, no positional column identity — those are L2 → L3 concerns
(below). The tree covers the standard relational vocabulary — source
scan, filter, projection, aggregation (grouping + aggregate functions),
windowing, deduplication, heavy-hitter ranking, set/join operators,
ordering, and named/referenced sub-plans (for expressing shared
sub-computations) — plus a small number of extension points for
per-language surface features that don't yet have a general relational
equivalent. Column references are still symbolic names at this layer,
resolved to positions by binding, below.

## Name resolution: binding

Binding walks the L2 tree and produces one self-contained schema that
every column reference in the resulting L3 tree indexes into: it seeds
columns from whatever catalog is available (or a minimal always-present
floor if the catalog knows nothing), and accounts for any name the
query references that the catalog didn't already know about. A schema
records whether it's a *complete* enumeration of a row's columns or an
*open* superset a runtime row may exceed — different query languages
sit at different points on that spectrum (a language with no fixed
schema, only label-like references, versus one with a declared
catalog) and binding accommodates both without forcing them into the
same shape.

Why isolate name resolution as its own pass, rather than resolving
inline while translating:

- **Everything downstream becomes purely structural and total.** Once
  a schema exists, later passes work over positions, not names — a
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

## The L2 → L3 step

One shared step — used by every front end, regardless of language —
does three things in order: bind names to schema positions (above),
translate the tree structurally into L3's own shape, then run a shared
cross-language normalization pass so that semantically equivalent
queries, from any supported language, arrive at the same canonical
shape (covered in [`l3-intent-algebra.md`](./l3-intent-algebra.md)).
This ordering matters: normalization operates on already-resolved,
already-translated trees, so it never has to reason about per-language
surface syntax.

## Column identity: two sources of truth

A source may arrive at L2 in one of two shapes: schema-less (known only
by whatever columns a query happens to reference) or already
schema-carrying (arriving with a declared, complete column set from an
external catalog). Binding accommodates both:

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
[`l3-intent-algebra.md`](./l3-intent-algebra.md#unique-keys-and-cross-query-sharing).

## Interface

The one real extension point at this layer is the schema source a
front end's leaves resolve against:

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

The L2 → L3 step every front end shares:

```rust
pub fn convert_root(legacy: &QueryExpr, accuracy: &AccuracyTarget) -> Result<QueryExpr, ConvertError>;
```

Takes L2's per-language tree in, returns L3's canonical tree out —
binding, structural conversion, and canonicalization in one call, so no
front end can reach L3 through any other path.
