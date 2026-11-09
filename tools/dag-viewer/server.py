#!/usr/bin/env python3
"""Local server for the ASAPPlanner Stage Viewer: serves the page, writes the
example documents, and plans PlanningWorkload files with
`asap_planner --run-until stage3 --format json`."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
import threading
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
BINARY = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")) / "debug" / "asap_planner"
EXAMPLES = HERE / "examples.json"
# The `--cost-inputs` file of each cost model a request may name: <name>.json.
COST_INPUTS = HERE / "cost-inputs"


def build_planner() -> None:
    subprocess.run(
        ["cargo", "build", "-q", "-p", "asap-devtools", "--bin", "asap_planner"],
        cwd=REPO,
        check=True,
    )


# The file next to a SQL workload that declares the tables it reads: a JSON
# array of `--sql-catalog-table` declarations. A PlanningWorkload declares no tables.
TABLES_FILE = "tables.json"


def sidecar_tables(workload: Path) -> list:
    """The SQL tables declared in the `tables.json` next to `workload`;
    empty when there is none."""
    path = workload.parent / TABLES_FILE
    if not path.exists():
        return []
    tables = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(tables, list):
        raise ValueError(f"{path}: must be an array of table declarations")
    return tables


def planner_args(workload: Path, tables: list, capabilities: str | None, cost_model: str | None,
                 accuracy_guarantees: str | None, out: str) -> list[str]:
    """The `asap_planner` arguments that plan `workload` through Stage 3 into
    the asap-stage-pipeline/v2 document `out`: the SQL `tables` it reads,
    the deployment capabilities file `capabilities`, the cost model
    `cost_model` (a COST_MODELS name) and the accuracy guarantee mode
    (`"enforced"`; anything else ignores them). Without capabilities or a
    cost model, the planner's defaults apply."""
    args = [str(BINARY), "--workload", str(workload), *table_args(tables, "table"),
            "--run-until", "stage3", "--format", "json"]
    if capabilities is not None:
        args.extend(["--deployment-capabilities", capabilities])
    if cost_model is not None:
        args.extend(["--cost-inputs", str(COST_INPUTS / f"{cost_model}.json")])
    if accuracy_guarantees != "enforced":
        args.append("--ignore-accuracy-guarantees")
    return args + ["--out", out]


def example_args(example: dict, out: str) -> list[str]:
    """The `asap_planner` arguments that write one examples.json entry: its
    PlanningWorkload file (`workload`, relative to this directory) with the
    SQL tables in the `tables.json` next to it, if any. Accuracy guarantees
    are ignored unless the entry's `accuracy_guarantees` is `"enforced"`."""
    mode = example.get("accuracy_guarantees", "ignored")
    if mode not in ("ignored", "enforced"):
        raise ValueError(f"{example.get('id')}: accuracy_guarantees must be ignored or enforced")
    workload = HERE / example["workload"]
    return planner_args(workload, sidecar_tables(workload), None, None, mode, out)


def write_examples(force: bool) -> None:
    """Write each examples.json document that is missing (all with `force`)
    into out/, which git ignores."""
    for example in json.loads(EXAMPLES.read_text(encoding="utf-8")):
        out = HERE / example["file"]
        if out.exists() and not force:
            continue
        out.parent.mkdir(parents=True, exist_ok=True)
        print(f"[examples] writing {example['file']}", flush=True)
        subprocess.run(example_args(example, str(out)), cwd=REPO, check=True)


def table_args(tables: list, label: str) -> list[str]:
    """One `--sql-catalog-table <json>` per declared SQL table, after checking each
    declaration (`{"name", "columns": [{"name", "type", "nullable"}],
    "time_index"}`); errors name the table as `<label> #<n>`. Raises
    ValueError for a malformed declaration."""
    args: list[str] = []
    names: set[str] = set()
    for index, table in enumerate(tables, 1):
        if not isinstance(table, dict):
            raise ValueError(f"{label} #{index} must be an object")
        name = table.get("name")
        columns = table.get("columns")
        if not isinstance(name, str) or not name.strip():
            raise ValueError(f"{label} #{index}: name must be a non-empty string")
        if name in names:
            raise ValueError(f"{label} #{index}: table {name} is declared twice")
        names.add(name)
        if not isinstance(columns, list) or not columns or len(columns) > 500:
            raise ValueError(f"{label} #{index}: columns must contain 1–500 entries")
        for number, column in enumerate(columns, 1):
            where = f"{label} #{index}: column #{number}"
            if not isinstance(column, dict):
                raise ValueError(f"{where} must be an object")
            if not isinstance(column.get("name"), str) or not column["name"].strip():
                raise ValueError(f"{where}: name must be a non-empty string")
            if not isinstance(column.get("type"), str):
                raise ValueError(f"{where}: type must be a string")
            if not isinstance(column.get("nullable", True), bool):
                raise ValueError(f"{where}: nullable must be a boolean")
        time_index = table.get("time_index")
        if time_index is not None and (
            isinstance(time_index, bool) or not isinstance(time_index, int) or not 0 <= time_index < len(columns)
        ):
            raise ValueError(f"{label} #{index}: time_index must be the index of one of its {len(columns)} columns")
        args.extend(["--sql-catalog-table", json.dumps(table, separators=(",", ":"))])
    return args


