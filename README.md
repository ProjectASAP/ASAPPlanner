# ASAPController

Unified control plane for the ASAP data-lifecycle stack.

This repo merges three previously-separate projects into a single workspace with a shared core and per-deployment-model plugin crates:

- `DataCollector/controller` — end-to-end lifecycle planner (collection → transmission → storage → analytics).
- `ASAPQuery[-backend]/asap-planner-rs` — analytics-query-only planner (CLI, YAML in → YAML out).
- `asap-fusion` — DataFusion operator-level rewrite rules with sketch awareness.

## Status

**Early implementation.** The workspace builds and is organized into a
layer-named crate stack (the query→sketch pipeline reads top-to-bottom). The
two docs below are the broader design + migration plan the code is following;
the deployment-model / runtime / bin crates they describe are still planned.

- [`docs/design.md`](docs/design.md) — target architecture, repo layout, extension points, wire contracts.
- [`docs/migration-plan.md`](docs/migration-plan.md) — phase-by-phase plan to land the merger without disrupting any running deployment.

## Architecture (summary)

**5-layer query→sketch pipeline**:

| Layer | What | Where it lives (today) |
|---|---|---|
| 1 | Query-language parsing (PromQL, SQL; DataFusion, ElasticDSL planned) | `asap-frontend-promql`, `asap-frontend-sql` |
| 2 | Per-language relational algebra tree | `asap-ir::intent_algebra::relational` (emitted by the front ends) |
| 3 | Intent algebra — language-, deployment-, AND data-model-independent IR (`QueryExpr` + `AggIntent`, intent only — no sketch type, no params). Supports both time-series and tabular data via a `Source` sum inside `Scan`. | `asap-ir::intent_algebra` |
| 4 | Cost-aware optimizer — CSE + cost model + sketch-vs-exact boundary + canonicalization. Produces the **sketch algebra** IR (sketch-bound: kind + params committed). | optimizer passes in `asap-plan`; sketch-bound IR in `asap-sketch` |
| 5 | Physical plan — stage allocation + emit to wire format | *planned* (per-deployment-model) |

### Crates

The workspace splits by pipeline role. Every crate depends (directly or
transitively) on **`asap-ir`**, and **`asap-ir` depends on nothing above it** —
that one rule keeps the shared IR free of any query-language, optimizer, or
runtime coupling.

| Crate | Role | Depends on | ~LOC |
|---|---|---|---|
| **`asap-ir`** | L2 relational + L3 canonical IR + L2→L3 converter, binder, resolution, schema, expr, workload types | *(nothing — no query-language deps)* | 3,900 |
| `asap-sketch` | L4 sketch-bound IR (`SketchExpr`) | `asap-ir` | 240 |
| `asap-plan` | optimizer layer — CSE (landed); cost-model / boundary / canonicalize (stubs) | `asap-ir` | 290 |
| `asap-frontend-promql` | PromQL L1→L2 | `asap-ir`, **promql-parser** | 1,030 |
| `asap-frontend-sql` | SQL L1→L2 | `asap-ir`, **datafusion** | 1,130 |
| `asap-lower` | facade re-exporting both front ends | the two `frontend-*` crates | 15 |
| `asap-e2e` | cross-language integration tests | `asap-frontend-promql` | 40 |

Splitting the front ends **quarantines their parsers**: a caller that needs
only PromQL depends on `asap-frontend-promql` and never compiles DataFusion,
and vice-versa (verified with `cargo tree`). `asap-lower` is the convenience
facade for callers that want both.

### Directory structure

```
crates/
├── ir/                             # asap-ir — the shared IR (largest crate)
│   └── src/
│       ├── lib.rs
│       ├── types.rs                #   AccuracyTarget, …
│       ├── workload.rs             #   QueryWorkload / QueryLanguage / SqlDialect (front-end input)
│       └── intent_algebra/
│           ├── relational.rs       #   L2: per-language relational tree the front ends emit
│           ├── query_expr.rs       #   L3: canonical QueryExpr — the IR everything pivots on
│           ├── agg_intent.rs       #   L3: AggIntent vocabulary (Sum/Quantile/Rate/TopK/…)
│           ├── expr_ir.rs          #   scalar expr IR (L2Expr / L3Expr)
│           ├── lower.rs            #   L2→L3 converter (convert_root)
│           ├── binder.rs           #   positional name-resolution seed
│           ├── column_resolution.rs
│           ├── schema.rs           #   per-edge Schema + unique-keys
│           └── names.rs
├── sketch/                         # asap-sketch — L4 sketch IR: expr / schema / sketch
├── plan/                           # asap-plan — optimizer: cse.rs (landed)
│   └── src/                        #   + cost_model.rs / boundary.rs / canonicalize.rs (stubs)
├── frontend-promql/                # asap-frontend-promql — promql.rs + error.rs (PromqlError)
├── frontend-sql/                   # asap-frontend-sql — sql/{mod,expr,types}.rs + error.rs (SqlError)
├── lower/                          # asap-lower — facade (lib.rs re-exports both front ends)
└── e2e/                            # asap-e2e — cross-language integration tests

# planned (docs/design.md §5.2): L5 physical framework, runtime service,
# deployment-model-* crates, control-proto, and the bin/ entrypoints.
```

**Why `asap-ir` holds the most.** L2 (the per-language relational tree) and L3
(the canonical intent algebra) live in one crate because the **L2→L3 converter
needs both** — front ends only *emit* L2, they don't own it. Its modules
(`relational` / `query_expr` / `agg_intent` / `schema` / `binder` / …) are
cleanly separated and could split into their own crates later if the crate
grows unwieldy; for now they share one compilation unit to keep the converter's
tight coupling in-crate.

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

- 5-layer pipeline: `docs/design.md` §3
- Core internals (L1-3 + L4 framework + L5 framework): `docs/design.md` §6
- Deployment model internals (each's rules + topology + emitter): `docs/design.md` §8
- Extension point for future deployment models: `docs/design.md` §9
- Consumption modes + dependency isolation: `docs/design.md` §11
- Open questions (incl. L2-tree contract, deployment model placement): `docs/design.md` §12
- Phase-by-phase migration: `docs/migration-plan.md`
