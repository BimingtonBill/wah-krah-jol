"""Pick the portal bench's stress doors from the world database.

The portal bench (`engine --portal-bench <doors.json>`) times the doorway at a fixed list of load
doors. This script writes that list: the doors where the portal has the most to draw, in the big
cities (Whiterun, Solitude, Windhelm, Markarth, Riften and their interiors), mixing
exterior-to-interior and interior-to-interior doors, spread over several places.

A door's load is two counts:

* ``near``: the references within ``RADIUS`` units of the door, in the door's own space - what the
  main camera draws around the doorway;
* ``beyond``: what the portal draws through it - every reference of the destination cell for an
  interior, or the references within ``RADIUS`` of the destination door for an exterior.

The score is ``near + beyond``. The picked list is deterministic for a given database, so running
the script again reproduces ``tools/bench/portal_bench_doors.json`` exactly.

Usage::

    python tools/bench/portal_bench_doors.py [--db $OPENSKYRIM_CONVERTED_DIR/skyrim_world.db]
        [--out tools/bench/portal_bench_doors.json]

The database is opened read-only.
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_DB = "$OPENSKYRIM_CONVERTED_DIR/skyrim_world.db"
DEFAULT_OUT = Path(__file__).with_name("portal_bench_doors.json")

#: How far around a door its references count, in Creation units.
RADIUS = 3000.0

#: The city worldspaces, by editor id, and the place name each is reported under.
CITIES = {
    "WhiterunWorld": "Whiterun",
    "SolitudeWorld": "Solitude",
    "WindhelmWorld": "Windhelm",
    "MarkarthWorld": "Markarth",
    "RiftenWorld": "Riften",
}

#: How many doors of each kind the list takes.
EXTERIOR_DOORS = 5
INTERIOR_DOORS = 3

#: Auto-load markers cross on contact and have no leaf to open: the bench opens doors with `E`.
AUTO_LOAD_MODEL = "autoloadmarker"

#: The highest load-order slot a benched door may come from: Skyrim.esm, Update.esm and the three
#: DLCs (00-04). Creation Club and mod content differs between installs, and the bench compares runs.
MAX_LOAD_ORDER_SLOT = 0x04


@dataclass
class Door:
    """One load door, as the ranking reads it."""

    ref_id: int
    #: ("worldspace", id) or ("interior", id): the space the door stands in.
    space: tuple[str, int]
    position: tuple[float, float, float]
    destination_ref: int
    #: The space the link leads into, or None when unresolved.
    destination: tuple[str, int] | None
    model: str = ""
    #: The place (city) the door counts for, or None outside the cities.
    place: str | None = None
    near: int = 0
    beyond: int = 0
    label: str = ""
    #: The name of the space the door stands in.
    source_label: str = ""

    @property
    def score(self) -> int:
        return self.near + self.beyond

    @property
    def kind(self) -> str:
        """``exterior-to-interior``, ``interior-to-interior`` or another pairing."""
        source = "exterior" if self.space[0] == "worldspace" else "interior"
        target = "exterior" if self.destination and self.destination[0] == "worldspace" else "interior"
        return f"{source}-to-{target}"


@dataclass
class World:
    """What the ranking needs from the database, loaded once."""

    doors: list[Door]
    #: References by space: a list of positions.
    references: dict[tuple[str, int], list[tuple[float, float, float]]] = field(default_factory=dict)
    #: Worldspace editor ids by id.
    worldspaces: dict[int, str] = field(default_factory=dict)
    #: Interior names by cell id.
    interior_names: dict[int, str] = field(default_factory=dict)


def within(position, others, radius=RADIUS) -> int:
    """How many of ``others`` lie within ``radius`` of ``position`` (3D distance)."""
    limit = radius * radius
    x, y, z = position
    return sum(
        1 for (ox, oy, oz) in others if (ox - x) ** 2 + (oy - y) ** 2 + (oz - z) ** 2 <= limit
    )


def city_places(world: World) -> dict[tuple[str, int], str]:
    """The spaces that count as each city: its worldspace, every interior a door of the city leads
    into, and every interior one door further in (Dragonsreach's upper floors, the Ratway)."""
    places: dict[tuple[str, int], str] = {}
    for ws_id, editor_id in world.worldspaces.items():
        editor_id = editor_id.rstrip("\x00")
        if editor_id in CITIES:
            places[("worldspace", ws_id)] = CITIES[editor_id]
    for _hop in range(2):
        for door in world.doors:
            place = places.get(door.space)
            if place and door.destination and door.destination[0] == "interior":
                places.setdefault(door.destination, place)
    return places


def space_name(world: World, space: tuple[str, int]) -> str:
    """A space's editor id: the worldspace's, or the interior cell's."""
    if space[0] == "worldspace":
        return world.worldspaces.get(space[1], "")
    return world.interior_names.get(space[1], "")


def benchable(door: Door) -> bool:
    """A door the bench can open with `E` and every install has: linked, not an auto-load marker,
    and from the base game or its DLCs at both ends."""
    return (
        door.destination is not None
        and AUTO_LOAD_MODEL not in door.model.lower()
        and door.ref_id >> 24 <= MAX_LOAD_ORDER_SLOT
        and door.destination_ref >> 24 <= MAX_LOAD_ORDER_SLOT
    )


def score_doors(world: World) -> list[Door]:
    """Every benchable city door, with its place and counts filled in."""
    places = city_places(world)
    by_ref = {door.ref_id: door for door in world.doors}
    scored = []
    for door in world.doors:
        door.place = places.get(door.space)
        if not door.place or not benchable(door):
            continue
        door.near = within(door.position, world.references.get(door.space, []))
        if door.destination[0] == "interior":
            door.beyond = len(world.references.get(door.destination, []))
        else:
            far = by_ref.get(door.destination_ref)
            door.beyond = (
                within(far.position, world.references.get(door.destination, [])) if far else 0
            )
        door.source_label = space_name(world, door.space)
        if door.destination[0] == "interior":
            door.label = world.interior_names.get(door.destination[1], "")
        else:
            door.label = world.worldspaces.get(door.destination[1], "")
        scored.append(door)
    return scored


def pick_doors(world: World, exterior=EXTERIOR_DOORS, interior=INTERIOR_DOORS) -> list[Door]:
    """The bench's list: the heaviest exterior-to-interior door of each city (best cities first),
    then the heaviest interior-to-interior city doors, one per city while cities last.

    Ties break on the reference id, so the list is the same on every run."""
    scored = score_doors(world)
    order = lambda door: (-door.score, door.ref_id)  # noqa: E731
    picked: list[Door] = []

    def take(candidates: list[Door], count: int) -> None:
        # One door per place first, then the next heaviest wherever it stands.
        candidates = sorted(candidates, key=order)
        places_taken: set[str] = set()
        chosen: list[Door] = []
        for door in candidates:
            if len(chosen) < count and door.place not in places_taken:
                chosen.append(door)
                places_taken.add(door.place)
        for door in candidates:
            if len(chosen) < count and door not in chosen:
                chosen.append(door)
        picked.extend(sorted(chosen, key=order))

    take([d for d in scored if d.kind == "exterior-to-interior"], exterior)
    take([d for d in scored if d.kind == "interior-to-interior"], interior)
    return picked


def reason(door: Door) -> str:
    what = "the destination cell" if door.destination[0] == "interior" else "the far door's surroundings"
    return (
        f"{door.place}, {door.kind}: {door.near} references within {RADIUS:.0f} units of the door, "
        f"{door.beyond} in {what}"
    )


def door_json(door: Door) -> dict:
    space_kind, space_id = door.space
    return {
        "ref_id": f"0x{door.ref_id:08X}",
        "place": door.place,
        "kind": door.kind,
        "space": door.source_label,
        "worldspace_id": space_id if space_kind == "worldspace" else None,
        "interior_cell_id": space_id if space_kind == "interior" else None,
        "position": [round(v, 2) for v in door.position],
        "destination": door.label,
        "near_references": door.near,
        "beyond_references": door.beyond,
        "reason": reason(door),
    }


def load_world(connection: sqlite3.Connection) -> World:
    """Reads the doors (the portal graph's query, `crates/engine/src/portal_graph.rs`) and every
    reference position by space."""
    doors = []
    rows = connection.execute(
        'SELECT d.ref_id, r.cell_id, c.worldspace_id, r.pos_x, r.pos_y, r.pos_z, '
        "d.destination_ref_id, d.destination_cell_id, d.destination_worldspace_id, "
        "COALESCE(s.model_path, '') "
        'FROM door_links d JOIN "references" r ON r.id = d.ref_id '
        "LEFT JOIN cells c ON c.id = r.cell_id LEFT JOIN statics s ON s.id = r.base_form_id "
        "ORDER BY d.ref_id"
    )
    for ref_id, cell_id, ws_id, x, y, z, dest_ref, dest_cell, dest_ws, model in rows:
        space = ("worldspace", ws_id) if ws_id is not None else ("interior", cell_id)
        if dest_ws is not None:
            destination = ("worldspace", dest_ws)
        elif dest_cell is not None:
            destination = ("interior", dest_cell)
        else:
            destination = None
        doors.append(Door(ref_id, space, (x, y, z), dest_ref, destination, model))
    references: dict[tuple[str, int], list[tuple[float, float, float]]] = {}
    rows = connection.execute(
        'SELECT r.cell_id, c.worldspace_id, r.pos_x, r.pos_y, r.pos_z FROM "references" r '
        "LEFT JOIN cells c ON c.id = r.cell_id"
    )
    for cell_id, ws_id, x, y, z in rows:
        space = ("worldspace", ws_id) if ws_id is not None else ("interior", cell_id)
        references.setdefault(space, []).append((x, y, z))
    worldspaces = {
        ws_id: (editor_id or "").rstrip("\x00")
        for ws_id, editor_id in connection.execute("SELECT id, editor_id FROM worldspaces")
    }
    interior_names = {
        cell_id: (name or "").rstrip("\x00")
        for cell_id, name in connection.execute(
            "SELECT id, interior_name FROM cells WHERE worldspace_id IS NULL"
        )
    }
    return World(doors, references, worldspaces, interior_names)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--db", default=DEFAULT_DB)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = parser.parse_args(argv)
    uri = f"file:{Path(args.db).as_posix()}?mode=ro"
    with sqlite3.connect(uri, uri=True) as connection:
        world = load_world(connection)
    picked = pick_doors(world)
    places = sorted({door.place for door in picked})
    document = {
        "note": "Written by tools/bench/portal_bench_doors.py; the portal bench's stress doors. "
        f"near = references within {RADIUS:.0f} units of the door in its own space; beyond = "
        "references in the destination cell (interior) or within the same radius of the "
        "destination door (exterior).",
        "radius": RADIUS,
        "doors": [door_json(door) for door in picked],
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    for door in picked:
        print(f"{door.ref_id:08X} {door.source_label} -> {door.label}: {reason(door)}")
    print(f"{len(picked)} doors over {len(places)} places ({', '.join(places)}) -> {args.out}")
    return 0 if len(places) >= 3 else 1


if __name__ == "__main__":
    sys.exit(main())
