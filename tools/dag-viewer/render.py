#!/usr/bin/env python3
"""Bake WorkloadGraph JSON into a single, portable Pre/Post-ASAP HTML page,
with the same query selection, workload-union, and node details as index.html,
but with
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
highlighting is a proxy" section. Structured cost/benefit annotations
(issue #286, `CostAnnotation` in crates/types/src/cost.rs) pass through
this deep-copy untouched, same as everything else `prepare_workload` below
doesn't explicitly rewrite — viewer.js reads `decision.baseline_cost` /
`.selected_cost` / `.benefit`, `NamedGraph.workload_cost`, and
`DagGraph.edge_annotations` directly, with no help needed from this file.

Usage:
  cargo run -p asap-devtools --bin dag_export -- --sql "..." --name q1 \\
    | python3 tools/dag-viewer/render.py -o rendered.html

  python3 tools/dag-viewer/render.py dag1.json dag2.json -o rendered.html

Multiple input files are merged exactly like index.html's multi-file drop: a
query name colliding with an earlier one is disambiguated by suffixing the
source filename.
"""

from __future__ import annotations

import argparse
import copy
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# The vendored libraries + node-style.js load as plain <script src>; inlined
# verbatim in place. viewer.js is handled separately below since the
# embedded data and mode override must land immediately before it in
# document order (its startup code reads both synchronously as soon as it
# runs) — see viewer.js's header comment.
_INLINE_LIBS = ("dagre.min.js", "cytoscape.min.js", "cytoscape-dagre.js", "node-style.js", "planner-ui.js")


def _compact(value: object) -> str:
    """Render one IR value compactly enough to fit inside a DAG node."""
    if value is None:
        return "none"
    if isinstance(value, bool):
        return str(value).lower()
    if isinstance(value, str):
        return value
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return ", ".join(_compact(item) for item in value) or "none"
    if not isinstance(value, dict):
        return str(value)

    # Common serde enum/newtype shapes in QueryExpr detail.
    if set(value) == {"Column"}:
        return f"col[{_compact(value['Column'])}]"
    if set(value) == {"Table"} and isinstance(value["Table"], dict):
        return value["Table"].get("table_ref", _compact(value["Table"]))
    if set(value) == {"TimeSeries"} and isinstance(value["TimeSeries"], dict):
        return value["TimeSeries"].get("metric", _compact(value["TimeSeries"]))
    if len(value) == 1:
        tag, payload = next(iter(value.items()))
        if payload is None:
            return tag
        return f"{tag}({_compact(payload)})"
    return ", ".join(f"{key}={_compact(item)}" for key, item in value.items())


def _column(value: object, input_schema: object = None) -> str:
    if isinstance(value, int):
        if isinstance(input_schema, dict):
            columns = input_schema.get("columns")
            if isinstance(columns, list) and value < len(columns):
                column = columns[value]
                if isinstance(column, dict) and column.get("name"):
                    return str(column["name"])
        return f"col[{value}]"
    return _compact(value)


def _measure(value: object, input_schema: object = None) -> str:
    if not isinstance(value, dict):
        return _compact(value)
    kind = str(value.get("kind", "aggregate"))
    args = []
    if value.get("col") is not None:
        args.append(_column(value["col"], input_schema))
    for key in ("q", "k", "population", "label", "lower", "upper"):
        if key in value:
            args.append(f"{key}={_compact(value[key])}")
    return f"{kind}({', '.join(args)})" if args else f"{kind}()"


def _bounded_lines(lines: list[str], maximum: int = 4, width: int = 64) -> str:
    """Keep graph boxes scannable; the sidebar owns the lossless detail."""
    shortened = [line if len(line) <= width else line[: width - 1] + "…" for line in lines]
    if len(shortened) > maximum:
        shortened = shortened[: maximum - 1] + [f"… +{len(shortened) - maximum + 1} more"]
    return "\n".join(shortened)


def _grouping(reduction: object, input_schema: object = None) -> str | None:
    if reduction == "PerEntity":
        return "per entity"
    if not isinstance(reduction, dict) or "Reduce" not in reduction:
        return None
    keys = reduction["Reduce"]
    if isinstance(keys, dict) and "without" in keys:
        return "group without " + ", ".join(_column(key, input_schema) for key in keys["without"])
    if isinstance(keys, list):
        return "group by " + (", ".join(_column(key, input_schema) for key in keys) or "all rows")
    return "group by " + _compact(keys)


