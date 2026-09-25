"""Tests for the portal bench's door ranking (tools/bench/portal_bench_doors.py).

Run with: python -m unittest tools/bench/test_portal_bench_doors.py
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import portal_bench_doors as bench  # noqa: E402

WHITERUN = ("worldspace", 1)
SOLITUDE = ("worldspace", 2)
TAMRIEL = ("worldspace", 3)
DRAGONSREACH = ("interior", 10)
JARLS_QUARTERS = ("interior", 11)
BLUE_PALACE = ("interior", 20)
FARMHOUSE = ("interior", 30)


def refs(count, at=(0.0, 0.0, 0.0), spread=10.0):
    x, y, z = at
    return [(x + i * spread, y, z) for i in range(count)]


def fixture():
    doors = [
        # Whiterun: a heavy door into Dragonsreach and a light one into the same place.
        bench.Door(0x100, WHITERUN, (0.0, 0.0, 0.0), 0x200, DRAGONSREACH, "WRDoor.nif"),
        bench.Door(0x101, WHITERUN, (50000.0, 0.0, 0.0), 0x201, DRAGONSREACH, "WRDoor.nif"),
        # An auto-load marker, however heavy, is never benched.
        bench.Door(0x102, WHITERUN, (0.0, 0.0, 0.0), 0x202, DRAGONSREACH, "AutoLoadMarker01.nif"),
        # Solitude's door into the Blue Palace.
        bench.Door(0x110, SOLITUDE, (0.0, 0.0, 0.0), 0x210, BLUE_PALACE, "SDoor.nif"),
        # Dragonsreach up to the Jarl's quarters: interior-to-interior, one hop inside the city.
        bench.Door(0x200, DRAGONSREACH, (0.0, 0.0, 0.0), 0x300, JARLS_QUARTERS, "Door.nif"),
        # A Creation Club door (load-order slot 0x10) is left out.
        bench.Door(0x10000100, WHITERUN, (0.0, 0.0, 0.0), 0x203, DRAGONSREACH, "Door.nif"),
        # Outside the cities: never picked.
        bench.Door(0x120, TAMRIEL, (0.0, 0.0, 0.0), 0x220, FARMHOUSE, "Door.nif"),
    ]
    references = {
        WHITERUN: refs(40) + refs(5, at=(50000.0, 0.0, 0.0)) + refs(3, at=(9000.0, 0.0, 0.0)),
        SOLITUDE: refs(10),
        TAMRIEL: refs(1000),
        DRAGONSREACH: refs(100),
        JARLS_QUARTERS: refs(30),
        BLUE_PALACE: refs(70),
        FARMHOUSE: refs(5),
    }
    worldspaces = {1: "WhiterunWorld", 2: "SolitudeWorld\x00", 3: "Tamriel"}
    names = {10: "WhiterunDragonsreach", 11: "JarlsQuarters", 20: "BluePalace", 30: "Farm"}
    return bench.World(doors, references, worldspaces, names)


class RankingTests(unittest.TestCase):
    def test_counts_references_near_the_door_and_beyond_it(self):
        scored = {door.ref_id: door for door in bench.score_doors(fixture())}
        heavy = scored[0x100]
        # 40 references within the radius; the 5 at 50000 and the 3 at 9000 are out of it.
        self.assertEqual(heavy.near, 40)
        self.assertEqual(heavy.beyond, 100)
        self.assertEqual(scored[0x101].near, 5)

    def test_only_benchable_city_doors_are_scored(self):
        scored = {door.ref_id for door in bench.score_doors(fixture())}
        self.assertNotIn(0x102, scored, "auto-load marker")
        self.assertNotIn(0x10000100, scored, "Creation Club door")
        self.assertNotIn(0x120, scored, "outside the cities")
        self.assertIn(0x200, scored, "an interior one hop inside a city counts as the city")

    def test_the_pick_takes_one_door_per_place_first_then_the_heaviest(self):
        picked = bench.pick_doors(fixture(), exterior=2, interior=1)
        self.assertEqual([door.ref_id for door in picked], [0x100, 0x110, 0x200])
        self.assertEqual(picked[1].place, "Solitude")
        self.assertEqual(picked[2].kind, "interior-to-interior")

    def test_the_pick_is_deterministic(self):
        first = [door.ref_id for door in bench.pick_doors(fixture())]
        second = [door.ref_id for door in bench.pick_doors(fixture())]
        self.assertEqual(first, second)

    def test_a_door_is_written_with_its_space_counts_and_reason(self):
        door = bench.pick_doors(fixture(), exterior=1, interior=0)[0]
        written = bench.door_json(door)
        self.assertEqual(written["ref_id"], "0x00000100")
        self.assertEqual(written["worldspace_id"], 1)
        self.assertIsNone(written["interior_cell_id"])
        self.assertEqual((written["near_references"], written["beyond_references"]), (40, 100))
        self.assertIn("Whiterun", written["reason"])


if __name__ == "__main__":
    unittest.main()
