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

### Phase 1 — Core extraction: all-5-layer shared infrastructure

**Scope:** Lift the DC controller's **layers 1–3** (query parsing + per-language algebra + sketch-algebra IR) into `crates/core/` verbatim, AND lift L4 and L5 **shared infrastructure**: rule engine, shared rule library, cost-model traits, `PhysicalPlanner` trait, stage allocator framework, sketch catalogue. Deployment models in later phases pick from this common base and only add deployment-model-specific pieces.

Per design §3, these five layers' infrastructure is query-language-independent AND deployment-model-independent; the concrete rule set, topology, and emitter are deployment-model-specific.

**Work — L1-3 (straight lift from DC):**
- **`core::query_language`** (L1): lift `DataCollector/controller/src/query_parser/{promql,sql,mod}.rs` → `core/src/query_language/`. One module per language.
- **`core::logical_plan`** (L2): lift per-language algebra tree definitions → `core/src/logical_plan/`. One `enum LogicalPlan` per language. Add `core::logical_plan::datafusion` as a thin re-export of `datafusion::logical_expr::LogicalPlan` for fusion's L2.
- **`core::sketch_algebra`** (L3): lift `algebra/{expr,directory}.rs` → `core/src/sketch_algebra/`. Enforce **intent-only** — `AggIntent` carries accuracy target, never sketch type. **Generalize `QueryExpr::Scan` over data model**: introduce a `Source` sum (`TimeSeries` / `Table` / `Join` variants) inside `Scan`, and a `DataModel` enum (`TimeSeries` / `Tabular` / `Any`). DC's current scan logic becomes `Source::TimeSeries`; `Source::Table` is new (stub impl fine for Phase 1 — deployment-model-asapfusion fills it in Phase 3). Add `AggIntent::requires() -> DataModel` so L4 rules can skip non-applicable intents. See design §3 "Data-model support" and §6 `core::sketch_algebra`. ~300 LOC on top of the DC lift.
- **`core::lower`**: lift `algebra/lower.rs` (L1→L2→L3 passes) → `core/src/lower/`. One `lower_<lang>` entry point per language. Each lowering produces the appropriate `Source` variant (PromQL / SQL → `Source::TimeSeries`; DataFusion → `Source::Table`).

**Work — L4 framework (NEW):**
- **`core::optimizer::engine`**: rule driver — fixed-point iteration, cycle detection, priority ordering. Generic over `OptimizerRule`.
- **`core::optimizer::rule` trait**: `OptimizerRule` + `RuleCategory` enum (`PushDown | Fusion | Elim | Bind | StageRouting`). Priority u16. `apply(&expr, &constraints) -> Option<QueryExpr>`.
- **`core::optimizer::rules`**: shared rule library. Lift the **deployment-model-agnostic rules** from DC's `algebra/optimizer.rs`:
    - `BindKllOnQuantile` (intent → KLL with params from accuracy)
    - `BindCmsOnCount` (+ `BindCmsOnSum`)
    - `BindHllOnCardinality`
    - `BindDdsketchOnQuantile` (alternative to KLL)
    - `FusionPassthrough` (Aggregate over Filter → push Filter under)
    - `ElimNoopFilter`, `ElimNoopSort`, `ElimNoopLimit`
    - DeploymentModel-specific rules (stage-aware push-down, TCO-aware deferral) STAY in `deployment-model-asaplifecycle` — not lifted.
- **`core::optimizer::cost`**: `CostModel` trait + `Accuracy` / `Latency` / `Dollars` value types. Lift DC's **generic** cost functions (memory budget, accuracy degradation curve). DeploymentModel-specific cost models (delta, online, pareto, tco) STAY in `deployment-model-asaplifecycle`.
- **`core::plan`**: `DeploymentConstraints` trait — memory budgets, network topology descriptor, available sketch backends.

**Work — L5 framework (NEW):**
- **`core::physical::planner_trait`**: `PhysicalPlanner` trait parameterized on `Topology` + `Output`.
- **`core::physical::topology`**: `TopologyDescriptor` trait + pre-baked impls — `ThreeStage` (edge/gateway/backend), `SingleStage` (backend-only), `ZeroStage` (in-process).
- **`core::physical::stage_allocator`**: generic topology-driven allocator. Lift DC's `SketchAllocator` from `algebra/allocator.rs`, parameterize the hardcoded 3-stage assumption over `TopologyDescriptor`.
- **`core::physical::sketch_catalog`**: candidate sketch types + parameter constraints. Lift from DC's `algebra/directory.rs`.

