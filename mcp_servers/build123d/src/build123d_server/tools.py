"""The two tools: argument validation, dispatch and error observations.

Arguments are checked against the canonical schema (bundled into
build123d_models/schemas/ by `cargo xtask codegen`) and then parsed into the
generated models. A failure is returned as an error result carrying
{code, message, retryable}, never as success.
"""

import json
import logging
import threading
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from importlib import resources
from typing import Any

import anyio
import mcp_types as types
from jsonschema import Draft202012Validator
from mcp.shared.exceptions import MCPError
from pydantic import BaseModel, ValidationError

from build123d_models.create_plate_request_schema import Build123dCreatePlateRequest
from build123d_models.create_plate_result_schema import Build123dCreatePlateResult
from build123d_models.measure_request_schema import Build123dMeasureRequest
from build123d_models.measure_result_schema import (
    Build123dBoundingBox,
    Build123dLength,
    Build123dMass,
    Build123dMeasureResult,
    Build123dVolume,
)
from build123d_server import geometry, handles

logger = logging.getLogger(__name__)

# Bounds the text an error echoes back, as the legacy service bounded its output.
MAX_ERROR_CHARS = 2_000

# OpenCASCADE is not documented as thread-safe, so CAD work runs one call at a
# time, as in the legacy service's execution lock.
_CAD_LOCK = threading.Lock()


class ToolError(Exception):
    """A failure reported to the caller as an error observation."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


@dataclass(frozen=True)
class ToolSpec[RequestT: BaseModel]:
    name: str
    title: str
    description: str
    request_model: type[RequestT]
    run: Callable[[RequestT], BaseModel]
    input_schema: dict[str, Any]
    output_schema: dict[str, Any]

    def descriptor(self) -> types.Tool:
        return types.Tool(
            name=self.name,
            title=self.title,
            description=self.description,
            input_schema=self.input_schema,
            output_schema=self.output_schema,
            annotations=types.ToolAnnotations(
                read_only_hint=True,
                destructive_hint=False,
                idempotent_hint=True,
                open_world_hint=False,
            ),
        )


def _schema(stem: str) -> dict[str, Any]:
    text = (resources.files("build123d_models") / "schemas" / f"{stem}.schema.json").read_text(
        encoding="utf-8"
    )
    schema: dict[str, Any] = json.loads(text)
    return schema


def _create_plate(request: Build123dCreatePlateRequest) -> Build123dCreatePlateResult:
    geometry.build_plate(request)
    return Build123dCreatePlateResult(part=handles.make_handle(request))


def _measure(request: Build123dMeasureRequest) -> Build123dMeasureResult:
    if not handles.matches(request.part):
        raise ToolError(
            "invalid_part_handle",
            "part_id does not match the part's parameters; pass the handle create_plate "
            "returned, unchanged",
        )
    solid = geometry.build_plate(request.part.plate)
    m = geometry.measure(solid, request.density.value)
    return Build123dMeasureResult(
        part_id=request.part.part_id,
        volume=Build123dVolume(value=m.volume_mm3, unit="mm3"),
        mass=Build123dMass(value=m.mass_g, unit="g"),
        bounding_box=Build123dBoundingBox(
            length=Build123dLength(value=m.length_mm, unit="mm"),
            width=Build123dLength(value=m.width_mm, unit="mm"),
            height=Build123dLength(value=m.height_mm, unit="mm"),
        ),
    )


TOOLS: Mapping[str, ToolSpec[Any]] = {
    spec.name: spec
    for spec in (
        ToolSpec(
            name="create_plate",
            title="Create plate",
            description=(
                "Build a flat rectangular plate with one through hole at each corner from "
                "dimensions in millimetres. Returns a part handle to pass to measure."
            ),
            request_model=Build123dCreatePlateRequest,
            run=_create_plate,
            input_schema=_schema("create_plate_request"),
            output_schema=_schema("create_plate_result"),
        ),
        ToolSpec(
            name="measure",
            title="Measure part",
            description=(
                "Measure a part from create_plate: volume (mm3), mass (g) from the given "
                "density (g/cm3), and bounding-box length, width and height (mm)."
            ),
            request_model=Build123dMeasureRequest,
            run=_measure,
            input_schema=_schema("measure_request"),
            output_schema=_schema("measure_result"),
        ),
    )
}

_VALIDATORS = {
    name: (Draft202012Validator(spec.input_schema), Draft202012Validator(spec.output_schema))
    for name, spec in TOOLS.items()
}


def _bounded(text: str) -> str:
    return text if len(text) <= MAX_ERROR_CHARS else text[: MAX_ERROR_CHARS - 3] + "..."


def _error_result(code: str, message: str) -> types.CallToolResult:
    error = {"code": code, "message": _bounded(message), "retryable": False}
    return types.CallToolResult(
        content=[types.TextContent(type="text", text=json.dumps(error))],
        is_error=True,
    )


def _locked[RequestT: BaseModel](
    run: Callable[[RequestT], BaseModel], request: RequestT
) -> BaseModel:
    with _CAD_LOCK:
        return run(request)


def list_tools() -> list[types.Tool]:
    return [spec.descriptor() for spec in TOOLS.values()]


async def call_tool(name: str, arguments: Mapping[str, Any] | None) -> types.CallToolResult:
    spec = TOOLS.get(name)
    if spec is None:
        raise MCPError(
            code=types.INVALID_PARAMS,
            message=f"unknown tool {name!r}; available: {', '.join(TOOLS)}",
        )
    input_validator, output_validator = _VALIDATORS[name]
    args = dict(arguments or {})
    problems = sorted(
        f"{'/'.join(str(p) for p in error.absolute_path) or '(root)'}: {error.message}"
        for error in input_validator.iter_errors(args)
    )
    if problems:
        return _error_result("invalid_arguments", "; ".join(problems))
    try:
        request = spec.request_model.model_validate(args)
    except ValidationError as exc:
        return _error_result("invalid_arguments", str(exc))

    try:
        result = await anyio.to_thread.run_sync(_locked, spec.run, request)
    except ToolError as exc:
        return _error_result(exc.code, exc.message)
    except geometry.GeometryError as exc:
        return _error_result("invalid_geometry", str(exc))
    except Exception:
        logger.exception("tool %s failed", name)
        return _error_result("internal", f"{name} failed inside the tool server")

    structured = result.model_dump(mode="json")
    if any(True for _ in output_validator.iter_errors(structured)):
        logger.error("tool %s produced a result that violates its schema", name)
        return _error_result("internal", f"{name} produced an invalid result")
    return types.CallToolResult(
        content=[types.TextContent(type="text", text=json.dumps(structured))],
        structured_content=structured,
        is_error=False,
    )
