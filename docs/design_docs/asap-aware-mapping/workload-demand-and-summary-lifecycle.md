# Design: Query Workloads, Data Workloads, and Summary Lifecycle Maintenance

## Audience and context

This document is for ASAPPlanner designers, architects, researchers, and
developers working on workload-aware plan selection. It defines how the
planner should describe query workload, data workload, and the lifecycle of
summary state. It is the design contract for the public Rust model and the
workload-to-lifecycle planning API; deployments still supply their own cost
statistics and runtime capabilities.

The terminology follows the ProjectASAP
[glossary](https://github.com/ProjectASAP/internal-docs/blob/03e1c70f5af3ae9221471898541067eee7f86338/glossary.md).
That glossary is authoritative for the meanings of data workload, query
workload, ad-hoc and predictable queries, one-time and repeated queries,
real-time and longitudinal queries, output cardinality, and lookback window.
This document maps those concepts into planner responsibilities and records
where the current model is incomplete.

This design is orthogonal to
[end-to-end accuracy guarantees](end-to-end-accuracy-guarantees.md). Accuracy
decides whether a candidate is correct enough. Workload demand and state
lifecycle decide whether building, maintaining, sharing, or recomputing that
candidate is worthwhile. Neither decision may override the other.

### Lifecycle terminology

This document uses **summary maintenance lifecycle** for the lifetime of
planner-selected summary state: build, prepare, share, incrementally maintain,
read, and retire. The final plan's promises about those actions are its
**summary maintenance lifecycle guarantees**. This term is intentionally
distinct from the broader **data lifecycle**, which covers data collection,
transmission, storage, and analytics. Unqualified "lifecycle guarantees" are
avoided.

## Problem and why now

A summary operator does not imply one execution lifecycle. The same exact or
approximate summary can be:

- built once from data at rest and discarded after one query;
- prepared before a known future query and retired afterward;
- shared across a bounded set of requests; or
- maintained incrementally as data continues to arrive.

Likewise, an exact stateless operator may run once over a batch, once per
update in an incremental pipeline, or once per readout. Operator statefulness,
execution schedule, and output representation are separate properties.

The phase contract is also independent of accuracy semantics. A value
operation may be exact, summary-derived, or approximate. The post-ASAP IR
therefore uses the generic phase nodes `UpdateTransform` (`UpdateValue ->
UpdateValue`) and `ReadoutPostProcess` (`ReadoutValue -> ReadoutValue`). Their
`ValueOperator` payload identifies the computation; the enclosing node carries
its output schema and accuracy guarantee. The exact-composition strategy emits
`ValueOperator::Exact` today, but it is only the first producer of these phase
nodes, not their definition.

The query expression alone cannot determine those properties. The same query
may arrive unexpectedly during exploration, run once at a scheduled time, or
repeat every ten seconds on a dashboard. Planning summary state from syntax
alone either misses reuse or invents reuse that the workload does not justify.

The normalized workload preserves `query_batch` and `repeating_queries` as
compatibility-shaped inputs, then exposes both through `QueryWorkload::entries`
as recurrence, predictability, requirements, and time-selection axes. Data
arrival and fresh ingestion evidence remain a separate `DataWorkload`; a
repeating query therefore never implies streaming data.

### Implementation map

- `asap_types::workload` defines the normalized query/data workload and
  evidence freshness contract.
- `PlanSpace::recurrence_profiles_from_workload` derives per-target read and
  update recurrence from an explicit root-to-workload-entry binding, without
  treating missing evidence as zero or relying on container order.
- `WorkloadAccuracyEvidence` supplies fresh cardinality and distribution to
  accuracy models.
- `plan_summary_maintenance_lifecycles` enumerates legal ephemeral, prepared, shared, and
  continuously maintained alternatives for the entries explicitly associated
  with the target, and compares their costs over the caller's explicit horizon.
- `global_selection_with_summary_maintenance_lifecycles` prices each semantic
  summary candidate using its cheapest legal summary maintenance lifecycle
  before global selection. Its recurrence profile includes repeated DAG paths,
  while the workload binding separately preserves time-selection and
  predictability facts.
- `materialize_with_summary_maintenance_lifecycles` materializes that phase-valid selection and
  attaches the selected state deployments. Each deployment retains assumptions
  and rejected alternatives for explanation.
- `UpdateTransform` and `ReadoutPostProcess` express availability boundaries
  for any value operator. Exact, summary-derived, and approximate producers use
  the same phase validation rather than defining accuracy-specific phase nodes.

## Inputs, outputs, and end-to-end behavior

The planner receives four logically distinct inputs:

1. logical queries, which define query semantics;
2. query workload, including per-query accuracy and latency requirements,
   predictability, recurrence, and queried time scope;
3. data-workload characteristics, including arrival, volume, cardinality, and
   distribution;
4. existing summaries and the lifecycle actions available to the deployment.

The implemented output is a phase-valid selected summary plan (or a
cost-preferred raw-recomputation fallback) plus explicit state deployments. A
state deployment states whether a summary is ephemeral, prepared, shared for a
bounded period, or continuously maintained. It retains costs, assumptions, and
structured rejection reasons. Exporting full input provenance remains a later
integration.

```text
            logical queries ---+
           query workload -----+
              data workload ---+--> candidate plans
        available summaries ---+       -> semantic and accuracy legality
                                         -> lifecycle alternatives
                                         -> horizon-normalized cost
                                         -> selected plan + deployments
```

For an unpredictable one-time query, the planner may read an existing summary,
build an ephemeral summary, or recompute from raw data. It must not assume
future reuse. For a predictable one-time query, it may additionally compare
preparing state in advance with building or recomputing at execution time. For
repeated queries, it may amortize build and maintenance cost across reads over
an explicit horizon.

### Target end-to-end decision order

```text
normalize query and data workloads
    -> derive recurrence, time-scope, and data evidence
    -> build a compact space of semantic plan alternatives
    -> validate semantic, schema, summary-capability, and phase constraints
    -> derive and check accuracy guarantees
    -> expand every legal candidate with summary-maintenance lifecycles
    -> normalize one-time and rate costs over an explicit horizon
    -> globally rank compatible plan-and-lifecycle combinations
    -> emit plan, deployments, accuracy guarantees,
       summary maintenance lifecycle guarantees, assumptions,
       and rejected alternatives
```

## Goals and non-goals

### Goals

- Represent glossary-defined query-workload and data-workload concepts without
  collapsing independent axes into one enum.
- Separate an operator's statefulness from its execution schedule and the
  lifecycle of the state it produces.
- Make unknown demand explicit and fail closed rather than treating it as zero
  or infinite reuse.
- Compare one-time and rate-valued costs only through an explicit horizon.
- Explain why a selected plan builds, reuses, maintains, or avoids summary
  state.
- Preserve a minimal path from the current batch/repeating workload and
  recurrence profile to the proposed model.

### Non-goals

- Scheduling jobs, assigning machines, admission control, or executing queries.
- Predicting future query text inside ASAPPlanner.
- Defining a sketch runtime or state-storage protocol.
- Choosing a concrete forecasting algorithm for uncertain demand.
- Changing accuracy targets or guarantee algebra.

## Heilmeier questions

- **What are we trying to do?** Choose whether summary state should be built,
  maintained, shared, reused, or avoided for different query workloads
  and data workload.
- **How is it done today, and what are the limits?** The planner distinguishes
  one-shot counts, fixed repeating intervals, and an ingest-rate proxy. It
  cannot distinguish an unexpected exploratory query from a scheduled one-time
  report, or data at rest from continuous ingestion as an explicit mode.
- **What is new, and why will it succeed?** Orthogonal workload axes and an
  explicit state lifecycle let the existing recurrence formulas compare the
  same summary under different deployment choices without changing query
  semantics.
- **Who cares?** Users need predictable latency and cost; operators need to
  know what state will exist and for how long; planner developers need demand
  assumptions to be auditable.
- **What are the risks and costs?** More inputs can make planning harder to
  configure, forecasts may be stale, and a large lifecycle search space can
  increase planning cost.
- **What are the checks for success?** The acceptance cases below must produce
  different lifecycle alternatives and cost terms for identical query syntax
  under different workload contracts.

## Proposed design

### Authoritative concepts and ownership

| Concept | Authoritative layer | Reason |
| --- | --- | --- |
| Query meaning | Pre-ASAP query IR | Workload metadata must not change semantics |
| Accuracy requirement | Query workload (per-query) | The required result fidelity may be explicit or supplied by the normalization default |
| Response-latency requirement | Query workload (per-query) | The optional end-to-end response-time bound belongs to one query execution |
| Query workload | Workload input | Arrival and recurrence are not inferable from syntax |
| Data workload | Workload input | Ingestion and distribution describe the data, not query workload |
| Summary capability | Summary properties | Merge, delete, and update support constrain legal lifecycles |
| State lifecycle | Physical planning decision | Lifecycle is selected, not declared by `SummaryAgg` |
| Cost | Cost model and explanation | Cost consumes all inputs but does not define their meaning |

### Query workload

Accuracy and latency are separate per-query requirements within the query
workload. They constrain different planner decisions and must not be collapsed
into one SLA value:

```rust
enum AccuracyRequirement {
    /// The caller supplied the required result fidelity.
    Explicit(AccuracyTarget),
    /// The source omitted accuracy; normalization applies the exact default.
    ImplicitExact,
}

enum LatencyRequirement {
    /// Maximum permitted end-to-end latency for one query execution.
    ExplicitMax(Duration),
    /// The caller supplied no latency bound.
    Unspecified,
}

struct QueryRequirements {
    accuracy: AccuracyRequirement,
    response_latency: LatencyRequirement,
}
```

An omitted accuracy field is not an unknown accuracy target and does not permit
arbitrary approximation: the current normalization policy makes it
`ImplicitExact`. Keeping that variant distinct from `Explicit(Exact)` preserves
whether the caller chose exactness or inherited the default. An unspecified
response-latency requirement imposes no response-time constraint; it is not a
zero-duration bound or evidence that every latency is acceptable. Accuracy is
checked as a legality constraint. The normalized model preserves response
latency, but the current planner does not yet reject plans against that bound.

#### Classification axes

The glossary classifications must be modeled independently.

##### Predictability

```rust
enum Predictability {
    /// The query shape is not known before arrival.
    AdHoc,
    /// The query or parameterized template is known before execution.
    Predictable {
        known_at: Option<Timestamp>,
    },
    /// The caller supplied no reliable classification.
    Unknown,
}
```

`AdHoc` does not mean repeated or one-time. It means the query shape was not
known in advance. The glossary currently places exploratory/ad-hoc queries in
the one-time category, so the MVP should accept `AdHoc + OneTime` and reserve
other combinations until a concrete use case establishes their semantics.

##### Recurrence

```rust
enum QueryRecurrence {
    OneTime {
        invocations: u64,
        execute_at: Option<Timestamp>,
    },
    Repeated {
        demand: RepeatedDemand,
    },
    Unknown,
}

enum RepeatedDemand {
    FixedInterval(Duration),
    Scheduled(Vec<Timestamp>),
    EstimatedRate(DemandEstimate),
}

struct DemandEstimate {
    /// Time range over which the demand was measured or forecast.
    observation_window: ObservationWindow,
    /// Expected demand, expressed in exactly one form.
    expected: ExpectedDemand,
    /// Highest expected invocation rate within the observation window.
    peak_rate: Option<Rate>,
    /// Highest expected number of simultaneously executing invocations.
    max_concurrency: Option<u64>,
    /// Confidence in this estimate, in the inclusive range [0.0, 1.0].
    confidence: Confidence,
    source: EvidenceSource,
    observed_at: Option<Timestamp>,
    valid_for: Option<Duration>,
}

enum ExpectedDemand {
    /// Expected total invocations over `observation_window`.
    InvocationCount(u64),
    /// Expected average invocations per second over `observation_window`.
    AverageRate(Rate),
}

struct ObservationWindow {
    start: Timestamp,
    end: Timestamp,
}

struct Confidence(f64);
```

One-time means no recurrence is expected for that workload entry. Several
one-time consumers may still share a subplan within a submitted workload.
Repeated means the same query expression over its selected data is evaluated
over time, matching the glossary. Parameterized templates require an explicit
equivalence policy before their executions count as the same query.

Query-workload volume is more than an average rate. Cost and latency can differ
for the same total request count when requests arrive in bursts or concurrently.
`ExpectedDemand` makes invocation count and average rate alternative
representations, preventing conflicting values in one estimate. The observation
window must be non-empty, rates must be finite and non-negative, and
`Confidence` must be between zero and one. Fixed intervals and explicit
schedules are declarations rather than estimates and do not need fabricated
confidence. The MVP may cost only invocation count and evaluation rate, but it
must preserve unsupported volume characteristics for explanation rather than
silently discarding them.

##### Queried time scope

```rust
enum QueryTimeScope {
    RealTime,
    Longitudinal,
    Mixed,
    Unknown,
}
```

`QueryTimeScope` is not a response-latency requirement. It classifies the event
time of the data selected by the query; `LatencyRequirement` constrains the
wall-clock time allowed to produce the result. They are independent: a
longitudinal query over archived data may require a 100 ms response, while a
real-time query over the latest data may permit a 30 second response.

This classification is not derived only from a numeric lookback. A five-minute
lookback over recent data is real-time; the same duration over archived data is
not. Planning input should therefore carry the classification and the concrete
time selection separately:

```rust
struct TimeSelection {
    scope: QueryTimeScope,
    lookback: Option<Duration>,
    as_of: Option<Timestamp>,
}
```

For example, the same five-minute lookback has a different scope depending on
whether it is anchored at the current planning time or at a historical time:

```rust
// The last five minutes: real-time.
TimeSelection {
    scope: QueryTimeScope::RealTime,
    lookback: Some(Duration::minutes(5)),
    as_of: None,
}

// A five-minute interval from archived data: longitudinal.
TimeSelection {
    scope: QueryTimeScope::Longitudinal,
    lookback: Some(Duration::minutes(5)),
    as_of: Some(timestamp!("2024-01-01T12:05:00Z")),
}
```

`lookback` is a query property already represented by temporal query nodes in
some frontends. The normalized workload should reference or derive it rather
than introduce a second conflicting value.

### Data workload is separate from query workload

```rust
enum DataArrival {
    AtRest,
    ContinuouslyIngesting,
    Mixed,
    Unknown,
}

/// Statistical distribution of keys in the input data.
enum DataDistribution {
    /// A small number of keys account for most observations.
    Zipf,
    /// Keys are approximately equally likely.
    Uniform,
    /// Observations arrive in bursts with a temporarily concentrated key set.
    Bursty,
}

struct DataWorkload {
    arrival: DataArrival,
    ingestion_volume: Evidence<u64>,
    ingestion_rate: Evidence<Rate>,
    input_cardinality: Evidence<u64>,
    distribution: Evidence<DataDistribution>,
}
```

`DataDistribution` reuses the existing ASAPPlanner classification. It describes
the key-frequency shape used by summary accuracy and cost models, not whether
data arrives continuously. An unavailable or unsupported distribution is
represented by `Evidence.value = None` rather than by assuming the default
distribution.

The former `DataCharacteristics` was a stale, continuous-ingestion-specific
case built around series count and samples per second. `DataWorkload` replaces
it as the normalized input rather than embedding that special case in the
general model. Data at rest may have row count and scan statistics without a
nonzero ingestion rate. Unknown arrival must not be interpreted as continuously
ingesting or at rest.

Every empirical value uses an evidence wrapper conceptually containing:

```rust
struct Evidence<T> {
    value: Option<T>,
    source: EvidenceSource,
    observed_at: Option<Timestamp>,
    valid_for: Option<Duration>,
}
```

This reuses the provenance and freshness principles from empirical summary
parameter configuration. Missing, stale, or future-dated evidence remains
unknown.

### Output cardinality is a derived or evidenced cost input

Output cardinality depends on input cardinality and grouping columns. The
planner may derive it analytically, accept a catalog estimate, or leave it
unknown. The source and freshness metadata must be preserved because output
cardinality affects summary size, read cost, post-processing cost, and network
cost. It is not a query correctness requirement.

### Separate operator state, schedule, and output

The physical design must not use `SummaryAgg` as shorthand for incremental
maintenance.

```rust
enum OperatorState {
    Stateless,
    Stateful {
        mergeable: bool,
        deletable: bool,
    },
}

enum EvaluationSchedule {
    OneShot,
    PerUpdate,
    OnRead,
}

enum OutputRepresentation {
    PlainRows,
    SummaryState,
    FinalizedValue,
}
```

A one-shot sketch builder is stateful while it consumes its input, but it does
not imply long-lived incremental maintenance. A stateless transform can run
`PerUpdate` before a downstream maintained summary. These types describe an
execution contract; they do not replace semantic operators in the post-ASAP IR.

### Summary maintenance lifecycle is a plan alternative

```rust
enum SummaryMaintenanceLifecycle {
    Ephemeral,
    Prepared {
        activate_at: Timestamp,
        retire_at: Timestamp,
    },
    Shared {
        retention: Duration,
    },
    ContinuouslyMaintained,
}
```

- `Ephemeral` builds state for one submitted workload and discards it afterward.
- `Prepared` builds or begins maintaining state before a predictable query and
  retires it after the known need ends.
- `Shared` retains state for multiple consumers over a bounded lifetime.
- `ContinuouslyMaintained` applies data updates until an explicit later
  deployment decision retires the state.

The summary family and its properties constrain which lifecycles are legal.
For example, an append-only sketch may support continuous inserts but not a
sliding-window lifecycle requiring deletion. Lifecycle legality is checked
before cost ranking, like accuracy legality. Deployments provide these
per-summary properties through `summary_maintenance_capabilities`; moving
real-time windows require deletion support as well as incremental updates.

### Existing summaries are planning input

An ad-hoc query cannot justify creating permanent state from unknown future
demand, but it may use compatible state that already exists. The planning
problem therefore needs a state catalog describing identity, parameters,
coverage, freshness, accuracy guarantee, lifecycle, and ownership. Catalog
integration is a separate implementation increment; this design only requires
that "reuse existing" and "create new" remain distinguishable alternatives.

### Cost over a horizon

`H` is the optimization horizon: the future wall-clock duration over which the
planner compares one-time and recurring costs. The existing cost model
represents it in seconds:

```rust
/// A finite, strictly positive optimization duration, in seconds.
struct Horizon(f64);
```

The horizon is not the query lookback, the queried time scope, or the response
latency bound. It answers only "over how much future execution time should
these alternatives be costed?" All alternatives in one decision must use the
same `H`. `reads(H)` is the number of query evaluations expected or scheduled
within that horizon; for a fixed evaluation rate it is
`H * evaluation_rate`, plus any separately modeled one-time invocations.
Who supplies `H`, and whether a deployment may default it, remains an explicit
architecture decision below. If no horizon is available, the planner must not
compare a one-time cost with a rate-valued cost.

For a stateful incremental alternative over horizon `H`:

```text
total(H) = build_cost
         + H * update_rate * maintenance_cost_per_update
         + reads(H) * summary_read_cost
         + H * retention_cost_rate
         + retirement_cost
```

For repeated raw recomputation:

```text
total(H) = reads(H) * raw_recompute_cost
```

Before materialization, lifecycle-aware global selection computes the cheapest
legal summary maintenance lifecycle total for every semantic summary sibling
whose cost evidence is complete. Those totals can reorder summary families;
unknown totals remain conservative and cannot win as invented zeroes. After
selection, materialization sums each unique selected summary deployment once
and can replace the selected summary plan with raw recomputation when the raw
cost is lower or the summary maintenance lifecycle is uncostable.

For an ephemeral summary:

```text
total = invocations * (build_cost + summary_read_cost + retirement_cost)
```

`retirement_cost` consistently means the one-time cost of ending a summary
state lifecycle, including deallocation or other cleanup. For ephemeral state,
retirement happens immediately after each invocation; for prepared, shared, or
continuously maintained state, it happens when that deployment is retired.

For prepared state, update and retention terms apply only between activation
and retirement. Existing state does not pay a new build cost, but its catalog
provenance must establish that assumption.

The existing `Cost`, `CostRate`, `EvaluationRate`, `UpdateRate`, `Horizon`, and
`total_cost` types are the minimum viable foundation. The implementation should
extend their explanations and lifecycle coverage instead of creating a second
recurrence cost system.

### Unknown and uncertain demand

Unknown demand is not zero demand and is not evidence of future reuse. The MVP
policy is:

- do not select newly created long-lived state solely on unknown future reuse;
- allow raw recomputation, ephemeral build, and reuse of already available
  compatible state;
- retain an explicit explanation of the missing demand evidence; and
- require an explicit planning objective before using an estimated demand
  distribution.

Future uncertain-demand support may add expected-cost, percentile-cost,
worst-case, or regret objectives. Those policies must consume a typed estimate
with confidence and provenance; they are not implicit behavior of
`Predictability::Unknown`.

## Review against the ProjectASAP glossary

The glossary review found the following required coverage and current gaps.

| Glossary concept | Current ASAPPlanner representation | Missing design support |
| --- | --- | --- |
| Data at rest vs continuously ingesting | `DataArrival` is explicit | Runtime/catalog-specific arrival discovery remains external |
| Ingestion volume | `DataWorkload::ingestion_volume` carries evidence | A concrete time basis for volume remains deployment-specific |
| Ingestion rate | Evidenced independently from query evaluation rate | Preserve richer unit/provenance metadata when integrations require it |
| Input cardinality | Evidenced workload-level cardinality feeds accuracy | Per-dataset/metric/column scoping remains future work |
| Data distribution | Evidenced built-in enum | Permit deployment-specific distributions later |
| Ad-hoc vs predictable | `Predictability` is independent from recurrence | Parameterized-template equivalence remains open |
| One-time vs repeated | One-time, fixed, scheduled, estimated, and unknown recurrence | Forecast-policy integration remains future work |
| Query volume and characteristics | Estimates preserve average/count, peak, concurrency, confidence, and freshness | Peak and concurrency are not yet consumed by cost or latency models |
| Real-time vs longitudinal | `TimeSelection` carries scope, lookback, and `as_of` | Conflict policy with temporal IR remains open |
| Output cardinality | May be inferred locally; no common evidenced input | Add derived/evidenced value and provenance for costing |
| Lookback window | Represented in temporal query shapes/frontends | Establish query IR as authority and expose it to workload costing |
| CTSA pipeline | Not explicitly modeled | Keep as architectural context; planner consumes collect/store/analyze facts but does not model transmission topology in the MVP |
| CSP(F) | Cost and fidelity partly modeled | Treat scale/performance/fidelity as objectives and constraints; do not collapse fidelity into cost |

Two terminology constraints apply:

1. A repeated query is not inherently a streaming-data workload. It may
   repeatedly query data at rest.
2. A one-time query is not inherently stateless. A predictable one-time query
   may justify prepared state, while an ephemeral summary is stateful during
   its one execution.

## Minimal complexity

The minimum input model is determined by the downstream applications selected
for integration, not by a context-free notion of the fewest possible fields.
Each supported use case must contribute the workload facts that can change
plan legality, accuracy, lifecycle, or cost:

- Time-series metric queries require queried time scope and lookback.
- Repeated dashboard queries, including an ASAPQuery integration, require
  recurrence and evaluation frequency so the planner can cost reuse and
  maintenance across executions.
- Batch queries over data at rest require an explicit at-rest arrival mode and
  must not be assigned a fabricated ingestion rate.
- Summary techniques whose accuracy depends on the input distribution require
  evidenced distribution characteristics; omitting them must produce unknown
  accuracy or a conservative fallback rather than a favorable assumption.

The initial implementation should include the union of fields required by its
committed integrations. Additional workload dimensions should be added when a
new downstream use case demonstrates that they affect a planning decision.

The simplest alternative is to extend `BatchEntry` with optional schedule and
classification fields and extend `RepeatingEntry` with time scope. That is a
reasonable serialization migration, but it is not a sufficient conceptual
model: it continues to make predictability and recurrence mutually exclusive
container choices, and it has no place for data arrival or state lifecycle.

The minimum new conceptual layers are therefore:

1. orthogonal query-demand metadata, required because glossary categories are
   not one taxonomy;
2. data-workload metadata, required because ingestion does not describe query
   recurrence;
3. state lifecycle as a physical alternative, required because one summary
   operator can be deployed ephemerally or incrementally.

No separate scheduler, forecasting framework, or replacement cost model is
introduced. Existing query IR, summary properties, accuracy model, and
recurrence cost types remain authoritative in their domains.

## Alternatives and decisions

### Encode workload class as one enum

Rejected. Variants such as `AdHoc`, `OneShot`, and `Repeated` overlap:
predictability and recurrence are different facts, and time scope is a third.

### Infer demand from query syntax or submitted root count

Rejected. Syntax contains no evidence of future arrival, and several roots in
one request establish only current structural sharing.

### Treat every summary as continuously maintained

Rejected. It excludes ephemeral construction over data at rest and overcharges
one-time plans. It also hides deployment lifetime from explanations.

### Treat every one-time query as raw recomputation

Rejected. An ephemeral summary may reduce memory or network cost during one
execution, an existing summary may already answer the query, and a predictable
future query may justify preparation.

### Fold fidelity into a scalar cost

Rejected. Accuracy and semantic correctness are constraints checked before
ranking. A cheaper plan cannot purchase permission to violate fidelity.

### Extend the existing recurrence profile only

Partially accepted for implementation reuse, rejected as the whole model.
`RecurrenceProfile` is an aggregated cost context for a target. It should remain
the derived input to cost decisions, while normalized workload metadata retains
predictability, time scope, provenance, and lifecycle information needed before
and after aggregation.

## Quality attributes and evidence

- **Understandability:** explanations use glossary terms and show each axis
  separately. Proxy: reviewers can distinguish repeated queries from continuous
  ingestion in exported plan evidence.
- **Debuggability:** selected and rejected lifecycle alternatives record costs,
  horizon-derived decisions, assumptions, and typed rejection reasons. Full
  demand/data provenance in exported explanations remains future work.
- **Maintainability:** current recurrence types remain the cost authority;
  normalized workload types remain the source authority. No duplicate formula
  system is introduced.
- **Extensibility:** scheduled and estimated recurrence fit without changing
  query semantics. Forecasting policies remain pluggable planning objectives.
- **Performance:** lifecycle enumeration expands the candidate space. The MVP
  should generate only capability-compatible alternatives and deduplicate
  equivalent deployments before ranking.
- **Operability:** every long-lived state has activation, retention or retirement
  semantics and ownership in output. Concrete runtime APIs are future work.
- **Security and privacy:** query logs and empirical distributions may be
  sensitive. Provenance must identify a source without requiring raw query-log
  contents to be embedded in exported plans.

## Acceptance and test design

Implementation acceptance is defined by identical logical queries producing
different legal lifecycle choices under different workload contracts:

1. **Unpredictable one-time query:** offers raw recomputation, compatible
   existing state, and ephemeral build; does not justify new continuous state.
2. **Predictable scheduled one-time query:** may offer prepared state with a
   bounded activation and retirement period.
3. **Repeated query over continuously ingesting data:** compares incremental
   maintenance and repeated recomputation using distinct update and evaluation
   rates over an explicit horizon.
4. **Repeated query over data at rest:** uses evaluation rate without inventing
   maintenance updates.
5. **Real-time and longitudinal queries with the same expression:** preserve
   different time selections and may receive different scan, retention, and
   summary alternatives.
6. **Unknown demand:** remains unknown in explanation and cannot make a newly
   created long-lived state win through assumed reuse.
7. **Mixed one-time and repeated consumers:** requires an explicit horizon and
   accounts for shared build cost once.
8. **Accuracy failure:** rejects a lifecycle regardless of favorable workload
   cost.

Focused unit tests should cover normalization, invalid combinations, evidence
freshness, lifecycle capability checks, and dimensional cost arithmetic.
End-to-end tests should cover cases 1–8 through candidate selection and exported
explanations. A reviewer who did not implement the workload types should design
or review at least the unknown-demand and mixed-consumer cases; that independent
review has not occurred for this design document.

## Risks, rollout, and exit criteria

The implementation should roll out additively:

1. add normalized metadata and explanations while preserving current
   batch/repeating behavior;
2. derive the existing `RecurrenceProfile` from the richer model;
3. add ephemeral and existing-state alternatives;
4. add prepared and continuously maintained lifecycle selection;
5. integrate empirical demand and state catalogs only when provenance and
   freshness contracts are available.

Compatibility requires old workloads to normalize without changing their
current decisions when no new metadata is supplied. Unknown new fields must
take the documented conservative path rather than acquire optimistic defaults.

Open decisions requiring architecture or product input:

- whether predictable parameterized query templates count as the same repeated
  query and under which equivalence relation;
- who supplies the optimization horizon and whether a deployment may define a
  default for purely repeated workloads;
- which planning objective governs uncertain demand;
- how state ownership, quota, and retirement requests cross the planner/runtime
  boundary;
- whether real-time versus longitudinal is supplied by the caller, derived by a
  policy using `as_of` and lookback, or both with conflict diagnostics; and
- the minimum evidence freshness required before empirical workload data may
  affect selection.

The design exits draft status when these decisions have owners, the normalized
input has a compatibility plan, and acceptance cases 1–8 can be expressed in
fixtures without runtime-specific assumptions.
