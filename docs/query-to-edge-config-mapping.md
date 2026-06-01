# Controller Mapping: Query → Edge Aggregation Config

> **Design doc.** How the controller lowers a query into the per-aggregation
> decisions the edge needs: **sketch family**, **aggregation scope**
> (per-series vs. whole-stream), **sparse vs. dense HLL**, and the rest of the
> `PrecomputeConfig` wire fields the edge consumes.
>
> Status: design. The L3 intent algebra and the L4 sketch-bound IR already
> exist; the **L3→L4 binding rules** and the **L4→edge emitter** that produce
> the edge config do **not** yet exist — this doc specifies them.

---

## TL;DR

- The edge (`asap-precompute-go`) consumes a `PrecomputeConfigSet{Version,
  []PrecomputeConfig}`. Two fields were recently added that the controller now
  has to drive: **`Scope`** (per-series vs. whole-stream, ASAPCollector #471)
  and the HLL **`hll_sparse`** knob (#472). Both currently default to the
  legacy behavior because nothing sets them — this doc closes that gap.
- The signals already exist in the query IR:

  | Edge decision | Driven by (query signal) | Where it lives today |
  | --- | --- | --- |
  | **Sketch family** + params | `AggIntent` kind + `AccuracyTarget` | `agg_intent.rs:35`, `types.rs:5` |
  | **Scope** (per-series / whole-stream) | `Aggregate.by: GroupKeys` (+ `TimeRange` child marker) | `query_expr.rs:47,278,369` |
  | **Sparse HLL** | `Cardinality` intent + expected cardinality + ε | `agg_intent.rs` (+ workload/catalog) |

- The decisions belong in two not-yet-built layers: an **L4 binding rule**
  (`AggIntent` → `SummaryKind`/`SummaryParams`) and an **L5 emitter**
  (`SummaryAgg` → `PrecomputeConfig`). This doc specifies both as pure mapping
  functions so they can be implemented and unit-tested in isolation.

---

## 1. Background: the two ends

### 1.1 What the query carries (L3 intent algebra — exists)

- **Aggregation intent** — `AggIntent` (`crates/core/src/intent_algebra/agg_intent.rs:35`):
  `Count{accuracy}`, `Sum`, `Min`, `Max`, `Avg`, `StdDev`, `Variance`,
  `Quantile{q,accuracy}`, `TopK{k,accuracy}`, `Cardinality{accuracy}`, `Rate`,
  `Increase`.
- **Grouping** — `Aggregate{ by: GroupKeys, aggs, ... }`
  (`query_expr.rs:278`); `GroupKeys(Vec<ColumnId>)` (`query_expr.rs:47`),
  `by.is_empty()` ⇒ no GROUP BY (global).
- **Per-series marker** — an `Aggregate` whose direct child is a `TimeRange`
  is a *per-series* (label-preserving) range reduction
  (`query_expr.rs:369,429`); `AggIntent::is_per_series()` is true for
  `Rate`/`Increase` (`agg_intent.rs:101`).
- **Accuracy** — `AccuracyTarget` (`types.rs:5`): `Exact`, `Epsilon(ε)`,
  `EpsilonDelta{ε,δ}`.
- **Not carried by the query:** *expected cardinality*. It must come from
  workload/catalog context (`crates/core/src/workload.rs`), not the query AST.

### 1.2 What the edge needs (`PrecomputeConfig` — exists, Go)

Fields the controller must populate (from `asap-precompute-go/config.go`,
post #471/#472):

| Field | Type | Meaning |
| --- | --- | --- |
| `AggID` | `uint64` | Controller-plan join key; one config per aggregation. |
| `SketchType` | enum | DDSketch / KLL / HLL / CountSketch / CountMinSketch / (Sum). |
| `Scope` | `AggMode` | `ModePerSeries` (0, default) / `ModeWholeStream`. |
| `AggregateBy` | `[]string` | Label keys to group by (empty + per-series ⇒ one sketch per raw series). |
| `SketchParams` | `map[string]float64` | `k`, `precision`, `width`/`depth`, `alpha`, **`sparse`**. |
| `Window`, `Mode` | windowing | size/slide/lateness; Tumbling/Sliding/Batch. |
| `MaxSeries`, `OnOverflow` | caps | per-shard cardinality cap (no-op under whole-stream). |
| `DeltaTransmission`, `Encoding`, `Temporality`, `MetricName` | wire | as today. |

> Note the naming: edge `PrecomputeConfig.Mode` is the **windowing** strategy
> (Tumbling/Sliding/Batch); the per-series/whole-stream axis is the separate
> **`Scope`** field (`AggMode`). This doc's "mode mapping" means **`Scope`**.

### 1.3 The gap

The L4 IR can *record* a binding — `L4Node::SummaryAgg { sketch, params, by,
... }` (`sketch_algebra/expr.rs:41`) — but:

1. **No L4 binding rule** turns `AggIntent`+`AccuracyTarget` into
   `(SummaryKind, SummaryParams)`.
2. **No L5 emitter** turns `SummaryAgg` into an edge `PrecomputeConfig`.

Both are called out as future work in `docs/design.md` (the `optimizer/rules/`
and `physical/emit/` modules don't exist yet). This doc specifies the two
mapping functions.

---

## 2. Decision 1 — Sketch family + params (L4 binding rule)

A pure function `bind(intent, accuracy, ctx) -> (SummaryKind, SummaryParams)`.
`ctx` carries deployment constraints (available families, memory budget) and
workload estimates (expected cardinality, value range).

| `AggIntent` | `SummaryKind` | Params from accuracy | Notes |
| --- | --- | --- | --- |
| `Quantile{q,ε}` | `DDSketch` (relative-error) or `Kll` (rank-error) | DDSketch `alpha=ε`; KLL `k≈1/ε` | pick by whether the query wants relative or rank error; default DDSketch for latency-style metrics |
| `TopK{k,ε}` | `CmsWithHeap` | `width≈e/ε`, `depth≈ln(1/δ)`, `heap_size=k` | heavy-hitter |
| `Cardinality{ε[,δ]}` | `Hll` (or `Theta` for set-ops) | `precision p = ⌈log2((1.04/ε)²)⌉` clamped to [4,18] | see Decision 3 for sparse |
| `Count{ε}` (per-key freq) | `Cms` | `width≈e/ε`, `depth≈ln(1/δ)` | when grouped/keyed |
| `Count{Exact}` | `Count` | — | exact accumulator |
| `Sum`/`Avg` | `Sum` (Avg = Sum+Count) | — | exact |
| `Min`/`Max` | `MinMax` | — | exact |
| `Rate`/`Increase` | `Rate`/`Increase` | — | per-series accumulators |
| `StdDev`/`Variance` | `Sum`(+Sum²+Count) | — | moment accumulators |

Edge `SketchType` is the projection of `SummaryKind` onto the families the edge
implements (DDSketch, KLL, HLL, CountSketch, CountMinSketch, Sum). `CmsWithHeap`
→ CountSketch/CountMin with `emit_heap`. `Theta`/`Kmv` have no edge family yet
→ either fall back to `Hll` or reject at bind time (deployment-constraint
check).

---

## 3. Decision 2 — Scope: per-series vs. whole-stream

This is the decision the dual-mode work (#471) exposed. It is driven entirely
by **grouping + the per-series marker**, already in the L3 IR.

### 3.1 The rule

```
scope(Aggregate { by, aggs, child }) =
    if aggs[0].is_per_series()                      -> PerSeries   // Rate/Increase
    else if child is TimeRange && by.is_empty()     -> PerSeries   // *_over_time per series
    else if by.is_empty()                           -> WholeStream // global, cross-series
    else /* by non-empty */                         -> PerSeries with AggregateBy = by
```

Mapping to edge fields:

| L3 shape | Example | Edge `Scope` | Edge `AggregateBy` |
| --- | --- | --- | --- |
| per-series reduction (`Rate`/`Increase`, or `TimeRange` child, `by` empty) | `rate(http_requests[5m])` | `ModePerSeries` | `[]` (full label set per series) |
| grouped cross-series | `sum by (zone) (...)`, `quantile by (route) (...)` | `ModePerSeries` | `by` (e.g. `[zone]`) → one sketch per group |
| global cross-series, no group | `count(distinct user_id)`, fleet-wide `quantile(0.99, ...)` | **`ModeWholeStream`** | `[]` (collapsed to one sketch per AggID) |

The crucial case is the **last row**: a global, ungrouped, cross-series
aggregate (`by.is_empty()` **and not** per-series) is exactly when
whole-stream applies — one summary tracks the union of all series, memory
`O(S_sketch)` instead of `O(|series|·S_sketch)`.

### 3.2 Why grouped ≠ whole-stream

`sum by (zone)` is **not** whole-stream: it needs one sketch *per zone*, i.e.
per-series keyed by the `by` set. Whole-stream is reserved for the truly
ungrouped global aggregate. So `AggregateBy = by` and `Scope = PerSeries`
whenever `by` is non-empty; `Scope = WholeStream` only when `by` is empty and
the aggregate is cross-series.

### 3.3 Item subject (reconciling with `ItemLabel`)

For whole-stream cardinality/frequency families the edge can ingest either the
metric **value** or an **inner item dimension** (the existing `ItemLabel`
knob). The controller sets `ItemLabel` when the query counts/ranks a
*dimension* rather than a value — e.g. `count(distinct user_id)` ⇒ whole-stream
HLL with `ItemLabel = user_id` (distinct *items*), vs a global value-distribution
quantile ⇒ whole-stream DDSketch over the value. The signal is which
`ColumnId` the `AggIntent` references as its subject.

---

## 4. Decision 3 — Sparse vs. dense HLL

Sparse HLL (#66/#472) is a *memory* representation choice, invisible to
accuracy and wire format. So the rule optimizes expected memory, and is safe to
default aggressively.

### 4.1 The rule

```
hll_sparse(scope, expected_card, precision) =
    // sparse wins below the in-memory crossover (~4096 non-zero registers);
    // it costs nothing in accuracy or wire bytes, only a promote-on-growth.
    if scope == WholeStream      -> false   // single instance, high cardinality by construction; dense
    else if expected_card is unknown
                                  -> true    // per-series: most series are low-cardinality; sparse-by-default
    else if expected_card < promote_threshold(precision)
                                  -> true
    else                          -> false
```

Rationale:

- **Per-series HLL** is the whole reason sparse exists: at high series count,
  most individual series have low distinct cardinality, so each dense 16 KB
  array is mostly empty. Default **sparse** for per-series scope. Worst case
  (a series that does grow) auto-promotes to dense, so there's no accuracy or
  correctness risk — only a one-time promotion.
- **Whole-stream HLL** is a *single* instance tracking the union of all series
  → cardinality is high by construction → it will promote almost immediately,
  so start **dense** and skip the promotion churn.
- When the workload/catalog provides an expected-cardinality estimate, use it
  against the precision-derived promotion threshold; otherwise per-series
  defaults to sparse (the safe, common case).

### 4.2 Precision and sparse interact, but independently

`precision` is set by Decision 1 from ε; `sparse` only changes the in-memory
layout for that precision. They compose freely — sparse p=14 and dense p=14
produce byte-identical snapshots.

---

## 5. The end-to-end mapping

```
QueryExpr (L3)
   └─ Aggregate { by, aggs:[AggIntent], child }
        │
        │  L4 binding rule  (NEW — core/optimizer/rules/)
        ▼
   SummaryAgg { sketch: SummaryKind, params: SummaryParams, by }   (sketch_algebra/expr.rs:41)
        │
        │  L5 emitter  (NEW — deployment-model emitter)
        ▼
   PrecomputeConfig {
       AggID:        <stable id for this aggregation>,
       SketchType:   project(SummaryKind),
       SketchParams: project(SummaryParams) + {"sparse": hll_sparse(...)},
       Scope:        scope(Aggregate),                 // §3
       AggregateBy:  by_as_label_names,                // §3
       Window/Mode/Delta/Temporality/MetricName/MaxSeries: from deployment policy,
   }
        │  batched into PrecomputeConfigSet{Version, []PrecomputeConfig}
        ▼
   edge (asap-precompute-go) via control-plane poll/push
```

### 5.1 AggID stability

`AggID` must be stable across re-planning for the same logical aggregation, so
that a config update (e.g. a precision change) lands on the existing edge
aggregator via `UpdateConfig` rather than orphaning state. Derive it from a
canonical hash of `(metric, AggIntent-kind, by, accuracy)` — *not* from plan
position. A change to `Scope` deliberately resets the edge window (#471's
`resetForScopeChange`), so scope flips are correct but lossy for the in-flight
window; keep `AggID` stable across them anyway so the aggregator object is
reused.

### 5.2 Versioning

The emitter bumps `PrecomputeConfigSet.Version` monotonically; the edge acks
and applies in place. Same-scope param changes (precision, delta) are hot;
scope flips reset the window. This is already implemented edge-side (#471).

---

## 6. Worked examples

| Query | family | scope | AggregateBy | sparse | notes |
| --- | --- | --- | --- | --- | --- |
| `count(distinct user_id)` (global) | HLL | WholeStream | `[]` | dense | one instance, `ItemLabel=user_id` |
| `count by (region) (distinct user_id)` | HLL | PerSeries | `[region]` | sparse | one HLL per region; most regions low-card |
| `histogram_quantile(0.99, rate(latency[5m]))` | DDSketch | PerSeries | `[]` | n/a | per-series; `alpha` from ε |
| `quantile(0.99, latency)` (fleet-wide) | DDSketch | WholeStream | `[]` | n/a | pooled value distribution |
| `topk(10, sum by (endpoint)(qps))` | CmsWithHeap | PerSeries | `[endpoint]` | n/a | heap_size=10 |
| `sum(cpu_seconds)` (global) | Sum | WholeStream | `[]` | n/a | grand total |
| `sum by (zone)(cpu_seconds)` | Sum | PerSeries | `[zone]` | n/a | one sum per zone |

---

## 7. Where to implement (file map)

| Piece | Location | Status |
| --- | --- | --- |
| L4 binding rule `bind(intent,accuracy,ctx)` (§2) | `crates/core/src/optimizer/rules/` (new) | to build |
| Scope rule `scope(Aggregate)` (§3) | same rule module; reads `query_expr.rs` markers | to build |
| Sparse rule `hll_sparse(...)` (§4) | same rule module | to build |
| L5 emitter `SummaryAgg → PrecomputeConfig` (§5) | deployment-model emitter (`crates/deployment-model-asapquery/…`, new) | to build |
| `PrecomputeConfig`/`PrecomputeConfigSet` wire type (Rust mirror of the Go struct) | new, in the emitter crate or a shared wire crate | to build |
| Edge consumption (`Scope`, `hll_sparse`, `AggregateBy`) | `asap-precompute-go` / `asapedgeprocessor` | **done** (#471/#472) |

### 7.1 Suggested build order

1. Rust mirror of `PrecomputeConfig`/`PrecomputeConfigSet` with serde matching
   the Go JSON (the edge already deserializes it). Round-trip test against a Go
   fixture.
2. The three pure mapping functions (§2/§3/§4) with table-driven unit tests
   (the §6 examples are the fixtures).
3. The L5 emitter wiring them together + `AggID` derivation (§5.1).
4. End-to-end: a PromQL/SQL query → `PrecomputeConfigSet` golden test.

---

## 8. Open questions

1. **Expected cardinality source.** Sparse HLL and CMS sizing want a
   cardinality estimate the query doesn't carry. Catalog stats? Online
   feedback from edge `Stats()`? Default-and-adapt? (§4 defaults to
   sparse-when-unknown for per-series, which is safe but not optimal.)
2. **DDSketch vs. KLL for `Quantile`.** Relative vs. rank error — pick by
   metric semantics (latency ⇒ DDSketch) or expose as a query hint?
3. **Theta/KMV with no edge family.** Reject at bind time, or fall back to HLL
   (losing set-ops)?
4. **Per-group cardinality explosion.** `count by (high_card_label)(distinct
   ...)` is per-series HLL with huge group count — does `MaxSeries`/overflow
   policy suffice, or should the planner refuse / downgrade precision?
5. **Whole-stream + windowing.** Confirm whole-stream composes with all
   `Window`/`Mode` settings the planner may choose (it does edge-side; the
   emitter must not emit Sliding into an additive-merge backend — see the
   edge's Sliding warning).
