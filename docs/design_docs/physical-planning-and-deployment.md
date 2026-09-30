# Physical Planning, Summary Maintenance, and Deployment

## 1. Architecture

A Post-ASAP computation is progressively realized through four layers:

```mermaid
flowchart LR
    L["Logical Post-ASAP DAG<br/><b>What computation?</b>"]
    M["Summary Maintenance Lifecycle<br/><b>How is state maintained?</b>"]
    P["Physical DAG(s)<br/><b>How is it executed?</b>"]
    D["Deployment Plan / DAG<br/><b>How is it instantiated?</b>"]

    L -->|"Summary Maintenance<br/>Candidate Generation"| M
    M -->|"Physical Plan<br/>Compiler"| P
    P -->|"Deployment Plan<br/>Compiler"| D
```

| Layer | Defines |
| --- | --- |
| **Logical Post-ASAP DAG** | Computation semantics |
| **Summary Maintenance Lifecycle** | Build, retention, reuse, and window strategy |
| **Physical DAG(s)** | Supported physical candidates, executable operators and typed input boundaries |
| **Deployment Plan / DAG** | Selected candidate, concrete data/state bindings and operational lifecycle |

ASAPPlanner owns the first three layers and the shared physical operator
implementation library. Deployment systems such as ASAPQuery and asap-fusion
own deployment compilation and operation. The lifecycle is a planning contract
associated with the logical DAG, not a separate computation IR.

The Logical Post-ASAP DAG is preceded by the Pre-ASAP DAG (`PreASAPNode`), the
language-independent query semantics before summary selection. Both are
logical. Planning builds Post-ASAP `PostASAPNode` trees; `export_post_asap_dag`
exports the selected tree as a `PostASAPDAGTransport`, which is the Physical Plan
Compiler's input. Its per-node execution phase (ingestion or query time) is
decided by the selected summary maintenance lifecycle, as the layer contract
below states.

### Layer contract

1. **Logical Post-ASAP** (`CandidatePostASAPDAGs`) decides what to compute: summary
   families, readouts and sharing. It does not decide placement; timing that a
   realization strategy writes while building a candidate is provisional.
2. **Summary maintenance lifecycle** (Planner) lists the lifecycle choices for
   each unique retained state: every summary state (`SummaryAgg`) and every
   maintained population that does not feed a summary state.
   A chosen assignment determines every node's
   `ExecutionTiming`, plus window framework and retention.
   `SummaryMaintenanceLifecyclePlan::export_timed_dag` applies it: a retained
   (non-`Ephemeral`) state and all of its inputs run at ingestion time;
   readouts, other consumers, and `Ephemeral` states not consumed by retained
   state run at query time. A population that feeds a summary state is one of
   that state's inputs and follows its timing.
3. **Physical compile** (Planner) reads timing: ingestion-time nodes form the
   precompute DAG and the rest form the query DAG, joined by typed outputs. It
   does not see raw ingestion, panes, storage or stored-state readout.
4. **Backend** chooses the lifecycle assignment with its own `CostModel`:
   precompute CPU (`maintenance_cost_per_update`), sketch/summary store cost
   (`retention_cost_rate`), query reads (`summary_read_cost`) and per-query
   builds (`build_cost`, for `Ephemeral`), counting shared state once.
   `Ephemeral` requires the deployment to supply the state's raw input as a
   query-time source.

### Candidate generation and deployment selection

Planner exposes the supported, semantically legal **physical plan candidates**.
It does not discard a computation family or materialization placement merely
because a deployment-independent cost estimate prefers another candidate.
Logical candidates are an internal search stage, not the deployment handoff.

```text
Query semantics + accuracy and lifecycle requirements
                    ↓ Planner
Supported Physical DAG candidates + typed inputs/outputs + requirements
                    ↓ backend
Binding feasibility + runtime statistics + resource limits + ERP
                    ↓ backend deployment compiler
Selected PrecomputePlan + QueryPlan + StoredOutputReferences
```

