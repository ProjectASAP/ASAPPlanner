# L4 — physical plan

**Status: an interface design.** Everything below describes the
intended shape of this layer — what a deployment must implement. This
layer's concrete realization is inherently deployment-specific: a
downstream deployment supplies its own instance of it.

## What L4 is for

L2 and L3 are symbolic — they describe computation and summary choices
without committing to *where* anything runs. L4 produces the concrete,
placed plan: which physical stage each piece of computation runs at,
how much parallelism it gets, and — ultimately — a form a specific
deployment can act on (e.g. serialize into that deployment's own
configuration format). The same summary-bound plan, combined with a
different topology, is meant to produce a different placement through
the same logic — topology is a parameter this layer's logic takes, not
something earlier layers need to re-run for.

## Core concepts

- **Physical planner** — the interface a deployment implements: given a
  summary-bound plan and a topology, produce that deployment's own
  physical output.
- **Topology** — which physical stages exist (e.g. edge, gateway,
  backend, or a single in-process stage) and how they connect. A
  deployment supplies its own topology; the physical planner interface
  is generic over it.
- **Stage** — a categorical placement tier (e.g. "edge," "backend").
- **Executor** — a concrete runtime instance that executes a piece of
  the plan, situated at one stage. A single stage can back many
  executors (e.g. a large edge fleet all at the "edge" stage); a
  singleton stage backs exactly one.
- **Deployment constraints** — the per-deployment budgets and
  capabilities (memory, available summary backends, network topology,
  the concrete list of executors) that placement must respect.

## Division of responsibility

Placement is meant to split into two decisions:

1. **`stage-allocate`** — given the summary-bound plan, the topology,
   and deployment constraints, decide which *stage* each piece of the
   plan runs at. Generic across deployments: it only needs to know
   which stages exist and how they connect, not which concrete
   executors a deployment happens to have.
2. **`physical-lower`** — given a stage-level assignment, decide which
   concrete executor(s) at that stage actually run each piece, and
   emit the deployment's own output format. Deployment-specific: it
   uses the deployment's own executor list and its own policy for
   spreading work across them.

`stage-allocate` is also where a summary merge is meant to get
inserted — wherever a summary-bound plan is cut across a stage
boundary that produces more than one instance of the same summary
state, those instances need to be merged before serving can use them
as a single value (see
[`l3-summary-bound-ir.md`](./l3-summary-bound-ir.md)).

## Summary catalog

A registry of what summaries exist, keyed by summary family: which
intents each can serve, whether it supports merging/deletion/subtraction,
its accuracy model, what grouping shapes it supports, its valid
parameter ranges, and its cost characteristics. Built once, ahead of
time; read by L3's binding decision (to choose a family) and by L4 (to
instantiate it). Rejecting an out-of-range parameter belongs here, at
catalog level, so L4 only ever receives an already-valid configuration. This catalog is meant to be shared
infrastructure that every deployment reads from, rather than something
each deployment reinvents independently — though which summary
families and parameter ranges are actually registered is itself
deployment policy.

## Interface

Speculative (see this doc's status note at the top) — kept as a
concrete target to design against. Every concrete-looking name below
(`StageId`'s example values, `Executor`'s addressing, an actual
`TopologyDescriptor`) is **interface only, in this repo** —
ASAPController defines the shape a deployment implements against; the
deployment defines, reserves, and ships the actual values, names its
own stages, addresses its own executors, and describes its own
topology:

```rust
pub trait PhysicalPlanner {
    type Topology: TopologyDescriptor;
    type Output;
    fn lower(&self, l3: Rc<L3Node>, t: &Self::Topology) -> Result<Self::Output, PlanError>;
}

pub trait TopologyDescriptor {
    fn stages(&self) -> &[StageDescriptor];
    // A `StageEdge` names a pair of stages data is allowed to flow
    // between (e.g. "edge → backend" if that deployment's edge tier
    // ships summaries up to a backend tier) — the topology's connectivity
    // graph, distinct from which stages merely *exist* (`stages()` above).
    // `StageAllocator::allocate` only assigns a piece of the plan to move
    // from one stage to another along an edge this list actually
    // contains; a `TopologyDescriptor` with no edge between two stages is
    // how a deployment declares those two stages can't exchange data
    // directly.
    fn edges(&self) -> &[StageEdge];
}

// `StageId` is an opaque, deployment-chosen string — ASAPController
// neither defines nor reserves any particular value. "edge" / "gateway"
// / "backend" / "in-process" below are one deployment's illustrative
// choice (roughly: close to data ingestion / an intermediate
// aggregation tier / a centralized serving tier / no network hop at
// all), not a fixed enum — a different deployment can and should name
// its own stages differently.
pub struct StageId(pub String);

pub struct Executor {
    pub id: ExecutorId,
    pub stage: StageId,
    pub capabilities: ExecutorCaps,   // memory budget, available summary backends, network neighbours
    pub address: ExecutorAddr,        // OpAMP agent / HTTP endpoint / in-process handle — deployment-defined
}

// Stage-level allocation: given an L3 tree + a topology, decide which
// nodes land on which stage, subject to deployment constraints.
// Per-executor fan-out is the deployment model's own PhysicalPlanner,
// using the executor list from DeploymentConstraints::executors().
pub struct StageAllocator;
impl StageAllocator {
    pub fn allocate<T: TopologyDescriptor>(
        &self, l3: Rc<L3Node>, topology: &T, c: &DeploymentConstraints,
    ) -> Result<Vec<StageAssignment>, PlanError>;
}
```

A deployment model implements `PhysicalPlanner`, delegating the
stage-level decision to `StageAllocator` and handling its own
per-executor fan-out on top:

```rust
impl PhysicalPlanner for LifecyclePlanner {
    type Topology = ThreeStage;
    type Output = Vec<(ExecutorId, ExecutorPlan)>;
    fn lower(&self, l3: Rc<L3Node>, t: &ThreeStage) -> Result<Self::Output, PlanError> {
        let assignments = StageAllocator.allocate(l3, t, &self.constraints)?;
        let executors: &[Executor] = self.constraints.executors();
        // deployment-specific post-processing (cost accounting, backend
        // push preparation, ...) happens here.
        Ok(/* ... */)
    }
}
```
