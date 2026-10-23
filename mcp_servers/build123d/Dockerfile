# syntax=docker/dockerfile:1
#
# build123d MCP server. The build context is this directory, so the image
# holds only the server, its generated models and its locked dependencies.
#
# Targets:
#   runtime  the server; what the compose service `build123d` runs
#   test     runtime plus the dev tools and tests; `build123d-test` runs it
#
# Base images are pinned by digest. To update, change tag and digest together.

ARG PYTHON_IMAGE=python:3.12.13-slim-bookworm@sha256:4766d8b510c428e595d74b9cc5bbb2fae8e26316fffb4adc89908d79aacd58a2
ARG UV_IMAGE=ghcr.io/astral-sh/uv:0.11.26@sha256:3d868e555f8f1dbc324afa005066cd11e1053fc4743b9808ca8025283e65efa5

FROM ${UV_IMAGE} AS uv

FROM ${PYTHON_IMAGE} AS system
# OpenCASCADE (the OCP wheel) loads libGL and libexpat when it is imported,
# even though nothing is rendered.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends libgl1 libexpat1 \
    && rm -rf /var/lib/apt/lists/*
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PATH=/opt/venv/bin:$PATH

FROM system AS build
COPY --from=uv /uv /usr/local/bin/uv
ENV UV_PROJECT_ENVIRONMENT=/opt/venv \
    UV_PYTHON_DOWNLOADS=never \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy
WORKDIR /src
COPY pyproject.toml uv.lock ./
RUN uv sync --frozen --no-dev --no-install-project
COPY src ./src
COPY models ./models
RUN uv sync --frozen --no-dev --no-editable

FROM build AS build-test
RUN uv sync --frozen --no-editable

FROM system AS runtime
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin app
COPY --from=build /opt/venv /opt/venv
USER 10001:10001
WORKDIR /tmp
EXPOSE 8000
CMD ["python", "-m", "build123d_server"]

FROM runtime AS test
COPY --from=build-test /opt/venv /opt/venv
WORKDIR /app
COPY pyproject.toml ./
COPY src ./src
COPY tests ./tests
# The root filesystem is read-only in compose; caches go to the /tmp tmpfs.
# mypy's cache for build123d and OCP overflows that 64 MB tmpfs, and the
# container is single-use, so the mypy cache is disabled.
ENV MYPY_CACHE_DIR=/dev/null \
    RUFF_CACHE_DIR=/tmp/ruff
CMD ["sh", "-c", "ruff check && ruff format --check && mypy && pytest -p no:cacheprovider"]
