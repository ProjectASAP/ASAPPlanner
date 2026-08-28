# Design: Workload Demand and Summary Lifecycle

## Audience and context

This document is for ASAPPlanner designers, architects, researchers, and
developers working on workload-aware plan selection. It defines how the planner
should describe query demand, data workload, and the lifecycle of summary state.
It is a design contract, not a description of the current public Rust API.

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

The query expression alone cannot determine those properties. The same query
may arrive unexpectedly during exploration, run once at a scheduled time, or
repeat every ten seconds on a dashboard. Planning summary state from syntax
alone either misses reuse or invents reuse that the workload does not justify.

The current normalized workload distinguishes a one-shot `query_batch` from
fixed-interval `repeating_queries`, and the recurrence cost model distinguishes
one-shot consumers from evaluation and update rates. This is a useful base, but
it does not represent predictability, uncertain demand, real-time versus
longitudinal scope, at-rest versus continuously ingesting data, or summary-state
lifecycle. It also risks treating "repeating query" and "streaming data" as the
same fact even though the glossary defines them on different axes.

## Inputs, outputs, and end-to-end behavior

The planner receives four logically distinct inputs:

1. logical queries and their correctness and latency requirements;
2. query-workload demand, including predictability and recurrence;
3. data-workload characteristics, including ingestion and queried time scope;
4. existing summaries and the lifecycle actions available to the deployment.

The output is a legal physical-plan choice plus explicit state deployments. A
state deployment states whether a summary is ephemeral, prepared, shared for a
bounded period, or continuously maintained. Its cost explanation identifies
the demand and data evidence used in the decision.

```text
logical queries + requirements
             query demand -----+
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
- Implementing the proposed public types in this documentation-only PR.

## Heilmeier questions

- **What are we trying to do?** Choose whether summary state should be built,
  maintained, shared, reused, or avoided for different kinds of query demand
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
- **How long will it take?** The minimum implementation can extend normalized
  workload input, lifecycle candidates, and explanations incrementally. Demand
  forecasting and runtime state catalogs are later integrations.
- **What are the checks for success?** The acceptance cases below must produce
  different lifecycle alternatives and cost terms for identical query syntax
  under different workload contracts.

## Proposed design

### Authoritative concepts and ownership

| Concept | Authoritative layer | Reason |
| --- | --- | --- |
| Query meaning | Pre-ASAP query IR | Workload metadata must not change semantics |
| Accuracy and latency requirement | Per-query requirements | Requirements belong to the requested result |
| Query demand | Workload input | Arrival and recurrence are not inferable from syntax |
| Data workload | Workload input | Ingestion and distribution describe the data, not query demand |
| Summary capability | Summary properties | Merge, delete, and update support constrain legal lifecycles |
| State lifecycle | Physical planning decision | Lifecycle is selected, not declared by `SummaryAgg` |
| Cost | Cost model and explanation | Cost consumes all inputs but does not define their meaning |

### Query workload: three independent axes

The glossary classifications must be modeled independently.

#### Predictability

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

#### Recurrence

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
```

One-time means no recurrence is expected for that workload entry. Several
one-time consumers may still share a subplan within a submitted workload.
Repeated means the same query expression over its selected data is evaluated
over time, matching the glossary. Parameterized templates require an explicit
equivalence policy before their executions count as the same query.

Query-workload volume is more than an average rate. Cost and latency can differ
for the same total request count when requests arrive in bursts or concurrently.
An empirical `DemandEstimate` should therefore be able to carry an observation
window, expected invocation count or rate, peak rate, concurrency, confidence,
and provenance. Fixed intervals and explicit schedules are declarations rather
than estimates and do not need fabricated confidence. The MVP may cost only
invocation count and evaluation rate, but it must preserve unsupported volume
characteristics for explanation rather than silently discarding them.

#### Queried time scope

```rust
enum QueryTimeScope {
    RealTime,
    Longitudinal,
    Mixed,
    Unknown,
}
```

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

