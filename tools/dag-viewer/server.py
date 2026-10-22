#!/usr/bin/env python3
"""Local server for the ASAPPlanner Stage Viewer: serves the page, writes the
example documents, and plans editor queries and PlanningWorkload files with
`stage_pipeline`."""

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
BINARY = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")) / "debug" / "stage_pipeline"
EXAMPLES = HERE / "examples.json"


def build_planner() -> None:
    subprocess.run(
        ["cargo", "build", "-q", "-p", "asap-devtools", "--bin", "stage_pipeline"],
        cwd=REPO,
        check=True,
    )


# The file next to a SQL workload that declares the tables it reads: a JSON
# array of `--table` declarations. A PlanningWorkload declares no tables.
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


def example_args(example: dict, out: str) -> list[str]:
    """The `stage_pipeline` arguments that write one examples.json entry: a
    built-in example (`example`, with the exhaustive search its story
    describes) or a PlanningWorkload file (`workload`, relative to this
    directory, with the default search, the entry's
    `deployment_capabilities` and the SQL tables in the `tables.json` next to
    the file, if any). Accuracy guarantees are ignored, as
    `stage_pipeline` does by default, unless the entry's
    `accuracy_guarantees` is `"enforced"`."""
    mode = example.get("accuracy_guarantees", "ignored")
    if mode not in ("ignored", "enforced"):
        raise ValueError(f"{example.get('id')}: accuracy_guarantees must be ignored or enforced")
    enforce = ["--enforce-accuracy-guarantees"] if mode == "enforced" else []
    if "workload" in example:
        workload = HERE / example["workload"]
        args = [str(BINARY), "--workload", str(workload), *table_args(sidecar_tables(workload), "table")]
        if example.get("deployment_capabilities") is not None:
            args.extend(["--deployment-capabilities", json.dumps(example["deployment_capabilities"], separators=(",", ":"))])
        if example.get("cost_model") is not None:
            args.extend(["--cost-model", example["cost_model"]])
        return args + enforce + ["--out", out]
    return [str(BINARY), "--example", example["example"], "--exhaustive", *enforce, "--out", out]


def write_examples(force: bool) -> None:
    """Write each examples.json document that is missing (all with `force`):
    the built-in examples into out/, which git ignores; the committed
    workload examples where they are committed."""
    for example in json.loads(EXAMPLES.read_text(encoding="utf-8")):
        out = HERE / example["file"]
        if out.exists() and not force:
            continue
        out.parent.mkdir(parents=True, exist_ok=True)
        print(f"[examples] writing {example['file']}", flush=True)
        subprocess.run(example_args(example, str(out)), cwd=REPO, check=True)


class PlanRejected(ValueError):
    """`stage_pipeline` refused the workload: more candidate combinations
    than it enumerates (exit status 3)."""