Planner owns operators, dependencies, sharing, and each candidate's
materialization frontier. The backend rejects candidates it cannot realize and
prices feasible candidates over a comparable workload and time horizon. It binds
the selected candidate; it does not lower the logical computation again, exchange
operators, or move an operator across the selected frontier. A missing quote is
not a zero-cost implementation. ERP evidence cannot authorize an illegal rewrite.

The candidate inventory must identify its supported search scope and budget.
If a configured exhaustive enumeration exceeds its budget, planning fails
explicitly instead of selecting from an undisclosed partial inventory. Reports
separate unsupported compilation, deployment infeasibility, missing evidence,
and a feasible candidate that loses on cost. Absence is not a cost comparison.

For `sum by(job)(rate(m[1m]))`, Rate remains per series before grouped Sum.
`CandidatePostASAPDAGs` offers one such candidate, with a per-series Rate state and a grouped
Sum state. Its lifecycle assignment places it: a retained Sum state finalizes
Rate and builds Sum within a bounded precompute run; an `Ephemeral` Sum over a
retained Rate state leaves the Rate readout and Sum in the query DAG. Storing a
value requires its exact evaluation window, revision, readiness and serving
cadence to match the query contract.

For instant-vector TopK, CMS/CountSketch with a candidate heap requires explicit
series identity and a supported latest-value input protocol. Appending historical
sample values does not preserve instant-vector semantics. Replacement, rank
decrease, expiry, grouping and the required approximation guarantee must be
validated before admitting that physical candidate.

Planner's candidate space decides what to compute, not placement. For an
instant-vector PromQL TopK, Planner resolves rows that carry the complete series
identity and lists the current-series heap realizations per root with the other
candidates, unranked. Precompute or query-time placement of Rate and grouped Sum
is not a separate Planner candidate: the summary maintenance lifecycle assigns
each node's timing, and the physical compiler reads it.

This is the target ownership contract. A backend path that still reconstructs
operators from logical candidates has not completed this integration.

### Input semantics and summary semantics

`source`, `filter`, `grouping` and `window` describe input-data semantics:
where records originate, which records qualify, how they are grouped and which
time interval applies. They are not a complete description of arbitrary summary
computation. In particular, the same four fields can summarize different value
expressions or produce different states.

| Concern | Required semantic information |
| --- | --- |
| Input computation | Source identities and schemas, filters, joins/transforms and their order, or a reference to the canonical input sub-DAG |
| Values and grouping | Value expressions, item identities and weights where applicable, group keys and types, and operation-defined null/duplicate handling |
| Time | Time column and interpretation, interval bounds, evaluation alignment, and distinction between query range and maintained panes |
| Summary computation | Exact operation or sketch family, algorithm and parameters, and supported build/merge behavior |
| Output | State versus finalized value, output schema/type, and readout parameters when part of the output computation |

For example, KLL over `latency_seconds` and KLL over `log(latency_seconds)` differ
even with identical source, filter, grouping and window. Likewise, weighted
frequency state needs both item and weight expressions. More complex inputs
must retain their computation DAG; four descriptive fields cannot replace it.

The canonical selected computation is authoritative. These categories describe
what must be preserved, not a new flat IR or a second expression language.
Operator-defined behavior should be referenced through its canonical contract,
not independently configured in deployment metadata. Unsupported or unresolved
semantics cannot be treated as compatible.

Logical planning defines the semantics; physical compilation realizes them as
operators and typed boundaries. Deployment binds concrete readers and state
records that satisfy those requirements. A stored summary definition records or
references the relevant semantics for compatibility checks. Matching a definition
alone does not establish actual window coverage, revision compatibility or
readiness; those require runtime checks. Physical location, encoding, scheduling
and retention are separate execution/deployment contracts.

Persisted semantic identity, its wire format and any tenant or dataset binding
belong to the deployment. Planner provides the typed `PostASAPDAGTransport` that a
deployment canonicalizes; it does not define a stored-definition format.

### Running example

Suppose p50 and p99 are requested over the same latency samples in a five-minute window,
and one Planner candidate uses KLL with `k=200`. Assume query windows align with one-minute
pane boundaries and that the selected parameters satisfy the required guarantees.
Operator names below are illustrative; the example defines the design, not a
claim that the entire deployment integration is implemented.

