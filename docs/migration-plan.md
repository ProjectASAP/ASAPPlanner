# ASAPController — migration plan

Companion to `asap-controller-design.md`. Concrete phase-by-phase plan to get from today's three-repo mess to the target layout.

## Premise

- **Target repo**: `github.com/ProjectASAP/ASAPController` — currently empty.
- **Sources**: `DataCollector/controller/`, `ASAPQuery-backend/asap-planner-rs/` (the *newer* of the two copies), `asap-fusion/`.
- **Hard constraint**: ASAPQuery-backend must keep running throughout. `POST /api/v1/streaming-config` contract must not break. OpAMP push to agents must not break.
- **Soft constraint**: minimise the "big-bang" window. Each phase lands a working state.

## Phase-by-phase plan

Ordered by risk, lowest first. Each phase is a single PR unless noted.

---

### Phase 0 — Skeleton (ASAPController)

**Scope:** Create the empty workspace + crate skeleton; no logic yet.

**Work:**
- In `ASAPController/`: workspace `Cargo.toml` listing all 7 crates as empty.
- Create each crate with a `lib.rs` containing just `// placeholder` and a minimal `Cargo.toml`.
- Check in `README.md`, `docs/design.md` (this design), `docs/migration-plan.md` (this file).
- Copy `DataCollector/opamp.proto` → `ASAPController/proto/opamp.proto`. Wire `control-proto` to generate from it via `prost-build`. Verify `cargo build` succeeds.
- CI: basic `cargo build && cargo test && cargo clippy -- -D warnings && cargo fmt --check` on every PR.

**Risk:** ~zero. No semantics migrated yet.

**Exit criteria:** `cargo build` green on empty workspace; CI running.

**PR size:** ~200 LOC (boilerplate).

---

### Phase 1 — Core extraction: types only

**Scope:** Move the types that all three scenarios will need into `crates/core/`. Pure types + trait definitions. No algorithms.

**Work:**
- `core::workload`: lift `QuerySpec` from `DataCollector/controller/src/analyzer.rs` and `QueryWorkload` as the new sum type. Vendor the bits of `asap_types::AggregationConfig` / `QueryConfig` / `StreamingConfig` / `InferenceConfig` / `AggregationReference` that will be needed — either by re-exporting `asap_types` as a path dep OR (preferred) by publishing `asap_types` to crates.io/a git dep so ASAPController can depend on it without vendoring ASAPQuery-backend's whole workspace.
- `core::plan`: define `Plan`, `PlanNode`, `Expr` trait stubs + the `Planner`, `Optimizer`, `PlanRewriteRule`, `PlanEmitter` traits. No concrete impls.
- `core::cost`: `CostModel` trait + `Accuracy`/`Latency`/`Dollars` value types.
- `core::emit`: `PlanEmitter` trait + `EmitError`.
- `core::registry`: `ScenarioRegistry`, `Scenario` trait, `ScenarioId` enum with the 3 known variants.

**Risk:** low — no runtime behaviour yet; the traits may need to be revised as scenarios land. Plan for a second "core refactor" PR mid-migration once 2 of 3 scenarios have landed.

**Decision point:** `asap_types` — vendor or publish? **Recommendation: publish to a private git tag.** Pinning a git SHA avoids the churn of path deps across repos. Do this early so Phase 2+ can depend on a stable `asap_types` version.

**Exit criteria:** `cargo build` green; `core` compiles with zero `todo!()`s (trait stubs are fine).

**PR size:** ~800 LOC.

---

### Phase 2 — Runtime extraction: HTTP + store

**Scope:** Lift DC controller's HTTP server + in-memory stores into `crates/runtime/`. OpAMP + replanner + monitor wait till later phases (they can pull behind a `feature` flag for now).

**Work:**
- `runtime/http/` — copy `DataCollector/controller/src/main.rs`'s axum routes verbatim, split into modules by route. Keep all behaviour.
- `runtime/store/` — lift `store/PlanStore` and `store/WorkloadStore` as-is.
- `runtime/backend_client/` — lift `backend_client.rs` as-is.
- The HTTP handlers still reference DC's planner concretely at this point. That's OK; we'll abstract in Phase 4.

**Risk:** low — all code is a 1:1 copy. Tests that shipped with DC should pass as-is.

