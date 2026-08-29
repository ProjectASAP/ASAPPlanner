# CSE Sharing and Summary-Maintenance Lifecycle Costing

## Context

Common-subexpression elimination (CSE) discovers when several consumers can
reuse one semantically equivalent computation. In ASAPPlanner, reuse may
involve summary state, so structural sharing alone is not enough to decide
whether sharing is desirable.

The planner must keep three questions separate:

1. **Legality:** may these consumers share this computation?
2. **Semantic choice:** should the candidate plan share one result or compute
   independently?
3. **Physical lifecycle:** if the result is shared, should its summary state be
   built once, prepared, retained for a bounded period, read from existing
   state, or incrementally maintained?

The first question is not a cost decision. The second and third must be costed
together over the query and data workloads.

This document refines the original cost-based decision from issues #223 and
#237 using the workload and summary-maintenance lifecycle model in PR #300.
See also the [overall planner design](README.md) and the detailed
[workload/lifecycle design](asap-aware-mapping/workload-demand-and-summary-lifecycle.md).

## Terminology

**Summary maintenance lifecycle** means the lifetime of planner-selected
summary state: build, prepare, share, incrementally maintain, read, and
retire. A materialized plan records its **summary maintenance lifecycle
guarantees**.

This is distinct from the broader **data lifecycle**, which covers data
collection, transmission, storage, and analytics.

## Decision

Use cost-based selection over compatible whole-plan and lifecycle
combinations. CSE detection contributes alternatives to the semantic
candidate space; it does not materialize shared state and does not choose a
lifecycle.

```text
legally shareable sub-DAG
    -> semantic alternatives:
         share one computation
         recompute independently
    -> for each summary-bearing alternative, legal maintenance lifecycles:
         ephemeral / build once
         prepared
         bounded shared
         continuously maintained
         compatible existing state
    -> accuracy and phase validation
    -> lifecycle-aware cost over H
    -> global selection
    -> materialization and lifecycle guarantees
```

The optimizer's decision unit is therefore:

```text
complete Post-ASAP candidate plan
    × compatible summary-maintenance lifecycle assignment
```

It is not sound to choose `Share` using a generic maintenance weight and then
attach a lifecycle afterward. The lifecycle changes the cost and can reverse
the decision.

## Legality remains cost-independent

[`share_common_subtrees`](../../crates/types/src/pre_asap/cse.rs) detects
structurally identical, legally shareable Pre-ASAP sub-DAGs. Schema and unique-
key rules remain in `asap-types`; that lower layer must not depend on the
mapping layer's `CostModel`.

Detection therefore answers only:

> Is sharing a valid alternative?

It must not answer:

> Is sharing cheaper for this workload?

Both share and recompute alternatives remain visible after detection. Costing
orders legal alternatives but never grants permission to violate semantics,
schema, phase constraints, summary capabilities, or accuracy requirements.

## Workload inputs

CSE costing consumes normalized evidence from three inputs:

- **query workload:** one-time and repeated demand, predictability, evaluation
  rate, concurrency, accuracy and latency requirements, and concrete time
  selection;
- **data workload:** at-rest, continuously ingesting, mixed, or unknown
  arrival, plus fresh ingestion-rate, volume, cardinality, and distribution
  evidence;
- **planning horizon `H`:** the finite interval over which one-time costs and
  cost rates are compared.

Repeated query demand does not imply continuously arriving data. Likewise,
two structural consumers do not imply two executions: recurrence and
effective uses must be derived from the bound workload.

Missing or stale evidence remains unknown. It is never converted to zero
maintenance cost, infinite reuse, or continuous ingestion.

## Lifecycle-aware alternatives

### Recompute independently

For repeated raw recomputation over horizon `H`:

```text
recompute_total(H)
    = reads(H) × raw_recompute_cost_per_read
```

For one-time consumers, `reads(H)` includes their explicit invocation count.
No update or retention cost is invented for data at rest.

### Build once or ephemeral sharing

One shared result may be built for a bounded invocation set and retired when
those reads finish:

```text
ephemeral_shared_total
    = build_cost
    + reads × summary_read_cost
    + retirement_cost
```

This can beat independent recomputation without implying continuous
maintenance.

### Prepared or bounded shared state

