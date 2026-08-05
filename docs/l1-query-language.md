# L1 — query language

Per-language front ends, one per supported query language. Each turns a
raw query string into that language's own native shape — whatever
representation is most natural for that language (e.g. a parsed AST,
or an already-planned relational tree) — with no summary awareness and
no shared vocabulary yet; that only starts once every front end reaches
L2.

Front ends never depend on each other: each language's parsing path is
fully independent, so adding or removing a supported language never
touches any other front end.

A single query language may support more than one concrete surface
dialect at this layer — different syntax that still elaborates to the
same native representation. Dialect differences are purely a parsing
concern; they never affect what a later layer can plan.

## Parse → lower: converging on one shape

Different front ends can look nothing alike at L1 — one may start from
a bare AST, another from an already-planned tree — but every front end
must converge on the *same* L2 shape, through the *same* L2 entry
point. That convergence, not the L1 parsing itself, is the part that
matters at the design level:

```
Query string (language A)                Query string (language B)
   │  language A's own parser                │  language B's own parser
   ▼                                          ▼
language A's native representation ← L1    language B's native representation ← L1
   │  interpret into shared vocabulary        │  interpret into shared vocabulary
   ▼                                          ▼
                    shared logical plan  ← L2 — same type for every language
                    columns are still named references
                                        │
                                        │  one shared L2 → L3 step, for every language:
                                        │    1. bind — name → schema-position resolution
                                        │    2. convert — purely structural L2 → L3 translation
                                        │    3. canonicalize — shared cross-language normalization
                                        ▼
                    canonical intent tree  ← L3 — columns are positions
                                              + a self-contained schema on every scan
```

Binding is not itself a layer — it's the pass that sits on the L2 → L3
edge. *Why* the canonical form uses positional column identity, how
binding differs between a schema-less source and a catalog-backed one,
and what a unique key is for are all L2/L3 design questions, covered in
[`l2-logical-plan.md`](./l2-logical-plan.md) and
[`l3-intent-algebra.md`](./l3-intent-algebra.md) — this section only
needs to establish that every front end feeds the identical mechanism.

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

## Interface

There is no shared L1 trait — each front end exposes its own free
functions, differing in language-specific arguments (SQL takes a
catalog + dialect; PromQL doesn't) but converging on the same return
type once lowering finishes:

```rust
// PromQL
pub fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Result<QueryExpr, PromqlError>;
pub fn lower_promql_batch(workload: &QueryWorkload) -> Vec<Result<QueryExpr, PromqlError>>;

// SQL
pub async fn lower_sql(query: &str, catalog: &SqlCatalog, accuracy: AccuracyTarget) -> Result<QueryExpr, SqlError>;
pub async fn lower_sql_dialect(query: &str, catalog: &SqlCatalog, dialect: SqlDialect, accuracy: AccuracyTarget) -> Result<QueryExpr, SqlError>;
pub async fn lower_sql_batch(workload: &QueryWorkload, catalog: &SqlCatalog) -> Vec<Result<QueryExpr, SqlError>>;
```

Both converge on the canonical L3 `QueryExpr` — the interface that
actually matters lives one step down, at the shared L2 → L3 step this
doc already describes (see [`l2-logical-plan.md`](./l2-logical-plan.md#interface)).
SQL's extra `catalog`/`dialect` parameters are exactly the schema-source
asymmetry covered above: SQL supplies its own resolved schema up front;
PromQL doesn't have one to supply, so it has no equivalent parameter.
