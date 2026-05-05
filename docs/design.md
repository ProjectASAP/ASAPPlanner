# ASAPController — design

Target repo: **`github.com/ProjectASAP/ASAPController`** (currently empty).

Merges three existing codebases into one:

1. **`DataCollector/controller`** (Rust) — service-shaped; end-to-end data lifecycle planner (collection → transmission → storage → analytics query); OpAMP + HTTP; SLA-driven replanning loop.
2. **`ASAPQuery[-backend]/asap-planner-rs`** (Rust) — CLI-shaped; analytics-query-only planner; YAML-in → YAML-out. Two divergent copies today.
3. **`asap-fusion`** (Rust) — library-shaped; DataFusion operator-level rewrite rules with sketch awareness; multi-query batch fusion is aspirational, executor is a thin wrapper.

## 1. Goals

1. **One repo, one workspace.** One `Cargo.toml` at root, one place to file issues, one release cadence.
2. **Common core, pluggable deployment models.** The three existing codebases each solve a *different* planning problem. Factor out what's actually shared; keep the deployment-model-specific parts as crates so each deployment model can evolve independently.
3. **Extensible for future deployment models** (4th, 5th, …). A new deployment model should land as a new crate that implements well-defined traits — not by touching the core or the runtime.
4. **Preserve the two deployment shapes:**
    - **Service**: long-running HTTP+OpAMP process that accepts live `QuerySpec`s, replans on SLA violations, pushes configs to agents/backends. (DC controller today.)
    - **CLI**: one-shot "read workload YAML → emit `streaming_config.yaml` + `inference_config.yaml`". (`asap-planner-rs` today.)
    Both should be thin shells over the same core.
5. **No regression in today's wire contracts.** `POST /api/v1/streaming-config` to ASAPQuery-backend and OpAMP push to agents must keep working byte-for-byte through the migration. The backend's capability-miss callback `ControllerClient.create_plan` must keep working.
6. **Sketch is a primitive, not a mandate.** The optimizer selects among physical alternatives for each logical operator — e.g. `HashJoin` / `SortMergeJoin` / `SketchJoin`, or `SortAgg` / `HashAgg` / `SketchAgg` — the same way a traditional DB optimizer picks a join algorithm. A plan may come back with zero sketch operators when an exact path Pareto-dominates for the query's accuracy target. Sketches are one primitive class the optimizer can reach for; the framework does not privilege them.

## 2. Non-goals

- **Not** redesigning the `Plan` IR end-to-end. DC's `algebra/` + fusion's `translator/optimizer/executor` stay where they are semantically; we merge them into a shared `Plan` crate with a clean trait seam, not a green-field rewrite.
- **Not** unifying DataFusion's `LogicalPlan` with DC's custom algebra at the type level. Those are different universes. Deployment models own their IR; core owns the *staged dispatch contract*.
- **Not** in-scope: changing the existing wire protocol between controller and backend. ASAPQuery-backend keeps consuming `streaming_config.yaml` and keeps responding on `/api/v1/plan`.

## 3. The organizing spine: DC controller's 5-layer pipeline

DataCollector/controller already documents its query→sketch translation as a 5-layer pipeline (`DataCollector/controller/docs/query-to-sketch-translation.md`). That's the spine of this merger:

| # | Layer | What it does | Today's locations |
|---|-------|--------------|-------------------|
| 1 | **Query Language** | Parse raw strings (PromQL, SQL, DataFusion, ElasticDSL, …) into a language-specific AST | DC `controller/src/query_parser/{promql,sql,mod}.rs`; asap-planner-rs pulls `promql-parser` + `sqlparser` directly; asap-fusion consumes a pre-built DataFusion `LogicalPlan` (its L1 happens upstream) |
| 2 | **Language Logical Plan** | Per-language algebra tree (`Aggregate` / `Window` / `Filter` / `Sort` / `Limit`) preserving language semantics, **no sketch names, no sketch binding** | DC `controller/src/algebra/lower.rs`; asap-fusion inherits DataFusion's `LogicalPlan` as its L2; asap-planner-rs has no L2 today (uses a template-pattern catalogue) — **Phase 4 builds one** |
| 3 | **Intent algebra** | Language- and deployment-independent IR: `QueryExpr` + `AggIntent`. Describes **intent only** — *what* to compute, with accuracy target. **No sketch type, no sketch parameters, no sketch-bound nodes** (`SketchAgg` / `SketchJoin` / `SketchSubtract` etc. live in the L4 IR `SketchExpr`). **No language-shaped operators** (no `HistogramQuantile`, no `PromQLSubquery` — those are PromQL L2 nodes that lower to data-model-agnostic shapes here). **One canonical form per plan** — no `WindowedAgg` (use `Window` over `Aggregate`). Heavy-hitter intents are first-class (`AggIntent::TopK`) so heavy-hitter sketches bind directly on the intent rather than on a generic `Sort + Limit` shape; generic `Sort + Limit` survives in `QueryExpr` for non-heavy-hitter cases (e.g. `ORDER BY name LIMIT 10`). Every edge carries a typed `Schema`. Data-model-agnostic — `QueryExpr::Scan` wraps a `Source` sum with `TimeSeries` / `Table` / `Join` variants so the same L3 IR covers ASAPQuery's time-series queries and asap-fusion's tabular queries. | DC `controller/src/algebra/{expr,directory}.rs`; asap-fusion's 3-variant `SubPopulationAnalyticsType` maps to a subset; asap-planner-rs's 9-variant `Statistic` maps to a subset — **Phase 4 splits sketch binding out of planner's current fused L3+L4** |
| 4 | **Sketch algebra + optimizer** | Cost-aware algebraic rewrite rules under deployment constraints. **This is where sketch binding happens** — L4 rules take intent-only L3 (`QueryExpr`) and emit the sketch-bound IR (`SketchExpr`). ~12 rules in DC; a smaller targeted subset in planner; `SketchConfigRule` + `HashModeRule` in fusion. | Core provides the **rule engine driver** + `OptimizerRule` trait + a shared rule library + the sketch-bound IR `core::sketch_algebra::SketchExpr`; deployment models **pick** which rules to enable + supply their own deployment constraints. DC `controller/src/algebra/optimizer.rs` (rules); fusion `src/optimizer/rules/`; planner's `map_statistic_to_precompute_operator` |
| 5 | **Physical Execution Plan** | Assign ops to pipeline stages (edge / gateway / backend / object store); produce the deployment-specific artifact (OpAMP YAML, `streaming_config.yaml`, rewritten DataFusion `LogicalPlan`). **Sketch binding is already committed by L4**; L5 is about stage allocation + emission. | Core provides the **stage allocator framework** + `PhysicalPlanner` trait + the sketch catalogue; deployment models supply their own **topology** (3-stage / 1-stage / 0-stage) + their own **emitter** for the output format. DC `controller/src/algebra/{physical,allocator,plan}.rs`; asap-planner-rs `output/generator.rs`; asap-fusion `src/executor/` |

**The doc's key claim: layers 1–3 are query-language-independent and workload-independent.** That makes them the natural **common core**. Every deployment model reads PromQL (or SQL, or …) the same way, lowers it to the same per-language algebra, and lowers THAT to the same intent-algebra IR (intent only, no sketch binding).

**L4 and L5 also have substantial common infrastructure.** Initially we assumed deployment models owned L4/L5 wholesale; on closer inspection what's actually deployment-model-specific is *which rules fire* (L4) and *what topology + output format* (L5) — not the rule engine, not the allocator, not the sketch catalogue. Those frameworks belong in core. This makes deployment models significantly thinner: each becomes a small crate that picks rules from a shared library, declares a deployment topology, and writes an emitter.

### Sketch binding lives in L4, not L3

A key clarification after cross-checking the three source repos: **L3 is intent-only**. DC's `AggIntent` names *what* to compute (`Quantile(0.99, ε=0.01)`, `Cardinality(δ=0.001)`, …) without committing to a sketch type. Picking KLL vs DDSketch, CMS vs CMS-with-heap, parameter sizes — all of that is L4's job, driven by deployment constraints.

More generally: **L1–L3 lower into a logical representation + `AggIntent`; L4 and L5 choose the concrete execution plan.** That choice is a standard physical-operator selection — `HashJoin` vs `SortMergeJoin` vs `SketchJoin`; `SortAgg` vs `HashAgg` vs `SketchAgg`. "Use a sketch" is one option among several; the same L4 rule framework that picks sketch parameters also picks between sketch and non-sketch operators when a rule is registered for the intent. This keeps the existing 5-layer split intact: nothing about the layering presupposes the output contains a sketch.

Today:
- DC: correctly separated (L3 has `AggIntent`, L4 picks sketch via cost model).
- asap-fusion: correctly separated — `SketchConfigRule` at L4 fills `SketchConfig::NULL` with concrete `CountMinSketch{5,4096}` / `KLL{k=200,m=8}`.
- asap-planner-rs: **L3+L4 fused today**. `map_statistic_to_precompute_operator` jumps from `Statistic` straight to `AggregationType::DatasketchesKLL{k=200}` in one call. **Phase 4 splits this** — `Statistic → AggIntent` at L3, `AggIntent + DeploymentConstraints → AggregationType + SketchParams` at L4.

### Intent vocabulary: DC's `AggIntent` is a superset; deployment models use subsets

DC's pre-cleanup superset had ~25 variants; after L3 normalisation (see §6) it's smaller because language-flavored synonyms (`QuantileOverTime → Window + Quantile`) no longer earn their own intent. The post-cleanup core is 9 (`Count, Sum, Min, Max, Quantile, TopK, Cardinality, Rate, Increase`); the long-term ceiling is bounded by genuinely-distinct operations (stddev, variance, approximate-join-cardinality, …), not by language-flavored synonyms. Planner's 9-variant `Statistic` maps directly: `Topk` keeps its own intent (heavy-hitter sketches like SpaceSaving / CMS-with-heap compute it as a single primitive, so the intent earns L3 visibility). Fusion's 3 variants (`Count, Sum, Quantile`) map directly. Adding a new intent (e.g. stddev) is a core change that deployment models opt into.

### Data-model support: both time-series and tabular

ASAPQuery-backend / DC controller operate on time-series data (metrics + labels + timestamp); asap-fusion operates on tabular data (DataFusion `LogicalPlan` over relations) that may or may not be time-indexed. These two data models differ fundamentally in their leaf shape (`metric + labels + time` vs. `table + columns`), but they share everything above the leaf — filter semantics, aggregation semantics, sketches themselves.

Core handles this with:
- **`QueryExpr::Scan { source: Source, ... }`** where `Source` is a sum type (`TimeSeries`, `Table`, `Join`, …). Deployment models' L1→L2→L3 lowering produces the appropriate variant; L4 rules that care about the data model gate on `source.data_model()`.
- **`AggIntent::requires() -> DataModel`** — each intent variant tags whether it's data-model-agnostic (`Count`, `Sum`, `Min`, `Max`, `Quantile`, `Cardinality`), time-series-only (`Rate`, `Increase` — both carry PromQL counter-reset semantics), or tabular-only (future additions for joins, correlated subqueries).
- **Sketches are data-model-agnostic by construction.** KLL / CMS / HLL / DDSketch ingest a stream of values; that stream can come from a time-series window or a table column, the sketch does not know or care. So `BindKllOnQuantile` and siblings work uniformly across both.

Practically, this means:
- `deployment-model-asapquery` + `deployment-model-asaplifecycle` lower into `QueryExpr` with `Source::TimeSeries` leaves.
- `deployment-model-asapfusion` lowers into `QueryExpr` with `Source::Table` leaves (and, in future, `Source::Join` when it extends to multi-table queries).
- A hypothetical OLAP deployment model that runs approximate queries over tabular data reuses `Source::Table` + the same `AggIntent` subset fusion uses, plus any OLAP-specific intents it adds.

See §6 `core::intent_algebra` for the concrete type sketches.

### Scope: start single-query, grow into workload-aware

The initial implementation can operate on **one query at a time** — L4 picks physical operators per query against per-query constraints, and `CostModel::workload_cost` degenerates to a sum of per-plan costs. This matches today's three source repos (all single-query planners) and is the minimum bar for parity during the migration.

**Workload-awareness is an extension, not a rewrite.** When ≥2 queries are planned together, `workload_cost` credits shared sub-expressions (sketches, precomputed aggregates) so the planner can pick a plan for `q1` that lets `q2` read its output for free. Nothing in the L1–L5 spine changes — only the cost objective widens and the rule engine gains cross-plan visibility. See §6 `core::cost` and §13 future work.

### L2 is mandatory; the tree shape is an evolvable contract

Every deployment model must produce an L2 tree, even when the source language didn't originally come as one. asap-planner-rs's current approach (PromQL pattern catalogue → `IntermediateAggConfig`) skips L2; Phase 4 will reverse-engineer the five PromQL pattern shapes into a `PromqlLogicalPlan` tree so the L1→L2→L3 pipeline is uniform.

A future deployment model whose source semantics genuinely don't fit a tree (e.g. a constraint-based query language) would motivate revisiting the L2 contract at that time. Until then, L2 = per-language tree, mandatory, no elision.

### System I/O contract — what the controller takes in, what it emits

The controller is a **planner**, not an executor. It does not run queries; it decides where each piece of a query runs. The data plane (OTel collectors / ASAPQuery-backend / DataFusion `SessionContext`) runs them.

**System input — what the controller takes in.** A `QueryWorkload` (one or more `QuerySpec`s) plus deployment context (available executors, their capabilities, current SLA targets, telemetry of recent violations). The `QuerySpec` carries the raw query string in its source language (PromQL / SQL / DataFusion / ElasticDSL) and the accuracy / latency / cost target. Workloads can arrive via four entry points (HTTP `POST /plan`, OpAMP capability-miss callback, YAML file for the CLI shell, query-log replay); they all normalise to `QueryWorkload` before L1.

