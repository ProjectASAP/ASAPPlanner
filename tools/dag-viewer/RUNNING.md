# Run the DAG viewer

```bash
cd ASAPPlanner
python3 tools/dag-viewer/server.py
```

The server builds `asap_planner` (`--skip-build` reuses a built one), plans
the example tabs it has not planned yet (`--regenerate` plans them again),
and serves the page. Open:

```text
http://localhost:8000/
```

Press `Ctrl+C` in the server terminal to stop it and free port 8000.
