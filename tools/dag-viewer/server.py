#!/usr/bin/env python3
"""Local interactive server for planning query workloads and viewing IR DAGs."""

from __future__ import annotations

import argparse
import json
import subprocess
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
BINARY = REPO / "target" / "debug" / "dag_export"


def build_exporter() -> None:
    subprocess.run(
        ["cargo", "build", "-q", "-p", "asap-devtools", "--bin", "dag_export"],
        cwd=REPO,
        check=True,
    )


def plan_workload(payload: dict) -> dict:
    queries = payload.get("queries")
    if not isinstance(queries, list) or not queries or len(queries) > 50:
        raise ValueError("queries must contain 1–50 entries")
    epsilon = float(payload.get("epsilon", 0.01))
    if not 0 < epsilon < 1:
        raise ValueError("epsilon must be between 0 and 1")

    args = [str(BINARY), "--post-asap", "--epsilon", str(epsilon)]
    for index, query in enumerate(queries, 1):
        if not isinstance(query, dict):
            raise ValueError(f"query #{index} must be an object")
        language = query.get("language", "sql")
        text = query.get("text")
        name = query.get("name") or f"q{index}"
        if language not in {"sql", "promql"}:
            raise ValueError(f"query #{index}: language must be sql or promql")
        if not isinstance(text, str) or not text.strip() or len(text) > 100_000:
            raise ValueError(f"query #{index}: text must contain 1–100000 characters")
        args.extend([f"--{language}", text, "--name", str(name)])

    result = subprocess.run(args, cwd=REPO, text=True, capture_output=True, timeout=120)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "dag_export failed")
    workload = json.loads(result.stdout)
    workload["planner_stderr"] = result.stderr
    return workload


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
            payload = json.loads(self.rfile.read(length))
            response = plan_workload(payload)
            self._send_json(HTTPStatus.OK, response)
        except (ValueError, json.JSONDecodeError) as error:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": str(error)})
        except subprocess.TimeoutExpired:
            self._send_json(HTTPStatus.GATEWAY_TIMEOUT, {"error": "planning exceeded 120 seconds"})
        except Exception as error:  # surface planner failures to this local UI
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {"error": str(error)})

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
    parser.add_argument("--skip-build", action="store_true", help="use an already-built target/debug/dag_export")
    args = parser.parse_args()
    if not args.skip_build:
        build_exporter()
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