The data source identifies where samples come from. Filters, grouping and the
window determine which samples enter each summary. Here `pane_duration: 1m`
means each stored pane covers one minute; the query range is five minutes.
Neither duration specifies how often maintenance runs or how long state is kept.

The example evolves through the architecture as follows:

```text
1. Logical Post-ASAP DAG

raw latency
     ↓
KLLBuild(k=200)
     ↓
KLLMerge
   ┌─┴─────┐
   ↓       ↓
  p50     p99

          │
          │ Summary Maintenance Candidate Generation
          ▼

2. Summary Maintenance Lifecycle

KLLBuild(k=200)
  strategy = continuously maintain
  window   = 1-minute panes
  reuse    = p50 + p99
  query    = merge panes covering requested aligned 5 minutes

          │
          │ Physical Plan Compiler
          ▼

3. Physical DAGs

Precompute DAG:
RawInput<Latency, 1m>
        ↓
NativeKllBuild(k=200)
        ↓
KllStateOutput

Query DAG:
InputSlot<KllState>[5 panes]
        ↓
NativeKllMerge(k=200)
      ┌─┴────────┐
      ↓          ↓
 NativeP50   NativeP99

          │
          │ Deployment Plan Compiler
          ▼

4. Deployment Plan / DAG

Precompute:
OTLP latency source
        ↓
run KLL build over each complete 1-minute input pane
        ↓
store as latency-kll-1m/<window>

Query:
resolve five latency-kll-1m states
        ↓
execute query Physical DAG
        ↓
return p50 / p99
```

Each stage adds a different class of decision while preserving the preceding
contracts. Here, continuous maintenance means recurring production of pane state;
the bounded build DAG does not itself implement an unbounded streaming window.

## 2. Logical Post-ASAP DAG → Summary Maintenance Lifecycle

The **Logical Post-ASAP DAG** defines computation semantics:

```text
Scan(latency)
     ↓
KLLBuild(k=200)
     ↓
KLLMerge
   ┌─┴────────────┐
   ↓              ↓
Quantile(.5)  Quantile(.99)
```

It establishes that KLL with `k=200` is used and that the merge is shared by the
two readouts. It does not determine when KLL states are built or retained.

**Summary Maintenance Candidate Generation** enumerates legal lifecycle choices
using workload demand, window/freshness requirements and supported physical
implementations. Backend selection uses runtime feasibility and cost after
physical compilation. The following example follows one candidate.

Candidate generation and selection are separate steps. For every unique retained
state, enumeration reports each lifecycle (ephemeral, prepared, shared,
continuously maintained) as legal, with a Planner cost or explicitly unknown
cost, or as rejected with a reason. Planner does not remove a legal alternative
because its own estimate prefers another. A deployment prices the legal
alternatives over the whole workload, counting shared state once, and binds one
lifecycle per state. Binding checks that the choice is legal and that states on
one maintenance path share an evaluation schedule. An alternative whose cost is
unknown can be bound only when the deployment's cost model is authoritative for
complete-candidate cost; unknown cost is never treated as zero. It then yields the same
lifecycle guarantee and window framework the physical compiler consumes when
Planner selects. Planner's own cheapest-alternative selection remains available
for callers without deployment pricing. The window framework is decided for the
complete combination, not for one alternative in isolation.

A maintained population (for example, the current series of `topk by(job)(1, m)`)
is retained state like a summary. Retaining it maintains the latest sample per
series at ingestion and leaves only the readout at query time. Choosing
`Ephemeral` rebuilds that snapshot from raw samples for each query, so the
deployment must supply the raw source at query time. The caller's `CostModel`
prices both through the same lifecycle hooks; a model without population
evidence leaves them unknown, and they are not selected.

For the running example, assume it selects:

```text
producer: KLLBuild(k=200)

strategy:
    continuously maintain

window realization:
    1-minute panes

query requirement:
    combine panes covering the requested aligned 5-minute range

reuse:
    one merged state serves p50 and p99
```

This produces the **Summary Maintenance Lifecycle**.

The lifecycle specifies how the selected logical summary should be maintained,
but not its concrete operator implementation or storage location.

