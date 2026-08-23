# Running the DAG viewer

## 1. Generate a graph

Skip this step to just look around. The page ships with a small committed
example, `dag.example.json`, and loads it automatically if there's no
`dag.json` sitting next to it.

For your own queries, run the wrapper script from the repo root:

```sh
./tools/dag-viewer/generate-sample.sh
```

It writes `tools/dag-viewer/dag.json` from three fixed example queries. Open
the script and edit the `--sql`/`--promql` lines to try your own instead.

For more control, call the underlying binary directly:

```sh
cargo run -p asap-devtools --bin dag_export -- \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  --promql "topk(5, rate(http_requests_total[5m]))" --name q2 \
  > tools/dag-viewer/dag.json
```

Repeat `--sql` or `--promql` for each query you want in the file. `--name` is optional. Skip it and queries get `q1`, `q2`, and so on. Everything lands in one `WorkloadGraph`.

Save it as `tools/dag-viewer/dag.json`. The viewer looks for that exact path on load, before falling back to the example.

## 2. Open the viewer on your own machine

Open `tools/dag-viewer/index.html` in a browser. Double-click it, or use `file://` directly.

Some browsers block local script loads over `file://`. Chrome is one of them. If the page stays blank, serve the folder instead:

```sh
cd tools/dag-viewer
python3 -m http.server 8420
```

Visit `http://localhost:8420/`.

`dag.json` sits in the same folder, so the page loads it automatically. No drag and drop needed. You can still drag other exported files onto the page, or use the file picker, to load more queries alongside it.

## 3. Open the viewer over a remote tunnel

Working on a remote machine through an SSH tunnel? Drag and drop won't work here. The file lives on the remote machine. Your browser only sees files on yours.

Run the server on the remote machine:

```sh
cd tools/dag-viewer
python3 -m http.server 8420
```

Tunnel port 8420 to your machine, then open `http://localhost:8420/` locally. `dag.json` loads on page load, same as before.

## 4. Using the page

Click a node to see its detail in the side panel. That includes its category and whether it's a query's root.

Multiple queries show up as tabs across the top.

A node with a blue ring is structurally identical to a node in another loaded query. Turn it off with "Highlight shared subtrees" in the header.

The side panel also shows the query's SQL or PromQL text under "Query source," plus a legend for every node category.

Once two or more queries are loaded, the Single/Compare/Union buttons in the header switch between viewing one query at a time (default), several side by side in lanes with shared-subtree links drawn between them, or merged into one graph with shared nodes collapsed. See "Compare and Union mode" in README.md for details.