**Work — other core pieces:**
- **`core::pipeline`**: the L1→…→L5 driver. ~300 LOC.
- **`core::workload`**: lift `QuerySpec` + add `QueryWorkload` sum type.
- **`core::emit`**: `PlanEmitter` trait + `EmitError`.
- **`core::registry`**: `DeploymentModelRegistry`, `DeploymentModelId` enum.

**Testing:**
- Port DC's existing `query_parser/` + `algebra/` unit tests verbatim. All pass after the move.
- Golden-file round-trip test for L1→L2→L3 (parse → lower → JSON of `QueryExpr`).
- Golden-file test for L4 shared rule library: each rule has a "before → after" `QueryExpr` fixture demonstrating its behavior.
- Unit tests for `StageAllocator` under each pre-baked topology.

**Risk:** medium-high. This phase does more than a straight lift — it generalizes DC's hardcoded `SketchAllocator` over topology + lifts a subset of optimizer rules into a shared library. Keep changes mechanical; any semantic revision is deferred to Phase 5/7.

**Decision points:**
1. `asap_types` — publish to a private git tag in Phase 0.
2. `promql-parser` / `sqlparser` versions — adopt DC's newer versions.
3. **Which DC rules go into `core::optimizer::rules`?** Rule of thumb: if it depends only on `QueryExpr` + `DeploymentConstraints` (no DC-specific types), lift. If it reaches into DC's stage graph, backend_client, or OTel YAML shape, it stays in `deployment-model-asaplifecycle`. Default: lift `Bind*`, `Elim*`, `FusionPassthrough`; keep `StageAwarePushDown`, `TransmissionCostRewrite` in deployment-model-asaplifecycle.

**Exit criteria:** `cargo build` green; DC's parser + algebra + subset of optimizer unit tests pass from `core/`; `StageAllocator` unit-tested against all pre-baked topologies; golden-file corpus locked in.

**PR size:** ~6300 LOC (L1-3 lift ~4000 + L4/L5 framework ~2000 + `Source` sum / `DataModel` generalization ~300). Could split into (a) L1-3 lift + traits + `Source` sum, (b) L4/L5 framework — prefer this if the PR review would otherwise be unwieldy.

---

### Phase 2 — Runtime extraction: HTTP + store

**Scope:** Lift DC controller's HTTP server + in-memory stores into `crates/runtime/`. OpAMP + replanner + monitor wait till later phases (they can pull behind a `feature` flag for now).

**Work:**
- `runtime/http/` — copy `DataCollector/controller/src/main.rs`'s axum routes verbatim, split into modules by route. Keep all behavior.
- `runtime/store/` — lift `store/PlanStore` and `store/WorkloadStore` as-is.
- `runtime/backend_client/` — lift `backend_client.rs` as-is.
- The HTTP handlers still reference DC's planner concretely at this point. That's OK; we'll abstract in Phase 4.

**Risk:** low — all code is a 1:1 copy. Tests that shipped with DC should pass as-is.

**Exit criteria:** `cargo build` green; runtime compiles against core trait stubs; DC controller's existing HTTP integration tests (if any) pass when wired to runtime.

**PR size:** ~1500 LOC (mostly moves, little rewriting).

---

### Phase 3 — `deployment-model-asapfusion`: first thin deployment model

**Scope:** Land `deployment-model-asapfusion` as a thin deployment model that picks rules from core's shared library. Lowest-risk because fusion's existing `SketchConfigRule` maps directly onto core's `BindKllOnQuantile` + `BindCmsOnCount` + `BindHllOnCardinality` that Phase 1 already lifted.

**Decision — where does `deployment-model-asapfusion` live?** Default: **in the `asap-fusion` repo**, as a crate that depends on `asap-control-core` + `asap-control-optimizer`. asap-fusion is a research project with independent release cadence; keeping it in-tree in its own repo preserves that. If the team prefers lockstep, the alternative is `crates/deployment-model-asapfusion/` in ASAPController workspace — architecture is identical either way (see design §8).