**Workload features that the controller cares about, beyond the queries themselves:**
- Execution model: **batch** (one-shot YAML, query-log replay) vs **streaming** (live pipeline that must keep producing results as data arrives).
- Data input source: time-series scrape, tabular relation, query log replay, etc. Drives `Source` variant choice in L3.
- Reuse opportunity: ≥2 queries planned together → `CostModel::workload_cost` credits shared sub-expressions.

**System output — what the controller emits.** For each registered executor in the deployment, a sub-DAG of the optimized plan plus the configuration that lets that executor run it. Concretely:

| Output | Consumer | Wire shape |
|---|---|---|
| Per-executor sub-DAG assignment | The executor (edge agent / gateway / backend / DataFusion session) | OpAMP `RemoteConfig` (OTel YAML) / `streaming_config.yaml` POST / rewritten `LogicalPlan` |
| Cut-edges between executors | The transport between executors (OTel pipeline, HTTP, sketch-merge / compute-from-raw over the precompute engine, …) | Implied by the per-executor configs; not a separate artifact |
| Plan ID + provenance metadata | The controller's own `PlanStore` for replan / EXPLAIN / observability | JSON / proto |

**Premise behind the staged dispatch.** Two distinct concepts:

- **Stage** (`StageId`) — a categorical *tier* in the data lifecycle (edge / gateway / backend / in-process). The topology declares which stages exist (3-stage / 1-stage / 0-stage). Stages are roles, not instances.
- **Executor** (`Executor`, defined in `core::physical::executor`) — a *concrete runtime instance* that occupies a stage. Carries `id`, `stage: StageId`, `capabilities`, and `address` (OpAMP agent / HTTP endpoint / in-process handle). One stage may have N executors — e.g. a 50-host edge fleet is 50 `Executor`s all with `stage = StageId("edge")`; a singleton backend is one `Executor` at `stage = StageId("backend")`.

Every stage is in principle capable of running the entire query tree — all stages speak the same physical operators. The controller's job is to decide *which stage* runs *which sub-tree / sub-DAG* under the deployment's constraints (memory budget per stage, network bandwidth between stages, sketch backends available at each stage). A "stage assignment" is a colouring of the L4-bound `SketchExpr` DAG by `StageId`, with sketch-merge / data-shipping nodes inserted on the cut edges. L5's `StageAllocator` does the colouring at stage granularity; the per-deployment-model `PhysicalPlanner` then materialises each stage's sub-DAG into one config per `Executor` at that stage (varying only by per-executor connection / identity details). The executor list comes from `DeploymentConstraints::executors()`.

This is what makes the topology a *parameter* rather than an axis of code: the same `SketchExpr` plus a different `TopologyDescriptor` produces edge-only / 1-stage / 3-stage / 0-stage placements without rewriting the plan.

**Symbolic plan vs concrete plan.** L3 `QueryExpr` and L4 `SketchExpr` are symbolic — they describe operations and bindings without committing to *where* anything runs. L5 produces the concrete plan: stage-assigned, executor-targeted, ready to serialize into the executor's configuration format. The split is what lets the controller swap topologies (single-stage backend → three-stage edge/gateway/backend) without re-running L1-L4.

This drives the core/deployment model split:

- **`crates/core/`** owns all 5 layers of **shared infrastructure**: L1-3 end-to-end (parsers + lowering + intent-only IR), plus L4's rule engine driver + rule library + cost-model traits, plus L5's stage-allocator framework + `PhysicalPlanner` trait + sketch catalogue.
- **Each deployment model is a thin crate** that: (1) picks which of core's L4 rules to enable + adds any deployment-model-specific rules, (2) declares its deployment topology (how many stages, where data flows), (3) provides an emitter for its output format. That's usually a few hundred lines, not thousands.

## 4. Principles

### P1. Core owns shared infrastructure across all 5 layers; deployment models own choices

