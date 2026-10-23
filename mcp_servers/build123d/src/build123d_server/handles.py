"""Part handles: the parameters a part is rebuilt from, plus an ID derived from them.

The ID is a hash of the validated parameters, so equal parameters give equal
IDs (both tools are idempotent) and a handle whose parameters were edited
after create_plate no longer matches its ID.
"""

import hashlib
import json

from build123d_models.create_plate_request_schema import Build123dCreatePlateRequest
from build123d_models.part_schema import Build123dPart

# Changing how parts are built or hashed changes this tag, so old IDs stop matching.
_DOMAIN = b"autosys.build123d.plate.v1\n"


def part_id(plate: Build123dCreatePlateRequest) -> str:
    # Fields are floats after validation, so 110 and 110.0 hash alike.
    canonical = json.dumps(
        plate.model_dump(mode="json"),
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    )
    return "sha256:" + hashlib.sha256(_DOMAIN + canonical.encode()).hexdigest()


def make_handle(plate: Build123dCreatePlateRequest) -> Build123dPart:
    return Build123dPart(part_id=part_id(plate), plate=plate)


def matches(handle: Build123dPart) -> bool:
    return handle.part_id == part_id(handle.plate)
