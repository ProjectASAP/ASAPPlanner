# ASAPController

Unified control plane for the ASAP data-lifecycle stack.

This repo merges three previously-separate projects into a single workspace with a shared core and per-scenario plugin crates:

- `DataCollector/controller` — end-to-end lifecycle planner (collection → transmission → storage → analytics).
- `ASAPQuery[-backend]/asap-planner-rs` — analytics-query-only planner (CLI, YAML in → YAML out).
- `asap-fusion` — DataFusion operator-level rewrite rules with sketch awareness.

## Status

**Design phase.** The two docs below are the current design + migration plan; no code has landed yet.

- [`docs/design.md`](docs/design.md) — target architecture, repo layout, extension points, wire contracts.
- [`docs/migration-plan.md`](docs/migration-plan.md) — phase-by-phase plan to land the merger without disrupting any running deployment.

## Architecture (summary)

**5-layer query→sketch pipeline**:

| Layer | What | Where it lives |
|---|---|---|
| 1 | Query-language parsing (PromQL, SQL, DataFusion, ElasticDSL) | `core::query_language` |
| 2 | Per-language logical plan (relational algebra tree) | `core::logical_plan` |
| 3 | Sketch algebra — language-, deployment-, AND data-model-independent IR (`QueryExpr` + `AggIntent`, intent only). Supports both time-series and tabular data via a `Source` sum inside `Scan`. | `core::sketch_algebra` |
| 4 | Cost-aware optimizer — rule engine + shared rule library; scenarios pick rules | **framework in `core::optimizer`**; choices in each scenario |
| 5 | Physical plan — stage allocation + emit to wire format | **framework in `core::physical`**; topology + emitter in each scenario |

**Core owns the shared infrastructure across all 5 layers**, not just L1-3. Scenarios are thin: they pick which rules fire (L4), declare their deployment topology (L5), and provide an emitter for their output format. Typical scenario crate: **500-2500 LOC**.

```
crates/
  core/                 # all 5 layers of shared infrastructure; no I/O
    query_language/     # L1 — parsers
    logical_plan/       # L2 — per-language algebra trees
    sketch_algebra/     # L3 — QueryExpr + AggIntent (intent only, ~25 variants superset)
    lower/              # L1→L2→L3 passes
    optimizer/          # L4 framework — rule engine + shared rule library + cost traits
    physical/           # L5 framework — PhysicalPlanner + stage allocator + topology + sketch catalogue
    pipeline/           # L1→…→L5 driver, parameterized on scenario
  runtime/              # HTTP / OpAMP / replanner / store — service skeleton
  scenario-lifecycle    # DC-specific rules + 3-stage topology + OTel/backend emitters
  scenario-query        # precompute-engine rules + 1-stage + StreamingConfig/InferenceConfig YAML emitters
  scenario-fusion       # DataFusion rewrites + 0-stage (in-process) + LogicalPlan emit
  control-proto         # OpAMP proto + internal proto (tonic/prost)
  testing               # shared fixtures
bin/
  asap-controller       # long-running service, all scenarios
  asap-query             # one-shot CLI (scenario-query only)
  asap-lifecycle        # OPTIONAL: standalone service with only scenario-lifecycle
  asap-fusion-bench     # OPTIONAL: benchmark harness over scenario-fusion
```

A new scenario lands by adding one crate with `rules.rs` (pick L4 rules) + `topology.rs` + an emitter, plus one line in `bin/asap-controller/main.rs`. No changes to core or runtime.

## Consumption modes

Three ways downstream can use ASAPController — mix as needed:

| Mode | Use case | How |
|---|---|---|
| **Rust library** | In-process use of the IR or a specific scenario (e.g. asap-fusion benchmarks) | `Cargo.toml`: `asap-control-core = { git = "...", tag = "v0.1.0" }` or any individual `scenario-*` crate. Per-crate dep isolation keeps dep trees small (scenario-fusion pulls DataFusion; scenario-query pulls PromQL/SQL parsers; neither pulls axum/OpAMP). |
| **HTTP service sidecar** | Production control plane — e.g. ASAPQuery-backend POSTing QuerySpec on capability-miss | Run `asap-controller` binary, POST to `/api/v1/plan`. Same contract DC controller speaks today. |
| **CLI / Docker image** | One-shot init container (e.g. docker-compose init job that writes `streaming_config.yaml`) | `asap-controller plan --workload ... --output-dir ...` or the dedicated `asap-query` binary |

## Data plane lives elsewhere

ASAPController is the **control plane only**. The data plane — OTel collectors, ASAPQuery-backend's query engine, asap-fusion users' DataFusion runtimes — stays in its original repo. Communication is always over wire (OpAMP, HTTP, Prometheus scrape). This boundary is unchanged by the merger.

## Scenario placement: flexible

Because core now owns the L4/L5 infrastructure (not just L1-3), scenario crates are small and largely self-contained. A scenario can live either:

- **Inside ASAPController workspace** (lockstep release with core, one-PR cross-scenario changes)
- **In its own downstream repo** (independent release cadence, depends on published `asap-control-core`)

Both produce functionally identical artefacts. The placement is a deployment/team-ownership decision, not an architectural fork. Default:
- `scenario-lifecycle` + `scenario-query` in ASAPController workspace (share a YAML emitter).
- `scenario-fusion` in the `asap-fusion` repo (research cadence, independent release).

## Key design references

- 5-layer pipeline: `docs/design.md` §3
- Core internals (L1-3 + L4 framework + L5 framework): `docs/design.md` §6
- Scenario internals (each's rules + topology + emitter): `docs/design.md` §8
- Extension point for future scenarios: `docs/design.md` §9
- Consumption modes + dependency isolation: `docs/design.md` §11
- Open questions (incl. L2-tree contract, scenario placement): `docs/design.md` §12
- Phase-by-phase migration: `docs/migration-plan.md`
