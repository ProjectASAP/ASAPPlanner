# DataFusion query-time engine: agent orchestration plan

**Status:** Proposal, in discussion. Companion to [datafusion-query-engine.md](datafusion-query-engine.md); the D-numbers below refer to that doc.

**Audience:** Whoever coordinates the implementation (a person, or a coordinating agent session) and the agents it launches.

This plan splits the work into issues across ASAPPlanner (the new engine crate) and ASAPQuery (the deployment that hosts it). It says which issues can run in parallel, how to run agents on them, and what context each agent gets.

## Relation to ASAPQuery's engine-boundaries design

ASAPQuery's `.design_docs/asapplanner-engine-boundaries.md` (ProjectASAP/ASAPQuery#935, merged) defines the deployment side: `QueryEngine` and `IngestEngine` traits, `QueryDispatcher`, `PlanInstaller`, `ViewStore` and `RawDataStore`, and a 14-step plan. The new engine replaces that doc's executor-backed `QueryEngine` (its I1, I15 and step 9) behind the same `QueryEngine` trait. Its decision 1 already keeps the traits independent of the executor's runtime, for this reason.

### Conflicts to settle before the dependent work starts (issue A0)

Three conflicts fix contracts that two tracks implement in parallel. They are expensive to change later.

**Settled on 2026-11-25:** each one takes the proposed resolution in the table. The remaining work for A0 is updating the engine-boundaries doc to match.

| Conflict | Engine-boundaries doc | This proposal | Proposed resolution | Blocks |
|---|---|---|---|---|
| Batches | `QueryEngine::execute(&AnswerSource, &QueryRequest)` takes one query, and the dispatcher handles queries one at a time | Batches of queries (D2, D10) | Add `execute_batch` to `QueryEngine` and `dispatch_batch` to `QueryDispatcher`; `execute` is a batch of one | E1, W0 |
| View key | Decisions 43/44: `(ViewDefinitionId, ViewInputDataScope)`. The later comment on #935 proposes `(PlanId, PhysicalASAPNodeId, window)`, because `definition_id` omits θ, window width and selection | `(definition_id, selection)` (D4), which is wrong for the same reason | `(PlanId, PhysicalASAPNodeId, window)`; update both docs | E5a, W3 |
| Sketch kernels | The ingest engine builds states with the executor's kernel factory (I7) | Port the kernels into the new crate (D5); the executor is deleted | Move the kernels into a shared crate first (E2), and have both engines use it | E2, E5a, #935 step 8 |

### Conflicts to settle while implementing

Each of these has one owner and changes in one place:

- **Engine-error policy.** The engine-boundaries doc's I12 sends engine errors to the fallback; D16 returns them. The engine returns `Err` either way, and the policy lives only in `QueryDispatcher`.
- **Query matching.** The engine-boundaries doc's I18 matches exact text after trimming; D10 matches the parsed form. Start with exact text; the matching code is private to the dispatcher.
- **Raw data source.** In the engine-boundaries doc, ASAPQuery's ingest engine writes the raw data the plan retains to `RawDataStore` (decision 23, I7). D4 assumed reading from Prometheus. Use the engine-boundaries model; a Prometheus-backed reader is a stopgap if W1 is ready before the ingest engine writes raw data.
- **Store traits.** The new crate defines only the narrow read/write interface it needs. ASAPQuery's `QueryEngine` implementation adapts `ViewStore` and `RawDataStore` to it, so dependencies point one way.
- **Eviction.** It follows the engine-boundaries doc's I17: the query engine evicts by time after each query. That happens in ASAPQuery's wrapper, not in the new crate.
- **Watermark.** The ingest engine owns it (decision 20). The wrapper passes it to the engine for `OnFirstRead` saves (D9).
- **Threads.** I15's thread pool existed because the executor's `RunContext` cannot move between threads. It does not apply to the new engine.
- **`OnFirstRead`.** The engine-boundaries doc does not cover the query engine writing views. D9 adds it, using the existing `ViewStore::put`.

## Issues

