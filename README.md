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
| 5 | Physical runtime — The controller emits configurations to physical runtime, with the execution environment consider the parallelism, hardware types, lifecycle stages, distributed workers | The implementation of this layer should be in different downstream application repos. |

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

**Why L2 and L3 are separate crates.** `asap-ir` is the canonical L3 IR — the
vocabulary every downstream layer pivots on. The L2 relational tree and the
L2→L3 converter live in `asap-l2` because only the *front ends* need them: they
emit L2 and call `convert_root`. Keeping them out of `asap-ir` means the
optimizer (`asap-plan`), the summary IR (`asap-sketch`), and any future
L3-consuming layer compile against a lean core without the converter/binder
machinery. (The converter co-locates with L2 rather than L3 because it owns the
L2 tree definition and only *reads* L3.)

*(Planned.)* A new deployment model will land by adding one crate with `rules.rs` (pick L4 rules) + `topology.rs` + an emitter, plus one line in `bin/asap-controller/main.rs` — no changes to the IR crates.

## Consumption modes

**Today only the Rust-library mode below is available** (depend on `asap-ir` for the IR, or a front-end crate to lower queries). The HTTP-service and CLI modes ship with the runtime + bin crates, which are planned (see `docs/design.md`). Three intended ways downstream can use ASAPController — mix as needed:

| Mode | Use case | How |
|---|---|---|
| **Rust library** | In-process use of the IR or a specific deployment model (e.g. asap-fusion benchmarks) | `Cargo.toml`: `asap-ir = { git = "...", tag = "v0.1.0" }` or any individual `deployment-model-*` crate. Per-crate dep isolation keeps dep trees small (deployment-model-asapfusion pulls DataFusion; deployment-model-asapquery pulls PromQL/SQL parsers; neither pulls axum/OpAMP). |
| **HTTP service sidecar** | Production control plane — e.g. ASAPQuery-backend POSTing QuerySpec on capability-miss | Run `asap-controller` binary, POST to `/api/v1/plan`. Same contract DC controller speaks today. |
| **CLI / Docker image** | One-shot init container (e.g. docker-compose init job that writes `streaming_config.yaml`) | `asap-controller plan --workload ... --output-dir ...` or the dedicated `asap-query` binary |

## Data plane lives elsewhere

ASAPController is the **control plane only**. The data plane — OTel collectors, ASAPQuery-backend's query engine, asap-fusion users' DataFusion runtimes — stays in its original repo. Communication is always over wire (OpAMP, HTTP, Prometheus scrape). This boundary is unchanged by the merger.

## Deployment model placement: flexible *(planned)*

Once the L4/L5 infrastructure lands in the IR/optimizer crates (not just L1-3), deployment model crates will be small and largely self-contained. A deployment model can live either:

- **Inside ASAPController workspace** (lockstep release with core, one-PR cross-deployment-model changes)
- **In its own downstream repo** (independent release cadence, depends on published `asap-ir`)

Both produce functionally identical artifacts. The placement is a deployment/team-ownership decision, not an architectural fork. Default:
- `deployment-model-asaplifecycle` + `deployment-model-asapquery` in ASAPController workspace (share a YAML emitter).
- `deployment-model-asapfusion` in the `asap-fusion` repo (research cadence, independent release).

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

Cargo resolves the parser to the commit pinned in `Cargo.lock`, fetches it via git
(authenticated by `gh`), and compiles the workspace.

**Troubleshooting.** If the fetch fails with `authentication failed` or
`repository not found`, it's one of two things: your account doesn't have access to
the private repo, or git isn't using your credentials — re-run `gh auth setup-git`.
Don't hand-edit `~/.gitconfig` with a `url.…insteadOf` rule containing a personal
access token; that embeds *your* token in plaintext and shares it with anyone who
copies the snippet. Keep credentials in `gh` (or git's keychain helper) instead.

## Key design references

- 5-layer pipeline overview + glossary: [`docs/design.md`](docs/design.md)
- Per-layer detail: [`docs/l1-query-language.md`](docs/l1-query-language.md),
  [`docs/l2-logical-plan.md`](docs/l2-logical-plan.md),
  [`docs/l3-intent-algebra.md`](docs/l3-intent-algebra.md),
  [`docs/l4-summary-bound-ir.md`](docs/l4-summary-bound-ir.md) (also covers
  serving-time execution), [`docs/l5-physical-plan.md`](docs/l5-physical-plan.md)
- Consumption modes + dependency isolation: "Consumption modes" above