This plan assumes the in-repo-at-asap-fusion placement. If in-workspace is chosen, swap paths accordingly.

**Work:**
- In `asap-fusion/`: add new crate `deployment-model-asapfusion/` that depends on `asap-control-core`. Move the translator + optimizer + executor here.
- **Fill in `Source::Table` lowering**: Phase 1 stubbed this; now implement the L2 (DataFusion `LogicalPlan`) → L3 (`QueryExpr` with `Source::Table`) lowering pass in `core::lower::datafusion` — or, if fusion prefers, keep the lowering inside the deployment model crate and use the `Source::Table` type directly. Either way, Phase 1's stub becomes concrete.
- `impl DeploymentModel for ASAPFusionDeploymentModel`:
    - `rules()` picks `BindKllOnQuantile`, `BindCmsOnCount`, `BindHllOnCardinality` from `core::optimizer::rules::*` (drops asap-fusion's `SketchConfigRule` in favor of core's rules).
    - Adds deployment-model-specific `HashModeRule` (stays local — depends on DataFusion-specific types).
    - `topology()` returns `core::physical::topology::ZeroStage`.
    - `emitter()` returns the DataFusion `LogicalPlan` rewriter.
- Port the microbenchmarks to `deployment-model-asapfusion/benches/`. Verify numbers within 5% of pre-migration baseline.
- Legacy `asap-fusion` crate becomes a thin shim that re-exports the new crate, so in-flight benchmark harnesses keep compiling.

**Risk:** low — no service consumes asap-fusion today; only downstream is its own benches. The rule-lift cleanup (`SketchConfigRule` → core rules) is the main change; golden-file test over the 3 existing rewrites catches any divergence.

**Exit criteria:** `cargo bench -p deployment-model-asapfusion` produces numbers within 5% of pre-migration; 3 rewrites documented in `asap-fusion/TESTS.md` produce identical output.

**PR size:** ~1800 LOC (asap-fusion's ~3k lines minus what folds into core's shared rule library).

---

### Phase 4 — `deployment-model-asapquery`: migrate asap-planner-rs's newer copy

**Scope:** Move `ASAPQuery-backend/asap-planner-rs/` (the newer copy) into `crates/deployment-model-asapquery/`, **and refactor it to fit the 5-layer model**: reverse-engineer PromQL pattern templates into an L2 tree, and split L3 intent from L4 sketch binding. Delete the older copy in `ASAPQuery/`.

This phase is larger than a straight lift because two structural conformance changes are bundled in.

**Work — the lift:**
- Diff the two `asap-planner-rs` copies. Newer (backend) copy wins. **Port only the newer; discard the older.**
- Copy `ASAPQuery-backend/asap-planner-rs/src/{lib,main}.rs` + `src/{planner,output,query_log,prometheus_client}/` → `crates/deployment-model-asapquery/src/`.
- `impl DeploymentModel for ASAPQueryDeploymentModel` — registers YAML emitters (`StreamingConfigEmitter` + `InferenceConfigEmitter`) and HTTP route `POST /plan/query`.
- Port the CLI: `bin/asap-query/main.rs` with clap flags mirroring `asap-planner-rs`'s current CLI.

**Work — L2 tree conformance (NEW):**
- Define `PromqlLogicalPlan` in `core::logical_plan::promql` that expresses the five pattern shapes planner currently template-matches (`OnlyTemporal`×2, `OnlySpatial`, `OneTemporalOneSpatial`×2) as first-class L2 tree nodes (`Aggregate` / `Window` / `Filter` / `Sort` / `Limit`).
- Write L1→L2 lowering in `core::lower::promql` that takes a `promql_parser::parser::Expr` and produces a `PromqlLogicalPlan`. The five existing pattern shapes become five recognized L2 tree shapes plus a generic fall-through.
- Retire planner's `PromQLPattern` / `PromQLPatternBuilder` / `QueryPatternType` — replaced by tree-shape inspection at L3 lowering.
- Do the same for SQL (retire `SQLPatternMatcher`, produce `SqlLogicalPlan`).
- **This is the conformance cost the user explicitly accepted for architectural uniformity.** Budget accordingly.

