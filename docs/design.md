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
6. **Sketch is a primitive, not a mandate.** The optimizer selects among physical alternatives for each logical operator — e.g. `HashJoin` / `SortMergeJoin` / `SketchJoin`, or `SortAgg` / `HashAgg` / `SketchAgg` — the same way a traditional DB optimizer picks a join algorithm. A plan may come back with zero sketch operators when an exact path Pareto-dominates for the query's accuracy target. Sketches are one primitive class the optimizer can reach for; the framework does not privilege them.

## 2. Non-goals

- **Not** redesigning the `Plan` IR end-to-end. DC's `algebra/` + fusion's `translator/optimizer/executor` stay where they are semantically; we merge them into a shared `Plan` crate with a clean trait seam, not a green-field rewrite.
- **Not** unifying DataFusion's `LogicalPlan` with DC's custom algebra at the type level. Those are different universes. Scenarios own their IR; core owns the *staged dispatch contract*.
- **Not** in-scope: changing the existing wire protocol between controller and backend. ASAPQuery-backend keeps consuming `streaming_config.yaml` and keeps responding on `/api/v1/plan`.

## 3. The organizing spine: DC controller's 5-layer pipeline

DataCollector/controller already documents its query→sketch translation as a 5-layer pipeline (`DataCollector/controller/docs/query-to-sketch-translation.md`). That's the spine of this merger:

| # | Layer | What it does | Today's locations |
|---|-------|--------------|-------------------|
| 1 | **Query Language** | Parse raw strings (PromQL, SQL, DataFusion, ElasticDSL, …) into a language-specific AST | DC `controller/src/query_parser/{promql,sql,mod}.rs`; asap-planner-rs pulls `promql-parser` + `sqlparser` directly; asap-fusion consumes a pre-built DataFusion `LogicalPlan` (its L1 happens upstream) |
| 2 | **Language Logical Plan** | Per-language algebra tree (`Aggregate` / `Window` / `Filter` / `Sort` / `Limit`) preserving language semantics, **no sketch names, no sketch binding** | DC `controller/src/algebra/lower.rs`; asap-fusion inherits DataFusion's `LogicalPlan` as its L2; asap-planner-rs has no L2 today (uses a template-pattern catalogue) — **Phase 4 builds one** |
| 3 | **Sketch Logical Plan** (sketch algebra) | Language- and deployment-independent IR: `QueryExpr` + `AggIntent`. Describes **intent only** — *what* to compute, with accuracy target. **No sketch type, no sketch parameters, no sketch-bound nodes** (`SketchAgg` / `SketchJoin` / `SketchSubtract` etc. live in the L4 IR `SketchExpr`). **No language-shaped operators** (no `HistogramQuantile`, no `PromQLSubquery` — those are PromQL L2 nodes that lower to data-model-agnostic shapes here). **One canonical form per plan** — no `WindowedAgg` (use `Window` over `Aggregate`). Heavy-hitter intents are first-class (`AggIntent::TopK`) so heavy-hitter sketches bind directly on the intent rather than on a generic `Sort + Limit` shape; generic `Sort + Limit` survives in `QueryExpr` for non-heavy-hitter cases (e.g. `ORDER BY name LIMIT 10`). Every edge carries a typed `Schema`. Data-model-agnostic — `QueryExpr::Scan` wraps a `Source` sum with `TimeSeries` / `Table` / `Join` variants so the same L3 IR covers ASAPQuery's time-series queries and asap-fusion's tabular queries. | DC `controller/src/algebra/{expr,directory}.rs`; asap-fusion's 3-variant `SubPopulationAnalyticsType` maps to a subset; asap-planner-rs's 9-variant `Statistic` maps to a subset — **Phase 4 splits sketch binding out of planner's current fused L3+L4** |
| 4 | **Sketch Optimizer** | Cost-aware algebraic rewrite rules under deployment constraints. **This is where sketch binding happens** — L4 rules take intent-only L3 and emit sketch-bound L3+. ~12 rules in DC; a smaller targeted subset in planner; `SketchConfigRule` + `HashModeRule` in fusion. | Core provides the **rule engine driver** + `OptimizerRule` trait + a shared rule library; scenarios **pick** which rules to enable + supply their own deployment constraints. DC `controller/src/algebra/optimizer.rs` (rules); fusion `src/optimizer/rules/`; planner's `map_statistic_to_precompute_operator` |
| 5 | **Physical Execution Plan** | Assign ops to pipeline stages (edge / gateway / backend / object store); produce the deployment-specific artifact (OpAMP YAML, `streaming_config.yaml`, rewritten DataFusion `LogicalPlan`). **Sketch binding is already committed by L4**; L5 is about stage allocation + emission. | Core provides the **stage allocator framework** + `PhysicalPlanner` trait + the sketch catalogue; scenarios supply their own **topology** (3-stage / 1-stage / 0-stage) + their own **emitter** for the output format. DC `controller/src/algebra/{physical,allocator,plan}.rs`; asap-planner-rs `output/generator.rs`; asap-fusion `src/executor/` |

