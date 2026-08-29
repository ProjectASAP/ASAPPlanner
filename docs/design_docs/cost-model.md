# Cost Model

## Purpose

The cost model estimates the resource cost of every legal ASAPPlanner
alternative in a common currency. The global optimizer uses those estimates to
select a compatible Post-ASAP plan and a summary-maintenance lifecycle for
each stateful node.

The unit of optimization is:

```text
complete Post-ASAP candidate plan
    × compatible summary-maintenance lifecycle assignment
```

Cost must be evaluated after semantic, schema, phase, capability, and accuracy
validation, but before final selection and materialization.

See the [overall planner design](README.md) and the
[workload and summary-maintenance lifecycle design](asap-aware-mapping/workload-demand-and-summary-lifecycle.md).

## Responsibilities

The cost model is responsible for:

- defining typed one-time and recurring cost units;
- consuming workload, data, candidate, summary, and runtime evidence;
- estimating primitive build, update, read, retention, retirement, transfer,
  and raw-computation costs;
- calculating lifecycle-aware costs over one explicit planning horizon;
- estimating costs for nested and phase-composed plans;
- accounting for shared state once rather than once per reference;
- returning comparable estimates with provenance and assumptions;
- preserving unknown or stale inputs as unknown;
- providing estimates for every legal alternative without filtering the
  candidate set.

The cost model is not responsible for:

- parsing query languages or constructing the Pre-ASAP DAG;
- deciding semantic equivalence;
- deciding schema, phase, or summary-capability legality;
- deriving or approving accuracy guarantees;
- generating candidate plans;
- choosing which compatible alternatives form the final plan;
- materializing summaries, scheduling jobs, or assigning machines;
- changing a materialized summary family without explicit replanning.

Those responsibilities belong respectively to the frontend, semantic mapping,
correctness and accuracy models, global optimizer, and deployment/runtime
layers.

## Cost vocabulary and units

One-time costs and cost rates are different types and must not be added
directly.

| Quantity | Unit | Meaning |
|---|---|---|
| `Cost` | cost units | A one-time action such as build or retirement |
| `CostRate` | cost units/second | A recurring cost such as retention or steady maintenance |
| `EvaluationRate` | evaluations/second | How often consumers read a result |
| `UpdateRate` | updates/second | How often incoming data changes maintained state |
| `Horizon` (`H`) | seconds | The interval over which recurring and one-time alternatives are compared |

The only valid conversion from a rate to a comparable total is:

```text
total_cost(H) = one_time_cost + H × recurring_cost_rate
```

`H` must be finite and strictly positive. Every alternative in one comparison
uses the same `H`. A latency requirement is not a horizon: latency constrains
one query result, while `H` determines how much future activity is included in
the economic comparison.

## Factors that determine cost

### Query workload

Query workload determines:

- one-time invocation count;
- fixed, scheduled, or estimated repeated demand;
- evaluation rate and expected reads within `H`;
- effective consumer count after sharing decisions;
- predictability and preparation windows;
- concurrency and peak demand when supplied;
- per-query latency and accuracy requirements;
- real-time, longitudinal, mixed, or unknown time scope;
- lookback and concrete `as_of` selection.

For fixed repeating intervals `t_i`:

```text
evaluation_rate = Σ_i (1 / t_i)
reads(H) = one_time_invocations + H × evaluation_rate
```

Scheduled demand counts only executions inside `H`. Estimated demand may be
used only while its evidence is fresh. Structural references are not a
substitute for execution frequency.

### Data workload

Data workload determines:

- whether data is at rest, continuously ingesting, mixed, or unknown;
- update rate and update count within `H`;
- ingestion volume and input cardinality;
- data distribution and skew;
- whether a moving real-time window requires deletion or expiry;
- whether evidence is fresh enough to use.

For maintained state:

```text
updates(H) = H × update_rate
```

Repeated queries do not imply continuous data. Data at rest contributes no
invented update cost. Unknown arrival or stale ingestion evidence cannot make
continuous maintenance appear free.

### Candidate plan structure

Cost depends on the complete Post-ASAP DAG, including:

- summary family and parameters;
- exact versus approximate implementation;
- grouping and subpopulation organization;
- nested summaries and post-processing;
- update-path transforms and readout-time operations;
- roll-ups and semantic rewrites;
- CSE sharing and number of effective consumers;
- shared node identity and whether state already exists.

The same logical query can therefore have different costs for KLL, DDSketch,
an exact accumulator, raw recomputation, or a nested composition.

### Summary physical properties

Summary physical properties affect numeric cost because they determine how
much state and work a legal candidate requires:

