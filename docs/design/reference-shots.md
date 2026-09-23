# Reference shots: rendering the engine from a game screenshot's camera

**Status:** design, 2026-09-22 (Claude). Implemented by impl-017 (engine `--shots`), filled by
research-018 (camera poses for the UESP references). `open_door` added 2026-09-23 (impl-150).

## Why

To judge the renderer against the real game, the engine renders the same view as a reference
screenshot of Skyrim, from the same place, facing the same way, with the same field of view and
aspect. We then compare the two images side by side. The references are 26 UESP screenshots of
Alftand and Blackreach in `local/reference/uesp/` (git-ignored, CC BY-SA, sources in
`SOURCES.txt`). They are never committed.

## The shots file (the contract)

A JSON file. Angles are in **degrees**, positions are in **Creation units** (the coordinates
`skyrim_world.db` stores), so a pose can be read off the database and edited by hand.

```json
{
  "width": 1400,
  "height": 1050,
  "shots": [
    {
      "name": "SR-place-Alftand_02",
      "worldspace_id": 60,
      "interior_cell_id": null,
      "position": [77000.0, 77500.0, -5200.0],
      "yaw": 135.0,
      "pitch": 20.0,
      "hfov": 75.0,
      "reference": "local/reference/uesp/SR-place-Alftand_02.jpg",
      "note": "free text, ignored by the engine"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `width`, `height` | Window size for every shot in the file, in pixels. Use the reference's aspect. |
| `name` | Output file stem: the engine writes `<out>/<name>.png`. |
| `worldspace_id` | For an exterior shot, the worldspace (60 = Tamriel, 0x1EE62 = Blackreach, 432215 = AlftandWorld). `null` for an interior. |
| `interior_cell_id` | For an interior shot, the interior cell (86723 = Alftand01 "Alftand Glacial Ruins", 355355 = Alftand02 "Alftand Animonculory"). `null` for an exterior. Exactly one of the two is set. |
| `position` | The camera's eye, Creation units, absolute (not relative to a cell or the render origin). |
| `yaw` | Skyrim heading: 0 looks north (Creation `+Y`), 90 looks east (`+X`), **clockwise seen from above**. The same convention as an `XTEL` or a player `GetAngle Z`; see `transition::arrival_camera_rotation`. |
| `pitch` | Positive looks **down**, negative up, as Skyrim's player X angle. Range -89..89. |
| `hfov` | Horizontal field of view in degrees at this file's aspect. |
| `open_door` | Optional: a load door **reference FormID to open before this shot**, as a hex string (`"0x0001CBB0"`). The shot is then of the doorway from outside the open door, with the portal rendering the space beyond it. Omitted for a shot of a camera pose and nothing else. |
| `reference`, `note` | Ignored by the engine; carried for the comparison tools and humans. |

Field names, units and sign conventions are fixed. Tools may add fields, and the engine ignores
fields it does not know.

## Engine behaviour (`--shots <file> [--shots-out <dir>]`)

- Starts like an interactive run (the portal, lights and atmosphere plugins, and sky or
  underground lighting, as `--demo-tour` gets them), with no player controller, fly camera
  input or HUD text. The window is `width` x `height`.
- For each shot in order:
  1. Move to the shot's space, as a door crossing does: set `ActiveCell`, and for exteriors
     move the `RenderOrigin` to the camera's cell.
  2. Place the camera and set the perspective projection's vertical FOV from `hfov` and the
     aspect.
  3. **If the shot names a door**, open it and wait for the doorway (below). Then
  4. Wait until the view has **settled**: streaming has no pending cell loads for that space,
     and the spawned scenes have finished loading. Then wait a few more frames for lights and
     shadows. If the view never settles, time out (default 30 s), log it, and shoot anyway.
  5. Save the screenshot to `<out>/<name>.png`, and **close the door the shot opened** again.
- After the last shot, write `<out>/shots.log` with one line per shot (name, settle time,
  whether it timed out, resident meshes), then exit with code 0. Exit non-zero only when the
  file is invalid or a shot could not be taken at all.
- `--shots-out` defaults to a folder named after the shots file, next to it.

## Shots through an open doorway (`open_door`)

```json
{
  "name": "sven-door-oblique",
  "worldspace_id": 60,
  "position": [20797.8, -46068.8, -2.1],
  "yaw": 201.5,
  "pitch": 5.2,
  "hfov": 75.0,
  "open_door": "0x0001CBB0"
}
```

Worked examples, one doorway square and one oblique for each of two doors in two spaces:
`tools/reference/doorway_shots.json`.

Without this field nothing about a `--shots` run changes: no door is asked for, so none is open,
and a shot's settle, frames and bytes are what they always were.

A shot that names a door has the engine open it **the way a player's `E` does** - the
`OpenDoor` message, which `door_animation` answers by running the door's own `Open` clip - and
never crosses it. The shot is taken from outside, so the camera has to stand in front of the
doorway's plane (`portal::MIN_PORTAL_DOOR_DISTANCE`, one unit in front); a pose inside the
doorway or behind it is one the portal cannot render through, and the shot fails rather than
producing a picture of a wall.

The shot is photographed only once the doorway is one a player would see:

| What is waited for | Where it comes from |
|---|---|
| The door reference is streamed in | `streaming` |
| Its own animation has resolved, so asking it swings it rather than opening it as a static leaf | `door_animation`'s `AnimationGraphHandle`, or `shots::DOOR_QUIET_FRAMES` (120) frames of a view with nothing left to load - `door_animation`'s own patience - for a door whose model has no clip at all |
| `DoorState::is_open` - the doorway is a way through | `doors::DoorState` |
| The destination cell is resident | `transition::destination_is_resident` |
| The portal is drawing through **this** door's doorway | the doorway quad's pose and the portal camera (`shots::portal_renders_through_door`) |
| The `Open` clip has run to its end, so the leaf stands where the swing left it | the door's `AnimationPlayer` |
| The view has settled (the steps above) | `streaming` |

A door that does not get there within `shots::DOOR_TIMEOUT_SECONDS` (30 s) fails **that shot**:
`shots.log` gets a `FAILED:` line naming the door and the facts that were missing, and the run
exits non-zero. The shots after it are still rendered, because a shots file is usually a sweep
and one bad pose should not cost the others.

After a door shot's image is on disk the engine asks the door to close again (the same `OpenDoor`
message - `E` on an open door closes it) and waits for it, so a shot's frame does not depend on
the shots before it. A door whose model has no clip of its own cannot be closed at all: the engine
has no swing to play back, which is the same reason a player's `E` cannot close one. `shots.log`
says so, and the door stays open.

## Comparison

`tools/compare_shots.py` (research-018) puts each reference and its render side by side at the
same size, one image per shot, plus one contact sheet. Pose refinement is iterative: render,
compare, move the camera, render again.
