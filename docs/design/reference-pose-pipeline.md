# Finding a reference shot's camera pose: automatic first, by hand only when needed

**Status:** process, 2026-09-24, set by the user. The shots-file format and the engine's `--shots`
and `--start-shot` modes are in [`reference-shots.md`](reference-shots.md). This page is the order of work.

Every reference shot (a Skyrim screenshot paired with a camera pose in `tools/reference/*.json`)
goes through the same three steps. The user's time goes only to the shots the first two steps
could not settle.

## 1. Automatic ball-park fit

A DeepSeek research worker proposes a pose for each new shot. It works from the world database
(`$OPENSKYRIM_CONVERTED_DIR/skyrim_world.db`) and the tools in `tools/research/`:
- `uesp_space_map.py`, to find the place;
- `uesp_pose_check.py`, to project the database's objects onto the screenshot;
- `aim.py`, to point the camera at a landmark;
- engine renders with `--shots`.

**The fit is capped.** At most two render rounds per shot, and at most about 30 minutes for a
batch. The job is a ball-park, not a perfect match. On 2026-09-24 three uncapped workers rendered
170-300 candidates each and still left a third of the shots at `area only`, and the user fixed
those by hand in minutes.

## 2. Automatic check

Each shot's final render is graded against its screenshot, in research-020's vocabulary:
- `matched`: the same photograph, lighting aside;
- `close`: the same subject and side, framing or height a little off;
- `area only`: the right place but not the same view;
- `wrong`: the wrong subject.

A second worker, or the lead, grades by looking at the render beside the reference, never the
worker grading its own fit. Record every verdict:

```
python tools/research/pose_queue.py grade <shot name> <grade> --by <who>
```

`matched` and `close` are accepted and the shot is done. Anything else goes to step 3.

## 3. By hand, for the rest

```
python tools/research/pose_queue.py build
```

This writes `local/reference/manual-queue.json` (only the shots not accepted, minus any the user
flagged as not findable) and `local/reference/fit-queue.vbs`. The user double-clicks the `.vbs`:
- the engine opens in free flight at the first queued shot, with its Skyrim picture in the corner;
- N / B step through the queue;
- P saves the view;
- X flags "can't find it".

Then:

```
python tools/research/pose_queue.py merge
```

This folds the saved views into the shots files. It takes position, yaw and pitch; each shot keeps
its own field of view, reference and metrics. It grades those shots `hand-fit` and rebuilds the
queue. `python tools/research/pose_queue.py status` shows the counts at any time.

## Files

| File | In git | What |
|---|---|---|
| `tools/reference/*.json` | yes | the shots: poses, references, notes, metrics |
| `local/reference/pose-grades.json` | no | the grade per shot, who gave it and when |
| `local/reference/manual-queue.json`, `fit-queue.vbs` | no | the current by-hand queue and its launcher |
| `local/reference/manual-poses.jsonl` | no | every P press (the engine appends) |
| `local/reference/not-found.jsonl` | no | shots the user flagged with X |
