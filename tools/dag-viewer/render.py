#!/usr/bin/env python3
"""Bake one or more dag_export WorkloadGraph JSON files into a single,
portable HTML page — the same tools/dag-viewer UI (Single/Compare/Union,
click-to-inspect, shared-subtree/CSE highlighting) as index.html, but with
the vendored JS libraries and the query data all inlined into one file.

Why this exists alongside index.html:
  - No server or drag-and-drop needed: open the output file and the data is
    already loaded, so it works headlessly — piped over SSH, opened in an
    environment with no attached browser to drive interactively, attached to
    a bug report, etc.
  - index.html's own "auto-load a sibling dag.json" convenience is a
    fetch(), which most browsers block as cross-origin when the page is
    opened via file:// — embedding the JSON directly sidesteps that too, not
    just the no-server case.

This does not add anything index.html doesn't already do — it shares
viewer.js and node-style.js with it verbatim (see viewer.js's header
comment) and only differs in packaging: one query's worth of exported
`QueryExpr` detail *is* its plan (see the side panel on node click), and
shared-hash highlighting *is* what this repo has for CSE today — both a
hash-based proxy, not real CSE output; see README.md's "Shared-subtree
highlighting is a proxy" section. Per-node cost isn't in dag_export's output
yet (no cost estimator is wired into pre-ASAP IR), so there's nothing here
to render for it; if `detail` ever grows a `cost` field, the side panel
shows it automatically since it dumps `detail` verbatim, no viewer change
needed.

Usage:
  cargo run -p asap-devtools --bin dag_export -- --sql "..." --name q1 \\
    | python3 tools/dag-viewer/render.py -o rendered.html

  python3 tools/dag-viewer/render.py dag1.json dag2.json -o rendered.html --mode union

Multiple input files are merged exactly like index.html's multi-file drop: a
query name colliding with an earlier one is disambiguated by suffixing the
source filename.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# The vendored libraries + node-style.js load as plain <script src>; inlined
# verbatim in place. viewer.js is handled separately below since the
# embedded data and mode override must land immediately before it in
# document order (its startup code reads both synchronously as soon as it
# runs) — see viewer.js's header comment.
_INLINE_LIBS = ("dagre.min.js", "cytoscape.min.js", "cytoscape-dagre.js", "node-style.js")


def load_workload(paths: list[Path]) -> dict:
    """Merge one or more WorkloadGraph JSON files into one, matching
    viewer.js's loadFiles(): a query name colliding with an earlier one is
    disambiguated by suffixing the source filename."""
    queries = []
    seen_names = set()
    for path in paths:
        data = json.loads(path.read_text())
        for q in data.get("queries", []):
            name = q["name"]
            if name in seen_names:
                name = f'{q["name"]} ({path.name})'
            seen_names.add(name)
            queries.append({**q, "name": name})
    return {"queries": queries}


def _json_script(obj: object) -> str:
    """JSON-serialize `obj` for embedding inside an HTML <script> tag. Plain
    json.dumps output can legally contain a literal `</script` substring
    (e.g. inside a SQL/PromQL query string on NamedGraph.source) that would
    close the tag early when parsed as HTML, so `<` is escaped wherever it
    could start such a sequence."""
    return json.dumps(obj).replace("<", "\\u003c")


def render(workload: dict, mode: str) -> str:
    html = (HERE / "index.html").read_text()

    for name in _INLINE_LIBS:
        tag = f'<script src="{name}"></script>'
        if tag not in html:
            raise RuntimeError(f"render.py: expected to find {tag!r} in index.html — did it move or get renamed?")
        html = html.replace(tag, f"<script>\n{(HERE / name).read_text()}\n</script>", 1)

    embedded = f'<script type="application/json" id="embedded-workload">{_json_script(workload)}</script>'
    config = f"<script>window.__DAG_RENDER__ = {_json_script({'mode': mode})};</script>\n" if mode != "single" else ""
    viewer_tag = '<script src="viewer.js"></script>'
    if viewer_tag not in html:
        raise RuntimeError(f"render.py: expected to find {viewer_tag!r} in index.html — did it move or get renamed?")
    html = html.replace(
        viewer_tag,
        f"{embedded}\n{config}<script>\n{(HERE / 'viewer.js').read_text()}\n</script>",
        1,
    )
    return html


def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "files",
        nargs="*",
        type=Path,
        help="dag_export WorkloadGraph JSON file(s); reads stdin if none are given",
    )
    parser.add_argument("-o", "--output", type=Path, required=True, help="output HTML file path")
    parser.add_argument(
        "--mode",
        choices=["single", "compare", "union"],
        default="single",
        help="view mode the page opens into; compare/union pre-select every loaded query (default: single)",
    )
    args = parser.parse_args()

    workload = load_workload(args.files) if args.files else json.loads(sys.stdin.read())
    if not workload.get("queries"):
        print("render.py: no queries in input — nothing to render", file=sys.stderr)
        sys.exit(1)
    if args.mode != "single" and len(workload["queries"]) < 2:
        print(
            f"render.py: --mode {args.mode} needs 2+ queries to do anything "
            f"(got {len(workload['queries'])}) — the page will just show its 'select more' hint",
            file=sys.stderr,
        )

    args.output.write_text(render(workload, args.mode))
    n = len(workload["queries"])
    print(f"wrote {args.output} ({n} quer{'y' if n == 1 else 'ies'})")


if __name__ == "__main__":
    main()
