# Blackreach demo: walkable Alftand -> Blackreach with no loading screens

Owner: Claude (lead). Status: working, 2026-09-22 (`--demo-tour` passes 4/4 crossings and the walk test; see the AGENTS.md handoff). Research: `docs/research/worldspace-transition-demo.md`
(t02), whose route table and recipe this design follows.

## What the user gets

`engine.exe --assets %OPENSKYRIM_CONVERTED_DIR% --demo alftand --walk` starts the player on foot in
front of the Alftand entrance in the Pale (Tamriel, grid 19,18). Mouse to look, WASD to walk, Shift
to run, Space to jump, F to toggle flying, E to open a load door. Four doors lead down: Tamriel ->
Alftand01 -> Alftand02 -> AlftandWorld -> Blackreach. Each crossing is instant: the destination was
streamed in while the player approached the door.

## Pieces and owners (disjoint files)

| Piece | Task | Owns |
|---|---|---|
| Door types (contract) | lead, done | `crates/engine/src/doors.rs` |
| Converter: `XTEL` -> `door_links`, more base types, schema v4 | `impl-005` | `crates/converter/src/esm/exporter.rs`, `crates/converter/src/esm/mod.rs`, `crates/shared/src/lib.rs` |
| Engine: active cell, interiors, rebase guard, door spawn + crossing | `impl-006` | `crates/engine/src/streaming.rs`, `crates/engine/src/world/database.rs`, new `crates/engine/src/transition.rs` |
| Engine: player controller, mouse look, walking, E to open, prompt | `impl-007` | new `crates/engine/src/player.rs` |
| CLI flags, plugin wiring, demo start | lead, after the three merge | `crates/engine/src/config.rs`, `crates/engine/src/app.rs`, `crates/engine/src/lib.rs` |

Nobody but the lead edits `doors.rs`, `app.rs`, `config.rs` or `lib.rs`. A worker that needs a
contract change says so in its report (and tells its sibling, see crosstalk below) instead of
editing the contract.

## Database schema version 4 (impl-005 produces it, impl-006 consumes it)

`crates/shared/src/lib.rs` database schema version 3 -> 4, and the `schema_info` insert with it.

New table, created with the others and filled in the same transaction:

```sql
CREATE TABLE door_links (
    ref_id INTEGER PRIMARY KEY,            -- the source door REFR FormID (load-order remapped)
    destination_ref_id INTEGER NOT NULL,   -- XTEL bytes 0..4, remapped like any FormID
    pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,   -- XTEL 4..16, arrival
    rot_x REAL NOT NULL, rot_y REAL NOT NULL, rot_z REAL NOT NULL,   -- XTEL 16..28, arrival
    destination_cell_id INTEGER,           -- resolved post-pass from references.cell_id, NULL if unresolved
    destination_worldspace_id INTEGER      -- resolved post-pass from references.worldspace_id (NULL = interior)
);
```

`statics` (table name unchanged) additionally receives `MODL` model paths and `OBND` bounds for
`DOOR`, `ACTI`, `FLOR`, `CONT`, `TREE` and `LIGH` base records, exactly as it does for
`STAT`/`MSTT`/`FURN` today, so their references render.

## Engine behaviour (impl-006)

- `ActiveCell { worldspace_id: u32, interior: Option<u32> }` is a public resource in
  `streaming.rs`, initialised from `EngineConfig` when absent. `plan_cells` streams exteriors of
  `ActiveCell.worldspace_id` around the camera, or only `CellKey::Interior(id)` while inside.
- Every spawned reference with a `door_links` row whose destination resolved gets a
  `doors::LoadDoor` on its root entity (the label: the destination interior's `interior_name`, else
  the destination worldspace's `editor_id`).
- Pre-stream: while the `StreamingCamera` is within 800 units of a `LoadDoor`, its destination is
  requested too (the interior cell, or the 3x3 grid around the arrival point in the destination
  worldspace), so a crossing never waits.
- `doors::ActivateDoor` performs the crossing: set `ActiveCell`, set `RenderOrigin` for an exterior
  destination, move the camera to the arrival position and yaw, send `doors::DoorCrossed`.
- Rebasing is frozen while an interior is active. Interiors unload when they stop being active and
  are not pre-streamed. Acceptance and fixture runs behave exactly as before (no doors in fixtures,
  `ActiveCell` equal to the config).

## Player (impl-007)

A `PlayerPlugin` in `player.rs`, added by the lead only for `--walk`. It never runs in acceptance
or benchmark mode. It drives the `StreamingCamera` entity's `Transform`; the old `fly_camera`
system is not registered when the plugin is active. Walking needs ground and walls: the controller
casts rays against the spawned meshes (Bevy's mesh ray casting, if it sees this renderer's meshes;
otherwise the reference bounds - the worker establishes which works). It reads `LoadDoor`, writes
`ActivateDoor`, and listens for `DoorCrossed` to reset velocity.

## Crosstalk between the two engine workers

`impl-006-...` and `impl-007-...` run at the same time. Both build against `doors.rs` as written.
If either finds the contract insufficient, it messages the other with the exact change it needs,
writes the proposal into its report, and codes against the contract as it stands.
