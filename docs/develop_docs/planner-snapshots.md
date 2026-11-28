# Planner snapshots

The planner has no reference answer to test against, so CI pins what it
produces. For each query in a corpus, the plan that `asap_planner
--run-until stage3` selects is committed as a snapshot. The `Corpus snapshots` workflow
checks them nightly on `main` (and on demand from the Actions tab, for any
branch); a change to any of these plans fails it until the snapshots are
regenerated and the change is explained. That includes a different plan at
the committed plan's cost (see [Strict and local runs](#strict-and-local-runs)).

## Layout

`crates/planner-cli/tests/corpus_snapshots/<corpus>/`:

- `corpus.json`: the inputs shared by the corpus's queries (`asap_planner_args`,
  the repeating-query entry and the `data_workload`), and the queries, each
  with a stable `id`. Each query is planned on its own. A query with a
  `skip_reason` entry (saying why) is skipped and has no snapshot.
- `<id>.json`: that query's snapshot, pretty-printed with sorted keys.

## What a snapshot holds

Only the plan, which must not change between runs:

- `query` and `accuracy_guarantees`;
- `selected`: the selected plan's `dag` (Stage 3's `PhysicalASAPDAG`, with
  the retention and lifetime of its nodes), its `cost` (`cost_per_second`,
  `usage`, `per_node`) and `accepted` accuracy per query;
- `materialized_views` and any `warnings`, `materialized_views_error` and
  `planner_executor_error`;
- for a query that does not plan, `selected: null` and the `error`.

Floats are rounded to 12 significant digits. Everything about the search is
left out: the selection MILP's size and timings, the Stage 1 and Stage 2
candidate lists, and the memo. Timings vary from run to run, and the rest
describes how the plan was found, not the plan.

## Sketch plan snapshots

The corpus queries carry no accuracy target, so their plans are exact.
`crates/planner-cli/tests/sketch_plan_snapshots/` pins the plans of workloads
that select sketches: `manifest.json` lists each workload (a workload file, its deployment
capabilities and cost inputs files, and whether accuracy guarantees are
enforced), and `<id>.json` is its snapshot, holding the same fields as a
corpus snapshot, with `queries` (every query's text) in place of `query`. The test,
`sketch_plans_match_snapshots`, is in the same `corpus_snapshots` binary and
runs nightly with it; the two tests never run at once. Regenerate with:

```sh
UPDATE_SNAPSHOTS=1 cargo test --release -p asap-planner-cli --test corpus_snapshots -- --ignored sketch_plans_match_snapshots
```

## When the check fails

The test names each changed query and its first differing line (for a
sketch plan workload, its first differing JSON path). CI uploads
the regenerated snapshots, with each one's full `asap_planner` document
(`<id>.document.json`, which [`tools/dag-viewer`](../../tools/dag-viewer/README.md)
opens), as the `corpus-snapshots` artifact.

If the change is intended, regenerate the snapshots and commit them:

```sh
UPDATE_SNAPSHOTS=1 cargo test --release -p asap-planner-cli --test corpus_snapshots -- --ignored
```

Then list each changed query and why it changed under **Snapshot changes** in
the PR description.

## Strict and local runs

The nightly workflow sets `STRICT_SNAPSHOTS` (to `1`; any value turns it
on): any difference from a committed snapshot fails, including a different
plan of the same cost (a different KLL `k`, lifetime or sharing, say). Several plans can tie on cost,
and a change to the planner can move which one the selection returns; the
PR that moves it then updates the snapshots, instead of the next PR that
regenerates them picking up the change unexplained.

Without `STRICT_SNAPSHOTS`, as on a local run, a plan that changed at the
committed plan's cost (relative difference at most 1e-9), with the same
accepted accuracy, warnings and errors, passes and is noted on stderr.
Run with `STRICT_SNAPSHOTS=1` to check what the nightly run will.

## Other failures

The test also fails, instead of recording a snapshot, in these cases:

- A snapshot file has no matching checked query in `corpus.json`, or no
  matching workload in `manifest.json`.
- A sketch plan workload has no snapshot file.
- `asap_planner` failed.

## Adding queries

Add the query to `corpus.json` with a new `id` and regenerate. Before you
commit, run the check twice and confirm it passes each time. Planning has no
time limit, so a query that takes minutes in a release build slows every
nightly run; give such a query a `skip_reason`.