See §3. Core is NOT just types + trait stubs — it ships working parsers + lowering passes (L1-3), a rule engine + rule library + cost-model traits (L4 framework), and a stage allocator + physical-plan framework + sketch catalogue (L5 framework). What deployment models plug in is **which rules fire** (picking from core's library + adding their own), **deployment topology** (how many stages; DC=3, query=1, fusion=0), and an **emitter** for the output format. A deployment model that accepts all core's default L4 rules and uses `core::physical::single_stage_topology` is maybe 200 lines of code.

### P2. Core has no I/O

No HTTP, no OpAMP, no YAML, no Prometheus scrape, no `tokio::spawn`. Pure algorithms over in-memory data. This is what makes deployment models unit-testable without running the runtime.

### P3. Runtime is a thin binary, deployment models are libraries

The `asap-controller` binary is assembled from: runtime (HTTP/OpAMP/replanner/store) + N deployment model crates registered as plugins. Swapping deployment models is a build-time feature flag or a runtime registry entry. No deployment model may reach directly into another.

### P4. One input boundary, one output boundary

**Input**: everything that enters the controller (HTTP `QuerySpec`, Prometheus query log replay, YAML workload, capability-miss callback) normalizes into a single `QueryWorkload` type in core — a collection of `QuerySpec`s each feeding L1→L2→L3.

**Output**: L5 emitters (OpAMP `RemoteConfig`, backend `StreamingConfig` POST, one-shot YAML file, rewritten DataFusion `LogicalPlan`) all implement a `PlanEmitter` trait. Deployment models supply emitter implementations; core doesn't know which emitters exist.

This is the "extension point for future deployment models" — a new deployment model adds an L4 rule set + an L5 emitter and registers them.

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
│   ├── deployment-models/             # per-deployment-model design notes
│   └── adr/                   # architecture decision records
├── proto/
│   ├── asap_control.proto     # Plan IR + QueryWorkload on the wire
│   └── opamp.proto            # vendored from DataCollector
├── crates/
│   ├── core/                  # Shared infrastructure across all 5 layers; no I/O
│   │   ├── query_language/    # L1: per-language parsers — promql/, sql/, datafusion/, elasticdsl/
│   │   ├── logical_plan/      # L2: per-language algebra tree (Aggregate/Window/Filter/…)
│   │   ├── intent_algebra/    # L3 IR: QueryExpr + AggIntent + Schema/HasSchema (intent only)
│   │   ├── sketch_algebra/    # L4 IR: SketchExpr (sketch-bound — kind + params committed)
│   │   ├── lower/             # L1→L2→L3 lowering passes (one entry per language)
│   │   ├── optimizer/         # L4 framework — produces SketchExpr from QueryExpr:
│   │   │   ├── engine/        #   rule driver — fixed-point iteration, cycle detection, priority
│   │   │   ├── trait/         #   OptimizerRule + RuleCategory (PushDown / Fusion / Elim / Bind)
│   │   │   ├── rules/         #   shared rule library (e.g. sketch-binding rules; stream-vs-batch picker)
│   │   │   └── cost/          #   CostModel trait + generic impls (memory budget, accuracy degradation)
│   │   ├── physical/          # L5 framework:
│   │   │   ├── planner_trait/ #   PhysicalPlanner trait
│   │   │   ├── stage_allocator/ # generic topology-driven allocator
│   │   │   ├── topology/      #   Topology descriptor types (edge/gateway/backend, single, zero)
│   │   │   ├── executor/      #   Executor type — concrete runtime instance occupying a stage
│   │   │   └── sketch_catalog/ # candidate sketch types + parameter constraints
│   │   ├── pipeline/          # orchestrates L1→L2→L3→L4→L5, parameterized on deployment model
│   │   ├── workload/          # QueryWorkload wrapper around QuerySpecs
│   │   ├── emit/              # PlanEmitter trait — implemented by deployment model L5
│   │   ├── registry/          # DeploymentModelRegistry, DeploymentModelId
│   │   └── telemetry/         # tracing macros, metric names (no exporter)
│   ├── runtime/               # service skeleton — HTTP, OpAMP, replanner
│   │   ├── http/              # axum surface — /plan, /replan, /metrics, /status
│   │   ├── opamp/             # WebSocket OpAMP server
│   │   ├── monitor/           # Scraper, Thresholds, Violation
│   │   ├── replan/            # Replanner — SLA + expiry triggers
│   │   ├── store/             # PlanStore, WorkloadStore
│   │   └── backend_client/    # HTTP client (pushes to ASAPQuery-backend, etc.)
│   ├── deployment-model-asaplifecycle/    # thin — picks rules, 3-stage topology, OTel/backend emitters
│   │   ├── rules.rs           # L4 rule selection + DC-specific rules (stage-aware push-down)
│   │   ├── topology.rs        # 3-stage: edge / gateway / backend
│   │   ├── cost.rs            # deployment-model-specific cost impls — delta / online / pareto / tco
│   │   └── emit/              # OpAmpRemoteConfig + AsapqueryBackendConfig emitters
│   ├── deployment-model-asapquery/        # thin — rules, 1-stage topology, YAML emitters + query-log input
│   │   ├── rules.rs           # L4 rule selection + sketch-binding rule (split from map_statistic_*)
│   │   ├── topology.rs        # 1-stage: backend-only
│   │   ├── emit/              # StreamingConfig.yaml + InferenceConfig.yaml
│   │   ├── query_log/         # extra L1 input: Prometheus query-log replay
│   │   └── schema/            # PromQLSchema discovery from Prometheus (feeds L1)
│   ├── deployment-model-asapfusion/       # thin — DF-flavored rules, 0-stage, in-process emit
│   │   ├── rules.rs           # L4 rules for DataFusion LogicalPlan (sketch-aware rewrites)
│   │   ├── topology.rs        # 0-stage: in-process
│   │   ├── emit/              # rewritten DataFusion LogicalPlan
│   │   ├── executor/          # DataFusion SessionContext wrapper (library-mode execution)
│   │   └── sketch_support/    # asap_sketchlib-backed rewrites
│   ├── control-proto/         # generated from proto/ (tonic/prost)
│   └── testing/               # test fixtures + harness shared across deployment models
└── bin/
    ├── asap-controller/       # long-running service with all deployment models
    │   └── main.rs            # axum + OpAMP + replanner + registered deployment models
    ├── asap-query/             # one-shot CLI for deployment-model-asapquery (what asap-planner-rs is today)
    │   └── main.rs            # clap — read workload YAML, emit two YAMLs
    ├── asap-lifecycle/        # OPTIONAL: standalone service with only deployment-model-asaplifecycle
    │   └── main.rs            # slimmer image — no DataFusion, no query YAML emitters
    └── asap-fusion-bench/     # OPTIONAL: benchmark harness over deployment-model-asapfusion
        └── main.rs            # criterion entry; used by researchers
```

**Per-deployment-model standalone binaries** are first-class. Each `bin/<name>/` is a thin shell that `use`s only the deployment model crates it needs — so `bin/asap-lifecycle/` doesn't pull `datafusion` into its dep tree, and `bin/asap-fusion-bench/` doesn't pull `axum`/`opamp`. Feature flags on the workspace root let you `cargo build -p asap-lifecycle` and get a minimal binary.

### Why three deployment model crates today, not one monolithic `deployment-models/`

Each deployment model has a different problem shape:

| | **lifecycle** | **query** | **fusion** |
|---|---|---|---|
| Input | workload + live metrics | workload YAML / query log | DataFusion `LogicalPlan` |
| Decision unit | end-to-end staged pipeline | per-aggregation YAML | per-operator rewrite |
| Output | OpAMP OTel config + `StreamingConfig` YAML | `streaming_config.yaml` + `inference_config.yaml` | rewritten `LogicalPlan` |
| Trigger | QuerySpec, SLA violation, expiry | one-shot CLI invocation | a DataFusion session constructing a query |
| Cost model | accuracy × latency × $ staged | single-query accuracy/latency | operator selectivity / sketch feasibility |

Mashing them into one crate means the union of all their dependencies (sqlparser + promql-parser + DataFusion + OpAMP proto + Prometheus client) bleeds into every downstream user. Separating them means a user who only needs `deployment-model-asapfusion` (an offline query-optimization benchmark, say) can depend on it without pulling OpAMP.

## 6. Core crate details (layers 1–3 + driver)

Core is not a trait-stubs library. It ships real L1/L2/L3 code lifted from DC's `controller/src/query_parser/` + `controller/src/algebra/` and exposes a small set of traits for L4/L5 plugin points.

### `core::query_language` — Layer 1

Per-language parsers, one module each:

- `promql/` — wraps `promql-parser`. Input: `&str` PromQL. Output: `PromqlAst`.
- `sql/` — wraps `sqlparser`. Input: `&str` SQL. Output: `SqlAst`.
- `datafusion/` — wraps DataFusion's own parser. Output: `DfAst`.
- `elasticdsl/` — wraps `elastic_dsl_utilities`. Output: `EsAst`.

Each returns a language-flavored AST type. No sketch awareness.

### `core::logical_plan` — Layer 2

A **per-language** algebra tree — one `enum LogicalPlan` per language. Preserves language-specific semantics (PromQL instant vs range vector, SQL window frames, Elastic buckets) that would be lossy to collapse this early. Types are symmetric: `Aggregate { AggFunc }`, `Window`, `Filter`, `Sort`, `Limit`. No sketch names yet.

### `core::intent_algebra` — Layer 3

The language- and deployment-independent IR. **Pure intent at this layer**: no language-specific operators (no `HistogramQuantile`, no `PromQLSubquery` — those are L2 PromQL nodes), no sketch types, no sketch parameters, no physical operator choice. Data-model-agnostic: supports both **time-series** inputs (ASAPQuery-backend, DC lifecycle) and **tabular** inputs (asap-fusion, future OLAP deployment models) via a `Source` sum type inside `QueryExpr::Scan`.

#### Design rules for L3

1. **One canonical form per plan.** No redundant variants whose semantics decompose into other variants. `Window` over `Aggregate` is the canonical windowed-aggregate shape; there is no separate `WindowedAgg`. This keeps L4 rule matching unambiguous (a rule fires on one shape, not on N synonyms). The exception is when an "intent" is its own physical primitive: heavy-hitter top-k is served by sketches (SpaceSaving, CMS-with-heap) as a single operation, so `AggIntent::TopK` is a first-class intent at L3 — distinct from the generic `Sort + Limit` operator pair, which still appears in `QueryExpr` for non-heavy-hitter cases (e.g. `ORDER BY name LIMIT 10`).
2. **Language-orthogonal.** No PromQL-shaped or SQL-shaped operators leak into L3. `HistogramQuantile` is a PromQL artifact (it consumes Prometheus's specific bucketed-histogram exposition format and produces a quantile from buckets) — it lives in `core::logical_plan::promql` (L2) and lowers to bucket reads + a regular `Quantile` intent over those bucket counts. `PromQLSubquery` (`<expr>[range:resolution]`) is a *driver* construct — it asks the engine to evaluate the inner expression at N timestamps — and is expanded by the PromQL L1→L2 lowering into a set of independent queries, not preserved as an L3 DAG node.
3. **Intent at L3, sketch at L4.** L3 carries `AggIntent` ("compute a quantile to ε=0.01 accuracy"). The choice between `HashAgg` / `SortAgg` / `SketchAgg(KLL{k=200})` is made by L4 cost-aware rules, not encoded in L3.
4. **One physical-choice node per logical operator.** L3 has `Aggregate` (logical) and `Join` (logical). L4 produces the sketch-bound physical alternatives (`SketchAgg`, `SketchJoin`, `SketchSubtract`, `SketchDelete`, `SketchEstimate`, `SketchMerge`) into an extended IR — see "L4 sketch-bound IR" below. Mixing logical and physical at L3 (the previous draft did this with `SketchAgg` / `JoinSketch` at L3) creates ambiguity about which layer owns which decision.
5. **DAG, not tree.** Edges between nodes carry typed schemas (see "Schema flow" below). A node's output schema is a function of its inputs and parameters and is verifiable independently of the surrounding context. The shape is a DAG, not a tree, because **a producer node can have multiple parents that share its precomputed intermediate state** — instead of recomputing the same sub-expression once per consumer, the consumers fan in to a single node and read its output. Three sources of fan-in:
    1. **Explicit, in-query.** SQL CTEs (`WITH name AS (expr) SELECT ... FROM name JOIN name AS n2 ON ...`) and PromQL recording rules name a sub-expression and reference it N times; each reference becomes a `QueryExpr::Ref(name)` parent of the named producer.
    2. **L4 reuse rules.** When two queries planned together both need (e.g.) a p99 quantile of the same series, the optimizer can introduce a shared sketch node whose output feeds both — even though neither query author wrote a CTE.
    3. **L4 stage / shard structure.** A pre-aggregate computed once on the edge can feed multiple downstream gateway-stage operators.

    `CostModel::workload_cost` credits a shared producer's build cost once across all consumers, which is what makes reuse a Pareto win. Tree IRs lose this — they have to duplicate the producer for every consumer.

```rust
pub enum QueryExpr {
    // ── Base relations ────────────────────────────────────────────────────
    /// A metric stream / table / join. Outermost leaf.
    Scan { source: Source, predicates: Vec<Predicate> },
    /// Reference to a CTE / let-binding by name; resolved at plan time.
    Ref(String),

    // ── Filtering & projection ────────────────────────────────────────────
    /// σ — row-level filter (WHERE / PromQL label matchers).
    Filter  { child: Box<QueryExpr>, pred: Predicate },
    /// π — column projection (SELECT list).
    Project { child: Box<QueryExpr>, cols: Vec<ProjectItem> },

    // ── Aggregation (logical, intent-only) ────────────────────────────────
    /// γ + α — GROUP BY + aggregate intents, with optional HAVING.
    /// `aggs` carry `AggIntent`; concrete sketch / non-sketch operator
    /// is chosen by L4 and lives in the L4-extended IR (`SketchExpr`).
    Aggregate { child: Box<QueryExpr>, by: Vec<GroupKey>,
                aggs: Vec<AggIntent>, having: Option<Predicate> },

    // ── Time / streaming windows ──────────────────────────────────────────
    /// ψ — tumbling / sliding / session window over the time axis. Defines
    /// the lifecycle (flush / reset bounds) of any aggregate in its sub-tree / sub-DAG.
    /// PromQL `[5m]` and streaming windows lower here. SQL `OVER (...)`
    /// analytic frames are a different node — see `WindowFunc` below.
    Window { child: Box<QueryExpr>, kind: WindowKind,
             size: Duration, slide: Option<Duration> },

    // ── Distributed-execution structure ───────────────────────────────────
    /// Partition the stream by key tuple (`GROUP BY` / PromQL `by (dims)`).
    Partition { child: Box<QueryExpr>, keys: PartitionKeys },
    /// δ — SQL `DISTINCT` / row deduplication on `cols`.
    Distinct  { child: Box<QueryExpr>, cols: Vec<ColumnRef> },
    /// ⊕ — union of sub-results from independent stages or shards (the
    /// exact-merge case). Sketch unions are a separate node in `SketchExpr`
    /// because they carry sketch-family / params type constraints.
    Merge     { children: Vec<QueryExpr> },

    // ── Joins (logical) ───────────────────────────────────────────────────
    /// Logical join. L4 picks the physical alternative — `HashJoin` /
    /// `SortMergeJoin` / `SketchJoin` (e.g. KMV / theta-sketch / join-sample)
    /// — based on selectivity, memory budget, and accuracy target. The
    /// sketch-aware variant lives in `SketchExpr::SketchJoin`.
    Join { kind: JoinKind, left: Box<QueryExpr>, right: Box<QueryExpr>,
           pred: Option<Predicate> },

    // ── Set operators ─────────────────────────────────────────────────────
    /// UNION / INTERSECT / EXCEPT, with or without ALL.
    SetOp { kind: SetOpKind, all: bool,
            left: Box<QueryExpr>, right: Box<QueryExpr> },

    // ── Ordering & limiting ───────────────────────────────────────────────
    /// Generic order-by — survives L3 for non-heavy-hitter cases
    /// (`ORDER BY name LIMIT 10`, `ORDER BY ts DESC LIMIT 1`).
    Sort  { child: Box<QueryExpr>, keys: Vec<SortKey> },
    /// `LIMIT n OFFSET k`. The heavy-hitter shape (`ORDER BY count DESC
    /// LIMIT k`, PromQL `topk(k, …)`) is recognised at L1→L2→L3 lowering
    /// and produces `AggIntent::TopK` rather than generic `Sort + Limit`,
    /// so heavy-hitter sketches (SpaceSaving, CMS-with-heap) bind on the
    /// intent. Generic `Sort + Limit` flows through unchanged.
    Limit { child: Box<QueryExpr>, n: u64, offset: u64 },

    // ── Subquery / CTE ────────────────────────────────────────────────────
    Subquery   { child: Box<QueryExpr>, alias: String },
    /// SQL `WITH name AS (expr) IN body`; lowering target for PromQL
    /// recording-rule bindings.
    LetBinding { name: String, expr: Box<QueryExpr>, body: Box<QueryExpr> },

    // ── Analytic (OVER) window functions ──────────────────────────────────
    /// SQL `OVER (PARTITION BY ... ORDER BY ... ROWS BETWEEN ...)`.
    /// Distinct from `Window` above — that is a streaming/tumbling window
    /// over the time axis; this is an analytic frame over already-grouped rows.
    WindowFunc { child: Box<QueryExpr>, func: WindowFuncKind,
                 partition_by: Vec<GroupKey>, order_by: Vec<SortKey>,
                 frame: Option<WindowFrame> },

    // ── Binary composition ────────────────────────────────────────────────
    /// Arithmetic / comparison / boolean composition between two relational
    /// sub-expressions (PromQL binary ops including `and`/`or`/`unless`,
    /// SQL boolean composition).
    BinaryOp { op: BinaryOpKind, lhs: Box<QueryExpr>, rhs: Box<QueryExpr>,
               vector_match: Option<VectorMatch> },
}

pub enum WindowKind { Tumbling, Sliding, Session }

pub enum Source {
    /// Time-series input — deployment-model-asapquery / deployment-model-asaplifecycle shape.
    TimeSeries { metric: MetricRef, time: TimeRange, labels: LabelFilter },
    /// Tabular input — deployment-model-asapfusion / future-OLAP shape.
    Table      { table_ref: TableRef, columns: Vec<ColumnRef> },
    /// Join over Sources composes leaf shapes recursively.
    Join       { left: Box<Source>, right: Box<Source>, on: JoinKey },
    // Future: WindowedStream, Subquery — added by deployment models that need them.
}

pub enum DataModel { TimeSeries, Tabular, Any }

impl Source {
    pub fn data_model(&self) -> DataModel { /* … */ }
    /// Output schema produced by this leaf (see "Schema flow").
    pub fn schema(&self, catalog: &SchemaCatalog) -> Schema { /* … */ }
}
```

#### What was removed during cleanup, and why

| Removed | Reason | Replacement |
|---|---|---|
| `TopK { k, by }` *as a `QueryExpr` node* | A `QueryExpr`-level top-k operator collapses two distinct concepts: (a) the *intent* of "compute heavy hitters", which has its own sketch primitive, and (b) the generic operator pair `Sort + Limit`, which doesn't. Splitting them puts heavy-hitter logic at the right level | Heavy-hitter intent → `AggIntent::TopK` (L3, recognised by L1→L2→L3 lowering of `ORDER BY count DESC LIMIT k`, PromQL `topk(k, …)`); generic ordering+limit → `Sort + Limit` `QueryExpr` nodes (unchanged). Both retained — they describe different things |
| `WindowedAgg { intent, window }` | Equivalent to `Window` over `Aggregate`; redundant. Tumbling/sliding kind moves onto `Window::kind` | `Window { kind: …, … } → Aggregate { aggs: [intent] }` |
| `SketchAgg { intent, col }` | Sketch-bound; L3 must be intent-only | `Aggregate { aggs: [intent] }` at L3; L4 emits `SketchExpr::SketchAgg` |
| `JoinSketch { outer, inner, key }` | Sketch-bound physical alternative; L3 must be intent-only. Several papers describe sketch-of-join (KMV, theta, join-sample) — picking one is an L4 cost decision, not an L3 surface choice | `Join { … }` at L3; L4 emits `SketchExpr::SketchJoin` when a `Bind*OnJoin` rule fires |
| `HistogramQuantile { phi }` | PromQL artifact — consumes Prometheus's specific bucketed-histogram format. Language-specific | Lowers in PromQL L1→L2 to bucket reads + `Aggregate { aggs: [Quantile{q: phi, …}] }` over the bucket counts |
| `PromQLSubquery { range, resolution }` | Driver construct — asks the engine to evaluate the inner expression at N timestamps; not a single DAG node | Expanded by PromQL L1→L2 lowering into a set of independent `QueryExpr` instances, not preserved at L3 |
| `Dedup { col }` | Single-column-only spelling of SQL `DISTINCT` | Renamed to `Distinct { cols }`, generalised to N columns |

#### Schema flow — every L3 edge carries a typed schema

Every node has a derivable output schema given its input schemas and parameters. The DAG is type-checked: a `Filter` whose predicate references a column not in its child's output schema fails at plan time.

```rust
pub struct Schema {
    pub fields:      Vec<Field>,
    /// Index into `fields` for the time axis, if any. PromQL leaves carry one;
    /// SQL leaves may or may not.
    pub time_index:  Option<usize>,
    /// Optional metadata for reuse-aware planning. Each inner `Vec<usize>` is
    /// a set of column indices that together uniquely identify rows; the outer
    /// `Vec` allows multiple unique-key sets (e.g. primary key + another unique
    /// constraint).
    ///
    /// **Populated by**: the per-node input/output spec (e.g. `Aggregate { by, .. }`
    /// emits `unique_keys = [by]`; `Distinct { cols }` adds `cols`; `Project`
    /// carries forward the retained columns; most other nodes pass through).
    ///
    /// **Consumed by**: `CostModel::workload_cost` only — the reuse-aware path
    /// that credits shared sub-expressions across multiple queries. Single-query
    /// plans, the `Bind*` rules, push-down rules, and L5 emitters do not read
    /// this field. If the reuse path is deferred (see §13 future work), this
    /// field is dead weight; it lives here so the metadata is available the
    /// moment workload-aware planning lands without requiring an L3-wide
    /// schema change.
    pub unique_keys: Vec<Vec<usize>>,
}

pub struct Field {
    pub name:     String,
    pub dtype:    DataType,    // Int64 / Float64 / Utf8 / Map<Utf8,Utf8> / …
    pub nullable: bool,
}

pub trait HasSchema {
    fn input_schemas(&self) -> Vec<&Schema>;
    fn output_schema(&self, inputs: &[&Schema], cat: &SchemaCatalog) -> Schema;
}
```

Per-node input/output spec — the stable contract for L3 nodes (full implementation in `core/src/intent_algebra/schema.rs`). Each row reads independently: every column position, type, and constraint is named explicitly rather than carried by a shorthand like `S` or `L` / `R`.

| Node | Input schemas | Output schema |
|---|---|---|
| `Scan { source }` | none — leaf node | `source.schema(catalog)` — derived from the source's catalog metadata (TimeSeries metric labels + value + timestamp; or Table columns from `information_schema`; or recursive `Source::Join`) |
| `Ref(name)` | none — pointer node | the output schema of the `LetBinding` whose `name` matches; resolved at plan time |
| `Filter { pred }` | one input schema (the child's output) | the child's input schema unchanged — `Filter` is a row-level refinement; no columns added, removed, or re-typed |
| `Project { cols }` | one input schema | the input schema projected to `cols` — fields filtered and reordered to match `cols`; `time_index` and `unique_keys` carried over for retained columns |
| `Aggregate { by, aggs }` | one input schema | the `by` columns (carried verbatim from input) followed by one new column per entry in `aggs`, each named and typed by `AggIntent::output_type(input_field)`; `unique_keys = [by]` |
| `Window { kind, size, slide }` | one input schema, **must** contain a `time_index` field | the input schema extended with synthetic `window_id` and `window_start` / `window_end` metadata fields |
| `Partition { keys }` | one input schema | the input schema unchanged — logical-only marker; carries a sharding hint for L5's stage allocator |
| `Distinct { cols }` | one input schema | the input schema with `unique_keys` tightened to include `cols`; field types unchanged |
| `Merge` | N input schemas, all union-compatible (same field names, same types, same nullability, same `time_index` position if any) | the first input's schema (representative; checked union-compatible with the rest) |
| `Join { kind, pred }` | two input schemas — left and right children | the concatenation of left's fields and right's fields, minus columns that USING / NATURAL deduplicates; nullability widened on the OUTER side for outer joins |
| `SetOp { kind, all }` | two input schemas, must be union-compatible (as for `Merge`) | the left input's schema |
| `Sort { keys }` | one input schema; every field referenced in `keys` must be present in it | the input schema unchanged |
| `Limit { n, offset }` | one input schema | the input schema unchanged |
| `Subquery { alias }` | one input schema | the input schema with `alias` applied as the table alias to all field names |
| `LetBinding { name, expr, body }` | `expr` produces an intermediate schema bound to `name`; `body` consumes it (and any other in-scope bindings) | the `body`'s output schema |
| `WindowFunc { func, partition_by, order_by, frame }` | one input schema; every field in `partition_by` / `order_by` must be present | the input schema extended with one new column carrying the analytic-function output (named after `func`, typed per `func`) |
| `BinaryOp { op, vector_match }` | two input schemas — left and right operands. PromQL vector-match constraints (`on`/`ignoring` + `group_left`/`group_right`) govern label-set compatibility | for arithmetic/comparison `op`: the left input's schema with the value column re-typed to the result of `op`; for boolean `op` (`and`, `or`, `unless`): the left input's schema with a boolean value column |

#### DAG schema, DB schema, sketch catalog — three distinct metadata sources

These three are sometimes conflated and shouldn't be. Only the first two are *schemas* (descriptions of stream / table shape); the sketch catalog is a *registry* of available primitives, not a description of a stream:

| Source | Where it lives | What it describes | Who reads it |
|---|---|---|---|
| **DAG schema** | On every edge of the L3 / L4 / L5 DAG (`Schema` above) | Columns + types flowing between operators | L4 rules (selectivity estimation, push-down legality), L5 emitter |
| **DB / source schema** | The query target (Prometheus TSDB metric metadata, SQL `information_schema`, DataFusion catalog) | What metrics / tables / columns exist in the data plane, with their types and indexing | `core::lower::*` to resolve names during L1→L2; exposed through a `SchemaCatalog` interface |
| **Sketch catalog** | `core::physical::sketch_catalog` (built at startup; static) | What sketches the runtime can build; what intents each one serves; mergeability, accuracy / confidence guarantees, supported aggregation keys, parameter ranges | L4 binding rules to choose a sketch for an `AggIntent`; L5 to instantiate the sketch |

L1→L2 lowering reads the **DB schema** to resolve symbols. L3 onward, every edge carries a **DAG schema** that is type-checked locally. L4 binding rules consult the **sketch catalog** to map an intent to a concrete sketch under the deployment's constraints. They are three separate inputs to three distinct decisions.

#### `AggIntent` — what to compute, not how

```rust
pub enum AggIntent {
    // Data-model-agnostic
    Count       { accuracy: AccuracyTarget },
    Sum,
    Min, Max,
    Quantile    { q: f64, accuracy: AccuracyTarget },
    /// Heavy-hitter top-k. Distinct from generic `Sort + Limit` because a
    /// dedicated sketch primitive (SpaceSaving, CMS-with-heap, Misra-Gries)
    /// computes it as a single operation. L1→L2→L3 lowering produces this
    /// when it recognises a heavy-hitter shape (`ORDER BY count DESC LIMIT k`,
    /// PromQL `topk(k, …)`); other ordering+limit cases stay as
    /// `QueryExpr::Sort + QueryExpr::Limit`.
    TopK        { k: usize, by: Vec<ColumnRef>, accuracy: AccuracyTarget },
    Cardinality { accuracy: AccuracyTarget },

    // Time-series streaming derivatives — specific operations, not just
    // "Sum / Count over a Window". `Rate` is the per-second average derivative
    // computed with PromQL's counter-reset adjustment, not a generic windowed
    // mean. Kept distinct because (a) they have counter-reset semantics that
    // exact `Sum` does not, and (b) sketch backends specialised for
    // derivatives (e.g. delta-set aggregator) bind on these intents directly.
    Rate     { window: Duration },
    Increase { window: Duration },

    // Tabular / OLAP — added as deployment models demand
    // CorrelatedSubqueryCount { … }, ApproxJoinCardinality { … },
}

impl AggIntent {
    /// Which data-model this intent semantically requires. L4 rules
    /// consult this to skip non-applicable intents (e.g. `Rate` over
    /// a `Source::Table` is nonsense).
    pub fn requires(&self) -> DataModel { /* … */ }
    /// Output column type — used by L3 schema derivation for `Aggregate`.
    pub fn output_type(&self, input: &Field) -> DataType { /* … */ }
    /// Which sketch families in the catalog can serve this intent.
    /// Read by L4 binding rules.
    pub fn candidate_sketches(&self) -> &'static [SketchKind] { /* … */ }
}
```

**Why `TopK` *is* an intent (and `Sort + Limit` is not collapsed into it).** Heavy-hitter top-k has a dedicated sketch primitive — SpaceSaving / CMS-with-heap / Misra-Gries compute it in a single pass with sub-linear memory. L4 binding rules want to fire on the *intent* "give me the top-k frequent items" rather than on a syntactic shape, the same argument that makes `Quantile` an intent rather than a `Sort` + "pick the φ-th element" pattern. So `AggIntent::TopK` lives at L3. Generic `QueryExpr::Sort + QueryExpr::Limit` *also* survives, because not every order-by-limit query is heavy-hitter (`ORDER BY name LIMIT 10`, `ORDER BY ts DESC LIMIT 1`); these have no sketch alternative and stay as generic operators. The canonical-form invariant is preserved because L1→L2→L3 lowering picks one or the other deterministically based on whether it recognises a heavy-hitter pattern.

**Why no `QuantileOverTime` intent?** It duplicated `Quantile` over a `Window`. The window — its kind, its size, its slide — is fully captured by the surrounding `Window { … }` node; the quantile *operation* is the same regardless of whether the input was a windowed time-series or a row-grouped table. PromQL's `quantile_over_time(0.99, m[5m])` lowers cleanly to `Window{size=5m} → Aggregate{aggs:[Quantile{q=0.99}]}`. One intent halves the L4 rule surface (one bind rule per operation, not per language-flavor of an operation).

**Why `Rate` and `Increase` survive that argument.** They are not "Sum / Count over a Window with a different name" — they include PromQL's counter-reset adjustment, which is a non-trivial transformation an exact `Sum` does not perform. They earn distinct intent variants because they parameterise different physical operators (delta-set aggregators bind on these intents directly). If a non-PromQL streaming language has the same notion (e.g. SQL `RATE() OVER (RANGE)`), it lowers to the same intent — the intent vocabulary names the operation, not the language.

### `core::sketch_algebra` — Layer 4 IR (`SketchExpr`)

L4 binding rules consume L3 `QueryExpr` (in `core::intent_algebra`) and produce `SketchExpr` (in `core::sketch_algebra`). This is the IR L5 emitters consume. The two-IR split — intent-only L3 (`QueryExpr`) and sketch-bound L4 (`SketchExpr`), in two separate modules — gives L4 rule application a clean type signature: `fn apply(&QueryExpr, &Constraints) -> Option<SketchExpr>`, and the boundary cannot be silently violated.

```rust
pub enum SketchExpr {
    /// Any logical L3 node passes through unchanged when no L4 rule rewrote
    /// it — a `Filter` doesn't need a sketch counterpart.
    Logical(QueryExpr),

    /// Sketch aggregation. L4 picked the sketch type and parameters from the
    /// catalog given the `AggIntent` and `DeploymentConstraints`.
    SketchAgg {
        child:  Box<SketchExpr>,
        sketch: SketchKind,         // Kll, Cms, Hll, DDSketch, CmsWithHeap, …
        params: SketchParams,       // catalog-validated
        col:    ColumnRef,
        by:     Vec<GroupKey>,
    },

    /// Sketch-aware join (KMV / theta-sketch for join cardinality;
    /// join-sample for join sampling). Emitted only when a `Bind*OnJoin`
    /// rule fires — L3 always presents the logical `Join` for L4 to choose.
    SketchJoin {
        outer:  Box<SketchExpr>,
        inner:  Box<SketchExpr>,
        key:    ColumnRef,
        sketch: SketchKind,
        params: SketchParams,
    },

    /// Subtract one sketch from another. Valid only for sketches with a
    /// linear-inverse property (CMS, theta, count-based). Lets the planner
    /// compute "all-A minus all-B" cardinality / count without re-scanning.
    SketchSubtract { left: Box<SketchExpr>, right: Box<SketchExpr> },

    /// Delete a key from a sketch (CMS update with -1, deletable Bloom
    /// filter, …). Valid only for deletion-supporting sketches.
    SketchDelete { sketch_input: Box<SketchExpr>, key: ColumnRef },

    /// Read out a query result from a built sketch. Inverse of `SketchAgg`.
    /// `query` says what to extract — quantile φ, count for key k, cardinality.
    SketchEstimate { sketch_input: Box<SketchExpr>, query: SketchQuery },

    /// ⊕ — union of sketches across stages / shards. Distinct from L3
    /// `Merge` because sketch union has type constraints (same family,
    /// same params). L5 stage allocator emits this when distributing.
    SketchMerge { children: Vec<SketchExpr> },
}
```

The optimizer's job is to selectively replace logical aggregates / joins with their sketch-bound variants when a binding rule fires; everything else stays inside `SketchExpr::Logical(…)`.

#### Per-node input/output spec for `SketchExpr`

L4 introduces a new field type into `Schema`:

```rust
pub enum DataType {
    // … the L3 types (Int64, Float64, Utf8, Map<…>, …) …
    /// Sketch state. Carries the sketch family + params so the type system
    /// rejects merges of incompatible sketches at plan time.
    Sketch(SketchKind, SketchParams),
}
```

A "sketch-state schema" is a regular `Schema` whose value-bearing field has a `DataType::Sketch(...)` dtype. Reading rules:

| Node | Input schemas | Output schema |
|---|---|---|
| `Logical(qe)` | whatever the inner L3 node `qe` consumes (per the L3 table above) | whatever `qe` produces — straight pass-through |
| `SketchAgg { child, sketch, params, col, by }` | one input schema; must contain `col` (the column being summarised) and every field referenced in `by` (group keys) | the `by` columns carried over verbatim, followed by one synthetic field of dtype `Sketch(sketch, params)` carrying the partial sketch state per group; `unique_keys = [by]` |
| `SketchJoin { outer, inner, key, sketch, params }` | two input schemas (outer + inner); both must contain `key` with compatible types | one field of dtype `Sketch(sketch, params)` carrying the join-cardinality / join-sample state — read out by a downstream `SketchEstimate` |
| `SketchSubtract { left, right }` | two input schemas, each with exactly one `Sketch(s, p)` field; **`s` and `p` must match** between the two inputs (catalog rejects mismatches at plan time); the `s` family must have `subtractable = true` in the catalog | one `Sketch(s, p)` field carrying the subtracted state (same family + params as inputs) |
| `SketchDelete { sketch_input, key }` | one input schema with a `Sketch(s, p)` field whose catalog entry has `deletable = true`; plus the `key` column to delete | the input schema unchanged in type — sketch state is mutated logically (the key's contribution removed) but the field's `(sketch, params)` signature is preserved |
| `SketchEstimate { sketch_input, query }` | one input schema with a `Sketch(s, p)` field; `query` (e.g. `Quantile(φ)`, `PointCount(k)`, `Cardinality`) must appear in the catalog entry's `supported_intents` for `s` | a regular row-shaped schema carrying the answer — `Float64` for quantile, `Int64` for count / cardinality, an array of `(key, count)` for top-k. The `Sketch(...)` field type does *not* propagate downstream of an Estimate |
| `SketchMerge { children }` | N input schemas, each with a `Sketch(s, p)` field; **all N must agree on `(s, p)`**; the `s` family must have `mergeable = true` in the catalog | one `Sketch(s, p)` field carrying the unioned state (same family + params as inputs) |

Two type-system invariants make L4 robust:

1. **Sketch-family mismatch is a plan-time error.** `SketchSubtract` over `Sketch(KLL, …)` and `Sketch(CMS, …)` fails type-checking before L5 ever sees it.
2. **Catalog capability flags gate which nodes can fire.** `SketchSubtract` requires `subtractable`, `SketchDelete` requires `deletable`, `SketchMerge` requires `mergeable`. The catalog (see §6 `core::physical::sketch_catalog`) is the single source of truth for these flags; binding rules consult it before producing the node.

**Why a `Source` sum instead of two parallel `QueryExpr` trees:** most `QueryExpr` nodes (`Filter`, `Aggregate`) are data-model-agnostic — filter semantics are the same whether the input is a time-series window or a table scan. Only the leaf `Scan` differs. Keeping one tree with a polymorphic leaf means L4 rules like `BindKllOnQuantile` work uniformly across both data models; rules that care about data-model specifics (stage-aware push-down for TS; join-selectivity for tabular) gate on `source.data_model()` + `intent.requires()`.

**Sketches are data-model-agnostic by construction.** KLL / CMS / HLL / DDSketch ingest a stream of values. That stream can come from a time-series window (`Source::TimeSeries`) or a table column (`Source::Table`); the sketch doesn't know or care. So `BindKllOnQuantile` works identically regardless of `Source`.

### `core::lower` — L1 → L2 → L3 passes

One pass per language, each producing the same `intent_algebra::QueryExpr`:

```rust
pub fn lower_promql(ast: PromqlAst, schema: &MetricSchema) -> Result<QueryExpr>;
pub fn lower_sql(ast: SqlAst, schema: &TableSchema) -> Result<QueryExpr>;
pub fn lower_datafusion(ast: DfAst, ctx: &DfContext) -> Result<QueryExpr>;
pub fn lower_elasticdsl(ast: EsAst, schema: &IndexSchema) -> Result<QueryExpr>;
```

Once a query hits L3 it's language-agnostic. All deployment models downstream see the same IR.

### `core::pipeline` — orchestration

The L1→…→L5 driver. Parameterised on a deployment model's optimizer rules (L4) + emitter (L5):

```rust
pub struct Pipeline<S: DeploymentModel> {
    deployment_model: S,
}

impl<S: DeploymentModel> Pipeline<S> {
    pub fn run(&self, workload: &QueryWorkload) -> Result<S::EmitterOutput, PipelineError> {
        let l3: Vec<QueryExpr> = workload.queries()
            .iter()
            .map(|q| self.parse_and_lower(q))   // L1→L2→L3
            .collect::<Result<_, _>>()?;
        let l4 = self.deployment_model.optimizer().optimize(l3)?;  // L4
        let l5 = self.deployment_model.physical().lower(l4)?;      // L5
        self.deployment_model.emitter().emit(&l5)                   // L5 → bytes/protobuf/DF plan
    }
}
```

Core owns the driver. Deployment models own what goes into each deployment-model-specific seam.

### `core::optimizer` — Layer 4 framework

Core ships the **rule engine + trait surface + a shared rule library**. Deployment models pick which rules to enable.

```rust
// trait (in core)
pub trait OptimizerRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn category(&self) -> RuleCategory;   // PushDown | Fusion | Elim | Bind | ...
    fn priority(&self) -> u16;
    fn apply(&self, expr: &QueryExpr, c: &DeploymentConstraints) -> Option<QueryExpr>;
}