**Exit criteria:** `cargo build` green; runtime compiles against core trait stubs; DC controller's existing HTTP integration tests (if any) pass when wired to runtime.

**PR size:** ~1500 LOC (mostly moves, little rewriting).

---

### Phase 3 — `scenario-fusion`: first scenario, library-only

**Scope:** Copy `asap-fusion` into `crates/scenario-fusion/`. This is the lowest-risk scenario because it's a pure library with no HTTP/OpAMP surface — and it's the scenario most people don't depend on yet.

**Work:**
- Copy `asap-fusion/src/*` → `crates/scenario-fusion/src/`.
- `impl Scenario for FusionScenario` in `lib.rs`. For now, the scenario registers no HTTP routes; it's just available as a library.
- Port the microbenchmarks to `scenario-fusion/benches/`. Verify numbers are within noise of the pre-migration baseline.
- Update `asap-fusion`'s own README to say "moved to `ASAPController/crates/scenario-fusion`, see …"; leave a transitional shim in `asap-fusion` that re-exports the new crate (so in-flight benchmark harnesses keep compiling).

**Risk:** low — no service consumes `asap-fusion` today; only downstream is `asap-fusion`'s own benches.

**Exit criteria:** `cargo bench -p scenario-fusion` produces numbers; sketch microbenchmarks within 5% of pre-migration.

**PR size:** ~3000 LOC (asap-fusion is ~3k lines + benches).

---

### Phase 4 — `scenario-query`: migrate asap-planner-rs's newer copy

**Scope:** Move `ASAPQuery-backend/asap-planner-rs/` (the newer copy) into `crates/scenario-query/`. Delete the older copy in `ASAPQuery/`.

**Work:**
- Diff the two `asap-planner-rs` copies (ASAPQuery vs ASAPQuery-backend). Reconcile — the newer copy (backend) has the `rustls-tls` change and richer `lib.rs` re-exports; the older copy has nothing the newer lacks. **Decision: port only the newer copy; discard the older.**
- Copy `ASAPQuery-backend/asap-planner-rs/src/{lib,main}.rs` + `src/{planner,output,query_log,prometheus_client}/` → `crates/scenario-query/src/`.
- `impl Scenario for QueryScenario` — registers the YAML emitter (`StreamingConfigEmitter` + `InferenceConfigEmitter`) and an HTTP route `POST /plan/query`.
- Port the CLI: `bin/asap-plan/main.rs` gains clap flags mirroring `asap-planner-rs`'s current CLI (`--input_config`, `--query-log`, `--prometheus-url`, `--output_dir`, `--streaming_engine`, `--query-language`). Implementation is a thin shell that builds a `QueryWorkload`, hands to `QueryScenario`, writes emitter output to disk.
- **Golden-file test**: capture a corpus of today's `asap-planner-rs` inputs → outputs. The new CLI must produce byte-identical output. Put this under `bin/asap-plan/tests/golden/`.

**Risk:** medium. `asap-planner-rs` has subtle edge cases (YAML emission formatting, label ordering) that the golden-file test catches.

**Parallel work** (can land in parallel with this PR, not dependent):
- Update ASAPQuery-backend's docker-compose-precompute.yml to swap the `asap-planner-rs` init container image for an `asap-controller:latest` invocation — `asap-controller plan --workload /config/controller-config.yaml --output-dir /asap-planner-output`. Ship as a separate PR on the ASAPQuery-backend repo **after** ASAPController publishes its first binary release.

**Exit criteria:** `asap-plan` CLI produces byte-identical YAML to `asap-planner-rs` on the golden-file corpus; HTTP route `POST /plan/query` returns the same YAML over HTTP.

**PR size:** ~4000 LOC (asap-planner-rs is ~3–4k lines + new CLI shell + golden tests).

---

### Phase 5 — `scenario-lifecycle`: DC controller's planner

**Scope:** Move DC controller's planning logic into `crates/scenario-lifecycle/`. This is the largest and highest-risk scenario.

