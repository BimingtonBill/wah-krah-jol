# Real lights from LIGH references; auto-load doors that fire on contact

Owner: Claude (lead). Status: designed 2026-09-22, not started. Follows `docs/design/blackreach-demo.md`.

## Why

- Interiors and Blackreach are lit by ambient light and a lantern carried by the camera, because
  Skyrim's `LIGH` references (torches, braziers, Dwemer lamps, Blackreach's glowing fungus lights)
  are not lights in the engine. Alftand reads flat and Blackreach reads dark blue.
- Load doors open only with E. Skyrim's `AutoLoadDoor` markers (the Alftand entrance is one) cross
  when the player walks into them; with E the user has to find an invisible door.

## Lights

### Converter (schema v5, `impl-014`)

`LIGH` records carry `DATA`: time (i32), radius (u32), colour (4 x u8, RGB + unused), flags (u32),
falloff exponent (f32), FOV (f32), near clip (f32), flicker period/intensity amplitude/movement
amplitude (3 x f32), then value (u32), weight (f32) - layout per UESP "Skyrim Mod:Mod File Format/LIGH";
the worker verifies it against real bytes. New table:

```sql
CREATE TABLE lights (
    id INTEGER PRIMARY KEY,        -- LIGH FormID
    editor_id TEXT,
    radius REAL NOT NULL,          -- Creation units
    color_r INTEGER NOT NULL, color_g INTEGER NOT NULL, color_b INTEGER NOT NULL,
    flags INTEGER NOT NULL,        -- DATA flags (dynamic, can carry, negative, flicker, off by default, ...)
    falloff REAL NOT NULL,
    fade REAL                      -- FNAM, if present
);
```

Every `LIGH` record gets a row, with or without a model (the `statics` rule for models is unchanged).
Reference-level overrides (`XRDS` radius on the `REFR`) go in a nullable `references`-side column only
if the worker finds them common on the demo route; otherwise note them as a gap.
`WORLD_DATABASE_SCHEMA_VERSION` 4 -> 5 (lead edits `crates/shared` and the engine fixtures).

### Engine (`impl-015`)

- The reference load query returns the reference's light row (LEFT JOIN `lights` on the base id).
- `spawn_cell` gives such references a Bevy `PointLight` child: colour from the row, `range` = radius,
  intensity scaled so the light is clearly visible at half its radius in these Creation-unit distances,
  no shadows. Negative lights and lights flagged off-by-default are skipped.
- Budget: Bevy's clustered forward renderer handles many point lights, but keep only the nearest N
  (start at 64) enabled around the camera, re-chosen as the camera moves.
- The camera lantern and the underground ambient (`app.rs`) stay, dimmed by the lead once real lights
  render.

## Auto-load doors (`impl-016`, after impl-015)

- Contract change (lead, in `crates/engine/src/doors.rs`): `LoadDoor` gains `auto_load: bool`.
- Streaming sets it for doors whose base editor id or model marks them as auto-load
  (`AutoLoadDoor*` / `AutoLoadMarker*`), from the base's `statics` row.
- The player controller crosses an auto-load door when the player's feet enter a trigger box at the
  door (its bounds, or about 160 x 240 x 40 units for a marker without bounds) while moving toward
  it; the E prompt is not shown for auto-load doors. Other doors keep E.
