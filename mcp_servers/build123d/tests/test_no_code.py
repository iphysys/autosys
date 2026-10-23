"""No tool accepts code or a script to execute, and the server cannot run any.

Three layers: what MCP discovery advertises admits no free-form text, calls
that try to pass code are refused, and the server's source never reaches an
interpreter, a subprocess or a dynamic import.
"""

import ast
from pathlib import Path
from typing import Any

import pytest
from mcp import Client
from mcp.shared.exceptions import MCPError

import build123d_models
import build123d_server
from conftest import DENSITY, plate_args

pytestmark = pytest.mark.anyio

# The only string pattern any argument may have: a part_id. It admits 71
# characters of hex, never code or a path.
ALLOWED_STRING_PATTERNS = {"^sha256:[0-9a-f]{64}$"}


def string_leaves_and_open_objects(schema: dict[str, Any]) -> list[str]:
    """Paths in an advertised input schema that could carry arbitrary data."""
    defs: dict[str, Any] = schema.get("$defs", {})
    problems: list[str] = []

    def visit(node: dict[str, Any], path: str, seen: frozenset[str]) -> None:
        if "$ref" in node:
            name = node["$ref"].removeprefix("#/$defs/")
            if name in seen:
                problems.append(f"{path}: recursive $ref {name}")
                return
            visit(defs[name], path, seen | {name})
            return
        if "const" in node or "enum" in node:
            return
        kind = node.get("type")
        if kind in ("number", "integer", "boolean"):
            return
        if kind == "string":
            if node.get("pattern") not in ALLOWED_STRING_PATTERNS:
                problems.append(f"{path}: free-form string")
            return
        if kind == "object":
            if node.get("additionalProperties") is not False:
                problems.append(f"{path}: object accepts unlisted fields")
            for key, child in node.get("properties", {}).items():
                visit(child, f"{path}/{key}", seen)
            return
        problems.append(f"{path}: unchecked schema {sorted(node)}")

    visit(schema, "", frozenset())
    return problems


async def test_advertised_inputs_admit_no_free_form_text(server_url: str) -> None:
    async with Client(server_url) as client:
        listed = (await client.list_tools()).tools
    assert listed
    for tool in listed:
        assert string_leaves_and_open_objects(tool.input_schema) == [], tool.name


@pytest.mark.parametrize(
    "smuggled",
    [
        {"script": "import os; os.system('id')"},
        {"code": "print(1)"},
        {"script_path": "sessions/x/model.py"},
        {"command": ["python", "model.py"]},
    ],
)
async def test_calls_carrying_code_are_refused(server_url: str, smuggled: dict[str, Any]) -> None:
    async with Client(server_url) as client:
        created = await client.call_tool("create_plate", plate_args(110, 70, 6))
        valid = {
            "create_plate": plate_args(110, 70, 6),
            "measure": {"part": created.structured_content["part"], "density": DENSITY},
        }
        for name, arguments in valid.items():
            result = await client.call_tool(name, arguments | smuggled)
            assert result.is_error, name


@pytest.mark.parametrize("name", ["execute", "run_script", "exec", "eval", "python", "shell"])
async def test_there_is_no_tool_that_runs_code(server_url: str, name: str) -> None:
    async with Client(server_url) as client:
        with pytest.raises(MCPError):
            await client.call_tool(name, {"script": "print(1)"})


# Calls and modules that would let the server run code it was given.
FORBIDDEN_CALLS = {"exec", "eval", "compile", "__import__", "breakpoint"}
FORBIDDEN_MODULES = {
    "code",
    "codeop",
    "ctypes",
    "marshal",
    "multiprocessing",
    "pickle",
    "pty",
    "runpy",
    "shelve",
    "subprocess",
}
ALLOWED_IMPORTLIB = {"importlib.metadata", "importlib.resources"}
FORBIDDEN_OS_PREFIXES = ("system", "popen", "exec", "spawn", "fork", "posix_spawn")


def forbidden_uses(path: Path) -> list[str]:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    found: list[str] = []
    for node in ast.walk(tree):
        line = getattr(node, "lineno", 0)
        modules: list[str] = []
        if isinstance(node, ast.Import):
            modules = [alias.name for alias in node.names]
        elif isinstance(node, ast.ImportFrom) and node.module:
            modules = [node.module]
            if node.module == "importlib":
                modules = [f"importlib.{alias.name}" for alias in node.names]
        for module in modules:
            top = module.split(".")[0]
            if top in FORBIDDEN_MODULES or (top == "importlib" and module not in ALLOWED_IMPORTLIB):
                found.append(f"{path.name}:{line}: imports {module}")
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            if node.func.id in FORBIDDEN_CALLS:
                found.append(f"{path.name}:{line}: calls {node.func.id}")
        if (
            isinstance(node, ast.Attribute)
            and isinstance(node.value, ast.Name)
            and node.value.id == "os"
            and node.attr.startswith(FORBIDDEN_OS_PREFIXES)
        ):
            found.append(f"{path.name}:{line}: uses os.{node.attr}")
    return found


def test_the_server_source_cannot_run_code() -> None:
    roots = [Path(build123d_server.__file__).parent, Path(build123d_models.__file__).parent]
    sources = [p for root in roots for p in sorted(root.rglob("*.py"))]
    assert len(sources) >= 10
    assert [hit for source in sources for hit in forbidden_uses(source)] == []


def test_the_source_scan_catches_what_it_forbids(tmp_path: Path) -> None:
    sample = tmp_path / "sample.py"
    sample.write_text(
        "import subprocess\nfrom importlib import import_module\nimport os\n"
        "exec('1')\nos.system('id')\n",
        encoding="utf-8",
    )
    assert len(forbidden_uses(sample)) == 4
