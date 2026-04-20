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

## 3. The organising spine: DC controller's 5-layer pipeline

DataCollector/controller already documents its query→sketch translation as a 5-layer pipeline (`DataCollector/controller/docs/query-to-sketch-translation.md`). That's the spine of this merger:

| # | Layer | What it does | Today's locations |
|---|-------|--------------|-------------------|
| 1 | **Query Language** | Parse raw strings (PromQL, SQL, DataFusion, ElasticDSL, …) into a language-specific AST | DC `controller/src/query_parser/{promql,sql,mod}.rs`; asap-planner-rs pulls `promql-parser` + `sqlparser` directly |
| 2 | **Language Logical Plan** | Per-language algebra tree (`Aggregate` / `Window` / `Filter` / `Sort` / `Limit`) preserving language semantics, no sketch names | DC `controller/src/algebra/lower.rs` (the 1→2→3 pipeline) |
| 3 | **Sketch Logical Plan** (sketch algebra) | Language- and deployment-independent IR: `QueryExpr` + `AggIntent` (~25 operator variants) — *what* to compute, not *how* | DC `controller/src/algebra/{expr,directory}.rs` |
| 4 | **Sketch Optimizer** | ~12 cost-aware algebraic rewrite rules (push-down, fusion, elimination, budget-driven deferral) applied to fixed-point under deployment constraints | DC `controller/src/algebra/optimizer.rs`; asap-fusion `src/optimizer/rules/` (DataFusion-flavoured) |
| 5 | **Physical Execution Plan** | Commit to concrete `SketchType` + `SketchParams`; assign ops to pipeline stages (edge / gateway / backend / object store) | DC `controller/src/algebra/{physical,allocator,plan}.rs`; asap-planner-rs emits as `streaming_config.yaml` + `inference_config.yaml`; asap-fusion emits as rewritten DataFusion `LogicalPlan` |

**The doc's key claim: layers 1–3 are query-language-independent and workload-independent.** That makes them the natural **common core**. Every scenario — full-lifecycle planning, analytics-query-only planning, operator fusion, and any future scenario — reads PromQL (or SQL, or …) the same way, lowers it to the same language-independent algebra, and lowers THAT to the same sketch-algebra IR. Only layers 4 and 5 differ by scenario.

This drives the core/scenario split:

- **`crates/core/`** owns layers 1–3 verbatim. It's the parsers + the two lowering passes + the sketch-algebra IR. Real code with real algorithms — not just types and traits.
- **Each scenario owns its own L4 + L5.** Scenarios contribute optimizer rules (L4) and physical-plan emitters (L5). The driver that runs L1→L2→L3→L4→L5 lives in core as a small orchestration layer, parameterised on the scenario's rule set + emitter.

## 4. Principles

### P1. Core owns L1–3; scenarios own L4–5

See §3. Core is NOT just types + trait stubs — it ships working parsers + lowering passes. What scenarios plug in is the optimizer rule set (L4) and the physical-plan + emitter (L5). A new scenario that only redefines L4 or only redefines L5 is a valid minimal extension.

### P2. Core has no I/O

No HTTP, no OpAMP, no YAML, no Prometheus scrape, no `tokio::spawn`. Pure algorithms over in-memory data. This is what makes scenarios unit-testable without running the runtime.

### P3. Runtime is a thin binary, scenarios are libraries

The `asap-controller` binary is assembled from: runtime (HTTP/OpAMP/replanner/store) + N scenario crates registered as plugins. Swapping scenarios is a build-time feature flag or a runtime registry entry. No scenario may reach directly into another.

### P4. One input boundary, one output boundary

**Input**: everything that enters the controller (HTTP `QuerySpec`, Prometheus query log replay, YAML workload, capability-miss callback) normalises into a single `QueryWorkload` type in core — a collection of `QuerySpec`s each feeding L1→L2→L3.

**Output**: L5 emitters (OpAMP `RemoteConfig`, backend `StreamingConfig` POST, one-shot YAML file, rewritten DataFusion `LogicalPlan`) all implement a `PlanEmitter` trait. Scenarios supply emitter implementations; core doesn't know which emitters exist.

This is the "extension point for future scenarios" — a new scenario adds an L4 rule set + an L5 emitter and registers them.

### P5. No feature-flag spaghetti

