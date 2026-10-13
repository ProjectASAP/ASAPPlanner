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
| Q3 | `L2(src_ip)` | `SELECT SQRT(SUM(c * c)) FROM (SELECT src_ip, COUNT(*) AS c … GROUP BY src_ip)` | ε = 0.01, δ = 0.01 |

Data workload: the shared one (`continuously_ingesting`, about 66,667
samples/s, `zipf`, volume unknown) with `input_cardinality` = 10,000,000 and no
`data_ingestion_interval`. The catalog is `flows(ts, src_ip)`; the doc names
no other column.

**Stand-in.** The SQL frontend lowers Q1 to a distinct count but Q2 and Q3 to
exact relational plans (the doc's own TODO; #598). The planning stages
therefore run on a PromQL stand-in with the same structure, three statistics
of one input over the same 1-min window: `distinct_over_time(flows_src_ip[1m])`,
`entropy_over_time(flows_src_ip[1m])` and `l2_over_time(flows_src_ip[1m])`,
with Q1–Q3's targets and a 15-s ingestion interval (PromQL needs one).

## Expected candidates per stage

| Stage | Doc | Today (stand-in) |
|---|---|---|
| Language-specific frontends | One summary-free DAG: `Distinct`, `Entropy`, `L2` over `flows.src_ip`, last 1 m | Same for the stand-in. SQL: Q1 is `aggregate:cardinality`; Q2 and Q3 are `count` per `src_ip` → window function / projection → `sum` (not recognized). |
| Logical ASAP-aware optimization, Pass 1 | Each computation: exact, a specialized summary, UnivMon. 3 × 3 × 3 = **27** | Q1: exact, HLL, Theta, KMV, UnivMon. Q2, Q3: exact, UnivMon (no specialized entropy or norm summary). 5 × 2 × 2 = **20** |
| Logical ASAP-aware optimization, Pass 2 | Summary-capability rule: + 1 (one UnivMon for all three, sized for ε = 0.01) + 9 (3 pairs share a UnivMon × 3 options of the third). Independent kept. **37**. Window composition left out. | Not implemented: **20**, no candidate shares a node. |
| Physical ASAP-aware optimization | Same options as Example 1: no window form → rebuilt from raw samples each refresh. | One physical candidate per logical candidate, every sub-DAG `NotMaterialized`: **20**. |
| Plan selection | One UnivMon sized for ε = 0.01 vs three separate summaries; the shared one usually wins (one update per flow record instead of three). | HLL is invalid (its bound has no failure probability for the (ε, δ) target) and so is every UnivMon (its estimates have no guarantee). Q1 on Theta or KMV with Q2, Q3 exact is valid but costlier. The all-exact plan is selected. |

## Invariants and tests

### Language-specific frontends

| Invariant | Test | Status |
|---|---|---|
| The SQL workload is valid, in entry order | `workload_encodes_example2` | passes |
| SQL Q1 lowers to a distinct count over `flows` | `language_specific_frontends_sql_q1_lowers_to_a_distinct_count` | passes |
| SQL Q2 and Q3 lower to `FrequencyEntropy` and `FrequencyL2` | `language_specific_frontends_sql_q2_q3_lower_to_entropy_and_l2` | ignored: #598 |
| The stand-in lowers to the three statistics over one 1-min range | `language_specific_frontends_standin_lowers_to_three_statistics_of_one_input` | passes |
| One summary-free DAG; the queries share no node | `language_specific_frontends_one_summary_free_dag_without_sharing` | passes |

### Logical ASAP-aware optimization

| Invariant | Test | Status |
|---|---|---|
| Once recognized, the SQL workload has the stand-in's Pass 1 options | `logical_asap_optimization_sql_has_the_standin_options` | ignored: #598, and #603 (ambiguity 9) |
| Each statistic has an exact and a UnivMon option | `logical_asap_optimization_pass1_offers_exact_and_univmon_for_each_statistic` | passes |
| Q1 has a specialized distinct-count summary (HLL, Theta or KMV) | `logical_asap_optimization_pass1_offers_a_distinct_count_summary` | passes |
| Q2 and Q3 have a specialized entropy and norm summary | `logical_asap_optimization_pass1_offers_specialized_entropy_and_l2_summaries` | ignored: no PR yet |
| Every combination of the queries' own options is kept, with no shared summary | `logical_asap_optimization_keeps_every_independent_combination` | passes |
| A candidate has one UnivMon build read by all three queries, feeding a distinct-count, an entropy and an L2 estimate | `logical_asap_optimization_summary_capability_adds_one_univmon_for_all_three` | ignored: #595, #596 |
| For each pair, a candidate shares one UnivMon between the two while the third takes each of its own options | `logical_asap_optimization_summary_capability_adds_pairwise_shared_univmons` | ignored: #595, #596 |
| A shared UnivMon has the parameters Pass 1 gives its strictest consumer alone | `logical_asap_optimization_shared_univmon_is_sized_for_the_strictest_consumer` | ignored: #595, #596 |
| UnivMon is sized per target: Q3's (ε = 0.01) is larger than Q2's (ε = 0.05) | `logical_asap_optimization_univmon_is_sized_per_accuracy_target` | ignored: #621 |
| Only a UnivMon is ever shared across statistics | `logical_asap_optimization_only_univmon_is_shared_across_statistics` | passes (vacuously today) |
| Candidates are valid DAGs with unique ids | `logical_asap_optimization_candidates_are_valid_and_uniquely_named` | passes |

### Physical ASAP-aware optimization

| Invariant | Test | Status |
|---|---|---|
| One physical candidate per logical candidate, none pruned | `physical_asap_optimization_keeps_every_logical_candidate` | passes |
| The shared UnivMon stays one build read by all three queries | `physical_asap_optimization_keeps_the_shared_univmon_as_one_build` | ignored: #595, #596 |

Materialization and window variants are out of scope: the doc leaves out the
window-composition variants for this example, and a summary with no window
form is rebuilt at every refresh (Example 1's table).

### Plan selection

| Invariant | Test | Status |
|---|---|---|
| One selected id; every other candidate rejected once with a reason | `plan_selection_selects_one_and_explains_the_rest` | passes |
| The selected plan is the cheapest valid one | `plan_selection_selects_cheapest_valid` | passes |
| Each node of a valid candidate is charged once per consuming query while `NotMaterialized` (all today), once when materialized | `plan_selection_charges_each_node_per_consuming_query` | passes |
| A UnivMon estimate has a guarantee, so it is judged against its target instead of rejected for having none | `plan_selection_judges_univmon_with_an_accuracy_model` | ignored: L2 in #621; distinct count and entropy have no PR |
| One materialized UnivMon for all three costs no more than three separate UnivMons | `plan_selection_shared_univmon_costs_no_more_than_three` | ignored: #595, #596, #621, #604 |
| Selection direction: the selected plan has at most one UnivMon, because one shared UnivMon dominates several over the same input | `plan_selection_selected_plan_has_at_most_one_univmon` | passes (vacuously today: no UnivMon is valid) |

The doc's "the shared candidate usually wins" depends on the cost model, so
it is tested only as the dominance above: one build sized for the strictest
consumer costs no more than three builds that include one of that size. Whether
it also beats the exact plans is the model's call. The dominance needs the
shared UnivMon materialized (#604): plan selection charges a `NotMaterialized`
node once per consuming query, so unmaterialized it costs three builds of the
strictest size.

## Ambiguities and conflicts with the planner

1. **SQL recognition.** Q2 and Q3 lower to exact relational plans, so the doc's
   workload has no `Entropy` or `L2` intent (#598). Hence the stand-in. The
   stand-in's statistics are per series (`PerEntity`), the doc's over the
   whole column; with one `flows_src_ip` series they coincide.
2. **Pass 1 options.** The doc gives each computation three options. The
   planner gives Q1 five (three specialized distinct-count summaries) and Q2,
   Q3 two (no specialized entropy or norm summary). The tests check presence,
   not counts.
3. **Candidate count.** The doc's 37 excludes the identical-expression rule's
   shared-input variant (Example 1 counts it). Neither Pass 2 rule exists yet
   (#588, #595), so today there are 20. Tests do not pin a count.
4. **Independent UnivMons on a shared input.** The doc keeps independent
   candidates. Once Pass 2 shares the input, identical UnivMon builds may
   merge; the tests require the independent combinations only without a
   shared summary.
5. **Sizing.** "Sized for the strictest requirement, ε = 0.01" presumes
   UnivMon sizing per ε; UnivMon has a fixed shape today (#621).
6. **Accuracy.** Plan selection accepts an output whose guarantee meets the
   target or for which the deployment has evidence. No UnivMon estimate has
   a guarantee, so every UnivMon candidate is invalid; #621 certifies L2
   only. HLL is rejected against an (ε, δ) target because its bound has no
   failure probability; the doc treats a distinct summary as valid.
7. **Pairwise count.** "The third keeps any of its own 3 options" includes its
   own UnivMon, sized for itself. The test requires the third's full Pass 1
   option set.
8. **Data workload.** Only `input_cardinality` and `data_ingestion_interval`
   change, so the ingestion rate stays about 66,667/s with 10,000,000 keys.
9. **SQL candidates that fail to compose.** On `flows(ts, src_ip)`, sketch
   candidates for `COUNT(*) … GROUP BY src_ip` reference
   `ColumnRef::SampleValue`, and the schema has no `value` column, so
   composition fails (#603). This is a Pass 1 defect independent of this
   example.

## Devtool

No `--example planner-layering-2` yet. It needs:

* SQL `Entropy` and `L2` recognition (#598), so the demo shows the doc's
  workload rather than exact relational plans;
* the Pass 1 fix in ambiguity 9 (#603);
* a SQL lowering path with the `flows` catalog in `stage_pipeline` (devtool
  only; it lowers PromQL only today).
