# Testing the planner

Audience: planner developers. This page says which kind of test asserts
what, how tests are named, ignored and tiered, and which combinations of
query features the suite covers today. Tracking issue: #1253.

**Goal.** Each test layer asserts one kind of thing. A design change then
fails the tests of the rule it changes, not a large share of the suite, and
a cost or answer bug is caught by a check that does not share code with the
planner's own costing.

## Layers

| Layer | Asserts | Inputs | Where |
|---|---|---|---|
| Rule unit tests | one rule over a minimal memo: the candidates, edges or prune causes it adds | a one- or two-query workload planned up to the stage under test through the production frontends and Stage 0 | each stage crate's tests |
| Contracts | invariants of every stage's output (below) | every workload of the corpus registry | `crates/integration-tests/tests/contracts.rs` |
| Oracles | the planner's cost or answers against a computation that does not use the planner's costing | canonical shapes and small synthetic data sets | `crates/integration-tests/tests/{cost_oracle,execution_oracle}.rs` |
| Metamorphic relations | how the cost of two related workloads must compare | `planner_fuzz` with fixed seeds | `crates/devtools/tests/planner_fuzz.rs`, the `planner_fuzz` bin |
| Regressions | one bug stays fixed | one minimal workload per issue | next to the code the bug was in, or `crates/integration-tests` |
| Snapshots | the full selected plan and its cost do not change unexplained | the corpus registry | `crates/devtools/tests/corpus_snapshots.rs`, [planner snapshots](planner-snapshots.md) |

Snapshots are the only place that pins whole plans, candidate counts and
costs. Every other test asserts a property, a rule's output or an oracle's
number, so a change that moves an unrelated plan does not fail it.

### Rule unit tests

A rule unit test builds a memo with the production path, so it sees the
lowering a user's query gets (series identity included), then asserts what
one rule adds. It does not assert what later stages select. Example: Pass 2's
chain rule over `quantile_over_time(0.9, m[5m])` every 60 s adds one
`SummaryTumblingWindowChain` group with width 60 s; the test checks that
group and its candidates, not the plan.

Build inputs only through the production frontends and Stage 0:
`corpus_registry::repeating_promql_workload` writes a small workload and
`corpus_registry::plan_workload(.., until)` plans it up to the stage under
test with `asap_planner`'s own lowering and planning models
(`planning_stages::lower_workload`, `planning_models`), and Stage 3 through
the facade's `run_planning`. A test that
assembles IR by hand can drop what the frontend adds (for example the
series identity column) and then pass on a memo no user can produce.

### Contracts

Checked on every corpus registry workload:

- every id resolves (memo groups, candidates, physical candidates, edges,
  plan nodes), and every candidate's input groups exist;
- every edge the plan uses is logically and physically admissible
  (`PhysicalEdge::is_logically_and_physically_admissible`);
- the selection MILP's objective equals `asap_cost::plan_cost` of the
  selected plan, to a relative 1e-9;
- every node's cost is ≥ 0;
- every query root is served by a plan node of its query's group;
- constraint (K) holds on the selected plan: each query's composed error
  bound, recomputed over the plan from its readouts' requirements and the
  error contracts, is within its target.

A contract that fails on one workload names the workload id and the
violated invariant. `contracts::contract_violations(&planned, &cost_inputs)`
checks one planned workload, so a regression test can check its own
workload too (`milp_objective.rs` does). (K) is recomputed over the plan,
not read from the MILP: per node and metric, the node's readout requirement
plus β times each input's error (`LogicalRequirements::error_propagation`),
and at each query root within the target's ε and per-value δ, as the MILP
bounds it (no δ check without a target δ; at a scalar root no error is
accepted).

Three contracts are consistency checks, not oracles, because each reuses
the planner code it checks:

