"""Lock tests for the shared schema types in common/types.py.

RegionCoord.region_id is seam #2 (the USD layer schema): every specialist writes
its layer under layers/<specialist>/<region_id>.usda and Dev A reads layers back
by region, so the string format is a load-bearing cross-team contract. These tests
pin the format (forced sign, zero-pad, layer suffix) the way the mesh proto / ABI
lock tests pin the other seams. Other tests touch region_id only indirectly, for a
few small non-zero coords; the zero case and the width boundary live only here.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_types.py -v
"""

from common.types import JobKind, RegionCoord


def test_job_kind_wire_name_matches_proto_snake_case():
    # The mesh proto serializes JobKind with rename_all = "snake_case", so the
    # coordinator's POST /jobs accepts these exact strings and rejects an unknown
    # kind. The agents JobKind is an IntEnum (integer wire form), so the producer
    # must emit wire_name, not the int. Pin each variant to the proto spelling so a
    # rename on either side of the seam breaks loudly here.
    assert JobKind.TERRAIN.wire_name == "terrain"
    assert JobKind.FOLIAGE.wire_name == "foliage"
    assert JobKind.NPC_TICK.wire_name == "npc_tick"
    assert JobKind.DIFFUSION_TILE.wire_name == "diffusion_tile"
    assert JobKind.OPTIMIZATION.wire_name == "optimization"
    # No member forgotten: every variant maps to one of the five proto spellings.
    assert {k.wire_name for k in JobKind} == {
        "terrain", "foliage", "npc_tick", "diffusion_tile", "optimization"
    }


def test_region_id_forces_sign_and_zero_pads():
    # Both coords carry an explicit sign and are zero-padded to 4 digits; the layer
    # is appended raw after "_l". The canonical example (42, -17, 0) matches the
    # comment in types.py and the CLI tests.
    assert RegionCoord(x=42, y=-17, layer=0).region_id == "r+0042_-0017_l0"
    # Zero forces a "+" sign too — the case no other test exercises.
    assert RegionCoord(x=0, y=0, layer=0).region_id == "r+0000_+0000_l0"
    # Negative x, positive y, non-zero layer.
    assert RegionCoord(x=-4, y=3, layer=2).region_id == "r-0004_+0003_l2"
    # layer defaults to 0 (ground) when omitted.
    assert RegionCoord(x=1, y=2).region_id == "r+0001_+0002_l0"


def test_region_id_width_is_a_floor_beyond_four_digits():
    # Within +/-9999 each coord field is exactly 5 chars (sign + 4 digits)...
    assert RegionCoord(x=9999, y=-9999, layer=0).region_id == "r+9999_-9999_l0"
    # ...beyond that the width is a floor, not a cap: fields grow rather than truncate,
    # so the id stays unambiguous (parse by "_" delimiter, never by fixed offset).
    assert RegionCoord(x=10000, y=-10000, layer=0).region_id == "r+10000_-10000_l0"
