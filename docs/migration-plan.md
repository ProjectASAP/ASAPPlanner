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

### Phase 1 — Core extraction: layers 1–3 + traits

**Scope:** Lift the DC controller's **layers 1–3** (query parsing + per-language algebra + sketch-algebra IR) into `crates/core/` verbatim. Add trait seams for the L4/L5 plugin points scenarios will implement later. This is real code — parsers, lowering passes, `QueryExpr`/`AggIntent` — not just trait stubs.

Per design §3, these three layers are "query-language-independent and workload-independent" (per DC's own `query-to-sketch-translation.md`), so they belong in core, not in a scenario.

**Work:**
- **`core::query_language`** (L1): lift `DataCollector/controller/src/query_parser/{promql,sql,mod}.rs` into `core/src/query_language/`. One module per language. Keep wrappers over `promql-parser` + `sqlparser` as-is.
- **`core::logical_plan`** (L2): lift the per-language algebra tree definitions from DC's lowering code into `core/src/logical_plan/`. One `enum LogicalPlan` per language.
- **`core::sketch_algebra`** (L3): lift `DataCollector/controller/src/algebra/{expr,directory}.rs` (the `QueryExpr` + `AggIntent` IR + candidate sketch type directory) into `core/src/sketch_algebra/`.
- **`core::lower`**: lift `DataCollector/controller/src/algebra/lower.rs` (L1→L2→L3 passes) into `core/src/lower/`. One `lower_<lang>` entry point per language.
- **`core::pipeline`**: the L1→…→L5 driver. Parameterised on `Scenario` trait. Small new code (~200 LOC).
- **`core::workload`**: lift `QuerySpec` from `DataCollector/controller/src/analyzer.rs` + add `QueryWorkload` sum type. Pull in `asap_types::{AggregationConfig, QueryConfig, StreamingConfig, InferenceConfig, AggregationReference}` as a git-pinned dep (see Decision Point).
- **`core::plan`**: define the trait seams — `Optimizer`, `OptimizerRule`, `PhysicalPlanner`, `PlanEmitter`, `Scenario`, `DeploymentConstraints`. Traits only; concrete impls live in scenarios.
- **`core::cost`**: `CostModel` trait + `Accuracy`/`Latency`/`Dollars` value types. Concrete models stay in scenario crates.
- **`core::registry`**: `ScenarioRegistry`, `ScenarioId` enum with the 3 known variants.

**Testing:**
- Port DC's existing `query_parser/` + `algebra/` unit tests verbatim. They must all pass after the move.
- Add a golden-file round-trip test: `parse_promql("sum by (host)(rate(requests[5m]))") → lower → QueryExpr` produces a stable JSON representation. This pins L1-3 behaviour so Phase 4/5 refactors don't drift it.

**Risk:** medium. L1-3 are real code with real behaviour, not trait stubs. The DC controller's existing tests are the safety net — they must pass unchanged after the lift.

**Decision points:**
1. `asap_types` — vendor or publish? **Recommendation: publish to a private git tag.** Pinning a git SHA avoids path-dep churn across repos. Do this in Phase 0 so Phase 1+ can depend on a stable version.
2. `promql-parser` / `sqlparser` versions — DC uses newer (0.8 / 0.61) than asap-planner-rs (0.5 / 0.59). **Adopt DC's newer versions in core.** asap-planner-rs's query patterns will be re-verified against them in Phase 4.

**Exit criteria:** `cargo build` green; DC's parser + algebra unit tests all pass from `core/`; golden-file round-trip test locked in.

**PR size:** ~4000 LOC (L1-3 is the substantial half of DC controller by line count).

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

**Scope:** Move `ASAPQuery-backend/asap-planner-rs/` (the newer copy) into `crates/scenario-query/`, **and refactor it to fit the 5-layer model**: reverse-engineer PromQL pattern templates into an L2 tree, and split L3 intent from L4 sketch binding. Delete the older copy in `ASAPQuery/`.

This phase is larger than a straight lift because two structural conformance changes are bundled in.

**Work — the lift:**
- Diff the two `asap-planner-rs` copies. Newer (backend) copy wins. **Port only the newer; discard the older.**
- Copy `ASAPQuery-backend/asap-planner-rs/src/{lib,main}.rs` + `src/{planner,output,query_log,prometheus_client}/` → `crates/scenario-query/src/`.
- `impl Scenario for QueryScenario` — registers YAML emitters (`StreamingConfigEmitter` + `InferenceConfigEmitter`) and HTTP route `POST /plan/query`.
- Port the CLI: `bin/asap-plan/main.rs` with clap flags mirroring `asap-planner-rs`'s current CLI.

**Work — L2 tree conformance (NEW):**
- Define `PromqlLogicalPlan` in `core::logical_plan::promql` that expresses the five pattern shapes planner currently template-matches (`OnlyTemporal`×2, `OnlySpatial`, `OneTemporalOneSpatial`×2) as first-class L2 tree nodes (`Aggregate` / `Window` / `Filter` / `Sort` / `Limit`).
- Write L1→L2 lowering in `core::lower::promql` that takes a `promql_parser::parser::Expr` and produces a `PromqlLogicalPlan`. The five existing pattern shapes become five recognised L2 tree shapes plus a generic fall-through.
- Retire planner's `PromQLPattern` / `PromQLPatternBuilder` / `QueryPatternType` — replaced by tree-shape inspection at L3 lowering.
- Do the same for SQL (retire `SQLPatternMatcher`, produce `SqlLogicalPlan`).
- **This is the conformance cost the user explicitly accepted for architectural uniformity.** Budget accordingly.

**Work — L3 / L4 split (NEW):**
- In `core::lower::promql_to_sketch_algebra`: `PromqlLogicalPlan → QueryExpr` producing **intent-only** L3 (no sketch names).
- `Statistic` enum → `AggIntent` subset (9 of DC's 25 variants). No sketch type, no sketch params at L3.
- In `scenario-query/src/optimizer/sketch_binding.rs`: new L4 rule that takes `AggIntent + DeploymentConstraints` and produces `AggregationType + SketchParams`. This absorbs the work `map_statistic_to_precompute_operator` does today.
- Update `build_agg_configs_for_statistics` to call the L4 rule instead of doing the binding inline.
- `IntermediateAggConfig` loses its inline sketch binding; it's now an L4 output type.

**Testing:**
- **Golden-file test**: capture a corpus of today's `asap-planner-rs` inputs → outputs. The new CLI must produce byte-identical output. Put under `bin/asap-plan/tests/golden/`. This is the non-negotiable safety net for the refactor.
- **L2 round-trip test**: `parse → build PromqlLogicalPlan → pretty-print back → parse` stays stable across tree rewrites.
- **L3 intent-only test**: after L3 lowering, assert `AggIntent` carries no sketch-type information (type-system-enforced, not runtime-enforced — `AggIntent::Quantile` carries `AccuracyTarget`, not `SketchType`).

**Risk:** medium-high. Two refactors on top of a lift. Golden-file corpus is the gate.

**Parallel work** (separate PR, not dependent):
- Update ASAPQuery-backend's `docker-compose-precompute.yml` to swap the `asap-planner-rs` init container for `asap-controller plan --workload /config/controller-config.yaml --output-dir /asap-planner-output`. Ship **after** ASAPController publishes its first binary release.

**Exit criteria:**
- `asap-plan` CLI produces byte-identical YAML to `asap-planner-rs` on the golden-file corpus.
- HTTP `POST /plan/query` returns the same YAML over HTTP.
- `AggIntent` post-L3 contains zero `AggregationType` / sketch params (type-enforced).
- Planner's `PromQLPattern*` / `SQLPatternMatcher` files are deleted.

**PR size:** ~6000 LOC (the straight lift is ~4000; L2-tree work + L3/L4 split adds ~2000).

---

### Phase 5 — `scenario-lifecycle`: DC controller's L4 + L5

**Scope:** Move DC controller's **L4 optimizer + L5 physical plan + emitters** into `crates/scenario-lifecycle/`. Note L1-3 are already in core from Phase 1 — only L4/L5 remain. This is still the largest scenario because DC's cost-model code (planner/) is big.

**Work:**
- **L4**: copy `DataCollector/controller/src/algebra/optimizer.rs` + its rule modules into `crates/scenario-lifecycle/src/optimizer/`. Each rule implements `core::plan::OptimizerRule`.
- **L5 physical**: copy `DataCollector/controller/src/algebra/{physical,allocator,plan}.rs` into `crates/scenario-lifecycle/src/physical/`. `impl core::plan::PhysicalPlanner`.
- **Cost models**: copy `DataCollector/controller/src/planner/{delta_cost_model,online_cost_model,pareto,tco,rules,baseline,cost_model}.rs` into `crates/scenario-lifecycle/src/cost/`. Each implements `core::cost::CostModel`.
- **Stage-split**: `DataCollector/controller/src/planner/stage_split.rs` → `crates/scenario-lifecycle/src/physical/stage_split.rs` (scenario-specific — this is what makes lifecycle different from query).
- **Emitters**: `DataCollector/controller/src/config/generate_{agent,backend}_config*.rs` → `crates/scenario-lifecycle/src/emit/`. `OpAmpRemoteConfigEmitter` + `AsapqueryBackendConfigEmitter` (the latter calls `scenario-query`'s YAML emitter).
- `impl Scenario for LifecycleScenario` — registers HTTP routes: `POST /plan` (full-lifecycle planner), `POST /replan` (SLA-driven).
- **Wire OpAMP server**: copy `DataCollector/controller/src/opamp/` → `runtime/opamp/`. OpAMP is a transport, not a plan-type, so it lives in runtime, not in the scenario crate.
- **Wire replanner**: copy `DataCollector/controller/src/{replan,monitor}.rs` → `runtime/{replan,monitor}/`.
- Update `runtime/http/` to dispatch `POST /plan` through `ScenarioRegistry` instead of calling DC's planner directly.
- **Integration test**: send a `QuerySpec` over HTTP → L1-3 in core → L4 rules fire → L5 produces OTel YAML + backend `StreamingConfig` → fake backend asserts the POST body matches golden file. DC already has this test shape; port it.

**Risk:** high. DC's cost-model + stage-split code is ~40KB of subtle logic. Port **without** touching the logic; only adjust imports + trait impls. Any refactoring is out of scope for Phase 5 — file it against a follow-up.

**Exit criteria:** DC controller's existing integration tests pass against `asap-controller` binary. SLA-driven replan fires on violation. OpAMP push to agents works byte-identically.

**PR size:** ~5000 LOC (smaller than the original plan because L1-3 already moved in Phase 1). Consider splitting: (a) lift optimizer+physical+cost as-is behind trait impls; (b) wire through registry + runtime in a separate PR.

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
| 1. Core — L1-3 + traits | 1 | ~4200 | 1 week |
| 2. Runtime HTTP+store | 1 | ~5700 | 3 days |
| 3. scenario-fusion (L4+L5 over DF) | 1 | ~8700 | 1 week |
| 4. scenario-query (L2 tree + L3/L4 split + YAML + CLI) | 1 | ~14700 | 2.5 weeks |
| 5. scenario-lifecycle (L4+L5 + OpAMP + replanner) | 2 | ~19700 | 1.5 weeks |
| 6. Cutover (delete in source repos) | 3–4 | ~20200 | 1 week |
| 7. Core refactor | 1 | ~20700 | 3 days |

**Total**: ~7 weeks for one engineer. Phase 4 grew by a week — it now bundles the asap-planner-rs lift + two structural refactors (L2 tree construction, L3/L4 sketch-binding split). The alternative — making L2 optional to skip the planner refactor — was explicitly rejected in favour of architectural uniformity (see design doc §12 Q8). Parallelizable if two people: lifecycle (Phase 5) can start as soon as Phase 4's L2 tree lands; they share no files after that.

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
| PromQL L2 tree reverse-engineering loses a pattern-match case planner relied on | Medium | Generated YAML diverges from legacy | Golden-file corpus covers all 5 pattern shapes + generic fallthrough; tree-shape inspection at L3 lowering is a direct 1:1 translation of today's pattern-match logic, not a reinterpretation |
| L3/L4 split misses a case in `map_statistic_to_precompute_operator` | Medium | Wrong sketch selected for some `(Statistic, Treatment)` combos | Every branch of the current function must round-trip via the new L3→L4 path on the golden-file corpus; add table-driven unit tests enumerating every `Statistic × Treatment` combination |
| `AggIntent` as a 25-variant superset grows unbounded as scenarios add needs | Low | Core churn | Variants added only for intent shapes used by ≥1 shipped scenario; never speculative. |

## Decision log (during migration)

These are the questions that will come up mid-migration. Pre-decide as many as possible:

1. **Which version of `asap_types`?** ASAPQuery-backend's current one, pinned to a git SHA.
2. **Which `promql-parser` + `sqlparser` versions?** DC controller's (newer). ASAPQuery-backend's planner uses older versions; port it to the newer ones during Phase 4.
3. **How is `scenario-lifecycle`'s `StreamingConfig` YAML emitter related to `scenario-query`'s?** One emitter in `scenario-query`, called from `scenario-lifecycle`. Asymmetric dependency.
4. **Does `asap-controller` have feature flags to disable scenarios at build time?** Yes, one `--features lifecycle,query,fusion` with all three enabled by default. A minimal CLI binary (`asap-plan`) disables lifecycle and fusion.
5. **Who owns the Cargo.lock?** ASAPController. Downstream repos (ASAPQuery-backend, DataCollector) do NOT depend on ASAPController as a Cargo path dep — they pull the published binary via Docker or pin a git SHA.
6. **Does `ControllerClient` on the ASAPQuery-backend side need changes?** No. It keeps POSTing to `/api/v1/plan`; the controller's routing layer dispatches to `scenario-lifecycle` (same as DC controller does today).
7. **L2 tree for asap-planner-rs — mandatory or optional?** Mandatory. planner's current template-catalogue approach is replaced with a proper `PromqlLogicalPlan` tree in Phase 4. The extra conformance work is taken to keep the 5-layer model uniform. See design doc §12 Q8 for the escape hatch if a future scenario genuinely can't fit a tree.
8. **Sketch binding — L3 or L4?** L4. DC and fusion already do this correctly; planner's `map_statistic_to_precompute_operator` conflates L3+L4 and is split during Phase 4. `AggIntent` at L3 carries intent + accuracy only; concrete `AggregationType` / `SketchParams` are produced by an L4 rule.
9. **Intent vocabulary when scenarios disagree in width?** Core's `AggIntent` is the superset (DC's ~25 variants). Each scenario only uses / produces / accepts the subset it needs (planner 9, fusion 3). Adding a new intent variant is a core change that scenarios opt into.

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
