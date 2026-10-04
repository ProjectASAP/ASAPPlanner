# Run the Stage Viewer

```bash
cd ASAPPlanner
python3 tools/dag-viewer/server.py
```

The server builds `stage_pipeline`, writes any missing example documents
into `tools/dag-viewer/out/`, and serves the page at:

```text
http://localhost:8000/
```

- `--port 8765` serves on another port.
- `--skip-build` reuses an already-built `stage_pipeline` (under
  `$CARGO_TARGET_DIR` if set, otherwise `target/`).
- `--regenerate` rewrites the example documents, for example after a
  planner change.

To open one document instead of the examples:

```text
http://localhost:8000/?doc=examples/stage-pipeline.sample.json
```

Press `Ctrl+C` in the server terminal to stop it.
