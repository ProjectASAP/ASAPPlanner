# Testing infrastructure

> Status: proposal, not implemented. Design decisions are settled
> (2026-11-15); the tests and infrastructure are tracked in a tracking
> issue. Audience: planner developers.
> Companion: [Would these tests have caught the #1207 bugs?](bug-coverage.md)

## Principles

- Prefer end-to-end tests over the `asap_planner` binary to unit tests.
  Hand-written unit assertions, especially agent-written ones, give false
  confidence.
- Every test states the behavior it pins and why. A person confirms
  the why for any golden change (AGENTS.md).
- Cheap invariants run everywhere, so every test exercises them for free.

## Two axes

A test is an **oracle** (what decides pass/fail) applied to an **input
source**.

### Oracles

| Oracle | Proves | Weakness |
|---|---|---|
| Curated golden | Spec: this input plans like *this* | Only as good as its reviewer |
| Stability snapshot | Nothing changed unless explained | Says nothing about correctness |
| Structural invariants | Schemas stable, DAG valid, memo consistent | Not semantic |
| Metamorphic relations | Cost/reuse relations between related workloads | Needs a witness per relation |
| Differential (planner vs itself) | Two configurations agree | Both can be wrong the same way |
| Execution | Plan's answers within claimed ε/δ of exact | **Deferred** |

Curated goldens and stability snapshots use the same mechanism and differ in
intent: curated cases carry a why; corpus snapshots are bulk change
detectors.

### Input sources

- Hand-written workloads (curated goldens, per-rule cases).
- Real corpora (`crates/devtools/tests/corpus_snapshots/`, frontend-sql corpora).
- Deterministic generators (seeded).
- Fuzzer (`planner_fuzz`), with shrinking.

### Measuring the suite

Oracles and input sources describe tests of the planner. Two measures test
the suite itself instead, and decide nothing about the planner:
rule-firing coverage (which rules no test exercises) and mutation testing
(which code changes no test detects). See their sections below.

## Decisions

- **Decided: committed goldens**, not CI diffing against main. Expected
  output lives in the repo and changes in the same PR as the code, so the
  plan diff is in the PR's file diff and `git log` records when and why a
  plan changed. Diffing against main remains an option only for corpora too
  large to commit, as a warning rather than a blocker.
- **Decided: PromQL ≡ SQL means same memo root group under a declared
  source mapping.** The test declares that table `T(ts, value, labels…)` is
  the same data as metric `M`, rewrites the `Scan` leaf
  (`Source::Table` ↔ `Source::TimeSeries`, `crates/types/src/ir/operator/arguments.rs`),
  and asserts both queries hash-cons to the same root group. The mapping must
  pin row meaning (one row per sample). Replaces the "same shape above the
  leaf" comparison in `crates/devtools/tests/cross_language.rs`.
- **Decided: `asap_planner` may gain rule-selection flags**
  (e.g. `--only-rules`, `--disable-rule`) for per-rule tests.
- **Decided: goldens store `asap_planner --format json` stage output.**
  Tests the JSON cannot express (equivalence assertions, "must change",
  invariants, relations) use other harnesses.
- **Deferred: execution-based oracles.**

## Tests to add (tracking)

### `asap_planner --run-until stage0`

Logic: front ends (`asap-frontend-promql`, `asap-frontend-sql`,
`asap-frontend-metricsql`, `asap-frontend-common::canonicalize`) and
`asap-stage0-memo::memo_of_queries`.

- **T1 Lowering golden**
  - Tests: each query lowers to the expected memo.
  - Input → output: workload → Stage 0 JSON.
  - Oracle: curated golden.
  - Today: no Stage 0 golden; ~10 devtools tests check fields by hand;
    400+ front-end tests check the front-end tree, not the memo.
  - Blocked by: the JSON printer (#1207 PR 9) and the golden runner (D5).
- **T2 PromQL ≡ SQL ≡ MetricsQL (cross language)**
  - Tests: equivalent queries in different languages share one memo group.
  - Input → output: a golden case with one workload file per language and
    a SQL table declaring its metric (D11) → root group of each.
  - Oracle: same root group after the declared mapping rewrites the `Scan`
    leaf.
  - Today: none. `devtools/tests/cross_language.rs` compares pre-memo
    shapes (stale; replace it). No PromQL ≡ MetricsQL test.
  - Blocked by: the `time_series_metric` catalog field (D11).
- **T3 Equivalence within one language**
  - Tests: semantically equal spellings share one memo group:
    `sum by (a)` vs `sum without (rest)` (if PromQL metric schema is defined); SQL CTE vs subquery, `GROUP BY`
    order, aliases, conjunct order.
  - Input → output: pairs file → root group of each.
  - Oracle: same root group.
  - Today: none.
- **T6 Group properties**
  - Tests: `column_signs`, `aggregate_coverage`,
    `depends_on_evaluation_time`, `scanned_sources` per group.
  - Input → output: workload → per-group properties in Stage 0 JSON.
  - Oracle: curated golden.
  - Today: 43 hand-written tests; `depends_on_evaluation_time` and
    `scanned_sources` have 1 each.
  - Blocked by: as T1.

### `asap-types::stages::logical::memo` (`Memo::find_or_add_group`, `Memo::add_candidate`)

Invariants checked inside the memo, so every test that builds a memo
exercises them.

- **T4 Schema preservation**
  - Tests: every expression added to a group has the group's schema.
  - Input → output: any memo insertion → error naming the rule or stage
    that added the expression.
  - Oracle: always-on invariant.
  - Today: none; 100 per-operator schema unit tests (81 hand-written).
- **T25 Schema across stages**
  - Tests: each query's output schema is the same at every stage: its
    Stage 0 root group, its Pass 1/Pass 2 candidates, its Stage 2 physical
    node, and the Stage 3 plan's output for that query.
  - Input → output: any planned workload → pass / error naming the stage.
  - Oracle: always-on invariant at each stage boundary (DataFusion checks
    the schema after every optimizer rule).
  - Today: none.
  - Blocked by: #1207 PRs 5–6 for the Stage 2 and 3 checks.
- **T5 Memo consistency**
  - Tests: expressions in a group agree on schema and properties; no
    duplicate expressions; re-inserting the memo reproduces the same group
    ids; no orphan groups or candidates left by a failed or rejected
    attempt (ledger bugs `1232-18`, `1232-F1`, `1234-F4`).
  - Input → output: a finished memo → pass / error.
  - Oracle: debug-build invariant.
  - Today: none.

### `asap_planner --run-until stage1_pass1`

Logic: `asap-stage1-pass1-per-computation-candidates::pass1_memo`.

- **T7 Per-computation candidates**
  - Tests: the candidates built for each computation.
  - Input → output: workload → Pass 1 JSON.
  - Oracle: curated golden; migrate the 9 tests the audit flagged.
  - Today: 59 tests, 2 golden.
  - Blocked by: as T1.

### `asap_planner --run-until stage1_pass2 --only-rules <rule>`

Logic: `asap-stage1-pass2-reuse-candidates::rules::{rollup, topk, chains}`.

- **T8 Per-rule reuse**
  - Tests: one reuse rule fires when it should and not when it shouldn't.
  - Input → output: workload → Pass 1 text (before) and Pass 2 text
    (after).
  - Oracle: curated golden. Fails if the rule changed nothing, unless the
    case expects "unchanged". Two-sided pairs, negatives written as
    deliberately as positives (Calcite `MaterializedViewRelOptRulesTest`
    `.noMat()`): finer window → coarser query must reuse, coarser → finer
    must not; top-k reused by a smaller k, not a larger one.
  - Today: none; the crate has 0 tests; no top-k reuse test anywhere.
  - Blocked by: #1207 PR 4, `--only-rules`.
- **T24 Rule removal**
  - Tests: each rule earns its place; nothing else silently covers for it.
  - Input → output: T8's positive cases planned with and without
    `--disable-rule <rule>` → Stage 3 cost and plan.
  - Oracle: metamorphic: disabling a rule never lowers cost; on the rule's
    positive cases the plan loses the reuse (cost strictly higher), or the
    case documents why another rule covers it.
  - Today: none.
  - Blocked by: #1207 PR 6, `--disable-rule`.

### `asap_planner --run-until stage2`

Logic: `asap-stage2-physical-optimization::generate_physical_asap_memo_candidates`,
`asap-types::plan::physical_asap_dag::PhysicalASAPDAG::validate`.

- **T10 Physical candidates**
  - Tests: physical expansion of each logical candidate; the DAG is valid.
  - Input → output: workload → Stage 2 text.
  - Oracle: curated golden, plus `validate()` always on in the driver and
    in snapshot runs.
  - Today: none; the crate has 0 tests; snapshots never call `validate()`.
  - Blocked by: #1207 PR 5.

### `asap_planner --run-until stage3` (whole planner)

Logic: everything above plus `asap-stage3-plan-selection::select_plan`
(MILP) and `asap-cost::plan::cost::plan_cost`.

- **T22 Sharing summary**
  - Tests: which shared states/sketches exist and which queries read each,
    after each prefix of the workload (q1; q1+q2; …).
  - Input → output: workload → "who shares what" list per prefix.
  - Oracle: curated golden.
  - Today: none.
  - Blocked by: #1207 PR 6.
- **T11 Search differential**
  - Tests: plan selection finds the optimum over the candidates it has.
  - Input → output: small generated workloads → selected cost under the
    default and exhaustive searches.
  - Oracle: differential: equal cost; a "proved optimal" claim equals the
    exhaustive optimum.
  - Today: none; `joint_search_equivalence.rs` ignores its `_exhaustive`
    parameter (`:76`).
  - Blocked by: #1207 PR 6.
- **T9 Missed reuse (`planner_fuzz`)**
  - Tests: no reuse is missed between related workloads.
  - Input → output: generated workload pairs → costs and witness plans.
  - Oracle: metamorphic; commit a fixed-seed run that executes every
    relation.
  - Today: 23 relations; (d), (i), (k) and all 8 cost relations never run
    in a committed test.
  - Blocked by: #1207 PRs 4–6.
- **T13 Accuracy monotonicity**
  - Tests: a tighter accuracy target is never cheaper.
  - Input → output: workload and a copy with tighter targets → costs.
  - Oracle: metamorphic.
  - Today: none.
  - Blocked by: #1207 PR 6.
- **T15 Order invariance**
  - Tests: reordering queries does not change the plan.
  - Input → output: workload and a permutation → plans.
  - Oracle: metamorphic.
  - Today: fuzz `Reordered` relation exists, never runs.
  - Blocked by: #1207 PR 6.
- **T16 Separability**
  - Tests: workloads over disjoint sources do not interact.
  - Input → output: W1, W2, W1 ∪ W2 → plans and costs.
  - Oracle: metamorphic: plan of the union is the two plans side by side;
    cost is the sum.
  - Today: none.
  - Blocked by: #1207 PR 6.
- **T17 Renaming invariance**
  - Tests: no behavior depends on names.
  - Input → output: workload and a consistently renamed copy (metrics,
    labels, tables, columns) → plans.
  - Oracle: metamorphic: same plan up to names.
  - Today: none.
  - Blocked by: #1207 PR 6 (the Stage 0 part can run now).
- **T19 Plan cost relations**
  - Tests: plan-level cost behaves as expected (e.g. joint ≤ separate).
  - Input → output: workload and its per-query split → costs.
  - Oracle: metamorphic; one shared helper instead of the 4 copies today.
  - Today: 38 per-operator arithmetic tests; `plan/cost.rs` has no tests.
  - Blocked by: #1207 PR 6.

- **T26 Plan-changing inputs**
  - Tests: each non-query input changes the plan the way it should:
    latency bounds (e.g. a tight bound forces precompute instead of a raw
    scan), deployment capabilities (an unsupported sketch family is never
    selected), retention.
  - Input → output: a workload and the same workload with one input
    changed → Stage 3 JSON of each.
  - Oracle: curated golden of both plans.
  - Today: none found by the audit.
  - Blocked by: #1207 PR 6.
- **T23 Corpus snapshots**
  - Tests: real-corpus plans do not change unless explained.
  - Input → output: `crates/devtools/tests/corpus_snapshots/` corpora →
    selected plan per query.
  - Oracle: stability snapshot.
  - Today: Stage 3 only; nightly; failing since #1207 stubbed the later
    stages.
  - To do: re-bless once #1207 lands; decide whether to snapshot each
    stage, not just Stage 3.
  - Blocked by: #1207 PR 6.

### `asap-accuracy`

- **T12 Propagation (`asap-accuracy::propagation`)**
  - Tests: per-node accuracy derived for each plan node.
  - Input → output: plan → per-node ε/δ.
  - Oracle: curated golden; replaces the stale `propagation.rs` /
    `function_rules.rs` tests.
  - Today: 32 hand-written tests of dormant pre-memo code;
    `guarantee/ddsketch.rs` has 0 tests.
  - Blocked by: #1207 PR 4+ (the memo-design rewrite of propagation).
- **T14 Composition algebra (`asap-accuracy::composition`, `asap-accuracy::guarantee`)**
  - Tests: how ε/δ combine.
  - Input → output: generated guarantees → composed guarantee.
  - Oracle: property: composition is monotone, and order-independent where
    it should be.
  - Today: none.

### Infrastructure (not tests)

- **T20 Ignored tests**: every `#[ignore]` test runs in some workflow, or is
  deleted. Today 7 of 9 never run.
- **T21 Replay = case directory** (D10): every bug repro is a golden case
  directory (D6) holding every planner input; committing it makes it a
  test. Later, if needed, `asap_planner --dump-case <dir>` writes a run's
  inputs as one.

## Existing tests: cleanup

The audit (see "Audit of existing tests") gives each existing test a
proposed fate. Its lists, re-checked against main when the work starts:

- **Delete stale tests**: its "Stale tests" section (e.g.
  `cross_language.rs` once T2 lands, the pre-memo accuracy tests once T12
  lands, the dead `planner_layering_common`).
- **Dedupe**: its "Overlaps" section (e.g. the 4 copies of the `cost()`
  helper, 7+ "selects a valid plan" smoke tests, count semantics tested in
  6 places).
- **Migrate to goldens**: tests marked "migrate to golden" (e.g. 9 Pass 1
  tests, see T7).
- **Fix name/assertion mismatches**: its "Name/assertion mismatches"
  section.- **Speed up the `planner_fuzz` campaign test**:
  `campaign_writes_summary_coverage_and_drafts` takes ~240 s for 2 cases;
  a tiny budget or a cheaper seed may keep it in PR CI.

## Existing infrastructure

- `corpus_snapshots`, `sketch_plan_snapshots` (devtools): committed Stage 3
  snapshots, nightly in release, `UPDATE_SNAPSHOTS=1` to regenerate.
- `planner_fuzz`: metamorphic relations with witnesses and shrinking; seeds
  in `crates/devtools/tests/planner_fuzz_seeds/`.
- `cross_language.rs`: PromQL/SQL shape equivalence (pre-memo; to replace).
- `integration-tests`: ~30 hand-written end-to-end files; slow (the
  slowest take minutes each in debug).
- Frontend conformance: `promql_conformance.rs`, `victoriametrics_go_golden.rs`.

## Lessons from Calcite and DataFusion

- **Before/after goldens per rule, "must change" by default** (Calcite
  `RelOptFixture.check()` / `checkUnchanged()`): catches rules that silently
  stop firing.
- **Regenerate by overwrite** (Calcite copies `_actual.xml` over the
  reference; DataFusion `--complete`, `cargo insta review`).
- **Data-driven end-to-end files** (DataFusion `.slt`, Calcite `.iq`): input
  plus expected output per section; adding a test needs no Rust. DataFusion's
  contributor guide prefers these over Rust integration tests over unit tests.
- **Invariants inside the optimizer driver**: cheap ones always (DataFusion
  schema check after every rule; Calcite `verifyTypeEquivalence` on every
  `transformTo`), expensive ones in debug builds (DataFusion
  `check_invariants`, Calcite `VolcanoPlanner.isValid`).
- **Config-matrix replay** (DataFusion `configMatrix`, TPC-H answers rerun
  under alternate flags): one corpus, many configurations, results must agree.
- **Frontend equivalence and round trips** (DataFusion SQL vs DataFrame plan
  equality, Substrait round trip; Calcite Rel→SQL fixed point).
- **Ratchets** (Calcite records expected SQLite-suite failure counts per file).

## Rule-firing coverage

Goal: find rules and rule branches that no test exercises.

- **Start with line coverage**: `cargo llvm-cov` over the rule crates
  while running the end-to-end suite. No instrumentation; coarse (a line
  can run without the rule producing anything).
- **Then named counters**, if the rule-selection flags make rule names
  available anyway: per rule and branch, count *matched*, *emitted a
  candidate*, *in the selected plan*. `asap_planner` reports them in its
  JSON output; a devtools script sums them over all goldens and corpora.
- Reading the report: never fired → coverage gap. Fired but never in a
  selected plan → dead, or no test where the rule wins.

## Mutation testing

Goal: measure whether the suite detects behavior changes; the direct check
against vacuous tests.

- Tool: `cargo-mutants` (not installed yet). It mutates functions (return
  a default, flip an operator, delete a statement), runs the tests, and
  lists mutants no test killed.
- Scope: pass-2 rules, accuracy, cost, schema derivation. Test command: the
  end-to-end golden suite.
- Cadence: nightly or weekly (each mutant is a rebuild plus a suite run);
  on PRs, `--in-diff` mutates only changed lines. Use a low `-j`: the
  workspace build and tests can OOM.
- Each surviving mutant is a to-do: add a case that kills it, or delete the
  code.
- Caveat: goldens kill almost any plan-changing mutant, so a high score
  means "behavior is visible in the output", not "behavior is correct". The
  score is most useful for finding code that changes no plan (dead code or
  missing cases).

## Audit of existing tests

Done 2026-11-13 at `76418c7a7` by reading the code (nothing was run); the
report is not committed, since it goes stale as #1207 lands, and is re-run
when cleanup starts. Each test was classified by subject, oracle, input
source, entry point, strength, cost and proposed fate, and the report
listed overlaps, stale tests and ignored tests. Its gaps are items under
"Tests to add".

## Design decisions

All decided (2026-11-15). Timing constraints: group A shapes the JSON
printer, stubbed until #1207 PR 9 (`asap_planner.rs:742`); D18 shapes the
solver setup written in #1207 PR 6.

### A. Golden content (before #1207 PR 9)

- **D1 Stable ids. Decided: canonical renumbering.** Group and candidate
  ids follow insertion order, so an unrelated change renumbers every
  golden. The printer renumbers depth-first from the query roots, in query
  order. Raw ids would bring the churn back, so they are printed only
  outside goldens (e.g. behind a debug flag), to match output to logs and
  error messages.
- **D2 Numbers. Decided: round, compare as text.** The printer rounds
  costs and ε/δ to 12 significant digits (as corpus snapshots do); goldens
  are compared as plain text, not with a numeric tolerance.
- **D3 Exclusions. Decided:** goldens leave out timings, solver statistics
  (gap, node counts), absolute paths and raw ids (D1), as corpus snapshots
  do.
- **D4 Per-stage output. Decided: full memo per step.** The printer
  (`asap_planner.rs:369`) already emits one top-level key per stage and
  `memo.steps = [stage0, pass1, pass2]`. Each step is the full memo at that
  point, with candidates marked `added` relative to the step before and
  tagged with their origin. A Stage 0 change repeats in every step's diff;
  accepted for simplicity. D1's renumbering must also follow the groups a
  candidate reads from, so groups added in Pass 2 get stable ids.

### B. Golden runner

- **D5 One generic runner. Decided: `libtest-mimic`.** One test binary
  (`harness = false`) walks `tests/goldens/**`; adding a test adds files,
  not Rust. Each case is a named test, so `cargo test --test goldens
  rollup/` runs a subset and every failure is listed. Adds one
  dev-dependency.
- **D6 Case layout. Decided: one directory per case.**
  ```
  tests/goldens/<area>/<case>/      e.g. rules/rollup/coarse_sum_from_fine/
    case.toml       why (one line); args (--run-until, --only-rules, ...);
                    stage3 = "exact" | "cost" (D18); pending (D22);
                    tier (D19); assertions (D15)
    workload.json   passed to --workload
    cost_inputs.json, deployment_capabilities.json, tables/*.sql (optional)
    expected.json   the --format json output, written by UPDATE_SNAPSHOTS=1
  ```
- **D7 Invocation. Decided: subprocess.** The runner runs the
  `asap_planner` binary with the case's arguments, as corpus snapshots do,
  so tests cover argument parsing and the printer and match the command a
  failure prints (D25).
- **D8 Regenerate and diff. Decided:** `UPDATE_SNAPSHOTS=1` rewrites
  `expected.json` instead of comparing; goldens are pretty-printed JSON
  with sorted keys, so failures and PR diffs are line diffs. E.g.
  `UPDATE_SNAPSHOTS=1 cargo test --release --test goldens rollup/`.

### C. `asap_planner` command line

- **D9 Rule names. Decided: a separate `Rule` enum** in `asap-types`
  with `Display`/`FromStr` snake_case names, used by `--only-rules`,
  `--disable-rule`, rule-firing coverage and the JSON. It is distinct from
  `OriginOfCandidateForMemoGroup` (a candidate's kind), whose names today
  differ from the modules (`chains` vs `tumbling_windows`, `topk` vs
  `top_k_reuse`). For D13, each candidate also records the `Rule` that
  added it.
- **D10 Replay file (T21). Decided: the case directory is the replay
  format;** no separate single-file bundle. `asap_planner --dump-case
  <dir>` is added only when a real need appears (e.g. D26).
- **D11 Source mapping (T2). Decided: a field on `--sql-catalog-table`,**
  e.g. `"time_series_metric": "http_requests"`; the SQL frontend lowers
  that table as `Source::TimeSeries`. With repeatable `--workload` (one
  file per language), T2 is an ordinary golden case with a
  `same_group: [q1, q2]` assertion.

### D. Invariants (T4, T5, T25)

- **D12 Placement. Decided: always on, every build.** Schema checks
  inline in `Memo::find_or_add_group` / `add_candidate`; the T5
  consistency walk at each stage boundary. Debug-only would never run,
  since tests run in release (D19). If profiling shows the T5 walk is
  costly, move it behind an `asap_planner --check-invariants` flag that the
  golden runner and fuzzer always pass.
- **D13 Naming the culprit. Decided by D9:** an invariant error names the
  `Rule` (or stage) recorded on the candidate that broke it.
- **D14 Panic or error. Decided: typed error, own exit code.** An
  invariant violation is `InvariantViolation { rule, check, group }`
  (D13), and `asap_planner` exits with a dedicated code (e.g. 3). The
  golden runner always fails such a case, even under `UPDATE_SNAPSHOTS=1`,
  unless it is pending (D22), so a bug is never blessed. The fuzzer counts
  it as a finding, separate from crashes. Planning errors (infeasible,
  invalid input) stay ordinary outcomes a golden may record.

### E. Non-golden harnesses

- **D15 Assertions (T2, T3, T8). Decided: a small fixed vocabulary** in
  `case.toml`, checked by the golden runner against the JSON output, so a
  wrong rewrite of `expected.json` still fails. Start with:
  ```toml
  same_group = [["q1", "q2"]]
  different_group = [["q1", "q3"]]
  rule_fires = ["rollup"]
  rule_does_not_fire = ["top_k_reuse"]
  ```
  New kinds are added only when a test needs one; no generic JSON-path
  assertions.
- **D16 Relations (T9, T13, T15–T17, T24). Decided: on golden cases and
  generated workloads.** Relations live in one shared module. The golden
  runner re-plans every case shuffled (T15) and renamed (T17) and checks
  the plan is unchanged; T9, T13, T16 and T24 run on a case only when its
  `case.toml` lists them (`relations = [...]`). `planner_fuzz` applies all
  of them to generated workloads, with a committed fixed-seed run on every
  PR.
- **D17 Property tests (T14). Decided: `proptest`,** a dev-dependency of
  `asap-accuracy` only. Generators list edge values explicitly (−0.0,
  NaN, ±Inf, q ∉ [0, 1]); failures shrink automatically and are kept in a
  committed regressions file.

### F. CI and determinism

- **D18 Stage 3 determinism. Decided: two kinds of Stage 3 golden.** A
  MILP can return a different plan of equal cost (ties), or stop at a
  different plan under a time limit or several threads. Settle before
  #1207 PR 6 writes the solver setup.
  - Small cases: one thread, fixed seed, no time limit, solved to proven
    optimality, with a deterministic tie-break (e.g. a tiny penalty
    preferring lower canonical ids). The golden stores the exact plan.
  - Large cases (corpus, nightly; may hit the time limit): the golden
    stores only the plan's cost and the sharing summary (T22).
- **D19 Tiers. Decided: everything on PRs, in release.** Every golden
  runs on every PR in release mode. A case found to be slow moves to the
  nightly workflow then (e.g. a `tier: nightly` field in its metadata);
  no nightly tier is built up front.

### G. Process

- **D20 Reviewing golden changes. Decided:** extend AGENTS.md's corpus
  snapshot rule to all goldens. The PR lists each changed case with a
  one-line why; a person, not the agent that made the change, confirms
  each why. No CI check enforces it.
- **D21 Authorship. Decided: no extra process.** No `author` field or
  rule beyond AGENTS.md's existing guidance on independent test design.

### H. More

- **D22 Pending cases. Decided: yes.** A case can be committed before the
  code it tests exists, marked "pending until #1207 PR N". The runner
  expects it to fail while pending and fails if it starts passing, so the
  marker is removed when the PR lands. Lets acceptance cases (e.g. T8) be
  written before the implementation.
- **D23 Whole workload and per query. Decided: both.** Goldens plan the
  whole workload, since sharing is the point; per-query planning stays
  where it is useful (e.g. corpus snapshots today).
- **D25 Shared defaults. Decided: one defaults directory.**
  `tests/goldens/defaults/` holds the default cost inputs, deployment
  capabilities and data facts; a file in the case directory replaces the
  default file. A `workload.json` without `data_workload` gets
  `defaults/data_workload.json`. A failing case prints the exact `asap_planner` command with
  resolved paths, so it can be reproduced by copy and paste.
- **D28 Sharing summary (T22). Decided: the printer emits it as its own
  key** in the Stage 3 JSON, rather than the runner deriving it.
- **D26 Fuzz findings become cases. Decided: the fuzzer writes the case.**
  Next to each finding's issue draft, `planner_fuzz` writes a golden case
  directory (D6): the shrunk `workload.json`, non-default inputs, and
  `case.toml` with `why = "planner_fuzz finding <id>"` and
  `pending = "<issue>"`. The fix PR copies it to `tests/goldens/fuzz/` and
  removes `pending`.
- **D27 Corpus snapshots. Decided: migrate into the golden runner,** as a
  follow-up once the runner exists. Each `o11y_bench` query becomes
  `tests/goldens/corpus/o11y_bench/qNN/` with `stage3 = "cost"` (D18); the
  corpus's data facts are copied into each `workload.json` by the script
  that generates the corpus. `corpus_snapshots.rs` is then deleted.

## Open questions

- Which existing `integration-tests` files migrate to JSON goldens.