def table_args(tables: list, label: str) -> list[str]:
    """One `--table <json>` per declared SQL table, after checking each
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
        args.extend(["--table", json.dumps(table, separators=(",", ":"))])
    return args


def plan_args(payload: object, out: str) -> list[str]:
    """The `stage_pipeline` arguments for a Query planner request: SQL and
    PromQL queries in order, each with its name; SQL tables declared with
    `--table`. Raises ValueError for a malformed request."""
    if not isinstance(payload, dict):
        raise ValueError("request must be a JSON object")
    queries = payload.get("queries")
    if not isinstance(queries, list) or not queries or len(queries) > 50:
        raise ValueError("queries must contain 1–50 entries")
    if not all(isinstance(query, dict) for query in queries):
        raise ValueError("each query must be an object")

    schemas = payload.get("schemas", [])
    if not isinstance(schemas, list) or len(schemas) > 50:
        raise ValueError("schemas must be an array with at most 50 tables")
    args = [str(BINARY)]
    has_sql = any(query.get("language", "sql") == "sql" for query in queries)
    table_flags = table_args(schemas, "schema")
    schema_names = {schema["name"] for schema in schemas}
    if has_sql:
        args.extend(table_flags)
    first_use: dict[str, int] = {}
    for index, query in enumerate(queries, 1):
        language = query.get("language", "sql")
        if language not in ("sql", "promql"):
            raise ValueError(f"query #{index}: language must be sql or promql")
        raw_name = query.get("name")
        if raw_name is not None and not isinstance(raw_name, str):
            raise ValueError(f"query #{index}: name must be a string")
        name = (raw_name or "").strip() or f"q{index}"
        if name in first_use:
            default = " (its default name)" if name == f"q{index}" and not (raw_name or "").strip() else ""
            raise ValueError(f"query #{index}: name {name}{default} is already used by query #{first_use[name]}")
        first_use[name] = index
        text = query.get("text")
        if not isinstance(text, str) or not text.strip() or len(text) > 100_000:
            raise ValueError(f"query #{index}: text must contain 1–100000 characters")
        selected_schemas = query.get("schemas", [])
        if language == "sql" and (
            not isinstance(selected_schemas, list)
            or any(not isinstance(schema, str) or schema not in schema_names for schema in selected_schemas)
        ):
            raise ValueError(f"query #{index}: selected schemas must be names of declared tables")
        args.extend([f"--{language}", text, "--name", name])

    max_error = payload.get("max_error")
    if max_error is not None:
        if isinstance(max_error, bool) or not isinstance(max_error, (int, float)):
            raise ValueError("max_error must be a number")
        if not 0 < max_error < 1:
            raise ValueError("max_error must be between 0 and 1")
        args.extend(["--max-error", str(max_error)])
    # The target's error metric; empty leaves it unset, and Stage 3 then
    # accepts a guarantee in any metric and reports the one it accepted.
    accuracy_metric = payload.get("accuracy_metric")
    if accuracy_metric not in (None, ""):
        if accuracy_metric not in ("absolute", "relative", "rank"):
            raise ValueError("accuracy_metric must be absolute, relative or rank")
        if max_error is None:
            raise ValueError("accuracy_metric needs max_error")
        args.extend(["--accuracy-metric", accuracy_metric])
    # Declared source facts, each for every source the queries read.
    for key, flag, kind in (
        ("series_count", "--series-count", int),
        ("rows_per_second", "--rows-per-second", float),
        ("rows_held", "--rows-held", int),
    ):
        value = payload.get(key)
        if value is None:
            continue
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValueError(f"{key} must be a number")
        if kind is int and (value != int(value) or value <= 0):
            raise ValueError(f"{key} must be a positive integer")
        if kind is float and value < 0:
            raise ValueError(f"{key} must be a non-negative number")
        args.extend([flag, str(kind(value))])
    interval = payload.get("interval_ms", 15_000)
    if isinstance(interval, bool) or not isinstance(interval, int) or interval <= 0:
        raise ValueError("interval_ms must be a positive integer")
    args.extend(["--sample-interval-ms", str(interval), "--out", out])
    return args


# One planning request at a time: stage_pipeline is CPU- and memory-heavy,
# and the editor shows one plan.
PLAN_LOCK = threading.Lock()


def plan(payload: object) -> dict:
    """Plan a Query planner request with `stage_pipeline`; returns its
    asap-stage-pipeline/v1 document. Requests run one at a time."""
    plan_args(payload, "out.json")  # a malformed request fails before the lock
    count = len(payload["queries"])
    print(f"\n[planner] {count} quer{'y' if count == 1 else 'ies'}", flush=True)
    return run_stage_pipeline(lambda tmp, out: plan_args(payload, out))


def workload_args(payload: object, workload_path: str, out: str) -> list[str]:
    """The `stage_pipeline` arguments that plan a PlanningWorkload request:
    `{"workload": <PlanningWorkload JSON>, "deployment_capabilities": <JSON or
    null>, "cost_model": <a COST_MODELS name or null>, "accuracy_guarantees":
    <"ignored", "enforced" or null (ignored, stage_pipeline's default)>,
    "tables": <the SQL tables the workload reads, as `--table` declarations,
    or null>}`, the workload written to `workload_path`. A PlanningWorkload
    declares no tables, so a SQL workload plans only with `tables`. `stage_pipeline`
    validates the workload itself. Raises ValueError for a malformed
    request."""
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
    table_flags = table_args(tables or [], "table")
    Path(workload_path).write_text(json.dumps(workload), encoding="utf-8")
    args = [str(BINARY), "--workload", workload_path, *table_flags]
    if capabilities is not None:
        args.extend(["--deployment-capabilities", json.dumps(capabilities, separators=(",", ":"))])
    if cost_model is not None:
        args.extend(["--cost-model", cost_model])
    if mode == "enforced":
        args.append("--enforce-accuracy-guarantees")
    return args + ["--out", out]


# The priced cost models `--cost-model` takes without further flags.
COST_MODELS = ("usage-over-time", "aws-fargate-us-east-1", "provisioned-capacity")


# How long one planning request may run.
PLAN_TIMEOUT_SECONDS = 300


def run_stage_pipeline(make_args) -> dict:
    """Run `stage_pipeline` with `make_args(tmp_dir, out)`; returns its
    asap-stage-pipeline/v1 document. Requests run one at a time."""
    with PLAN_LOCK, tempfile.TemporaryDirectory() as tmp:
        out = str(Path(tmp) / "plan.json")
        args = make_args(tmp, out)
        result = subprocess.run(args, cwd=REPO, text=True, capture_output=True, timeout=PLAN_TIMEOUT_SECONDS)
        if result.returncode == 3:
            raise PlanRejected(result.stderr.strip())
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip() or "stage_pipeline failed")
        document = json.loads(Path(out).read_text(encoding="utf-8"))
    print(f"[planner] Stage 3 selected {document['stage3_plan_selection']['selected']}", flush=True)
    return document


def plan_workload(payload: object) -> dict:
    """Plan a PlanningWorkload request (`workload_args`) with `stage_pipeline
    --workload` and the default search."""
    print("\n[planner] workload", flush=True)
    return run_stage_pipeline(lambda tmp, out: workload_args(payload, str(Path(tmp) / "workload.json"), out))


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
        endpoints = {"/api/plan": plan, "/api/plan-workload": plan_workload}
        if self.path not in endpoints:
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 5_000_000:
                raise ValueError("request body must contain at most 5 MB")
            self._send_json(HTTPStatus.OK, endpoints[self.path](json.loads(self.rfile.read(length))))
        except PlanRejected as error:
            self._send_json(HTTPStatus.UNPROCESSABLE_ENTITY, {"error": str(error)})
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
    parser.add_argument("--skip-build", action="store_true", help="reuse an already-built stage_pipeline")
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
