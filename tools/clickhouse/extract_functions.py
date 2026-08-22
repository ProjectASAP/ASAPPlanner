#!/usr/bin/env python3
"""Dev-only discovery tool for issue #225, item 3.

Compares ClickHouse's own aggregate-function surface (`system.functions`)
against `asap_sql_function_catalog::CLICKHOUSE_BUILTINS`
(`crates/sql-function-catalog/src/lib.rs`) and reports the diff:

  * candidates -- ClickHouse aggregate names not yet in `CLICKHOUSE_BUILTINS`
    (things a maintainer might want to add support for)
  * possibly stale -- `CLICKHOUSE_BUILTINS` entries that no longer appear in
    ClickHouse's own list (e.g. renamed or removed upstream)

This is a *reporting* tool for a human to act on, not a code generator: it
never touches `crates/sql-function-catalog/src/lib.rs`. Existence (+ arity,
which ClickHouse's `system.functions` doesn't usefully expose per-overload)
is all the issue's own scope asks for -- deciding each new entry's
`RewriteKind`/semantic is still a judgment call for whoever reads the report.

Not wired into CI (see `tools/clickhouse/README.md`): this repo has no
ClickHouse service anywhere in its CI, and the project decided to keep it
that way. Run it locally instead.

Uses `chdb` (https://github.com/chdb-io/chdb) -- ClickHouse embedded as an
in-process Python library -- so there's no server to start: `pip install
chdb` is the only setup step. `chdb` is only imported inside
`fetch_clickhouse_aggregate_names`, so every other function here (the
catalog-source parsing, the diffing, the report formatting) runs and is
unit-testable without it installed.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# Relative to the repo root; matches the crate PR #227 introduced.
CATALOG_SRC_PATH = "crates/sql-function-catalog/src/lib.rs"

# ClickHouse's aggregate-combinator suffixes (-If, -Array, -Distinct, ...):
# https://clickhouse.com/docs/en/sql-reference/aggregate-functions/combinators
# apply to *any* base aggregate function and are never enumerated as their
# own `system.functions` row (there is no row named "countIf" -- only
# "count"). A `CLICKHOUSE_BUILTINS` entry like "countif" is therefore
# base name "count" + combinator "If", not a name ClickHouse lists on its
# own -- so a naive diff would misreport it as stale every single run. These
# are ordered longest-suffix-first so "-OrDefault"/"-OrNull" aren't shadowed
# by a shorter accidental match.
KNOWN_COMBINATOR_SUFFIXES = (
    "ordefault",
    "ornull",
    "distinct",
    "resample",
    "simplestate",
    "foreach",
    "state",
    "merge",
    "array",
    "if",
)


def parse_catalog_names(source: str) -> set[str]:
    """Extract every `name: "..."` inside the `CLICKHOUSE_BUILTINS` const
    array from `source` (the text of sql-function-catalog's `lib.rs`).

    Deliberately a small regex over the block, not a Rust parser -- the
    catalog's own `name` fields are plain string literals, and slicing out
    just the `CLICKHOUSE_BUILTINS` block first (rather than matching
    `name: "..."` over the whole file) keeps this from also picking up
    `NativeFunction` entries, which are a different table with a different
    meaning here.
    """
    start = source.find("pub const CLICKHOUSE_BUILTINS")
    if start == -1:
        raise ValueError(
            f"couldn't find `pub const CLICKHOUSE_BUILTINS` in {CATALOG_SRC_PATH} -- "
            "has it been renamed or moved?"
        )
    end = source.find("\n];", start)
    if end == -1:
        raise ValueError(
            "found `CLICKHOUSE_BUILTINS` but not its closing `];` -- "
            "malformed source or the array's formatting changed"
        )
    block = source[start:end]
    return set(re.findall(r'name:\s*"([a-z0-9_]+)"', block))


def load_catalog_names(repo_root: Path) -> set[str]:
    path = repo_root / CATALOG_SRC_PATH
    return parse_catalog_names(path.read_text())


def fetch_clickhouse_aggregate_names() -> set[str]:
    """Every aggregate function name ClickHouse's embedded `system.functions`
    lists (lowercased -- `CLICKHOUSE_BUILTINS` names are lowercase, and SQL
    function names are case-insensitive in ClickHouse anyway). Imports
    `chdb` lazily so the rest of this module works without it installed.
    """
    import chdb

    result = chdb.query(
        "SELECT name FROM system.functions WHERE is_aggregate = 1 ORDER BY name FORMAT JSON",
        "JSON",
    )
    payload = json.loads(str(result))
    return {row["name"].lower() for row in payload["data"]}


def combinator_base(name: str) -> str | None:
    """If `name` looks like `base + combinator suffix` (e.g. "countif" ->
    "count"), return `base`. Otherwise `None`. Only used to explain an
    apparently-stale catalog entry, never to invent new candidates -- a
    bare suffix match is too weak a signal to report as "ClickHouse has
    this function", only to note "this one isn't really missing".
    """
    for suffix in KNOWN_COMBINATOR_SUFFIXES:
        if name.endswith(suffix) and len(name) > len(suffix):
            return name[: -len(suffix)]
    return None


def diff(
    catalog_names: set[str], clickhouse_names: set[str]
) -> tuple[list[str], list[str], list[tuple[str, str]]]:
    """`(candidates, stale, combinator_derived)`.

    `candidates` -- ClickHouse aggregate names not in the catalog.
    `stale` -- catalog names ClickHouse's own list doesn't contain *and*
      that don't explain themselves as a combinator of a base ClickHouse
      still lists (e.g. an actual rename/removal upstream).
    `combinator_derived` -- catalog names ClickHouse's list doesn't contain
      directly, but which are a combinator of a base name it does list
      (e.g. ("countif", "count")) -- not stale, just not separately
      enumerated; reported for visibility, not as an action item.
    """
    candidates = sorted(clickhouse_names - catalog_names)
    missing = catalog_names - clickhouse_names
    stale: list[str] = []
    combinator_derived: list[tuple[str, str]] = []
    for name in sorted(missing):
        base = combinator_base(name)
        if base is not None and base in clickhouse_names:
            combinator_derived.append((name, base))
        else:
            stale.append(name)
    return candidates, stale, combinator_derived


def format_report(
    candidates: list[str], stale: list[str], combinator_derived: list[tuple[str, str]]
) -> str:
    """Diff-friendly (sorted, one name per line) plain-text report."""
    lines: list[str] = []
    lines.append(f"# ClickHouse aggregate-function catalog diff ({CATALOG_SRC_PATH})")
    lines.append("")
    lines.append(f"## Candidates -- in ClickHouse, not in CLICKHOUSE_BUILTINS ({len(candidates)})")
    lines.append("")
    if candidates:
        lines.extend(f"  {name}" for name in candidates)
    else:
        lines.append("  (none)")
    lines.append("")
    lines.append(f"## Possibly stale -- in CLICKHOUSE_BUILTINS, not in ClickHouse ({len(stale)})")
    lines.append("")
    if stale:
        lines.extend(f"  {name}" for name in stale)
    else:
        lines.append("  (none)")
    lines.append("")
    lines.append(
        "## Combinator-derived -- not separately enumerated by ClickHouse, "
        f"not actually missing ({len(combinator_derived)})"
    )
    lines.append("")
    if combinator_derived:
        lines.extend(f"  {name}  (= {base} + combinator)" for name, base in combinator_derived)
    else:
        lines.append("  (none)")
    lines.append("")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="repo root containing crates/sql-function-catalog (default: inferred from this file's location)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="write the report to this file instead of stdout",
    )
    args = parser.parse_args(argv)

    try:
        catalog_names = load_catalog_names(args.repo_root)
    except (OSError, ValueError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    try:
        clickhouse_names = fetch_clickhouse_aggregate_names()
    except ImportError:
        print(
            "error: chdb is not installed -- run `pip install chdb` first "
            "(see tools/clickhouse/README.md)",
            file=sys.stderr,
        )
        return 1

    candidates, stale, combinator_derived = diff(catalog_names, clickhouse_names)
    report = format_report(candidates, stale, combinator_derived)

    if args.output:
        args.output.write_text(report)
    else:
        print(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
