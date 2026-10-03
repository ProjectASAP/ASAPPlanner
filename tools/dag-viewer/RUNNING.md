# Run the DAG viewer

```bash
cd ASAPPlanner
python3 tools/dag-viewer/server.py
```

Open:

```text
http://localhost:8000/
```

To open the Stages view with the sample stage-pipeline document:

```text
http://localhost:8000/?doc=examples/stage-pipeline.sample.json
```

Use `--port 8765` to serve on another port, and `--skip-build` to reuse an
already-built `target/debug/dag_export`.

Press `Ctrl+C` in the server terminal to stop it and free port 8000.
