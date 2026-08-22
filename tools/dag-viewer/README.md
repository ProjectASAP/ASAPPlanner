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
- Node color/icon is driven by category — see the legend in the side panel,
  or `node-style.js` for the underlying `kind -> category` table.
- Once two or more queries are loaded, switch to **Compare** or **Union**
  mode (buttons next to the drop zone) to see them together instead of one
  at a time — see below.

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

## Shared-subtree highlighting is a proxy, not real CSE

The highlight (and Compare/Union mode's notion of "shared") is computed by
hashing each node's `(kind, detail, children)` bottom-up and matching hashes
across queries — see the doc comment on `crates/types/src/dag_export.rs`.
This is **not** the same guarantee a real CSE pass gives:

1. **No `PartialEq` + legality re-check.** A real CSE pass (once one exists
   and is wired into an end-to-end multi-root planning path — see
   `asap_types::pre_asap::cse::share_common_subtrees` and issue #223) follows
   a hash match with a full `PartialEq` check and a legality gate (e.g. can
   this actually be hoisted without changing semantics) before treating two
   subtrees as the same. The viewer only has the hash, so a hash collision
   or a node that hashes alike but isn't legally shareable would still show
   up as "shared"/"merged" here.
2. **No real `Rc` identity crosses this tool's process boundary.** Each
   query given to the `dag_export` binary is lowered and exported
   independently, and the viewer's multi-file-load feature merges JSON
   produced by entirely separate invocations. There's no `Rc<QueryExpr>` for
   the viewer to compare pointer identity on — hash equality is the only
   signal available to it, by construction, regardless of how the hash
   itself is computed.

If/when real CSE output (a `CseWorkloadPlan`'s `bindings`/`Ref`s) is
available from an end-to-end planning path, Union mode's converging-edges
view is a natural place to render that directly instead of (or alongside)
this hash-based proxy.

## Vendored dependencies

`cytoscape.min.js`, `dagre.min.js`, and `cytoscape-dagre.js` are vendored here
(not CDN-fetched) so the page works fully offline and doesn't drift with
upstream releases. All three are MIT-licensed:
- <https://github.com/cytoscape/cytoscape.js>
- <https://github.com/dagrejs/dagre>
- <https://github.com/cytoscape/cytoscape.js-dagre>
