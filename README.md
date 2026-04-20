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

Organised around DC controller's documented **5-layer query→sketch pipeline**:

| Layer | What | Where it lives |
|---|---|---|
| 1 | Query-language parsing (PromQL, SQL, DataFusion, ElasticDSL) | `core::query_language` |
| 2 | Per-language logical plan (relational algebra tree) | `core::logical_plan` |
| 3 | Sketch algebra — language- and deployment-independent IR (`QueryExpr` + `AggIntent`) | `core::sketch_algebra` |
| 4 | Cost-aware optimizer — rewrite rules | **each scenario's `optimizer/`** |
| 5 | Physical plan — concrete sketch types + stage allocation | **each scenario's `physical/` + `emit/`** |

**Layers 1-3 are language- and workload-independent** — so they live in `core` and are shared across every scenario. **Layers 4-5 are deployment-specific** — each scenario owns its own rules + physical lowering + emitter.

```
crates/
  core/              # L1-3 (parsers + lowering + sketch algebra) + traits; no I/O
  runtime/           # HTTP / OpAMP / replanner / store
  scenario-lifecycle # L4+L5 for full-stack (ex-DC controller)
  scenario-query     # L4+L5 for analytics-query-only (ex-asap-planner-rs)
  scenario-fusion    # L4+L5 for DataFusion operator fusion (ex-asap-fusion)
  control-proto      # OpAMP + internal proto
  testing            # shared fixtures
bin/
  asap-controller    # service binary
  asap-plan          # one-shot CLI
```

A new scenario lands by adding one crate with an `optimizer/` (L4 rules) and a `physical/` + `emit/` (L5) — plus one line in `bin/asap-controller/main.rs`. No changes to core or runtime.

See `docs/design.md` §3 for the 5-layer model, §6 for core internals, §8 for scenario internals, and `docs/migration-plan.md` for the phase-by-phase lift.