**The doc's key claim: layers 1–3 are query-language-independent and workload-independent.** That makes them the natural **common core**. Every scenario reads PromQL (or SQL, or …) the same way, lowers it to the same per-language algebra, and lowers THAT to the same sketch-algebra IR (intent only, no sketch binding).

**L4 and L5 also have substantial common infrastructure.** Initially we assumed scenarios owned L4/L5 wholesale; on closer inspection what's actually scenario-specific is *which rules fire* (L4) and *what topology + output format* (L5) — not the rule engine, not the allocator, not the sketch catalogue. Those frameworks belong in core. This makes scenarios significantly thinner: each becomes a small crate that picks rules from a shared library, declares a deployment topology, and writes an emitter.

### Sketch binding lives in L4, not L3

A key clarification after cross-checking the three source repos: **L3 is intent-only**. DC's `AggIntent` names *what* to compute (`Quantile(0.99, ε=0.01)`, `Cardinality(δ=0.001)`, …) without committing to a sketch type. Picking KLL vs DDSketch, CMS vs CMS-with-heap, parameter sizes — all of that is L4's job, driven by deployment constraints.

More generally: **L1–L3 lower into a logical representation + `AggIntent`; L4 and L5 choose the concrete execution plan.** That choice is a standard physical-operator selection — `HashJoin` vs `SortMergeJoin` vs `SketchJoin`; `SortAgg` vs `HashAgg` vs `SketchAgg`. "Use a sketch" is one option among several; the same L4 rule framework that picks sketch parameters also picks between sketch and non-sketch operators when a rule is registered for the intent. This keeps the existing 5-layer split intact: nothing about the layering presupposes the output contains a sketch.

Today:
- DC: correctly separated (L3 has `AggIntent`, L4 picks sketch via cost model).
- asap-fusion: correctly separated — `SketchConfigRule` at L4 fills `SketchConfig::NULL` with concrete `CountMinSketch{5,4096}` / `KLL{k=200,m=8}`.
- asap-planner-rs: **L3+L4 fused today**. `map_statistic_to_precompute_operator` jumps from `Statistic` straight to `AggregationType::DatasketchesKLL{k=200}` in one call. **Phase 4 splits this** — `Statistic → AggIntent` at L3, `AggIntent + DeploymentConstraints → AggregationType + SketchParams` at L4.

### Intent vocabulary: DC's `AggIntent` is a superset; scenarios use subsets

DC's pre-cleanup superset had ~25 variants; after L3 normalisation (see §6) it's smaller because language-flavored synonyms (`QuantileOverTime → Window + Quantile`) no longer earn their own intent. The post-cleanup core is 9 (`Count, Sum, Min, Max, Quantile, TopK, Cardinality, Rate, Increase`); the long-term ceiling is bounded by genuinely-distinct operations (stddev, variance, approximate-join-cardinality, …), not by language-flavored synonyms. Planner's 9-variant `Statistic` maps directly: `Topk` keeps its own intent (heavy-hitter sketches like SpaceSaving / CMS-with-heap compute it as a single primitive, so the intent earns L3 visibility). Fusion's 3 variants (`Count, Sum, Quantile`) map directly. Adding a new intent (e.g. stddev) is a core change that scenarios opt into.

### Data-model support: both time-series and tabular

ASAPQuery-backend / DC controller operate on time-series data (metrics + labels + timestamp); asap-fusion operates on tabular data (DataFusion `LogicalPlan` over relations) that may or may not be time-indexed. These two data models differ fundamentally in their leaf shape (`metric + labels + time` vs. `table + columns`), but they share everything above the leaf — filter semantics, aggregation semantics, sketches themselves.

Core handles this with:
- **`QueryExpr::Scan { source: Source, ... }`** where `Source` is a sum type (`TimeSeries`, `Table`, `Join`, …). Scenarios' L1→L2→L3 lowering produces the appropriate variant; L4 rules that care about the data model gate on `source.data_model()`.
- **`AggIntent::requires() -> DataModel`** — each intent variant tags whether it's data-model-agnostic (`Count`, `Sum`, `Min`, `Max`, `Quantile`, `Cardinality`), time-series-only (`Rate`, `Increase` — both carry PromQL counter-reset semantics), or tabular-only (future additions for joins, correlated subqueries).
- **Sketches are data-model-agnostic by construction.** KLL / CMS / HLL / DDSketch ingest a stream of values; that stream can come from a time-series window or a table column, the sketch does not know or care. So `BindKllOnQuantile` and siblings work uniformly across both.

