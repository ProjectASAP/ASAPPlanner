# PromQL lowering — positional `ColumnId`, the Binder, and CSE

How `asap-frontend-promql` turns a PromQL string into the canonical Layer-3
intent algebra, and *why* the IR uses positional column identity, an explicit
name-resolution pass, and unique-key metadata.

This is the PromQL companion to [`design.md` §6](design.md) ("Core crate
details — Layer 3"). It mirrors the `asapquery-backend` control-plane IR:
**two IRs joined by a Binder.**

## The pipeline at a glance

```
PromQL string
   │  parse                             promql-parser (ProjectASAP fork, branch asap)
   ▼
Expr AST                  ← L1
   │  front-end lowering                crates/frontend-promql/src/promql.rs
   ▼
relational::QueryExpr     ← L2 — columns are NAMES (ColumnRef::Named, Aggregate.keys: Vec<String>)
   │  Binder pass: build Schema +       crates/l2/src/binder.rs
   │  resolve names → ColumnId           + crates/l2/src/column_resolution.rs
   ▼
query_expr::QueryExpr     ← L3 — columns are POSITIONS (Aggregate.by: Vec<ColumnId>)
                            + a self-contained Schema rides on each Scan
```

The three layers are **L1 (`Expr AST`) → L2 (`relational::QueryExpr`, names) →
L3 (`query_expr::QueryExpr`, positions)**. The **Binder is not a layer** — it is
the pass that sits on the L2→L3 edge, turning names into positional `ColumnId`s.
`convert_root` (`crates/l2/src/lower.rs`) runs it first, converts the L2 tree
structurally, then runs the shared `canonicalize` pass (`crates/l2/src/canonicalize.rs`)
so both front ends emit one canonical form:

```rust
pub fn convert_root(legacy: &LQueryExpr, accuracy: &AccuracyTarget)
    -> Result<CQueryExpr, ConvertError>
{
    let fallback = Binder::new().bind(legacy);  // ← L2→L3 name resolution, once
    let l3 = convert(legacy, &fallback, accuracy)?; // ← purely structural
    Ok(canonicalize(l3))                        // ← shared normalization (#34)
}
```

The worked example at the end traces a query through all three layers (with the
Binder pass shown explicitly between L2 and L3).

---

## Why positional `ColumnId`

`ColumnId = usize` (`schema.rs`), an index into `Schema::columns`. Everywhere
the *canonical* IR names a column — `Aggregate.by`, `Schema::unique_keys`,
`Schema::time_index` — it is a position, not a string.

The IR is deliberately split in two:

| | Layer-2 `relational::QueryExpr` | Canonical `query_expr::QueryExpr` |
|---|---|---|
| Column identity | `ColumnRef::Named(String)`, `Aggregate.keys: Vec<String>` | `ColumnId = usize`, `Aggregate.by: Vec<ColumnId>` |
| Source of names | whatever the PromQL parser emits | resolved against a `Schema` |

Why convert names → positions at all:

1. **Identity is settled once.** A string `"service"` means nothing until you
   know which schema it lives in and at what offset. If every downstream pass
   (schema flow, push-down, CSE, cost model, L5 emitters) carried names, each
   would re-resolve and each would own a "column not found" failure path. A
   `ColumnId` is an array index that **cannot dangle** — resolution already
   happened.
2. **The canonical tree is self-describing.** The `Scan` node carries the
   `Schema`, so any sub-tree's output schema is computable without surrounding
   context (`QueryExpr::output_schema_in`). Positions index straight into it.
3. **It matches the backend wire format.** `ColumnId` is aliased to `usize`
   specifically to line up with `design.md`'s `unique_keys: Vec<Vec<usize>>`.
   The point of the restructure was convergence with the backend IR, not a
   parallel L3.

The named alias is kept (rather than a bare `usize`) so code can still pattern
on intent — *"this is a column position, not just any number."*

---

## Why the Binder is its own pass

`Binder::bind` (`binder.rs`) walks the L2 tree and returns **one**
self-contained `Schema { columns, time_index, unique_keys }` that every
`ColumnId` in the converted tree indexes into. It:

1. Seeds columns from the catalog (`SchemaCatalog::columns_for`), or the
   `(ts, value)` floor if the catalog knows nothing.
2. Guarantees that `(ts, value)` floor is present.
3. Appends one `Utf8` column per referenced-but-unknown name — collected from
   `Aggregate.keys`, `TopK.by`, `Partition.keys` (`collect_referenced_columns`).

Why isolate this instead of resolving inline during lowering:

- **The converter becomes purely structural and total.** Once the schema
  exists, positional resolution downstream can't fail to *find* a column. Every
  failure mode (`ResolveError::NotFound`, `NoSampleValue`, `WildcardNotPositional`)
  is concentrated in this one pass.
- **Schema/catalog policy is swappable without touching lowering.** The default
  `UsageDerivedCatalog` knows nothing — honest for observability, where metric
  label sets are open-ended. A registry-backed `SchemaCatalog` is future work,
  and the Binder pass does not change when it lands — only the catalog impl
  swaps.
- **It is the natural home for resolution-policy errors.** `without(...)` is
  rejected here: a usage-derived schema can't enumerate "all labels *except*
  these," so the error belongs in binding, not smeared across lowering.

---

## Why `unique_keys` / CSE

`unique_keys: Vec<Vec<ColumnId>>` (`schema.rs`). Each inner vec is a set of
column positions that *together* uniquely identify a row; the outer vec allows
several such sets. It is populated by the per-node output-schema rule —
`Aggregate { by, .. }` emits `unique_keys = [by-positions]` when `by` is
non-empty (`query_expr.rs`), most other nodes pass through.

**What it is for: workload-level CSE.** When several queries are planned
together, `cse::dedupe_subtrees` hoists a shared sub-DAG into a `LetBinding` so
the cost model credits the producer once, with each root referencing it via
`Ref`. But sharing is only sound if the producer emits *the same rows* for every
consumer — and a unique key is exactly what proves that.

`cse_reuse_is_legal` is the gatekeeper (`schema.rs`):

```rust
pub fn cse_reuse_is_legal(producer_schema: &Schema, consumer_count: usize)
    -> Result<(), CseError>
{
    if consumer_count < 2          { return Err(CseError::InsufficientConsumers(consumer_count)); }
    if !producer_schema.has_unique_key() { return Err(CseError::NoUniqueKeys); }
    Ok(())
}
```

Why `unique_keys` rather than just deduping structurally-identical subtrees:
structural identity (`format!("{child:?}")`) tells you two consumers *want* the
same producer — it does **not** tell you the producer's output is *stable across
reads*. Without a provable unique key, two `Ref`s could observe different row
sets, and crediting the sharing would be unsound. Structural identity is the
candidate-finder; `unique_keys` is the correctness predicate. Expressing keys as
`ColumnId` sets is what lets the deduper assert this cheaply — another reason
positions exist.

**Status:** this PR lands the scaffolding (`dedupe_subtrees`,
`cse_reuse_is_legal`, `Schema::unique_keys`, `LetBinding`/`Ref`). The cost-model
integration that makes it influence planning is tracked in **#6**. Single-query
plans never read `unique_keys`.

---

## Worked example — one query through L1 → L2 → L3

Four steps: the three layers, plus the Binder pass shown explicitly on the
L2→L3 edge.

```promql
topk by (service) (10, count_over_time(requests{env="prod"}[1m]))
```

This is the heavy-hitter case (`topk` over `count` → one-pass sketch), exercised
by `topk_over_count_is_heavy_hitter_topk` in `crates/frontend-promql/tests/promql_lowering.rs`.

### Stage 1 — L1 parse (`promql-parser`)

```
Expr::Aggregate {
    op:       topk,
    param:    NumberLiteral(10),
    modifier: by (service),
    expr: Expr::Call {
        func: count_over_time,
        args: [ Expr::MatrixSelector { vs: requests{env="prod"}, range: 1m } ],
    },
}
```

### Stage 2 — L2 relational IR (names) · `promql.rs`

`walk_aggregate` resolves the group modifier to `keys = ["service"]`, lowers the
inner `count_over_time(...[1m])` to `Inner { metric: "requests",
matchers: [env=="prod"], window: 1m, func: Count }`, and — because the op is
`topk` *and* the inner func is `Count` — picks the heavy-hitter branch
(`Outer::TopK { k: 10, descending: true }` → `heavy_hitter == true`):

```
TopK { k: 10, by: ["service"],                 ← columns are still NAMES
  input: Window { duration: 1m, slide: None,
    input: Filter { pred: Compare { left: Column("env"), op: Eq, right: "prod" },
      input: Source(SourceSpec { name: "requests" }) } } }
```

No `Aggregate` wraps the scan — the heavy-hitter sketch counts directly off the
windowed scan (`window_scan`). Grouping rides as a *name list* on `TopK.by`,
awaiting resolution.

### Stage 3 — Binder pass (L2→L3 edge): build the Schema, resolve names → `ColumnId`

`Binder::bind` walks the L2 tree:

- `source_name() == "requests"`; `UsageDerivedCatalog` returns `None` → start
  from the `(ts, value)` floor.
- `collect_referenced_columns` finds `TopK.by = ["service"]` → append `service`
  as a `Utf8` column.

Result — the single self-contained schema:

```
Schema {
    columns:     [ ts:Timestamp(0), value:Float64(1), service:Utf8(2) ],
    time_index:  Some(0),
    unique_keys: [],          ← UsageDerivedCatalog proves no unique key
}
```

`resolve_named_keys(["service"], schema)` → `"service"` is at position 2 →
`by = [2]`.

### Stage 4 — L3 canonical IR (positions) · `lower.rs convert`

The `TopK` arm rewrites to the canonical `Aggregate{TopK}`, threading the
accuracy target and the resolved positional `by`; the `Filter`-over-`Source`
folds into `Scan.predicates`; the bound schema rides on the `Scan`:

```
Aggregate {
    by:   [2],                                  ← service, POSITIONAL now
    aggs: [ TopK { k: 10, accuracy: <target> } ],
    having: None,
    child: Window { kind: Tumbling, size: 1m, slide: None,
      child: Scan {
        source:     TimeSeries { metric: "requests" },
        predicates: [ Compare { left: Column("env"), op: Eq, right: "prod" } ],
        schema:     Schema { [ts, value, service], time_index: Some(0), unique_keys: [] },
      } } }
```

### Schema flow & the CSE gate on this tree

`output_schema_in` for the top `Aggregate`: `by = [2]` is non-empty, so its
output schema is

```
Schema {
    columns:     [ service:Utf8, topk_10:Utf8 ],   ← group key + TopK output
    time_index:  None,
    unique_keys: [[0]],                            ← the group key is now a unique key
}
```

Now suppose a second query shared the same `Window → Scan` producer. The deduper
would propose hoisting it and call
`cse_reuse_is_legal(window.output_schema(), 2)`. The window passes the Scan's
schema through unchanged — and that schema's `unique_keys` is **empty** (the
`UsageDerivedCatalog` couldn't prove one). So the gate returns
`Err(NoUniqueKeys)` and the producer is **not** shared — each consumer
recomputes it.

That refusal is the design working as intended: under the default catalog we
cannot assert that a raw windowed scan yields identical rows across reads, so we
decline to share rather than risk an unsound plan. A registry-backed
`SchemaCatalog` that declared, say, `(ts, service)` unique on `requests` would
populate `Scan.schema.unique_keys`, flip the gate green, and let the windowed
scan be hoisted into a `LetBinding` — without any change to the Binder or
converter (cf. the `dedupe_subtrees_basic` test, which constructs exactly such a
Scan). The next section traces exactly that.

---

## `unique_keys` propagation

`unique_keys` is **not** something the Binder computes — `Binder::bind` always
emits `unique_keys: Vec::new()`. It enters at the leaf (from the catalog) and is
then derived edge-by-edge by each operator's output-schema rule
(`QueryExpr::output_schema_in`). The rules:

| Operator | `unique_keys` of its output |
|---|---|
| `Scan` | **verbatim** from the schema the Binder/catalog built |
| `Window`, `Filter`, `Partition`, `Sort`, `Limit`, `Subquery`, `Project` | **pass through** the child's unchanged |
| `Aggregate { by }` | **replaced** with `[[0..by.len()]]` — the group keys, *re-based to output positions*; empty when `by` is empty |
| `Distinct { cols }` | child's keys **plus** `cols` added as a new key (`add_unique_key`) |
| `Merge` | first child's |
| `SetOp`, `Join`, `BinaryOp` | left / `lhs` child's |

Two rules carry the weight: leaf-bearing operators **pass keys through** untouched,
while `Aggregate` **manufactures** a key — grouping by a column makes that column
unique in the result, so it becomes the new key (and the input's keys are
dropped, because the grouped output no longer has those rows).

### Same tree, under a registry catalog

Take the worked example's canonical tree, but bind it with a `SchemaCatalog` that
declares `(ts, service)` unique on `requests`. Now the leaf schema arrives with a
key, and we can watch it flow up (bottom → top):

```
                                                      ── unique_keys on this edge ──
Scan { requests,                                       [[0, 2]]      ← from catalog
       schema: [ts(0), value(1), service(2)],                          (ts, service)
               unique_keys = [[0, 2]] }
  ▲
Window { 1m }                                          [[0, 2]]      ← pass-through
  ▲                                                                    (time_index present)
Aggregate { by:[2]=service, aggs:[TopK{10}] }          [[0]]         ← REPLACED
       output cols: [service(0), topk_10(1)]                           by re-based to
                                                                       output position 0
```

Three things to read off this:

1. **Scan** hands up the catalog's `[[0, 2]]` verbatim.
2. **Window** (and any `Filter`/`Sort`/`Limit` between) passes `[[0, 2]]` straight
   through — these operators don't change which rows are distinct.
3. **Aggregate** does *not* forward `[[0, 2]]`. After `GROUP BY service`, the old
   per-sample identity is gone; what's unique now is `service` itself — and in the
   output schema `service` sits at **position 0**, so the derived key is `[[0]]`,
   not `[[2]]`. This re-basing is why keys are positional `ColumnId`s, not names:
   the same column is id `2` below the aggregate and id `0` above it.

### `ColumnId` is relative to a schema — the `2` vs `0`

A `ColumnId` is a position *within one schema*. The input and output edges of
`Aggregate` are **different schemas**, so the *same* logical `service` column
gets a different id on each. Watch it with sample rows.

**Input edge** (below the aggregate) — `service` is column **2**:

| `ts` · id 0 | `value` · id 1 | `service` · id 2 |
|---|---|---|
| 100 | 0.5 | api |
| 100 | 0.3 | web |
| 200 | 0.7 | api |
| 200 | 0.4 | web |

→ "group by `service`" is written `by = [2]`. And every `(ts, service)` combo
occurs once, so the edge carries `unique_keys = [[0, 2]]`.

**Output edge** (above the aggregate) — a *brand-new* table; `service` is now
column **0**:

| `service` · id 0 | `topk_10` · id 1 |
|---|---|
| api | … |
| web | … |

→ after grouping, each `service` appears exactly once, so `service` *alone* is
the key — at its **new** position: `unique_keys = [[0]]`.

So `2` and `0` both name `service`; they differ only because input and output are
different schemas. This is also the cleanest way to see how the two concepts
divide up:

- **`ColumnId`** answers *"which column"* — a pointer (`by = [2]`: group by the
  column at position 2). Used everywhere a column must be named.
- **`unique_keys`** answers *"which set(s) of columns are jointly non-duplicating"*
  — a fact about the data, *written using* `ColumnId`s (`[[0, 2]]`: columns 0 and
  2 together identify a row). Read only by the CSE gate.

The outer `Vec` allows several such sets: `[[0, 2]]` = one key (the pair);
`[[0], [1, 2]]` = two independent keys.

### Why this makes CSE legal here

The shared producer the deduper would hoist is the `Window → Scan` sub-tree. Its
output edge now carries `unique_keys = [[0, 2]]`, so:

```
cse_reuse_is_legal( window.output_schema()  // unique_keys = [[0, 2]]
                  , 2 /* consumers */ )      ==> Ok(())
```

The gate fires green, the `Window → Scan` is hoisted into a `LetBinding`, and both
queries `Ref` it — scan + window computed once. Under the default
`UsageDerivedCatalog` the very same tree carries `unique_keys = []` at every edge
(nothing manufactures a key below the top `Aggregate`), so the gate returns
`Err(NoUniqueKeys)` and each consumer recomputes. **The only thing that changed
was the leaf key the catalog supplied; propagation and the gate did the rest.**
