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
| `promql-parser` | Apache-2.0 | [crates.io](https://crates.io/crates/promql-parser) (`GreptimeTeam/promql-parser`) | L1 PromQL parsing. |
| `serde` (+ derive) | MIT OR Apache-2.0 | crates.io | Serialization of the intent-algebra IR. |
| `serde_json` | MIT OR Apache-2.0 | crates.io | JSON (de)serialization in tests/IR. |
| `thiserror` | MIT OR Apache-2.0 | crates.io | Error types. |

Transitive dependencies pulled in by the above (notably the `lrpar` / `lrlex` /
`cfgrammar` parser-toolkit stack behind `promql-parser`, and `regex`) carry their
own licenses — overwhelmingly MIT and/or Apache-2.0. Regenerate the full set with
`cargo about generate` if a complete NOTICE bundle is needed for a release.

## `promql-parser`

`promql-parser` is consumed straight from crates.io (`promql-parser = "0.10"`).
We previously carried a private mirror (`ProjectASAP/promql-parser`, `asap`
branch) with local patches adding experimental functions
(`mad_over_time`, `first_over_time`, `ts_of_{first,last,max,min}_over_time`,
`histogram_quantiles`, `info`, `max_of`, `min_of`, `step`, `range`) ahead of
upstream. Upstream `GreptimeTeam/promql-parser` 0.10.0 shipped all of these, so
the fork is no longer needed. The one behavioral difference: upstream 0.10.0
also dropped the legacy `holt_winters` alias for
`double_exponential_smoothing` — ASAPController no longer accepts that
spelling either (see `crates/frontend-promql/src/promql.rs`).

No CI credentials or private git access are required to build.