**Work — L3 / L4 split (NEW):**
- In `core::lower::promql_to_sketch_algebra`: `PromqlLogicalPlan → QueryExpr` producing **intent-only** L3 (no sketch names).
- `Statistic` enum → `AggIntent` subset (9 of DC's 25 variants). No sketch type, no sketch params at L3.
- In `deployment-model-asapquery/src/rules.rs`: picks core's `BindKllOnQuantile`, `BindCmsOnCount`, etc. from Phase 1's shared rule library + adds a deployment-model-specific `PrecomputeEngineBindRule` (handles the precompute-engine-specific flavor: which sketches are available in ASAPQuery-backend's accumulator set, what params match the engine's config schema, `DeltaSetAggregator` auto-injection before CMS/HydraKLL).
- Update `build_agg_configs_for_statistics` to call `core::optimizer::engine::RuleEngine::run()` with the deployment model's rule set instead of the inline `map_statistic_to_precompute_operator` fusion.
- `IntermediateAggConfig` loses its inline sketch binding; it's now an L4 output type produced by `PrecomputeEngineBindRule`.

**Work — topology + emitter:**
- `deployment-model-asapquery/src/topology.rs`: declare `core::physical::topology::SingleStage` (backend-only).
- `deployment-model-asapquery/src/emit/`: `StreamingConfigEmitter` + `InferenceConfigEmitter`. Lift from asap-planner-rs's `output/generator.rs`. These are the authoritative YAML emitters (deployment-model-asaplifecycle calls them).

**Testing:**
- **Golden-file test**: capture a corpus of today's `asap-planner-rs` inputs → outputs. The new CLI must produce byte-identical output. Put under `bin/asap-query/tests/golden/`. This is the non-negotiable safety net for the refactor.
- **L2 round-trip test**: `parse → build PromqlLogicalPlan → pretty-print back → parse` stays stable across tree rewrites.
- **L3 intent-only test**: after L3 lowering, assert `AggIntent` carries no sketch-type information (type-system-enforced, not runtime-enforced — `AggIntent::Quantile` carries `AccuracyTarget`, not `SketchType`).

**Risk:** medium-high. Two refactors on top of a lift. Golden-file corpus is the gate.

**Parallel work** (separate PR, not dependent):
- Update ASAPQuery-backend's `docker-compose-precompute.yml` to swap the `asap-planner-rs` init container for `asap-controller plan --workload /config/controller-config.yaml --output-dir /asap-planner-output`. Ship **after** ASAPController publishes its first binary release.

**Exit criteria:**
- `asap-query` CLI produces byte-identical YAML to `asap-planner-rs` on the golden-file corpus.
- HTTP `POST /plan/query` returns the same YAML over HTTP.
- `AggIntent` post-L3 contains zero `AggregationType` / sketch params (type-enforced).
- Planner's `PromQLPattern*` / `SQLPatternMatcher` files are deleted.

**PR size:** ~4500 LOC (straight lift ~2500 after L4 rules move to core's shared library; L2-tree work + L3/L4 split adds ~2000).

---

### Phase 5 — `deployment-model-asaplifecycle`: DC's remaining deployment-model-specific pieces

**Scope:** Land `deployment-model-asaplifecycle` as a thin deployment model. L1-3 + L4 shared rules + L5 framework already moved in Phase 1; only DC's **deployment-model-specific** L4/L5 pieces remain: stage-aware rewrite rules, cost models, the 3-stage `ThreeStage` topology's DC-specific constraints, OpAMP + backend emitters, SLA replanner wiring.

**Work — deployment model crate:**
- **Rule selection**: `deployment-model-asaplifecycle/src/rules.rs` picks from `core::optimizer::rules::*` (Bind*, Fusion*, Elim*) + adds DC-specific rules:
    - `StageAwarePushDown` — pushes ops toward edge stage when data size + edge memory budget permit
    - `TransmissionCostRewrite` — uses the TCO model to defer aggregation when it saves bandwidth
    - `DeltaRewrite` — replaces raw sends with delta-encoded sends when the online cost model says it pays off
  Each impls `core::optimizer::rule::OptimizerRule`.
- **Cost models**: `deployment-model-asaplifecycle/src/cost.rs` — lift `DataCollector/controller/src/planner/{delta_cost_model,online_cost_model,pareto,tco,rules,baseline,cost_model}.rs`. Each impls `core::optimizer::cost::CostModel`. These stay deployment-model-specific because they encode DC's deployment assumptions (edge memory, WAN bandwidth, multi-tenant).
- **Topology**: `deployment-model-asaplifecycle/src/topology.rs` — uses `core::physical::topology::ThreeStage`; only deployment-model-specific bit is declaring which stage roles map to which OpAMP `AgentRole` enum values.
- **Emitters**: `deployment-model-asaplifecycle/src/emit/` — `OpAmpRemoteConfigEmitter` (per-role OTel YAML, lifted from `DataCollector/controller/src/config/generate_{agent,backend}_config*.rs`) + `AsapqueryBackendConfigEmitter` (calls `deployment-model-asapquery::StreamingConfigEmitter` for the YAML bytes, then POSTs via `runtime::backend_client`).
- `impl DeploymentModel for ASAPLifecycleDeploymentModel` — registers HTTP routes `POST /plan` and `POST /replan`.

**Work — runtime wiring:**
- **OpAMP server**: move `DataCollector/controller/src/opamp/` → `runtime/opamp/`. OpAMP is a transport, not a plan-type, so it lives in runtime.
- **Replanner + monitor**: move `DataCollector/controller/src/{replan,monitor}.rs` → `runtime/{replan,monitor}/`.
- **Dispatch**: `runtime/http/` routes `POST /plan` through `DeploymentModelRegistry` instead of calling DC's planner directly.

**Integration test:**
- Send a `QuerySpec` over HTTP → core runs L1-3 → `ASAPLifecycleDeploymentModel`'s rule set fires (core rules + DC-specific rules) → `StageAllocator` with `ThreeStage` topology produces staged plan → emitters produce OTel YAML + `StreamingConfig` → fake backend asserts POST body matches golden file. Port DC's existing test of this shape.

**Risk:** medium. Cost-model code (`delta/online/pareto/tco`) is ~40KB of subtle logic; port mechanically — do not touch the logic. The generic pieces already in core (stage allocator, bind rules) have been unit-tested in Phase 1, reducing this phase's surface area meaningfully.

**Exit criteria:** DC controller's existing integration tests pass against `asap-controller` binary. SLA-driven replan fires on violation. OpAMP push to agents + backend POST both byte-identical to DC today.

**PR size:** ~3500 LOC (down from the previous 5000 estimate, because Phase 1 now lifts L4's shared rule library + L5's `StageAllocator` framework). Cost models are ~1800 LOC of that 3500; rules + topology + emitters + integration tests make up the rest.

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
- **ASAPQuery PR**: delete `asap-planner-rs/` (the older copy). Any downstream refs in ASAPQuery update to point at ASAPController's `asap-query` binary.
- **DataCollector PR**: delete `controller/` directory. Anyone who ran `cargo build -p controller` now runs `cargo build -p asap-controller` against the new repo. Update DC's README to redirect.
- **asap-fusion PR**: convert the repo to a thin shim that re-exports `deployment-model-asapfusion` OR archive the repo outright if no external user depends on it. Recommendation: **archive**.

**Risk:** medium. Deletion is final but reversible via `git revert`; actual blast radius is low because Phase 5 has already proven the new binary works.

**Exit criteria:**
- `ASAPQuery-backend` workspace builds without `asap-planner-rs/`.
- Docker Compose starts successfully with the new init container.
- CI on all four repos passes.

**PR size:** ~500 LOC across 3–4 PRs (all in source repos, not ASAPController).

---

### Phase 7 — Core refactor: tighten traits now that 3 deployment models exist

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
| 1. Core — L1-3 + L4/L5 framework + shared rules + `Source`/`DataModel` | 1–2 | ~6500 | 1.5 weeks |
| 2. Runtime HTTP+store | 1 | ~7700 | 3 days |
| 3. deployment-model-asapfusion (thin; rules fold into core library) | 1 | ~9500 | 1 week |
| 4. deployment-model-asapquery (L2 tree + L3/L4 split + YAML + CLI) | 1 | ~14000 | 2 weeks |
| 5. deployment-model-asaplifecycle (cost models + deployment-model-specific rules + OpAMP wiring) | 2 | ~17500 | 1 week |
| 6. Cutover (delete in source repos) | 3–4 | ~18000 | 1 week |
| 7. Core refactor | 1 | ~18500 | 3 days |

**Total**: ~6.5 weeks for one engineer. Phase 1 grew (+0.5w) because it now also lifts L4/L5 shared infrastructure. Phases 3 and 5 shrank correspondingly (-1w combined) because fusion's `SketchConfigRule`-style bindings and lifecycle's `StageAllocator` now come from core's shared library. Net: slightly faster total, much cleaner per-deployment-model crates (each ~1500-2500 LOC of deployment-model-specific code rather than 5000+).

Parallelisable if two people: lifecycle (Phase 5) can start as soon as Phase 1's L4/L5 framework lands (before Phase 4's L2 tree work).

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| YAML emission byte-differences between old and new planner | Medium | Breaks ASAPQuery-backend's config consumer | Golden-file test corpus in Phase 4; fuzz harness over the YAML schema |
| OpAMP protocol detail drifts during port | Low | Breaks agent config push | Port opamp/ verbatim in Phase 2's runtime crate; don't touch until it's proven to work |
| DataFusion version skew in deployment-model-asapfusion | Low | Benchmarks drift | Pin DataFusion version in workspace `Cargo.toml`; measure benches before + after |
| Two asap-planner-rs copies have subtle diff we miss | Medium | Behaviour regression | Explicit diff + reconcile step in Phase 4; one authoritative copy (the backend's newer one) |
| ASAPQuery-backend deploys break during cutover | Low | Production outage | Phase 6 only after Phase 5 fully validates; staged rollout of docker-compose change |
| Scope creep — "while we're at it, redesign the cost model" | High | Doubles timeline | Explicit non-goal list in design doc §2; defer all rewrites to post-migration phases |
| Core traits are wrong; need to rev after deployment models land | Medium | Phase 7 PR larger than expected | Accept; Phase 7 is planned for exactly this. Don't over-design Phase 1. |
| PromQL L2 tree reverse-engineering loses a pattern-match case planner relied on | Medium | Generated YAML diverges from legacy | Golden-file corpus covers all 5 pattern shapes + generic fallthrough; tree-shape inspection at L3 lowering is a direct 1:1 translation of today's pattern-match logic, not a reinterpretation |
| L3/L4 split misses a case in `map_statistic_to_precompute_operator` | Medium | Wrong sketch selected for some `(Statistic, Treatment)` combos | Every branch of the current function must round-trip via the new L3→L4 path on the golden-file corpus; add table-driven unit tests enumerating every `Statistic × Treatment` combination |
| `AggIntent` as a 25-variant superset grows unbounded as deployment models add needs | Low | Core churn | Variants added only for intent shapes used by ≥1 shipped deployment model; never speculative. |

## Decision log (during migration)

These are the questions that will come up mid-migration. Pre-decide as many as possible:

1. **Which version of `asap_types`?** ASAPQuery-backend's current one, pinned to a git SHA.
2. **Which `promql-parser` + `sqlparser` versions?** DC controller's (newer). ASAPQuery-backend's planner uses older versions; port it to the newer ones during Phase 4.
3. **How is `deployment-model-asaplifecycle`'s `StreamingConfig` YAML emitter related to `deployment-model-asapquery`'s?** One emitter in `deployment-model-asapquery`, called from `deployment-model-asaplifecycle`. Asymmetric dependency.
4. **Does `asap-controller` have feature flags to disable deployment models at build time?** Yes, one `--features lifecycle,query,fusion` with all three enabled by default. A minimal CLI binary (`asap-query`) disables lifecycle and fusion.
5. **Who owns the Cargo.lock?** ASAPController. Downstream repos (ASAPQuery-backend, DataCollector) do NOT depend on ASAPController as a Cargo path dep — they pull the published binary via Docker or pin a git SHA.
6. **Does `ControllerClient` on the ASAPQuery-backend side need changes?** No. It keeps POSTing to `/api/v1/plan`; the controller's routing layer dispatches to `deployment-model-asaplifecycle` (same as DC controller does today).
7. **L2 tree for asap-planner-rs — mandatory or optional?** Mandatory. planner's current template-catalogue approach is replaced with a proper `PromqlLogicalPlan` tree in Phase 4. The extra conformance work is taken to keep the 5-layer model uniform. See design doc §12 Q8 for the escape hatch if a future deployment model genuinely can't fit a tree.
8. **Sketch binding — L3 or L4?** L4. DC and fusion already do this correctly; planner's `map_statistic_to_precompute_operator` conflates L3+L4 and is split during Phase 4. `AggIntent` at L3 carries intent + accuracy only; concrete `AggregationType` / `SketchParams` are produced by an L4 rule.
9. **Intent vocabulary when deployment models disagree in width?** Core's `AggIntent` is the superset (DC's ~25 variants). Each deployment model only uses / produces / accepts the subset it needs (planner 9, fusion 3). Adding a new intent variant is a core change that deployment models opt into.
10. **Tabular vs time-series data models?** Handled in core from Phase 1 via the `Source` sum (`TimeSeries` / `Table` / `Join`) inside `QueryExpr::Scan` and the `DataModel` tag on `AggIntent`. ASAPQuery produces `Source::TimeSeries`, asap-fusion produces `Source::Table`. `Source::Table` is stubbed in Phase 1 and filled in by Phase 3 (fusion). Sketches themselves are data-model-agnostic; the generalization is purely at the IR leaf.

## What a new deployment model looks like post-migration

This is the test of the architecture. Adding a 4th deployment model (e.g. "edge-caching") — typical size ~500-2000 LOC:

1. `cargo new --lib crates/deployment-model-edge-cache` (in ASAPController workspace) OR a new crate in your own repo that depends on `asap-control-core = { git = "...", tag = "v0.1.0" }`.
2. Four files in `src/`:
   - `lib.rs` — `impl DeploymentModel for EdgeCacheDeploymentModel`
   - `rules.rs` — pick rules from `core::optimizer::rules::*` + add deployment-model-specific rules
   - `topology.rs` — declare a `TopologyDescriptor` (e.g. `TwoStage { edge, backend }`) or pick a pre-baked core one
   - `emit/edge_agent_config.rs` — `impl PlanEmitter`
3. Optionally: `cost.rs` if the deployment model needs its own cost model; otherwise use `core::optimizer::cost::*`.
4. If landing in ASAPController workspace: 1 line in `bin/asap-controller/main.rs` to register + 1 line in workspace `Cargo.toml`. If out-of-tree: publish as a binary in your own repo.

**No changes** to runtime, core, or other deployment models. That's the extension-point test passing.

### Why post-migration deployment models are much smaller than pre-migration code

Pre-migration: each deployment model ships its own rule engine, cost traits, allocator, topology handling, sketch catalogue. Total per deployment model: 3000-8000 LOC.

Post-migration: those all come from `core::optimizer::*` + `core::physical::*` as imports. Per-deployment-model code is only:
- Which rules to enable (usually a dozen lines — pick from `core::optimizer::rules`)
- Topology declaration (a dozen lines — often reusing a pre-baked `core::physical::topology::*`)
- DeploymentModel-specific rules that truly can't be shared (100-500 LOC each)
- DeploymentModel-specific cost models (if different from core's generic)
- Emitter(s) for the output format (100-1000 LOC)

Typical post-migration deployment model: **500-2500 LOC**. Extremely easy to understand, review, and evolve.

## Rollback strategy

Each phase is a `git revert`-able PR. The three source repos (`DataCollector`, `ASAPQuery`, `ASAPQuery-backend`, `asap-fusion`) are NOT modified until Phase 6. If Phase 5 reveals the design is wrong, we can revert 0–4 and start over without disrupting any live service.

Phase 6 is the only one-way door. Before landing it:
- At least 1 week of the new `asap-controller` binary running in staging.
- Golden-file corpus at 100% match rate for 1 week.
- SLA replan observed firing correctly at least once in staging.
