# Planner-layering implementation status

Audience: planner developers. Audit baseline: PR #557 (`e0e1e2d7`), against
[the #509 proposal](../design_docs/proposals/planner-layering.md). The proposal
is a target contract, not a statement that its examples execute today.

| Proposal contract | Evidence at #557 | Remaining scope |
| --- | --- | --- |
| Language frontends and common logical IR | SQL/PromQL/MetricsQL lower to unified operators and scalars. | Floating-point SQL frequency L2 products now have a conservative logical rewrite. Integer products and the entropy idiom remain unrecognized. Preserve alias lineage, filters, NULL groups, empty inputs, count overflow and entropy units when adding recognition. |
| Local exact and summary alternatives | `replacement::summary_candidates`, realization rules and candidate inventory exist; supplied accuracy models reach Pass 1. | Specialized entropy/norm families in Example 2 are illustrative, not registered families. UnivMon certifies only unit-update total count; L2, entropy and cardinality epsilon/delta bounds need verified evidence or a deployment model. |
| Summary-capability sharing | CSE interns structurally identical producers, including states with different readers. | It does not enumerate all partial sharing partitions or resize compatible states to the strictest consumer. Example 2's 37 candidates are not an acceptance result. |
| Window composition | Mergeable state IR/native merge exists; physical pane compatibility and reuse cost helpers exist. | Automatic logical sliding/tumbling/EH alternatives over differing windows, boundary coverage and error proofs are absent. A merge kernel alone does not implement Examples 1/3. |
| Physical materialization | Ephemeral/prepared/shared/continuously maintained lifecycle alternatives, costing, capabilities and latency checks exist. | Incremental query-time pane retention, historical backfill and the complete Example 4 matrix need executable implementations and explicit state/input contracts. |
| Whole-workload selection | One unified selected DAG; shared states are interned and costed across their consumers. | `replacement.rs` documents its selection as non-exhaustive over interacting choices. The proposal's cheapest complete candidate guarantee and 54/156 inventories need a complete workload search/selection path. |
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