Physical feasibility may feed back into selection. For example, if the required
pane-based maintenance cannot be implemented, this lifecycle candidate cannot be
selected. One-minute panes alone also cannot cover an arbitrarily phased query
window; that requires supported boundary handling or a different candidate.

## 3. Summary Maintenance Lifecycle → Physical DAG

The **Physical Plan Compiler** consumes both computation semantics and maintenance
requirements:

```text
Logical Post-ASAP DAG (PostASAPDAGTransport)
+ Summary Maintenance Lifecycle
+ physical capabilities
        ↓
Physical Plan Compiler
        ↓
Physical DAG(s)
```

For the running example, the lifecycle creates two execution boundaries.

These two halves are named as `PhysicalCandidate` names them, `precompute`
and `query`. *Maintenance* stays the lifecycle's word (section 2): it covers
how state is built, retained, reused and scheduled. A precompute DAG is the
physical object that a maintenance lifecycle compiles to, so reusing
*maintenance* for it collapses two layers that the crates keep apart:
`asap-aware-mapping::summary_maintenance_*` owns the lifecycle, and
`asap-physical-operators::physical_planner` owns the DAGs.

### Precompute Physical DAG

```text
RawInputSlot<Latency>(
    window = 1m,
    bounded = true
)
        ↓
NativeKllBuild(k=200)
        ↓
KllStateOutput(k=200)
```

This DAG computes the state of one maintained one-minute pane. Its input
contract requires all input samples matching the source, filters and group within that pane; the deployment supplies that
bounded input from its source integration.

### Query Physical DAG

```text
InputSlot<KllState>(
    k = 200,
    coverage = requested aligned 5m
)
        ↓
NativeKllMerge(k=200)
      ┌─┴──────────────────┐
      ↓                    ↓
NativeQuantile(.50)   NativeQuantile(.99)
```

The Physical Plan Compiler chooses `NativeKllBuild`, `NativeKllMerge`, and the
physical quantile implementations, validates state compatibility, and preserves
the shared merge. It also resolves expressions, schemas, ordered dependencies
and execution properties.

The resulting Physical DAGs know that compatible KLL states are required, but
do not know where those states are stored.

For example:

```text
InputSlot<KllState>
```

is physical, while:

```text
s3://.../latency-kll/12:01
```

is deployment-specific. Placement and scheduling also remain outside the Physical
DAG. If the required behavior cannot be realized, physical compilation fails.

### Physical candidates include precompute computation

Materialization frontiers are Planner decisions. A candidate records both the
precompute Physical DAG and the query Physical DAG, with typed outputs connecting
them. The deployment compiler binds those outputs; it does not move operators.
Lifecycle timing gives the frontier: ingestion-time nodes read by query-time
nodes. For `sum by(job)(rate(m[1m]))`, the two lifecycle choices of the single
logical candidate give:

```text
Candidate A (Rate state retained, Sum Ephemeral):
  precompute: counter samples → per-series Rate state
  materialized output: per-series Rate states for window/evaluation/revision
  query: stored Rate states → Rate readout → grouped Sum → result

Candidate B (Rate and Sum states retained):
  precompute: counter samples → per-series Rate → grouped Sum state
  materialized output: grouped Sum states for window/evaluation/revision
  query: stored grouped Sum states → Sum readout → result
```

Explicit frontiers passed to `compile_candidates` can also persist per-series
rate values; deriving that frontier from timing inside the physical planner is
not yet implemented.

Both preserve reset-aware Rate before Sum. Summing raw counters before Rate is
not equivalent. The counter-state build may be another precompute DAG; typed
state inputs do not imply that a deployment can construct or bind those states.

The shared library exposes `physical_planner::compile_candidates(...)` to lower
explicit frontier candidates to `PhysicalCandidate { precompute, query,
materialized_outputs }`. `select_candidate(...)` accepts deployment feasibility
and scoped complete-workload costs and chooses the lowest-cost feasible
candidate. Costs must describe the same workload and planning horizon; missing
feasibility is rejected before pricing. The optimizer supplies candidate
frontiers and cost evidence, including updates, retention, recurrence and sharing.
`enumerate_frontiers` constructs bounded, reachable antichain frontiers above explicit input boundaries, including query-only and fully precomputed results. It fails explicitly when the candidate budget is exceeded. Maintenance selection must still reject frontiers that violate window, freshness, or reuse requirements; deployment feasibility is checked before pricing.

