# ASAP query DAG viewer

Interactive viewer for the pre-ASAP query IR (`QueryExpr`), for manual IR
review/debugging, eyeballing common shapes across a corpus, and spotting shared sub-DAGs across queries.

## Generate a graph

```sh
cargo run -p asap-devtools --bin dag_export -- \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  --sql "SELECT service, AVG(latency) FROM metrics GROUP BY service" --name q2 \
  --promql "topk(5, rate(http_requests_total[5m]))" --name q3 \
  > /tmp/dag.json
```

`--name` is optional (defaults to `q<n>`). Repeat `--sql`/`--promql` for as
many queries as you want in one file — they all land in one `WorkloadGraph`.
You can also load several separate JSON files into the viewer at once (drag
multiple files, or run the exporter more than once); their queries are merged.
Add `--epsilon <f64>` to also populate `notes` with sketch-approximation
findings — see "Replacement explanations (`notes`)" below.

## Open the viewer

This is a static page with no server or build step — open `index.html`
directly in a browser (`file://tools/dag-viewer/index.html`), or serve the
directory (`python3 -m http.server` from here) if your browser blocks local
JS module loads. Drop the generated JSON onto the page, or use the file
picker.

- Click a node to inspect its full field detail in the side panel.
- Multiple loaded queries show up as tabs across the top ("Single" mode,
  the default). Nodes that are structurally identical to a node in *another*
  loaded query get a blue ring — toggle this off with "Highlight shared
  subtrees".
- Each query's root node (its final output) gets a small red badge.
- A node whose `notes` array is non-empty gets a small colored badge in the
  bottom-right corner — click the node to see each note's kind and reason in
  the side panel. See "Replacement explanations (`notes`)" below.
- Node color/icon is driven by category — see the legend in the side panel,
  or `node-style.js` for the underlying `kind -> category` table.
- Once two or more queries are loaded, switch to **Compare** or **Union**
  mode (buttons next to the drop zone) to see them together instead of one
  at a time — see below.

## Render a standalone page from Python

`index.html` needs a browser attached — dropping a file onto it, or a
sibling `dag.json` it can `fetch()`. Neither works if you're generating a
graph from an environment with no browser to look at it in (piped over SSH,
a CI job, an agent session): `render.py` bakes a `dag_export` JSON straight
into a single portable HTML file instead, with the query data and every
vendored script inlined — open the output directly, nothing else to fetch:

```sh
cargo run -p asap-devtools --bin dag_export -- --sql "..." --name q1 \
  | python3 tools/dag-viewer/render.py -o rendered.html

# or from files already on disk, opening straight into Union mode:
python3 tools/dag-viewer/render.py dag1.json dag2.json -o rendered.html --mode union
```

It's the exact same UI as `index.html` — `--mode` just pre-selects every
loaded query and switches the page's *initial* view; Single/Compare/Union,
click-to-inspect, and the highlight toggle all still work after it opens.
`index.html` and `render.py`'s output share one copy of the interaction
logic (`viewer.js`) and the category table (`node-style.js`), so anything
below in this doc — the mode descriptions, the shared-hash caveats — applies
to both equally.

Node/edge transitions (mode switches, the shared-subtree ring, layout on
load) are animated in both — see `viewer.js`'s `LAYOUT_ANIMATION` and the
`transition-property` entries in `buildCyStyle()` if you want to retune or
disable that.

Per-node cost isn't rendered anywhere in either page — `dag_export` doesn't
emit one today (no cost estimator is wired into pre-ASAP IR yet). The side
panel already dumps a clicked node's full `detail` verbatim, so a future
`cost` field added there would show up with no viewer change needed.

## Visual style and node categories

