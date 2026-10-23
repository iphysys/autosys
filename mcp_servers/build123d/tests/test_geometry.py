"""Plate geometry and measurement, without the MCP layer."""

import itertools
import math

import pytest

from build123d_models.create_plate_request_schema import Build123dCreatePlateRequest
from build123d_server import geometry
from conftest import (
    DENSITY_G_PER_CM3,
    DRAFT_EXAMPLES,
    MASS_RELATIVE_TOLERANCE,
    closed_form_mass_g,
    plate_args,
)


def measure(length: float, width: float, thickness: float) -> geometry.Measurement:
    plate = Build123dCreatePlateRequest(**plate_args(length, width, thickness))
    return geometry.measure(geometry.build_plate(plate), DENSITY_G_PER_CM3)


@pytest.mark.parametrize(("length", "width", "thickness", "quoted_mass"), DRAFT_EXAMPLES)
def test_mass_matches_the_draft_formula_for_its_examples(
    length: float, width: float, thickness: float, quoted_mass: float
) -> None:
    reference = closed_form_mass_g(length, width, thickness)
    mass = measure(length, width, thickness).mass_g
    assert abs(mass - reference) / reference <= MASS_RELATIVE_TOLERANCE
    # The draft quotes these masses to one decimal.
    assert round(mass, 1) == quoted_mass


@pytest.mark.parametrize(
    ("length", "width", "thickness"),
    list(itertools.product((110.0, 125.5, 140.0), (70.0, 81.25, 90.0), (6.0, 8.0, 12.5))),
)
def test_mass_matches_the_draft_formula_across_the_requirement_ranges(
    length: float, width: float, thickness: float
) -> None:
    reference = closed_form_mass_g(length, width, thickness)
    mass = measure(length, width, thickness).mass_g
    assert abs(mass - reference) / reference <= MASS_RELATIVE_TOLERANCE


def test_volume_is_the_plate_less_four_through_holes() -> None:
    m = measure(120.0, 80.0, 6.0)
    expected = (120.0 * 80.0 - 4 * math.pi * 3.25**2) * 6.0
    assert m.volume_mm3 == pytest.approx(expected, rel=1e-9)
    assert m.mass_g == pytest.approx(m.volume_mm3 * DENSITY_G_PER_CM3 * 1e-3, rel=1e-12)


def test_bounding_box_is_the_plate_envelope() -> None:
    m = measure(112.0, 72.0, 6.0)
    assert m.length_mm == pytest.approx(112.0, abs=1e-6)
    assert m.width_mm == pytest.approx(72.0, abs=1e-6)
    assert m.height_mm == pytest.approx(6.0, abs=1e-6)


@pytest.mark.parametrize(
    "overrides",
    [
        # The holes would break the edges.
        {"hole_inset_mm": 3.0},
        {"hole_inset_mm": 3.25},
        # Neighbouring holes would meet along the length, then the width.
        {"length_mm": 26.5},
        {"width_mm": 20.0},
    ],
)
def test_impossible_plates_are_rejected(overrides: dict[str, float]) -> None:
    plate = Build123dCreatePlateRequest(**(plate_args(110.0, 70.0, 6.0) | overrides))
    with pytest.raises(geometry.GeometryError):
        geometry.build_plate(plate)
