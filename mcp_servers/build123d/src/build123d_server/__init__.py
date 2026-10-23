"""MCP server with typed build123d operations.

Two tools, create_plate and measure, take only typed numeric arguments that
are validated against the canonical schemas in schemas/tools/build123d/. No
tool accepts or executes code. The server keeps no state between calls.
"""
