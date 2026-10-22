"""Opt-in SimpleCADAPI execution inside the existing isolated CAD worker."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from importlib import import_module
from pathlib import Path, PurePosixPath
from uuid import uuid4

MAX_GRAPH_BYTES = 10 * 1024 * 1024


def resolve_outputs(workspace: Path, value: object) -> Path:
    if not isinstance(value, str) or not value.strip():
        raise ValueError("'outputs_dir' must be a session-relative path")
    parts = PurePosixPath(value).parts
    if (
        len(parts) != 3
        or parts[0] != "sessions"
        or parts[2] != "outputs"
        or not parts[1]
        or not all(c.isascii() and (c.isalnum() or c in "_-") for c in parts[1])
    ):
        raise ValueError("outputs_dir must be sessions/<session-id>/outputs")
    root = workspace.resolve()
    output = (root / value).resolve()
    if not output.is_relative_to(root):
        raise ValueError("outputs_dir escapes the workspace")
    if not output.is_dir():
        raise ValueError("session outputs directory does not exist")
    return output


def inspect_graph(path: Path) -> dict:
    if path.is_symlink() or not path.is_file():
        raise ValueError("Required fresh OperationGraph is missing or is a symlink")
    if path.stat().st_size > MAX_GRAPH_BYTES:
        raise ValueError("OperationGraph exceeds the 10 MiB limit")
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise TypeError("OperationGraph must be a JSON object")
    graph = import_module("simplecadapi.topology.model").OperationGraph.from_dict(payload)
    if graph.node_count < 1:
        raise ValueError("OperationGraph contains no operation nodes")
    operations = [node.op for node in graph.topological_order()]
    return {"graph_nodes": graph.node_count, "graph_operations": operations}


def execute_simplecadapi(
    workspace: Path, script: Path, outputs: Path,
    timeout: int, bound
) -> tuple[int, dict]:
    deadline = time.monotonic() + timeout
    directory = outputs / "simplecadapi" / uuid4().hex
    directory = directory.resolve()
    if not directory.is_relative_to(outputs.resolve()):
        raise ValueError("candidate directory escapes session outputs")
    directory.mkdir(parents=True, exist_ok=False)
    graph_path = directory / "model_graph.json"
    env = {
        "PATH": os.defpath, "HOME": "/tmp", "TMPDIR": "/tmp",
        "PYTHONDONTWRITEBYTECODE": "1", "PYTHONUNBUFFERED": "1",
        "SIMPLECADAPI_OUTPUT_DIR": str(directory),
        "SIMPLECADAPI_GRAPH_PATH": str(graph_path),
    }
    result = subprocess.run(
        [sys.executable, str(script)], cwd=workspace, env=env,
        capture_output=True, text=True, timeout=timeout, check=False,
    )
    payload = {
        "ok": False, "exit_code": result.returncode,
        "stdout": bound(result.stdout), "stderr": bound(result.stderr),
        "outputs_dir": directory.relative_to(workspace).as_posix(),
        "graph_checked": False, "geometry_checked": False,
    }
    if result.returncode:
        return 422, payload
    # Inspect in a clean interpreter, not the process that ran generated code.
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise subprocess.TimeoutExpired([sys.executable, str(script)], timeout)
    inspection = subprocess.run(
        [sys.executable, "-I", str(Path(__file__).resolve()), str(graph_path)],
        cwd="/tmp", env=env, capture_output=True, text=True,
        timeout=remaining, check=False,
    )
    if inspection.returncode:
        payload.update(exit_code=1, stderr=bound(
            result.stderr + "\nOperationGraph validation failed: " + inspection.stderr
        ))
        return 422, payload
    metadata = json.loads(inspection.stdout)
    payload.update(ok=True, graph_checked=True, **metadata)
    payload["graph_path"] = graph_path.relative_to(workspace).as_posix()
    payload["changed_files"] = sorted(
        path.relative_to(workspace).as_posix() for path in directory.rglob("*")
        if path.is_file() and not path.is_symlink()
        and path.resolve().is_relative_to(directory)
    )
    return 200, payload


if __name__ == "__main__":
    try:
        print(json.dumps(inspect_graph(Path(sys.argv[1]))))
    except Exception as exc:  # noqa: BLE001
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        sys.exit(1)
