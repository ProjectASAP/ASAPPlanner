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

## 3. The organizing spine: DC controller's 5-layer pipeline

DataCollector/controller already documents its query→sketch translation as a 5-layer pipeline (`DataCollector/controller/docs/query-to-sketch-translation.md`). That's the spine of this merger:

| # | Layer | What it does | Today's locations |
|---|-------|--------------|-------------------|
| 1 | **Query Language** | Parse raw strings (PromQL, SQL, DataFusion, ElasticDSL, …) into a language-specific AST | DC `controller/src/query_parser/{promql,sql,mod}.rs`; asap-planner-rs pulls `promql-parser` + `sqlparser` directly; asap-fusion consumes a pre-built DataFusion `LogicalPlan` (its L1 happens upstream) |
| 2 | **Language Logical Plan** | Per-language algebra tree (`Aggregate` / `Window` / `Filter` / `Sort` / `Limit`) preserving language semantics, **no sketch names, no sketch binding** | DC `controller/src/algebra/lower.rs`; asap-fusion inherits DataFusion's `LogicalPlan` as its L2; asap-planner-rs has no L2 today (uses a template-pattern catalogue) — **Phase 4 builds one** |
| 3 | **Sketch Logical Plan** (sketch algebra) | Language- and deployment-independent IR: `QueryExpr` + `AggIntent` (~25 intent variants). Describes **intent only** — *what* to compute, with accuracy target. **No sketch type, no sketch parameters.** Data-model-agnostic — `QueryExpr::Scan` wraps a `Source` sum with `TimeSeries` / `Table` / `Join` variants so the same L3 IR covers ASAPQuery's time-series queries and asap-fusion's tabular queries. | DC `controller/src/algebra/{expr,directory}.rs`; asap-fusion's 3-variant `SubPopulationAnalyticsType` maps to a subset; asap-planner-rs's 9-variant `Statistic` maps to a subset — **Phase 4 splits sketch binding out of planner's current fused L3+L4** |
| 4 | **Sketch Optimizer** | Cost-aware algebraic rewrite rules under deployment constraints. **This is where sketch binding happens** — L4 rules take intent-only L3 and emit sketch-bound L3+. ~12 rules in DC; a smaller targeted subset in planner; `SketchConfigRule` + `HashModeRule` in fusion. | Core provides the **rule engine driver** + `OptimizerRule` trait + a shared rule library; scenarios **pick** which rules to enable + supply their own deployment constraints. DC `controller/src/algebra/optimizer.rs` (rules); fusion `src/optimizer/rules/`; planner's `map_statistic_to_precompute_operator` |
| 5 | **Physical Execution Plan** | Assign ops to pipeline stages (edge / gateway / backend / object store); produce the deployment-specific artefact (OpAMP YAML, `streaming_config.yaml`, rewritten DataFusion `LogicalPlan`). **Sketch binding is already committed by L4**; L5 is about stage allocation + emission. | Core provides the **stage allocator framework** + `PhysicalPlanner` trait + the sketch catalogue; scenarios supply their own **topology** (3-stage / 1-stage / 0-stage) + their own **emitter** for the output format. DC `controller/src/algebra/{physical,allocator,plan}.rs`; asap-planner-rs `output/generator.rs`; asap-fusion `src/executor/` |

**The doc's key claim: layers 1–3 are query-language-independent and workload-independent.** That makes them the natural **common core**. Every scenario reads PromQL (or SQL, or …) the same way, lowers it to the same per-language algebra, and lowers THAT to the same sketch-algebra IR (intent only, no sketch binding).

**L4 and L5 also have substantial common infrastructure.** Initially we assumed scenarios owned L4/L5 wholesale; on closer inspection what's actually scenario-specific is *which rules fire* (L4) and *what topology + output format* (L5) — not the rule engine, not the allocator, not the sketch catalogue. Those frameworks belong in core. This makes scenarios significantly thinner: each becomes a small crate that picks rules from a shared library, declares a deployment topology, and writes an emitter.

### Sketch binding lives in L4, not L3

A key clarification after cross-checking the three source repos: **L3 is intent-only**. DC's `AggIntent` names *what* to compute (`Quantile(0.99, ε=0.01)`, `Cardinality(δ=0.001)`, …) without committing to a sketch type. Picking KLL vs DDSketch, CMS vs CMS-with-heap, parameter sizes — all of that is L4's job, driven by deployment constraints.

