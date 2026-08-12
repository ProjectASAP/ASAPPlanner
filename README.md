# ASAPController

This repo unifies the common parts of Query-to-Primitive/Summary translation and query optimization logic, in ASAP, including ASAPQuery, ASAPFusion, ASAPCollector, ASAPBGP, ASAPWavelets, etc. 

## Status

The workspace builds and is organized into a
layer-named crate stack (the query→summary pipeline reads top-to-bottom).
[`docs/design.md`](docs/design.md) is the design index the code is
following — an overview of the L1-L5 pipeline, linking into a dedicated
doc per layer; the deployment-model / runtime / bin crates it describes
are still planned.

## Architecture for control plane in ASAP

**5-layer query→summary pipeline** (see [`docs/design.md`](docs/design.md) for the full design):

| Layer | What | Where it lives |
|---|---|---|
| 1 | Query-language parsing (PromQL, SQL; DataFusion, ElasticDSL planned) | `asap-frontend-promql`, `asap-frontend-sql` |
| 2 | Per-language relational algebra tree + the shared L2→L3 converter (incl. the post-lowering canonicalization pass both languages run through) | `asap-l2` (emitted by the front ends) |
| 3 | Intent algebra defined by ASAPController itself — query language and runtime independent IR (intent only — no summary type, no summary params). | `asap-ir::intent_algebra` |
| 4 | Cost-aware optimizer — CSE + pluggable cost model + the summary-vs-exact accuracy decision (`AggIntent → SummaryKind`, landed). Produces the **summary-bound** IR (kind + params committed), plus the serving-time `SummaryExecutor` interface that answers a query against it. | binding/optimizer passes in `asap-plan`; summary-bound IR + serving-time executor in `asap-sketch` |
| 5 | Physical runtime / Data plane — The controller emits configurations to physical runtime, with the execution environment consider the parallelism, hardware types, lifecycle stages, distributed workers | The implementation of this layer should be in different downstream application repos. |

### Crates

The workspace splits by pipeline role. Every crate depends (directly or
transitively) on **`asap-ir`**, and **`asap-ir` depends on nothing above it** —
that one rule keeps the shared IR free of any query-language, optimizer, or
runtime coupling.

