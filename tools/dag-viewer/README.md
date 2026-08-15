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
- Multiple loaded queries show up as tabs across the top.
- Nodes that are structurally identical to a node in *another* loaded query
  get a blue ring — toggle this off with "Highlight shared subtrees".
- Each query's root node (its final output) gets a small red badge.
- Node color/icon is driven by category — see the legend in the side panel,
  or `node-style.js` for the underlying `kind -> category` table.

## Visual style and node categories

Node colors, icons, and shapes are adapted from
[`ProjectASAP/bgp-query-dag-explorer`](https://github.com/ProjectASAP/bgp-query-dag-explorer)'s
visual language, applied to the real `QueryExpr` IR instead of that repo's
hardcoded BGP query set. `tools/dag-viewer/node-style.js` is the single
source of truth for which of `QueryExpr`'s ~24 node kinds belongs to which of
the 9 categories (data / filter / derive / aggregate / window / join / set /
sort / bind) — edit that file to reclassify a kind or retune the palette;
nothing else in `index.html` needs to change.

## Future work

Compare/Union modes that lay multiple queries out in shared lanes (like the
reference repo's) were intentionally left out of the restyle — see
[issue #186](https://github.com/ProjectASAP/ASAPPlanner/issues/186) for
why (short version: the reference's lane layout was built for BGP's flat,
linear query shapes, and `QueryExpr` has real branching DAGs) and the
proposed scope for a v2.

## Shared-subtree highlighting is a proxy, not real CSE

The highlight is computed by hashing each node's `(kind, detail, children)`
bottom-up and matching hashes across queries — see the doc comment on
`crates/types/src/dag_export.rs`. It is **not** driven by
`asap_plan::cse::dedupe_subtrees`: that pass isn't wired into any end-to-end
multi-root planning path today (no caller outside its own unit tests), so
there's nothing yet that would emit a real `CseWorkloadPlan` to visualize.
Once that wiring exists, this viewer is a natural place to render its
`bindings`/`Ref` output as converging edges instead of this hash-based proxy.

## Vendored dependencies

`cytoscape.min.js`, `dagre.min.js`, and `cytoscape-dagre.js` are vendored here
(not CDN-fetched) so the page works fully offline and doesn't drift with
upstream releases. All three are MIT-licensed:
- <https://github.com/cytoscape/cytoscape.js>
- <https://github.com/dagrejs/dagre>
- <https://github.com/cytoscape/cytoscape.js-dagre>
