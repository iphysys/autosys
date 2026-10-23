# build123d tool server

An MCP server with two typed build123d operations. No tool accepts code or a script, and the server keeps no state between calls.

| Tool | Arguments | Result |
| --- | --- | --- |
| `create_plate` | `length_mm`, `width_mm`, `thickness_mm`, `hole_diameter_mm`, `hole_inset_mm` | `part`: a handle with `part_id` and the plate parameters |
| `measure` | `part` (from `create_plate`, unchanged), `density` `{value, unit: "g/cm3"}` | `volume` (`mm3`), `mass` (`g`), `bounding_box` `length`, `width`, `height` (`mm`) |

- **Frame.** Length runs along X, width along Y and thickness along Z. The plate is centred on the origin in X and Y, with its bottom face on Z = 0.
- **Holes.** There are four, through the plate, one at each corner. Each hole centre sits `hole_inset_mm` from both adjacent edges.
- **Part handle.** `part_id` is a hash of the parameters. Equal parameters give equal IDs, and `measure` rejects a handle whose parameters no longer match its ID.
- **Errors** are MCP error results carrying `{code, message, retryable}`. The codes are `invalid_arguments`, `invalid_geometry`, `invalid_part_handle` and `internal`. An unknown tool name is a protocol error.

The canonical schemas are in [`schemas/tools/build123d/`](../../schemas/tools/build123d/). [`models/build123d_models/`](models/build123d_models/) is generated from them by `cargo xtask codegen`: pydantic models, plus self-contained copies of the schemas, which are what the server advertises and validates against. Never edit it by hand.

## Layout

- `src/build123d_server/`
  - `geometry.py`: builds and measures the plate with build123d;
  - `handles.py`: part IDs;
  - `tools.py`: validation, dispatch and error results;
  - `app.py`: the MCP server over streamable HTTP at `/mcp` (stateless, JSON responses, 16 KiB request limit), plus `/health`.
- `tests/`
  - `test_geometry.py`: mass against the draft benchmark's closed-form formula, within 0.5 %;
  - `test_tools.py`, `test_mcp_http.py`: the tools, in process and over HTTP;
  - `test_no_code.py`: advertised schemas admit no free-form text; code-carrying calls are refused; the source never runs code;
  - `test_egress.py`, `egress_probe.py`: no internet from the compose network.

## Run

The container, from the repository root (see the main README, section Tool servers):

```sh
docker compose -f deploy/compose/tools.yaml up -d --wait build123d
cargo xtask test-tools
```

The tests without Docker, from this directory:

```sh
uv sync --frozen
uv run --frozen pytest
uv run --frozen ruff check && uv run --frozen ruff format --check && uv run --frozen mypy
```

`BUILD123D_HOST`, `BUILD123D_PORT` and `BUILD123D_ALLOWED_HOSTS` (accepted `Host` headers) configure the server. Its defaults suit the compose service.
