# Planner layering, Example 2: acceptance spec

Audience: planner designers and the implementers of the summary-capability
rule, UnivMon sizing and accuracy, and SQL `Entropy`/`L2` recognition.
Source: [planner-layering.md](planner-layering.md), "Example 2: One summary for
several computations". Tests:
`crates/integration-tests/tests/planner_layering_example2.rs` (helpers in
`tests/planner_layering_common/`).

This spec was written by a test designer who does not implement the stages.
Tests run the library pipeline `plan_selection::plan_stages` (Stage 1 → 2 → 3).
A test that needs a missing feature is `#[ignore]`d with the feature's name.

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
exact relational plans (the doc's own TODO). Stages 1–3 therefore run on a
PromQL stand-in with the same structure, three statistics of one input over
the same 1-min window: `distinct_over_time(flows_src_ip[1m])`,
`entropy_over_time(flows_src_ip[1m])` and `l2_over_time(flows_src_ip[1m])`,
with Q1–Q3's targets and a 15-s ingestion interval (PromQL needs one).

## Expected candidates per stage

| Stage | Doc | Today (stand-in) |
|---|---|---|
| 0 | One summary-free DAG: `Distinct`, `Entropy`, `L2` over `flows.src_ip`, last 1 m | Same for the stand-in. SQL: Q1 is `aggregate:cardinality`; Q2 and Q3 are `count` per `src_ip` → window function / projection → `sum` (not recognized). |
| 1, Pass 1 | Each computation: exact, a specialized summary, UnivMon. 3 × 3 × 3 = **27** | Q1: exact, HLL, Theta, KMV, UnivMon. Q2, Q3: exact, UnivMon (no specialized entropy or norm summary). 5 × 2 × 2 = **20** |
| 1, Pass 2 | Summary-capability rule: + 1 (one UnivMon for all three, sized for ε = 0.01) + 9 (3 pairs share a UnivMon × 3 options of the third). Independent kept. **37**. Window composition left out. | Identical-expression rule only: a shared-input variant of every combination, **40**. In that variant, UnivMons chosen by several queries merge into one build because Pass 1 gives every UnivMon the same parameters. |
| 2 | Same options as Example 1: no window form → rebuilt from raw samples each refresh. | One physical candidate per logical candidate, everything at query time. |
| 3 | One UnivMon sized for ε = 0.01 vs three separate summaries; the shared one usually wins (one update per flow record instead of three). | HLL rejected (its bound has no failure probability for the (ε, δ) target); every UnivMon rejected (no accuracy model). An all-exact shared-input plan is selected. |

## Invariants and tests

### Stage 0

| Invariant | Test | Status |
|---|---|---|
| The SQL workload is valid, in entry order | `workload_encodes_example2` | passes |
| SQL Q1 lowers to a distinct count over `flows` | `stage0_sql_q1_lowers_to_a_distinct_count` | passes |
| SQL Q2 and Q3 lower to `FrequencyEntropy` and `FrequencyL2` | `stage0_sql_q2_q3_lower_to_entropy_and_l2` | ignored: SQL frontend recognition |
| The stand-in lowers to the three statistics over one 1-min range | `stage0_standin_lowers_to_three_statistics_of_one_input` | passes |
| One summary-free DAG; the queries share no node | `stage0_one_summary_free_dag_without_sharing` | passes |

### Stage 1

| Invariant | Test | Status |
|---|---|---|
| Each statistic has an exact and a UnivMon option | `stage1_pass1_offers_exact_and_univmon_for_each_statistic` | passes |
| Q1 has a specialized distinct-count summary (HLL, Theta or KMV) | `stage1_pass1_offers_a_distinct_count_summary` | passes |
| Q2 and Q3 have a specialized entropy and norm summary | `stage1_pass1_offers_specialized_entropy_and_l2_summaries` | ignored: Pass 1 families |
| Every combination of the queries' own options is kept, with no shared summary | `stage1_keeps_every_independent_combination` | passes |
| A candidate has one UnivMon build read by all three queries, feeding a distinct-count, an entropy and an L2 estimate | `stage1_summary_capability_adds_one_univmon_for_all_three` | passes (see note) |
| For each pair, a candidate shares one UnivMon between the two while the third takes each of its own options | `stage1_summary_capability_adds_pairwise_shared_univmons` | ignored: summary-capability rule |
| A shared UnivMon has the parameters Pass 1 gives its strictest consumer alone | `stage1_shared_univmon_is_sized_for_the_strictest_consumer` | passes (see note) |
| Pass 1 sizes UnivMon per target: Q3's (ε = 0.01) is larger than Q2's (ε = 0.05) | `stage1_univmon_is_sized_per_accuracy_target` | ignored: UnivMon sizing |
| Only a UnivMon is ever shared across statistics | `stage1_only_univmon_is_shared_across_statistics` | passes |
| Candidates are valid DAGs with unique ids | `stage1_candidates_are_valid_and_uniquely_named` | passes |

Note: the all-three UnivMon exists today only because Pass 1's UnivMon
parameters ignore ε, so the identical-expression rule merges the three builds
in the shared-input variant. Once UnivMon is sized per target, these two tests
fail until the summary-capability rule sizes one UnivMon for the strictest
consumer.

### Stage 2

| Invariant | Test | Status |
|---|---|---|
| One physical candidate per logical candidate | `stage2_keeps_every_logical_candidate` | passes |
| The shared UnivMon stays one build read by all three queries | `stage2_keeps_the_shared_univmon_as_one_build` | passes |

Materialization and window variants are out of scope: the doc leaves out the
window-composition variants for this example, and a summary with no window
form is rebuilt at every refresh (Example 1's table).

### Stage 3

| Invariant | Test | Status |
|---|---|---|
| One selected id; every other candidate rejected once with a reason | `stage3_selects_one_and_explains_the_rest` | passes |
| The selected plan is the cheapest valid one | `stage3_selects_cheapest_valid` | passes |
| Each node is charged once (a shared summary is costed once) | `stage3_charges_each_node_once` | passes |
| UnivMon candidates are judged by an accuracy model, not rejected for lacking one | `stage3_judges_univmon_with_an_accuracy_model` | ignored: UnivMon accuracy model |
| One UnivMon for all three costs no more than three separate UnivMons | `stage3_shared_univmon_costs_no_more_than_three` | ignored: UnivMon accuracy model |
| Selection direction: the selected plan has at most one UnivMon, because one shared UnivMon dominates several over the same input | `stage3_selected_plan_has_at_most_one_univmon` | passes (vacuously today: no UnivMon is valid) |

The doc's "the shared candidate usually wins" depends on the cost model, so
it is tested only as the dominance above: one build sized for the strictest
consumer costs no more than three builds that include one of that size. Whether
it also beats the exact plans is the model's call.

## Ambiguities and conflicts with the planner

1. **SQL recognition.** Q2 and Q3 lower to exact relational plans, so the doc's
   workload has no `Entropy` or `L2` intent. Hence the stand-in. The stand-in's
   statistics are per series (`PerEntity`), the doc's over the whole column;
   with one `flows_src_ip` series they coincide.
2. **Pass 1 options.** The doc gives each computation three options. The
   planner gives Q1 five (three specialized distinct-count summaries) and Q2,
   Q3 two (no specialized entropy or norm summary). The tests check presence,
   not counts.
3. **Candidate count.** The doc's 37 excludes the identical-expression rule's
   shared-input variant, which the planner adds (Example 1 counts it). Tests do
   not pin a count.
4. **Independent UnivMons on a shared input.** The doc keeps independent
   candidates. The planner keeps them only with separate inputs: in the
   shared-input variant, identical UnivMon builds always merge.
5. **Sizing.** "Sized for the strictest requirement, ε = 0.01" presumes
   UnivMon sizing per ε; Pass 1 uses fixed UnivMon parameters.
6. **Accuracy model.** Stage 3 has no UnivMon accuracy model and rejects every
   UnivMon candidate. It also rejects HLL against an (ε, δ) target because the
   HLL bound has no failure probability; the doc treats a distinct summary as
   valid.
7. **Pairwise count.** "The third keeps any of its own 3 options" includes its
   own UnivMon, sized for itself. The test requires the third's full Pass 1
   option set.
8. **Data workload.** Only `input_cardinality` and `data_ingestion_interval`
   change, so the ingestion rate stays about 66,667/s with 10,000,000 keys.
9. **SQL candidates that fail to build.** On `flows(ts, src_ip)`, 180 of the
   500 SQL candidates fail in Stage 1: sketch alternatives for `COUNT(*) …
   GROUP BY src_ip` reference `ColumnRef::SampleValue`, and the schema has no
   `value` column. With extra numeric columns they build, apparently reading
   another column. This is a Pass 1 defect independent of this example.

## Devtool

No `--example planner-layering-2` yet. It needs:

* SQL `Entropy` and `L2` recognition (frontend), so the demo shows the doc's
  workload rather than exact relational plans;
* the Pass 1 fix in ambiguity 9: today `stage_pipeline` aborts exporting the
  first unbuildable candidate;
* a SQL lowering path with the `flows` catalog in `stage_pipeline` (devtool
  only; it lowers PromQL only today).
