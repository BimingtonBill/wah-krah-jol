# Phase 2 Profiling

The profiling stage is implemented as an opt-in, reproducible campaign around the release engine.
It does not require or redistribute Skyrim assets for the synthetic scenario. World scenarios require
the user's converted, legally owned asset set.

## Captured data

Each run writes a self-contained directory containing `metadata.json`, `frame-metrics.json`,
`cpu-spans.json`, `gpu-passes.json`, `streaming.json`, `memory.json`, and `summary.md`.

- CPU spans cover camera/world work, streaming planning, database queue/query/total latency, cell
  commit and spawn, terrain mesh generation, asset readiness, origin rebasing, and water systems.
- GPU data is sourced from Bevy 0.19 render diagnostics. Timestamp and pipeline-statistics support
  is recorded per run; counters the active backend cannot expose are listed under `unavailable`
  instead of being estimated.
- Renderer proof records whether GPU preprocessing, GPU culling and indirect drawing became active,
  plus the maximum occlusion-culling views, HZB views, indirect phase buffers, indirect batch sets
  and the number of proof frames. Bevy 0.19 does not expose native indirect draw counts or rejected
  instance counts to the main world; those fields remain explicitly `unavailable`, never zero-filled.
- Streaming includes aggregate counts plus request, stale-discard, unload, origin-rebase and commit
  events. It also records the per-frame commit budget and its raw worst value. A wall-clock overrun
  is classified as a violation only after a fixed 1 ms Windows scheduler tolerance; the independent
  frame-P95 acceptance threshold remains exactly 16.67 ms.
- Memory includes periodic process samples and the derived GiB/minute slope. The growth gate compares
  the first and last samples after a steady-state settling window equal to 10% of the scenario
  duration, capped at 60 seconds. Peak memory still covers the entire run. This excludes one-time
  asset and pipeline warm-up without hiding sustained growth in the long acceptance scenarios.
- Metadata records scenario, run, commit, dirty-worktree state, build profile and machine details.

## Reproducible campaign

Run the synthetic renderer without proprietary assets:

```powershell
./scripts/phase2-profile.ps1 -Scenario synthetic -Repetitions 3
```

The acceptance runner also executes `--streaming-fixture`: a proprietary-free database/cache scene
that performs rapid traversal, teleports, repeated origin rebasing and a return to the initial cell.
It rejects duplicate, orphaned or missing cell roots, stale work that was not discarded, incomplete
unload, worker shutdown failures, and per-frame commit-budget violations.

Run every scenario with converted assets and coordinates selected during integration:

```powershell
./scripts/phase2-profile.ps1 -Scenario all -Assets D:\SkyrimConverted `
  -RuralGridX 0 -RuralGridY 0 -DenseGridX 12 -DenseGridY 8 `
  -WaterGridX 7 -WaterGridY -2 -Repetitions 3
```

Results go to `target/profiling/<timestamp>-<cpu>-<gpu>`. The runner uses medians to reduce noise.
Pass `-UpdateBaseline` to write `profiling-baseline.json`. Against an existing baseline, material
regressions over 5% are warnings and regressions over 10% fail the campaign. Fixed noise floors of
5 FPS, 1.5 ms and 0.05 GiB prevent sub-millisecond scheduler jitter and sample granularity from being
amplified into false percentage regressions. Higher FPS is better; lower frame latency and memory
are better.
The profiling runner uses Windows `AboveNormal` process priority, inherited by the engine, to reduce
desktop-process preemption without using unsafe real-time scheduling.

`-Quick` reduces a campaign to one short repetition for plumbing checks. Stability defaults to 30
minutes. The dedicated `GPU Profiling` workflow is manually dispatched on a Windows GPU runner and
retains the complete bundles as CI artifacts.

## World loading and pacing metrics

These measurements show where world-loading time goes; they change nothing about what the engine
loads or in what order. They are fields of the benchmark report (`--benchmark-output`, report
`format_version` 8) and spans and events in the profiling bundle.

- **Time to a loaded world.** "Loaded" means every cell of the stream window is resident, no cell is
  loading, no database request is in flight, the model arming queue is empty, and no model or
  terrain/water surface is still pending. `time_to_world_ready_ms` and `frames_to_world_ready` are
  measured from the first frame to the first frame that holds, warm-up included. When it never holds,
  `world_ready_reached` is `false` and the times are `null`.
- **A jump.** `--benchmark-jump <grid-x>,<grid-y>` (same worldspace) moves the camera to the centre of
  that cell, at its ground height plus the usual start offset, on the first frame the world is ready.
  `time_to_world_ready_after_jump_ms` and `frames_to_world_ready_after_jump` are measured from the
  jump to the next frame the world is loaded again (`jump_issued` says whether it happened).
- **Falling behind at speed.** With `--auto-fly-speed` set, `fly_lag` records the horizontal distance
  from the camera at which each model finished loading: `models_ready`, `ready_within_one_cell`
  (within 4096 units), and `ready_distance_p5` and `ready_distance_min` in units. A model that
  finishes close to the camera arrived late. `peak_arming_queue_depth` is the largest number of
  models waiting to be armed at once.
- **Per-stage commit costs.** Spans `streaming/terrain_mesh` (the four terrain quadrant meshes and
  colliders), `streaming/spawn_references`, `streaming/terrain_validation` and
  `streaming/terrain_seam_weld`, next to `streaming/spawn_cell` and `streaming/cell_commit`. Each
  committed cell also logs a `spawned references=N model_loads=M` timeline event, followed by its
  `committed` event with the whole commit time.
- **Pipelines on first use.** `render_pipelines` counts, per render frame, the render pipelines newly
  queued in Bevy's `PipelineCache` and the ones that finished creating. It gives the mean render
  thread time on frames with pipeline activity against the rest, and the ten slowest render frames
  with their counts, so a spike frame can be matched with pipeline creation.

Example, a fast fly over a rural area, then a jump to a far cell:

```powershell
cargo run --release -p engine -- --assets D:\SkyrimConverted --grid-x 0 --grid-y 0 `
  --auto-fly-speed 5000 --benchmark-jump 40,12 --benchmark-duration 30 `
  --benchmark-output target/pacing-report.json --run-label pacing
```

## Interpretation

Compare identical scenario, resolution, release profile and hardware. Start with frame P95/P99,
then inspect the top CPU spans, GPU passes and streaming timeline in the same run. A missing GPU
counter means unsupported instrumentation, not a zero value. Real-asset and target-hardware sign-off
remains an execution result, not something the repository can pre-certify.
