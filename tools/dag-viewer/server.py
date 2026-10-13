#!/usr/bin/env python3
"""Local interactive server: plans Query planner workloads with `stage_pipeline` and serves the DAG viewer."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from render import load_document

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
BINARY = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")) / "debug" / "stage_pipeline"


def build_planner() -> None:
    subprocess.run(
        ["cargo", "build", "-q", "-p", "asap-devtools", "--bin", "stage_pipeline"],
        cwd=REPO,
        check=True,
    )


def plan_args(payload: dict, out: str) -> list[str]:
    """The `stage_pipeline` arguments for a Query planner request: SQL and
    PromQL queries in order, each with its name; SQL tables declared with
    `--table`."""
    queries = payload.get("queries")
    if not isinstance(queries, list) or not queries or len(queries) > 50:
        raise ValueError("queries must contain 1–50 entries")
    if not all(isinstance(query, dict) for query in queries):
        raise ValueError("each query must be an object")

    schemas = payload.get("schemas", [])
    if not isinstance(schemas, list) or len(schemas) > 50:
        raise ValueError("schemas must be an array with at most 50 tables")
    args = [str(BINARY)]
    schema_names: set[str] = set()
    for index, schema in enumerate(schemas, 1):
        if not isinstance(schema, dict):
            raise ValueError(f"schema #{index} must be an object")
        name = schema.get("name")
        columns = schema.get("columns")
        if not isinstance(name, str) or not name.strip():
            raise ValueError(f"schema #{index}: name is required")
        if name in schema_names:
            raise ValueError(f"schema #{index}: duplicate table name {name}")
        schema_names.add(name)
        if not isinstance(columns, list) or not columns or len(columns) > 500:
            raise ValueError(f"schema #{index}: columns must contain 1–500 entries")
        if any(query.get("language", "sql") == "sql" for query in queries):
            args.extend(["--table", json.dumps(schema, separators=(",", ":"))])
    names: set[str] = set()
    for index, query in enumerate(queries, 1):
        language = query.get("language", "sql")
        if language not in {"sql", "promql"}:
            raise ValueError(f"query #{index}: language must be sql or promql")
        name = str(query.get("name") or "").strip() or f"q{index}"
        if name in names:
            raise ValueError(f"query #{index}: duplicate name {name}")
        names.add(name)
        text = query.get("text")
        if not isinstance(text, str) or not text.strip() or len(text) > 100_000:
            raise ValueError(f"query #{index}: text must contain 1–100000 characters")
        selected_schemas = query.get("schemas", [])
        if language == "sql" and (
            not isinstance(selected_schemas, list)
            or any(schema not in schema_names for schema in selected_schemas)
        ):
            raise ValueError(f"query #{index}: selected schemas must all be enabled")
        args.extend([f"--{language}", text, "--name", name])

    epsilon = payload.get("epsilon")
    if epsilon is not None:
        if not isinstance(epsilon, (int, float)) or not 0 < epsilon < 1:
            raise ValueError("epsilon must be between 0 and 1")
        args.extend(["--epsilon", str(epsilon)])
    interval = payload.get("interval_ms", 15_000)
    if not isinstance(interval, int) or interval <= 0:
        raise ValueError("interval_ms must be a positive integer")
    args.extend(["--interval-ms", str(interval), "--out", out])
    return args


def plan(payload: dict) -> dict:
    """Plan a Query planner request with `stage_pipeline`; returns its
    asap-stage-pipeline/v1 document."""
    with tempfile.TemporaryDirectory() as tmp:
        out = str(Path(tmp) / "plan.json")
        args = plan_args(payload, out)
        print(f"\n[planner] {len(payload['queries'])} quer{'y' if len(payload['queries']) == 1 else 'ies'}", flush=True)
        result = subprocess.run(args, cwd=REPO, text=True, capture_output=True, timeout=120)
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip() or "stage_pipeline failed")
        document = json.loads(Path(out).read_text(encoding="utf-8"))
    print(f"[planner] Stage 3 selected {document['stage3_plan_selection']['selected']}", flush=True)
    return document


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(HERE), **kwargs)

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path != "/api/plan":
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 5_000_000:
                raise ValueError("request body must contain at most 5 MB")
            self._send_json(HTTPStatus.OK, plan(json.loads(self.rfile.read(length))))
        except (ValueError, json.JSONDecodeError) as error:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": str(error)})
        except subprocess.TimeoutExpired:
            self._send_json(HTTPStatus.GATEWAY_TIMEOUT, {"error": "planning exceeded 120 seconds"})
        except Exception as error:  # surface planner failures to this local UI
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {"error": str(error)})

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path == "/api/example":
            try:
                self._send_json(HTTPStatus.OK, load_document(HERE / "dag.example.json"))
            except Exception as error:
                self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {"error": str(error)})
            return
        super().do_GET()

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
    args = parser.parse_args()
    if not args.skip_build:
        build_planner()
    if not BINARY.exists():
        parser.error(f"{BINARY} does not exist; start without --skip-build")
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"ASAP DAG viewer: http://{args.host}:{args.port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopped.")
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
