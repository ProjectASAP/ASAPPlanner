# Planner snapshots

The planner has no reference answer to test against, so CI pins what it
produces. For each query in a corpus, the `stage_pipeline` document (default
search) is committed as a snapshot. A PR that changes any of these plans fails
the `corpus-snapshots` CI job until the snapshots are regenerated and the
change is explained.

## Layout

`crates/devtools/tests/corpus_snapshots/<corpus>/`:

- `corpus.json`: the inputs shared by the corpus's queries (`stage_pipeline_args`,
  the repeating-query entry and the `data_workload`), and the queries, each
  with a stable `id`. Each query is planned on its own.
- `<id>.json`: that query's snapshot. It is the `stage_pipeline` document,
  pretty-printed with sorted keys and without the search report's timings.

## When the check fails

The test names each changed query and its first differing line. CI uploads the
regenerated snapshots as the `corpus-snapshots` artifact.

If the change is intended, regenerate the snapshots and commit them:

```sh
UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored
```

Then list each changed query and why it changed under **Snapshot changes** in
the PR description. To inspect a plan, open the snapshot in
[`tools/dag-viewer`](../../tools/dag-viewer/README.md).

The test also fails, instead of recording a snapshot, in these cases:

- A query's search was ended by the wall-clock time limit. Its plan would then
  depend on machine speed.
- A snapshot file has no matching query in `corpus.json`.
- `stage_pipeline` crashed.

## Adding queries

Add the query to `corpus.json` with a new `id` and regenerate. Before you
commit, run the check twice, plus once on one core (`taskset -c 0`), and confirm
the snapshots stay identical. Keep each query under about 10 s and 500 MB with
a release build. Heavier queries belong in a separate, non-blocking job.