Predictable demand can justify preparing state before execution or retaining
it for a declared window. Cost includes the applicable build, read, retention,
update, and retirement terms only for that window. Prepared state is legal
only when every consumer relying on it is covered by the activation interval.

### Continuously maintained sharing

For incrementally maintained state over horizon `H`:

```text
maintained_shared_total(H)
    = build_cost
    + H × update_rate × maintenance_cost_per_update
    + reads(H) × summary_read_cost
    + H × retention_cost_rate
    + retirement_cost
```

This alternative requires continuously arriving or mixed data, fresh update-
rate evidence, runtime maintenance support, and incremental-update support for
the chosen summary. Moving real-time windows additionally require deletion or
an equivalent expiry capability.

### Existing materialized state

An existing summary is a separate alternative, not a zero-cost version of a
new summary. Its catalog entry must establish compatibility, ownership,
freshness, accuracy guarantee, representation, and summary maintenance
lifecycle guarantees. Only then may costing omit a new build term.

## Accuracy and nested plans

Sharing does not weaken accuracy requirements. Every complete candidate plan
must propagate guarantees through nested summaries, shared consumers, and
post-processing. A shared result is legal only when its end-to-end guarantee
satisfies every consumer that depends on it.

For consumers with different accuracy requirements, the optimizer may compare:

- one shared summary satisfying the tightest compatible requirement;
- several summaries with different parameters;
- independent raw recomputation;
- another semantic rewrite or roll-up.

Unknown or unsupported guarantee propagation rejects the affected candidate
before cost ranking.

## Cost model and optimizer responsibilities

The cost model and optimizer have different contracts.

The **cost model**:

- supplies build, maintenance, read, retention, retirement, and raw-recompute
  estimates;
- preserves the distinction between one-time `Cost` and recurring `CostRate`;
- evaluates every comparable alternative over the same explicit `H`;
- reports unknown inputs as unknown.

The **global optimizer**:

- enumerates compatible share/recompute and lifecycle combinations;
- accounts for one shared state deployment only once;
- prevents incompatible consumers or nested phases from being combined;
- ranks complete plan-and-lifecycle combinations;
- retains raw recomputation as an explicit fallback;
- records rejected alternatives and their reasons.

The cost model estimates; it does not filter candidates or decide semantic
legality.

## Materialization commitment

Selection happens before materialization. Once the final plan materializes a
particular summary family and maintenance lifecycle, the runtime must honor
that commitment.

For example, if the selected shared deployment is an incrementally maintained
KLL, the runtime cannot silently maintain DDSketch instead. A later replan may
choose DDSketch, but deployment must explicitly build or migrate its state,
cut over readers, and retire the KLL state. This commitment is represented by
the final plan's summary maintenance lifecycle guarantees.

## Current implementation status in PR #300

The current implementation provides the pieces needed for this design:

- CSE legality and share/recompute alternatives;
- workload-derived recurrence and update-rate evidence;
- explicit `Cost`, `CostRate`, and `Horizon` types;
- summary-maintenance lifecycle alternatives and capability checks;
- lifecycle cost inputs on the common `CostModel`;
- typed `SummaryMaintenanceLifecycleGuarantee` output;
- raw recomputation fallback.

The integration is not yet a full joint optimizer. Today, semantic global
selection chooses a plan first; lifecycle planning then selects deployments
for that materialized plan and may replace it with raw recomputation. It does
not yet reconsider every sibling semantic candidate using lifecycle-aware
whole-plan cost.

The target integration is to move lifecycle expansion and cost evaluation
before the final global selection, so summary family, CSE sharing, and
maintenance lifecycle are selected together. Until that integration lands,
documentation and explanations must distinguish the target design from the
currently implemented selection boundary.

## Defaults and fail-closed behavior

Structural node counts and family weights may remain explicit heuristic
defaults for tests and deployments without measured statistics, but they must
not masquerade as observed lifecycle cost. A deployment with measured data
overrides the corresponding `CostModel` hooks.

The following never make a long-lived shared summary win by assumption:

- missing horizon when one-time and rate costs must be combined;
- missing or stale update-rate evidence;
- missing build, read, maintenance, retention, or retirement cost evidence;
- unknown runtime or summary maintenance capabilities;
- unknown accuracy propagation.

In those cases the optimizer preserves an explainable conservative fallback
or leaves the alternative uncosted.
