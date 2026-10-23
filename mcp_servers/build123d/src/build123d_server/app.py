"""The MCP server and its streamable HTTP application."""

import os
from importlib import metadata
from typing import Any

import mcp_types as types
from mcp.server.context import ServerRequestContext
from mcp.server.lowlevel.server import Server
from mcp.server.transport_security import TransportSecuritySettings
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from build123d_server import tools

MCP_PATH = "/mcp"

# Every valid request is a few hundred bytes; the legacy service capped bodies
# at the same size.
MAX_REQUEST_BODY_BYTES = 16_384

# Host header values accepted on /mcp (DNS-rebinding protection). The compose
# service name plus loopback for local runs.
DEFAULT_ALLOWED_HOSTS = "build123d:*,127.0.0.1:*,localhost:*"


async def _list_tools(
    ctx: ServerRequestContext[Any], params: types.PaginatedRequestParams | None
) -> types.ListToolsResult:
    return types.ListToolsResult(tools=tools.list_tools())


async def _call_tool(
    ctx: ServerRequestContext[Any], params: types.CallToolRequestParams
) -> types.CallToolResult:
    return await tools.call_tool(params.name, params.arguments)


def create_server() -> Server[Any]:
    return Server(
        "build123d",
        version=metadata.version("autosys-build123d"),
        description="Typed build123d operations. No tool accepts or executes code.",
        on_list_tools=_list_tools,
        on_call_tool=_call_tool,
    )


async def _health(request: Request) -> JSONResponse:
    return JSONResponse({"ok": True, "service": "build123d"})


def create_app() -> Starlette:
    allowed_hosts = os.environ.get("BUILD123D_ALLOWED_HOSTS", DEFAULT_ALLOWED_HOSTS)
    # Stateless with JSON responses: no session survives between calls, so a
    # restarted container serves the next call like any other.
    return create_server().streamable_http_app(
        streamable_http_path=MCP_PATH,
        json_response=True,
        stateless_http=True,
        max_request_body_size=MAX_REQUEST_BODY_BYTES,
        transport_security=TransportSecuritySettings(
            enable_dns_rebinding_protection=True,
            allowed_hosts=[h.strip() for h in allowed_hosts.split(",") if h.strip()],
            allowed_origins=[],
        ),
        custom_starlette_routes=[Route("/health", _health, methods=["GET"])],
    )
