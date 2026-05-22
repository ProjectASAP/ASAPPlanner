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
additions ahead of upstream releases (see `docs/` and the `crates/lower` manifest).

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

The mirror was created as a one-way copy (not a GitHub fork). To pull future
upstream changes:

```sh
git clone --bare https://github.com/GreptimeTeam/promql-parser.git
cd promql-parser.git
git push --mirror https://github.com/ProjectASAP/promql-parser.git   # if main is unmodified
```

If local modifications live on `main`, keep upstream on a separate branch and
merge/rebase instead of mirror-pushing (which overwrites).
