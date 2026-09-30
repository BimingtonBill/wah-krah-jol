# Reference shots (`--shots`)

`--shots <file>` renders a list of exact camera poses, one PNG each, then exits. It exists for
visual review: take a screenshot in Skyrim, write down the camera pose, and render the same view
here, so the two images can be compared side by side and a regression shows up as a changed image
rather than a vague impression.

```
engine --assets <converted assets> --shots review/riverwood.json [--shots-out review/out]
```

## The shots file

```json
{
  "width": 1400,
  "height": 1050,
  "shots": [
    {
      "name": "riverwood-bridge",
      "worldspace_id": 60,
      "interior_cell_id": null,
      "position": [21340.0, -44890.0, 520.0],
      "yaw": 152.7,
      "pitch": 7.9,
      "hfov": 75.0,
      "reference": "review/riverwood-bridge.jpg",
      "note": "the bridge from the south bank"
    }
  ]
}
```

| Field | Meaning |
| :-- | :-- |
| `width`, `height` | The frame size in pixels. The window is opened at exactly this size, so every PNG is too; use the size the reference screenshots were taken at. |
| `name` | Output file stem: the image is `<out>/<name>.png`. A plain file name, unique in the file. |
| `worldspace_id` | The worldspace of an exterior shot (60 is Tamriel). `null` or absent leaves it to `--worldspace`. |
| `interior_cell_id` | An interior cell. Such a shot is skipped and logged: the streamer holds exterior cells only. |
| `position` | The camera's eye in Creation units, absolute (not relative to a cell or the render origin). |
| `yaw` | Skyrim heading in degrees: 0 looks north (`+Y`), 90 east (`+X`), clockwise seen from above. |
| `pitch` | Skyrim's X angle in degrees: positive looks down. |
| `hfov` | Horizontal field of view in degrees, between 0 and 180; the vertical one follows from the frame's aspect. |
| `reference`, `note` | Free text for people and comparison tools; the engine ignores them, and any other field. |

A file that cannot be read, is not JSON of this shape, has no shots, a zero frame, a missing or
non-numeric pose field, a field of view out of range, or a name that is empty, repeated or not a
plain file name stops the run before a window opens, with a message that names the file and the
problem.

## What a run does

Streaming starts at the first exterior shot: its worldspace (when it names one) and the grid square
its camera stands over. The run streams that one worldspace; a shot in another worldspace is
skipped and logged, like an interior shot, so a file that mixes worldspaces is rendered one
worldspace per run.

For each shot in order, the run places the camera at the pose (the streamer then loads the cells
around it) and waits until the view has settled: no cell loading, no database request in flight,
no model or surface waiting for its assets, no model waiting to be armed, no out-of-range cell
still waiting to be unloaded, and the renderer's final path running, for `SETTLE_QUIET_FRAMES`
(10) frames in a row, and not before `WARM_UP_SECONDS` (2 s) after start-up, while the first
pipelines compile. It then saves the primary window to `<out>/<name>.png`. A shot that has not
settled after `SETTLE_TIMEOUT_SECONDS` (30 s) is still captured, and the log says the timeout took
it and what was still pending.

`--shots-out` defaults to a `<file stem>-shots/` folder beside the shots file. `shots.log` there
has one line per shot: its name, the frames it waited, whether it settled or timed out, and the
image's path; skipped shots have a line saying why. The run exits with success once every shot is
written, and with an error when an image or the log could not be written.

`--headless` is ignored during a shots run, since the image is taken of the window. `--shots`
cannot be combined with a benchmark, `--acceptance-screenshot`, `--auto-fly-speed` or a fixture.
`--run-label` names the run in the window title (`OpenSkyrim - shots: <label>`).
