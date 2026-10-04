#!/usr/bin/env python3
"""Local server for the ASAPPlanner Stage Viewer: serves the page, writes the
#509 example documents, and plans editor queries with `stage_pipeline`."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
BINARY = TARGET / "debug" / "stage_pipeline"
EXAMPLES = HERE / "examples.json"


def build_planner() -> None:
    subprocess.run(
        ["cargo", "build", "-q", "-p", "asap-devtools", "--bin", "stage_pipeline"],
        cwd=REPO,
        check=True,
    )


def write_examples(force: bool) -> None:
    """Write each examples.json document that is missing (all with `force`)."""
    for example in json.loads(EXAMPLES.read_text(encoding="utf-8")):
        out = HERE / example["file"]
        if out.exists() and not force:
            continue
        out.parent.mkdir(parents=True, exist_ok=True)
        print(f"[examples] writing {example['file']}", flush=True)
        subprocess.run(
            [str(BINARY), "--example", example["example"], "--out", str(out)],
            cwd=REPO,
            check=True,
        )


def plan_args(payload: dict, out: str) -> list[str]:
    """The `stage_pipeline` arguments for an editor request."""
    queries = payload.get("queries")
    if not isinstance(queries, list) or not 1 <= len(queries) <= 20:
        raise ValueError("queries must contain 1–20 PromQL queries")
    args = [str(BINARY)]
    for index, query in enumerate(queries, 1):
        if not isinstance(query, str) or not query.strip() or len(query) > 10_000:
            raise ValueError(f"query #{index} must contain 1–10000 characters")
        args.extend(["--promql", query])
    epsilon, delta = payload.get("epsilon"), payload.get("delta")
    if delta is not None and epsilon is None:
        raise ValueError("δ needs ε")
    for name, value in (("epsilon", epsilon), ("delta", delta)):
        if value is None:
            continue
        if not isinstance(value, (int, float)) or not 0 < value < 1:
            raise ValueError(f"{name} must be between 0 and 1")
        args.extend([f"--{name}", str(value)])
    interval = payload.get("interval_ms", 15_000)
    if not isinstance(interval, int) or interval <= 0:
        raise ValueError("interval_ms must be a positive integer")
    args.extend(["--interval-ms", str(interval), "--out", out])
    return args


def plan(payload: dict) -> dict:
    with tempfile.TemporaryDirectory() as tmp:
        out = str(Path(tmp) / "plan.json")
        args = plan_args(payload, out)
        print(f"[planner] {len(payload['queries'])} quer{'y' if len(payload['queries']) == 1 else 'ies'}", flush=True)
        result = subprocess.run(args, cwd=REPO, text=True, capture_output=True, timeout=120)
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip() or "stage_pipeline failed")
        return json.loads(Path(out).read_text(encoding="utf-8"))


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(HERE), **kwargs)

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path != "/api/plan":
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 1_000_000:
                raise ValueError("request body must contain at most 1 MB")
            self._send_json(HTTPStatus.OK, plan(json.loads(self.rfile.read(length))))
        except (ValueError, json.JSONDecodeError) as error:
            self._send_json(HTTPStatus.BAD_REQUEST, {"error": str(error)})
        except subprocess.TimeoutExpired:
            self._send_json(HTTPStatus.GATEWAY_TIMEOUT, {"error": "planning exceeded 120 seconds"})
        except Exception as error:  # surface planner failures to this local UI
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {"error": str(error)})

    def end_headers(self) -> None:
        # A local development viewer: stale pages and documents cost more
        # than reloading them.
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
    parser.add_argument("--regenerate", action="store_true", help="rewrite the example documents")
    args = parser.parse_args()
    if not args.skip_build:
        build_planner()
    if not BINARY.exists():
        parser.error(f"{BINARY} does not exist; start without --skip-build")
    write_examples(force=args.regenerate)
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