| Lane | Issue | Depends on | Notes |
|---|---|---|---|
| Decide | **A0.** Update the engine-boundaries doc with the three settled resolutions | — | Decisions settled 2026-11-25 |
| Spike | **E0a.** Check that a compiled DataFusion `ExecutionPlan` can be run again per batch (D6) | — | Throwaway code; write-up only |
| Spike | **E0b.** Check how the `TimeRange` cross join batches its output (D13 risk) | — | Throwaway code; write-up only |
| Kernels | **E2.** Move `crates/executor/src/summary_kernels/` into a shared crate as the `Sketch` trait (D5); the executor depends on it | — | Also unblocks the engine-boundaries doc's step 8 |
| Engine | **E1 (S1).** Crate skeleton: cutting and scheduler (D3), batch API (D2), read/write interfaces with in-memory implementations (D4), `Filter`, `Project`, `Values`, `Concat`, `Sort`, `Limit` | E0a | Sets the one-file-per-operator layout later issues follow |
| Engine | **E7.** Corpus coverage report: compile every planner corpus plan and list the failures | E1 | Progress measure for later issues |
| Engine | **E3 (S2).** `eval_time`, the `TimeRange` join, `TimeShift`, range functions (D7) | E1, E0b | |
| Engine | **E5a.** Sketch handles and registry, build/merge/estimate functions, view-read leaf (D5) | E1, E2 | |
| Engine | **E6 (S7).** SQL joins, set operations, window functions (D13) | E1 | Independent of PromQL |
| Engine | **E4a.** PromQL aggregates with SQL functions, per-group top-k (D13) | E3 | |
| Engine | **E4b.** `BinaryOp`, ported from the executor's `vector_binary.rs` (D13) | E3 | |
| Engine | **E4c.** Label functions (D11), relabel, the remaining PromQL operators | E3 | |
| Engine | **E5b.** `OnFirstRead` saves (D9), the test switch that treats every view as `OnFirstRead` (D12), query-time window chains | E3, E5a | |
| ASAPQuery | Engine-boundaries steps 1–7 | — | Already planned there |
| ASAPQuery | **W0.** Batch endpoint and `dispatch_batch` (D10) | engine-boundaries step 7, A0 (batch row) | |
| ASAPQuery | **W1 (S4).** `QueryEngine` implementation wrapping the new crate, raw-data plans only | E4a–c, engine-boundaries steps 5–7, raw data available | The integration point |
| ASAPQuery | **W2.** A `promql-compliance` harness mode that plans with ASAPPlanner and runs the new engine (D12) | W1 | |
| ASAPQuery | **W3 (S6).** Views from the ingest engine | engine-boundaries step 8, E5a | The precompute track |
| Finish | **S8.** `check_compiles` moves to the new crate; `crates/executor` and the old ASAPQuery engines are deleted (engine-boundaries step 14) | all | |

## What runs in parallel

```text
Wave 1:  A0 (doc update)  ‖  E0a  ‖  E0b  ‖  E2  ‖  engine-boundaries steps 1–7
Wave 2:  E1                       (after E0a)
Wave 3:  E3  ‖  E5a  ‖  E6  ‖  E7
Wave 4:  E4a  ‖  E4b  ‖  E4c  ‖  E5b  ‖  W0
Wave 5:  W1 → W2;  W3 once engine-boundaries step 8 lands
Wave 6:  S8
```

The critical path is E0a → E1 → E3 → E4a/b/c → W1 → W2. E2 is the only issue that unblocks the precompute track, so it starts in wave 1.

## How to run the agents

- **One coordinating session per repository.** It owns a tracker issue with an order table and a decisions section. It writes each issue's brief, launches the agents, and checks their PRs against the design doc. It does not write engine code itself.
- **Three agents per issue**, following `AGENTS.md`: consequential changes get test design or review from someone who did not implement them.
  1. **Test author:** writes the acceptance tests from the issue, in a PR of tests only (`#[ignore]` until implemented). A person approves these for E1, E3 and W1, since they fix semantics.
  2. **Implementer:** makes those tests pass. It changes a test only by flagging it in the PR.
  3. **Reviewer:** a fresh agent given the code and the design doc, not the implementer's reasoning.
- **Isolation:**
  - one worktree per issue: `.worktrees/<issue>` in ASAPPlanner, `gwt.sh` in ASAPQuery;
  - stacked branches;
  - PRs of about 400 lines or fewer, each compiling and passing tests.
- **Avoiding conflicts between parallel agents:**
  - E1 creates `lower/<operator>.rs` modules and one registration list, so later agents mostly add files.
  - Changes to the plan types (`crates/types`) or to the engine's read/write interfaces go back to the coordinator.
- **People decide:** A0, the acceptance tests for E1, E3 and W1, and every merge. Agents never merge.

## Context each agent gets

Each brief follows one template:

1. **Goal and scope:** the issue text, its acceptance criteria, and what is out of scope. It states the simplest-first rule: no performance work.
2. **Decisions:** the relevant D-numbers in [datafusion-query-engine.md](datafusion-query-engine.md), and for ASAPQuery work the engine-boundaries decisions. These are linked, not pasted.
3. **Code to read:**
   - Plan types: `crates/types/src/plan/physical_asap_dag.rs` and `crates/types/src/ir/operator/{asap,non_asap}.rs`.
   - The operator's D13 row.
   - The executor file to port, if any (`vector_binary.rs`, `range_function.rs`, `summary_kernels/`). These are read only: the executor's behavior is not trusted.
   - For kept outputs, sidra's notes (`sidra/doc_dev/observation/1012_duckdb_datafusion_performance_gap.md`).
4. **Rules:**
   - `AGENTS.md`;
   - the worktree path and the branch to stack on;
   - stop and report if the plan types or the engine's interfaces would need to change.
5. **Done means:** the commands to run (`cargo test -p asap-query-time-batch-engine`, the E7 coverage diff) and a PR in the repository's template.
6. **Report back:** the PR link, what is still not covered, and any decision the agent had to make, flagged rather than buried.
