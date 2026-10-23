"""The tool handlers: validation, results and error observations."""

import json
from typing import Any

import mcp_types as types
import pytest
from jsonschema import Draft202012Validator
from mcp.shared.exceptions import MCPError

from build123d_server import tools
from conftest import DENSITY, MASS_RELATIVE_TOLERANCE, closed_form_mass_g, plate_args

pytestmark = pytest.mark.anyio


def error_of(result: types.CallToolResult) -> dict[str, Any]:
    assert result.is_error
    assert result.structured_content is None
    content = result.content[0]
    assert isinstance(content, types.TextContent)
    error: dict[str, Any] = json.loads(content.text)
    assert set(error) == {"code", "message", "retryable"}
    assert error["retryable"] is False
    return error


async def structured(name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    result = await tools.call_tool(name, arguments)
    assert not result.is_error, result.content
    value: dict[str, Any] = result.structured_content
    Draft202012Validator(tools.TOOLS[name].output_schema).validate(value)
    return value


async def test_create_then_measure_reports_units_and_the_draft_mass() -> None:
    created = await structured("create_plate", plate_args(110, 70, 6))
    measured = await structured("measure", {"part": created["part"], "density": DENSITY})

    assert measured["part_id"] == created["part"]["part_id"]
    assert measured["volume"]["unit"] == "mm3"
    assert measured["mass"]["unit"] == "g"
    assert {v["unit"] for v in measured["bounding_box"].values()} == {"mm"}
    reference = closed_form_mass_g(110, 70, 6)
    assert abs(measured["mass"]["value"] - reference) / reference <= MASS_RELATIVE_TOLERANCE


async def test_equal_parameters_give_the_same_part_id() -> None:
    a = await structured("create_plate", plate_args(110, 70, 6))
    b = await structured("create_plate", plate_args(110.0, 70.0, 6.0))
    c = await structured("create_plate", plate_args(111, 70, 6))
    assert a == b
    assert a["part"]["part_id"] != c["part"]["part_id"]


async def test_measure_rejects_a_handle_edited_after_create_plate() -> None:
    created = await structured("create_plate", plate_args(110, 70, 6))
    part = created["part"]
    part["plate"]["length_mm"] = 100
    result = await tools.call_tool("measure", {"part": part, "density": DENSITY})
    assert error_of(result)["code"] == "invalid_part_handle"


@pytest.mark.parametrize(
    "density",
    [
        {"value": 2700, "unit": "kg/m3"},
        {"value": 0, "unit": "g/cm3"},
        {"value": "2.70", "unit": "g/cm3"},
    ],
)
async def test_measure_rejects_other_densities(density: dict[str, Any]) -> None:
    created = await structured("create_plate", plate_args(110, 70, 6))
    result = await tools.call_tool("measure", {"part": created["part"], "density": density})
    assert error_of(result)["code"] == "invalid_arguments"


@pytest.mark.parametrize(
    "arguments",
    [
        {},
        plate_args(110, 70, 6) | {"length_mm": "110"},
        plate_args(110, 70, 6) | {"length_mm": True},
        plate_args(110, 70, 6) | {"thickness_mm": 0},
        plate_args(110, 70, 6) | {"width_mm": -70},
        {k: v for k, v in plate_args(110, 70, 6).items() if k != "hole_inset_mm"},
    ],
)
async def test_create_plate_rejects_invalid_arguments(arguments: dict[str, Any]) -> None:
    result = await tools.call_tool("create_plate", arguments)
    assert error_of(result)["code"] == "invalid_arguments"


async def test_impossible_geometry_is_an_error_observation() -> None:
    result = await tools.call_tool("create_plate", plate_args(110, 70, 6) | {"hole_inset_mm": 3})
    error = error_of(result)
    assert error["code"] == "invalid_geometry"
    assert "hole_inset_mm" in error["message"]


async def test_unknown_tools_are_a_protocol_error() -> None:
    with pytest.raises(MCPError) as caught:
        await tools.call_tool("create_box", {})
    assert caught.value.error.code == types.INVALID_PARAMS