def _sort_key(value: object) -> str:
    if not isinstance(value, dict):
        return _compact(value)
    expr = _compact(value.get("expr"))
    direction = "ascending" if value.get("ascending", True) else "descending"
    nulls = "nulls first" if value.get("nulls_first", False) else "nulls last"
    return f"{expr} {direction}, {nulls}"


def _semantic_label(node: dict, input_schema: object = None) -> str:
    """Build a box label from a node's concrete IR fields, not its summary."""
    kind = str(node.get("kind", "Node"))
    detail = node.get("detail") or {}
    if not isinstance(detail, dict):
        return f"{kind}\n{_compact(detail)}"

    lines = [kind]
    if kind == "Scan":
        source = detail.get("source")
        if source is None:
            # Older/demo fixtures sometimes kept the source only in the
            # exporter label ("Scan(metrics)") and left detail empty.
            match = re.fullmatch(r"Scan\(([^)]+)\)(?:\s+.*)?", str(node.get("label", "")))
            source = match.group(1) if match else "unknown"
        lines.append(f"source: {_compact(source)}")
        if detail.get("predicates"):
            lines.append(f"where: {_compact(detail['predicates'])}")
    elif kind == "Aggregate":
        measures = detail.get("measures") or []
        lines.extend(f"measure: {_measure(measure, input_schema)}" for measure in measures)
        grouping = _grouping(detail.get("reduction"), input_schema)
        if grouping:
            lines.append(grouping)
        if detail.get("having"):
            lines.append(f"having: {_compact(detail['having'])}")
    elif kind == "Sort":
        lines.extend(f"sort: {_sort_key(key)}" for key in detail.get("keys") or [])
        if detail.get("partition_by"):
            lines.append(f"within: {_compact(detail['partition_by'])}")
    elif kind == "Project":
        output_schema = node.get("schema")
        output_columns = output_schema.get("columns") if isinstance(output_schema, dict) else None
        if isinstance(output_columns, list) and output_columns:
            names = [str(column.get("name", "?")) for column in output_columns if isinstance(column, dict)]
            lines.append("columns: " + ", ".join(names))
        else:
            lines.append(f"columns: {len(detail.get('cols') or [])}")
    elif kind == "Filter":
        lines.append(f"where: {_compact(detail.get('pred'))}")
    elif kind == "Join":
        lines.append(f"type: {_compact(detail.get('kind'))}")
        if detail.get("pred"):
            lines.append(f"on: {_compact(detail['pred'])}")
    elif kind == "Limit":
        lines.append(f"rows: {_compact(detail.get('n'))}")
        if detail.get("offset"):
            lines.append(f"offset: {_compact(detail['offset'])}")
    elif kind == "BinaryOp":
        lines.append(f"operation: {_compact(detail.get('op'))}")
        if detail.get("vector_match"):
            lines.append(f"match: {_compact(detail['vector_match'])}")
    elif kind == "SummaryAgg":
        lines.append(f"family: {_compact(detail.get('family'))}")
        if "col" in detail:
            lines.append(f"input: {_column(detail['col'])}")
        reduction = _grouping(detail.get("reduction"), input_schema)
        if reduction:
            lines.append(reduction)
        if "grouping" in detail:
            lines.append(f"summary layout: {_compact(detail['grouping'])}")
    elif kind == "SummaryJoin":
        lines.append(f"family: {_compact(detail.get('family'))}")
        for key in ("key", "grouping"):
            if key in detail:
                lines.append(f"{key}: {_compact(detail[key])}")
    elif kind == "SummaryEstimate":
        lines.append(f"query: {_compact(detail.get('query'))}")
    elif kind == "SummaryDelete":
        lines.append(f"key: {_compact(detail.get('key'))}")
    elif kind == "KeepPreAsap":
        nested = detail.get("pre_asap_subgraph")
        nested_nodes = nested.get("nodes", []) if isinstance(nested, dict) else []
        nested_root = nested.get("root") if isinstance(nested, dict) else None
        root = next((item for item in nested_nodes if item.get("id") == nested_root), None)
        lines.append(f"unchanged: {root.get('kind', 'pre-ASAP subtree') if root else 'pre-ASAP subtree'}")
    else:
        # Less common variants still show their own scalar IR fields. Avoid
        # schema/subgraph blobs, which belong in the click-to-inspect panel.
        for key, value in detail.items():
            if key not in {"schema", "pre_asap_subgraph"} and value not in (None, [], {}):
                lines.append(f"{key}: {_compact(value)}")

    return _bounded_lines(lines)