// engine (in core) — fixed-point iteration, cycle detection, priority ordering
pub struct RuleEngine { /* ... */ }
impl RuleEngine {
    pub fn new(rules: Vec<Box<dyn OptimizerRule>>) -> Self { /* ... */ }
    pub fn run(&self, exprs: Vec<QueryExpr>, c: &DeploymentConstraints)
        -> Result<Vec<QueryExpr>, OptError>;
}

// shared rule library (in core::optimizer::rules) — opt-in from deployment models
pub mod rules {
    pub struct BindKllOnQuantile;       // AggIntent::Quantile → bind KLL(k by accuracy)
    pub struct BindCmsOnCount;          // AggIntent::Count → bind CMS(w, d)
    pub struct BindHllOnCardinality;    // AggIntent::Cardinality → bind HLL(p)
    pub struct FusionPassthrough;       // Aggregate over Filter → push Filter under Aggregate
    pub struct ElimNoopFilter;          // Filter(true) → child
    // ... etc
    impl OptimizerRule for BindKllOnQuantile { /* ... */ }
}
```

Deployment models compose rule sets by picking from the shared library + adding their own:

```rust
// in deployment-model-asaplifecycle
use asap_control_core::optimizer::{RuleEngine, rules::*};
fn rule_set() -> Vec<Box<dyn OptimizerRule>> {
    vec![
        Box::new(BindKllOnQuantile),
        Box::new(BindCmsOnCount),
        Box::new(FusionPassthrough),
        // DC-specific additions:
        Box::new(StageAwarePushDown),      // pushes ops to edge when possible
        Box::new(TransmissionCostRewrite), // uses TCO model to defer aggregation
    ]
}
```

Core also ships `DeploymentConstraints` as a trait object; each deployment model supplies a concrete impl with its deployment's memory budgets, network topology, available sketch backends, and the registered `Executor` list (one entry per concrete runtime instance, each tagged with its `StageId`). The executor list is populated from the deployment model's discovery channel — OpAMP for DC lifecycle, static config for single-backend query, the in-process `SessionContext` itself for fusion.

### `core::physical` — Layer 5 framework

```rust
pub trait PhysicalPlanner {
    type Topology: TopologyDescriptor;
    type Output;
    fn lower(&self, l4: Vec<QueryExpr>, t: &Self::Topology) -> Result<Self::Output, PlanError>;
}