The lifecycle layer decides timing; physical compilation reads it. Lowering a
node does not depend on the frontier, so each query DAG is lowered once and
different lifecycle assignments are different cuts of that lowering.
`compile(dag, inputs, roots)` yields the complete `PhysicalDAG`.
`frontier_from_timing` reads an assignment's timing (a lifecycle assignment's
view, or a DAG from `export_timed_dag`) and returns its frontier: ingestion-time nodes read by
query-time nodes, or an ingestion-time root; a query-time node feeding an
ingestion-time node is rejected. `cut_candidate(&compiled, &frontier)` then
partitions the lowered operators: the frontier's ancestors form the precompute
DAG and the rest form the query DAG. Helper operators are numbered by their
Planner node (`u64::MAX - (node_id << 16) - index`), so a cut is byte-identical to
`compile_candidate` for that frontier.
`compile_candidate(s)` and `enumerate_frontiers` wrap the same path. Temporal
pane candidates remain a separate lowering.

Physical compilation opens no readers. Bounded precompute outputs become typed
query inputs. Their source, filters, grouping, build window, evaluation time, readiness and
revision contracts must accompany the selected lifecycle and be checked during
deployment binding. Type compatibility alone does not establish reuse legality.

The Planner integration test executes both candidates through the shared runtime
and reverses the selected frontier with two controlled cost fixtures. It also
rejects shadowed/duplicate boundaries and incomparable planning horizons. This
establishes Planner capability; it does not establish that ASAPQuery currently
supports persisting every scalar/result-output frontier.

## 4. Physical DAG → Deployment Plan / DAG

The **Deployment Plan Compiler** binds the Physical DAGs to the concrete deployment:

```text
Physical DAGs
+ Summary Maintenance Lifecycle
+ deployment catalog/state
+ sources/materializations
+ operational policy
        ↓
Deployment Plan Compiler
        ↓
Deployment Plan / DAG
```

For the precompute DAG, it may produce:

```text
Source:
    RawInputSlot<Latency>
        → complete bounded panes from the OTLP latency source

Schedule:
    each 1-minute pane, once its completion requirements are met

Execution:
    RawInput → NativeKllBuild(k=200)

Output:
    KllStateOutput
        → latency-kll-1m/<window>
```

For a query over `(12:00, 12:05]`, its input-binding rule resolves:

```text
InputSlot<KllState>[5 panes]
    ├── latency-kll-1m/(12:00,12:01]
    ├── latency-kll-1m/(12:01,12:02]
    ├── latency-kll-1m/(12:02,12:03]
    ├── latency-kll-1m/(12:03,12:04]
    └── latency-kll-1m/(12:04,12:05]
              ↓
        Query Physical DAG
              ↓
           p50, p99
```

The Deployment Plan Compiler establishes bindings and checks that their contracts
satisfy the physical inputs and selected lifecycle, including KLL parameters,
source, filters, grouping, window coverage and revision scope. The deployment engine
resolves request-specific states and checks their actual coverage, revisions and
readiness at execution time. A compiled plan cannot establish future readiness.

The compiler does not replace `NativeKllMerge`, choose another sketch, or decide
to maintain different windows. Such changes require replanning. A Deployment
Plan / DAG is an operational instantiation, not another computation IR.

## 5. Responsibility Boundary

The complete example makes the ownership boundary explicit:

| Stage | KLL example decision |
| --- | --- |
| **Logical Post-ASAP DAG** | Use `KLL(k=200)` with shared merge for p50/p99 |
| **Summary Maintenance Candidate Generation** | Maintain 1-minute panes and reuse them for aligned five-minute queries |
| **Summary Maintenance Lifecycle** | Record pane/window/freshness/reuse requirements and each node's execution timing |
| **Physical Plan Compiler** | Lower to native KLL build, merge, and readout operators |
| **Physical DAG** | Define precompute and query DAGs with typed input/output boundaries |
| **Deployment Plan Compiler** | Bind raw input and KLL state slots to concrete sources/materializations |
| **Deployment Plan / DAG** | Specify maintenance schedules, stored-pane resolution and query execution |

