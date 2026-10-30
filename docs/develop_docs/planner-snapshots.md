# Planner snapshots

The planner has no reference answer to test against, so CI pins what it
produces. For each query in a corpus, the plan that `stage_pipeline` selects
(default search) is committed as a snapshot. The `Corpus snapshots` workflow
checks them nightly on `main` (and on demand from the Actions tab, for any
branch); a change to any of these plans fails it until the snapshots are
regenerated and the change is explained.

## Layout

`crates/devtools/tests/corpus_snapshots/<corpus>/`:

- `corpus.json`: the inputs shared by the corpus's queries (`stage_pipeline_args`,
  the repeating-query entry and the `data_workload`), and the queries, each
  with a stable `id`. Each query is planned on its own. A query with a
  `skip_reason` entry (saying why) is skipped and has no snapshot.
- `<id>.json`: that query's snapshot, pretty-printed with sorted keys.

## What a snapshot holds

Only the plan, which must not change between runs:

- `query` and `accuracy_guarantees`;
- `selected`: the selected Stage 2 candidate's `display_name` and `dag`, its
  `cost` (`cost_per_second`, `usage`, `per_node`) and `accepted` accuracy;
- `proven_optimal`: whether the search proved the plan optimal (its status is
  `optimal`) rather than stopping at an evaluation budget;
- `materialized_views`, `pane_retention_ms` and any `warnings`;
- for a query that does not plan, `selected: null`, the `error` up to its
  embedded search report, and the causes and reasons of the rejections.

Floats are rounded to 12 significant digits. Everything about the search
except whether it proved the plan optimal is left out: candidate ids (such as
`pane-round-12-joint-plan-1`), the search report's counters, bounds and timings, the Stage 1 and Stage 2
candidate lists, and the memo (`stage_pipeline` runs with `--no-memo`). These
vary from run to run because the search's pane-width rounds have wall-clock
budgets, so a different round can find the same plan.

## Sketch plan snapshots

The corpus queries carry no accuracy target, so their plans are exact.
`crates/devtools/tests/sketch_plan_snapshots/` pins the plans of workloads
that select sketches: `manifest.json` lists each workload (a workload file or
built-in example, its deployment, and whether accuracy guarantees are
enforced), and `<id>.json` is its snapshot, holding the same fields as a
corpus snapshot plus every query's text and the `search_status`. The test,
`sketch_plans_match_snapshots`, is in the same `corpus_snapshots` binary and
runs nightly with it; the two tests never run at once. Every workload must end
with status `optimal`: a search stopped by any budget may select another plan
on a slower machine, so any other status fails. Regenerate with:

```sh
UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored sketch_plans_match_snapshots
```

## When the check fails

The test names each changed query and its first differing line. CI uploads
the regenerated snapshots, with each one's full `stage_pipeline` document
(`<id>.document.json`, which [`tools/dag-viewer`](../../tools/dag-viewer/README.md)
opens), as the `corpus-snapshots` artifact.

If the change is intended, regenerate the snapshots and commit them:

```sh
UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored
```

Then list each changed query and why it changed under **Snapshot changes** in
the PR description.

The test also fails, instead of recording a snapshot, in these cases:

- A query's search was ended by a wall-clock budget: its status is
  `time_limit`, or planning used the whole 240 s limit. Its plan would then
  depend on machine speed. Only the final pane round's status is checked: an
  earlier round cut short by its own wall-clock budget is not reported.
- A snapshot file has no matching checked query in `corpus.json`.
- `stage_pipeline` crashed.

## Adding queries

Add the query to `corpus.json` with a new `id` and regenerate. Before you
commit, run the check twice, plus once on one core (`taskset -c 0`), and confirm
it passes each time. Keep each query well under the 240 s limit on one core
with a release build; otherwise give it a `skip_reason`.