- parameter-dependent state size and retention footprint;
- update, merge, deletion, and readout complexity;
- input and output cardinality;
- number of physical instances created by grouping;
- bytes transferred or stored;
- rows processed by update-path transforms and readout post-processing.

These properties are converted into primitive build, update, read, retention,
retirement, and transfer estimates using runtime performance evidence.

### Summary and runtime capabilities

Capabilities determine legality, not numeric cost. They include:

- incremental-update, merge, subtract, and deletion support;
- supported update/readout execution phases;
- available ephemeral, prepared, shared, and continuous lifecycles;
- supported state placement, transfer, and storage operations.

An unsupported alternative is rejected before costing. The planner must not
represent an unsupported operation by assigning it an arbitrarily high cost:
that would incorrectly allow it to win if every other estimate were even
higher or unknown.

### Runtime performance evidence

Measured or modeled runtime performance may determine numeric cost, for
example:

- CPU time per summary update or readout;
- storage cost per byte-second;
- network cost per transferred byte;
- fixed deployment and retirement overhead;
- machine-, region-, or execution-stage-specific operator throughput.

This evidence is distinct from capability flags. “The runtime supports KLL
deletion” is a legality fact; “one KLL deletion costs X CPU units on this
runtime” is cost evidence.

### Accuracy requirements and data characteristics

Accuracy affects cost indirectly by changing legal summary families and their
parameters. A tighter error requirement may require a larger sketch, more
samples, or exact computation. Cardinality, distribution, skew, and other
fresh data evidence may affect both sizing and read/post-processing cost.

The accuracy model derives guarantees; the cost model prices candidates that
already carry valid guarantees.

### Existing materialized state

Existing state may avoid a new build cost only when a catalog establishes:

- semantic and parameter compatibility;
- ownership and shareability;
- freshness and coverage;
- accuracy guarantee;
- representation and execution phase;
- summary maintenance lifecycle guarantees.

Existing state is a distinct alternative, not a newly built summary with an
assumed zero build cost.

## Primitive cost inputs

For one concrete summary state, the model may provide:

```text
build_cost
maintenance_cost_per_update
summary_read_cost
retention_cost_rate
retirement_cost
```

For raw and stateless execution it may additionally provide:

```text
raw_recompute_cost_per_read
operator_cost_per_input_row
expected_input_rows
expected_output_rows
transfer_cost
```

Every estimate must name its model/version provenance. Deployment-specific
measurements may override documented heuristic defaults.

## Cost calculations

### Raw recomputation

For a query evaluated directly from raw or Pre-ASAP input:

```text
raw_total(H)
    = reads(H) × raw_recompute_cost_per_read
```

The result has no summary build, retention, maintenance, or retirement term.

### Ephemeral summary

Ephemeral state is rebuilt and retired for every invocation:

```text
ephemeral_total(H)
    = reads(H)
      × (build_cost + summary_read_cost + retirement_cost)
```

This is appropriate for one-time or unpredictable demand and does not imply
future reuse.

### Prepared summary

For a predictable activation window of `T` seconds:

```text
prepared_total(T)
    = build_cost
    + updates(T) × maintenance_cost_per_update
    + reads(T) × summary_read_cost
    + T × retention_cost_rate
    + retirement_cost
```

For data at rest, `updates(T) = 0`. Preparation is legal only when the declared
window covers every consumer that relies on the state.

### Bounded shared summary

For one state shared across multiple reads over horizon `H`:

```text
shared_total(H)
    = build_cost
    + updates(H) × maintenance_cost_per_update
    + reads(H) × summary_read_cost
    + H × retention_cost_rate
    + retirement_cost
```

The build, maintenance, retention, and retirement terms are charged once for
the shared state. Read cost is charged for every evaluation.

### Continuously maintained summary

For continuously ingesting or mixed data:

```text
continuous_total(H)
    = build_cost
    + H × update_rate × maintenance_cost_per_update
    + reads(H) × summary_read_cost
    + H × retention_cost_rate
    + retirement_cost
```

This alternative requires fresh update-rate evidence and incremental-update
support. Moving real-time windows additionally require deletion or equivalent
expiry support.

### Existing summary

For compatible existing state:

```text
existing_total(H)
    = remaining_update_cost(H)
    + reads(H) × summary_read_cost
    + remaining_retention_cost(H)
    + transition_or_retirement_cost
```

A new build term is omitted only when catalog provenance proves that the state
already exists and is reusable. Migration or cutover costs are included when
the selected plan changes representation or summary family.