```text
Logical:
    "Use KLL for p50/p99."

Lifecycle:
    "Maintain reusable 1-minute KLL panes."

Physical:
    "Execute NativeKllBuild and
     NativeKllMerge → {p50, p99}."

Deployment:
    "Read OTLP here, store panes here,
     and bind these five panes for this aligned query."
```

The deployment engine executes the bound Physical DAGs through ASAPPlanner's
shared physical operator implementation library, `asap-physical-operators`, and
its DAG runtime. The merge executes once per run for both consumers. Execution
does not introduce additional planning decisions.

The physical layer does not own raw ingestion, pane construction or geometry,
storage formats, or decoding persisted bytes into typed state. It compiles
computation over typed input contracts: summary build, merge (for example KLL
merge), sketch estimates and exact finalization. The deployment constructs panes,
reads and decodes stored state, and binds the typed values to input slots.
Compiled physical plans are Planner outputs and keep their own serialized form.

Each maintained pane contributes its input samples once. A replacement snapshot
replaces that pane's state; query merging must not count both the old and new
snapshots as separate inputs.

## 6. Executable acceptance coverage

The tests cover optimizer-selected lifecycle execution alongside independent
operator/runtime fixtures:

| Test | Contract exercised |
| --- | --- |
| `summary_maintenance_lifecycle_e2e::continuous_lifecycle_compiles_and_executes_spatial_kll` | PromQL workload → selected continuous lifecycle → logical DAG → compiled precompute/query candidate → results in independent revisions; an unbounded candidate fails before pricing, and a bounded request candidate summarizes the same input samples |
| `summary_maintenance_lifecycle_e2e::chosen_lifecycle_timing_decides_precompute_contents` | PromQL workload → enumerated lifecycles → explicit choice → timed DAG → compiled candidate; ContinuouslyMaintained stores the state in precompute, Ephemeral leaves precompute empty and reads the raw source at query time; both return the same p99 |
| `summary_maintenance_lifecycle_e2e::lifecycle_timing_cuts_one_compilation` | KLL quantile and grouped Rate→Sum: one compilation cut by the ContinuouslyMaintained and Ephemeral timed DAGs equals `compile_candidate` for each; the frontier is the retained state or empty |
| `summary_maintenance_lifecycle_e2e::chosen_population_lifecycle_decides_precompute_contents` | PromQL `topk by(job)` over a maintained population → explicit choice → timed DAG → compiled candidate; ContinuouslyMaintained stores the population in precompute, Ephemeral rebuilds it from raw samples at query time; both rank alike |
| `summary_maintenance_lifecycle_e2e::planner_lifecycle_selection_reproduces_strategy_timing` | For PromQL summary fixtures, the timed DAG from Planner's retained selection equals the DAG realization strategies produce |
| `kll_pane_execution::five_panes_roundtrip_and_shared_merge_runs_once` | Explicit one-minute precompute DAGs → real MessagePack state bytes → five required query inputs → shared native merge → p50/p99; counts every sample once, checks adjacent aligned windows and instruments one merge start per run |
| `kll_pane_execution::restored_panes_reject_corruption_parameters_schema_and_missing_binding` | Corrupt bytes, parameter relabelling, incompatible schemas and absent bindings fail explicitly |
| `precompute_candidates::grouped_rate_can_be_materialized_before_or_after_grouped_sum` | Cost changes select different legal precompute frontiers; both selected candidates execute with the same reset-sensitive result; uncompilable candidates are not priced |
| `sql_to_physical::sql_filter_grouped_sum_executes_and_rebinds` | SQL text → candidate search → physical compilation → shared Scan predicates and grouped summary execution; NULL samples are ignored and fresh bindings produce new results |

Pane construction, pane timestamp checks, stored identity, revisions, readiness
and complete coverage of required input samples are deployment
responsibilities. Real storage and HTTP execution belong to
deployment-repository E2E tests.
