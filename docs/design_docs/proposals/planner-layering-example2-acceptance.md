# Planner layering, Example 2: acceptance spec

Audience: planner designers and the implementers of the summary-capability
rule, UnivMon sizing and accuracy, and SQL `Entropy`/`L2` recognition.
Source: [planner-layering.md](planner-layering.md), "Example 2: One summary for
several computations". Tests:
`crates/integration-tests/tests/planner_layering_example2.rs` (helpers in
`tests/planner_layering_common/`).

This spec was written by a test designer who does not implement the stages.
Tests run the stages through `planner_layering_common::run_stages`: Pass 1
and composition, `optimize_physical_asap` and `select_plan`. A test that
needs a missing feature is `#[ignore]`d with the PR that delivers it.

## Workload

Three SQL dashboard panels over `flows`, every 10 s, `lookback` 1 m, `as_of`
evaluation time, no latency requirement.

| Query | Computation | SQL (abridged) | Accuracy |
|---|---|---|---|
| Q1 | `Distinct(src_ip)` | `SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '1 minute'` | ε = 0.02, δ = 0.01 |
| Q2 | `Entropy(src_ip)` | `SELECT -SUM(p * LN(p)) FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p … GROUP BY src_ip)` | ε = 0.05, δ = 0.01 |
| Q3 | `L2(src_ip)` | `SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (SELECT src_ip, COUNT(*) AS c … GROUP BY src_ip)` | ε = 0.01, δ = 0.01 |

Data workload: the shared arrival (`Ingesting`, about 66,667 samples/s) with
`sources["flows"].series_count` = 10,000,000 and no `sample_interval` (SQL
needs none). The catalog is `flows(ts, src_ip)`; the doc names no other
column. Q3 casts its counts to DOUBLE: the doc's Int64 `c * c` can overflow,
so the SQL frontend recognizes only the Float64 product.

**Stand-in.** The SQL frontend lowers Q1 to a distinct count and recognizes
Q2 and Q3 as `Entropy` and `L2`. It lowers the time filter to the same
1-min `TimeRange` a PromQL range selector lowers to. Most tests were
written before it did and run the planning stages on a PromQL stand-in with the same
structure, three statistics of one input over the same 1-min window:
`distinct_over_time(flows_src_ip[1m])`,
`entropy_over_time(flows_src_ip[1m])` and `l2_over_time(flows_src_ip[1m])`,
with Q1–Q3's targets and a 15-s `sample_interval` on `flows_src_ip`. The
`stage_pipeline --example planner-layering-2 --enforce-accuracy-guarantees` document plans the SQL
workload itself.

## Expected candidates per stage

| Stage | Doc | Planner (SQL workload, `stage_pipeline --example planner-layering-2 --enforce-accuracy-guarantees`) |
|---|---|---|
| Query-language frontends | One summary-free DAG: `Distinct`, `Entropy`, `L2` over `flows.src_ip`, last 1 m | Same. Q1 is a distinct count; Q2 is `Entropy` and Q3 is `L2` over the per-source counts, each with an exact population count for the empty-input NULL. |
| Logical ASAP-aware optimization, Pass 1 | Each computation: exact, a specialized summary, UnivMon. 3 × 3 × 3 = **27** | Q1: exact, HLL, KMV, UnivMon (no Theta: `asap_sketchlib` has no Theta sketch). Q2, Q3: exact, UnivMon (no specialized entropy or norm summary). 4 × 2 × 2 = **16** |
| Logical ASAP-aware optimization, Pass 2 | Summary-capability rule: + 1 (one UnivMon for all three, sized for ε = 0.01) + 9 (3 pairs share a UnivMon × 3 options of the third). Independent kept. **37**. Window composition left out. | Identical-expression rule (Q2 and Q3's shared per-source count, and the shared scan) and summary-capability rule (one UnivMon for two of the queries or all three, sized with each parameter's largest value among its readers): **82** |
| Physical ASAP-aware optimization | Same options as Example 1: no window form → rebuilt from raw samples each refresh. | Each logical candidate with every sub-DAG `NotMaterialized`, plus `MatAtQuery(Memory)` for each summary build two or more queries read: **184** |
| Plan selection | One UnivMon sized for ε = 0.01 vs three separate summaries; the shared one usually wins (one update per flow record instead of three). | 164 invalid: HLL or UnivMon for Q1 (120), whose bound has failure probability None without a distinct bound for `flows`, and UnivMon for Q2 (44), whose entropy has no guarantee. 20 valid. Under cost model 1, KMV for Q1 with the other two exact (P33) costs 6.097 per second and P1, all exact, 6.104, the exact distinct count's values held while Q1 runs; the shared per-source count costs 7.46 rebuilt for each query and 7.06 kept `MatAtQuery`; with the shared scan also built once per evaluation, KMV for Q1 (P35-m3) is selected at 5.745, and all exact costs 5.752; UnivMon for Q3 costs 8.09. |