def prepare_workload(workload: dict) -> dict:
    """Copy a workload and replace every graph label with readable IR text."""
    prepared = copy.deepcopy(workload)

    def prepare_graph(graph: object) -> None:
        if not isinstance(graph, dict):
            return
        by_id = {node.get("id"): node for node in graph.get("nodes", [])}
        for node in graph.get("nodes", []):
            child = by_id.get((node.get("children") or [None])[0])
            input_schema = (
                child.get("schema") or (child.get("detail") or {}).get("schema")
                if isinstance(child, dict)
                else None
            )
            node["label"] = _semantic_label(node, input_schema)
            nested = (node.get("detail") or {}).get("pre_asap_subgraph")
            prepare_graph(nested)

    for query in prepared.get("queries", []):
        prepare_graph(query.get("graph"))
        prepare_graph(query.get("post_graph"))
    return prepared


def load_workload(paths: list[Path]) -> dict:
    """Merge one or more WorkloadGraph JSON files into one, matching
    viewer.js's loadFiles(): a query name colliding with an earlier one is
    disambiguated by suffixing the source filename."""
    queries = []
    seen_names = set()
    for path in paths:
        data = json.loads(path.read_text())
        incoming = data.get("queries", [])
        if not incoming and isinstance(data.get("graph"), dict) and isinstance(data.get("deployments"), list):
            incoming = [{
                "name": path.stem or "Summary maintenance plan",
                "graph": data["graph"],
                "post_graph": data["graph"],
                "lifecycle_plan": True,
                "lifecycle_summary": {
                    "selected_raw_recompute": data.get("selected_raw_recompute", False),
                    "summary_total_cost": data.get("summary_total_cost"),
                    "raw_recompute_total_cost": data.get("raw_recompute_total_cost"),
                    "horizon_seconds": data.get("horizon_seconds"),
                    "evaluation_rate_per_second": data.get("evaluation_rate_per_second"),
                    "update_rate_per_second": data.get("update_rate_per_second"),
                    "expected_reads": data.get("expected_reads"),
                    "deployment_count": len(data["deployments"]),
                },
            }]
        for q in incoming:
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


def render(workload: dict) -> str:
    workload = prepare_workload(workload)
    html = (HERE / "index.html").read_text()

    for name in _INLINE_LIBS:
        tag = f'<script src="{name}"></script>'
        if tag not in html:
            raise RuntimeError(f"render.py: expected to find {tag!r} in index.html — did it move or get renamed?")
        html = html.replace(tag, f"<script>\n{(HERE / name).read_text()}\n</script>", 1)

    embedded = f'<script type="application/json" id="embedded-workload">{_json_script(workload)}</script>'
    viewer_tag = '<script src="viewer.js"></script>'
    if viewer_tag not in html:
        raise RuntimeError(f"render.py: expected to find {viewer_tag!r} in index.html — did it move or get renamed?")
    html = html.replace(
        viewer_tag,
        f"{embedded}\n<script>\n{(HERE / 'viewer.js').read_text()}\n</script>",
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
    args = parser.parse_args()

    try:
        workload = load_workload(args.files) if args.files else json.loads(sys.stdin.read())
    except FileNotFoundError as err:
        print(f"render.py: {err.filename} — no such file (run dag_export first?)", file=sys.stderr)
        sys.exit(1)
    except json.JSONDecodeError as err:
        source = args.files[0] if len(args.files) == 1 else "input"
        print(f"render.py: {source} isn't valid JSON — {err}", file=sys.stderr)
        sys.exit(1)
    if not workload.get("queries"):
        print("render.py: no queries in input — nothing to render", file=sys.stderr)
        sys.exit(1)
    args.output.write_text(render(workload))
    n = len(workload["queries"])
    print(f"wrote {args.output} ({n} quer{'y' if n == 1 else 'ies'})")


if __name__ == "__main__":
    main()