Practically, this means:
- `scenario-query` + `scenario-lifecycle` lower into `QueryExpr` with `Source::TimeSeries` leaves.
- `scenario-fusion` lowers into `QueryExpr` with `Source::Table` leaves (and, in future, `Source::Join` when it extends to multi-table queries).
- A hypothetical OLAP scenario that runs approximate queries over tabular data reuses `Source::Table` + the same `AggIntent` subset fusion uses, plus any OLAP-specific intents it adds.

See §6 `core::sketch_algebra` for the concrete type sketches.

### Scope: start single-query, grow into workload-aware

The initial implementation can operate on **one query at a time** — L4 picks physical operators per query against per-query constraints, and `CostModel::workload_cost` degenerates to a sum of per-plan costs. This matches today's three source repos (all single-query planners) and is the minimum bar for parity during the migration.

**Workload-awareness is an extension, not a rewrite.** When ≥2 queries are planned together, `workload_cost` credits shared sub-expressions (sketches, precomputed aggregates) so the planner can pick a plan for `q1` that lets `q2` read its output for free. Nothing in the L1–L5 spine changes — only the cost objective widens and the rule engine gains cross-plan visibility. See §6 `core::cost` and §13 future work.

### L2 is mandatory; the tree shape is an evolvable contract

Every scenario must produce an L2 tree, even when the source language didn't originally come as one. asap-planner-rs's current approach (PromQL pattern catalogue → `IntermediateAggConfig`) skips L2; Phase 4 will reverse-engineer the five PromQL pattern shapes into a `PromqlLogicalPlan` tree so the L1→L2→L3 pipeline is uniform.

A future scenario whose source semantics genuinely don't fit a tree (e.g. a constraint-based query language) would motivate revisiting the L2 contract at that time. Until then, L2 = per-language tree, mandatory, no elision.

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

**Premise behind the per-executor split.** Every executor (edge collector, gateway, backend, DataFusion runtime) is in principle capable of running the entire query tree — they all speak the same physical operators. The controller's job is to decide *which* executor runs *which sub-tree / sub-DAG* under the deployment's constraints (memory budget per stage, network bandwidth between stages, sketch backends available at each stage). A "stage assignment" is a colouring of the L4-bound `SketchExpr` DAG by executor, with sketch-merge / data-shipping nodes inserted on the cut edges. L5's `StageAllocator` does the colouring; the `PhysicalPlanner` per scenario emits the per-executor configs.

This is what makes the topology a *parameter* rather than an axis of code: the same `SketchExpr` plus a different `TopologyDescriptor` produces edge-only / 1-stage / 3-stage / 0-stage placements without rewriting the plan.

**Symbolic plan vs concrete plan.** L3 `QueryExpr` and L4 `SketchExpr` are symbolic — they describe operations and bindings without committing to *where* anything runs. L5 produces the concrete plan: stage-assigned, executor-targeted, ready to serialize into the executor's configuration format. The split is what lets the controller swap topologies (single-stage backend → three-stage edge/gateway/backend) without re-running L1-L4.

This drives the core/scenario split:

- **`crates/core/`** owns all 5 layers of **shared infrastructure**: L1-3 end-to-end (parsers + lowering + intent-only IR), plus L4's rule engine driver + rule library + cost-model traits, plus L5's stage-allocator framework + `PhysicalPlanner` trait + sketch catalogue.
- **Each scenario is a thin crate** that: (1) picks which of core's L4 rules to enable + adds any scenario-specific rules, (2) declares its deployment topology (how many stages, where data flows), (3) provides an emitter for its output format. That's usually a few hundred lines, not thousands.

## 4. Principles

### P1. Core owns shared infrastructure across all 5 layers; scenarios own choices