## Invariants and tests

### Query-language frontends

| Invariant | Test | Status |
|---|---|---|
| The SQL workload is valid, in entry order | `workload_encodes_example2` | passes |
| SQL Q1 lowers to a distinct count over `flows` | `stage0_query_language_frontends_sql_q1_lowers_to_a_distinct_count` | passes |
| SQL Q2 and Q3 lower to `FrequencyEntropy` and `FrequencyL2` | `stage0_query_language_frontends_sql_q2_q3_lower_to_entropy_and_l2` | passes |
| The stand-in lowers to the three statistics over one 1-min range | `stage0_query_language_frontends_standin_lowers_to_three_statistics_of_one_input` | passes |
| One summary-free DAG; the queries share no node | `stage0_query_language_frontends_one_summary_free_dag_without_sharing` | passes |

### Logical ASAP-aware optimization

| Invariant | Test | Status |
|---|---|---|
| Once recognized, the SQL workload has the stand-in's Pass 1 options | `stage1_logical_asap_optimization_sql_has_the_standin_options` | passes |
| Each statistic has an exact and a UnivMon option | `stage1_logical_asap_optimization_pass1_offers_exact_and_univmon_for_each_statistic` | passes |
| Q1 has a specialized distinct-count summary (HLL or KMV) | `stage1_logical_asap_optimization_pass1_offers_a_distinct_count_summary` | passes |
| Q2 and Q3 have a specialized entropy and norm summary | `stage1_logical_asap_optimization_pass1_offers_specialized_entropy_and_l2_summaries` | ignored: needs Pass 1 specialized entropy and L2 summary families (#444) |
| Every combination of the queries' own options is kept, with no shared summary | `stage1_logical_asap_optimization_keeps_every_independent_combination` | passes |
| A candidate has one UnivMon build read by all three queries, feeding a distinct-count, an entropy and an L2 estimate | `stage1_logical_asap_optimization_summary_capability_adds_one_univmon_for_all_three` | passes |
| For each pair, a candidate shares one UnivMon between the two while the third takes each of its own options | `stage1_logical_asap_optimization_summary_capability_adds_pairwise_shared_univmons` | passes |
| A shared UnivMon takes each parameter's largest value among its consumers, each sized alone | `stage1_logical_asap_optimization_shared_univmon_takes_each_parameters_largest_own_size` | passes |
| UnivMon is sized per target: Q3's (ε = 0.01) is larger than Q2's (ε = 0.05) | `stage1_logical_asap_optimization_univmon_is_sized_per_accuracy_target` | passes |
| Only a UnivMon is ever shared across statistics | `stage1_logical_asap_optimization_only_univmon_is_shared_across_statistics` | passes |
| Stage 1 does not read the distinct bound; Q1's HLL failure probability stays symbolic in the source's bound | `stage1_logical_asap_optimization_leaves_the_hll_failure_probability_symbolic` | passes |
| Candidates are valid DAGs with unique ids | `stage1_logical_asap_optimization_candidates_are_valid_and_uniquely_named` | passes |

### Physical ASAP-aware optimization

| Invariant | Test | Status |
|---|---|---|
| Each logical candidate keeps one physical candidate with nothing materialized, plus one per other materialization assignment; none pruned | `stage2_physical_asap_optimization_keeps_every_logical_candidate` | passes |
| The shared UnivMon stays one build read by all three queries | `stage2_physical_asap_optimization_keeps_the_shared_univmon_as_one_build` | passes |

Window-composition candidates are out of scope: the doc leaves them out for
this example, and a summary with no window form is rebuilt at every refresh
(Example 1's table) unless a shared build is kept `MatAtQuery` within one
evaluation.

### Plan selection

| Invariant | Test | Status |
|---|---|---|
| One selected id; every other candidate rejected once with a reason | `stage3_plan_selection_selects_one_and_explains_the_rest` | passes |
| The selected plan is the cheapest valid one | `stage3_plan_selection_selects_cheapest_valid` | passes |
| Each node of a valid candidate is charged once per consuming query while `NotMaterialized`, once when materialized | `stage3_plan_selection_charges_each_node_per_consuming_query` | passes |
| No candidate builds a Theta sketch | `stage3_plan_selection_has_no_theta_candidate` | passes |
| A UnivMon estimate has a guarantee, so it is judged against its target instead of rejected for having none | `stage3_plan_selection_judges_univmon_with_an_accuracy_model` | ignored: needs a UnivMon entropy accuracy model (#788); L2 and the distinct count have one |
| Given a distinct bound for the source, classic HLL's failure probability is bound and Q1's HLL is accepted; a larger bound enlarges its precision, also in a tumbling-pane chain | `stage3_plan_selection_accepts_hll_given_a_distinct_bound`, `…_enlarges_hll_for_a_larger_distinct_bound`, `…_enlarges_tumbling_pane_hll` | passes |
| KMV certifies Q1 for any failure probability, without a distinct bound | `stage3_plan_selection_accepts_kmv_for_any_failure_probability_without_a_bound` | passes |
| UnivMon's distinct count is certified given a distinct bound its layers cover; its L2 is certified from layer 0 alone, also through SQL Q3's exact population count and `CASE` | `stage3_plan_selection_accepts_univmon_distinct_given_a_distinct_bound`, `…_rejects_univmon_distinct_past_its_layers`, `…_accepts_univmon_l2` | passes |
| One materialized UnivMon for all three costs no more than three separate UnivMons | `stage3_plan_selection_shared_univmon_costs_no_more_than_three` | ignored: needs a UnivMon entropy accuracy model (#788) |
| Selection direction: the selected plan has at most one UnivMon, because one shared UnivMon dominates several over the same input | `stage3_plan_selection_selected_plan_has_at_most_one_univmon` | passes |
| The selected KMV plan's Q1 runs in the executor | `stage3_plan_selection_selected_kmv_plan_executes_end_to_end` | passes |

The doc's "the shared candidate usually wins" depends on the cost model, so
it is tested only as the dominance above: one build sized for the strictest
consumer costs no more than three builds that include one of that size. Whether
it also beats the exact plans is the model's call. The dominance needs the
shared UnivMon materialized: plan selection charges a `NotMaterialized`
node once per consuming query, so unmaterialized it costs three builds of the
strictest size.

## Ambiguities and conflicts with the planner

1. **SQL recognition.** The SQL frontend recognizes Q2 and Q3 only in the
   forms above (Q3 with a Float64 product). The stand-in's statistics are per
   series (`PerEntity`), the doc's over the whole column; with one
   `flows_src_ip` series they coincide.
2. **Pass 1 options.** The doc gives each computation three options. The
   planner gives Q1 four (exact and three distinct-count summaries) and Q2,
   Q3 two (no specialized entropy or norm summary). The tests check presence,
   not counts.
3. **Candidate count.** The doc's 37 excludes the identical-expression rule's
   shared-input candidates (Example 1 counts them). The planner applies both
   rules and gives 82. Tests do not pin a count.
4. **Independent UnivMons on a shared input.** The doc keeps independent
   candidates. Once Pass 2 shares the input, identical UnivMon builds may
   merge; the tests require the independent combinations only without a
   shared summary.
5. **Sizing.** "Sized for the strictest requirement, ε = 0.01" becomes: each
   consumer is sized alone, and the shared build takes each parameter's
   largest value among them.
6. **Accuracy.** Plan selection accepts an output whose guarantee meets the
   target or for which the deployment has evidence. UnivMon's L2 and
   distinct count are certified (the distinct count only given a distinct
   bound for the source); its entropy is not, so every UnivMon candidate for
   Q2 is invalid. HLL's and UnivMon's distinct-count failure probability
   stays unknown without the deployment's bound on distinct items per
   evaluation (`sources["flows"].max_distinct_per_evaluation`), which this
   example does not declare; KMV needs none. The doc treats a distinct
   summary as valid.
7. **Pairwise count.** "The third keeps any of its own 3 options" includes its
   own UnivMon, sized for itself. The test requires the third's full Pass 1
   option set.
8. **Data workload.** Only the `flows` source's facts change (its
   `series_count`, and no `sample_interval`), so the ingestion rate stays
   about 66,667/s with 10,000,000 keys.

## Devtool

`stage_pipeline --example planner-layering-2 --enforce-accuracy-guarantees`
plans the SQL workload with the `flows(ts, src_ip)` catalog (without the
flag, `stage_pipeline` ignores accuracy guarantees, and none of the plans
below is rejected for its guarantee). The stage viewer has no tab for it; open the
document with **Open stage document…**.
