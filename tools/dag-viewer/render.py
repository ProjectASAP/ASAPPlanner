#!/usr/bin/env python3
"""Bake a stage_pipeline document into a single, portable Pre/Post-ASAP HTML
page, with the same query selection, workload union, Stage 3 candidate table
and node details as index.html, but with the vendored JS libraries and the
document all inlined into one file.

Why this exists alongside index.html:
  - No server or drag-and-drop needed: open the output file and the data is
    already loaded, so it works headlessly — piped over SSH, attached to a
    bug report, etc.
  - index.html's own example load is a fetch(), which most browsers block
    as cross-origin when the page is opened via file://.

The document is embedded unchanged; stage-workload.js reads it in the page,
exactly as index.html does.

Usage:
  cargo run -p asap-devtools --bin stage_pipeline -- --promql "..." --out run.json
  python3 tools/dag-viewer/render.py run.json -o rendered.html
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
STAGE_PIPELINE_FORMAT = "asap-stage-pipeline/v1"

# The vendored libraries and the viewer's helpers load as plain
# <script src>; inlined verbatim in place. viewer.js is handled separately
# below since the embedded document must land immediately before it in
# document order (its startup code reads it synchronously).
_INLINE_LIBS = (
    "dagre.min.js",
    "cytoscape.min.js",
    "cytoscape-dagre.js",
    "node-style.js",
    "stage-workload.js",
    "planner-ui.js",
)


def check_document(document: object) -> dict:
    if not isinstance(document, dict) or document.get("format") != STAGE_PIPELINE_FORMAT:
        raise ValueError(f"not an {STAGE_PIPELINE_FORMAT} document (write one with stage_pipeline)")
    return document


def load_document(path: Path) -> dict:
    return check_document(json.loads(path.read_text()))


def _json_script(obj: object) -> str:
    """JSON-serialize `obj` for embedding inside an HTML <script> tag. Plain
    json.dumps output can legally contain a literal `</script` substring
    (e.g. inside a query's text) that would close the tag early when parsed
    as HTML, so `<` is escaped wherever it could start such a sequence."""
    return json.dumps(obj).replace("<", "\\u003c")


def render(document: dict) -> str:
    html = (HERE / "index.html").read_text()

    for name in _INLINE_LIBS:
        tag = f'<script src="{name}"></script>'
        if tag not in html:
            raise RuntimeError(f"render.py: expected to find {tag!r} in index.html — did it move or get renamed?")
        html = html.replace(tag, f"<script>\n{(HERE / name).read_text()}\n</script>", 1)

    embedded = f'<script type="application/json" id="embedded-workload">{_json_script(document)}</script>'
    viewer_tag = '<script src="viewer.js"></script>'
    if viewer_tag not in html:
        raise RuntimeError(f"render.py: expected to find {viewer_tag!r} in index.html — did it move or get renamed?")
    return html.replace(
        viewer_tag,
        f"{embedded}\n<script>\n{(HERE / 'viewer.js').read_text()}\n</script>",
        1,
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("file", nargs="?", type=Path, help="a stage_pipeline document; reads stdin if omitted")
    parser.add_argument("-o", "--output", type=Path, required=True, help="output HTML file path")
    args = parser.parse_args()

    try:
        document = load_document(args.file) if args.file else check_document(json.loads(sys.stdin.read()))
    except FileNotFoundError as err:
        print(f"render.py: {err.filename} — no such file (run stage_pipeline first?)", file=sys.stderr)
        sys.exit(1)
    except json.JSONDecodeError as err:
        print(f"render.py: {args.file or 'input'} isn't valid JSON — {err}", file=sys.stderr)
        sys.exit(1)
    except ValueError as err:
        print(f"render.py: {err}", file=sys.stderr)
        sys.exit(1)
    args.output.write_text(render(document))
    n = len(document.get("workload", {}).get("queries", []))
    print(f"wrote {args.output} ({n} quer{'y' if n == 1 else 'ies'})")


if __name__ == "__main__":
    main()
