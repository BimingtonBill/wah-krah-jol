# The PortalPlugin refactor: map and decisions

**Status:** **steps 1-5 are merged** (impl-153, `cdefe05`): one `add_plugins(PortalPlugin)` in
`run()`, `PortalOptions` in `config.rs`, `main.rs` byte-identical to `main`. The worker could not
run `cargo` in its session; the lead fixed three tests and fmt, and both tours pass on the merged
tree (Riverwood 8/8, Alftand 4/4, 0 panics). Step 0 (the baseline) was done before it: schema 19 on
disk, Riverwood 8/8, Alftand 4/4 on the pre-refactor tree.

**Step 6 is half done** (impl-154, one commit per file). `crates/engine/src/atmosphere.rs` now holds
the sky, the fog, the ambient and the per-space lighting, with `AtmospherePlugin` and their tests;
`run()` adds it with one `add_plugins` under exactly today's `atmosphere_applies` gate, and the sun
shadow cascades and the material fixture stay in `app.rs`. The terrain ring
(`crates/engine/src/terrain_ring.rs`: `camera_far_plane`, `exterior_fog`, `fog_off` and their tests)
has **not** moved yet - until it does, `atmosphere.rs` reads the two fog helpers from `app.rs` and
those three functions are `pub(crate)` there. `cargo` was refused in this worker's session as it was
in impl-153's, so **no check and no tour has been run on the step-6 commit** - `cargo test -p engine
--lib`, clippy, fmt and then the two tours are the lead's.

**The three places the move is not purely mechanical** (all of them a run's *registration*, never a
system's order among conflicting systems, and none of them visible in the tours):

1. `PortalPlugin` is added where `StreamingPlugin` is, so the player, the demo tour and the shots
   run are now registered only for runs that opened the world. `--walk --material-fixture` and
   `--benchmark-only --walk` are the combinations that lose them; a fixture run gains nothing.
2. `--benchmark-only --start-position <x y z>` now starts at the default pose: `StartPose` comes
   from `PortalPlugin` (H5), and a benchmark-only run has no runtime data, so it never gets one.
3. `run()` keeps the demo tour's `create_dir_all` (H3's shape): a plugin build cannot return the
   error that a folder it cannot write is, and today's run reports it as one.

research-084 (read-only DeepSeek worker) mapped every `main` -> `portal` difference in
`crates/engine/src/app.rs`, `config.rs` and `lib.rs` on 2026-09-23, against `main` = `2105bfd`.
The decisions in the first section are Claude's; everything after "The map" is the worker's report,
kept verbatim because a read-only run leaves its report nowhere else. The map was written against an
older tree: **every line number in it is stale** (the doorway anchor, impl-103/152, landed since).

The user approved the refactor on 2026-09-23. The rules it serves are the "Two tracks" section of
`AGENTS.md` and `docs/pr/two-tracks.md`; the acceptance the Phase 2 track set is that
`git diff main portal -- crates/engine/src/main.rs crates/engine/src/config.rs` shows only one
`add_plugins` call and one grouped block of CLI flags, and that the Riverwood (8/8) and Alftand
(4/4) tours still pass.

## Decisions

**The acceptance, as restated and agreed with the Phase 2 track (2026-09-23):** `main.rs` stays
**byte-identical** to `main` (it holds no `App`, so "one `add_plugins` call in `main.rs`" could only
be met by adding divergence); `run()` in `app.rs` adds the portal through exactly one
`add_plugins(PortalPlugin)`; `config.rs` differs by one `PortalOptions` block plus two lines in
`from_args`; the Riverwood tour passes 8/8 and the Alftand tour 4/4.


| case | decision | why |
|---|---|---|
| CLI grouping | **`PortalOptions`** struct with `parse_flag(&mut EngineConfig, ..) -> bool`, called before the upstream `match` | This branch merges from upstream repeatedly. One contiguous region plus two lines in `from_args` is the smallest conflict surface there is. It costs ~10 reader sites, nearly all in portal-owned files. `parse_flag` takes the whole config, so `--demo` can still write the flat `worldspace_id` / `start_grid` / `start_position` the streamer reads. |
| `--terrain-radius` | **stays outside** the portal block | Terrain, not doors; the Phase 2 track measures the ring with it. |
| `--shots` | **portal-owned, used by both tracks** | Documented as such; nothing to move. |
| H1 `ActiveCell` / `PrestreamCells` | **stay in `streaming.rs`**, documented as a general engine seam; `update_atmosphere` takes `Option<Res<ActiveCell>>` instead of a `run_if` | They cannot leave the streamer without a hole in `plan_cells`, and "where the camera is" is an honest general seam. |
| H2 `DoorTransition` edge | **move `TransitionPlugin`, keep the edge** | Verified in `bevy_ecs-0.19.0`: an ordering edge onto a set with no members is not an error. Fallback if it ever bites: one `add_plugins` line back in `StreamingPlugin`. |
| H3 shots window size | **stays in `run()`** as the ~10-line pre-window block | The window is sized before `WindowPlugin` exists; resizing mid-settle would be a new failure mode. |
| H4 `fly_camera` | **keep the two-line split** | Self-contained, changes no upstream line. |
| H5 start pose | **a `StartPose` resource seam**, not a portal branch inside `setup_world` | Keeps `setup_world` portable for the Phase 2 track's upstream work. |
| H6 `converter_schema_version()` | **keep the line on `portal`; do not restructure it here.** Agreed with the Phase 2 track: it becomes *their* upstream PR, "refactor(shared): one converter manifest schema version", moving the constant into `crates/shared` beside `WORLD_DATABASE_SCHEMA_VERSION`, ahead of upstream's first schema bump. `portal` picks it up by merging that `phase2/*` branch. | A contract both tracks depend on, so it lands once, upstream. |
| H7 `interactive` | **computed from `EngineConfig`**; `LightsPlugin` keeps today's gate | Otherwise a measured run with `--terrain-radius` gains lights and moves acceptance numbers. |
| H8, H9, H10 | **as the map proposes** | Pure moves; no copying of the atmosphere functions into the portal; messages registered once, in `TransitionPlugin`. |
| Step 6 (atmosphere/shadow/ring out of `app.rs`) | **do it, in the Phase 2 track's shapes:** `crates/engine/src/atmosphere.rs` with an `AtmospherePlugin` (sky, fog, ambient, per-space lighting - the lighting proposal waiting on upstream issue #24); `crates/engine/src/terrain_ring.rs` (the distant terrain ring). **Sun shadow cascades stay in `app.rs`** - their upstream PR is a small edit there (`sun_shadow_cascades(&EngineConfig)` plus one element on the `DirectionalLight`), so leaving them makes a clean merge when `main` brings it back. **The material fixture stays where it is.** | These are how the code will go upstream; matching them now means the merge back is a no-op rather than a conflict. |

## How it will run

Step 0 is a baseline, and nothing starts before it: impl-081 (converter, schema 19) and impl-082
(the door mirror) land, `app.rs`'s schema line goes to 19, the reconversion runs (announced to the
Phase 2 track before it publishes, per `AGENTS.md`), and both tours are recorded on the
pre-refactor tree. The queued lighting change (the lead-071 brief) touches no portal block and may
land before the refactor.

Steps 1-5 go to **one** coding worker as **one commit per step** in its worktree. Workers cannot run
the engine, so Claude runs both tours on the final tree; if one fails, the per-step commits make
it a bisection rather than a hunt.

## The map (research-084, verbatim)

<!-- research-084-portal-plugin-map.1 (research): research-084-portal-plugin-map - Where the portal track's changes live, and where they should -->
I have everything I need. Here is the map.

---

# research-084 — Where the portal track's changes live, and where they should

## The proposed `PortalPlugin`, in five lines

1. **`portal.rs` (as today):** portal material + `PortalState`, the portal camera and doorway quad, cell isolation, door-leaf visibility, destination atmosphere.
2. **`transition.rs` — moved out of `StreamingPlugin`:** the crossing (`CrossDoor`/`ActivateDoor`/`DoorCrossed`), `plan_door_prestream`, the `DoorTransition` set. Added for **every** run that has runtime data, exactly as `StreamingPlugin` adds it today.
3. **Door and model animation** (`DoorAnimationPlugin`, `ModelAnimationPlugin`) — interactive runs only, as today.
4. **The run modes:** `PlayerPlugin` under `--walk` (otherwise the upstream `fly_camera`), `DemoTourPlugin` + the demo-objective systems when `--demo-tour` is set, `ShotsPlugin` when `--shots` is set. The plugin reads `EngineConfig` itself and decides.
5. **Not in it, ever:** atmosphere/lighting (incl. `LightsPlugin`), sky and fog, sun shadow cascades, the terrain ring, snow, the material fixture's emissive check, `converter_schema_version`, and anything in `streaming.rs`.

`app.rs` then keeps only: the `--shots` pre-window block (it sets the window size), the `--start-position` camera pose, the fly/player switch, the `interactive` flag, and one `app.add_plugins(crate::portal::PortalPlugin)` line.

---

## Method, and a correction to the brief's "what is known"

I have no shell in this task, so I could not run `git diff`. `<workspace>` (the `main` worktree) is outside my readable roots, but `.git/refs/heads/main` and `.git/worktrees/impl-083-p2-pruned-texture-complete/HEAD` both read `2105bfd…`, and that second worktree is a p2 task (`crates/converter/` only) detached at exactly `main`. I used its `crates/engine/src/` as the `main` side and compared by hand, file by file (I read every line of `app.rs`, `config.rs`, `lib.rs` on both sides). Line ranges below are portal-side; hunk boundaries are mine, not git's, so a few may split or merge differently — the classification is what matters.

**The known-table is incomplete.** Comparing line counts on both sides (portal → main):

| file | portal | main | | file | portal | main |
|---|---|---|---|---|---|---|
| `main.rs` | 4 | 4 | | `streaming.rs` | 5861 | 2019 |
| `app.rs` | 3158 | 1579 | | `render.rs` | 2266 | 498 |
| `config.rs` | 568 | 293 | | `world/database.rs` | 1557 | 478 |
| `lib.rs` | 20 | 10 | | `world/components.rs` | 157 | 144 |
| | | | | `shaders/terrain.wgsl` | 119 | 63 |

`main.rs`, `config.rs`, `app.rs` and `lib.rs` are only the files the *acceptance* names. `portal` also differs from `main` by ~3,800 lines in `streaming.rs`, ~1,800 in `render.rs` and ~1,000 in `world/database.rs` — the fork's whole engine work, of which the portal feature is a part. The `PortalPlugin` refactor fixes the *portal-only* share of that; it does not make merging `main` cheap. I say so because the goal statement ("portal differs from main in the files everyone else also edits by as little as possible") is otherwise read as nearly met when it is not. The portal-only share of `app.rs` is the ~150 lines I classify below; the other ~1,450 are Phase-2-in-nature.

---

## Where the plugins are added today, and where the one call belongs

`main.rs` is four lines on both sides and **identical** to `main`:

```rust
fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    engine::run(engine::config::EngineConfig::from_env())
}
```

There is no `App` in `main.rs`; `App::new()` is at `app.rs:151` and every plugin is added in `run()` (`app.rs:162-260`). So the acceptance criterion "`git diff main portal -- main.rs` shows only one `add_plugins` call" **cannot be met as written**: today that diff is empty (zero calls), and `main.rs` cannot call `add_plugins` unless `run` hands it an `&mut App`.

**Recommendation: the call belongs in `app.rs`'s `run()`, not `main.rs`.** Reasons, given the goal:

- `run()` is where the `App` exists and where the ordering constraints are visible: `PortalPlugin` must come after `StreamingPlugin` (it reads `ActiveCell`/`StreamingWorld`) and after `EngineConfig` is inserted, and it must live inside the `runtime_data.is_some()` branch. A call in `main.rs` needs `run` to grow a plugin-hand-off parameter (an API change in the same shared file, plus `lib.rs`), and the ordering becomes implicit — worse than the line it removes.
- A call in `main.rs` means `main.rs` must know `interactive`, `shots_mode` and whether runtime data opened — all decided inside `run()`. Either that logic moves out (spreading portal code into `main.rs`) or the closure forces it back into the plugin anyway.
- `main.rs` being byte-identical is a *stronger* property than "one call", and it is free: upstream never edits `main.rs`.

So restate the criterion as: **`main.rs` is byte-identical to upstream, and `run()` adds the portal through exactly one `add_plugins(PortalPlugin)` call.** (If the other track insists on the letter of it, the only way is `run(config, impl FnOnce(&mut App))`, called at the end of the plugin block — I recommend against it and would say so in the PR description.)

---

## Table 1 — `crates/engine/src/app.rs` (portal side, 3158 lines)

Classes: **P** = portal-only · **2** = Phase 2 in nature · **G** = glue · **T** = track-specific value.

| portal lines | what it does | class | disposition |
|---|---|---|---|
| 9 | `use … shots::{ShotsFile, ShotsPlugin, ShotsRun}` | P | folds into the shots wiring the plugin owns |
| 11 | adds `ActiveCell` to the `streaming::{…}` import | G | needed only by the `run_if` at 211 — see H1 |
| 18-22 | `world::lighting::{AmbientBases, SpaceAtmosphere, …}` import | 2 | stays with the atmosphere |
| 27-28 | bevy `camera::ClearColorConfig`, `camera::Hdr` | 2 | atmosphere + glow |
| 33 | `light::{CascadeShadowConfig, CascadeShadowConfigBuilder, DirectionalLightShadowMap}` | 2 | shadows (PR 8) |
| 34 | `post_process::bloom::Bloom` | 2 | glow |
| 41 | `WindowResolution` in the window import | G | shots window size — must stay (H3) |
| 71, 90, 214, 217 | `SpaceLightingCatalog::open` + 5-tuple destructure + insert | 2 | per-space lighting |
| 95-126 | `--shots`: the three `ensure!`s, `ShotsFile::load`, output dir, `ShotsRun::new` | P | mostly into `PortalPlugin`; the file load + window size must stay in `run()` (H3) |
| 133-135 | window resolution from `ShotsRun::window_resolution` | G | stays — it is read before `DefaultPlugins` |
| 145-150 | `shots_mode`, `walk`, `interactive` and their comments | P+G | `walk`/`shots_mode` go into the plugin; `interactive` (or a pure `EngineConfig::interactive()`) stays — the Phase-2 lighting gate reads it |
| 181 | `.add_systems(Update, capture_acceptance_screenshot)` (main: `(fly_camera, capture_acceptance_screenshot)`) | G | the fly/player switch (H4) |
| 182-190 | `demo_tour` clone, `interactive`, `DemoTourPlugin { output_dir }` | P | `PortalPlugin` |
| 191-193 | `ShotsPlugin { run }` | P | `PortalPlugin` |
| 194-200 | `walk` → `PlayerPlugin`, else `fly_camera` | P+G | `PortalPlugin` + H4 |
| 201-207 | `spawn_demo_objective` / `update_demo_objective` | P | move both systems to `demo_tour.rs`, register from the plugin |
| 208-213 | `ClearColor(SKY_COLOR)` + `update_atmosphere.run_if(resource_exists::<ActiveCell>)` | 2+G | lighting; the `run_if` is H1 |
| 214-220, 234, 235-260 | `runtime_data` inserts, `StreamingPlugin`, `setup_world`, fixture branches | shared | unchanged |
| 221-233 | `if interactive { PortalPlugin, DoorAnimationPlugin, ModelAnimationPlugin, LightsPlugin }` | P (224/227/230), 2 (232) | the three portal plugins go inside `PortalPlugin`; **`LightsPlugin` must stay outside** (lighting) |
| 443-447 | `validate_streaming_fixture`: the ring-aware `maximum_resident` | 2 | terrain ring |
| 557-568, 621-643, 674 | material fixture: `exposed_emissive`, `emissive_is_visible`, the check that uses it | 2 | glow/materials; note `render.rs:1618` and `lights.rs:555` also read `emissive_is_visible` |
| 858, 865-866, 1024, 1229 | fixture lights take `DAY_SUN_ILLUMINANCE` / `SKY_AMBIENT_*` instead of literals | 2 | mechanical, stays |
| 1351-1355 | `converter_schema_version()` = 18 (main 14) | T | must differ per track (H6) |
| 1392-1399 | `atmosphere_applies(interactive, config)` | 2+G | lighting gate; ties the atmosphere to the ring |
| 1401-1545 | `SKY_COLOR`…`CAVERN_*`…`AMBIENT_BASES`…`SPACE_AMBIENT_BASES`…`DAY_SUN_*`…`SUN_TINT_STRENGTH` | 2 | lighting (issue #24) — **impl-073 rewrites part of this** |
| 1547-1611 | `space_atmosphere` | 2 | lighting — **impl-073** |
| 1613-1727 | `is_daylit`, `daylight_of`, `fallback_atmosphere`, `backdrop_of`, `sky_sun`, `usable_fog`, `srgb_u8` | 2 | lighting |
| 1729-1742 | `pub(crate) fn ambient_light` | 2 | read by `portal.rs:922` — cross-module seam, H9 |
| 1744-1769 | `pub(crate) fn atmosphere_fog` | 2 | read by `portal.rs:923` — H9 |
| 1771-1830 | `update_atmosphere` | 2+G | lighting; `ActiveCell` dependency is H1 |
| 1832-1890 | `DemoObjective` component + `spawn_demo_objective` + `update_demo_objective` | P | to `demo_tour.rs` |
| 1892-1893 | `START_EYE_HEIGHT` | P | player / start pose |
| 1901-1904 | `camera_far_plane` | 2 | terrain ring |
| 1906-1948 | `exterior_fog`, `fog_off`, `CREATION_UNITS_PER_METRE` | 2 | ring + shadows |
| 1950-2043 | `SUN_SHADOW_CASCADES`/`_MAP_SIZE`, `sun_shadow_cascades` (+ ~55 lines of comment) | 2 | shadows, PR 8 candidate |
| 2045-2138 | `setup_world`: new `origin` arg and the `start_position` branch (2059-2072); `camera_far_plane` (2073); per-camera `clear_color` (2076-2084); `Hdr` + `Bloom` (2092-2107); `DirectionalLightShadowMap` (2109-2115); `sun_shadow_cascades` (2122); ambient constants (2125-2127) | P+G (start pose) + 2 (rest) | start pose is portal; the rest is Phase 2 — H5 |
| 2318-2350 | test `the_canonical_emissive_is_checked_against_the_ambient_it_sits_in` | 2 | stays |
| 2352-2439 | upstream flight test + far-plane/fog tests | 2 | stays |
| 2441-2532 | shadow cascade tests | 2 | stays |
| 2534-2577 | atmosphere-applies + interior-fog tests | 2 | stays |
| 2579-3076 | per-space lighting tests over the fixture rows | 2 | stays |
| 3078-3157 | the three upstream manifest tests; `accepts_current_complete_runtime_assets` now takes the integration schema from `shared::WORLD_DATABASE_SCHEMA_VERSION` (3115-3119) | 2 | the literal `3` is wrong as soon as the DB schema moves; trivial upstream hygiene |

No **upstream drift** hunks found in `app.rs`: nothing is whitespace-only, reordered, or stale. The 22 deleted lines are the tuple split at 181, the old emissive literal/check, the old `12_000.0`/`(1600, 900)` literals, and the old ring bound.

---

## Table 2 — `crates/engine/src/config.rs` (portal side, 568 lines)

| portal lines | what it does | class | disposition |
|---|---|---|---|
| 11-18 | `terrain_radius` field + doc | 2 | ring — **not** portal |
| 46-62 | `walk`, `start_position`, `start_yaw`, `demo_tour`, `demo`, `shots`, `shots_out` fields | P | one block |
| 65-89 | `DEFAULT_TERRAIN_RADIUS`, `MAX_TERRAIN_RADIUS` + measurement table | 2 | ring |
| 99 | default `terrain_radius` | 2 | ring |
| 127-133 | defaults for the seven portal fields | P | one block |
| 145 | `terrain_radius_given` local | 2 | ring |
| 175-180 | `--terrain-radius` arm (clamped) | 2 | ring — shared with measured runs |
| 268-293 | `--walk`, `--demo-tour`, `--shots`, `--shots-out`, `--demo`, `--start-position`, `--start-yaw` arms | P | one block |
| 297-303 | a benchmark run keeps the ring off unless asked | 2 | ring |
| 307-316 | `shots_output_dir()` | P | with the shots run |
| 319-368 | `DemoStart` (alftand/blackreach/riverwood) + `apply` + `grid_of` | P | start positions of the demos |
| 391-410, 412-444 | ring default/clamp tests | 2 | ring |
| 446-480 | `demo_starts_set_worldspace_grid_and_position` | P | demo |
| 482-503 | `shots_options_parse_and_default_the_output_folder` | P | shots |
| 506-567 | `parses_runtime_options` | upstream | unchanged |

**So config.rs's +275 lines are two interleaved blocks, not one**: the portal flags (≈100 lines incl. tests) and the terrain ring (≈75 lines + the struct field), plus the shots/demo helpers. See the CLI section for how to make it one block without changing parsing.

---

## Table 3 — `crates/engine/src/lib.rs` (portal side, 20 lines)

| portal lines | what it does | class |
|---|---|---|
| 3, 4, 5 | `pub mod demo_tour; door_animation; doors;` | P |
| 6 | `pub mod lights;` | 2 (lighting) |
| 8, 10, 11 | `pub mod model_animation; player; portal;` | P |
| 14 | `pub mod shots;` | P |
| 15 | `pub mod snow;` | 2 (weather/material) |
| 17 | `pub mod transition;` | P (crossing) |
| +`world/mod.rs:4` | `pub mod lighting;` | 2 |

Ten new module declarations (8 portal, 2 Phase-2-shaped). Optional later win: put the portal modules under `src/portal/` so `lib.rs` gains one line instead of ten (mechanical, changes many `crate::player::`-style paths — only worth it if merge cost on `lib.rs` ever bites).

---

## The plugin boundary

**`PortalPlugin::build`, in order:**

1. `embedded_asset!(portal.wgsl)`, `MaterialPlugin::<PortalMaterial>`, `init_resource::<PortalState>`, the portal system chain (already there, `portal.rs:225-258`), `.after(DoorTransition)`.
2. `add_plugins(TransitionPlugin)` — **new**: the crossing and prestream, plus the door messages. Unconditional (today's behaviour via `StreamingPlugin`), so benchmark runs keep the same schedule.
3. If `walk`: `PlayerPlugin`, else nothing (the base schedule keeps `fly_camera`, H4).
4. If `demo_tour.is_some()`: `DemoTourPlugin { output_dir }` + the two objective systems (moved from `app.rs`).
5. If `shots.is_some()`: `ShotsPlugin` (the `ShotsRun` value handed in or rebuilt — H3).
6. If interactive: `DoorAnimationPlugin`, `ModelAnimationPlugin`.

**Must not contain** (these are Phase-2-shaped and belong to whichever plugin/PR the other track lands): `LightsPlugin`; `update_atmosphere` and every constant it reads; sun shadow cascades; `Hdr`/`Bloom`; the terrain ring (`--terrain-radius`, `camera_far_plane`, `exterior_fog`, `DistantTerrainRoot`, the ring branch in `plan_cells`); snow; the material fixture's emissive check; `converter_schema_version`; `StreamingPlugin`'s plan. Putting them inside `PortalPlugin` would make the portal PR that gets offered upstream in Phase 4 carry a hidden lighting/shadow/terrain change — exactly what the two-track split exists to prevent.

**Cross-check against `docs/pr/two-tracks.md` §"Which work goes where":**
- It covers the shadow cascades (PR 8) and lighting (item 10) — my biggest Phase-2 hunks. Agreed.
- **I disagree with one omission: the terrain ring is not on the list.** `--terrain-radius`, `DistantTerrainRoot`, `CellDetail::Terrain`, the ring planner, the ring fog/far plane and the ring bound in `validate_streaming_fixture` are a self-contained terrain/streaming feature with its own measurements (`config.rs:65-89`), used by the demos *and* by measured runs. It should be named as a Phase 2 candidate of its own (something like "the terrain-only ring beyond the full-detail grid"), or the config.rs diff will keep a second block forever.
- The **glow** work (`Hdr` + `Bloom` + `EMISSIVE_EXPOSURE` + `exposed_emissive` + the emissive fixture check) is only implicit in item 10. Name it; it spans `render.rs`, `lights.rs` and `app.rs`.
- Everything else I classified as Phase 2 maps onto an existing item (materials → 5/6, shadows → 8, lighting → 10, streaming robustness → 7).

---

## The CLI flags

Portal-only (13 of the additions): `--walk`, `--demo <name>`, `--start-position X Y Z`, `--start-yaw R`, `--demo-tour <dir>`, `--shots <file>`, `--shots-out <dir>`, plus their fields, defaults, `shots_output_dir()`, `DemoStart`/`grid_of` and three tests. Note `--shots` is portal-only *by ownership* — the reference-shot poses were fitted for the Phase 2 visual review too, so the Phase 2 track borrows the tool; that is fine and worth a doc line.

**Shared with the other track: `--terrain-radius`** (and `terrain_radius`, the two consts and the benchmark clamp). It is terrain, not doors; `scripts/phase2-acceptance.ps1` names no `--terrain-radius` (the ring is auto-off in benchmark runs, `config.rs:297-303`), but a Phase 2 ring measurement passes it explicitly (`docs/research/fixture-ring-timeout.md`). It must not go inside the portal block.

**Can the portal flags be one grouped block without changing parsing?** Yes. `from_args` is a `while let Some(argument) = args.next()` over a string `match`; arms are selected by exact string and consume only their own following args, so relocating them is behaviour-preserving. What grouping *cannot* move: the post-loop ring rule (297-303) must stay after the loop, and `--demo`'s arm writes `worldspace_id`/`start_grid`/`start_position` (flat fields the streamer reads) — a nested `EngineConfig::portal` struct must not swallow those. To make the diff literally one region rather than three, the options can be lifted into a small `PortalOptions` (field + defaults + `parse_flag(&mut config, argument, &mut args) -> bool`), with two lines added to `from_args` (`if config.portal.parse_flag(...) { continue; }` before the match) — then all portal parsing lives at the end of the file. Trade-off: every reader becomes `config.portal.walk` (≈10 sites, nearly all in portal-owned files) and two lines of upstream code move. I would do it if the merge-cost goal is real, and otherwise just place the three regions contiguously.

---

## The hard cases, and what to do with each

**H1 — `ActiveCell` is portal-track but lives in `streaming.rs` and is read by the plan.** `StreamingPlugin` inits `ActiveCell` (streaming.rs:74) and `PrestreamCells` (:75); `plan_cells` reads both (:445-446, :899-1014); `spawn_cell` even inserts `crate::portal::StreamedCellKey` (:1087). They cannot move into `PortalPlugin` without a hole in the streamer. Also `update_atmosphere.run_if(resource_exists::<ActiveCell>)` (app.rs:211) exists only because the resource comes and goes with the streamer. **Handle:** leave them where they are and document `ActiveCell`/`PrestreamCells` as a general engine seam ("where the camera is", "extra cells to keep resident") — that is also the honest upstream story. Replace the `run_if` with `Option<Res<ActiveCell>>` inside `update_atmosphere` so the lighting code does not reference a portal-track resource by name.

**H2 — `DoorTransition` is an ordering edge that crosses the boundary.** `StreamingPlugin`'s plan is `.after(DoorTransition)` (streaming.rs:98); `player.rs:745/747`, `portal.rs:256` and `shots.rs:345` order against it too. If `TransitionPlugin` moves to `PortalPlugin`, the edge in `streaming.rs` points at a set with no members in a non-interactive run. I checked `bevy_ecs-0.19.0`: `ScheduleError::SetNotFound` is only returned by `systems_in_set`/`remove_systems_in_set` (`schedule.rs:964-1042`), never for an ordering edge, so an empty set is harmless. **Handle:** move the plugin, keep the edge. Fallback if it ever bites: keep the one `add_plugins(TransitionPlugin)` line in `StreamingPlugin` (a one-line diff in an already-portal-heavy file).

**H3 — the shots run's window size is read before `DefaultPlugins` exists.** `--shots` sets the window resolution (`app.rs:131-143`), so the shots file must be loaded and validated in `run()` before the `App` is built with `WindowPlugin`. `PortalPlugin::build` runs after `WindowPlugin` is added but before its `finish`, so it cannot insert a `Window`. **Handle:** keep the ~10-line block that loads the file, fails fast and yields `Option<ShotsRun>`; insert the `ShotsRun` as a resource (or hand it to `PortalPlugin`) and let the plugin add `ShotsPlugin`. Do *not* resize the window on the first frame — a `--shots` capture settles for seconds, but a resized target mid-settle is a second failure mode for no gain.

**H4 — `fly_camera` and the player drive the same camera.** `app.rs` removes `fly_camera` from the unconditional tuple and adds it in the non-walk branch (main: line 128; portal: 181 vs 199). `fly_camera` is upstream code in an upstream function; the decision is portal. **Handle (two options):** keep the two-line split as it is (self-contained, no upstream change), or make the tuple byte-identical to `main` by giving `fly_camera` a `run_if` on a portal-inserted marker (one portal name in a shared file, and `attach_player` already marks the camera on frame 1). I recommend the second only if the extra two lines ever conflict; either way this is a permanent ~2-line app.rs diff.

**H5 — `setup_world` carries the portal's start pose.** `app.rs:2059-2072` reads `config.start_position`/`start_yaw` and calls `transition::arrival_camera_rotation`; `setup_world` is upstream code and also carries the Phase-2 clear colour, far plane, Hdr/Bloom and shadow spawns. **Handle:** either keep the ~14-line branch (it runs at `Startup`, so it can read anything), or add a general seam: `PortalPlugin` inserts a `StartPose(Transform)` resource and `setup_world` uses `start_pose.unwrap_or(default)` (≈4 shared lines + portal-side resource). The seam is better if the Phase 2 track later ports the surrounding block upstream.

**H6 — `converter_schema_version()` (app.rs:1351-1355) is a per-track value in a shared file.** It gates `validate_runtime_assets` before any plugin runs, so it cannot hide in `PortalPlugin`; and it will move again (18 → 19 with impl-081, and on every Phase 2 schema PR as its own copy). **Handle:** keep the one line (and doc), but propose the Phase-2-shaped fix: publish the required manifest schema in `crates/shared/` next to the existing `shared::WORLD_DATABASE_SCHEMA_VERSION` so each track has one value in the crate they both already edit, and this line disappears.

**H7 — `atmosphere_applies` mixes a portal concept (`interactive`) with the ring.** Its result gates sky/fog/ambient *and* decides whether the ring draws fog. **Handle:** keep it in the lighting code, but compute `interactive` from `EngineConfig` (it is a pure function: `walk && !benchmark && auto_fly_speed<=0 && shots.is_none()`, `|| demo_tour || shots`) so the lighting block does not depend on a local in `run()`. Note `LightsPlugin` must keep the same gate it has today (`interactive`, not `atmosphere_applies`) or a measured run with `--terrain-radius` would gain lights and change acceptance numbers.

**H8 — portal-only code sitting in `app.rs` that the plugin must own:** the two demo-objective systems (1832-1890) and the demo-tour/shots/player registration (182-207). They are pure moves; their only dependencies are `crate::doors::DoorCrossed` and `crate::demo_tour`, both portal-side.

**H9 — the portal reads app.rs's lighting functions.** `portal.rs:920-923` calls `crate::app::{space_atmosphere, ambient_light, atmosphere_fog}`. Whichever module gains the atmosphere (today `app.rs`; ideally `world/atmosphere.rs` when the lighting track takes it upstream) must stay reachable from `portal.rs`; a move is a three-line import change in `portal.rs`. Do not "solve" it by copying the functions into the portal.

**H10 — messages registered three times.** `CrossDoor`/`OpenDoor`/`DoorCrossed` are declared by `transition.rs`, `player.rs` and `door_animation.rs`. Harmless today, but register them once (in `TransitionPlugin`, which is now inside the portal group) and drop the duplicates.

---

## The order of work

Each step ends with `cargo test -p engine` plus a tour run — `--demo-tour local/tour-riverwood` (8/8 crossings, 0 dark frames) and `--demo-tour local/tour` (Alftand, 4/4). No step changes behaviour, so any failure belongs to the step that caused it.

0. **Baseline.** Let impl-081 (schema 19) and impl-082 (door mirror) land, bump `app.rs`'s `converter_schema_version()` to 19, reconvert, and record the two tours' results on the pre-refactor tree. Without this, a later failure cannot be blamed on the refactor.
1. **Move the demo objective.** `app.rs:1832-1890` → `demo_tour.rs`; register the two systems from `DemoTourPlugin`. Pure move; the objective is HUD text, so the tours are unaffected.
2. **Move `TransitionPlugin` to `PortalPlugin`** (drop the line in `StreamingPlugin`). Highest-risk step, so it goes first among the wiring: the tour is exactly a sequence of crossings, which is the fastest signal.
3. **Fold the rest into `PortalPlugin`** one plugin at a time, in this order: `DoorAnimationPlugin`, `ModelAnimationPlugin`, then `PlayerPlugin` (with the fly/player switch), then `DemoTourPlugin` + `ShotsPlugin`. After each, run the tour that uses it. `app.rs`'s block 182-233 shrinks to one call plus the shots pre-window block.
4. **Config grouping.** Move the portal fields/defaults/arms into one contiguous region (or `PortalOptions`, per the CLI section). `cargo test -p engine` covers parsing; then the Riverwood tour for the `--demo`/`--walk` path.
5. **The residue.** Decide H4 (`fly_camera`), H5 (start pose), H1/H7 (`ActiveCell` run_if, `interactive`) and H6 (schema const) with the lead; each is ≤5 lines and can be one small commit with its own tour run.
6. **Optional, last: move the Phase-2 code out of `app.rs`** (atmosphere + shadow + ring blocks, 1392-2138) into a module of its own. This is what actually shrinks `app.rs`'s diff — a *new* file never conflicts on a merge, while relocating 750 lines between two files that `main` also edits does not. Do it only after impl-073 (which rewrites exactly that block) and after the extraction above, and treat it as a separate worker task with a pure-move acceptance.

**What the queued lighting change will move (impl-073, `lights.rs` + `app.rs`):** `app.rs`'s `DAY_SUN_LEVEL` (1529), a new `SKY_FILL`, the ambient computation inside `space_atmosphere` (1575-1587), `INTERIOR_AMBIENT_LEVEL` (1488, becoming `pub` for `lights.rs`), and the lighting tests around 2695-2885; plus `lights.rs`'s `LIGHT_EXPOSURE` (50→10) and `HALF_RADIUS_ILLUMINANCE`. It touches **no** portal-only block and none of the plugin wiring, so it can land before or after the extraction; if step 6 is taken, it must land before it. One sentence in `app.rs:2093` ("`LIGHT_EXPOSURE` = 50 times the interior ambient") is inside the `Hdr`/`Bloom` comment it will invalidate — worth telling that worker.

---

## Report to the lead

**Did:** reconstructed the `main`→`portal` diff for `app.rs`, `config.rs`, `lib.rs` by hand (no shell; `main`'s side read from the `impl-083` worktree detached at `2105bfd`), classified every region as portal-only / Phase 2 / glue / drift, found the plugin registration sites, and mapped the cross-module seams (`portal.rs`→`app.rs` atmosphere; `render.rs`/`lights.rs`→`app.rs::emissive_is_visible`; `lights.rs`→`INTERIOR_AMBIENT_BRIGHTNESS`; `streaming.rs`→`ActiveCell`/`PrestreamCells`/`DoorTransition`).

**Files changed:** none — read-only, as briefed. No engine or build run.

**Could not do:** run `git diff` (no shell in this task), so the hunk boundaries are hand-derived and a few may split differently from git's; my comparison of the mechanical fixture regions (app.rs 240-1130) is by eye, so a one-line substitution there could have escaped me. `<workspace>` was not readable; if you want the true diff, run `git diff main portal -- crates/engine/src/app.rs crates/engine/src/config.rs crates/engine/src/lib.rs > local/research-084.diff` and spot-check my two tables against it.

**Two things I would act on before writing the refactor brief:** (1) the acceptance criterion about `main.rs` cannot be met as written — `main.rs` has no `App` and must stay byte-identical; put the single call in `app.rs`'s `run()` and restate the criterion. (2) `config.rs`'s diff is two blocks, not one, because `--terrain-radius` is the terrain ring, not the portal; it needs to be named on the Phase 2 candidate list (my only disagreement with `two-tracks.md`).

[ds-agent] run=research-084-portal-plugin-map.1 session=7b3ca10d-05be-4bb3-a56c-ccbd5be700ab status=ok turns=68 tokens_in=137331 tokens_out=67610 denied=Glob,Read