# One planning request at a time: asap_planner is CPU- and memory-heavy.
PLAN_LOCK = threading.Lock()


def workload_args(payload: object, tmp: str, out: str) -> list[str]:
    """The `asap_planner` arguments that plan a PlanningWorkload request:
    `{"workload": <PlanningWorkload JSON>, "deployment_capabilities": <JSON or
    null>, "cost_model": <a COST_MODELS name or null>, "accuracy_guarantees":
    <"ignored", "enforced" or null (ignored)>, "tables": <the SQL tables the
    workload reads, as `--sql-catalog-table` declarations, or null>}`. The
    workload and deployment capabilities are written into the directory
    `tmp`. A PlanningWorkload declares no tables, so a SQL workload plans
    only with `tables`. `asap_planner` validates the workload itself.
    Raises ValueError for a malformed request."""
    if not isinstance(payload, dict):
        raise ValueError("request must be a JSON object")
    workload = payload.get("workload")
    if not isinstance(workload, dict) or not isinstance(workload.get("query_workload"), dict):
        raise ValueError("workload must be a PlanningWorkload object with a query_workload")
    capabilities = payload.get("deployment_capabilities")
    if capabilities is not None and not isinstance(capabilities, dict):
        raise ValueError("deployment_capabilities must be an object or null")
    cost_model = payload.get("cost_model")
    if cost_model is not None and cost_model not in COST_MODELS:
        raise ValueError(f"cost_model must be one of {', '.join(COST_MODELS)} or null")
    mode = payload.get("accuracy_guarantees")
    if mode not in (None, "ignored", "enforced"):
        raise ValueError("accuracy_guarantees must be ignored, enforced or null")
    tables = payload.get("tables")
    if tables is not None and (not isinstance(tables, list) or len(tables) > 50):
        raise ValueError("tables must be an array with at most 50 tables, or null")
    table_args(tables or [], "table")  # a malformed table fails before anything is written
    workload_path = Path(tmp) / "workload.json"
    workload_path.write_text(json.dumps(workload), encoding="utf-8")
    capabilities_path = None
    if capabilities is not None:
        capabilities_path = str(Path(tmp) / "deployment-capabilities.json")
        Path(capabilities_path).write_text(json.dumps(capabilities), encoding="utf-8")
    return planner_args(workload_path, tables or [], capabilities_path, cost_model, mode, out)


# The cost models a request may name: the files in cost-inputs/.
COST_MODELS = tuple(sorted(path.stem for path in COST_INPUTS.glob("*.json")))


# How long one planning request may run.
PLAN_TIMEOUT_SECONDS = 300


def plan_workload(payload: object) -> dict:
    """Plan a PlanningWorkload request (`workload_args`) with `asap_planner`;
    returns its asap-stage-pipeline/v2 document. Requests run one at a
    time."""
    print("\n[planner] workload", flush=True)
    with PLAN_LOCK, tempfile.TemporaryDirectory() as tmp:
        out = str(Path(tmp) / "plan.json")
        args = workload_args(payload, tmp, out)
        result = subprocess.run(args, cwd=REPO, text=True, capture_output=True, timeout=PLAN_TIMEOUT_SECONDS)
        # A failed stage exits 1 but still writes the document, with the error.
        if not Path(out).exists():
            raise RuntimeError(result.stderr.strip() or "asap_planner failed")
        document = json.loads(Path(out).read_text(encoding="utf-8"))
    # Stage 3's `plan`, or null with its `error`.
    selected = "a plan" if (document["stage3_plan_selection"] or {}).get("plan") else "no plan"
    print(f"[planner] Stage 3 selected {selected}", flush=True)
    return document


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(HERE), **kwargs)

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        # The page asks whether it is served by this server (it can plan).
        if self.path == "/api/status":
            self._send_json(HTTPStatus.OK, {"planner": True})
            return
        super().do_GET()

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path != "/api/plan-workload":
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 5_000_000:
                raise ValueError("request body must contain at most 5 MB")
            self._send_json(HTTPStatus.OK, plan_workload(json.loads(self.rfile.read(length))))
        except (ValueError, json.JSONDecodeError) as error:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": str(error)})
        except subprocess.TimeoutExpired:
            self._send_json(HTTPStatus.GATEWAY_TIMEOUT, {"error": f"planning exceeded {PLAN_TIMEOUT_SECONDS} seconds"})
        except Exception as error:  # surface planner failures to this local UI
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {"error": str(error)})

    def end_headers(self) -> None:
        # This is a local development viewer: stale HTML/JS/example JSON is
        # more harmful than the tiny files are expensive to reload.
        self.send_header("Cache-Control", "no-store, max-age=0")
        super().end_headers()

    def _send_json(self, status: HTTPStatus, value: object) -> None:
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--skip-build", action="store_true", help="reuse an already-built asap_planner")
    parser.add_argument("--regenerate", action="store_true", help="rewrite the example documents in out/")
    args = parser.parse_args()
    if not args.skip_build:
        build_planner()
    if not BINARY.exists():
        parser.error(f"{BINARY} does not exist; start without --skip-build")
    write_examples(args.regenerate)
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"ASAPPlanner Stage Viewer: http://{args.host}:{args.port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopped.")
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