Today:
- DC: correctly separated (L3 has `AggIntent`, L4 picks sketch via cost model).
- asap-fusion: correctly separated — `SketchConfigRule` at L4 fills `SketchConfig::NULL` with concrete `CountMinSketch{5,4096}` / `KLL{k=200,m=8}`.
- asap-planner-rs: **L3+L4 fused today**. `map_statistic_to_precompute_operator` jumps from `Statistic` straight to `AggregationType::DatasketchesKLL{k=200}` in one call. **Phase 4 splits this** — `Statistic → AggIntent` at L3, `AggIntent + DeploymentConstraints → AggregationType + SketchParams` at L4.

### Intent vocabulary: DC's `AggIntent` is a superset; scenarios use subsets

DC has ~25 variants. Planner uses 9 (`Count, Sum, Cardinality, Increase, Rate, Min, Max, Quantile, Topk`). Fusion uses 3 (`Count, Sum, Quantile`). A scenario's L4+L5 only has to handle its own subset — but it reads and writes the **same `AggIntent` enum**. Adding a new intent (e.g. stddev) is a core change that scenarios can then opt in to.

### Data-model support: both time-series and tabular

ASAPQuery-backend / DC controller operate on time-series data (metrics + labels + timestamp); asap-fusion operates on tabular data (DataFusion `LogicalPlan` over relations) that may or may not be time-indexed. These two data models differ fundamentally in their leaf shape (`metric + labels + time` vs. `table + columns`), but they share everything above the leaf — filter semantics, aggregation semantics, sketches themselves.

Core handles this with:
- **`QueryExpr::Scan { source: Source, ... }`** where `Source` is a sum type (`TimeSeries`, `Table`, `Join`, …). Scenarios' L1→L2→L3 lowering produces the appropriate variant; L4 rules that care about the data model gate on `source.data_model()`.
- **`AggIntent::requires() -> DataModel`** — each intent variant tags whether it's data-model-agnostic (`Count`, `Sum`, `Quantile`, `Cardinality`), time-series-only (`Rate`, `Increase`, `QuantileOverTime`), or tabular-only (future additions for joins, correlated subqueries).
- **Sketches are data-model-agnostic by construction.** KLL / CMS / HLL / DDSketch ingest a stream of values; that stream can come from a time-series window or a table column, the sketch does not know or care. So `BindKllOnQuantile` and siblings work uniformly across both.

Practically, this means:
- `scenario-query` + `scenario-lifecycle` lower into `QueryExpr` with `Source::TimeSeries` leaves.
- `scenario-fusion` lowers into `QueryExpr` with `Source::Table` leaves (and, in future, `Source::Join` when it extends to multi-table queries).
- A hypothetical OLAP scenario that runs approximate queries over tabular data reuses `Source::Table` + the same `AggIntent` subset fusion uses, plus any OLAP-specific intents it adds.

See §6 `core::sketch_algebra` for the concrete type sketches.

### L2 is mandatory; the tree shape is an evolvable contract

Every scenario must produce an L2 tree, even when the source language didn't originally come as one. asap-planner-rs's current approach (PromQL pattern catalogue → `IntermediateAggConfig`) skips L2; Phase 4 will reverse-engineer the five PromQL pattern shapes into a `PromqlLogicalPlan` tree so the L1→L2→L3 pipeline is uniform.

A future scenario whose source semantics genuinely don't fit a tree (e.g. a constraint-based query language) would motivate revisiting the L2 contract at that time. Until then, L2 = per-language tree, mandatory, no elision.

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
│   │   ├── sketch_algebra/    # L3: QueryExpr + AggIntent (~25 variants) + sketch directory
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

The language- and deployment-independent IR. Data-model-agnostic: supports both **time-series** inputs (ASAPQuery-backend, DC lifecycle) and **tabular** inputs (asap-fusion, future OLAP scenarios) via a `Source` sum type inside `QueryExpr::Scan`.

