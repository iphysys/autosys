"""The server over MCP streamable HTTP, as the harness's tool gateway reaches it."""

import json
import urllib.error
import urllib.request
from typing import Any
from urllib.parse import urlsplit, urlunsplit

import mcp_types as types
import pytest
from mcp import Client

from build123d_server import tools
from build123d_server.app import MAX_REQUEST_BODY_BYTES
from conftest import (
    DENSITY,
    DRAFT_EXAMPLES,
    MASS_RELATIVE_TOLERANCE,
    closed_form_mass_g,
    plate_args,
)

pytestmark = pytest.mark.anyio


def base_url(server_url: str) -> str:
    parts = urlsplit(server_url)
    return urlunsplit((parts.scheme, parts.netloc, "", "", ""))


async def test_discovery_lists_exactly_the_two_tools_with_canonical_schemas(
    server_url: str,
) -> None:
    async with Client(server_url) as client:
        listed = (await client.list_tools()).tools
    assert sorted(t.name for t in listed) == ["create_plate", "measure"]
    for tool in listed:
        spec = tools.TOOLS[tool.name]
        assert tool.input_schema == spec.input_schema
        assert tool.output_schema == spec.output_schema
        assert tool.annotations is not None
        assert tool.annotations.read_only_hint is True
        assert tool.annotations.idempotent_hint is True
        assert tool.annotations.open_world_hint is False


@pytest.mark.parametrize(("length", "width", "thickness", "quoted_mass"), DRAFT_EXAMPLES)
async def test_reported_mass_matches_the_draft_formula_over_mcp(
    server_url: str, length: float, width: float, thickness: float, quoted_mass: float
) -> None:
    async with Client(server_url) as client:
        created = await client.call_tool("create_plate", plate_args(length, width, thickness))
        assert not created.is_error, created.content
        measured = await client.call_tool(
            "measure", {"part": created.structured_content["part"], "density": DENSITY}
        )
    assert not measured.is_error, measured.content
    mass: dict[str, Any] = measured.structured_content["mass"]
    assert mass["unit"] == "g"
    reference = closed_form_mass_g(length, width, thickness)
    assert abs(mass["value"] - reference) / reference <= MASS_RELATIVE_TOLERANCE


async def test_a_failed_call_is_an_error_result_not_success(server_url: str) -> None:
    async with Client(server_url) as client:
        result = await client.call_tool(
            "create_plate", plate_args(110, 70, 6) | {"hole_inset_mm": 1}
        )
    assert result.is_error
    content = result.content[0]
    assert isinstance(content, types.TextContent)
    assert json.loads(content.text)["code"] == "invalid_geometry"


def test_health(server_url: str) -> None:
    with urllib.request.urlopen(base_url(server_url) + "/health", timeout=10) as response:  # noqa: S310 - test URL
        assert json.load(response) == {"ok": True, "service": "build123d"}


def test_oversized_requests_are_refused(server_url: str) -> None:
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "create_plate", "arguments": {"pad": "x" * MAX_REQUEST_BODY_BYTES}},
        }
    ).encode()
    request = urllib.request.Request(  # noqa: S310 - test URL
        server_url,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        },
    )
    with pytest.raises(urllib.error.HTTPError) as caught:
        urllib.request.urlopen(request, timeout=10)  # noqa: S310 - test URL
    assert caught.value.code == 413
