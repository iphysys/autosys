"""Runs the server: `python -m build123d_server`.

BUILD123D_HOST and BUILD123D_PORT choose the listening address.
"""

import logging
import os

import uvicorn

from build123d_server.app import create_app


def main() -> None:
    logging.basicConfig(level=logging.INFO)
    uvicorn.run(
        create_app(),
        host=os.environ.get("BUILD123D_HOST", "0.0.0.0"),  # noqa: S104 - reachable only inside its network
        port=int(os.environ.get("BUILD123D_PORT", "8000")),
        limit_concurrency=32,
        timeout_keep_alive=5,
        log_level="info",
    )


if __name__ == "__main__":
    main()
