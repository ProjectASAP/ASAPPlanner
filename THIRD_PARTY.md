# Third-party software

ASAPController bundles third-party Rust crates. This file records the notable
direct dependencies, their licenses, and any attribution obligations. It is a
convenience summary, **not legal advice** — for an exhaustive, machine-generated
list (including transitive dependencies) run e.g. `cargo tree` or
[`cargo about`](https://github.com/EmbarkStudios/cargo-about).

All licenses below are OSI-approved and **permissive** (MIT / Apache-2.0); none
are copyleft. ASAPController itself is therefore not obligated to be open-sourced
on account of these dependencies.

## Direct dependencies

| Crate | License | Source | Notes |
|---|---|---|---|
| `promql-parser` | Apache-2.0 | **Private mirror** `ProjectASAP/promql-parser` of [`GreptimeTeam/promql-parser`](https://github.com/GreptimeTeam/promql-parser) | L1 PromQL parsing. See below. |
| `serde` (+ derive) | MIT OR Apache-2.0 | crates.io | Serialization of the intent-algebra IR. |
| `serde_json` | MIT OR Apache-2.0 | crates.io | JSON (de)serialization in tests/IR. |
| `thiserror` | MIT OR Apache-2.0 | crates.io | Error types. |

Transitive dependencies pulled in by the above (notably the `lrpar` / `lrlex` /
`cfgrammar` parser-toolkit stack behind `promql-parser`, and `regex`) carry their
own licenses — overwhelmingly MIT and/or Apache-2.0. Regenerate the full set with
`cargo about generate` if a complete NOTICE bundle is needed for a release.

## `promql-parser` — vendored Apache-2.0 mirror

`promql-parser` is consumed as a **git dependency on a private mirror**
(`ProjectASAP/promql-parser`) of the upstream Apache-2.0 project
`GreptimeTeam/promql-parser`, so we can carry local PromQL grammar/function
additions ahead of upstream releases. Branch layout:

- **`main`** — an untouched mirror of upstream `GreptimeTeam/promql-parser`.
- **`asap`** — our working branch; ASAPController's `crates/lower` manifest pins
  this branch. It currently adds the experimental functions present in
  Prometheus `promql/parser/functions.go` but missing upstream (`mad_over_time`,
  `first_over_time`, `ts_of_{first,last,max,min}_over_time`, `histogram_quantiles`,
  `info`, `max_of`, `min_of`, `step`, `range`). Modified files are marked per
  Apache-2.0 §4(b).

Apache-2.0 explicitly permits copying, modifying, **keeping modifications
private**, and commercial use, and it is **not copyleft**. The conditions in
§4 ("Redistribution") apply only when the software is **distributed outside the
organization**. If/when ASAPController (with this parser compiled in) is
distributed externally, retain the following:

- a copy of the **Apache-2.0 license** text (kept in the mirror as `LICENSE`);
- the upstream **`NOTICE`** file's attribution content, if present;
- original copyright / patent / attribution notices in the source; and
- a prominent note in **each file we modify** stating that it was changed
  (Apache-2.0 §4(b)).

Purely internal use (private mirror, internal builds, no external distribution)
carries essentially none of these obligations beyond keeping `LICENSE`/`NOTICE`
in the mirror, which the mirror already does.

### Keeping the mirror in sync with upstream

The mirror was created as a one-way copy (not a GitHub fork). `main` stays a
pristine upstream mirror; local edits live on `asap`. To pull future upstream
changes, refresh `main` then rebase `asap`:

```sh
# refresh the pristine mirror branch
git clone --bare https://github.com/GreptimeTeam/promql-parser.git
cd promql-parser.git
git push https://github.com/ProjectASAP/promql-parser.git +refs/heads/main:refs/heads/main
# then, in a normal clone: git checkout asap && git rebase main && git push --force-with-lease
```

### CI / build access

Because `promql-parser` is a private git dependency, any build needs read access
to `ProjectASAP/promql-parser`:

- **Local dev:** `gh auth setup-git` (uses your GitHub credentials).
- **CI:** the `rust.yml` workflow sets `CARGO_NET_GIT_FETCH_WITH_CLI=true` and
  configures a git credential helper from the secret **`CARGO_PRIVATE_GIT_TOKEN`**
  — add that org/repo secret (a fine-grained PAT or GitHub App token with read
  access to the mirror) or CI will fail to fetch the dependency. Until the secret
  exists, `rust.yml`'s auto-triggers are **disabled** (manual `workflow_dispatch`
  only); re-enable the commented `push` / `pull_request` triggers afterward.