See §3. Core is NOT just types + trait stubs — it ships working parsers + lowering passes (L1-3), a rule engine + rule library + cost-model traits (L4 framework), and a stage allocator + physical-plan framework + sketch catalogue (L5 framework). What scenarios plug in is **which rules fire** (picking from core's library + adding their own), **deployment topology** (how many stages; DC=3, query=1, fusion=0), and an **emitter** for the output format. A scenario that accepts all core's default L4 rules and uses `core::physical::single_stage_topology` is maybe 200 lines of code.

### P2. Core has no I/O

No HTTP, no OpAMP, no YAML, no Prometheus scrape, no `tokio::spawn`. Pure algorithms over in-memory data. This is what makes scenarios unit-testable without running the runtime.

### P3. Runtime is a thin binary, scenarios are libraries

The `asap-controller` binary is assembled from: runtime (HTTP/OpAMP/replanner/store) + N scenario crates registered as plugins. Swapping scenarios is a build-time feature flag or a runtime registry entry. No scenario may reach directly into another.

### P4. One input boundary, one output boundary

**Input**: everything that enters the controller (HTTP `QuerySpec`, Prometheus query log replay, YAML workload, capability-miss callback) normalizes into a single `QueryWorkload` type in core — a collection of `QuerySpec`s each feeding L1→L2→L3.

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
│   ├── core/                  # Shared infrastructure across all 5 layers; no I/O
│   │   ├── query_language/    # L1: per-language parsers — promql/, sql/, datafusion/, elasticdsl/
│   │   ├── logical_plan/      # L2: per-language algebra tree (Aggregate/Window/Filter/…)
│   │   ├── sketch_algebra/    # L3: QueryExpr + AggIntent + Schema/HasSchema + sketch directory
│   │   ├── lower/             # L1→L2→L3 lowering passes (one entry per language)
│   │   ├── optimizer/         # L4 framework:
│   │   │   ├── engine/        #   rule driver — fixed-point iteration, cycle detection, priority
│   │   │   ├── trait/         #   OptimizerRule + RuleCategory (PushDown / Fusion / Elim / Bind)
│   │   │   ├── rules/         #   shared rule library (e.g. sketch-binding rules; stream-vs-batch picker)
│   │   │   └── cost/          #   CostModel trait + generic impls (memory budget, accuracy degradation)
│   │   ├── physical/          # L5 framework:
│   │   │   ├── planner_trait/ #   PhysicalPlanner trait
│   │   │   ├── stage_allocator/ # generic topology-driven allocator
│   │   │   ├── topology/      #   Topology descriptor types (edge/gateway/backend, single, zero)
│   │   │   └── sketch_catalog/ # candidate sketch types + parameter constraints
│   │   ├── pipeline/          # orchestrates L1→L2→L3→L4→L5, parameterized on scenario
│   │   ├── workload/          # QueryWorkload wrapper around QuerySpecs
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
│   ├── scenario-lifecycle/    # thin — picks rules, 3-stage topology, OTel/backend emitters
│   │   ├── rules.rs           # L4 rule selection + DC-specific rules (stage-aware push-down)
│   │   ├── topology.rs        # 3-stage: edge / gateway / backend
│   │   ├── cost.rs            # scenario-specific cost impls — delta / online / pareto / tco
│   │   └── emit/              # OpAmpRemoteConfig + AsapqueryBackendConfig emitters
│   ├── scenario-query/        # thin — rules, 1-stage topology, YAML emitters + query-log input
│   │   ├── rules.rs           # L4 rule selection + sketch-binding rule (split from map_statistic_*)
│   │   ├── topology.rs        # 1-stage: backend-only
│   │   ├── emit/              # StreamingConfig.yaml + InferenceConfig.yaml
│   │   ├── query_log/         # extra L1 input: Prometheus query-log replay
│   │   └── schema/            # PromQLSchema discovery from Prometheus (feeds L1)
│   ├── scenario-fusion/       # thin — DF-flavored rules, 0-stage, in-process emit
│   │   ├── rules.rs           # L4 rules for DataFusion LogicalPlan (sketch-aware rewrites)
│   │   ├── topology.rs        # 0-stage: in-process
│   │   ├── emit/              # rewritten DataFusion LogicalPlan
│   │   ├── executor/          # DataFusion SessionContext wrapper (library-mode execution)
│   │   └── sketch_support/    # asap_sketchlib-backed rewrites
│   ├── control-proto/         # generated from proto/ (tonic/prost)
│   └── testing/               # test fixtures + harness shared across scenarios
└── bin/
    ├── asap-controller/       # long-running service with all scenarios
    │   └── main.rs            # axum + OpAMP + replanner + registered scenarios
    ├── asap-query/             # one-shot CLI for scenario-query (what asap-planner-rs is today)
    │   └── main.rs            # clap — read workload YAML, emit two YAMLs
    ├── asap-lifecycle/        # OPTIONAL: standalone service with only scenario-lifecycle
    │   └── main.rs            # slimmer image — no DataFusion, no query YAML emitters
    └── asap-fusion-bench/     # OPTIONAL: benchmark harness over scenario-fusion
        └── main.rs            # criterion entry; used by researchers
```

**Per-scenario standalone binaries** are first-class. Each `bin/<name>/` is a thin shell that `use`s only the scenario crates it needs — so `bin/asap-lifecycle/` doesn't pull `datafusion` into its dep tree, and `bin/asap-fusion-bench/` doesn't pull `axum`/`opamp`. Feature flags on the workspace root let you `cargo build -p asap-lifecycle` and get a minimal binary.

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

Each returns a language-flavored AST type. No sketch awareness.

### `core::logical_plan` — Layer 2

A **per-language** algebra tree — one `enum LogicalPlan` per language. Preserves language-specific semantics (PromQL instant vs range vector, SQL window frames, Elastic buckets) that would be lossy to collapse this early. Types are symmetric: `Aggregate { AggFunc }`, `Window`, `Filter`, `Sort`, `Limit`. No sketch names yet.

### `core::sketch_algebra` — Layer 3

The language- and deployment-independent IR. **Pure intent at this layer**: no language-specific operators (no `HistogramQuantile`, no `PromQLSubquery` — those are L2 PromQL nodes), no sketch types, no sketch parameters, no physical operator choice. Data-model-agnostic: supports both **time-series** inputs (ASAPQuery-backend, DC lifecycle) and **tabular** inputs (asap-fusion, future OLAP scenarios) via a `Source` sum type inside `QueryExpr::Scan`.

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
    /// Time-series input — scenario-query / scenario-lifecycle shape.
    TimeSeries { metric: MetricRef, time: TimeRange, labels: LabelFilter },
    /// Tabular input — scenario-fusion / future-OLAP shape.
    Table      { table_ref: TableRef, columns: Vec<ColumnRef> },
    /// Join over Sources composes leaf shapes recursively.
    Join       { left: Box<Source>, right: Box<Source>, on: JoinKey },
    // Future: WindowedStream, Subquery — added by scenarios that need them.
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
    /// Functional dependencies / unique-key sets. L4 reuses these to recognise
    /// when two sub-expressions produce identical streams (sketch-reuse driver).
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

Per-node input/output spec — the stable contract for L3 nodes (full implementation in `core/src/sketch_algebra/schema.rs`). Each row reads independently: every column position, type, and constraint is named explicitly rather than carried by a shorthand like `S` or `L` / `R`.

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

#### Three distinct schemas — DAG vs DB vs sketch catalog

These three are sometimes conflated and shouldn't be:

| Schema | Where it lives | What it describes | Who reads it |
|---|---|---|---|
| **DAG schema** | On every edge of the L3 / L4 / L5 DAG (`Schema` above) | Columns + types flowing between operators | L4 rules (selectivity estimation, push-down legality), L5 emitter |
| **DB / source schema** | The query target (Prometheus TSDB metric metadata, SQL `information_schema`, DataFusion catalog) | What metrics / tables / columns exist in the data plane, with their types and indexing | `core::lower::*` to resolve names during L1→L2; exposed through a `SchemaCatalog` interface |
| **Sketch catalog metadata** | `core::physical::sketch_catalog` (built at startup; static) | What sketches the runtime can build; what intents each one serves; mergeability, accuracy / confidence guarantees, supported aggregation keys, parameter ranges | L4 binding rules to choose a sketch for an `AggIntent`; L5 to instantiate the sketch |

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

    // Tabular / OLAP — added as scenarios demand
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

#### L4 sketch-bound IR — `SketchExpr`

L4 binding rules consume L3 `QueryExpr` and produce `SketchExpr`. This is the IR L5 emitters consume. The two-IR split — pure-logical L3 (`QueryExpr`) and sketch-bound L4 (`SketchExpr`) — gives L4 rule application a clean type signature: `fn apply(&QueryExpr, &Constraints) -> Option<SketchExpr>`, and the boundary cannot be silently violated.

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

**Why a `Source` sum instead of two parallel `QueryExpr` trees:** most `QueryExpr` nodes (`Filter`, `Aggregate`) are data-model-agnostic — filter semantics are the same whether the input is a time-series window or a table scan. Only the leaf `Scan` differs. Keeping one tree with a polymorphic leaf means L4 rules like `BindKllOnQuantile` work uniformly across both data models; rules that care about data-model specifics (stage-aware push-down for TS; join-selectivity for tabular) gate on `source.data_model()` + `intent.requires()`.

**Sketches are data-model-agnostic by construction.** KLL / CMS / HLL / DDSketch ingest a stream of values. That stream can come from a time-series window (`Source::TimeSeries`) or a table column (`Source::Table`); the sketch doesn't know or care. So `BindKllOnQuantile` works identically regardless of `Source`.

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

### `core::optimizer` — Layer 4 framework

Core ships the **rule engine + trait surface + a shared rule library**. Scenarios pick which rules to enable.

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

// shared rule library (in core::optimizer::rules) — opt-in from scenarios
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

Scenarios compose rule sets by picking from the shared library + adding their own:

```rust
// in scenario-lifecycle
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

Core also ships `DeploymentConstraints` as a trait object; each scenario supplies a concrete impl with its deployment's memory budgets, network topology, available sketch backends.

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

// generic stage allocator — given a QueryExpr tree + a topology, decide which
// ops land on which stage subject to constraints
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

Scenarios use these pieces:

```rust
// in scenario-lifecycle
use asap_control_core::physical::{StageAllocator, topology::ThreeStage};

impl PhysicalPlanner for LifecyclePlanner {
    type Topology = ThreeStage;
    type Output = (StagedExprs, SketchBindings);
    fn lower(&self, l4: Vec<QueryExpr>, t: &ThreeStage) -> Result<_, _> {
        let assignments = StageAllocator.allocate(&l4, t, &self.constraints)?;
        // scenario-specific post-processing: DC's delta_cost logic,
        // backend_client push preparation, etc.
        Ok(/* ... */)
    }
}
```

### `core::plan` — shared traits bridging layers

```rust
pub trait Scenario {
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

### `scenario-lifecycle` (thin)

- **Data model**: time-series. L2→L3 lowering produces `QueryExpr` with `Source::TimeSeries` leaves.
- **L4 rules**: picks from `core::optimizer::rules::*` (Bind*, Fusion*, Elim*) + adds DC-specific rules that require stage awareness (`StageAwarePushDown`, `TransmissionCostRewrite`). Adding a stage-specific rule = a new file in `scenario-lifecycle/src/rules.rs`, impl `OptimizerRule`. Unchanged rules come from core.
- **L5 topology**: `core::physical::topology::ThreeStage` (edge → gateway → backend). No scenario-specific allocator logic — `StageAllocator` handles the tree walk; lifecycle only declares what the topology looks like.
- **Cost models**: `delta / online-EMA / Pareto / TCO` — these live in `scenario-lifecycle/src/cost.rs` because they're specific to the DC deployment's network/compute assumptions. Implement `core::optimizer::cost::CostModel`.
- **L5 emitters**: `OpAmpRemoteConfigEmitter` (per-role OTel YAML over OpAMP WebSocket) and `AsapqueryBackendConfigEmitter` (calls `scenario-query`'s `StreamingConfigEmitter` for the YAML bytes, then POSTs to backend).
- **HTTP route**: `POST /plan` for full-lifecycle planning, `POST /replan` for SLA-triggered.
- **Size estimate**: ~1500 LOC (was ~5000 pre-refactor). Cost models are the bulk; rule selection + topology + emitter are each a few hundred lines.

### `scenario-query` (thin)

- **Data model**: time-series. L2→L3 lowering produces `QueryExpr` with `Source::TimeSeries` leaves.
- **Inherited L1**: uses `core::query_language::promql` and `core::query_language::sql`.
- **NEW L2 tree** (Phase 4 work): defines `PromqlLogicalPlan` in `core::logical_plan::promql` that expresses the five pattern shapes asap-planner-rs currently template-matches as first-class L2 nodes. Replaces the pattern-catalogue approach with a proper L1→L2 tree rewrite. SQL side gets a matching `SqlLogicalPlan`.
- **L3 intent**: maps `Statistic` enum (9 variants) onto `core::sketch_algebra::AggIntent` subset. Planner's `Topk` maps directly to `AggIntent::TopK` (heavy-hitter intent, served by SpaceSaving / CMS-with-heap at L4).
- **L4 rules**: picks from `core::optimizer::rules::*` (all `Bind*` rules are relevant since this scenario covers most sketch types) + a scenario-specific sketch-binding rule for the precompute engine's flavor (which sketches are available, what params, `DeltaSetAggregator` auto-injection before CMS/HydraKLL). This rule absorbs `map_statistic_to_precompute_operator`'s sketch-binding half.
- **L5 topology**: `core::physical::topology::SingleStage` (backend-only). No stage-split; `StageAllocator` returns everything on one stage trivially.
- **L5 emitters**: `StreamingConfigEmitter` + `InferenceConfigEmitter` (YAML bytes). **Authoritative** for these two formats — `scenario-lifecycle` calls them when it needs to POST to ASAPQuery-backend.
- **Extra L1 inputs**: `query_log/` for Prometheus-query-log replay (unique to this scenario); `schema/` for `PromQLSchema` discovery from a live Prometheus URL.
- **HTTP route**: `POST /plan/query` (JSON `QuerySpec` in, YAML stream out). Also backs the `bin/asap-query` one-shot CLI.
- **Size estimate**: ~2500 LOC (was ~6000 pre-refactor — saved by picking from shared rule library; still pays the L2 tree + L3/L4 split refactor cost, which is one-time).

### `scenario-fusion` (thin)

- **Data model**: tabular. L2→L3 lowering produces `QueryExpr` with `Source::Table` leaves (and, in future, `Source::Join` when fusion extends to multi-table). Crucially **not time-indexed** — fusion works on arbitrary DataFusion relations; time is just another column if present.
- **L1 opt-out**: fusion consumes a pre-built DataFusion `LogicalPlan` from its caller. `core::query_language` is not invoked. Library-mode scenario.
- **L2 inherited from DataFusion**: DataFusion's `LogicalPlan` *is* fusion's L2 tree. `core::logical_plan::datafusion` is a thin re-export of `datafusion::logical_expr::LogicalPlan` so scenarios that want to take a DataFusion plan as input have a canonical name for it.
- **L3 intent**: `SubPopulationAnalyticsType` (3 variants: `Count`, `Sum`, `Quantile`) maps to `core::sketch_algebra::AggIntent` subset.
- **L4 rules**: picks `BindCmsOnCount` and `BindKllOnQuantile` from `core::optimizer::rules::*` (which already cover fusion's `SketchConfigRule` semantics) + scenario-specific `HashModeRule`. Rules operate on DataFusion `LogicalPlan` (via Extension wrapping), not on `QueryExpr` trees — this lets DataFusion's `context.state().optimize` run *after* fusion's rewrites, keeping free reuse of DF's standard optimizer passes.
- **L5 topology**: `core::physical::topology::ZeroStage` (in-process).
- **L5 emitter**: rewritten `datafusion::LogicalPlan`. Not a wire format — this scenario is library-mode.
- **Executor**: `ASAPExecutor` wraps a DataFusion `SessionContext`. Users construct `scenario-fusion` in-process.
- **HTTP route**: none by default.
- **Conformance cost**: near zero. Fusion's `SketchConfigRule` logic is replaced with picks from `core::optimizer::rules::*`; its `HashModeRule` stays scenario-specific.
- **Size estimate**: ~1800 LOC (was ~3000 pre-refactor; the `SketchConfigRule` code folds into core's shared rule library).

The sketch microbenchmarks (KLL/CMS) move with the crate and keep running. The TODO items from `asap-fusion/TODO.md` (batch/multi-query execution, time semantics, distributed model) remain open but are now filed against `scenario-fusion/TODO.md` in-repo.

### Data plane communication

Control plane (ASAPController) and data plane (OTel agents / ASAPQuery-backend / DataFusion runtimes) always talk **over wire**, never via in-process calls. This is unchanged from today:

| Scenario | Data plane lives in | Wire protocol |
|---|---|---|
| lifecycle | DataCollector (OTel collectors, agent + backend roles) | OpAMP WebSocket (config push) + HTTP POST (`StreamingConfig` → ASAPQuery-backend) + Prometheus scrape (metrics in) |
| query | ASAPQuery-backend (query engine, SimpleMapStore) | HTTP POST `/api/v1/streaming-config` + `/api/v1/plan` (capability-miss callback in) + YAML file on disk (init-container mode) |
| fusion | Caller's DataFusion `SessionContext` | in-process library call (no wire) |

**Data plane code stays in its original repo.** ASAPController only owns the control plane. The merger doesn't move OTel collectors out of DataCollector, doesn't move the query engine out of ASAPQuery-backend, and doesn't move DataFusion out of asap-fusion's users. Each data plane keeps its own release cadence.

### Scenario placement: in-repo or out-of-repo

Because core owns the L4/L5 infrastructure (not just L1-3), a scenario crate is small and largely self-contained. That means scenarios can live either:

- **Inside ASAPController workspace** — `crates/scenario-<name>/`, lockstep release with core, cross-scenario changes are one PR.
- **In their own downstream repo** — declares `asap-control-core` as a git-tagged dep, releases independently, owns its own CI.

Both produce functionally identical artifacts because they pick from the same `core::optimizer::rules` library and use the same traits. The placement is a **deployment / team-ownership decision**, not an architectural fork.

Default recommendation:
- **scenario-lifecycle** and **scenario-query** in ASAPController (they share `scenario-query`'s YAML emitter via a workspace `path = "../scenario-query"` dep — trivial in-workspace).
- **scenario-fusion** out-of-tree in `asap-fusion` repo (research project with independent benchmark cadence; depends on `asap-control-core` + `asap-control-optimizer` as published git tags).

Future scenarios choose whichever placement fits the team that owns them.

### Asymmetric dependency

`scenario-lifecycle` depends on `scenario-query` because the `StreamingConfig` YAML emitter is authoritative there. When both live in ASAPController workspace this is a trivial `path =` dep. If one is ever moved out-of-tree, we lift the emitter into core to avoid a cross-repo Cargo dep.

## 9. Extension point — a 4th scenario

With the L4/L5 framework in core, adding a new scenario is mostly picking + a bit of glue. A hypothetical "edge caching" scenario that decides which queries to cache at the edge vs. backend would land as:

```
crates/scenario-edge-cache/                   # or your own repo
├── Cargo.toml                                 # depends on asap-control-core
├── src/
│   ├── lib.rs                                 # impl Scenario for EdgeCacheScenario
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
- optional: add `ScenarioId::EdgeCache` to `core::registry`

Typical size: ~500-2000 LOC depending on how scenario-specific the rules / cost model / emitter are. If the scenario accepts core's defaults everywhere, ~300 LOC is realistic.

### Standalone binary for one scenario

If you want a slim binary that runs only one scenario:

```
bin/asap-edge-cache/
├── Cargo.toml     # depends only on runtime + scenario-edge-cache, not other scenarios
└── src/main.rs    # registers only EdgeCacheScenario; no DataFusion, no query YAML
```

Cargo builds this with its own minimal dep tree — no `datafusion`, no `promql-parser` unless this scenario needs it. Useful when a deployment is dedicated to one scenario (e.g. an edge-caching-only service).

## 10. Wire protocols — what changes, what doesn't

**Unchanged** (hard contract with other systems):
- `POST /api/v1/streaming-config` on ASAPQuery-backend — consumed YAML format
- `POST /api/v1/plan` on DC controller (from backend capability-miss) — request JSON
- OpAMP `ServerToAgent.remote_config` payload — OTel collector YAML
- Prometheus scrape format

**Internal** (controller's own surface, still HTTP+JSON for now):
- `POST /plan` — new unified entry point that takes `QueryWorkload` in, returns a plan ID + list of emitted artifacts (URIs). `/api/v1/plan` proxies to this.
- `GET /plans/:id` — plan inspection
- `GET /status` — runtime + scenario health

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
    "crates/scenario-lifecycle",
    "crates/scenario-query",
    "crates/scenario-fusion",
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

4. **Do we vendor DataFusion's IR into core?** No. `scenario-fusion` owns its DataFusion-flavored plan; `core::plan::Plan` stays an enum with a variant that wraps a `datafusion::LogicalPlan` behind a feature flag. Core itself never reaches into DataFusion types.

5. **OpAMP proto: vendored, or from crates.io?** Today DC vendors. Recommend: keep vendored in `proto/opamp.proto`, generate via `prost-build` in `crates/control-proto`. Same thing DC does today, just moved.

6. **What happens to ASAPQuery-backend's `asap-planner-rs` directory post-migration?** Deleted. ASAPQuery-backend's docker-compose drops the `asap-planner-rs` init container; the controller's `scenario-query` now runs in-process (service mode) or as the `asap-query` CLI (one-shot mode). The ASAPQuery-backend repo shrinks by two directories.

7. **Versioning?** Start at `0.1.0` on the workspace. Scenarios can rev independently later via per-crate versions, but initially lockstep.

8. **What if a future scenario can't fit a tree L2?** L2 is currently a per-language tree, mandatory for every scenario. asap-planner-rs's Phase 4 conformance cost (reverse-engineering PromQL pattern templates into a `PromqlLogicalPlan` tree) was taken deliberately to keep the architecture uniform. If a future scenario's source language doesn't map naturally onto a tree (e.g. a constraint-based or dataflow-graph query language), that's the moment to re-examine the L2 contract — the `core::logical_plan` module is a single Rust trait + per-language types, not a deep assumption baked across the codebase. Until then, L2 = tree.

9. **Scenario placement — in ASAPController workspace, or in own repo?** Both supported, same architecture either way (see §8 "Scenario placement"). Default: `scenario-lifecycle` and `scenario-query` in ASAPController (easy cross-scenario changes, shared YAML emitter); `scenario-fusion` in the `asap-fusion` repo (research cadence). A new scenario picks based on team ownership and release cadence preferences.

10. **How far do shared L4/L5 rules live in core before they become scenario-specific?** The rule of thumb: if ≥2 current scenarios would use it, it lives in `core::optimizer::rules::*`. If only 1 scenario uses it *and* it depends on scenario-specific types (DataFusion's `LogicalPlan`, OTel YAML shape), it lives in the scenario crate. `SketchConfigRule`-style "bind intent to concrete sketch" rules belong in core (shared). `StageAwarePushDown` (which needs DC's stage graph) belongs in `scenario-lifecycle`. When a rule straddles — e.g. a fusion rewrite that *could* be generalized to any L4-compatible plan — start it in the scenario; lift to core once a second scenario wants it. Don't pre-emptively generalize.

11. **How does the design support non-time-series data (asap-fusion's tabular queries, future OLAP scenarios)?** Already handled — see §3 "Data-model support" and §6 `core::sketch_algebra`. `QueryExpr::Scan` wraps a `Source` sum (`TimeSeries` / `Table` / `Join`); `AggIntent::requires() -> DataModel` tags which intents apply to which data models; sketches themselves are data-model-agnostic. asap-fusion uses `Source::Table` exclusively; ASAPQuery uses `Source::TimeSeries`; a future OLAP scenario picks whichever fits. The only implementation cost is that L4 rules which genuinely don't apply across data models (e.g. "merge overlapping time windows") must gate on `source.data_model()`; data-model-agnostic rules (the `Bind*` family) need no changes.

### Resolved during the L3 IR cleanup (see §6 `core::sketch_algebra`)

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

## 14. Success criteria

The migration is done when:

1. `asap-controller` binary runs and passes DC controller's existing integration tests (OpAMP push, backend config POST, SLA replan).
2. `asap-query` binary takes the same YAML input asap-planner-rs does today and produces byte-identical `streaming_config.yaml` + `inference_config.yaml` (fuzz-test against a corpus of fixtures).
3. ASAPQuery-backend's docker-compose no longer starts `asap-planner-rs`; the controller handles both shapes.
4. `asap-fusion`'s microbenchmarks still run under `scenario-fusion` with identical numbers.
5. The `DataCollector/controller/`, `ASAPQuery/asap-planner-rs/`, `ASAPQuery-backend/asap-planner-rs/`, and `asap-fusion/` directories are deletable (or already deleted) without breaking any currently-running deployment.
6. A new hypothetical scenario can be added with zero changes outside its crate + one line in `bin/asap-controller/main.rs`.