pub trait TopologyDescriptor {
    fn stages(&self) -> &[StageDescriptor];
    fn edges(&self) -> &[StageEdge];
}

// pre-baked topologies in core
pub mod topology {
    pub struct ThreeStage { /* edge → gateway → backend */ }
    pub struct SingleStage { /* backend-only */ }
    pub struct ZeroStage;   /* in-process */
}

/// Identifier for a stage role (categorical tier in the data lifecycle).
pub struct StageId(pub String);   // "edge" / "gateway" / "backend" / "in-process"

/// A **concrete runtime instance** that executes a sub-DAG of the plan.
/// One stage may have many executors — e.g. a 50-host edge fleet is 50
/// executors all with `stage = StageId("edge")`; a singleton backend is
/// one executor at `stage = StageId("backend")`. The stage allocator
/// works at stage granularity; the deployment-model planner / emitter
/// materialises each stage's sub-DAG into one concrete config per executor.
pub struct Executor {
    pub id:           ExecutorId,        // stable identifier across replans
    pub stage:        StageId,           // which stage this executor occupies
    pub capabilities: ExecutorCaps,      // memory budget, available sketch backends, network neighbours
    pub address:      ExecutorAddr,      // OpAMP agent / HTTP endpoint / in-process handle
}

pub struct ExecutorId(pub String);

pub enum ExecutorAddr {
    OpAmpAgent(AgentId),                 // OTel collector managed via OpAMP (DC lifecycle)
    HttpEndpoint(Url),                   // ASAPQuery-backend, etc.
    InProcess,                           // asap-fusion library-mode SessionContext
}

// generic stage allocator — given a QueryExpr tree + a topology, decide which
// ops land on which stage subject to constraints. Stage-level only; per-executor
// fan-out happens in the deployment model's PhysicalPlanner using the executor
// list from `DeploymentConstraints::executors()`.
pub struct StageAllocator;
impl StageAllocator {
    pub fn allocate<T: TopologyDescriptor>(
        &self, exprs: &[QueryExpr], topology: &T, c: &DeploymentConstraints,
    ) -> Result<Vec<StageAssignment>, PlanError>;
}

// sketch catalogue — what sketches exist, what they support, what params they
// accept. Built at startup from the registered sketch backends; queried by L4
// binding rules to map an `AggIntent` → `SketchKind` + `SketchParams`, and by
// L5 to instantiate the chosen sketch.
pub struct SketchCatalog {
    pub entries: Vec<SketchEntry>,
}

pub struct SketchEntry {
    pub kind:                 SketchKind,            // Kll, Cms, Hll, DDSketch, KMV, Theta, …
    /// Which `AggIntent` variants this sketch can serve.
    pub supported_intents:    &'static [IntentTag],  // Quantile, Count, Cardinality, JoinCardinality, …
    /// Mergeability — sketches built on disjoint inputs combine without re-scan.
    /// (CMS / HLL / KLL / theta = mergeable; SpaceSaving = approximately;
    /// some heavy-hitter variants = no.)
    pub mergeable:            Mergeability,
    /// Whether the sketch supports point deletion (CMS update with -1,
    /// deletable Bloom filter) — gates `SketchExpr::SketchDelete`.
    pub deletable:            bool,
    /// Whether the sketch admits a linear inverse (CMS, theta, count-based)
    /// — gates `SketchExpr::SketchSubtract`.
    pub subtractable:         bool,
    /// Accuracy / confidence model: error bounds as a function of params.
    /// `(eps, delta)` for randomised sketches; absolute error for KLL; etc.
    pub accuracy:             AccuracyModel,
    /// Aggregation keys this sketch supports natively. Some sketches are
    /// keyed (CMS over (label, value)); others are unkeyed (HLL).
    pub aggregated_keys:      KeyShape,              // Unkeyed | KeyedScalar | KeyedTuple
    /// Parameter ranges + defaults. Catalog rejects out-of-range params at
    /// L4 bind time so L5 never sees an unsupported configuration.
    pub param_ranges:         ParamRanges,
    /// Memory + CPU model used by `CostModel`. Function of params.
    pub cost_model:           SketchCostModel,
}
```

Deployment models use these pieces:

```rust
// in deployment-model-asaplifecycle
use asap_control_core::physical::{StageAllocator, topology::ThreeStage, Executor};

impl PhysicalPlanner for LifecyclePlanner {
    type Topology = ThreeStage;
    type Output = Vec<(ExecutorId, ExecutorPlan)>;   // one entry per executor
    fn lower(&self, l4: Vec<QueryExpr>, t: &ThreeStage) -> Result<_, _> {
        // 1. Stage-level: colour the DAG by StageId.
        let assignments = StageAllocator.allocate(&l4, t, &self.constraints)?;
        // 2. Per-executor fan-out: for each Executor in the deployment,
        //    pick the sub-DAG assigned to its stage and produce its config.
        let executors: &[Executor] = self.constraints.executors();
        // deployment-model-specific post-processing: DC's delta_cost logic,
        // backend_client push preparation, etc.
        Ok(/* ... */)
    }
}
```

### `core::plan` — shared traits bridging layers

```rust
pub trait DeploymentModel {
    type Topology: TopologyDescriptor;
    type EmitterOutput;
    fn rules(&self) -> Vec<Box<dyn OptimizerRule>>;
    fn topology(&self) -> &Self::Topology;
    fn physical(&self) -> &dyn PhysicalPlanner<Topology = Self::Topology, Output = _>;
    fn emitter(&self) -> &dyn PlanEmitter<Output = Self::EmitterOutput>;
    fn constraints(&self) -> &dyn DeploymentConstraints;
}
```

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

pub struct QuerySpec {
    /// Stable identifier — preserved across `replan` cycles so the runtime
    /// can correlate plan outputs with the originating spec, and L4 reuse
    /// rules can name shared producers across consumers in the same workload.
    pub id: QueryId,

    /// Source-language query string + which language it's written in.
    /// L1 parses `query` against `language`; no schema lookup yet.
    pub query:    String,
    pub language: QueryLanguage,            // PromQL | Sql | DataFusion | ElasticDsl

    /// Evaluation time range. Some languages (PromQL) carry their own ranges
    /// inside `query`; this field wins when both are set.
    pub time_range: TimeRange,

    /// Per-target SLA. Drives L4 binding (which sketch / which params) and
    /// L5 stage placement (push down or not). The three are independent —
    /// `accuracy: Exact` disables all `Bind*` rules; an unset latency /
    /// dollars target lets the cost model pick freely.
    pub accuracy: AccuracyTarget,           // Exact | Epsilon(f64) | EpsilonDelta { ε, δ }
    pub latency:  Option<Duration>,         // p99 evaluation latency target
    pub dollars:  Option<Dollars>,          // per-evaluation budget

    /// Routing hint: which deployment model should plan this query. When
    /// unset, the runtime defaults to the deployment model bound to the
    /// inbound HTTP route (`POST /plan/lifecycle` vs `POST /plan/query`).
    pub deployment_model: Option<DeploymentModelId>,

    /// How the query is *evaluated*: one-shot, continuous, or scheduled.
    /// Distinct from `data` below — a one-shot query against a streaming
    /// source is "evaluate now over the latest window"; a streaming query
    /// against a batch source doesn't make sense and is rejected at L1.
    /// Drives L4 binding (mergeable vs one-shot sketch family) and the
    /// L5 wire format (`OneShot` emits config + result; `Streaming` and
    /// `Periodic` emit a config that keeps running).
    pub shape: QueryShape,

    /// Shape of the *data* feeding the query. Workload-level summary —
    /// per-leaf detail rides on `Source::data_shape` (§6 L3). For a join
    /// over streaming + batch this is `Mixed`, and the planner reads the
    /// per-leaf shape during L4. Drives binding choices: an
    /// `AppendOnlyStream` unlocks incremental, mergeable sketches and
    /// retraction-free aggregation; `Batch` lets the planner pick a
    /// non-mergeable estimator (e.g. exact percentile over a sort) that
    /// wouldn't survive a distributed streaming setting; `Mutable`
    /// requires retraction-aware operators (out of scope today — the
    /// planner refuses sketch binding and falls back to re-scan).
    pub data: DataShape,
}

pub enum QueryLanguage   { PromQL, Sql, DataFusion, ElasticDsl }
pub enum AccuracyTarget  { Exact, Epsilon(f64), EpsilonDelta { eps: f64, delta: f64 } }

pub enum QueryShape {
    /// Evaluate once. Plan, execute, return result, discard state.
    /// SQL ad-hoc queries; one-off PromQL via `POST /plan`.
    OneShot,
    /// Continuous query — output stream that the executor keeps emitting
    /// as new data arrives. No fixed cadence; the runtime emits whenever
    /// the underlying state changes. Streaming dashboards, alerting
    /// expressions evaluated by the agent rather than by a poller.
    Streaming,
    /// Re-evaluated at a fixed cadence — Prometheus recording rules,
    /// scheduled dashboard panels, alerting evaluation cycles. The
    /// planner amortises sketch / aggregate build cost across evaluations
    /// within the cadence and reuses state between adjacent windows.
    Periodic { every: Duration },
}

pub enum DataShape {
    /// Bounded relation, fully materialised at plan time. SQL tables,
    /// Parquet / CSV files, DataFusion in-process tables.
    Batch,
    /// Append-only stream — events arrive over time, never updated or
    /// deleted. Metrics, logs, event streams. The common case for
    /// asaplifecycle and asapquery.
    AppendOnlyStream,
    /// Mutable relation — inserts + updates + deletes. Operational
    /// databases, CRUD-style tables. Sketch binding is currently refused
    /// for this shape (no retraction-aware sketches in the catalog yet).
    Mutable,
    /// Join across sources of differing shape. The planner consults
    /// `Source::data_shape` per leaf during L4; this variant exists so
    /// callers don't have to flatten a workload-level summary.
    Mixed,
}
```

All three deployment models take `&QueryWorkload`. Same type, different planners.