**Work:**
- Copy `DataCollector/controller/src/{planner,algebra,config,query_parser}/` → `crates/scenario-lifecycle/src/`.
- `impl Scenario for LifecycleScenario` — registers:
    - HTTP routes: `POST /plan` (full-lifecycle planner), `POST /replan` (SLA-driven).
    - Emitters: `OpAmpRemoteConfigEmitter`, `StreamingConfigEmitter` (delegates to `scenario-query`'s emitter), `AsapqueryBackendConfigEmitter`.
- Wire OpAMP server — copy `DataCollector/controller/src/opamp/` to `runtime/opamp/` (yes, runtime not scenario — OpAMP is a transport, not a plan-type).
- Wire replanner — copy `DataCollector/controller/src/{replan,monitor}.rs` to `runtime/{replan,monitor}/`.
- Update `runtime/http/` to dispatch `POST /plan` through the `ScenarioRegistry` instead of calling DC's planner directly.
- Integration test: send a `QuerySpec` over HTTP, assert the correct OTel config is pushed via OpAMP and the correct `StreamingConfig` is POSTed to a fake backend. DC controller already has tests of this shape; port them.

**Risk:** high. DC controller's planner is 60K lines (src/main.rs + planner/ + algebra/ = most of the repo). Staged plan generation, delta cost modelling, stage-split — all subtle. Port *without* touching the logic; only adjust imports + trait impls.

**Exit criteria:** DC controller's existing integration tests pass against `asap-controller` binary. SLA-driven replan fires on violation. OpAMP push to agents works byte-identically.

**PR size:** ~8000 LOC (this is the big one). Consider splitting: (a) lift algebra+planner as-is; (b) wire through registry in a separate PR.

---

### Phase 6 — Cutover: ASAPQuery-backend deletes its planner copy, DC deletes its controller dir

**Scope:** Remove duplication. This phase is small code-wise but high blast radius.

**Work:**
- **ASAPQuery-backend PR**:
    - Delete `asap-planner-rs/` directory.
    - Update workspace `Cargo.toml` to drop the member.
    - Update `asap-quickstart/docker-compose-precompute.yml` to replace the `asap-planner-rs` init container with an `asap-controller plan` invocation.
    - Update `asap-quickstart/Dockerfile.queryengine-local` if it ever references asap-planner-rs (likely it doesn't).
    - Capability-miss callback in `SimpleEngine` continues to `POST /api/v1/plan` on the controller — no code change, same endpoint.
- **ASAPQuery PR**: delete `asap-planner-rs/` (the older copy). Any downstream refs in ASAPQuery update to point at ASAPController's `asap-plan` binary.
- **DataCollector PR**: delete `controller/` directory. Anyone who ran `cargo build -p controller` now runs `cargo build -p asap-controller` against the new repo. Update DC's README to redirect.
- **asap-fusion PR**: convert the repo to a thin shim that re-exports `scenario-fusion` OR archive the repo outright if no external user depends on it. Recommendation: **archive**.

**Risk:** medium. Deletion is final but reversible via `git revert`; actual blast radius is low because Phase 5 has already proven the new binary works.

**Exit criteria:**
- `ASAPQuery-backend` workspace builds without `asap-planner-rs/`.
- Docker Compose starts successfully with the new init container.
- CI on all four repos passes.

**PR size:** ~500 LOC across 3–4 PRs (all in source repos, not ASAPController).

---

### Phase 7 — Core refactor: tighten traits now that 3 scenarios exist

**Scope:** Look at the shape of the `Planner` / `Optimizer` / `PlanRewriteRule` traits with 3 real implementations, tighten anything awkward.

**Work:**
- Look at any `todo!()` or `#[allow(unused)]` left over from Phase 1's trait stubs.
- Tighten associated types.
- Document the stable contract in `docs/design.md`.
- Publish `asap-control-core` 0.1.0 to a private registry if external users want to depend on it.

**Risk:** low — internal cleanup.

**PR size:** ~500 LOC.

---

## Timeline

| Phase | PRs | Cumulative LOC | Est. wall time (1 engineer) |
|---|---|---|---|
| 0. Skeleton | 1 | ~200 | 1 day |
| 1. Core types | 1 | ~1000 | 3 days |
| 2. Runtime HTTP+store | 1 | ~2500 | 3 days |
| 3. scenario-fusion | 1 | ~5500 | 1 week |
| 4. scenario-query + asap-plan CLI | 1 | ~9500 | 1.5 weeks |
| 5. scenario-lifecycle | 2 | ~17500 | 2 weeks |
| 6. Cutover (delete) | 3–4 | ~18000 | 1 week |
| 7. Core refactor | 1 | ~18500 | 3 days |

**Total**: ~6 weeks for one engineer. Parallelizable if two people: one on lifecycle (Phase 5, the long tail) while another does Phases 2–4.

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| YAML emission byte-differences between old and new planner | Medium | Breaks ASAPQuery-backend's config consumer | Golden-file test corpus in Phase 4; fuzz harness over the YAML schema |
| OpAMP protocol detail drifts during port | Low | Breaks agent config push | Port opamp/ verbatim in Phase 2's runtime crate; don't touch until it's proven to work |
| DataFusion version skew in scenario-fusion | Low | Benchmarks drift | Pin DataFusion version in workspace `Cargo.toml`; measure benches before + after |
| Two asap-planner-rs copies have subtle diff we miss | Medium | Behaviour regression | Explicit diff + reconcile step in Phase 4; one authoritative copy (the backend's newer one) |
| ASAPQuery-backend deploys break during cutover | Low | Production outage | Phase 6 only after Phase 5 fully validates; staged rollout of docker-compose change |
| Scope creep — "while we're at it, redesign the cost model" | High | Doubles timeline | Explicit non-goal list in design doc §2; defer all rewrites to post-migration phases |
| Core traits are wrong; need to rev after scenarios land | Medium | Phase 7 PR larger than expected | Accept; Phase 7 is planned for exactly this. Don't over-design Phase 1. |

## Decision log (during migration)

These are the questions that will come up mid-migration. Pre-decide as many as possible:

1. **Which version of `asap_types`?** ASAPQuery-backend's current one, pinned to a git SHA.
2. **Which `promql-parser` + `sqlparser` versions?** DC controller's (newer). ASAPQuery-backend's planner uses older versions; port it to the newer ones during Phase 4.
3. **How is `scenario-lifecycle`'s `StreamingConfig` YAML emitter related to `scenario-query`'s?** One emitter in `scenario-query`, called from `scenario-lifecycle`. Asymmetric dependency.
4. **Does `asap-controller` have feature flags to disable scenarios at build time?** Yes, one `--features lifecycle,query,fusion` with all three enabled by default. A minimal CLI binary (`asap-plan`) disables lifecycle and fusion.
5. **Who owns the Cargo.lock?** ASAPController. Downstream repos (ASAPQuery-backend, DataCollector) do NOT depend on ASAPController as a Cargo path dep — they pull the published binary via Docker or pin a git SHA.
6. **Does `ControllerClient` on the ASAPQuery-backend side need changes?** No. It keeps POSTing to `/api/v1/plan`; the controller's routing layer dispatches to `scenario-lifecycle` (same as DC controller does today).

## What a new scenario looks like post-migration

This is the test of the architecture. To add a 4th scenario (e.g. "edge-caching"):

1. `cargo new --lib crates/scenario-edge-cache`
2. Add `asap-control-core = { path = "../core" }` to the new crate's `Cargo.toml`.
3. Implement:
    - `struct EdgeCacheScenario; impl Scenario for EdgeCacheScenario { … }`
    - `struct EdgeCachePlanner; impl Planner for EdgeCachePlanner { … }` (type Input = QueryWorkload, type Output = Plan)
    - `struct EdgeConfigEmitter; impl PlanEmitter for EdgeConfigEmitter { type PlanInput = Plan; type Output = EdgeAgentConfig; … }`
4. Add 1 line to `bin/asap-controller/main.rs`:
   ```rust
   registry.register::<EdgeCacheScenario>();
   ```
5. Add 1 line to `ScenarioId` enum in core (optional, for explicit routing).
6. Ship.

**No** changes to runtime, core, or other scenarios. That's the extension-point test passing.

## Rollback strategy

Each phase is a `git revert`-able PR. The three source repos (`DataCollector`, `ASAPQuery`, `ASAPQuery-backend`, `asap-fusion`) are NOT modified until Phase 6. If Phase 5 reveals the design is wrong, we can revert 0–4 and start over without disrupting any live service.

Phase 6 is the only one-way door. Before landing it:
- At least 1 week of the new `asap-controller` binary running in staging.
- Golden-file corpus at 100% match rate for 1 week.
- SLA replan observed firing correctly at least once in staging.