Node colors, icons, and shapes are adapted from
[`ProjectASAP/bgp-query-dag-explorer`](https://github.com/ProjectASAP/bgp-query-dag-explorer)'s
visual language, applied to the real `QueryExpr` IR instead of that repo's
hardcoded BGP query set. `tools/dag-viewer/node-style.js` is the single
source of truth for which of `QueryExpr`'s ~24 node kinds belongs to which of
the 9 categories (data / filter / derive / aggregate / window / join / set /
sort / bind) — edit that file to reclassify a kind or retune the palette;
nothing else in `index.html` needs to change.

## Compare and Union mode

Once two or more queries are loaded, the mode toggle next to the drop zone
switches between three views:

- **Single** (default) — one query's DAG at a time, selected via the tabs.
  Unchanged from before.
- **Compare** — every checked query gets its own lane (a dashed box titled
  with the query's name), laid out side by side. A dashed line connects
  nodes across lanes whenever their structural hash matches, so "what's
  shared vs. query-specific" is a line you can trace instead of a ring you
  have to hunt for one node at a time.
- **Union** — the checked queries are merged into a single graph. Any node
  whose hash is shared by two or more of them is drawn once, and edges from
  every query that reaches that node converge onto it, instead of each
  query drawing its own disconnected copy of the shared subtree. A merged
  node gets a double border; the side panel lists which queries it's
  present in (and, if it's more than one query's root, all of them).

Switching into Compare or Union the first time selects every currently
loaded query by default; use the checkboxes in the tab bar to narrow it
down (each mode needs at least two selected, or it shows a hint instead of
an empty graph). The selection is remembered when you switch back to
Single and later back to Compare/Union.

Branching is the normal case here, not an edge case: `QueryExpr` graphs have
real branching at arbitrary depth (`Merge`/`Concat` with N children,
`Join`/`SetOp`/`BinaryOp` with two, `LetBinding` with two structurally
different children), and Union mode's merge can give a single node several
parents at once — e.g. two queries whose `Aggregate` differs but whose
underlying `Scan` is identical converge two different parents onto that one
`Scan` node. `dagre` lays out multi-parent DAGs natively, so no special
casing was needed for that beyond building the merged node/edge set
correctly. Compare mode's lanes use `cytoscape.js`'s compound-node support
(each lane is a parent node containing that query's nodes); the dashed
cross-lane links are added to the graph *after* the per-lane layout runs,
so they're purely visual and never distort which lane a node lands in.

This is the same lane-based interaction model as the reference repo
mentioned in [issue #186](https://github.com/ProjectASAP/ASAPPlanner/issues/186)
(`ProjectASAP/bgp-query-dag-explorer`), adapted for real DAGs: that repo's
layout assumes each query is a flat, linear list of `steps`, which doesn't
carry over as-is once a query can branch.

### "Shared" here means matching hash, not real CSE

Compare and Union mode build on the exact same signal as the single-view
highlighting described below: a node is "shared"/"merged" if its structural
hash matches a node in another selected query. It is **not** real `Rc`
identity and **not** the output of a real common-subexpression-elimination
pass — see the next section for why, and don't read the UI's "shared" /
"merged" language as claiming otherwise.

## Shared-subtree highlighting is a real-hash proxy, not real CSE (yet)

The highlight (and Compare/Union mode's notion of "shared") is computed by
matching each node's `hash` across queries (a node is "shared" once its
hash shows up under ≥ 2 distinct query names).
That `hash` is no longer a viewer-only reimplementation: as of issue #223
stage 3, `dag_export`'s per-node `hash` is computed by calling
`asap_types::pre_asap::cse::structural_hash` directly — the exact same
function, on the exact same input, that
`asap_types::pre_asap::cse::share_common_subtrees`'s `InternTable` uses to
bucket its own merge candidates (`crates/types/src/dag_export.rs`,
`crates/types/src/pre_asap/cse.rs`). Two nodes with equal `hash` here really
are exactly the pair `InternTable::intern` would go on to run its
`PartialEq` check against.

It's still a **proxy**, though, for two independent reasons — a hash match
here does not by itself mean `share_common_subtrees` ran and actually merged
those nodes onto one `Rc`:

1. **The `PartialEq` + legality gate isn't re-run.** `structural_hash` is
   deliberately only a coarse bucketing filter (hash collisions are
   possible, and never disambiguated here); the viewer trusts a hash match
   as "structurally identical" without also re-checking `PartialEq` or
   `Schema::has_unique_key()` the way `InternTable::intern` does before
   actually sharing an `Rc`. A node with no provable unique key (e.g. an
   ungrouped `Aggregate`) can still show up highlighted here even though
   real CSE would never hoist it.
2. **No real `Rc` identity crosses this tool's process boundaries.** Each
   `--sql`/`--promql` query given to the `dag_export` binary is lowered and
   exported independently — `share_common_subtrees` is never actually
   invoked in this path — and the viewer's own multi-file-load feature
   merges JSON produced by entirely separate `dag_export` invocations (or
   even separate machines/times). There is no `Rc<QueryExpr>` for the
   viewer to compare pointer identity on; hash equality is the only signal
   available to it, by construction, regardless of how the hash is
   computed.

Closing this fully — real `Rc::ptr_eq`-based highlighting reflecting an
actual `share_common_subtrees` run — would mean threading `Rc` pointer
identity from a single in-process `share_common_subtrees` call through
`dag_export`'s node-flattening and into the exported JSON (a new field
alongside `hash`), and having the `dag_export` binary actually call
`share_common_subtrees` across all queries given in one invocation before
exporting. That's a reasonable follow-up but a materially bigger change than
this hash-unification step (new export API, binary changes, and a viewer
highlighting-logic change) and orthogonal to it, so it's left for a future
issue rather than folded into #223 stage 3.

## Replacement explanations (`notes`)

Each `DagNode` in the exported JSON carries a `notes` field:

```jsonc
"notes": [
  { "kind": "SketchApproximation", "reason": "quantile(q=0.99) realizes as a Kll sketch — …" }
]
```

`notes` is `Vec<DagNote>` (`crates/types/src/dag_export.rs`), omitted from the
JSON entirely when empty — which is the common case, and always true unless
the export was run with `--epsilon` (see below). `asap_types::dag_export`
itself never populates this field; it exists purely as a layering seam so a
*higher* crate can annotate an already-exported graph without `asap_types`
knowing anything about that crate's concepts (`asap_types` is a lower crate
`asap-aware-mapping` depends on, never the reverse).

The one populator today is `dag_export`'s own devtools binary
(`crates/devtools/src/bin/dag_export.rs`): after exporting each query, it
calls `asap_aware_mapping::explain_replacements` (issue #257) on the same
`QueryExpr` and, for every `ReplacementExplanation` returned, finds the
`DagNode` candidates whose `hash` equals the explanation's `node_hash`, then
confirms structural equality between the node's in-process source expression
and the explanation target before pushing a `DagNote { kind, reason }`. The
hash is a narrowing filter, not identity; the equality check makes annotation
matching collision-safe.
`kind` is the `Debug` form of `asap_aware_mapping::ExplanationKind`
(`"SketchApproximation"` or `"CommonSubexpressionReuse"` today,
`#[non_exhaustive]` — a future variant just shows up as its own tag, no
viewer change required); `reason` is that explanation's own rationale text,
verbatim.

`SketchApproximation` notes only appear when the export ran with an
approximate accuracy target: `dag_export`'s default is
`AccuracyTarget::Exact` (nothing to approximate, so nothing to report), so
you need the exporter's own `--epsilon <f64>` flag to see one, e.g.:

```sh
cargo run -p asap-devtools --bin dag_export -- \
  --epsilon 0.01 --sql "SELECT quantile(0.99, latency) FROM metrics" --name p99 \
  > /tmp/dag.json
```

`CommonSubexpressionReuse` notes don't need `--epsilon`: they come from a
subtree repeated within one query (e.g. the same branch appearing on both
sides of a `BinaryOp`), which `share_common_subtrees` can detect regardless
of accuracy target.

In the viewer, a node with non-empty `notes` gets a small colored badge in
its bottom-right corner (color keyed to `kind` — see `node-style.js`'s
`NOTE_KIND_COLOR`); click the node to see each note's kind and full reason
text in the side panel, the same "click for detail" pattern the rest of the
side panel already uses for a node's `detail`/root/shared-subtree status.

## Vendored dependencies

`cytoscape.min.js`, `dagre.min.js`, and `cytoscape-dagre.js` are vendored here
(not CDN-fetched) so the page works fully offline and doesn't drift with
upstream releases. All three are MIT-licensed:
- <https://github.com/cytoscape/cytoscape.js>
- <https://github.com/dagrejs/dagre>
- <https://github.com/cytoscape/cytoscape.js-dagre>
