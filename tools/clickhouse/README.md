# ClickHouse function-catalog extraction (issue #225, item 3)

`extract_functions.py` is a **dev-only, manual discovery tool**. It compares
ClickHouse's own aggregate-function list (`system.functions`) against
[`asap_sql_function_catalog::CLICKHOUSE_BUILTINS`](../../crates/sql-function-catalog/src/lib.rs)
and reports:

- **candidates** -- ClickHouse aggregate names not yet in `CLICKHOUSE_BUILTINS`
  (things worth considering adding),
- **possibly stale** -- `CLICKHOUSE_BUILTINS` entries that no longer appear in
  ClickHouse's own list (e.g. renamed or removed upstream), and
- **combinator-derived** -- catalog entries like `countif` that are really a
  base function (`count`) plus one of ClickHouse's
  [aggregate combinators](https://clickhouse.com/docs/en/sql-reference/aggregate-functions/combinators)
  (`-If`, `-Distinct`, `-Array`, ...), which ClickHouse doesn't enumerate as
  its own `system.functions` row -- reported separately so it isn't
  misreported as stale.

It **only reports**. It never edits `crates/sql-function-catalog/src/lib.rs` --
existence (+ arity) is all issue #225 asks the catalog to track, and deciding
each new entry's `RewriteKind` / canonical semantic is a human judgment call,
not something this script attempts.

## Why this isn't wired into CI

ClickHouse's function surface can only be introspected against an actual
ClickHouse (`system.functions` isn't documented data, it's a live catalog
table). This repo has no ClickHouse service anywhere in CI, and the project
decided deliberately not to add one just for this — see issue #225's own
"needs a decision on where such extraction tooling would actually run" open
question. So: a maintainer runs this locally, by hand, whenever they want to
check for drift or discover new candidates. It's never invoked automatically.

## Running it

This tool uses [`chdb`](https://github.com/chdb-io/chdb) -- ClickHouse
embedded as an in-process Python library. There's no server to start:

```sh
pip install chdb
python3 tools/clickhouse/extract_functions.py
```

Optional flags:

- `--repo-root PATH` -- repo root containing `crates/sql-function-catalog`
  (default: inferred from this script's own location).
- `--output PATH` -- write the report to a file instead of stdout.

Because `chdb` runs ClickHouse in-process rather than connecting to a
server, the report reflects whatever ClickHouse version the installed
`chdb` package embeds. `pip install --upgrade chdb` to check against a
newer ClickHouse release.

## Running its tests

The catalog-parsing and diff logic (everything except the actual `chdb`
query) has plain `unittest` coverage that needs no ClickHouse at all:

```sh
cd tools/clickhouse && python3 -m unittest test_extract_functions -v
```
