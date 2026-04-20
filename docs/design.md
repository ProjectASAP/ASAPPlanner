# ASAPController — design

Target repo: **`github.com/ProjectASAP/ASAPController`** (currently empty).

Merges three existing codebases into one:

1. **`DataCollector/controller`** (Rust) — service-shaped; end-to-end data lifecycle planner (collection → transmission → storage → analytics query); OpAMP + HTTP; SLA-driven replanning loop.
2. **`ASAPQuery[-backend]/asap-planner-rs`** (Rust) — CLI-shaped; analytics-query-only planner; YAML-in → YAML-out. Two divergent copies today.
3. **`asap-fusion`** (Rust) — library-shaped; DataFusion operator-level rewrite rules with sketch awareness; multi-query batch fusion is aspirational, executor is a thin wrapper.

## 1. Goals

1. **One repo, one workspace.** One `Cargo.toml` at root, one place to file issues, one release cadence.
2. **Common core, pluggable scenarios.** The three existing codebases each solve a *different* planning problem. Factor out what's actually shared; keep the scenario-specific parts as crates so each scenario can evolve independently.
3. **Extensible for future scenarios** (4th, 5th, …). A new scenario should land as a new crate that implements well-defined traits — not by touching the core or the runtime.
4. **Preserve the two deployment shapes:**
    - **Service**: long-running HTTP+OpAMP process that accepts live `QuerySpec`s, replans on SLA violations, pushes configs to agents/backends. (DC controller today.)
    - **CLI**: one-shot "read workload YAML → emit `streaming_config.yaml` + `inference_config.yaml`". (`asap-planner-rs` today.)
    Both should be thin shells over the same core.
5. **No regression in today's wire contracts.** `POST /api/v1/streaming-config` to ASAPQuery-backend and OpAMP push to agents must keep working byte-for-byte through the migration. The backend's capability-miss callback `ControllerClient.create_plan` must keep working.

## 2. Non-goals

- **Not** redesigning the `Plan` IR end-to-end. DC's `algebra/` + fusion's `translator/optimizer/executor` stay where they are semantically; we merge them into a shared `Plan` crate with a clean trait seam, not a green-field rewrite.
- **Not** unifying DataFusion's `LogicalPlan` with DC's custom algebra at the type level. Those are different universes. Scenarios own their IR; core owns the *staged dispatch contract*.
- **Not** in-scope: changing the existing wire protocol between controller and backend. ASAPQuery-backend keeps consuming `streaming_config.yaml` and keeps responding on `/api/v1/plan`.

## 3. Principles

### P1. Core is small, scenarios are large

If a type exists in exactly one of the three source repos, it belongs in that repo's scenario crate, not in core. Core only holds things that appear in 2+ sources, *or* that a new 4th scenario will clearly need.

### P2. Core has no I/O

No HTTP, no OpAMP, no YAML, no Prometheus scrape, no `tokio::spawn`. Pure types + traits + in-memory algorithms. This is what makes scenarios unit-testable without running the runtime.

### P3. Runtime is a thin binary, scenarios are libraries

The `asap-controller` binary is assembled from: runtime (HTTP/OpAMP/replanner/store) + N scenario crates registered as plugins. Swapping scenarios is a build-time feature flag or a runtime registry entry. No scenario may reach directly into another.

### P4. One input boundary, one output boundary

**Input**: everything that enters the controller (HTTP `QuerySpec`, Prometheus query log replay, YAML workload, capability-miss callback) normalises into a single `QueryWorkload` type in core.

**Output**: everything that leaves (OpAMP `RemoteConfig`, backend `StreamingConfig` POST, one-shot YAML file, a rewritten DataFusion `LogicalPlan`) is emitted by a `PlanEmitter` trait. Scenarios supply emitter implementations; core doesn't know which emitters exist.

This is the "extension point for future scenarios" — a new scenario adds a `Planner` + `PlanEmitter` pair and registers them.

### P5. No feature-flag spaghetti