struct DataWorkload {
    arrival: DataArrival,
    ingestion_volume: Evidence<u64>,
    ingestion_rate: Evidence<Rate>,
    input_cardinality: Evidence<u64>,
    distribution: Evidence<DataDistribution>,
}
```

The current `DataCharacteristics` supplies continuous-ingestion fields such as
series count and samples per second. It should become one source for this model,
not the authoritative definition of all data workloads. Data at rest may have
row count and scan statistics without a nonzero ingestion rate. Unknown arrival
must not be interpreted as continuously ingesting or at rest.

Every empirical value uses an evidence wrapper conceptually containing:

```rust
struct Evidence<T> {
    value: Option<T>,
    source: EvidenceSource,
    observed_at: Option<Timestamp>,
    valid_for: Option<Duration>,
    applicability: Applicability,
}
```

This reuses the provenance and freshness principles from empirical summary
parameter configuration. A missing or stale value remains unknown.

### Output cardinality is a derived or evidenced cost input

Output cardinality depends on input cardinality and grouping columns. The
planner may derive it analytically, accept a catalog estimate, or leave it
unknown. The source and applicability must be preserved because output
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

### State lifecycle is a plan alternative

```rust
enum StateLifecycle {
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
before cost ranking, like accuracy legality.

### Existing summaries are planning input

An ad-hoc query cannot justify creating permanent state from unknown future
demand, but it may use compatible state that already exists. The planning
problem therefore needs a state catalog describing identity, parameters,
coverage, freshness, accuracy guarantee, lifecycle, and ownership. Catalog
integration is a separate implementation increment; this design only requires
that "reuse existing" and "create new" remain distinguishable alternatives.

### Cost over a horizon

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

For an ephemeral summary:

```text
total = invocations * (build_cost + summary_read_cost + disposal_cost)
```

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

### End-to-end decision order

```text
normalize query and data workload
    -> derive demand, time-scope, and data evidence
    -> enumerate semantic plan alternatives
    -> enumerate legal execution contracts and state lifecycles
    -> validate summary capabilities and phase constraints
    -> derive and check accuracy guarantees
    -> normalize one-time and rate costs over an explicit horizon
    -> rank legal alternatives
    -> emit plan, deployments, assumptions, and rejected alternatives
```

## Review against the ProjectASAP glossary

The glossary review found the following required coverage and current gaps.

| Glossary concept | Current ASAPPlanner representation | Missing design support |
| --- | --- | --- |
| Data at rest vs continuously ingesting | Continuous ingest characteristics are available; no explicit arrival mode | Add `DataArrival`; support at-rest statistics without inventing update rate |
| Ingestion volume | Not a first-class workload input | Add evidenced volume with a time basis |
| Ingestion rate | Derived from series count and sample rate | Preserve as evidenced rate; do not conflate with query evaluation rate |
| Input cardinality | Partial `series_count` and distinct-key inputs | Define applicability to dataset, metric, columns, and time window |
| Data distribution | Small built-in enum | Preserve source/freshness; permit deployment-specific distributions later |
| Ad-hoc vs predictable | Not represented | Add predictability independently from recurrence |
| One-time vs repeated | Batch entries and fixed-interval repeating entries | Add scheduled one-time, unknown recurrence, and estimated/scheduled repetition |
| Query volume and characteristics | Fixed interval or structural consumer count | Add observation window, peak/burst and concurrency evidence where latency or capacity models require it |
| Real-time vs longitudinal | Temporal IR can carry ranges; no workload classification | Add time scope plus concrete selection; avoid inferring scope from lookback alone |
| Output cardinality | May be inferred locally; no common evidenced input | Add derived/evidenced value and provenance for costing |
| Lookback window | Represented in temporal query shapes/frontends | Establish query IR as authority and expose it to workload costing |
| CTSA pipeline | Not explicitly modeled | Keep as architectural context; planner consumes collect/store/analyze facts but does not model transmission topology in the MVP |
| CSP(F) | Cost and fidelity partly modeled | Treat scale/performance/fidelity as objectives and constraints; do not collapse fidelity into cost |

Two terminology corrections are required in future code changes:

1. A repeated query is not inherently a streaming-data workload. It may
   repeatedly query data at rest.
2. A one-time query is not inherently stateless. A predictable one-time query
   may justify prepared state, while an ephemeral summary is stateful during
   its one execution.

## Minimal complexity

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
- **Debuggability:** selected and rejected lifecycle alternatives record demand,
  horizon, data statistics, and provenance. Proxy: no lifecycle decision is
  explained only as a scalar cost.
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