- MILP objective = `plan_cost`: Stage 3 and `plan_cost` share
  `node_resource_usage_and_cost`, so a costing bug can leave them equal
  (Example 1 of #1253);
- edge admissibility: Stage 2 builds edges with the same
  `is_logically_and_physically_admissible` predicate, so a bug in the
  predicate passes;
- (K): the recomputation uses the planner's error contracts and
  propagation factors, so a wrong contract passes.

They catch a stage that disagrees with another (a dropped constraint, a
stale id); the oracles below catch a wrong rule.

### Oracles

| Oracle | Expected value computed from | Tolerance |
|---|---|---|
| Hand-computed cost | the cost formulas of the concepts doc ([§5, "Cost and retention"](../design_docs/concepts/memo-groups-properties-requirements.md)) evaluated in the test: executions per second, rows or states read, windows selected, retained windows, times the unit costs | relative 1e-9 per node and for the plan |
| Execution | the selected plan run by the executor on synthetic data, against an exact evaluation of the query over the same data | exact queries: equal (floats to 1e-9 relative); approximate queries: the fraction of values off by more than ε is at most δ, measured over a stated number of values with a fixed seed |
| Exhaustive search | every plan of a small memo (Appendix A of the concepts doc) that passes the contracts (admissible edges, (K)), costed with `plan_cost`, against the MILP's | the MILP's cost equals the cheapest enumerated plan's, to a relative 1e-9 (`mip_rel_gap` is 0); a memo with more than 10⁵ plans is not enumerated and the test reports the cap was hit |

The concepts doc and its Appendix A are added by #1224
(`docs/memo-group-model`); the link resolves once #1224 merges.

The exhaustive search requires equality, not "MILP ≤ enumerated": a MILP
that drops a constraint returns a cheaper invalid plan, and an enumeration
that misses plans returns a dearer one; both pass a one-sided check. The
enumerated plans must also pass the contracts, so the two sides cannot agree
on an invalid plan.

A hand-computed test does not call the planner's costing:

- each expected value is a literal, or arithmetic over literals the doc
  comment states (counts such as executions per second, windows selected,
  retained windows);
- only the unit costs (cost per CPU operation, per retained byte-second) are
  read from the cost inputs; no `asap_cost` function or planner helper
  derives a count;
- where a known bug exists, the test is shown to fail on the code before
  its fix (Example 1 of #1253 on the code before #1237).

The canonical cost shapes are a `Scan`, a chain built at ingestion time, a
`TimeRange` over a chain, a `TimeRange` over a `TimeShift` over a chain (an
offset range query, o11y q21) and a `SummaryMerge` of selected windows. A
hand-computed test states each count it uses in its doc comment (for q21:
61 evaluations per 60 s, 5 windows of 1,000 states selected per evaluation).
It costs the selected plan with cost inputs that charge only CPU
operations, or only retained bytes, so each node's expected cost is a count
times one unit; the per-operation work (one operation per row, state or
accumulator) is the cost model's and is stated in the test, the counts are
what the oracle checks.

The execution oracle runs the plan as a runtime does: each materialized
view (a chain) is refreshed once per window its retention keeps, in an
ingestion run over that window's samples, then the query run reads them.
Its synthetic data is 200 series of one gauge, one sample per second for
30 minutes, each value uniform in [0, 100) from a fixed seed plus 10 times
the minute it falls in, so a plan that reads the wrong windows moves its
answer far outside ε; an approximate query is checked over all 200 values
(at most δ × 200 may be off), and two answers for one series fail. The
query run is bound raw data only when the deployment keeps it, so a plan
that scans raw data on a deployment without it fails. A
query shape the executor cannot run yet is ignored with the executor
issue that adds it, not reshaped.

### Metamorphic relations

`planner_fuzz` relates two workloads whose costs must compare in a known way
([its README](../../crates/devtools/src/planner_fuzz/README.md) lists the
relations). It checks, among others: adding a query never lowers cost
(`added-query`), and a duplicate query adds about 0 (`f-exact-duplicate`).
Tests run fixed seeds; the long runs are nightly.

Two relations are not checked yet (T5):

- tightening accuracy never lowers cost: every fuzz workload is planned with
  accuracy not enforced (the README's "looser accuracy reads the tighter
  build" row is "not checked");
- scaling a source's rate scales the cost it drives: no relation changes a
  rate.

### Regressions

One minimal workload per issue, smallest that still shows the bug. The doc
comment names the issue and states the behavior; the name states the
behavior, not the issue number.

## Corpus registry

One list of in-repo workloads, each with the deployment it is planned for
(capabilities, cost inputs, whether accuracy is enforced), read by the
contracts, the snapshots and the oracles:
`asap_devtools::corpus_registry::corpus_workloads()`, ids
`<source>/<name>` (e.g. `o11y_bench/q21`). `CorpusWorkload::plan(until)`
plans one as `asap_planner --run-until <until>` does, and
`CorpusWorkload::asap_planner_args` writes its inputs for the CLI.
The ids' source prefixes are `corpus_registry::SOURCES`; `planner_fuzz`'s
tests read their seeds from the registry (`fuzz_seed_workloads`).

| Source | Workloads |
|---|---|
| o11y benchmark corpus (`crates/devtools/tests/corpus_snapshots/o11y_bench/corpus.json`) | one per checked query |
| sketch plan snapshots (`crates/devtools/tests/sketch_plan_snapshots/manifest.json`) | the MVP and raw-retained deployments' workloads |
| planner-layering examples (`tools/dag-viewer/workloads/planner-layering/`) | examples 1, 2, 3a, 3b |
| viewer memo examples (`tools/dag-viewer/memo-examples/`) | PromQL and SQL |
| random mixed workload sets (`tools/dag-viewer/workloads/index.json`) | three sets of ten |
| `planner_fuzz` seeds (`crates/devtools/tests/planner_fuzz_seeds/`) | planned as the fuzz tester plans them |

No ASAPWorkloads workload files exist in this repository: its query
catalog patterns appear only as `planner_fuzz` generator patterns, so the
registry has no ASAPWorkloads source.

A workload added to a source is planned by every suite that reads the
registry. Planning time in a debug build, measured 2026-11-10: under 1 s for
most workloads, up to 7 s for the slowest outside the random mixed sets,
and about 7 s for each random mixed workload, so the contracts run the
random mixed sets in the full tier, planning workloads in parallel on every
core (11 s for the 30 on 56 cores; 221 s one at a time).

## Naming

A test's name states the behavior it checks, read as a sentence:
`a_time_range_over_a_shifted_chain_merges_every_selected_window`, not
`q21_cost` or `test_issue_1237`. Its doc comment gives the input and the
expected outcome with numbers in one or two lines. Helpers name what they
return and its unit (`plan_cost_per_second`, `retained_tumbling_windows`).

## Ignored tests

- Every `#[ignore]` carries a reason that starts with the issue or PR that
  will remove it: `#[ignore = "#1242: the executor has no exact top-k"]`.
  A bare `#[ignore]` is not used.
- A test that finds a bug is not bent around it: it stays as it should be,
  ignored with the bug's issue, and the fix removes the ignore.
- Every test whose result depends on a wall-clock budget or deadline is
  ignored (#1067; done in #1180). A time is reported, never asserted.
- A slow test is ignored with its tier (below), not deleted.
- Ignores that predate this policy (the list under the coverage matrix)
  are relabelled when each test is classified under #1253.

## Tiers

Tiers are markers only; which tier CI runs when is configured separately.

| Tier | Marker | Holds | Budget |
|---|---|---|---|
| fast | none: runs under `cargo test` | rule unit tests, contracts over the fast registry workloads, hand-computed costs, execution oracles on small synthetic data (seconds), regressions | minutes for the workspace in a debug build |
| full | `#[ignore = "full tier: <why>"]` in a module named `full_tier` | contracts over the slow registry workloads, execution oracles over larger data | run with `cargo test -- --ignored full_tier` |
| nightly | `#[ignore = "nightly tier: <why>"]` in a module named `nightly_tier` | fuzz runs, exhaustive search, planning-time trend (recorded, not asserted) | run with `cargo test --release -- --ignored nightly_tier` |
| snapshots | the existing ignore reasons of `corpus_plans_match_snapshots` and `sketch_plans_match_snapshots` | the corpus and sketch plan snapshots, in a release build, never two at once | the commands of [planner snapshots](planner-snapshots.md), e.g. `cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored` |

The module name lets `cargo test` select a tier by test path; the reason
says why the test is not fast. Both markers are kept, and a meta-test
checks that every ignore reason starting `full tier:` or `nightly tier:`
sits in the module of the same name, so the two cannot drift apart
(`crates/integration-tests/tests/tier_markers.rs`).

Execution oracles on small synthetic data run in the fast tier, a change
from #1253, which puts the execution oracle in nightly: they take seconds.
Larger data sets go to the full tier.

## Stack hygiene

A PR in a stack carries no newly failing test compared with its base. A test
that the PR's design change breaks is either fixed in that PR or marked
`#[ignore = "<issue or PR that fixes it>: <reason>"]`, and the PR body's
Verification table lists passed / failed / ignored and the newly failing
count for every test scope it ran.

A PR that regenerates snapshots (one command, in [planner
snapshots](planner-snapshots.md)) lists each changed workload with its old →
new `cost_per_second` and why it changed.

## Coverage matrix

Measured on 2026-11-10 by opening each named test and checking that it
exercises the cell's combination; paths are relative to `crates/`. A cell
names one test, or "gap". Update a cell when a test fills it.

The matrix is factored on purpose: each axis is crossed with the language
in its own table, not the full language × operator × time shape × accuracy
× data facts × capabilities product #1253 describes, whose cells would be
mostly empty. The one cross product that decides plan shape, operator ×
time shape × accuracy, has its own table below.

Tokens:
- `L`: frontend lowering only. No memo and no Stage 3.
- `S3`: planned through Stage 3. The test calls `e2e_plan`, `plan_installable`, `run_planning`, `run_planning_stages_over`, `run_search_over`, the integration helpers `run_all_stages`, `selected_dag` or `logical_candidates*`, or the `asap_planner --run-until stage3` CLI.
- `X`: the plan Stage 3 selected is executed by `asap_executor`, and its answers are compared with expected values.
- `Xd`: executed on a direct DAG, without the planner's search: either the query text lowered and run with `test_support::physical_asap_dag_at_query_time`, or operators or a DAG built by hand (no query text). The cell says which. Answers are compared.
- `[ign]`: the test is marked `#[ignore]`, so it does not run in default CI.
- `n/a`: the language has no such construct.

### Operator × language

| operator | PromQL | SQL | MetricSQL |
|---|---|---|---|
| sum | S3+X `planner/tests/runtime_install.rs::an_instant_selector_reads_the_latest_sample_per_series` | S3+X `integration-tests/tests/filtered_aggregates.rs::exact_filtered_sum_is_null_for_groups_without_matches` | L `frontend-metricsql/tests/lowering.rs::selector_range_aggregate_and_call_share_the_canonical_shape`; exec gap |
| avg | S3 `integration-tests/tests/joint_search_equivalence.rs::composed_quantile_and_sum_outputs_are_validated_together`; Xd (lowered) `integration-tests/tests/per_series_exact_execution.rs::exact_per_series_range_functions_execute` (avg_over_time) | S3+X `integration-tests/tests/stage1_logical_asap_optimization.rs::sql_avg_from_sum_and_count_executes_as_avg` | gap (only a test that rejects `avg(foo, bar)`) |
| count | S3 `planner/tests/e2e_plan.rs::each_plan_carries_its_selected_root` (count_over_time); Xd (lowered) `integration-tests/tests/per_series_exact_execution.rs::exact_per_series_range_functions_execute` | S3+X `integration-tests/tests/filtered_aggregates.rs::exact_filtered_count_keeps_groups_without_matches` | L `frontend-metricsql/tests/lowering_semantics.rs::count_counts_series`; exec gap |
| min / max | S3 `devtools/tests/asap_planner.rs::a_counter_function_over_a_declared_gauge_plans_with_a_warning` (max_over_time), `devtools/tests/planner_fuzz.rs::threshold_witness_costs_over_the_result` (min by); Xd (lowered) `integration-tests/tests/per_series_exact_execution.rs::exact_per_series_range_functions_execute`; S3+X `integration-tests/tests/execution_oracle.rs::an_exact_max_over_chain_windows_equals_the_window_maximum` (max_over_time over a chain) | S3 `integration-tests/tests/independent_plan_seed.rs::percentile_and_max_over_one_scan_cost_no_more_than_separate_plans`; Xd (hand-built) `executor/tests/physical_semantics.rs::global_extrema_bind_with_planner_derived_schema` | gap |
| quantile | S3 `planner/tests/runtime_install.rs::the_plan_carries_stage3s_predicted_usage`; Xd (lowered) `integration-tests/tests/per_series_exact_execution.rs::quantile_bounds_follow_promql` (exact only; `planner/tests/columnar_batches.rs` runs a KLL build but checks no answer); S3+X `integration-tests/tests/execution_oracle.rs::an_approximate_quantile_is_within_its_rank_error_for_all_but_delta_of_the_series` (sketch chain, rank ε; `::an_approximate_quantile_is_within_its_relative_error_for_all_but_delta_of_the_series`, relative ε) | S3+X `integration-tests/tests/filtered_aggregates.rs::kll_quantile_of_all_null_group_is_null` (KLL plan; answers on trivial groups only, no ε check) | L `frontend-metricsql/tests/lowering.rs::supported_parameterized_functions_require_their_exact_arity`; exec gap |
| topk / bottomk | topk: S3 `devtools/tests/asap_planner.rs::example1_document_validates_its_stage_dags`; Xd (lowered) `executor/tests/current_series_heap.rs::population_fixture_compiles_with_dynamic_identity`. Heap-sketch answers are checked only in `integration-tests/tests/promql_to_post_asap.rs::planner_heap_topk_reference_execution_matches_ground_truth` [ign], by an evaluator inside the test, not the executor. bottomk: L only `devtools/tests/cross_language.rs::ascending_count_ranked_topk_stays_generic_in_both_languages` | ORDER BY value … LIMIT: S3 `integration-tests/tests/sql_to_post_asap.rs::sql_relational_parents_retain_summary_bound_aggregate`. Top-k by count (heavy hitter) stops at Pass 1: `integration-tests/tests/per_computation_candidates_sql.rs::topk_heap_over_an_int64_key_counts_raw_rows`. Xd (hand-built) `executor/tests/physical_semantics.rs::limit_offset_fetch_matrix` | gap |
| distinct count | L `frontend-promql/tests/promql_lowering.rs::distinct_over_time_lowers_to_cardinality_over_nested_windows`; S3 gap; exec gap | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_tumbling_windows_built_from_table_rows_answer_exactly` | gap |
| rate / increase | S3 `devtools/tests/asap_planner.rs::example1_document_validates_its_stage_dags` (rate), `integration-tests/tests/promql_to_post_asap.rs::value_ranked_topk_over_binary_ratio_finalizes_both_summary_operands` (increase); Xd (hand-built) `executor/tests/promql_values.rs::recovered_exact_counter_uses_window_and_omits_insufficient_samples` | n/a | L `frontend-metricsql/tests/lowering.rs::selector_range_aggregate_and_call_share_the_canonical_shape`; exec gap |
| binary op | S3 `integration-tests/tests/promql_to_post_asap.rs::promql_binary_arithmetic_preserves_nested_structure_and_vector_matching`; Xd (hand-built) `executor/tests/promql_binary.rs::compiled_binary_matches_series_and_preserves_checked_division` | S3 `devtools/tests/asap_planner.rs::example2_plans_the_sql_workload` (entropy and L2 arithmetic); Xd (lowered) `integration-tests/tests/sql_frequency.rs::recognized_sql_l2_executes_exactly` | gap (`sum(up) + sum(down)` appears only as an error case) |
| offset | S3 `integration-tests/tests/milp_objective.rs::the_milp_objective_is_the_plan_cost_on_an_offset_range_query`; Xd (hand-built) `executor/tests/time_series_window.rs::a_shifted_range_window_ends_its_offset_before_the_evaluation`; S3+X `integration-tests/tests/execution_oracle.rs::an_exact_sum_over_shifted_chain_windows_equals_the_shifted_window_sum` | n/a (shifted `ts` window predicate: gap) | gap |
| `@` modifier | L `integration-tests/tests/nested.rs::q40_at_modifier_absolute`. Stage 2 unit test on a hand-built memo: `stage2-physical-optimization/src/catalog/tests.rs::an_at_t_subtree_is_offered_ingestion_time`. S3 gap. Xd (hand-built) `executor/tests/tumbling_window_chain.rs::a_time_shift_at_end_evaluates_its_input_at_the_requests_end` | n/a | gap |
| subquery | S3 only [ign]: `devtools/tests/corpus_snapshots/o11y_bench/q05.json` (and q06, q11) via `devtools/tests/corpus_snapshots.rs::corpus_plans_match_snapshots`. L `integration-tests/tests/nested.rs::q27_nested_subquery_prometheus_docs_example`. Xd (hand-built; memory limit only) `executor/tests/series_window_memory_limit.rs::a_long_subquery_over_many_series_fails_near_the_memory_limit` | derived table: S3 `integration-tests/tests/sql_to_post_asap.rs::sql_relational_parents_retain_summary_bound_aggregate`; Xd (lowered) `integration-tests/tests/sql_frequency.rs::recognized_sql_entropy_executes_in_nats` | gap |

### Operator × time shape × accuracy

"Range" is a workload entry with `range_evaluation`; "repeating" is a
fixed-interval repeating entry without it. Tests built on the integration
helpers `selected_dag` / `run_all_stages` leave the recurrence unknown, so
they fill no cell here. An ε target is enforced unless the cell says
ignored.

PromQL:

| operator | range, exact | range, ε | repeating, exact | repeating, ε |
|---|---|---|---|---|
| sum | gap | S3 `integration-tests/tests/milp_objective.rs::the_milp_objective_is_the_plan_cost_on_an_offset_range_query` | S3+X `integration-tests/tests/execution_oracle.rs::an_exact_sum_over_chain_windows_equals_the_sum_of_the_window` | S3+X `planner/tests/runtime_install.rs::an_instant_selector_reads_the_latest_sample_per_series` (rank 0.01) |
| avg | gap | gap | S3 [ign] `integration-tests/tests/sharing_issue_regressions.rs::exact_coarse_sum_reuses_fine_sum` (avg_over_time) | S3 `integration-tests/tests/joint_search_equivalence.rs::composed_quantile_and_sum_outputs_are_validated_together` |
| count | gap | gap | S3 `devtools/tests/planner_fuzz.rs::exact_rollup_witness_costs_for_a_grouped_count` | S3 `planner/tests/e2e_plan.rs::each_plan_carries_its_selected_root` (count_over_time) |
| min / max | gap | S3 [ign] `devtools/tests/corpus_snapshots.rs::corpus_plans_match_snapshots` (q11, max_over_time) | S3+X `integration-tests/tests/execution_oracle.rs::an_exact_max_over_chain_windows_equals_the_window_maximum` | gap |
| quantile | gap | S3 [ign] `devtools/tests/corpus_snapshots.rs::corpus_plans_match_snapshots` (q02, `histogram_quantile` only) | gap | S3+X `integration-tests/tests/execution_oracle.rs::an_approximate_quantile_is_within_its_rank_error_for_all_but_delta_of_the_series` |
| topk | gap | gap | S3 `integration-tests/tests/sharing_issue_regressions.rs::duplicate_exact_topk_shares_work` | S3 `integration-tests/tests/joint_search_equivalence.rs::fused_top_k_preserves_its_independently_requested_inner_sum` |
| distinct count | gap | gap | gap | gap |
| rate / increase | gap | S3 `integration-tests/tests/milp_objective.rs::the_milp_objective_is_the_plan_cost_on_an_offset_range_query` | S3 `devtools/tests/asap_planner_acceptance.rs::stage3_selects_candidates_stage2_lists` | S3 `planner/tests/summary_sharing.rs::identical_ungrouped_queries_share_their_producers` |
| binary op | gap | S3 [ign] `devtools/tests/corpus_snapshots.rs::corpus_plans_match_snapshots` (q19, increase ratio) | S3 `integration-tests/tests/production_rollup_workloads.rs::production_binary_queries_use_joint_prices_with_mixed_schedules` | S3 `integration-tests/tests/accuracy_at_query_roots.rs::each_query_meets_its_target_at_its_root` (quantile ratio) |
| offset | L `frontend-promql/src/lib.rs::tests::at_start_and_at_end_are_lifted_above_the_selector_they_anchor` | S3 `integration-tests/tests/milp_objective.rs::the_milp_objective_is_the_plan_cost_on_an_offset_range_query` | S3+X `integration-tests/tests/execution_oracle.rs::an_exact_sum_over_shifted_chain_windows_equals_the_shifted_window_sum` | gap |

SQL has no range queries (no test sets `range_evaluation`, and the SQL
frontend does not read it):

| operator | repeating, exact | repeating, ε |
|---|---|---|
| sum | S3 `integration-tests/tests/sharing_issue_regressions.rs::sql_sum_shares_nested_windows_and_slower_readers` | gap |
| avg | gap | gap |
| count | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_row_counts_read_each_table_row_once` | gap |
| min / max | gap | gap |
| quantile | gap | S3 `integration-tests/tests/production_rollup_workloads.rs::production_three_sql_groupings_share_kll` |
| topk (ORDER BY … LIMIT) | gap | gap |
| distinct count | gap | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_tumbling_windows_built_from_table_rows_answer_exactly` |

No range query has an exact target or is executed (the q21 range query's
cost is hand-computed: `integration-tests/tests/cost_oracle.rs::a_time_range_over_a_shifted_chain_merges_every_selected_window`).

### Time shape

| shape | PromQL | SQL |
|---|---|---|
| one-off (`query_batch`) | S3 `devtools/tests/asap_planner.rs::the_search_reports_candidate_connections` | S3 `planner/tests/e2e_plan.rs::plans_every_query_in_entry_order` |
| range query (`range_evaluation`) | S3 `integration-tests/tests/milp_objective.rs::the_milp_objective_is_the_plan_cost_on_an_offset_range_query`; all 14 `corpus_snapshots/o11y_bench` queries [ign]; never executed | gap |
| repeating, fixed interval | S3+X `planner/tests/runtime_install.rs::an_instant_selector_reads_the_latest_sample_per_series` | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_row_counts_read_each_table_row_once` |
| repeating, two intervals on one input | S3 `planner/tests/runtime_install.rs::one_chain_serves_two_evaluation_contexts` | S3 `integration-tests/tests/sharing_issue_regressions.rs::sql_sum_shares_nested_windows_and_slower_readers` |
| repeating, estimated rate or unknown recurrence | S3 `integration-tests/tests/production_rollup_workloads.rs::production_binary_queries_use_joint_prices_with_mixed_schedules` | gap |

### Accuracy

| accuracy | PromQL | SQL |
|---|---|---|
| exact (`max_error` 0) | S3 `devtools/tests/asap_planner.rs::the_search_reports_candidate_connections` | S3+X `integration-tests/tests/filtered_aggregates.rs::exact_filtered_count_keeps_groups_without_matches` |
| ε target, guarantees enforced | S3 `planner/tests/runtime_install.rs::the_plan_carries_stage3s_predicted_usage` (rank 0.01) | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_tumbling_windows_built_from_table_rows_answer_exactly` (relative 0.05) |
| ε target, guarantees ignored | S3 `planner/tests/runtime_install.rs::ignoring_accuracy_guarantees_plans_the_undeclared_p99_query` | S3+X `integration-tests/tests/filtered_aggregates.rs::kll_quantile_of_all_null_group_is_null` (answers on trivial groups only, no ε check) |
| exact and ε in one workload | S3 `devtools/tests/asap_planner.rs::example1_document_validates_its_stage_dags` | not found |
| rank and relative metrics | S3 `integration-tests/tests/accuracy_at_query_roots.rs::each_query_meets_its_target_at_its_root` | S3 `integration-tests/tests/production_rollup_workloads.rs::production_three_sql_groupings_share_kll` (rank), `::production_sql_distinct_region_service_rollup` (relative) |

### Data facts

| fact | PromQL | SQL |
|---|---|---|
| `series_count` known | S3+X `planner/tests/runtime_install.rs::an_instant_selector_reads_the_latest_sample_per_series` | S3 `devtools/tests/asap_planner.rs::sql_queries_over_declared_tables_are_planned` |
| `series_count` absent | S3 `planner/tests/e2e_plan.rs::each_plan_carries_its_selected_root` (no sources declared) | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_row_counts_read_each_table_row_once` (`rows_held` only) |
| ingesting, `max_out_of_order_ms` unset (in order) | S3+X `planner/tests/runtime_install.rs::an_instant_selector_reads_the_latest_sample_per_series` | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_row_counts_read_each_table_row_once` |
| arrival at rest or unknown | S3 `devtools/tests/asap_planner.rs::the_search_reports_candidate_connections` (unknown) | S3 `planner/tests/e2e_plan.rs::plans_every_query_in_entry_order` (at rest) |
| arrival mixed | S3 `devtools/tests/asap_planner.rs::example3a_selects_a_valid_plan` | gap |
| out-of-order bound declared (`max_out_of_order_ms` set) | gap. Only a Stage 2 unit test on a hand-built memo, with no language: `stage2-physical-optimization/src/catalog/tests.rs::a_counter_state_runs_at_ingestion_time_over_ordered_rows` | gap (same unit test) |
| out-of-order samples executed | gap | gap |

### Deployment capabilities

| capability | PromQL | SQL |
|---|---|---|
| `supported_summaries` default | S3+X `planner/tests/runtime_install.rs::an_instant_selector_reads_the_latest_sample_per_series` | S3 `planner/tests/e2e_plan.rs::plans_every_query_in_entry_order` |
| `supported_summaries` restricted | S3 `devtools/tests/asap_planner.rs::deployment_capabilities_flag_sets_the_capabilities_and_shows_them` (KLL only); also `integration-tests/tests/promql_to_post_asap.rs::planner_heap_topk_reference_execution_matches_ground_truth` [ign] | gap. Only a Pass 1 unit test with no language: `stage1-pass1-per-computation-candidates/src/per_computation_candidates_tests.rs::unsupported_summaries_are_not_offered` |
| `raw_data_retained: false` | S3 `planner/tests/runtime_install.rs::the_plan_carries_stage3s_predicted_usage` | S3+X `planner/tests/sql_table_scans.rs::ingestion_time_row_counts_read_each_table_row_once` |
| `query_time_results_retained: true` | S3 `devtools/tests/asap_planner.rs::deployment_capabilities_flag_sets_the_capabilities_and_shows_them` | gap |

### Test counts per crate

Commands, run from the worktree root:
```
for c in crates/*/; do
  grep -rE '^\s*#\[(tokio::)?test(\(.*\))?\]' $c --include=*.rs | wc -l   # tests
  grep -rE '#\[ignore' $c --include=*.rs | wc -l                          # ignores
done
```

| crate | tests | `#[ignore` |
|---|---|---|
| accuracy | 24 | 0 |
| cost | 39 | 0 |
| devtools | 92 | 3 |
| executor | 281 | 1 |
| frontend-common | 49 | 0 |
| frontend-metricsql | 15 | 0 |
| frontend-promql | 218 | 0 |
| frontend-sql | 175 | 0 |
| integration-tests | 156 | 6 |
| metricsql-common-parser-support | 3 | 0 |
| metricsql-parser-vendored | 144 (plus `#[test_case]` cases in `src/parser/tokens.rs`, not counted) | 0 |
| planner | 31 | 1 |
| sql-function-catalog | 10 | 0 |
| stage0-memo | 15 | 0 |
| stage1-pass1-per-computation-candidates | 32 | 0 |
| stage1-pass2-reuse-candidates | 26 | 0 |
| stage2-physical-optimization | 13 | 0 |
| stage3-plan-selection | 15 | 0 |
| types | 200 | 0 |
| **total** | **1538** | **11** |

The 11 ignored tests:
- devtools:
  - `corpus_plans_match_snapshots`
  - `sketch_plans_match_snapshots`
  - `milp_search_selects_a_checked_ddsketch_plan_for_a_ratio_and_its_operands`
- integration-tests:
  - `finer_aggregate_reuse::filtered_kll_states_cost_no_more_than_separate_plans`
  - `operator_sharing::exact_aggregate_and_sketch_share_one_scan`
  - `operator_sharing::each_side_of_union_all_holds_a_summary_estimate`
  - `promql_to_post_asap::planner_heap_topk_reference_execution_matches_ground_truth`
  - `sharing_issue_regressions::exact_coarse_sum_reuses_fine_sum`
- planner: `summary_sharing::sql_p50_and_p99_share_one_producer`
- executor: one timing test in `src/summary_kernels/univmon.rs`

### Notable gaps

1. MetricSQL is never planned. No test sends a `metricsql` workload or `FrontendInput::Metricsql` to `e2e_plan` or `plan_installable`, even though the facade supports it. Only sum, count, quantile and rate are lowered.
2. Few planner-selected plans are executed with answers checked (`X`). PromQL has sum, sum_over_time, max_over_time, quantile_over_time (a sketch chain) and offset over a chain, at one evaluation time (`runtime_install`, `execution_oracle`). SQL has sum, count, avg, quantile (KLL) and distinct count. PromQL topk, rate, binary operations, `@` and subquery are never executed from a Stage 3 plan.
3. The only PromQL sketch plans whose answers the executor checks are the quantile sketch chains of `execution_oracle` (rank and relative ε). No CMS heap or top-k sketch plan is executed: the heap check is ignored and uses an evaluator written inside the test. `columnar_batches` runs a KLL build without checking an answer.
4. Range queries (`range_evaluation`) have two non-ignored tests, which check cost only (`milp_objective` and the q21 hand-computed cost in `cost_oracle`). There is no SQL range query, and no range query is ever executed.
5. Out-of-order data appears in a single Stage 2 unit test on a hand-built memo. No test in either language sets `max_out_of_order_ms` through the pipeline or executes late samples.
6. The `@` modifier is not planned past Stage 2 from query text. Subqueries reach Stage 3 only in the ignored corpus snapshots. Neither is executed except by operator-level tests (`Xd`, hand-built).
7. The corpus snapshots (14 `o11y_bench` queries: subquery, offset, histogram_quantile, range_evaluation) and the sketch plan snapshots (10 workloads) are `#[ignore]` and do not run by default.
8. Restricted `supported_summaries` is tested through Stage 3 by one PromQL CLI test (KLL only) and by none in SQL. `query_time_results_retained: true` is likewise tested in PromQL only.
9. Some operators stop early. PromQL `distinct_over_time` and `bottomk` are lowered but never planned. SQL top-k by count (heavy hitter) stops at Pass 1. MetricSQL avg, min/max, topk, offset, `@`, subquery and binary operations are not even lowered in any test.
10. SQL has no test for mixed arrival or for a recurrence that is an estimated rate or unknown. PromQL covers each with one planning-only test.
