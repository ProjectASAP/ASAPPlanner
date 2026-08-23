"""Unit tests for render.py's data-merging and HTML-assembly logic --
deliberately exercised without py_mini_racer or a browser (see render.py's
own ad hoc py_mini_racer validation, not committed here, and PR #249's
description for how the shared viewer.js/node-style.js logic itself was
checked). These tests only cover what render.py adds on top of index.html:
merging input files and correctly inlining/embedding into one HTML page.

Run with (from the repo root): python3 -m unittest discover -s tools/dag-viewer -p 'test_render.py'
(or `cd tools/dag-viewer && python3 -m unittest test_render`, or
`python3 -m pytest tools/dag-viewer/` if pytest is available -- these are
plain `unittest.TestCase`s, any of those runners work). Plain
`python3 -m unittest tools/dag-viewer/test_render.py` does NOT work run
from the repo root: unittest resolves that path to a bare `test_render`
module without adding tools/dag-viewer/ to sys.path, so this file's own
`from render import ...` below fails with `ModuleNotFoundError: No module
named 'render'` -- discover's `-s` (or a plain module name run from inside
the directory) puts the right directory on sys.path instead. Not wired
into CI -- this repo doesn't run Python in CI at all today.
"""

from __future__ import annotations

import json
import re
import tempfile
import unittest
from pathlib import Path

from render import _json_script, load_workload, render

HERE = Path(__file__).resolve().parent


def named_graph(name: str, source: str = "SELECT 1") -> dict:
    """A minimal well-formed NamedGraph: one leaf Scan node, its own root."""
    return {
        "name": name,
        "source": source,
        "graph": {
            "nodes": [
                {
                    "id": 0,
                    "kind": "Scan",
                    "label": "Scan(t)",
                    "detail": {},
                    "children": [],
                    "hash": 1,
                }
            ],
            "root": 0,
        },
    }


class LoadWorkloadTests(unittest.TestCase):
    def test_merges_queries_across_files_in_order(self):
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f2 = Path(d) / "b.json"
            f1.write_text(json.dumps({"queries": [named_graph("q1")]}))
            f2.write_text(json.dumps({"queries": [named_graph("q2")]}))

            workload = load_workload([f1, f2])

        self.assertEqual([q["name"] for q in workload["queries"]], ["q1", "q2"])

    def test_colliding_name_is_disambiguated_by_source_filename(self):
        # Matches viewer.js's loadFiles(): a later file's query named the
        # same as an earlier one gets " (filename)" appended instead of
        # silently overwriting or erroring.
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f2 = Path(d) / "b.json"
            f1.write_text(json.dumps({"queries": [named_graph("q1")]}))
            f2.write_text(json.dumps({"queries": [named_graph("q1")]}))

            workload = load_workload([f1, f2])

        self.assertEqual(workload["queries"][0]["name"], "q1")
        self.assertEqual(workload["queries"][1]["name"], "q1 (b.json)")

    def test_non_colliding_names_pass_through_unchanged(self):
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f1.write_text(json.dumps({"queries": [named_graph("q1"), named_graph("q2")]}))

            workload = load_workload([f1])

        self.assertEqual([q["name"] for q in workload["queries"]], ["q1", "q2"])


class RenderTests(unittest.TestCase):
    def test_no_script_src_tags_remain(self):
        # Substring-only "<script src=" would also match viewer.js's own
        # header comment (which documents index.html's <script src=...>
        # usage in prose) once that file is inlined verbatim -- so match the
        # real tag shape instead of a bare substring.
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload, mode="single")
        self.assertNotRegex(html, r'<script src="[^"]+"></script>')

    def test_embedded_workload_round_trips(self):
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload, mode="single")

        m = re.search(
            r'<script type="application/json" id="embedded-workload">(.*?)</script>',
            html,
            re.S,
        )
        self.assertIsNotNone(m, "embedded-workload <script> tag not found")
        self.assertEqual(json.loads(m.group(1)), workload)

    def test_single_mode_adds_no_render_config(self):
        # Bare "__DAG_RENDER__" also matches viewer.js's own header comment
        # (which documents the window.__DAG_RENDER__ config object in prose)
        # once that file is inlined verbatim -- match the actual assignment
        # statement instead.
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload, mode="single")
        self.assertNotRegex(html, r"window\.__DAG_RENDER__ =")

    def test_non_single_mode_sets_render_config(self):
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload, mode="union")
        m = re.search(r"window\.__DAG_RENDER__ = (\{.*?\});", html)
        self.assertIsNotNone(m, "__DAG_RENDER__ assignment not found")
        self.assertEqual(json.loads(m.group(1)), {"mode": "union"})

    def test_embedded_data_placed_before_viewer_js_body(self):
        # viewer.js's top-level startup code reads #embedded-workload and
        # window.__DAG_RENDER__ synchronously as soon as it runs, so both
        # must appear earlier in the document than viewer.js's own inlined
        # <script> block.
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload, mode="union")
        embedded_pos = html.index('id="embedded-workload"')
        config_pos = html.index("__DAG_RENDER__ =")
        viewer_pos = html.index("cytoscape.use(window.cytoscapeDagre)")  # viewer.js's first line
        self.assertLess(embedded_pos, viewer_pos)
        self.assertLess(config_pos, viewer_pos)

    def test_angle_bracket_in_query_source_does_not_break_out_of_script_tag(self):
        # A pathological (but legal JSON) query source containing a literal
        # "</script>" substring must not prematurely close the embedded
        # <script type="application/json"> tag when the browser parses it.
        workload = {"queries": [named_graph("q1", source="SELECT '</script><script>evil()</script>'")]}
        html = render(workload, mode="single")

        m = re.search(
            r'<script type="application/json" id="embedded-workload">(.*?)</script>',
            html,
            re.S,
        )
        self.assertIsNotNone(m)
        parsed = json.loads(m.group(1))
        self.assertEqual(parsed["queries"][0]["source"], "SELECT '</script><script>evil()</script>'")


class JsonScriptTests(unittest.TestCase):
    def test_escapes_angle_bracket_but_stays_valid_json(self):
        encoded = _json_script({"x": "</script>"})
        self.assertNotIn("</script>", encoded)
        self.assertEqual(json.loads(encoded), {"x": "</script>"})


if __name__ == "__main__":
    unittest.main()
