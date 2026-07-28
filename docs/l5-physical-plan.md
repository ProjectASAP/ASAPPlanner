# L5 — physical plan

**Status: an interface design, not a concrete implementation.**
Everything below describes the intended shape of this layer — what a
deployment must implement — rather than a shipped component. This
layer's concrete realization is inherently deployment-specific: a
downstream deployment supplies its own instance of it.

## What L5 is for

L3 and L4 are symbolic — they describe computation and summary choices
without committing to *where* anything runs. L5 produces the concrete,
placed plan: which physical stage each piece of computation runs at,
how much parallelism it gets, and — ultimately — a form a specific
deployment can act on (e.g. serialize into that deployment's own
configuration format). The same summary-bound plan, combined with a
different topology, is meant to produce a different placement without
re-running any earlier layer — topology is a parameter to this layer,
not a fork in its logic.

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

1. **Stage-level allocation** — given the summary-bound plan, the
   topology, and deployment constraints, decide which *stage* each
   piece of the plan runs at. This decision is stage-level only —
   generic across deployments, since it only needs to know which
   stages exist and how they connect, not which concrete executors a
   deployment happens to have.
2. **Per-executor fan-out** — given a stage-level assignment, decide
   which concrete executor(s) at that stage actually run each piece.
   This is deployment-specific, since it needs the deployment's own
   executor list and its own policy for spreading work across them.

Stage-level allocation is also where a summary merge is meant to get
inserted — wherever a summary-bound plan is cut across a stage
boundary that produces more than one instance of the same summary
state, those instances need to be merged before serving can use them
as a single value (see
[`l4-summary-bound-ir.md`](./l4-summary-bound-ir.md)).

## Summary catalog

A registry of what summaries exist, keyed by summary family: which
intents each can serve, whether it supports merging/deletion/subtraction,
its accuracy model, what grouping shapes it supports, its valid
parameter ranges, and its cost characteristics. Built once, ahead of
time; read by L4's binding decision (to choose a family) and by L5 (to
instantiate it). Rejecting an out-of-range parameter belongs here, at
catalog level, so that L5 never has to handle an unsupported
configuration reaching it. This catalog is meant to be shared
infrastructure that every deployment reads from, rather than something
each deployment reinvents independently — though which summary
families and parameter ranges are actually registered is itself
deployment policy.

## Interface

Speculative — nothing here is built (see this doc's status note at the
top). Kept as a concrete target to design against, not as an API to
depend on:

```rust
pub trait PhysicalPlanner {
    type Topology: TopologyDescriptor;
    type Output;
    fn lower(&self, l4: Rc<L4Node>, t: &Self::Topology) -> Result<Self::Output, PlanError>;
}

pub trait TopologyDescriptor {
    fn stages(&self) -> &[StageDescriptor];
    fn edges(&self) -> &[StageEdge];
}

pub struct StageId(pub String);   // "edge" / "gateway" / "backend" / "in-process"

pub struct Executor {
    pub id: ExecutorId,
    pub stage: StageId,
    pub capabilities: ExecutorCaps,   // memory budget, available summary backends, network neighbours
    pub address: ExecutorAddr,        // OpAMP agent / HTTP endpoint / in-process handle
}

// Stage-level allocation: given an L4 tree + a topology, decide which
// nodes land on which stage, subject to deployment constraints.
// Per-executor fan-out is the deployment model's own PhysicalPlanner,
// using the executor list from DeploymentConstraints::executors().
pub struct StageAllocator;
impl StageAllocator {
    pub fn allocate<T: TopologyDescriptor>(
        &self, l4: Rc<L4Node>, topology: &T, c: &DeploymentConstraints,
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
    fn lower(&self, l4: Rc<L4Node>, t: &ThreeStage) -> Result<Self::Output, PlanError> {
        let assignments = StageAllocator.allocate(l4, t, &self.constraints)?;
        let executors: &[Executor] = self.constraints.executors();
        // deployment-specific post-processing (cost accounting, backend
        // push preparation, ...) happens here.
        Ok(/* ... */)
    }
}
```