| Crate | Role | Depends on | ~LOC |
|---|---|---|---|
| **`asap-ir`** | **L3 canonical IR only** — `QueryExpr` + `AggIntent` + scalar expr IR + schema + names | *(nothing — no query-language deps)* | 3,400 |
| `asap-l2` | L2 per-language relational algebra + the L2→L3 converter (`convert_root`, binder, column resolution) + the shared post-lowering `canonicalize` pass | `asap-ir` | 2,700 |
| `asap-sketch` | L4 summary-bound IR (`SummaryExpr` / `L4Node` type definitions) + the serving-time `SummaryExecutor` trait / `execute` walker that answers a query against an already-bound plan | `asap-ir` | 1,000 |
| `asap-plan` | optimizer layer — CSE (landed); L3→L4 binding + the summary-vs-exact accuracy decision (landed, #98); pluggable `CostModel` interface (landed skeleton — wiring workload-level CSE credit into it is still open, #6/#33) | `asap-ir`, `asap-sketch` | 2,000 |
| `asap-frontend-promql` | PromQL L1→L2 (+ `HistogramCatalog` sample-type metadata, #79) | `asap-ir`, `asap-l2`, **promql-parser** | 2,100 |
| `asap-frontend-sql` | SQL L1→L2 | `asap-ir`, `asap-l2`, **datafusion** | 1,700 |
| `asap-lower` | facade re-exporting both front ends; owns the cross-language equivalence tests (`tests/cross_language.rs`) | the two `frontend-*` crates | 15 |
| `asap-e2e` | PromQL→L4 integration tests (lowering through L3→L4 binding) + shared fixtures | `asap-ir`, `asap-frontend-promql`, `asap-sketch`, `asap-plan` | 45 |

Two isolation wins fall out of this:
- **The front ends quarantine their parsers** — a caller that needs only PromQL depends on `asap-frontend-promql` and never compiles DataFusion, and vice-versa (verified with `cargo tree`). `asap-lower` is the facade for callers that want both.
- **L3-only consumers skip the L2 machinery** — `asap-sketch` (L4 types) depends on `asap-ir` alone, and `asap-plan` (L3→L4 binding + optimizer) adds only `asap-sketch` on top of that — neither pulls the L2 relational tree, the converter, or the binder (`asap-l2`). Only the front ends, which actually *lower* queries, need `asap-l2`.

### Directory structure

```
crates/
├── ir/                             # asap-ir — L3 canonical IR (the shared vocabulary)
│   └── src/
│       ├── lib.rs
│       ├── types.rs                #   AccuracyTarget, …
│       ├── workload.rs             #   QueryWorkload / QueryLanguage / SqlDialect (front-end input)
│       └── intent_algebra/
│           ├── query_expr.rs       #   L3: canonical QueryExpr — the IR everything pivots on
│           ├── agg_intent.rs       #   L3: AggIntent vocabulary (Sum/Quantile/Rate/TopK/…)
│           ├── expr_ir.rs          #   scalar expr IR (L2Expr / L3Expr / ColumnRef)
│           ├── schema.rs           #   per-edge Schema + unique-keys
│           └── names.rs            #   BindingName / QueryId
├── l2/                             # asap-l2 — L2 relational algebra + L2→L3 converter
│   └── src/
│       ├── relational.rs           #   L2: per-language relational tree the front ends emit
│       ├── lower.rs                #   L2→L3 converter (convert_root)
│       ├── canonicalize.rs         #   shared post-lowering normalization (heavy-hitter TopK, #34)
│       ├── binder.rs               #   positional name-resolution seed
│       └── column_resolution.rs
├── sketch/                         # asap-sketch — L4 summary-bound IR
│   └── src/                        #   expr.rs/schema.rs/sketch.rs (types) + exec.rs (serving-time SummaryExecutor)
├── plan/                           # asap-plan — L3→L4 binding + optimizer
│   └── src/                        #   bind.rs (L3→L4 binding) + boundary.rs (accuracy decision) + cost_model.rs (pluggable CostModel) + cse.rs
├── frontend-promql/                # asap-frontend-promql — promql.rs + histogram.rs (#79) + error.rs
├── frontend-sql/                   # asap-frontend-sql — sql/{mod,expr,types}.rs + error.rs (SqlError)
├── lower/                          # asap-lower — facade (re-exports both front ends) + cross-language tests
└── e2e/                            # asap-e2e — PromQL→L4 integration tests (through L3→L4 binding) + fixtures

# planned (see docs/l5-physical-plan.md): L5 physical framework, runtime
# service, deployment-model-* crates, control-proto, and the bin/ entrypoints.
```
 

## Building

The build is a normal `cargo build`. The only setup is GitHub access: `crates/frontend-promql`
depends on our private PromQL parser fork (`ProjectASAP/promql-parser`, branch `asap`)
as a git dependency, so Cargo has to be able to clone a private repo.

1. Get added to the `ProjectASAP/promql-parser` repo (ask an org owner).

2. Let git authenticate to GitHub over HTTPS. Easiest is the `gh` CLI as a
   credential helper:

   ```bash
   gh auth login        # choose GitHub.com → HTTPS, follow the prompts
   gh auth setup-git    # registers gh as git's credential helper
   ```

3. Build:

   ```bash
   cargo build
   ```

## Running

There's no standalone binary yet — the `bin/` + runtime crates are still
planned (see the "Physical runtime / Data plane" row above). Today the
workspace is exercised through its test suite and through runnable examples.

**Run the tests:**

```bash
cargo test --workspace
```

**Visualize the IR.** `asap-lower` ships an example, `topk_ir`, that lowers a
batch of PromQL and SQL queries through L1→L2→L3 and pretty-prints the
resulting `QueryExpr` — the L3 canonical IR (see
[`docs/l3-intent-algebra.md`](docs/l3-intent-algebra.md)) — as a tree:

```bash
cargo run -p asap-lower --example topk_ir
```

For each query it prints the source text followed by its `QueryExpr` tree via
Rust's `{:#?}` pretty-debug formatting. For example, `topk(5,
rate(http_requests_total[5m]))` lowers to:

```
━━━ P3 — topk over rate ━━━
topk(5, rate(http_requests_total[5m]))
Limit {
    n: 5,
    offset: 0,
    child: Sort {
        keys: [
            SortKey {
                expr: Column(
                    1,
                ),
                ascending: false,
                nulls_first: false,
            },
        ],
        partition_by: GroupKeys {
            keys: [],
            without: false,
        },
        child: Aggregate {
            reduction: PerEntity,
            aggs: [
                Rate,
            ],
            output_names: [
                "",
            ],
            having: None,
            child: TimeRange {
                range: 300s,
                child: Scan {
                    source: TimeSeries {
                        metric: "http_requests_total",
                    },
                    predicates: [],
                    schema: Schema {
                        columns: [
                            Column {
                                name: "ts",
                                dtype: Timestamp,
                                nullable: false,
                                table: None,
                            },
                            Column {
                                name: "value",
                                dtype: Float64,
                                nullable: false,
                                table: None,
                            },
                        ],
                        time_index: Some(
                            0,
                        ),
                        unique_keys: [],
                        closed: false,
                    },
                },
            },
        },
    },
}
```

To visualize a query of your own, edit `crates/lower/examples/topk_ir.rs` (or
copy it into a new example) and call `lower_promql(query, AccuracyTarget::Exact)`
or `lower_sql(query, &catalog, AccuracyTarget::Exact).await`, then print the
returned `QueryExpr` with `{:#?}`.