The `shape` × `data` cross-product isn't fully populated. Combinations the planner accepts and rejects:

| `shape` \ `data` | `Batch` | `AppendOnlyStream` | `Mutable` | `Mixed` |
|---|---|---|---|---|
| `OneShot` | ✓ ad-hoc SQL, fusion | ✓ "evaluate now over latest window" | ✓ via re-scan, no sketch binding | ✓ per-leaf shape decides |
| `Streaming` | ✗ rejected at L1 (no stream over a static dataset) | ✓ canonical streaming case | ✗ rejected (no retraction support) | ✓ if streaming leaves dominate |
| `Periodic { every }` | ✓ scheduled batch report | ✓ recording rules | ✓ via re-scan | ✓ |

The `shape` × `data` cross-product isn't fully populated. Combinations the planner accepts and rejects:

| `shape` \ `data` | `Batch` | `AppendOnlyStream` | `Mutable` | `Mixed` |
|---|---|---|---|---|
| `OneShot` | ✓ ad-hoc SQL, fusion | ✓ "evaluate now over latest window" | ✓ via re-scan, no sketch binding | ✓ per-leaf shape decides |
| `Streaming` | ✗ rejected at L1 (no stream over a static dataset) | ✓ canonical streaming case | ✗ rejected (no retraction support) | ✓ if streaming leaves dominate |
| `Periodic { every }` | ✓ scheduled batch report | ✓ recording rules | ✓ via re-scan | ✓ |

### `core::cost`

Extracted from DC's `planner/*` (delta, online, pareto, tco). Deployment models pick the models they need.

```rust
pub trait CostModel {
    fn accuracy(&self, plan: &Plan) -> Accuracy;
    fn latency(&self, plan: &Plan) -> Duration;
    fn dollars(&self, plan: &Plan) -> Dollars;

    /// Workload-level total cost. A straight sum of per-plan costs
    /// under-estimates how good a plan is when multiple queries share
    /// computation — a sketch or a precomputed aggregate built for
    /// one query serves the others for free. Implementations must
    /// identify reusable sub-expressions across `plans` and credit
    /// their build cost once, so the planner prefers plans that
    /// maximise reuse when the total-cost objective allows.
    fn workload_cost(&self, plans: &[Plan]) -> WorkloadCost;
}

pub struct WorkloadCost {
    pub total_latency: Duration,
    pub total_dollars: Dollars,
    /// Per-plan contribution, for EXPLAIN / observability.
    pub per_plan:      Vec<Contribution>,
    /// Sub-expressions built once, consumed by ≥2 plans. Drives
    /// decisions like "build a KLL for p99 that q1 and q2 both read"
    /// vs "run exact select-n per query".
    pub reused:        Vec<ReusedComponent>,
}
```

The reuse model is not sketch-specific: any primitive that is costly to build once and cheap to query (sketches, materialized aggregates, cached scan results, future wavelet summaries) plugs into the same `ReusedComponent` accounting.

### `core::emit`

```rust
pub trait PlanEmitter {
    type PlanInput;
    type Output;                             // YAML bytes, OpAMP RemoteConfig, rewritten LogicalPlan
    fn emit(&self, plan: &Self::PlanInput) -> Result<Self::Output, EmitError>;
}
```

Concrete emitters live in deployment model crates:

- `deployment-model-asapquery::yaml::StreamingConfigEmitter` → `streaming_config.yaml` bytes
- `deployment-model-asapquery::yaml::InferenceConfigEmitter` → `inference_config.yaml` bytes
- `deployment-model-asaplifecycle::emit::OpAmpRemoteConfigEmitter` → `opamp::RemoteConfig` protobuf
- `deployment-model-asapfusion::emit::DataFusionPlanEmitter` → rewritten `datafusion::LogicalPlan`

### `core::registry`

The extension point. The runtime binary does:

```rust
let mut reg = DeploymentModelRegistry::new();
reg.register::<ASAPLifecycleDeploymentModel>();
reg.register::<ASAPQueryDeploymentModel>();
reg.register::<ASAPFusionDeploymentModel>();
// add more...
```

Registration is by-type; the runtime looks up by `DeploymentModelId` (either from the `QuerySpec.deployment_model` field or from an HTTP route). A 4th deployment model is added by:

1. New crate `deployment-model-<name>/`
2. Implement `DeploymentModel` trait (wraps a `Planner` + its `PlanEmitter`s)
3. Register in `bin/asap-controller/main.rs`

No core change.

### End-to-end example — one query through all five layers

A worked trace of a single PromQL query as it flows L1 → L5. The query is intentionally simple (one metric, one window, one aggregate) so the IR shapes stay readable; production workloads have larger DAGs but the per-layer transformation is the same.

**Input.** A `QuerySpec` arrives at `POST /plan` carrying:

```text
quantile_over_time(0.99, http_request_duration_seconds{service="api"}[5m])
```

with `accuracy: AccuracyTarget::Epsilon(0.01)` and `language: PromQL`.

#### L1 — query language (parse to `PromqlAst`)

`core::query_language::promql::parse` wraps `promql-parser`. Output (sketch — actual fields come from the upstream crate):

```rust
PromqlAst::Call {
    func: BuiltinFn::QuantileOverTime,
    args: vec![
        Expr::Number(0.99),
        Expr::MatrixSelector {
            name: "http_request_duration_seconds",
            matchers: vec![LabelMatcher::Eq("service", "api")],
            range: Duration::from_secs(300),
        },
    ],
}
```

Pure language-level parse — no schema lookup, no sketch awareness. Same call returns the same AST regardless of deployment model.

#### L2 — language logical plan (`PromqlLogicalPlan` tree)

`core::lower::promql::lower_to_logical` walks the AST against a `MetricSchema` resolved from Prometheus's `/api/v1/labels`. The result preserves PromQL semantics — `quantile_over_time` is a range-vector aggregate, distinct from a generic `Aggregate { Quantile }` over a `Window`:

```rust
PromqlLogicalPlan::RangeAggregate {
    func: PromqlRangeAggFunc::QuantileOverTime { q: 0.99 },
    range: Duration::from_secs(300),
    matrix: Box::new(PromqlLogicalPlan::MatrixSelector {
        metric: "http_request_duration_seconds",
        matchers: vec![("service", LabelOp::Eq, "api")],
    }),
}
```

Per language: the SQL form `SELECT approx_percentile(latency, 0.99) FROM events WHERE service='api' AND ts > now() - INTERVAL '5 min'` lands in `SqlLogicalPlan` with a different shape. L2 is per-language; the shapes converge at L3.

#### L3 — intent algebra (`QueryExpr` + `AggIntent`)

`core::lower::promql::lower_to_intent` strips PromQL-specific shapes. `quantile_over_time` does **not** survive — the canonical L3 form for a windowed quantile is `Window` over `Aggregate{Quantile}` (see §6 design rule 1, "no `WindowedAgg`"; see §6 "Why no `QuantileOverTime` intent?"). The accuracy target threads through from the `QuerySpec`:

```rust
QueryExpr::Aggregate {
    by: vec![],
    aggs: vec![AggIntent::Quantile {
        q: 0.99,
        accuracy: AccuracyTarget::Epsilon(0.01),
    }],
    having: None,
    child: Box::new(QueryExpr::Window {
        kind: WindowKind::Sliding,
        size: Duration::from_secs(300),
        slide: None,
        child: Box::new(QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: MetricRef::from("http_request_duration_seconds"),
                time:   TimeRange::default(),     // supplied by QuerySpec at evaluation
                labels: LabelFilter::eq("service", "api"),
            },
            predicates: vec![],
        }),
    }),
}
```

Per-edge schemas (derived per the §6 input/output spec):

| Edge | Schema |
|---|---|
| `Scan` → `Window` | `{value: Float64, ts: Int64@time_index, service: Utf8}` |
| `Window` → `Aggregate` | the above + `{window_id: Int64, window_start: Int64, window_end: Int64}` |
| `Aggregate` → root | `{p99_value: Float64}` (one column per `AggIntent::Quantile`, typed via `output_type`) |

This IR is identical across deployment models — same DAG, same intent, same accuracy target. Below this point each deployment model picks its own L4 rules.

#### L4 — sketch algebra (`SketchExpr`)

The shared rule `core::optimizer::rules::BindKllOnQuantile` matches `Aggregate { aggs: [Quantile{q, accuracy}] }`, consults the sketch catalog (KLL has `supported_intents: [Quantile]`, is mergeable, satisfies `ε=0.01` at `k=200`), and rewrites the matched sub-DAG into a `SketchAgg` wrapped in a `SketchEstimate` (the readout). Everything not rewritten passes through in `SketchExpr::Logical(...)`:

```rust
SketchExpr::SketchEstimate {
    sketch_input: Box::new(SketchExpr::SketchAgg {
        child:  Box::new(SketchExpr::Logical(/* the L3 Window+Scan subtree */)),
        sketch: SketchKind::Kll,
        params: SketchParams::Kll(KllParams { k: 200 }),
        col:    ColumnRef::value(),
        by:     vec![],
    }),
    query: SketchQuery::Quantile { q: 0.99 },
}
```

Catalog-derived edge schemas:

| Edge | Schema |
|---|---|
| inner `Logical(Window)` → `SketchAgg` | as L3 above (logical pass-through) |
| `SketchAgg` → `SketchEstimate` | `{kll: Sketch(Kll, KllParams{k:200})}` |
| `SketchEstimate` → root | `{p99_value: Float64}` (the `Sketch(...)` field type does not propagate past `SketchEstimate`) |

The `Sketch(Kll, KllParams{k:200})` field type is the L4 type-system invariant — a downstream `SketchMerge` over a mismatched `Sketch(Cms, …)` input would fail at plan time, before L5 ever sees it. `BindKllOnQuantile` lives once in core and fires for all three deployment models; deployment-model-specific rules (lifecycle's `StageAwarePushDown`, fusion's `HashModeRule`) chain before or after.

#### L5 — physical plan (stage allocation + emission, per deployment model)

L4 chose the sketch family + params. L5 colors the DAG by `StageId` and emits per-executor configs. Same `SketchExpr` input; topology and emitter differ per deployment model.

##### deployment-model-asaplifecycle (3-stage, OpAMP + backend POST)

`StageAllocator` colors against `topology::ThreeStage`:

| Node | StageId | Why |
|---|---|---|
| `Scan` + `Window` + `SketchAgg` | `"edge"` | scrape happens on the agent host; KLL build is mergeable so it's safe to run early |
| `SketchMerge` (inserted on cut edge) | `"gateway"` | catalog says `KLL.mergeable = true`; reduces N edge streams to 1 |
| `SketchEstimate` | `"backend"` | readout where users query |

`PhysicalPlanner` then fans out via `DeploymentConstraints::executors()`. Given a deployment with 3 edge agents + 1 gateway + 1 backend:

```rust
vec![
    (ExecutorId("edge-001"),    OpAmpRemoteConfig { /* OTel YAML: scrape http_request_duration_seconds{service=api},
                                                        sliding 5m window, KLL k=200, forward to gateway-001 */ }),
    (ExecutorId("edge-002"),    OpAmpRemoteConfig { /* identical OTel YAML; differs only by agent_id */ }),
    (ExecutorId("edge-003"),    OpAmpRemoteConfig { /* … */ }),
    (ExecutorId("gateway-001"), OpAmpRemoteConfig { /* receive 3 KLL streams, SketchMerge, forward to backend */ }),
    (ExecutorId("backend-001"), AsapqueryBackendConfig { /* read merged KLL, SketchEstimate q=0.99 */ }),
]
```

The 3 edge configs are byte-identical except for `agent_id` — that's the §3 "one stage may have N executors all materialised from the same per-stage sub-DAG" property. The `AsapqueryBackendConfig` is produced by calling `deployment-model-asapquery::yaml::StreamingConfigEmitter` (see §8 asymmetric dep).

##### deployment-model-asapquery (1-stage, YAML)

`StageAllocator` against `topology::SingleStage` returns everything on `"backend"`. `PhysicalPlanner` produces one config per backend executor (typically one):

```yaml
# streaming_config.yaml — emitted by deployment-model-asapquery::yaml::StreamingConfigEmitter
operators:
  - name: kll_p99_request_duration
    kind: DatasketchesKLL
    params: { k: 200 }
    input:
      metric: http_request_duration_seconds
      label_filter: { service: api }
      window:  { kind: sliding, size: 5m }
estimates:
  - sketch: kll_p99_request_duration
    query:  { kind: quantile, q: 0.99 }
```

This is the byte-for-byte format ASAPQuery-backend already consumes — `inference_config.yaml` is emitted alongside by `InferenceConfigEmitter` for the readout side. Both are wire invariants (see §10).

##### deployment-model-asapfusion (0-stage, in-process `LogicalPlan`)

`StageAllocator` against `topology::ZeroStage` returns the whole DAG in-process. `PhysicalPlanner` rewrites the caller's `datafusion::LogicalPlan`, replacing the original `Aggregate(quantile(0.99, ...))` node with an `Extension` wrapping the KLL sketch op:

```rust
LogicalPlan::Extension(Extension {
    node: Arc::new(SketchAggExt {
        kind:     SketchKind::Kll,
        params:   KllParams { k: 200 },
        input:    /* original Window+Filter+Scan subtree, unchanged */,
        estimate: SketchQuery::Quantile { q: 0.99 },
    }),
})
```

Returned to the caller's `SessionContext` for execution by `asap-fusion`'s in-process operator. No wire format, no external executor; the data plane is the caller's process. Note also that `asap-fusion`'s entry point is `Source::Table` (not `Source::TimeSeries`) — this PromQL example is only illustrative for the fusion deployment model, which in practice consumes a pre-built DataFusion `LogicalPlan` and skips L1 (see §8).

#### What this example demonstrates