### Update-path transform feeding a summary

For a value transform executed per update before summary maintenance:

```text
pretransform_cost_rate
    = update_rate
      × (transform_cost_per_input_row
         + summary_maintenance_cost_per_update)
    + evaluation_rate × summary_read_cost
```

The transform consumes update values; it cannot consume a query-time readout.

### Readout-time post-processing

For an operation applied after reading a summary:

```text
postprocess_cost_rate
    = update_rate × summary_maintenance_cost_per_update
    + evaluation_rate
      × (summary_read_cost
         + expected_output_rows × postprocess_cost_per_row)
```

These phase formulas apply to exact, summary-derived, or approximate value
operators. Accuracy semantics are carried separately by the candidate's
guarantee.

### CSE sharing versus independent recomputation

CSE contributes semantic alternatives; it is not a separate lifecycle.

```text
independent_total(H)
    = Σ_consumer raw_or_candidate_cost(consumer, H)

shared_total(H)
    = cost(one shared candidate and lifecycle, H)
      + Σ_consumer read_or_postprocess_cost(consumer, H)
```

The optimizer compares these whole-plan totals. A context-free fallback may
use:

```text
consumer_count × structural_recompute_weight
    versus
shared_family_maintenance_weight
```

but this is only a heuristic when recurrence, lifecycle, and horizon evidence
are unavailable. It must not be presented as full lifecycle-aware cost.

### Grouping and shared-subpopulation organization

Grouping changes the number and size of physical summary instances. A simple
state-size estimate is:

```text
per-subpopulation_state
    = subpopulation_count × inner_summary_state_size

shared_grid_state
    = shared_grid_cells × inner_summary_state_size
```

For CMS-like structures, inner state size may be proportional to
`width × depth`; other families use their own parameter-dependent sizing
formula. State size then affects build, update, retention, transfer, and read
cost rather than acting as a disconnected preference score.

### Whole-plan aggregation

The total cost of a candidate plan is the sum of its unique physical actions:

```text
plan_total(H)
    = Σ unique summary deployments
        lifecycle_total(summary, H)
    + Σ stateless update/readout operations
        operator_total(operation, H)
    + transfer_and_transition_costs
```

Shared DAG nodes are counted once by physical identity. References to the same
state contribute their read or post-processing work but do not duplicate
build or maintenance cost. Nested plans must preserve phase compatibility and
must not double-count an internally shared descendant.

## Lifecycle expansion and global selection

For every legal semantic candidate, the optimizer enumerates legal summary-
maintenance lifecycles before ranking:

```text
semantic candidate
    × ephemeral
    × prepared
    × bounded shared
    × continuously maintained
    × compatible existing state
```

This is conceptual multiplication: incompatible combinations are removed by
capability, workload, phase, and schedule checks. The cost model estimates each
remaining combination. The global optimizer, not the cost model, selects the
lowest-cost compatible whole plan and retains raw recomputation as an explicit
fallback.

Selecting a semantic summary first and attaching a lifecycle afterward is
insufficient because lifecycle cost can reverse the summary-family or CSE
ranking.

## Unknown evidence and fail-closed behavior

Unknown is not zero. A total remains unknown when a required term is missing,
stale, non-finite, or has incompatible units.

The following cannot make a candidate win by assumption:

- missing horizon when one-time and rate costs must be combined;
- missing or stale evaluation or update rate;
- missing build, update, read, retention, retirement, or raw cost;
- unknown runtime or summary capability;
- unsupported accuracy propagation;
- a `NaN`, infinite, negative rate, or non-positive horizon.

An unknown-cost alternative remains visible for explanation but is not ranked
as cheaper than a fully costed legal alternative. If no summary alternative is
legally and completely costed, the planner preserves a conservative fallback
or reports that selection requires more evidence.

## Cost provenance and explanation

Every selected estimate should expose:

- cost-model name and version;
- primitive input values and their units;
- evidence source, observation window, and freshness;
- horizon and derived reads/updates;
- lifecycle and capability assumptions;
- one-time and recurring terms before normalization;
- total cost and alternatives compared;
- missing inputs and typed rejection reasons.

This allows users to distinguish measured deployment costs from heuristic
defaults and to understand why the same query receives a different plan under
a different workload or data distribution.

## Summary-maintenance commitment

The selected lifecycle becomes part of the emitted plan's summary maintenance
lifecycle guarantees. Once a KLL summary is materialized for incremental
maintenance, the runtime cannot silently maintain DDSketch instead. A replan
may choose a different family, but its cost must include explicit build or
migration, reader cutover, and retirement actions.
