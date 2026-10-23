import math
import os
import socket
import threading
import time
from collections.abc import Iterator

import pytest
import uvicorn

from build123d_server.app import MCP_PATH, create_app

# Values from docs/reference/benchmark-draft-part-design.md.
DENSITY_G_PER_CM3 = 2.70
HOLE_DIAMETER_MM = 6.5
HOLE_INSET_MM = 10.0
MASS_RELATIVE_TOLERANCE = 0.005  # 0.5 %

# The draft's worked examples: (length, width, thickness) in mm and the mass
# in g it quotes, to one decimal.
DRAFT_EXAMPLES = [
    (120.0, 80.0, 6.0, 153.4),
    (120.0, 70.0, 6.0, 133.9),
    (110.0, 70.0, 6.0, 122.6),
    (112.0, 72.0, 6.0, 128.5),
]


def closed_form_mass_g(length: float, width: float, thickness: float) -> float:
    """The draft's formula: 2.70e-3 × (L × W − 4 × π × 3.25²) × T grams, L, W, T in mm."""  # noqa: RUF002 - quoted verbatim
    return 2.70e-3 * (length * width - 4 * math.pi * 3.25**2) * thickness


def plate_args(length: float, width: float, thickness: float) -> dict[str, float]:
    return {
        "length_mm": length,
        "width_mm": width,
        "thickness_mm": thickness,
        "hole_diameter_mm": HOLE_DIAMETER_MM,
        "hole_inset_mm": HOLE_INSET_MM,
    }


DENSITY = {"value": DENSITY_G_PER_CM3, "unit": "g/cm3"}


@pytest.fixture
def anyio_backend() -> str:
    return "asyncio"


@pytest.fixture(scope="session")
def server_url() -> Iterator[str]:
    """The MCP endpoint under test.

    BUILD123D_URL when set, as in the container suite, where the server runs
    in its own container. Otherwise a server started here on loopback.
    """
    external = os.environ.get("BUILD123D_URL")
    if external:
        yield external
        return
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    server = uvicorn.Server(
        uvicorn.Config(create_app(), host="127.0.0.1", port=port, log_level="warning")
    )
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    deadline = time.monotonic() + 30
    while not server.started:
        if time.monotonic() > deadline or not thread.is_alive():
            raise RuntimeError("the test server did not start")
        time.sleep(0.05)
    yield f"http://127.0.0.1:{port}{MCP_PATH}"
    server.should_exit = True
    thread.join(timeout=10)