- **L1 → L3 are deployment-model-independent.** All three deployment models see the same `QueryExpr` for this query.
- **L4 binding is a shared rule.** `BindKllOnQuantile` lives once in `core::optimizer::rules` and fires for all three.
- **L5 is where deployment models diverge.** Same `SketchExpr` → three topologies → three emitter outputs. The topology is a parameter, not an axis of code (§3).
- **The sketch path is selected, not mandated.** If the `QuerySpec` had `accuracy: AccuracyTarget::Exact`, no `Bind*` rule would fire; L4 would pass `SketchExpr::Logical(QueryExpr::Aggregate{…})` through, and L5 would target an exact `HashAgg` (§1 goal 6).

## 7. Runtime crate details

Lifted from DC controller with zero semantic change:

- `http/` — axum server on `:8080`. Routes: `POST /plan`, `POST /replan`, `GET /plans/:id`, `GET /status`, `GET /metrics`
- `opamp/` — WebSocket OpAMP server on `:4320`. Same protocol DC speaks today.
- `monitor/` — `Scraper` polls Prometheus, emits `Violation`
- `replan/` — subscribes to violations + expiry ticks, re-invokes the deployment model registry
- `store/` — in-memory `PlanStore` + `WorkloadStore`. Pluggable backend later.
- `backend_client/` — pushes `StreamingConfig` YAML to ASAPQuery-backend's `/api/v1/streaming-config` endpoint. Factored out so new deployment models can push to other backends.

Runtime depends on `core` but NOT on any deployment model crate directly. It talks to deployment models via the registry.

## 8. Deployment model crate details (each owns its L4 + L5)

Each deployment model crate is a library with:

- A `DeploymentModel` impl that registers its planner, its emitters, and its HTTP routes (if any).
- Its own config types — no shared config crate.
- Its own cost model — maybe using `core::cost` primitives, maybe not.
- Its own integration tests.

### `deployment-model-asaplifecycle` (thin)

