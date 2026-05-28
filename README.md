# ASAPController

Unified control plane for the ASAP data-lifecycle stack.

This repo merges three previously-separate projects into a single workspace with a shared core and per-deployment-model plugin crates:

- `DataCollector/controller` — end-to-end lifecycle planner (collection → transmission → storage → analytics).
- `ASAPQuery[-backend]/asap-planner-rs` — analytics-query-only planner (CLI, YAML in → YAML out).
- `asap-fusion` — DataFusion operator-level rewrite rules with sketch awareness.

## Status

**Early implementation.** The workspace builds; the front-end crates are landing per the migration plan below. Present today: `crates/core` (shared planner core) and `crates/lower` (SQL/PromQL → L2 lowering). The two docs below are the design + migration plan that the code is following.

- [`docs/design.md`](docs/design.md) — target architecture, repo layout, extension points, wire contracts.
- [`docs/migration-plan.md`](docs/migration-plan.md) — phase-by-phase plan to land the merger without disrupting any running deployment.

## Architecture (summary)

**5-layer query→sketch pipeline**:

| Layer | What | Where it lives |
|---|---|---|
| 1 | Query-language parsing (PromQL, SQL, DataFusion, ElasticDSL) | `core::query_language` |
| 2 | Per-language logical plan (relational algebra tree) | `core::logical_plan` |
| 3 | Intent algebra — language-, deployment-, AND data-model-independent IR (`QueryExpr` + `AggIntent`, intent only — no sketch type, no params). Supports both time-series and tabular data via a `Source` sum inside `Scan`. | `core::intent_algebra` |
| 4 | Cost-aware optimizer — rule engine + shared rule library; deployment models pick rules. Produces the **sketch algebra** IR `SketchExpr` (sketch-bound: kind + params committed). | **framework in `core::optimizer`**; sketch-bound IR in `core::sketch_algebra`; rule choices in each deployment model |
| 5 | Physical plan — stage allocation + emit to wire format | **framework in `core::physical`**; topology + emitter in each deployment model |

**Core owns the shared infrastructure across all 5 layers**, not just L1-3. Deployment models are thin: they pick which rules fire (L4), declare their deployment topology (L5), and provide an emitter for their output format. Typical deployment model crate: **500-2500 LOC**.

```
crates/
  core/                 # all 5 layers of shared infrastructure; no I/O
    query_language/     # L1 — parsers
    logical_plan/       # L2 — per-language algebra trees
    intent_algebra/     # L3 IR — QueryExpr + AggIntent (intent only, ~25 variants superset)
    sketch_algebra/     # L4 IR — SketchExpr (sketch-bound: kind + params committed)
    lower/              # L1→L2→L3 passes
    optimizer/          # L4 framework — rule engine + shared rule library + cost traits; produces SketchExpr
    physical/           # L5 framework — PhysicalPlanner + stage allocator + topology + sketch catalogue
    pipeline/           # L1→…→L5 driver, parameterized on deployment model
  runtime/              # HTTP / OpAMP / replanner / store — service skeleton
  deployment-model-asaplifecycle    # DC-specific rules + 3-stage topology + OTel/backend emitters
  deployment-model-asapquery        # precompute-engine rules + 1-stage + StreamingConfig/InferenceConfig YAML emitters
  deployment-model-asapfusion       # DataFusion rewrites + 0-stage (in-process) + LogicalPlan emit
  control-proto         # OpAMP proto + internal proto (tonic/prost)
  testing               # shared fixtures
bin/
  asap-controller       # long-running service, all deployment models
  asap-query             # one-shot CLI (deployment-model-asapquery only)
  asap-lifecycle        # OPTIONAL: standalone service with only deployment-model-asaplifecycle
  asap-fusion-bench     # OPTIONAL: benchmark harness over deployment-model-asapfusion
```

A new deployment model lands by adding one crate with `rules.rs` (pick L4 rules) + `topology.rs` + an emitter, plus one line in `bin/asap-controller/main.rs`. No changes to core or runtime.

## Consumption modes

Three ways downstream can use ASAPController — mix as needed:

| Mode | Use case | How |
|---|---|---|
| **Rust library** | In-process use of the IR or a specific deployment model (e.g. asap-fusion benchmarks) | `Cargo.toml`: `asap-control-core = { git = "...", tag = "v0.1.0" }` or any individual `deployment-model-*` crate. Per-crate dep isolation keeps dep trees small (deployment-model-asapfusion pulls DataFusion; deployment-model-asapquery pulls PromQL/SQL parsers; neither pulls axum/OpAMP). |
| **HTTP service sidecar** | Production control plane — e.g. ASAPQuery-backend POSTing QuerySpec on capability-miss | Run `asap-controller` binary, POST to `/api/v1/plan`. Same contract DC controller speaks today. |
| **CLI / Docker image** | One-shot init container (e.g. docker-compose init job that writes `streaming_config.yaml`) | `asap-controller plan --workload ... --output-dir ...` or the dedicated `asap-query` binary |

## Data plane lives elsewhere

ASAPController is the **control plane only**. The data plane — OTel collectors, ASAPQuery-backend's query engine, asap-fusion users' DataFusion runtimes — stays in its original repo. Communication is always over wire (OpAMP, HTTP, Prometheus scrape). This boundary is unchanged by the merger.

## Deployment model placement: flexible

Because core now owns the L4/L5 infrastructure (not just L1-3), deployment model crates are small and largely self-contained. A deployment model can live either:

- **Inside ASAPController workspace** (lockstep release with core, one-PR cross-deployment-model changes)
- **In its own downstream repo** (independent release cadence, depends on published `asap-control-core`)

Both produce functionally identical artifacts. The placement is a deployment/team-ownership decision, not an architectural fork. Default:
- `deployment-model-asaplifecycle` + `deployment-model-asapquery` in ASAPController workspace (share a YAML emitter).
- `deployment-model-asapfusion` in the `asap-fusion` repo (research cadence, independent release).

## Building

The build is a normal `cargo build`. The only setup is GitHub access: `crates/lower`
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
