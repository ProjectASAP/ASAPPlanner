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

```
crates/
  core/              # IR, traits, no I/O
  runtime/           # HTTP / OpAMP / replanner / store
  scenario-lifecycle # full-stack planner (ex-DC controller)
  scenario-query     # analytics-query planner (ex-asap-planner-rs)
  scenario-fusion    # DataFusion operator fusion (ex-asap-fusion)
  control-proto      # OpAMP + internal proto
  testing            # shared fixtures
bin/
  asap-controller    # service binary
  asap-plan          # one-shot CLI
```

Core is pure types + traits, no I/O. Scenarios are self-contained plugin crates. A new scenario lands by adding one crate + one line in `bin/asap-controller/main.rs` — no changes to core or runtime.

See `docs/design.md` §3–§8 for the full picture.
