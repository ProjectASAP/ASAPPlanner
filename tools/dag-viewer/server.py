#!/usr/bin/env python3
"""Local interactive server for planning query workloads and viewing IR DAGs."""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
import threading
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

    args = [str(BINARY), "--post-asap", "--progress", "--epsilon", str(epsilon)]
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

    print(f"\n[planner] Received workload with {len(queries)} quer{'y' if len(queries) == 1 else 'ies'}", flush=True)
    stderr_lines: list[str] = []
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stdout_file:
        process = subprocess.Popen(
            args,
            cwd=REPO,
            text=True,
            stdout=stdout_file,
            stderr=subprocess.PIPE,
        )

        def relay_stderr() -> None:
            assert process.stderr is not None
            for line in process.stderr:
                line = line.rstrip()
                stderr_lines.append(line)
                print(f"[planner] {line}", flush=True)

        relay = threading.Thread(target=relay_stderr, daemon=True)
        relay.start()
        try:
            returncode = process.wait(timeout=120)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            relay.join()
            raise
        relay.join()
        stdout_file.seek(0)
        stdout = stdout_file.read()

    stderr = "\n".join(stderr_lines)
    if returncode != 0:
        raise RuntimeError(stderr.strip() or "dag_export failed")
    workload = json.loads(stdout)
    workload["planner_stderr"] = stderr
    print(f"[planner] Complete: generated {len(workload.get('queries', []))} pre/post DAG pairs", flush=True)
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
