"""Private HTTP adapter for isolated Build123d script execution.

API: GET /health and POST /execute with a workspace-relative ``script_path``.
The path guard and execution lock keep requests inside the shared workspace and
prevent concurrent CAD jobs from producing ambiguous artifact snapshots.
"""

import json
import os
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from importlib import import_module
from pathlib import Path
from urllib.parse import urlparse

HOST = os.getenv("SERVICE_HOST", "0.0.0.0")
PORT = int(os.getenv("SERVICE_PORT", "8000"))
WORKSPACE = Path(os.getenv("WORKSPACE_PATH", "/workspace")).resolve()
TIMEOUT_SECONDS = int(os.getenv("BUILD123D_TIMEOUT_SECONDS", "300"))
MAX_BODY_BYTES = 16_384
MAX_OUTPUT_CHARS = 30_000
TRUNCATION_MARKER = "\n... output truncated ..."
EXECUTION_LOCK = threading.Lock()


def bounded(value: str) -> str:
    if len(value) <= MAX_OUTPUT_CHARS:
        return value
    return value[: MAX_OUTPUT_CHARS - len(TRUNCATION_MARKER)] + TRUNCATION_MARKER


def resolve_workspace_path(value: object) -> Path:
    if not isinstance(value, str) or not value.strip():
        raise ValueError("'script_path' must be a non-empty string")

    candidate = (WORKSPACE / value).resolve()
    try:
        candidate.relative_to(WORKSPACE)
    except ValueError as exc:
        raise ValueError("path must remain inside /workspace") from exc

    if candidate.suffix.lower() != ".py":
        raise ValueError("'script_path' must reference a Python file")
    if not candidate.is_file():
        raise ValueError(f"script does not exist: {value}")
    return candidate


def snapshot_workspace() -> dict[str, tuple[int, int]]:
    files = {}
    for path in WORKSPACE.rglob("*"):
        if path.is_file():
            stat = path.stat()
            files[str(path.relative_to(WORKSPACE))] = (stat.st_mtime_ns, stat.st_size)
    return files


class Handler(BaseHTTPRequestHandler):
    server_version = "build123d-service/1"

    def send_json(self, status: int, payload: dict) -> None:
        data = json.dumps(payload, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self) -> None:
        if urlparse(self.path).path == "/health":
            self.send_json(200, {"ok": True, "service": "build123d"})
            return
        if (
            urlparse(self.path).path == "/health/review"
            and os.getenv("CAD_REVIEW_ENABLED") == "1"
        ):
            try:
                import_module("cad_visual_review")
                import_module("pyvista")
            except ImportError:
                self.send_json(
                    503, {"ok": False, "error": "CAD renderer is unavailable"}
                )
                return
            self.send_json(200, {"ok": True, "service": "cad-review"})
            return
        if (
            urlparse(self.path).path == "/health/simplecadapi"
            and os.getenv("SIMPLECADAPI_ENABLED") == "1"
        ):
            try:
                import_module("simplecadapi.topology.model")
            except ImportError:
                self.send_json(
                    503, {"ok": False, "error": "SimpleCADAPI is unavailable"}
                )
                return
            self.send_json(200, {"ok": True, "service": "simplecadapi"})
            return
        self.send_json(404, {"ok": False, "error": "not found"})

    def do_POST(self) -> None:
        route = urlparse(self.path).path
        if route not in {"/execute", "/execute/simplecadapi", "/render/review"}:
            self.send_json(404, {"ok": False, "error": "not found"})
            return
        if (
            route == "/execute/simplecadapi"
            and os.getenv("SIMPLECADAPI_ENABLED") != "1"
        ):
            self.send_json(404, {"ok": False, "error": "SimpleCADAPI is disabled"})
            return
        if route == "/render/review" and os.getenv("CAD_REVIEW_ENABLED") != "1":
            self.send_json(404, {"ok": False, "error": "CAD review is disabled"})
            return

        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > MAX_BODY_BYTES:
                raise ValueError("request body is missing or too large")
            request = json.loads(self.rfile.read(length))
            if not isinstance(request, dict):
                raise TypeError("request must be a JSON object")

            if route == "/render/review":
                from cad_visual_review import resolve_inputs

                resolve_inputs(
                    WORKSPACE, request.get("artifact_path"), request.get("review_dir")
                )
                with EXECUTION_LOCK:
                    try:
                        result = subprocess.run(
                            [
                                os.sys.executable,
                                "-I",
                                str(Path(__file__).with_name("cad_visual_review.py")),
                                str(WORKSPACE),
                                request["artifact_path"],
                                request["review_dir"],
                            ],
                            cwd="/tmp",
                            capture_output=True,
                            text=True,
                            timeout=TIMEOUT_SECONDS,
                            check=False,
                        )
                        payload = {
                            "ok": result.returncode == 0,
                            "exit_code": result.returncode,
                            "stderr": bounded(result.stderr),
                        }
                        if result.returncode == 0:
                            payload.update(
                                json.loads(result.stdout.strip().splitlines()[-1])
                            )
                        self.send_json(
                            200 if result.returncode == 0 else 422, payload
                        )
                    except subprocess.TimeoutExpired:
                        self.send_json(
                            408, {"ok": False, "error": "rendering timeout"}
                        )
                return

            script = resolve_workspace_path(request.get("script_path"))
            if route == "/execute/simplecadapi":
                if Path(request["script_path"]).is_absolute():
                    raise ValueError("SimpleCADAPI script_path must be workspace-relative")
                from simplecadapi_backend import execute_simplecadapi, resolve_outputs

                outputs = resolve_outputs(WORKSPACE, request.get("outputs_dir"))
                with EXECUTION_LOCK:
                    try:
                        status_code, payload = execute_simplecadapi(
                            WORKSPACE, script, outputs, TIMEOUT_SECONDS, bounded
                        )
                        self.send_json(status_code, payload)
                    except subprocess.TimeoutExpired:
                        self.send_json(
                            408, {"ok": False, "error": "execution timeout"}
                        )
                return

            command = ["python", str(script)]

            with EXECUTION_LOCK:
                before = snapshot_workspace()
                try:
                    result = subprocess.run(
                        command,
                        cwd=script.parent,
                        capture_output=True,
                        text=True,
                        timeout=TIMEOUT_SECONDS,
                        check=False,
                    )
                    after = snapshot_workspace()
                    changed_files = sorted(
                        name
                        for name, metadata in after.items()
                        if before.get(name) != metadata
                    )
                    payload = {
                        "ok": result.returncode == 0,
                        "exit_code": result.returncode,
                        "stdout": bounded(result.stdout),
                        "stderr": bounded(result.stderr),
                        "changed_files": changed_files,
                    }
                    self.send_json(200 if result.returncode == 0 else 422, payload)
                except subprocess.TimeoutExpired as exc:
                    self.send_json(
                        408,
                        {
                            "ok": False,
                            "error": "execution timeout",
                            "stdout": bounded(exc.stdout or ""),
                            "stderr": bounded(exc.stderr or ""),
                        },
                    )
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            self.send_json(400, {"ok": False, "error": str(exc)})
        except OSError as exc:
            self.send_json(500, {"ok": False, "error": str(exc)})


if __name__ == "__main__":
    ThreadingHTTPServer((HOST, PORT), Handler).serve_forever()
