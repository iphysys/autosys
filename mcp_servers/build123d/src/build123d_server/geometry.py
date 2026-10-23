"""Plate construction and measurement with build123d.

Pure functions: every call builds its solid from parameters, so nothing
survives between calls. The frame is the one create_plate_request.schema.json
states: length along X, width along Y, thickness along Z, centred on the
origin in X and Y, bottom face on Z = 0.
"""

import math
from dataclasses import dataclass

from build123d import Align, Box, Cylinder, Part, Pos

from build123d_models.create_plate_request_schema import Build123dCreatePlateRequest

# 1 cm3 = 1000 mm3, so g/cm3 times this factor is g/mm3.
G_PER_CM3_TO_G_PER_MM3 = 1e-3


class GeometryError(ValueError):
    """The parameters do not describe a valid plate."""


@dataclass(frozen=True)
class Measurement:
    volume_mm3: float
    mass_g: float
    length_mm: float
    width_mm: float
    height_mm: float


def build_plate(plate: Build123dCreatePlateRequest) -> Part:
    """A rectangular plate with one through hole at each corner."""
    radius = plate.hole_diameter_mm / 2
    if plate.hole_inset_mm <= radius:
        raise GeometryError(
            f"hole_inset_mm ({plate.hole_inset_mm}) must exceed half of hole_diameter_mm "
            f"({radius}), or the holes break the plate edges"
        )
    for name, extent in (("length_mm", plate.length_mm), ("width_mm", plate.width_mm)):
        if extent - 2 * plate.hole_inset_mm <= plate.hole_diameter_mm:
            raise GeometryError(
                f"{name} ({extent}) minus twice hole_inset_mm must exceed hole_diameter_mm "
                f"({plate.hole_diameter_mm}), or neighbouring holes meet"
            )

    thickness = plate.thickness_mm
    body = Box(
        plate.length_mm,
        plate.width_mm,
        thickness,
        align=(Align.CENTER, Align.CENTER, Align.MIN),
    )
    offset_x = plate.length_mm / 2 - plate.hole_inset_mm
    offset_y = plate.width_mm / 2 - plate.hole_inset_mm
    # The cutters are twice the plate thickness, centred on it, so the boolean
    # never relies on coplanar faces.
    holes = [
        Pos(x, y, thickness / 2) * Cylinder(radius, 2 * thickness)
        for x in (-offset_x, offset_x)
        for y in (-offset_y, offset_y)
    ]
    part = body - holes
    if not isinstance(part, Part) or not part.is_valid or len(part.solids()) != 1:
        raise GeometryError("the parameters do not give a single valid solid")
    return part


def measure(part: Part, density_g_per_cm3: float) -> Measurement:
    """Volume from OpenCASCADE mass properties, mass from volume and density,
    and the extents of the tight bounding box."""
    volume = float(part.volume)
    mass = volume * density_g_per_cm3 * G_PER_CM3_TO_G_PER_MM3
    size = part.bounding_box(optimal=True).size
    measurement = Measurement(
        volume_mm3=volume,
        mass_g=mass,
        length_mm=float(size.X),
        width_mm=float(size.Y),
        height_mm=float(size.Z),
    )
    values = (volume, mass, measurement.length_mm, measurement.width_mm, measurement.height_mm)
    if not all(math.isfinite(v) and v >= 0 for v in values):
        raise GeometryError("measuring the part gave a non-finite or negative value")
    return measurement
