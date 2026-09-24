# Finding a reference shot's camera pose: quick automatic pass, then the user, then a check

**Status:** process, 2026-09-24, set by the user. The shots-file format and the engine's
`--shots` and `--start-shot` modes are in [`reference-shots.md`](reference-shots.md). This page is
the order of work. The user's time goes only to the shots the automatic pass could not settle.

## 1. Automatic pass: about 5 minutes for a batch

```
python tools/research/pose_ballpark.py local/reference/<folder> tools/reference/<set>_shots
python tools/research/pose_queue.py check
```

- **Ball-park (seconds).** `pose_ballpark.py` places each new screenshot from its place name,
  using the world database:
  - a city or realm worldspace;
  - the load door into a dungeon, cave, mine or fort;
  - an interior cell's editor id;
  - a small hint table for regions UESP names differently ("The Rift" means near Riften).

  The camera is framed from a fixed offset. A name it can't place starts high above Skyrim.
  Shots already in the files are left alone.
- **Check (a few minutes).** `pose_queue.py check` renders every ungraded shot in one engine run
  per file, lays the pairs out on numbered pages (`grade_sheet.py`), and has one DeepSeek worker
  grade them from the pages (`tasks/deepseek/grade-reference-poses.md`). It records the grades in
  `local/reference/pose-grades.json`. Grades use research-020's vocabulary:
  - `matched` and `close` are accepted;
  - `area only` and `wrong` go to the user.

Measured on 2026-09-24 for 31 new screenshots: ball-park under 1 s, render 92 s, grade 124 s.
There is no search or refinement loop: a ball-park is good enough, or the user takes it.

## 2. The user: by hand, or skip

```
python tools/research/pose_queue.py build
```

This writes one queue file per render size and `local/reference/fit-queue.vbs`. The user
double-clicks it:
- the engine opens in free flight at the first queued shot, with its Skyrim picture in the corner;
- N / B step through the queue;
- P saves the view;
- X means "can't find it" and skips the shot for good.

A shot the user has already placed by hand is never queued again.

## 3. Check the user's poses

```
python tools/research/pose_queue.py merge
python tools/research/pose_queue.py check
```

`merge` folds the saved views into the shots files. It takes position, yaw and pitch; each shot
keeps its own field of view, reference and metrics. It marks the shot `unchecked`. The same
`check` then grades the user's poses. The result is recorded and not queued again.
`python tools/research/pose_queue.py status` shows the counts at any time.

## Files

| File | In git | What |
|---|---|---|
| `tools/reference/*.json` | yes | the shots: poses, references, notes, metrics |
| `local/reference/pose-grades.json` | no | the grade per shot, who gave it, when, and `hand` once the user placed it |
| `local/reference/grading/` | no | each check's renders, pages and grader report |
| `local/reference/manual-queue-*.json`, `fit-queue.vbs` | no | the current by-hand queue and its launcher |
| `local/reference/manual-poses.jsonl` | no | every P press (the engine appends) |
| `local/reference/not-found.jsonl` | no | shots the user skipped with X |
