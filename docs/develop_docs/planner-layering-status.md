# Planner-layering implementation status

Audience: planner developers. Audit baseline: PR #557 (`e0e1e2d7`), against
[the #509 proposal](../design_docs/proposals/planner-layering.md). The proposal
is a target contract, not a statement that its examples execute today.

| Proposal contract | Evidence at #557 | Remaining scope |
| --- | --- | --- |
| Language frontends and common logical IR | SQL/PromQL/MetricsQL lower to unified operators and scalars. | Floating-point SQL frequency L2 products and the normalized natural-log entropy idiom now have conservative logical rewrites. Integer L2 products remain unrecognized. Preserve alias lineage, filters, NULL groups, empty inputs, count overflow and entropy units when adding recognition. |
| Local exact and summary alternatives | `replacement::summary_candidates`, realization rules and candidate inventory exist; supplied accuracy models reach Pass 1. | Specialized entropy/norm families in Example 2 are illustrative, not registered families. UnivMon certifies only unit-update total count; L2, entropy and cardinality epsilon/delta bounds need verified evidence or a deployment model. |
| Summary-capability sharing | CSE interns structurally identical producers, including states with different readers. | The strict complete pass enumerates partial sharing partitions for identical admitted producers. It does not resize different state parameters to the strictest consumer. Example 2's 37 candidates are not an acceptance result. |
| Window composition | Mergeable state IR/native merge exists; physical pane compatibility and reuse cost helpers exist. | Cadence-based disjoint pane composition and automatic native materialization-frontier enumeration are available (acceptance below). Retained rotating panes, historical EH buckets and boundary-error certificates remain separate runtime work. |
| Physical materialization | Ephemeral/prepared/shared/continuously maintained lifecycle alternatives, costing, capabilities and latency checks exist. | Incremental query-time pane retention, historical backfill and the complete Example 4 matrix need executable implementations and explicit state/input contracts. |
| Whole-workload selection | One unified selected DAG; shared states are interned and costed across their consumers. | `MajorPass` remains non-exhaustive and rank-compatible. `CompletePass` supplies an exhaustive complete-cost path (acceptance below); the illustrative 54/156 counts are not asserted without their rule sets. |
| Deployment inputs and execution | `PlanningModels` bundles cost, accuracy, evidence and capabilities; native typed UnivMon supports one build with three readouts. | At #557 native exact distinct/L2/entropy fallback is absent; the follow-ups supply those native bindings. End-to-end SQL Example 2 is not established by the native UnivMon fixture. |
| Subtract/delete, parallelism, partitioning and resource planning | Some runtime memory/cancellation limits and maintenance capability flags exist. | These remain proposal TODOs; capability flags do not supply missing IR operators or a physical resource search. |

## Follow-up sequence

1. **Exact frequency execution.** Bind the existing L2/entropy intents to native
   reducers with typed identities, bits for entropy, NULL skipping, zero for an
   empty population, grouping, memory accounting and cooperative cancellation.
   This change supplies the fallback prerequisite; it does not certify UnivMon.
2. **SQL frequency recognition.** Add narrow, proven idiom recognition while
   retaining the original exact relational computation. Entropy with `LN` is in
   nats; the core intent is in bits. SQL `COUNT(*) GROUP BY nullable_key` counts
   a NULL group, while the frequency intents skip NULL. Do not erase these
   differences or SQL's NULL result for `SUM` over no groups.
3. **Summary sharing alternatives.** Enumerate compatible consumer partitions
   and size shared states against all consumers using the supplied accuracy
   model. Keep independent candidates. Tests must inspect the inventory and
   selected producer count, rather than manually construct a shared state.
4. **Tumbling-window composition.** Start with aligned, fixed windows and
   mergeable KLL, then add query-time retention and ingestion-time lifecycle
   choices. Validate offsets, boundary alignment, retention and raw fallback.
5. **Sliding/EH composition.** Separate changes for overlapping active windows
   and historical bucket coverage/error. Do not reuse a merge guarantee as a
   boundary-error guarantee.
6. **Complete physical candidate selection.** Explore interacting workload
   choices and lifecycle assignments, charge each shared producer once and
   check all consumers' accuracy/latency/capabilities. A bounded exhaustive
   implementation must fail explicitly on its budget instead of truncate.
7. **Proposal TODOs.** Design subtract/delete and parallelism/partitioning/
   resource inputs before implementing their planning choices.

The examples' numerical candidate counts depend on their stated rule sets.
Tests should establish those rule sets explicitly before asserting the counts.

## SQL L2 follow-up acceptance

`SemanticEquivalentRewriteStrategy` recognizes a single `SQRT(SUM(c*c))`
output when `c` is a grouped unit count and the product is already Float64.
It follows positional projection lineage, keeps the input predicates, inherits
count accuracy, and restores SQL's NULL result for an empty population. It
retains the original exact candidate. Recognition requires one nonnullable
Boolean, Int64 or Utf8 grouping key, no measure filters/HAVING and no
intervening operators that change the grouped population. The uncast integer
product in Example 2 remains a gap because SQL overflow is observable.

`frontend-sql/tests/frequency_l2.rs` covers recognition, refusal boundaries,
accuracy propagation and candidate retention; `integration-tests/tests/sql_frequency_l2.rs`
executes both SQL and the rewrite through raw connectors and wire compilation.
This step does not supply an L2 accuracy certificate or all sharing partitions.

## Exact distinct follow-up acceptance

The native binding executes `AggIntent::Cardinality` over one typed identity or
an ordered tuple, supports grouping, skips tuples containing NULL and returns
an Int64 zero for empty input. Typed key encoding preserves large integers,
normalizes signed zero and NaN payloads, and retains tuple boundaries. Dictionary
workspace is reserved and released under the normal execution limits. An empty
intent column list retains the existing sample-value convention.

`integration-tests/tests/sql_cardinality.rs` lowers `COUNT(DISTINCT src_ip)` and
executes its exact path through raw scan predicates and native wire binding.
Filtered aggregate measures remain outside native binding's existing scope.

## SQL entropy follow-up acceptance

The proposal's `-SUM(p * LN(p))` form now exposes `FrequencyEntropy` when `p`
is a grouped unit count divided by the full, unpartitioned count window.
Recognition requires one nonnullable Boolean/Int64/Utf8 identity, no
measure filters/HAVING, no ordering, and an unbounded window in both directions.
The rewrite converts the core intent's bits to nats using `ln(2)`, retains
SQL's negative zero for a single identity and uses an exact population count
to restore empty-input NULL. L2 now uses the same exact population guard, so a
zero approximate estimate cannot decide whether SQL returns NULL.

Frontend tests cover recognition, non-equivalent probability/window/unit
shapes, candidate retention and accuracy propagation. Native wire execution
checks filtering, nats, empty population, negative zero and unequal frequencies.
The original entropy SQL graph remains an alternative. Its native SQL window
binding is supplied by the next follow-up below. Neither
recognition nor the exact path proves UnivMon's probabilistic accuracy bound.

## Native entropy fallback follow-up acceptance

Native binding now implements SQL `LN` and the exact `SUM(column) OVER ()`
window over a complete, unordered relation with unbounded start/end bounds.
The window appends its total to every original row, keeps input metadata,
propagates all-NULL totals, supports recovery and obeys workspace limits.
Partitioned, ordered and finite frames remain explicitly unsupported. This
operator executes one SQL relation; it is unrelated to the proposal's missing
streaming tumbling/sliding/EH summary-window planning.

The entropy acceptance fixture now executes both the original SQL and its
frequency rewrite, comparing filtering, natural-log units, empty-input NULL
and unequal frequencies. Raw analytical cost lowering still excludes this
unordered SUM window; supplying native execution does not provide missing
cost evidence or extend the analytical adapter's existing ordered-window rule.

## Automatic window/materialization acceptance

`window_composition::enumerate_window_compositions` derives disjoint relative
panes from a summary's lookback and workload cadence. Five minutes every minute
produces the original state plus a five-pane merge. Non-divisible lookbacks use
the greatest common divisor, so coverage is exact. Source predicates, grouping,
summary parameters and selector offsets/anchors stay attached to the panes.
Unknown recurrence keeps the original; budget exhaustion returns an error.

`compile_materialization_candidates` lowers once and enumerates every legal
producer/reader frontier, including rebuilding all panes at query time. It
retains individual failures and never returns a truncated inventory.
`integration-tests/tests/automatic_window_composition.rs` verifies generated
panes execute identically with and without retained outputs. The deployment
still supplies each selector's exact raw window and binds retained outputs to
that window/revision. This does not introduce a rotating pane cache or claim
that cadence alone certifies compatibility with a catalog's pane origin.

## Complete workload selection acceptance

`pass::CompletePass` is registered as `complete` and re-exported by the planner
facade. Choose it with `UserInput::with_pass(&CompletePass::default())`, or call
`optimize` on an existing `ParsedWorkload`. It is opt-in because the default
`major` pass accepts rank-only models; a rank cannot certify a complete cost.

The pass enumerates the registered logical inventory, cadence-derived pane
compositions, every partial partition of identical sharing-legal producers,
and every legal lifecycle assignment. It checks shared lifecycle/window
consistency and executable phase contracts before comparing complete workload
quotes. Each state is bound to the union of precisely its consumers; a shared
producer is charged once by the additive cost hook. Nonadditive deployments
override `CostModel::complete_workload_candidate_cost`, including interactions
between roots and their physical implementation choices. Scalar workloads need
a complete-workload override because the legacy per-root hooks do not price
scalar execution. Missing/invalid quotes are retained as rejection reasons.

`CompletePass::enumerate` exposes priced full assignments and rejection reasons;
selection returns the cheapest quote in that inventory and records
`PlanOutput::workload_total_cost`. An exceeded logical, pane, partition or
lifecycle budget is an error even if a priced candidate was already found.
There is no heuristic fallback. This guarantee is over registered alternatives
with valid complete evidence; it does not invent missing sketch certificates,
physical implementations or additional parameter-sizing rules.

`integration-tests/tests/complete_workload_selection.rs` executes a winner that
local ranking would miss, finds a winning partial partition, checks union-demand
retention and single producer charging, and rejects budget exhaustion/unknown
costs. Native `compile_materialization_candidates` supplies the executable
frontier inventory to deployment models that compare physical placements.