If something must be optional (e.g. OpAMP for deployments that don't run OTel collectors), it's a separate crate, not a `cfg` block in core.

## 5. Target repo layout

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
│   ├── core/                  # Layers 1-3 + traits; no I/O
│   │   ├── query_language/    # L1: per-language parsers — promql/, sql/, datafusion/, elasticdsl/
│   │   ├── logical_plan/      # L2: per-language algebra tree (Aggregate/Window/Filter/…)
│   │   ├── sketch_algebra/    # L3: QueryExpr + AggIntent (~25 variants) + directory
│   │   ├── lower/             # L1→L2→L3 lowering passes (one entry per language)
│   │   ├── pipeline/          # orchestrates L1→L2→L3→L4→L5, parameterised on scenario
│   │   ├── workload/          # QueryWorkload wrapper around QuerySpecs
│   │   ├── cost/              # CostModel trait — consumed by scenario L4 rules
│   │   ├── emit/              # PlanEmitter trait — implemented by scenario L5
│   │   ├── registry/          # ScenarioRegistry, ScenarioId
│   │   └── telemetry/         # tracing macros, metric names (no exporter)
│   ├── runtime/               # service skeleton — HTTP, OpAMP, replanner
│   │   ├── http/              # axum surface — /plan, /replan, /metrics, /status
│   │   ├── opamp/             # WebSocket OpAMP server
│   │   ├── monitor/           # Scraper, Thresholds, Violation
│   │   ├── replan/            # Replanner — SLA + expiry triggers
│   │   ├── store/             # PlanStore, WorkloadStore
│   │   └── backend_client/    # HTTP client (pushes to ASAPQuery-backend, etc.)
│   ├── scenario-lifecycle/    # DC's end-to-end scenario — its own L4 + L5
│   │   ├── optimizer/         # L4: the ~12 cost-aware rules from DC algebra/optimizer.rs
│   │   ├── physical/          # L5: stage_split, delta_cost, pareto, tco, allocator
│   │   ├── emit/              # L5: OpAmpRemoteConfig + AsapqueryBackendConfig emitters
│   │   └── cost/              # scenario-specific cost-model impls (online / delta / pareto / tco)
│   ├── scenario-query/        # analytics-query planning — own L4 + L5 (+ query-log input)
│   │   ├── optimizer/         # L4: precompute-engine-targeted rules (smaller rule set than lifecycle)
│   │   ├── physical/          # L5: per-aggregation physical plan
│   │   ├── emit/              # L5: StreamingConfig.yaml + InferenceConfig.yaml emitters
│   │   ├── query_log/         # extra L1 input: Prometheus query-log replay
│   │   └── schema/            # PromQLSchema discovery from Prometheus (feeds L1)
│   ├── scenario-fusion/       # DataFusion operator-level fusion — own L4 + L5
│   │   ├── optimizer/         # L4: PlanRewriteRule + rules/ (sketch-aware DF rewrites)
│   │   ├── physical/          # L5: rewritten DataFusion LogicalPlan
│   │   ├── executor/          # DataFusion SessionContext wrapper (in-process execution)
│   │   └── sketch_support/    # asap_sketchlib-backed rewrites
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

## 6. Core crate details (layers 1–3 + driver)

Core is not a trait-stubs library. It ships real L1/L2/L3 code lifted from DC's `controller/src/query_parser/` + `controller/src/algebra/` and exposes a small set of traits for L4/L5 plugin points.

### `core::query_language` — Layer 1

Per-language parsers, one module each:

- `promql/` — wraps `promql-parser`. Input: `&str` PromQL. Output: `PromqlAst`.
- `sql/` — wraps `sqlparser`. Input: `&str` SQL. Output: `SqlAst`.
- `datafusion/` — wraps DataFusion's own parser. Output: `DfAst`.
- `elasticdsl/` — wraps `elastic_dsl_utilities`. Output: `EsAst`.

Each returns a language-flavoured AST type. No sketch awareness.

### `core::logical_plan` — Layer 2

A **per-language** algebra tree — one `enum LogicalPlan` per language. Preserves language-specific semantics (PromQL instant vs range vector, SQL window frames, Elastic buckets) that would be lossy to collapse this early. Types are symmetric: `Aggregate { AggFunc }`, `Window`, `Filter`, `Sort`, `Limit`. No sketch names yet.

### `core::sketch_algebra` — Layer 3

The language- and deployment-independent IR. This is the sketch-algebra layer documented in DC's `controller/src/algebra/expr.rs`:

```rust
pub enum QueryExpr {
    Scan { metric: MetricRef, time: TimeRange, labels: LabelFilter },
    Filter { child: Box<QueryExpr>, pred: Predicate },
    Aggregate { child: Box<QueryExpr>, by: Vec<Label>, intent: AggIntent },
    // ...
}

pub enum AggIntent {
    Count, Sum, Min, Max,
    Quantile(f64, AccuracyTarget),
    TopK(usize, AccuracyTarget),
    Cardinality(AccuracyTarget),
    // ~25 variants total
}
```

`AggIntent` describes **what** to compute (a quantile, a topk, a count) + what accuracy to hit — not **how** (KLL vs DDSketch vs CMS). That binding happens in L5.

### `core::lower` — L1 → L2 → L3 passes

One pass per language, each producing the same `sketch_algebra::QueryExpr`:

```rust
pub fn lower_promql(ast: PromqlAst, schema: &MetricSchema) -> Result<QueryExpr>;
pub fn lower_sql(ast: SqlAst, schema: &TableSchema) -> Result<QueryExpr>;
pub fn lower_datafusion(ast: DfAst, ctx: &DfContext) -> Result<QueryExpr>;
pub fn lower_elasticdsl(ast: EsAst, schema: &IndexSchema) -> Result<QueryExpr>;
```

Once a query hits L3 it's language-agnostic. All scenarios downstream see the same IR.

### `core::pipeline` — orchestration

The L1→…→L5 driver. Parameterised on a scenario's optimizer rules (L4) + emitter (L5):

```rust
pub struct Pipeline<S: Scenario> {
    scenario: S,
}

impl<S: Scenario> Pipeline<S> {
    pub fn run(&self, workload: &QueryWorkload) -> Result<S::EmitterOutput, PipelineError> {
        let l3: Vec<QueryExpr> = workload.queries()
            .iter()
            .map(|q| self.parse_and_lower(q))   // L1→L2→L3
            .collect::<Result<_, _>>()?;
        let l4 = self.scenario.optimizer().optimize(l3)?;  // L4
        let l5 = self.scenario.physical().lower(l4)?;      // L5
        self.scenario.emitter().emit(&l5)                   // L5 → bytes/protobuf/DF plan
    }
}
```

Core owns the driver. Scenarios own what goes into each scenario-specific seam.

### `core::plan` — shared IR pieces used across layers

```rust
pub trait Optimizer {
    fn rules(&self) -> &[Box<dyn OptimizerRule>];
    fn optimize(&self, l3: Vec<QueryExpr>) -> Result<OptimizedL3, PlanError>;
}

pub trait OptimizerRule {
    fn name(&self) -> &'static str;
    fn apply(&self, expr: &QueryExpr, constraints: &DeploymentConstraints) -> Option<QueryExpr>;
}

pub trait PhysicalPlanner {
    type Output;
    fn lower(&self, l4: OptimizedL3) -> Result<Self::Output, PlanError>;
}
```

`OptimizerRule::apply` takes a `DeploymentConstraints` reference — memory budgets, network topology, available sketch backends. Scenarios pass their own constraints; core provides the rule-driver that runs rules to fixed-point.

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

## 7. Runtime crate details

Lifted from DC controller with zero semantic change:

- `http/` — axum server on `:8080`. Routes: `POST /plan`, `POST /replan`, `GET /plans/:id`, `GET /status`, `GET /metrics`
- `opamp/` — WebSocket OpAMP server on `:4320`. Same protocol DC speaks today.
- `monitor/` — `Scraper` polls Prometheus, emits `Violation`
- `replan/` — subscribes to violations + expiry ticks, re-invokes the scenario registry
- `store/` — in-memory `PlanStore` + `WorkloadStore`. Pluggable backend later.
- `backend_client/` — pushes `StreamingConfig` YAML to ASAPQuery-backend's `/api/v1/streaming-config` endpoint. Factored out so new scenarios can push to other backends.

Runtime depends on `core` but NOT on any scenario crate directly. It talks to scenarios via the registry.

## 8. Scenario crate details (each owns its L4 + L5)

Each scenario crate is a library with:

- A `Scenario` impl that registers its planner, its emitters, and its HTTP routes (if any).
- Its own config types — no shared config crate.
- Its own cost model — maybe using `core::cost` primitives, maybe not.
- Its own integration tests.

### `scenario-lifecycle`

- **L4 rules**: the ~12 cost-aware rewrite rules from DC's `controller/src/algebra/optimizer.rs` — push-down, fusion, elimination, budget-driven deferral, aware of agent/gateway/backend stage boundaries.
- **L5 physical**: `stage_split` + `SketchAllocator` from DC's `controller/src/algebra/{physical,allocator,plan}.rs`. Commits to concrete `SketchType` + `SketchParams` per op; assigns ops to pipeline stages.
- **L5 emitters**: `OpAmpRemoteConfigEmitter` (per-role OTel YAML), `AsapqueryBackendConfigEmitter` (`StreamingConfig` YAML POSTed to backend — delegates to `scenario-query`'s emitter for the YAML bytes themselves).
- **Cost model**: delta / online (EMA) / Pareto / TCO, lifted from DC's `controller/src/planner/*`.
- **HTTP route**: registers `POST /plan` for full-lifecycle planning.

### `scenario-query`

- **L4 rules**: smaller rule set targeting the precompute engine specifically (fewer stage-aware rewrites; the deployment is just "backend only").
- **L5 physical**: per-aggregation physical plan matching the `StreamingConfig`/`InferenceConfig` YAML schema.
- **L5 emitters**: `StreamingConfigEmitter` + `InferenceConfigEmitter` (YAML bytes). These are the authoritative emitters for these two formats — `scenario-lifecycle` calls them when it needs to POST to ASAPQuery-backend.
- **Extra L1 inputs**: `query_log/` for Prometheus-query-log replay (unique to this scenario); `schema/` for PromQLSchema discovery from a live Prometheus URL.
- **HTTP route**: `POST /plan/query` (JSON `QuerySpec` in, YAML stream out). Also serves as the backend for `bin/asap-plan` one-shot CLI.

### `scenario-fusion`

- **L4 rules**: `PlanRewriteRule` trait + `rules/` subdirectory from `asap-fusion/src/optimizer/`. Sketch-aware DataFusion rewrites (e.g. "replace `approx_percentile_cont` with a KLL scan"). These rules operate on a DataFusion `LogicalPlan`, not on `core::sketch_algebra::QueryExpr` — so this scenario's L4 is a slightly different shape, which is fine: it still implements `Optimizer`, just over a different plan type.
- **L5 physical**: rewritten `datafusion::LogicalPlan`. Not a wire format — this scenario is library-mode.
- **Executor**: `ASAPExecutor` wraps a DataFusion `SessionContext`. Users construct `scenario-fusion` in-process from their own code.
- **HTTP route**: none by default.

The sketch microbenchmarks (KLL/CMS) move with the crate and keep running. The TODO items from `asap-fusion/TODO.md` (batch/multi-query execution, time semantics, distributed model) remain open but are now filed against `scenario-fusion/TODO.md` in-repo.

### Asymmetric dependency

`scenario-lifecycle` depends on `scenario-query` because the YAML emitters live there. No other cross-scenario deps.

## 9. Extension point — a 4th scenario

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

## 10. Wire protocols — what changes, what doesn't

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

## 11. Dependencies and Cargo surface

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

## 12. Open questions

1. **Do we need a cross-scenario cost model?** Today DC's lifecycle planner and asap-planner-rs's query planner have overlapping but not identical cost models. Answer for now: keep them separate in scenario crates; let them re-converge organically. If a third scenario needs the same model, lift at that point.

2. **Where does the backend's `ControllerClient.create_plan` call land?** Initially: HTTP `POST /api/v1/plan` on the controller, handled by `scenario-lifecycle` (same as today). Long-term: could route by `QuerySpec.scenario` to a different planner, but that's a follow-up.

3. **Which scenario owns `StreamingConfig` YAML emission?** Both `scenario-lifecycle` and `scenario-query` emit it today (DC's `config/generate_streaming_config_yaml` and `output/generator.rs` in `asap-planner-rs`). Plan: **one emitter in `scenario-query`**, called from both. This is why `scenario-lifecycle` depends on `scenario-query` in the dependency graph (asymmetric — query does not depend on lifecycle).

4. **Do we vendor DataFusion's IR into core?** No. `scenario-fusion` owns its DataFusion-flavoured plan; `core::plan::Plan` stays an enum with a variant that wraps a `datafusion::LogicalPlan` behind a feature flag. Core itself never reaches into DataFusion types.

5. **OpAMP proto: vendored, or from crates.io?** Today DC vendors. Recommend: keep vendored in `proto/opamp.proto`, generate via `prost-build` in `crates/control-proto`. Same thing DC does today, just moved.

6. **What happens to ASAPQuery-backend's `asap-planner-rs` directory post-migration?** Deleted. ASAPQuery-backend's docker-compose drops the `asap-planner-rs` init container; the controller's `scenario-query` now runs in-process (service mode) or as the `asap-plan` CLI (one-shot mode). The ASAPQuery-backend repo shrinks by two directories.

7. **Versioning?** Start at `0.1.0` on the workspace. Scenarios can rev independently later via per-crate versions, but initially lockstep.

## 13. Success criteria

The migration is done when:

1. `asap-controller` binary runs and passes DC controller's existing integration tests (OpAMP push, backend config POST, SLA replan).
2. `asap-plan` binary takes the same YAML input asap-planner-rs does today and produces byte-identical `streaming_config.yaml` + `inference_config.yaml` (fuzz-test against a corpus of fixtures).
3. ASAPQuery-backend's docker-compose no longer starts `asap-planner-rs`; the controller handles both shapes.
4. `asap-fusion`'s microbenchmarks still run under `scenario-fusion` with identical numbers.
5. The `DataCollector/controller/`, `ASAPQuery/asap-planner-rs/`, `ASAPQuery-backend/asap-planner-rs/`, and `asap-fusion/` directories are deletable (or already deleted) without breaking any currently-running deployment.
6. A new hypothetical scenario can be added with zero changes outside its crate + one line in `bin/asap-controller/main.rs`.