- **Data model**: time-series. L2→L3 lowering produces `QueryExpr` with `Source::TimeSeries` leaves.
- **L4 rules**: picks from `core::optimizer::rules::*` (Bind*, Fusion*, Elim*) + adds DC-specific rules that require stage awareness (`StageAwarePushDown`, `TransmissionCostRewrite`). Adding a stage-specific rule = a new file in `deployment-model-asaplifecycle/src/rules.rs`, impl `OptimizerRule`. Unchanged rules come from core.
- **L5 topology**: `core::physical::topology::ThreeStage` (edge → gateway → backend). No deployment-model-specific allocator logic — `StageAllocator` handles the tree walk; lifecycle only declares what the topology looks like.
- **Cost models**: `delta / online-EMA / Pareto / TCO` — these live in `deployment-model-asaplifecycle/src/cost.rs` because they're specific to the DC deployment's network/compute assumptions. Implement `core::optimizer::cost::CostModel`.
- **L5 emitters**: `OpAmpRemoteConfigEmitter` (per-role OTel YAML over OpAMP WebSocket) and `AsapqueryBackendConfigEmitter` (calls `deployment-model-asapquery`'s `StreamingConfigEmitter` for the YAML bytes, then POSTs to backend).
- **HTTP route**: `POST /plan` for full-lifecycle planning, `POST /replan` for SLA-triggered.
- **Size estimate**: ~1500 LOC (was ~5000 pre-refactor). Cost models are the bulk; rule selection + topology + emitter are each a few hundred lines.

### `deployment-model-asapquery` (thin)

- **Data model**: time-series. L2→L3 lowering produces `QueryExpr` with `Source::TimeSeries` leaves.
- **Inherited L1**: uses `core::query_language::promql` and `core::query_language::sql`.
- **NEW L2 tree** (Phase 4 work): defines `PromqlLogicalPlan` in `core::logical_plan::promql` that expresses the five pattern shapes asap-planner-rs currently template-matches as first-class L2 nodes. Replaces the pattern-catalogue approach with a proper L1→L2 tree rewrite. SQL side gets a matching `SqlLogicalPlan`.
- **L3 intent**: maps `Statistic` enum (9 variants) onto `core::intent_algebra::AggIntent` subset. Planner's `Topk` maps directly to `AggIntent::TopK` (heavy-hitter intent, served by SpaceSaving / CMS-with-heap at L4).
- **L4 rules**: picks from `core::optimizer::rules::*` (all `Bind*` rules are relevant since this deployment model covers most sketch types) + a deployment-model-specific sketch-binding rule for the precompute engine's flavor (which sketches are available, what params, `DeltaSetAggregator` auto-injection before CMS/HydraKLL). This rule absorbs `map_statistic_to_precompute_operator`'s sketch-binding half.
- **L5 topology**: `core::physical::topology::SingleStage` (backend-only). No stage-split; `StageAllocator` returns everything on one stage trivially.
- **L5 emitters**: `StreamingConfigEmitter` + `InferenceConfigEmitter` (YAML bytes). **Authoritative** for these two formats — `deployment-model-asaplifecycle` calls them when it needs to POST to ASAPQuery-backend.
- **Extra L1 inputs**: `query_log/` for Prometheus-query-log replay (unique to this deployment model); `schema/` for `PromQLSchema` discovery from a live Prometheus URL.
- **HTTP route**: `POST /plan/query` (JSON `QuerySpec` in, YAML stream out). Also backs the `bin/asap-query` one-shot CLI.
- **Size estimate**: ~2500 LOC (was ~6000 pre-refactor — saved by picking from shared rule library; still pays the L2 tree + L3/L4 split refactor cost, which is one-time).

### `deployment-model-asapfusion` (thin)

- **Data model**: tabular. L2→L3 lowering produces `QueryExpr` with `Source::Table` leaves (and, in future, `Source::Join` when fusion extends to multi-table). Crucially **not time-indexed** — fusion works on arbitrary DataFusion relations; time is just another column if present.
- **L1 opt-out**: fusion consumes a pre-built DataFusion `LogicalPlan` from its caller. `core::query_language` is not invoked. Library-mode deployment model.
- **L2 inherited from DataFusion**: DataFusion's `LogicalPlan` *is* fusion's L2 tree. `core::logical_plan::datafusion` is a thin re-export of `datafusion::logical_expr::LogicalPlan` so deployment models that want to take a DataFusion plan as input have a canonical name for it.
- **L3 intent**: `SubPopulationAnalyticsType` (3 variants: `Count`, `Sum`, `Quantile`) maps to `core::intent_algebra::AggIntent` subset.
- **L4 rules**: picks `BindCmsOnCount` and `BindKllOnQuantile` from `core::optimizer::rules::*` (which already cover fusion's `SketchConfigRule` semantics) + deployment-model-specific `HashModeRule`. Rules operate on DataFusion `LogicalPlan` (via Extension wrapping), not on `QueryExpr` trees — this lets DataFusion's `context.state().optimize` run *after* fusion's rewrites, keeping free reuse of DF's standard optimizer passes.
- **L5 topology**: `core::physical::topology::ZeroStage` (in-process).
- **L5 emitter**: rewritten `datafusion::LogicalPlan`. Not a wire format — this deployment model is library-mode.
- **Executor**: `ASAPExecutor` wraps a DataFusion `SessionContext`. Users construct `deployment-model-asapfusion` in-process.
- **HTTP route**: none by default.
- **Conformance cost**: near zero. Fusion's `SketchConfigRule` logic is replaced with picks from `core::optimizer::rules::*`; its `HashModeRule` stays deployment-model-specific.
- **Size estimate**: ~1800 LOC (was ~3000 pre-refactor; the `SketchConfigRule` code folds into core's shared rule library).

The sketch microbenchmarks (KLL/CMS) move with the crate and keep running. The TODO items from `asap-fusion/TODO.md` (batch/multi-query execution, time semantics, distributed model) remain open but are now filed against `deployment-model-asapfusion/TODO.md` in-repo.

### Data plane communication

Control plane (ASAPController) and data plane (OTel agents / ASAPQuery-backend / DataFusion runtimes) always talk **over wire**, never via in-process calls. This is unchanged from today:

| Deployment model | Data plane lives in | Wire protocol |
|---|---|---|
| lifecycle | DataCollector (OTel collectors, agent + backend roles) | OpAMP WebSocket (config push) + HTTP POST (`StreamingConfig` → ASAPQuery-backend) + Prometheus scrape (metrics in) |
| query | ASAPQuery-backend (query engine, SimpleMapStore) | HTTP POST `/api/v1/streaming-config` + `/api/v1/plan` (capability-miss callback in) + YAML file on disk (init-container mode) |
| fusion | Caller's DataFusion `SessionContext` | in-process library call (no wire) |

**Data plane code stays in its original repo.** ASAPController only owns the control plane. The merger doesn't move OTel collectors out of DataCollector, doesn't move the query engine out of ASAPQuery-backend, and doesn't move DataFusion out of asap-fusion's users. Each data plane keeps its own release cadence.

### Deployment model placement: in-repo or out-of-repo

Because core owns the L4/L5 infrastructure (not just L1-3), a deployment model crate is small and largely self-contained. That means deployment models can live either:

- **Inside ASAPController workspace** — `crates/deployment-model-<name>/`, lockstep release with core, cross-deployment-model changes are one PR.
- **In their own downstream repo** — declares `asap-control-core` as a git-tagged dep, releases independently, owns its own CI.

Both produce functionally identical artifacts because they pick from the same `core::optimizer::rules` library and use the same traits. The placement is a **deployment / team-ownership decision**, not an architectural fork.

Default recommendation:
- **deployment-model-asaplifecycle** and **deployment-model-asapquery** in ASAPController (they share `deployment-model-asapquery`'s YAML emitter via a workspace `path = "../deployment-model-asapquery"` dep — trivial in-workspace).
- **deployment-model-asapfusion** out-of-tree in `asap-fusion` repo (research project with independent benchmark cadence; depends on `asap-control-core` + `asap-control-optimizer` as published git tags).

Future deployment models choose whichever placement fits the team that owns them.

### Asymmetric dependency

`deployment-model-asaplifecycle` depends on `deployment-model-asapquery` because the `StreamingConfig` YAML emitter is authoritative there. When both live in ASAPController workspace this is a trivial `path =` dep. If one is ever moved out-of-tree, we lift the emitter into core to avoid a cross-repo Cargo dep.

## 9. Extension point — a 4th deployment model

With the L4/L5 framework in core, adding a new deployment model is mostly picking + a bit of glue. A hypothetical "edge caching" deployment model that decides which queries to cache at the edge vs. backend would land as:

```
crates/deployment-model-edge-cache/                   # or your own repo
├── Cargo.toml                                 # depends on asap-control-core
├── src/
│   ├── lib.rs                                 # impl DeploymentModel for EdgeCacheDeploymentModel
│   ├── rules.rs                               # pick core rules + add EdgeCacheBindRule
│   ├── topology.rs                            # TwoStage { edge, backend } TopologyDescriptor
│   ├── cost.rs                                # impl CostModel — hit ratio × bandwidth
│   └── emit/
│       └── edge_agent_config.rs               # impl PlanEmitter — emits edge-agent YAML
└── tests/
```

Total touch outside the new crate:
- 1 line in `bin/asap-controller/main.rs` to register (if in-workspace) OR a published binary in your own repo
- 1 line in workspace `Cargo.toml` (if in-workspace)
- optional: add `DeploymentModelId::EdgeCache` to `core::registry`

Typical size: ~500-2000 LOC depending on how deployment-model-specific the rules / cost model / emitter are. If the deployment model accepts core's defaults everywhere, ~300 LOC is realistic.

### Standalone binary for one deployment model

If you want a slim binary that runs only one deployment model:

```
bin/asap-edge-cache/
├── Cargo.toml     # depends only on runtime + deployment-model-edge-cache, not other deployment models
└── src/main.rs    # registers only EdgeCacheDeploymentModel; no DataFusion, no query YAML
```

Cargo builds this with its own minimal dep tree — no `datafusion`, no `promql-parser` unless this deployment model needs it. Useful when a deployment is dedicated to one deployment model (e.g. an edge-caching-only service).

## 10. Wire protocols — what changes, what doesn't

**Unchanged** (hard contract with other systems):
- `POST /api/v1/streaming-config` on ASAPQuery-backend — consumed YAML format
- `POST /api/v1/plan` on DC controller (from backend capability-miss) — request JSON
- OpAMP `ServerToAgent.remote_config` payload — OTel collector YAML
- Prometheus scrape format

**Internal** (controller's own surface, still HTTP+JSON for now):
- `POST /plan` — new unified entry point that takes `QueryWorkload` in, returns a plan ID + list of emitted artifacts (URIs). `/api/v1/plan` proxies to this.
- `GET /plans/:id` — plan inspection
- `GET /status` — runtime + deployment model health

**New** (internal-only, proto):
- `proto/asap_control.proto` defines `Plan`, `PlanNode`, `Expr` for persistence + store-internal serialization. Not on the wire between services. Optional; initial migration skips this and persists via `serde_json`.

## 11. Dependencies and Cargo surface

Workspace `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/core",
    "crates/runtime",
    "crates/deployment-model-asaplifecycle",
    "crates/deployment-model-asapquery",
    "crates/deployment-model-asapfusion",
    "crates/control-proto",
    "crates/testing",
    "bin/asap-controller",
    "bin/asap-query",
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
# deployment-model-specific deps kept in deployment model crates
```

Core depends only on `serde`, `tracing`, `thiserror`, and small utility crates. No HTTP, no DataFusion, no OpAMP.

Runtime depends on core + `axum` + `reqwest` + `tokio-tungstenite` + `prost` (OpAMP proto).

Deployment models depend on core. deployment-model-asapquery also pulls in `promql-parser`, `sqlparser`. deployment-model-asapfusion pulls in `datafusion` + `arrow`. deployment-model-asaplifecycle pulls in `sqlparser` + `promql-parser` + the cost-model math crates.

This matters: a user who only wants `deployment-model-asapfusion` (e.g., an offline benchmark) gets DataFusion but NOT axum/OpAMP.

## 12. Open questions

1. **Do we need a cross-deployment-model cost model?** Today DC's lifecycle planner and asap-planner-rs's query planner have overlapping but not identical cost models. Answer for now: keep them separate in deployment model crates; let them re-converge organically. If a third deployment model needs the same model, lift at that point.

2. **Where does the backend's `ControllerClient.create_plan` call land?** Initially: HTTP `POST /api/v1/plan` on the controller, handled by `deployment-model-asaplifecycle` (same as today). Long-term: could route by `QuerySpec.deployment_model` to a different planner, but that's a follow-up.

3. **Which deployment model owns `StreamingConfig` YAML emission?** Both `deployment-model-asaplifecycle` and `deployment-model-asapquery` emit it today (DC's `config/generate_streaming_config_yaml` and `output/generator.rs` in `asap-planner-rs`). Plan: **one emitter in `deployment-model-asapquery`**, called from both. This is why `deployment-model-asaplifecycle` depends on `deployment-model-asapquery` in the dependency graph (asymmetric — query does not depend on lifecycle).

4. **Do we vendor DataFusion's IR into core?** No. `deployment-model-asapfusion` owns its DataFusion-flavored plan; `core::plan::Plan` stays an enum with a variant that wraps a `datafusion::LogicalPlan` behind a feature flag. Core itself never reaches into DataFusion types.

5. **OpAMP proto: vendored, or from crates.io?** Today DC vendors. Recommend: keep vendored in `proto/opamp.proto`, generate via `prost-build` in `crates/control-proto`. Same thing DC does today, just moved.

6. **What happens to ASAPQuery-backend's `asap-planner-rs` directory post-migration?** Deleted. ASAPQuery-backend's docker-compose drops the `asap-planner-rs` init container; the controller's `deployment-model-asapquery` now runs in-process (service mode) or as the `asap-query` CLI (one-shot mode). The ASAPQuery-backend repo shrinks by two directories.

7. **Versioning?** Start at `0.1.0` on the workspace. Deployment models can rev independently later via per-crate versions, but initially lockstep.

8. **What if a future deployment model can't fit a tree L2?** L2 is currently a per-language tree, mandatory for every deployment model. asap-planner-rs's Phase 4 conformance cost (reverse-engineering PromQL pattern templates into a `PromqlLogicalPlan` tree) was taken deliberately to keep the architecture uniform. If a future deployment model's source language doesn't map naturally onto a tree (e.g. a constraint-based or dataflow-graph query language), that's the moment to re-examine the L2 contract — the `core::logical_plan` module is a single Rust trait + per-language types, not a deep assumption baked across the codebase. Until then, L2 = tree.

9. **Deployment model placement — in ASAPController workspace, or in own repo?** Both supported, same architecture either way (see §8 "Deployment model placement"). Default: `deployment-model-asaplifecycle` and `deployment-model-asapquery` in ASAPController (easy cross-deployment-model changes, shared YAML emitter); `deployment-model-asapfusion` in the `asap-fusion` repo (research cadence). A new deployment model picks based on team ownership and release cadence preferences.

10. **How far do shared L4/L5 rules live in core before they become deployment-model-specific?** The rule of thumb: if ≥2 current deployment models would use it, it lives in `core::optimizer::rules::*`. If only 1 deployment model uses it *and* it depends on deployment-model-specific types (DataFusion's `LogicalPlan`, OTel YAML shape), it lives in the deployment model crate. `SketchConfigRule`-style "bind intent to concrete sketch" rules belong in core (shared). `StageAwarePushDown` (which needs DC's stage graph) belongs in `deployment-model-asaplifecycle`. When a rule straddles — e.g. a fusion rewrite that *could* be generalized to any L4-compatible plan — start it in the deployment model; lift to core once a second deployment model wants it. Don't pre-emptively generalize.

11. **How does the design support non-time-series data (asap-fusion's tabular queries, future OLAP deployment models)?** Already handled — see §3 "Data-model support" and §6 `core::intent_algebra`. `QueryExpr::Scan` wraps a `Source` sum (`TimeSeries` / `Table` / `Join`); `AggIntent::requires() -> DataModel` tags which intents apply to which data models; sketches themselves are data-model-agnostic. asap-fusion uses `Source::Table` exclusively; ASAPQuery uses `Source::TimeSeries`; a future OLAP deployment model picks whichever fits. The only implementation cost is that L4 rules which genuinely don't apply across data models (e.g. "merge overlapping time windows") must gate on `source.data_model()`; data-model-agnostic rules (the `Bind*` family) need no changes.

### Resolved during the L3 IR cleanup (see §6 `core::intent_algebra`)

The following questions came up during review of the previous `QueryExpr` draft and are now settled. Listed here so the trail is visible:

- **Output of `SketchAgg` vs `WindowedAgg`?** Neither node exists at L3 anymore. `SketchAgg` is an L4 sketch-bound node (`SketchExpr::SketchAgg`) emitted by binding rules; `WindowedAgg` was redundant with `Window` over `Aggregate` and removed. Output of `SketchAgg` is a sketch-typed column carrying the partial state; `SketchEstimate` reads it out into a scalar / vector.
- **Is `TopK` different from `Sort + Limit`?** They overlap on heavy-hitter queries but are different concepts. `TopK` is now an `AggIntent` at L3 (heavy-hitter intent — SpaceSaving / CMS-with-heap / Misra-Gries serve it as a single primitive), while generic `Sort + Limit` survives as `QueryExpr` operators for non-heavy-hitter cases (`ORDER BY name LIMIT 10`). L1→L2→L3 lowering picks the intent form when it recognises a heavy-hitter pattern (`ORDER BY count DESC LIMIT k`, PromQL `topk(k, …)`) and the operator form otherwise.
- **What is `JoinSketch`?** A sketch-aware join (KMV / theta / join-sample). Moved to L4 (`SketchExpr::SketchJoin`) so the choice "exact join vs sketch join" is an L4 cost decision against `Join`, not a competing L3 surface.
- **`HistogramQuantile` and `PromQLSubquery` feel out of place** — they were. Both removed from L3. `histogram_quantile` lowers in PromQL L1→L2 to bucket reads + a `Quantile` intent. `<expr>[range:resolution]` is a driver that expands into multiple queries during PromQL L1→L2 lowering, not a DAG node.
- **Sketch subtract / delete / estimate operators?** Added: `SketchExpr::SketchSubtract`, `SketchDelete`, `SketchEstimate`. Catalog flags (`subtractable`, `deletable`) gate which sketch families admit them.
- **What's `Dedup` exactly?** SQL `DISTINCT`. Renamed to `Distinct { cols }` and generalised from one column to N.
- **Why differentiate `Quantile` and `QuantileOverTime`?** No reason — the surrounding `Window` already encodes the temporal axis. `QuantileOverTime` removed from `AggIntent`.
- **`WindowedAgg` vs `Window` + `Agg`?** Equivalent; `WindowedAgg` removed. `WindowKind::{Tumbling, Sliding, Session}` lives on the `Window` node so the streaming-window kind is explicit; SQL analytic `OVER (...)` stays on the separate `WindowFunc` node.
- **Need input/output specs more fine-grained than `QueryExpr`?** Done: every L3 edge carries a typed `Schema` (fields + time index + unique-key sets), and §6 lists per-node input/output schemas.

## 13. Future work

### Extending the primitive set beyond sketches

The L4 rule engine + `OptimizerRule` trait are **primitive-agnostic** — nothing in the framework is sketch-specific above the rule library. Future primitive classes land as additional rules against the same trait + additional entries in `CostModel`, not as new translation phases or parallel IRs. Candidates we explicitly anticipate:

1. **Wavelets** — a sibling approximation family (Haar / DWT + coefficient thresholding). Strong on smooth, low-entropy signals where thresholded coefficient sets dramatically out-compress randomized sketches. Slots in as a new physical alternative for the same `AggIntent` variants sketches serve today (range-sum, heavy-hitter, quantile). No change to L3.

2. **Reuse / precomputation as first-class primitives.** Materialized views, cached scan results, shared sub-expressions. The cost-model hook already exists (`CostModel::workload_cost` + `ReusedComponent`); the missing piece is rules that *introduce* a reuse node — e.g. "build this aggregate once for `q1`, rewrite `q2` to read from it". Orthogonal to approximation: you can reuse an exact aggregate or an approximate one.

3. **Other approximation algorithms** — sampling, coresets, online PCA / linear-regression normal equations, naive-Bayes-with-conjugate-priors. Anything with a monoid-shaped build + bounded error fits the same contract a sketch does.

The guiding principle: **same framework, different rule.** A new primitive class is never a translation-layer change — only a new rule, a new cost-model entry, and (if it introduces a new runtime op) a new physical operator in L5.

## 14. Glossary

Terms that are project-specific or that get conflated. Where the term has a Rust counterpart, the type is shown in backticks.

### Architecture roles

- **Deployment model** — A concrete bundle of (L4 rule choices) + (L5 topology) + (emitter), packaged as a `deployment-model-*` crate. Three exist today: `asaplifecycle`, `asapquery`, `asapfusion`.
- **Stage** (`StageId`) — A *categorical tier* in the data lifecycle: edge / gateway / backend / in-process. Roles, not instances. Declared by the deployment model's `TopologyDescriptor`.
- **Executor** (`Executor`) — A *concrete runtime instance* occupying a stage: a specific OTel agent, the ASAPQuery-backend process, a DataFusion `SessionContext`. One stage may have N executors (e.g. 50 edge agents). Carries `id`, `stage: StageId`, `capabilities`, `address`.
- **Topology** (`TopologyDescriptor`) — Declares which stages exist + how they connect (3-stage / 1-stage / 0-stage). Categorical, not instance-level.
- **DeploymentConstraints** — Trait object owned by each deployment model. Carries memory budgets, network topology, available sketch backends, and the registered `Executor` list. Threaded through L4 rules and the stage allocator.

### Layer drivers

- **RuleEngine** — Generic L4 driver in core (fixed-point iteration + cycle detection + priority ordering). One implementation; same code for every deployment model.
- **OptimizerRule** — Trait for an L4 rewrite. Categories: `PushDown | Fusion | Elim | Bind | StageRouting`. Deployment models pick which rules from `core::optimizer::rules` + add their own.
- **CostModel** — Trait scoring a plan's accuracy / latency / dollars. Generic implementations in core; deployment-model-specific cost models (DC's delta / online / pareto / TCO) live in their crates.
- **StageAllocator** — Generic L5 algorithm in core. Colors the L4-bound DAG by `StageId`. **Stage-granularity only — no executor knowledge.** One implementation; same code for every deployment model.
- **PhysicalPlanner** — Per-deployment-model L5 *driver* (one impl per deployment model). Calls `StageAllocator`, then fans the per-stage sub-DAGs out to executors via `DeploymentConstraints::executors()`, then produces the deployment-specific output type. Compare to allocator: planner = whole L5 driver, allocator = the stage-coloring step it uses.
- **PlanEmitter** — Per-deployment-model trait that serialises the planner's output to its wire format (OpAMP `RemoteConfig` / `streaming_config.yaml` / rewritten DataFusion `LogicalPlan`).

### IR by layer

- **L1 — query language** — Raw query string + parser (PromQL / SQL / DataFusion / ElasticDSL).
- **L2 — logical plan** — Per-language relational algebra tree (`PromqlLogicalPlan`, `SqlLogicalPlan`, etc.).
- **L3 — intent algebra** (`QueryExpr`, `core::intent_algebra`) — Symbolic, intent-only IR. No sketch type, no params. `AggIntent` carries accuracy targets only. The "algebra" is the operator surface (`Scan`, `Filter`, `Aggregate`, `Window`, …); the algebra is *intent-only* because no sketches have been bound yet.
- **L4 — sketch algebra** (`SketchExpr`, `core::sketch_algebra`) — Same DAG shape as L3, but sketches now committed (kind + params). Produced by L4 binding rules; consumed by L5 emitters. Note the naming inversion vs. earlier drafts: "sketch algebra" is L4 (where sketches actually live), not L3.
- **L5 — physical plan** — Stage-assigned, executor-targeted, ready to serialize. Produced by `PhysicalPlanner`, written out by `PlanEmitter`.

### Metadata sources (see §6 "DAG schema, DB schema, sketch catalog")

- **DAG schema** (`Schema`) — Columns + types + `unique_keys` carried on every L3/L4/L5 edge. Type-checked locally at each node.
- **DB schema** / **source schema** (`SchemaCatalog`) — Data-plane metadata (Prometheus TSDB / SQL `information_schema` / DataFusion catalog). Read by `core::lower::*` for L1→L2 symbol resolution.
- **Sketch catalog** (`SketchCatalog`) — Runtime registry of available primitives: what sketches exist, mergeability, deletability, parameter ranges, accuracy model. Consulted by L4 binding rules. **Not a schema** — a catalog of available primitives, not a description of a stream.

### Workload + identity

- **QueryWorkload** — Top-level controller input: one or more `QuerySpec`s plus workload features (batch vs streaming, reuse opportunity, data source). Arrives via HTTP POST, OpAMP capability-miss callback, YAML file, or query-log replay.
- **QuerySpec** — A single query: source-language string + accuracy / latency / cost target.

## 15. Success criteria

The migration is done when:

1. `asap-controller` binary runs and passes DC controller's existing integration tests (OpAMP push, backend config POST, SLA replan).
2. `asap-query` binary takes the same YAML input asap-planner-rs does today and produces byte-identical `streaming_config.yaml` + `inference_config.yaml` (fuzz-test against a corpus of fixtures).
3. ASAPQuery-backend's docker-compose no longer starts `asap-planner-rs`; the controller handles both shapes.
4. `asap-fusion`'s microbenchmarks still run under `deployment-model-asapfusion` with identical numbers.
5. The `DataCollector/controller/`, `ASAPQuery/asap-planner-rs/`, `ASAPQuery-backend/asap-planner-rs/`, and `asap-fusion/` directories are deletable (or already deleted) without breaking any currently-running deployment.
6. A new hypothetical deployment model can be added with zero changes outside its crate + one line in `bin/asap-controller/main.rs`.
