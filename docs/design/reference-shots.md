# Reference shots: rendering the engine from a game screenshot's camera

**Status:** design, 2026-09-22 (Claude). Implemented by impl-017 (engine `--shots`), filled by
research-018 (camera poses for the UESP references).

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
  3. Wait until the view has **settled**: streaming has no pending cell loads for that space,
     and the spawned scenes have finished loading. Then wait a few more frames for lights and
     shadows. If the view never settles, time out (default 30 s), log it, and shoot anyway.
  4. Save the screenshot to `<out>/<name>.png`.
- After the last shot, write `<out>/shots.log` with one line per shot (name, settle time,
  whether it timed out, resident meshes), then exit with code 0. Exit non-zero only when the
  file is invalid or a shot could not be taken at all.
- `--shots-out` defaults to a folder named after the shots file, next to it.

## Comparison

`tools/compare_shots.py` (research-018) puts each reference and its render side by side at the
same size, one image per shot, plus one contact sheet. Pose refinement is iterative: render,
compare, move the camera, render again.