If something must be optional (e.g. OpAMP for deployments that don't run OTel collectors), it's a separate crate, not a `cfg` block in core.

## 4. Target repo layout

```
ASAPController/
├── Cargo.toml                 # workspace
├── README.md
├── docs/
│   ├── design.md              # this file
│   ├── migration-plan.md      # the companion file
│   ├── scenarios/             # per-scenario design notes
│   └── adr/                   # architecture decision records
├── proto/
│   ├── asap_control.proto     # Plan IR + QueryWorkload on the wire
│   └── opamp.proto            # vendored from DataCollector
├── crates/
│   ├── core/                  # IR, traits, no I/O
│   │   ├── plan/              # Plan, PlanNode, Expr, PhysicalPlan (seam)
│   │   ├── workload/          # QueryWorkload, QuerySpec, QueryLog
│   │   ├── cost/              # CostModel trait, TCO, Pareto frontier
│   │   ├── emit/              # PlanEmitter trait + StreamingConfig /
│   │   │                      # InferenceConfig / OTelConfig value types
│   │   ├── registry/          # ScenarioRegistry, PlannerKey, EmitterKey
│   │   └── telemetry/         # tracing macros, metric names (no exporter)
│   ├── runtime/               # service skeleton — HTTP, OpAMP, replanner
│   │   ├── http/              # axum surface — /plan, /replan, /metrics, /status
│   │   ├── opamp/             # WebSocket OpAMP server
│   │   ├── monitor/           # Scraper, Thresholds, Violation
│   │   ├── replan/            # Replanner — SLA + expiry triggers
│   │   ├── store/             # PlanStore, WorkloadStore
│   │   └── backend_client/    # HTTP client (pushes to ASAPQuery-backend, etc.)
│   ├── scenario-lifecycle/    # DC's end-to-end scenario (collection→…→query)
│   │   ├── stage_split/       # splitting expressions across agent/gateway/backend
│   │   ├── delta_cost/        # raw vs sketch vs delta cost modelling
│   │   ├── pareto/            # Pareto frontier over accuracy × latency × $
│   │   └── tco/               # total cost of ownership model
│   ├── scenario-query/        # analytics query planning (asap-planner-rs + DC query hooks)
│   │   ├── pattern/           # PromQL pattern matching + SQL pattern matching
│   │   ├── single_query/      # per-query planner
│   │   ├── query_log/         # Prometheus query-log replay
│   │   ├── schema/            # PromQLSchema discovery from Prometheus
│   │   └── yaml/              # StreamingConfig + InferenceConfig YAML emitters
│   ├── scenario-fusion/       # operator-level batch fusion (ex-asap-fusion)
│   │   ├── translator/        # LogicalPlan → ASAP plan
│   │   ├── optimizer/         # PlanRewriteRule + rules/
│   │   ├── executor/          # DataFusion SessionContext wrapper
│   │   └── sketch_rules/      # sketch-aware rewrites
│   ├── control-proto/         # generated from proto/ (tonic/prost)
│   └── testing/               # test fixtures + harness shared across scenarios
└── bin/
    ├── asap-controller/       # long-running service
    │   └── main.rs            # axum + OpAMP + replanner + scenarios
    └── asap-plan/             # one-shot CLI (what asap-planner-rs is today)
        └── main.rs            # clap — read workload YAML, run scenario-query, emit two YAMLs
```

### Why three scenario crates today, not one monolithic `scenarios/`

Each scenario has a different problem shape:

| | **lifecycle** | **query** | **fusion** |
|---|---|---|---|
| Input | workload + live metrics | workload YAML / query log | DataFusion `LogicalPlan` |
| Decision unit | end-to-end staged pipeline | per-aggregation YAML | per-operator rewrite |
| Output | OpAMP OTel config + `StreamingConfig` YAML | `streaming_config.yaml` + `inference_config.yaml` | rewritten `LogicalPlan` |
| Trigger | QuerySpec, SLA violation, expiry | one-shot CLI invocation | a DataFusion session constructing a query |
| Cost model | accuracy × latency × $ staged | single-query accuracy/latency | operator selectivity / sketch feasibility |

Mashing them into one crate means the union of all their dependencies (sqlparser + promql-parser + DataFusion + OpAMP proto + Prometheus client) bleeds into every downstream user. Separating them means a user who only needs `scenario-fusion` (an offline query-optimization benchmark, say) can depend on it without pulling OpAMP.

## 5. Core crate details

### `core::plan`

A staged plan IR with three levels — matches the shape in both DC's `algebra/` and fusion's `translator/optimizer/executor`:

```rust
pub enum Plan { Logical(LogicalPlan), Physical(PhysicalPlan) }

pub trait Planner {
    type Input;                              // QueryWorkload, usually
    type Output;                             // Plan, usually
    fn plan(&self, input: Self::Input) -> Result<Self::Output, PlanError>;
}

pub trait Optimizer {
    fn rules(&self) -> &[Box<dyn PlanRewriteRule>];
    fn optimize(&self, plan: Plan) -> Result<Plan, PlanError>;
}

pub trait PlanRewriteRule {
    fn name(&self) -> &'static str;
    fn apply(&self, plan: Plan) -> Result<Option<Plan>, PlanError>;
}
```

Scenarios plug in by implementing `Planner`. `scenario-fusion` additionally implements `Optimizer` with its rule registry. `scenario-lifecycle` produces `Physical(PhysicalPlan)` directly (its staged physical plan).

### `core::workload`

One public type for every kind of input:

```rust
pub enum QueryWorkload {
    /// A single QuerySpec from an HTTP POST. DC's current entry point.
    Single(QuerySpec),
    /// A hand-authored YAML workload file. asap-planner-rs's current entry point.
    AuthoredSet(Vec<QuerySpec>),
    /// A Prometheus query log replayed. asap-planner-rs's alt entry point.
    QueryLog(QueryLogReplay),
}

pub struct QuerySpec { /* metric_name, aggregations, time_window, SLAs, ... */ }
```

All three scenarios take `&QueryWorkload`. Same type, different planners.

### `core::cost`

Extracted from DC's `planner/*` (delta, online, pareto, tco). Scenarios pick the models they need.

```rust
pub trait CostModel {
    fn accuracy(&self, plan: &Plan) -> Accuracy;
    fn latency(&self, plan: &Plan) -> Duration;
    fn dollars(&self, plan: &Plan) -> Dollars;
}
```

### `core::emit`

```rust
pub trait PlanEmitter {
    type PlanInput;
    type Output;                             // YAML bytes, OpAMP RemoteConfig, rewritten LogicalPlan
    fn emit(&self, plan: &Self::PlanInput) -> Result<Self::Output, EmitError>;
}
```

Concrete emitters live in scenario crates:

- `scenario-query::yaml::StreamingConfigEmitter` → `streaming_config.yaml` bytes
- `scenario-query::yaml::InferenceConfigEmitter` → `inference_config.yaml` bytes
- `scenario-lifecycle::emit::OpAmpRemoteConfigEmitter` → `opamp::RemoteConfig` protobuf
- `scenario-fusion::emit::DataFusionPlanEmitter` → rewritten `datafusion::LogicalPlan`

### `core::registry`

The extension point. The runtime binary does:

```rust
let mut reg = ScenarioRegistry::new();
reg.register::<LifecycleScenario>();
reg.register::<QueryScenario>();
reg.register::<FusionScenario>();
// add more...
```

Registration is by-type; the runtime looks up by `ScenarioId` (either from the `QuerySpec.scenario` field or from an HTTP route). A 4th scenario is added by:

1. New crate `scenario-<name>/`
2. Implement `Scenario` trait (wraps a `Planner` + its `PlanEmitter`s)
3. Register in `bin/asap-controller/main.rs`

No core change.

## 6. Runtime crate details

Lifted from DC controller with zero semantic change:

- `http/` — axum server on `:8080`. Routes: `POST /plan`, `POST /replan`, `GET /plans/:id`, `GET /status`, `GET /metrics`
- `opamp/` — WebSocket OpAMP server on `:4320`. Same protocol DC speaks today.
- `monitor/` — `Scraper` polls Prometheus, emits `Violation`
- `replan/` — subscribes to violations + expiry ticks, re-invokes the scenario registry
- `store/` — in-memory `PlanStore` + `WorkloadStore`. Pluggable backend later.
- `backend_client/` — pushes `StreamingConfig` YAML to ASAPQuery-backend's `/api/v1/streaming-config` endpoint. Factored out so new scenarios can push to other backends.

Runtime depends on `core` but NOT on any scenario crate directly. It talks to scenarios via the registry.

## 7. Scenario crate details

Each scenario crate is a library with:

- A `Scenario` impl that registers its planner, its emitters, and its HTTP routes (if any).
- Its own config types — no shared config crate.
- Its own cost model — maybe using `core::cost` primitives, maybe not.
- Its own integration tests.

### `scenario-lifecycle`

Everything from DC controller's `planner/`, `algebra/`, `config/generate_*`. Stage-split, delta cost, online cost, Pareto. Emits `OpAmpRemoteConfig` (per-role) + `StreamingConfig` YAML + `AsapqueryBackendConfig`. Registers HTTP route `POST /plan` as the full-lifecycle planner.

### `scenario-query`

What `asap-planner-rs` is today plus the bits of DC controller that overlap (`config/asapquery_backend.rs`, `config/generate_streaming_config_yaml`). Emits `StreamingConfig` + `InferenceConfig` YAML. Registers HTTP route `POST /plan/query` (JSON QuerySpec in, YAML stream out) AND serves as the CLI backend for `bin/asap-plan`.

### `scenario-fusion`

Exactly what `asap-fusion` is today: translator → optimizer → executor over DataFusion. Emits a rewritten `datafusion::LogicalPlan` (in-process; not a wire format). Registers no HTTP route by default (this is a library-mode scenario); users construct it in-process.

The sketch microbenchmarks stay. The TODO items (batch/multi-query execution, time semantics, distributed) remain open but are now filed against `scenario-fusion` specifically, not a separate repo.

## 8. Extension point — a 4th scenario

A hypothetical "edge caching" scenario that decides which queries to cache at the edge vs. backend would land as:

```
crates/scenario-edge-cache/
├── Cargo.toml               # depends on core only
├── src/
│   ├── lib.rs              # impl Scenario for EdgeCacheScenario
│   ├── planner.rs          # impl Planner — takes QueryWorkload, outputs CachePlan
│   ├── emit/
│   │   └── edge_config.rs  # impl PlanEmitter — emits edge-agent YAML
│   └── cost.rs             # impl CostModel — hit ratio × bandwidth
└── tests/
```

Total touch outside the new crate:
- 1 line in `bin/asap-controller/main.rs` to register
- 1 line in workspace `Cargo.toml`
- optional: add `ScenarioId::EdgeCache` to `core::registry`

## 9. Wire protocols — what changes, what doesn't

**Unchanged** (hard contract with other systems):
- `POST /api/v1/streaming-config` on ASAPQuery-backend — consumed YAML format
- `POST /api/v1/plan` on DC controller (from backend capability-miss) — request JSON
- OpAMP `ServerToAgent.remote_config` payload — OTel collector YAML
- Prometheus scrape format

**Internal** (controller's own surface, still HTTP+JSON for now):
- `POST /plan` — new unified entry point that takes `QueryWorkload` in, returns a plan ID + list of emitted artefacts (URIs). `/api/v1/plan` proxies to this.
- `GET /plans/:id` — plan inspection
- `GET /status` — runtime + scenario health

**New** (internal-only, proto):
- `proto/asap_control.proto` defines `Plan`, `PlanNode`, `Expr` for persistence + store-internal serialisation. Not on the wire between services. Optional; initial migration skips this and persists via `serde_json`.

## 10. Dependencies and Cargo surface

Workspace `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/core",
    "crates/runtime",
    "crates/scenario-lifecycle",
    "crates/scenario-query",
    "crates/scenario-fusion",
    "crates/control-proto",
    "crates/testing",
    "bin/asap-controller",
    "bin/asap-plan",
]

[workspace.dependencies]
tokio = "1"
axum = "0.7"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
prost = "0.13"
tonic = "0.12"
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
tracing = "0.1"
thiserror = "1"
# scenario-specific deps kept in scenario crates
```

Core depends only on `serde`, `tracing`, `thiserror`, and small utility crates. No HTTP, no DataFusion, no OpAMP.

Runtime depends on core + `axum` + `reqwest` + `tokio-tungstenite` + `prost` (OpAMP proto).

Scenarios depend on core. Scenario-query also pulls in `promql-parser`, `sqlparser`. Scenario-fusion pulls in `datafusion` + `arrow`. Scenario-lifecycle pulls in `sqlparser` + `promql-parser` + the cost-model math crates.

This matters: a user who only wants `scenario-fusion` (e.g., an offline benchmark) gets DataFusion but NOT axum/OpAMP.

## 11. Open questions

1. **Do we need a cross-scenario cost model?** Today DC's lifecycle planner and asap-planner-rs's query planner have overlapping but not identical cost models. Answer for now: keep them separate in scenario crates; let them re-converge organically. If a third scenario needs the same model, lift at that point.

2. **Where does the backend's `ControllerClient.create_plan` call land?** Initially: HTTP `POST /api/v1/plan` on the controller, handled by `scenario-lifecycle` (same as today). Long-term: could route by `QuerySpec.scenario` to a different planner, but that's a follow-up.

3. **Which scenario owns `StreamingConfig` YAML emission?** Both `scenario-lifecycle` and `scenario-query` emit it today (DC's `config/generate_streaming_config_yaml` and `output/generator.rs` in `asap-planner-rs`). Plan: **one emitter in `scenario-query`**, called from both. This is why `scenario-lifecycle` depends on `scenario-query` in the dependency graph (asymmetric — query does not depend on lifecycle).

4. **Do we vendor DataFusion's IR into core?** No. `scenario-fusion` owns its DataFusion-flavoured plan; `core::plan::Plan` stays an enum with a variant that wraps a `datafusion::LogicalPlan` behind a feature flag. Core itself never reaches into DataFusion types.

5. **OpAMP proto: vendored, or from crates.io?** Today DC vendors. Recommend: keep vendored in `proto/opamp.proto`, generate via `prost-build` in `crates/control-proto`. Same thing DC does today, just moved.

6. **What happens to ASAPQuery-backend's `asap-planner-rs` directory post-migration?** Deleted. ASAPQuery-backend's docker-compose drops the `asap-planner-rs` init container; the controller's `scenario-query` now runs in-process (service mode) or as the `asap-plan` CLI (one-shot mode). The ASAPQuery-backend repo shrinks by two directories.

7. **Versioning?** Start at `0.1.0` on the workspace. Scenarios can rev independently later via per-crate versions, but initially lockstep.

## 12. Success criteria

The migration is done when:

1. `asap-controller` binary runs and passes DC controller's existing integration tests (OpAMP push, backend config POST, SLA replan).
2. `asap-plan` binary takes the same YAML input asap-planner-rs does today and produces byte-identical `streaming_config.yaml` + `inference_config.yaml` (fuzz-test against a corpus of fixtures).
3. ASAPQuery-backend's docker-compose no longer starts `asap-planner-rs`; the controller handles both shapes.
4. `asap-fusion`'s microbenchmarks still run under `scenario-fusion` with identical numbers.
5. The `DataCollector/controller/`, `ASAPQuery/asap-planner-rs/`, `ASAPQuery-backend/asap-planner-rs/`, and `asap-fusion/` directories are deletable (or already deleted) without breaking any currently-running deployment.
6. A new hypothetical scenario can be added with zero changes outside its crate + one line in `bin/asap-controller/main.rs`.