```rust
pub enum QueryExpr {
    Scan { source: Source, predicates: Vec<Predicate> },
    Filter { child: Box<QueryExpr>, pred: Predicate },
    Aggregate { child: Box<QueryExpr>, by: Vec<GroupKey>, intent: AggIntent },
    // ...
}

pub enum Source {
    /// Time-series input — scenario-query / scenario-lifecycle shape.
    /// `metric` identifies a metric family; `time` bounds the window;
    /// `labels` constrains label-value combinations.
    TimeSeries {
        metric: MetricRef,
        time: TimeRange,
        labels: LabelFilter,
    },
    /// Tabular input — scenario-fusion / future-OLAP shape.
    /// `table_ref` identifies the table; `columns` projects the subset
    /// in use. Join / subquery composition nests `Source` values.
    Table {
        table_ref: TableRef,
        columns: Vec<ColumnRef>,
    },
    Join {
        left: Box<Source>,
        right: Box<Source>,
        on: JoinKey,
    },
    // Future: WindowedStream, Subquery — added by scenarios that need them.
}

pub enum DataModel {
    TimeSeries,
    Tabular,
    Any,
}

impl Source {
    pub fn data_model(&self) -> DataModel {
        match self {
            Self::TimeSeries { .. } => DataModel::TimeSeries,
            Self::Table { .. } | Self::Join { .. } => DataModel::Tabular,
        }
    }
}

pub enum AggIntent {
    // Data-model-agnostic — work on TimeSeries AND Tabular
    Count { accuracy: AccuracyTarget },
    Sum,
    Min, Max,
    Quantile { q: f64, accuracy: AccuracyTarget },
    TopK { k: usize, accuracy: AccuracyTarget },
    Cardinality { accuracy: AccuracyTarget },

    // Time-series only
    Rate { window: Duration },
    Increase { window: Duration },
    QuantileOverTime { q: f64, window: Duration, accuracy: AccuracyTarget },

    // Tabular / OLAP only — added as scenarios demand
    // CorrelatedSubqueryCount { ... },
    // ApproxJoinCardinality { ... },
    // ~25 variants total
}

impl AggIntent {
    /// Which data-model this intent semantically requires. L4 rules
    /// consult this to skip non-applicable intents (e.g. `Rate` over
    /// a `Source::Table` is nonsense).
    pub fn requires(&self) -> DataModel {
        match self {
            Self::Count{..} | Self::Sum | Self::Min | Self::Max
                | Self::Quantile{..} | Self::TopK{..} | Self::Cardinality{..}
                => DataModel::Any,
            Self::Rate{..} | Self::Increase{..} | Self::QuantileOverTime{..}
                => DataModel::TimeSeries,
        }
    }
}
```

`AggIntent` describes **what** to compute (a quantile, a topk, a count) + what accuracy to hit — not **how** (KLL vs DDSketch vs CMS). Sketch binding happens at L4.

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

// sketch catalogue — what sketches exist, what params they accept
pub struct SketchCatalog { /* built at startup; queried by L4 binding rules + L5 */ }
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
- **L3 intent**: maps `Statistic` enum (9 variants) onto `core::sketch_algebra::AggIntent` subset.
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

Both produce functionally identical artefacts because they pick from the same `core::optimizer::rules` library and use the same traits. The placement is a **deployment / team-ownership decision**, not an architectural fork.

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
- `POST /plan` — new unified entry point that takes `QueryWorkload` in, returns a plan ID + list of emitted artefacts (URIs). `/api/v1/plan` proxies to this.
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

## 13. Success criteria

The migration is done when:

1. `asap-controller` binary runs and passes DC controller's existing integration tests (OpAMP push, backend config POST, SLA replan).
2. `asap-query` binary takes the same YAML input asap-planner-rs does today and produces byte-identical `streaming_config.yaml` + `inference_config.yaml` (fuzz-test against a corpus of fixtures).
3. ASAPQuery-backend's docker-compose no longer starts `asap-planner-rs`; the controller handles both shapes.
4. `asap-fusion`'s microbenchmarks still run under `scenario-fusion` with identical numbers.
5. The `DataCollector/controller/`, `ASAPQuery/asap-planner-rs/`, `ASAPQuery-backend/asap-planner-rs/`, and `asap-fusion/` directories are deletable (or already deleted) without breaking any currently-running deployment.
6. A new hypothetical scenario can be added with zero changes outside its crate + one line in `bin/asap-controller/main.rs`.
