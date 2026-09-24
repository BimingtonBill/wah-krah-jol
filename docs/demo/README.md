# Running the demos: Alftand → Blackreach and Riverwood

This is the contribution guide for the demos the project uses to show what the engine can do. There
are two, and both are walked end to end with no loading screen anywhere. They are the fastest way to
get a real, running OpenSkyrim on your machine.

- **Alftand → Blackreach** (`--demo alftand`) — the descent: on foot from the Alftand entrance in
  the Pale, down four load doors, into Blackreach. Pick this one for the underground: interiors, the
  cavern's own lighting, and a long route that ends far from where it started.
- **Riverwood** (`--demo riverwood`) — the daylight village: from the Helgen road south-west of the
  village, through the doorways of its four houses and out again, ending on the river bank looking
  across at the mill. Pick this one for what the Alftand route cannot show — walking through a
  doorway into an interior and back out again, in daylight, with no loading screen: the lead's own
  tour run measured **8 crossings of 8 and 0 dark frames in 2,056 captured frames**
  (`local/demo/riverwood2/tour.txt`, the lead's log).

Everything here runs from **your own copy of Skyrim Special Edition**: this repository contains no
Bethesda data, and none may ever be committed. The converter reads your install read-only and
writes a converted asset folder somewhere of your choosing.

Paths are placeholders: `<SkyrimSE>` is your install (`.../steamapps/common/Skyrim Special
Edition`), `<converted>` is the folder the converter writes.

**The `play-*.cmd` launchers** in the repository root find `<converted>` through the
`OPENSKYRIM_CONVERTED_DIR` environment variable (ADR-0002). Set it once, then open a new
command window:

```bat
setx OPENSKYRIM_CONVERTED_DIR "D:\SkyrimConverted"
```

**About links in the design docs:** some documents under `docs/design/` link to the author's
internal working notes (research write-ups, task briefs, handoff logs). Those are not published in
this fork, so those links do not resolve here; the design documents themselves are complete.

- Design and history, Alftand: [`docs/design/blackreach-demo.md`](../design/blackreach-demo.md)
- Design and history, Riverwood: [`docs/design/riverwood-demo.md`](../design/riverwood-demo.md)
  (sections 1-7 are the data work that preceded the demo, section 8 what the first build and the
  first renders showed)
- The Alftand route, in the game's own data: [`docs/research/worldspace-transition-demo.md`](../research/worldspace-transition-demo.md)

## 1. What the demos are

| | Alftand → Blackreach | Riverwood |
|---|---|---|
| Start | Outside the Alftand entrance in the Pale (Tamriel, cell grid 18,18) — or straight into Blackreach with `--demo blackreach` | On the Helgen road south-west of the village (Tamriel, cell grid 4,-12 `Riverwood`), looking up the street at Sven's house — `--demo riverwood --walk` |
| Route | Tamriel → `Alftand01` → `Alftand02` → `AlftandWorld` → Blackreach, four load doors. `--demo blackreach` starts at the far end, on the arrival point of the route's last door | Four houses on the village street — Sven's, the Riverwood Trader, Alvor and Sigrid's, the Sleeping Giant Inn — eight doorways, each house entered and left again. Ten legs, 8,096 units (≈116 m), about a minute at walking speed without the door visits; it ends on the river bank looking across at the mill (`docs/design/riverwood-demo.md`, section 3) |
| Crossing | Walking into a load door crosses it. The destination was streamed in while you approached, so there is no loading screen, and your pose carries through instead of snapping (`crates/engine/src/transition.rs`) | The same, in daylight and in both directions: press **E** at each doorway and walk through it. The scripted tour does exactly that, holding `W` and photographing every frame either side of the swap |
| Also | Mouse look, walking, running, jumping, flight mode, and seeing through a doorway into the next interior | The same controls, in a lit village: the four house interiors, the street and the river bank in one continuous walk |

### What works

- Seamless interior and worldspace crossings, in both directions, with the arrival camera placed
  where the game's own `XTEL` data puts the player and the player's pose carried through the swap.
- Walking on the converted terrain and meshes: WASD, Shift to run, Space to jump, F for free flight.
- Load doors open with **E**, and the auto-load doors (the ones the game fires on contact) also
  cross on contact.
- Load doors swing open on their model's own `Open`/`Close` clips (`crates/engine/src/door_animation.rs`).
  A door whose own clip barely moves — the Alftand route's Dwemer load doors turn their leaves 5-9
  degrees — borrows its non-load twin's swing, or has its own clip scaled up until it clears the
  doorway; a model carrying no clip at all opens in the frame you ask for it.
- Skyrim's own object placement, rotations, scales and bounds; per-reference point lights from
  `LIGH` records (`crates/engine/src/lights.rs`), terrain out to `--terrain-radius` cells with
  distance fog, and the reference-shot camera (`--shots`).
- A space's ambient, backdrop, fog and sun come from the game's own `XCLL`/`LTMP`/`WTHR` records,
  resolved into the `space_lighting` table at conversion time (`crates/engine/src/world/lighting.rs`).
  A record's colour is applied as hue, at the calibrated brightness.
- Glow and emissive, on the materials the converter marks as deliberate emitters, rendered on an HDR
  camera with bloom (`EMISSIVE_EXPOSURE` in `crates/engine/src/lights.rs`).
- Directional snow on the statics the game gives a `DNAM`/`MATO` material
  (`crates/engine/src/snow.rs`), and a model's own looping `Idle` clip played on every reference of
  it — Riverwood's mill wheel is the model that has one (`crates/engine/src/model_animation.rs`).
- The terrain layer weights reach the shader as the game's own 17×17 `VTXT` sample grid,
  interpolated between samples as Skyrim interpolates them
  (`crates/engine/src/shaders/terrain.wgsl`).
- Editor-marker models that convert to an empty glTF scene are skipped and counted instead of
  failing the bounds gate (`empty_model_references` in `crates/engine/src/streaming.rs`).

### What does not work yet

Nothing below is a mystery: each is a known, recorded gap in `AGENTS.md`'s handoff log, in
`docs/research/` or in `docs/design/riverwood-demo.md`.

- **No NPCs or creatures.** Nothing walks but you. That is the loudest gap in Riverwood, which the
  game fills with guards, the innkeeper and townsfolk, a cow, chickens and a horse (14 NPC
  references stand in the stream box, 12 of them in the route's own cells) but where only the
  buildings, props and trees render. Actor animation, skinning and AI are Phase 4
  (`docs/roadmap/04-gameplay-and-physics.md`); the engine animates only what a model carries by
  itself, such as the door and `Idle` clips above.
- **No grass.** The `GRAS` records reach the database but no grass reference is placed or drawn, so
  Riverwood's street and river bank are bare where the game covers them with ground cover. The
  scattered `Landscape\Plants\*` props that *are* placed — thickets, ferns, clover, planters —
  render normally.
- **The river is a flat, cell-aligned plane.** Water is a full 4096-unit plane per qualifying cell
  at the level the record gives (`XCLW`), with no flow map (the village water type's
  `flow_normal_path` is NULL in the converted data), no current and no shoreline clipping: its edges
  land on cell boundaries, and a neighbouring cell with no water draws none at all.
- **No LOD beyond the terrain ring.** No grid in or around Riverwood has a `lod` row, so distant
  terrain is the terrain-only ring alone (`--terrain-radius`): the surrounding slopes are drawn, the
  peaks behind them are not.
- **The FX cards convert to empty scenes and never draw.** The smithy's chimney smoke
  (`FXSmokeRiverwoodSmith01`), the street mist (`FXMistStreet01`, 12 references), the mill wheel's
  spray (`FXMistMillWheel01`) and the forest motes (`FXMotesForest01`) all convert to a `.glb` with
  0 nodes and 0 meshes, so the village's smoke, mist and spray are missing entirely.
- **No physics beyond walking**, no combat, no item pickup, no inventory, quests or menus. There is
  no physics engine: walking and the ground height come from ray casting against the streamed
  meshes and reference bounds.
- **No audio.**
- **No saves.** Every run starts from the demo's start point.
- **Converted point lights have no shadows, cone or flicker**, and their falloff exponent, `FOV` and
  near clip are read but not applied (`crates/engine/src/lights.rs` lists what is left out), so a
  light also lights the far side of the wall it is mounted on and a torch burns steadily.
- **Ice reads as rock.**
- **Loosely placed `BlackPlane01` shapes leave black voids.**
- **No sky beyond one colour, and no weather change.** A space's weather record decides the sun's
  colour, its daylight and the horizon haze, but the sky itself is the camera's clear colour: no
  clouds, no sky dome, and nothing that changes with time.
- **Scripts are converted but not used for gameplay**: `.pex` becomes Luau, and the Papyrus runtime
  exists, but no quest or dialogue logic runs yet.
- A portal destination's lights can faintly reach the active space, because GPU light clustering
  ignores render layers.

The Riverwood gaps above — NPCs, grass, the river plane and LOD — and the FX cards' missing visuals
are measured in `docs/design/riverwood-demo.md`, section 4 (which section 8 confirms the first
renders held), and **none of them is specified in `docs/research/visual-gaps-spec.md`**, whose four
gaps are glow, per-space lighting and fog, snow and ice, and the broken objects. The one exception
is the conversion of an empty scene itself: that is that spec's gap 4.2, and the fix for it is what
makes such a model skip the bounds gate rather than fail it.

## 2. Prerequisites

### Required

| | |
|---|---|
| Copy of the game | **Skyrim Special Edition** (2016) with its official plugins (`Skyrim.esm`, `Update.esm`, `Dawnguard.esm`, `HearthFires.esm`, `Dragonborn.esm`). Your own copy; the converter reads it, never writes to it |
| OS (tested) | Windows 10/11 x64. The crates build on Linux too (CI runs the test suite on Ubuntu), but the demo has only been exercised on Windows |
| GPU | Anything with current Vulkan or Direct3D 12 drivers; the engine is wgpu/Bevy and wants a real GPU (it has not been tried on a software rasterizer) |
| Rust | Stable, 2024 edition — **1.85 or newer**. Check with `rustc --version` |
| C/C++ toolchain | Windows: the **Visual Studio 2022 Build Tools with the "Desktop development with C++" workload** (MSVC + Windows SDK). Linux: any `cc`/`gcc`/`clang` |

The C++ toolchain is not optional: rustc links the MSVC target against the Windows SDK, and the
build compiles C++ from source for `basis_universal` (KTX2), bundled SQLite, and the converter's
own KTX2 bridge (`crates/converter/build.rs`).

### Installing what is missing

```powershell
winget install Rustlang.Rustup
rustup default stable-msvc

winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

`tools/demo/setup-demo.ps1` checks all of this and prints these same commands if something is
missing. It never installs anything for you.

**CMake and Ninja are not needed.** The root `README.md` lists them, but the current dependency set
does not use the `cmake` crate at all: every native build script goes through `cc`
(`Cargo.lock` has no `cmake` entry). A working C/C++ compiler is the real requirement.

### Disk and time

| | |
|---|---|
| Converted assets | **~67 GB** for a full conversion of a stock install (the lead's measurement; this install includes the Creation Club content) |
| Converted assets, again | A re-run against an existing `<converted>` reuses the extraction cache and the per-asset manifest, but the output folder is still the big one — keep the headroom |
| Build output | `target/` is several GiB; the acceptance tooling asks for at least 10 GiB free on the repository drive |
| Cold conversion | **~5 hours**: 18,405,060 ms (5 h 7 m) for 242,969 assets, the first run against an empty output folder |
| Re-conversion | **~20 minutes** with a warm cache (measured: 1,141,318 ms with 0 converted, 242,969 reused) |
| First release build | Tens of minutes: it compiles Bevy and the whole workspace, and the release profile uses `codegen-units = 1` with thin LTO. Later builds are incremental |

Those timings came from one machine while other work was running on it, so treat them as an order
of magnitude. Do not run the acceptance or profiling scripts at the same time as a conversion:
they measure the GPU and the CPU, and they spoil each other.

## 3. Quick start

```powershell
# from the repository root
pwsh -File tools/demo/setup-demo.ps1 -SkyrimPath "<SkyrimSE>" -OutDir "<converted>"
```

That script, in order:

1. checks `cargo` and the C++ toolchain, and prints install commands instead of installing;
2. builds the release converter and engine;
3. finds your Skyrim SE install (parameter first, then Steam's registry keys and library folders,
   then it asks);
4. converts into `<converted>`, resuming an interrupted run if it finds one;
5. writes `play-demo.cmd` next to the output.

| Parameter | Meaning |
|---|---|
| `-SkyrimPath <path>` | Your install root or its `Data` folder. Omit to let it search |
| `-OutDir <path>` | Where the converted assets go. Never your Skyrim `Data` folder; keep it outside the repository |
| `-SkipBuild` | Do not run `cargo build` |
| `-SkipConvert` | Do not convert (with an existing `<converted>`, just write the launcher) |
| `-CpuJobs N` / `-IoJobs N` | Passed to the converter; 0 keeps its defaults (all cores / 2) |
| `-DryRun` | Print every command, run nothing |

Then start it:

```powershell
<converted>\play-demo.cmd
# or, by hand:
target\release\engine.exe --assets "<converted>" --demo alftand --walk
```

`tools/demo/play-demo.ps1` is the portable launcher behind that file: it finds `engine.exe`
relative to the repository, so it works from a checkout anywhere. Its `-Start` takes `alftand` or
`blackreach`:

```powershell
pwsh -File tools/demo/play-demo.ps1 -Assets "<converted>" -Start alftand
pwsh -File tools/demo/play-demo.ps1 -Assets "<converted>" -Start blackreach
```

The engine's own `--demo` also takes `riverwood`; the portable launcher does not offer that name
yet, so start the village demo with the engine directly:

```powershell
target\release\engine.exe --assets "<converted>" --demo riverwood --walk
```

`play-riverwood-demo.cmd` in the repository root is the lead's own launcher for exactly that start
(hard-coded paths; see section 9).

## 4. Step by step, by hand

### 4.1 Build

```powershell
cargo build --release -p converter --bin converter
cargo build --release -p engine --bin engine
```

The engine is `target\release\engine.exe` (`target/release/engine` on Linux), the converter is
`target\release\converter.exe`. `cargo check --workspace` is the fast way to check the tree builds
while you work; `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D
warnings` are what CI enforces.

For a walking tour - and for nothing else - there is a second profile that leaves out the
link-time pass `release` pays for:

```powershell
cargo build --profile release-fast -p engine
```

It builds the same optimised engine into `target\release-fast\engine.exe`. The first build pays for
the profile, because it compiles every dependency into `target\release-fast\` as well: 6m 32s on
this machine (2026-09-24, ~500 crates), against 4m 43s and 5m 49s for two `release` builds the same
day that recompiled only `shared` and the engine crate. After that only the engine crate is rebuilt.
**Never take a timing, a benchmark or a sign-off from it**: `lto = false` changes the frame rate,
and the tour's own capture density comes from the frame rate, so its frames are evidence of
*behaviour*, not of performance. `[profile.release]` (`lto = "thin"`, `codegen-units = 1`) is
unchanged and is the profile everything measured or published uses.

### 4.2 Convert

```powershell
target\release\converter.exe `
  "<SkyrimSE>\Data" `
  "<converted>" `
  --cpu-jobs 16 --io-jobs 2 `
  --report-json "<converted>-conversion-report.json"
```

- The first positional is the **`Data`** directory, not the install root. The second is the output;
  it must be a different folder, and it should be outside this repository.
- Plugins are discovered, not selected: every `.esm`/`.esp`/`.esl` in `Data` is converted (there is
  no `--plugins` flag). The five official masters get a fixed rank, everything else is ordered
  alphabetically.
- `--fail-fast` aborts on the first archive error instead of degrading to a warning.
- The conversion is complete when `<converted>\conversion-manifest.json` says `"complete": true`.
  Any warning sets it to `false`, and the engine then refuses to start.
- The output layout is `skyrim_world.db`, `cell_cache.rkyv`, `meshes/**/*.glb`, `textures/**/*.ktx2`,
  `scripts/*.luau`, the extracted `vfs/` (the largest part, and regenerable), and
  `.ingestion-cache/`.

**Interrupted?** The converter writes into `<converted>.staging-<pid>-<nanos>` next to the output
and renames it into place at the end, so a killed run leaves that staging folder behind. Continue it
instead of starting over:

```powershell
target\release\converter.exe "<SkyrimSE>\Data" "<converted>" `
  --resume-staging "<converted>.staging-<pid>-<nanos>"
```

`setup-demo.ps1` looks for that folder and adds the flag itself.

### 4.3 Run

```powershell
target\release\engine.exe --assets "<converted>" --demo alftand --walk
```

The engine checks the asset folder before it opens a window, and refuses to start unless
`skyrim_world.db` and `cell_cache.rkyv` exist, `conversion-manifest.json` has the current converter
schema (`18`) and `"complete": true`, and `integration-report.json` has the current database schema
(`5`) and `"passed": true`.

### 4.4 Walk a tour, or smoke one

The scripted tour walks a demo's route by itself and photographs every place and door:

```powershell
target\release\engine.exe --assets "<converted>" --demo riverwood --walk --demo-tour "<repo>\local\demo\tour-1"
```

With `--walk` it walks each doorway instead of activating the door itself - pressing `E`, holding
`W` and photographing the frames either side of every crossing into `walk-through\<stage>\` - and
finishes by holding `W` for four seconds to check the player walks on the ground. `tour.txt` holds
the log and the verdict, `tour PASSED after 8 crossings` for Riverwood and `... after 4 crossings`
for Alftand; `frames.txt` in a `walk-through` folder names that crossing's swap frame and the
twenty-odd frames of the window kept around it.

A stage waits for the place it stands in to stream in rather than sleeping a flat ten seconds, and
logs how long that took (`stage 3: settled after 0.17 s (10 quiet frames)`, or
`not settled after 10 s (...); photographing it anyway` when a place never arrives). The
walk-through keeps a ring of the frames it takes on the way in and the window around the swap, so a
tour leaves the frames it is judged on and not the thousand the older build wrote. About three
quarters of the old time was flat waiting (`docs/research/faster-automated-checks.md`, which
measured the two "before" numbers here from recorded logs):

| Full tour, `--walk` | before | `release` | `release-fast` |
|---|---|---|---|
| Riverwood (8 crossings) | 3m 53s | 1m 46s | 1m 51s |
| Alftand (4 crossings) | 2m 13s | 1m 01s | 1m 03s |

The frames left in `walk-through/` were 2,056 (3.3 GB) for the Riverwood tour in the last run before
the change and 165 after (162 and 165 across the two profiles here; the window is 20 or 21 frames
per crossing, and Alftand's 84 are four of those windows). Every one of those runs ended
`tour PASSED after 8 crossings` - `4` for Alftand - with the walk test grounded and the same
verdict as before.

For a quick check while working on the engine, walk the first door only:

```powershell
target\release\engine.exe --assets "<converted>" --demo riverwood --walk --demo-tour "<repo>\local\demo\smoke" --tour-doors 1
```

`--tour-doors N` walks the first `N` doors of the route and stops after the last crossing - no
route-end look-around, no walk test - and its verdict is `tour SMOKE after N crossings`. The
distinct word is the point: a smoke run is a quick check that the engine still walks the route, and
never a sign-off. A one-door smoke run of Riverwood took about 40 s. The full tour above is the one
that checks every crossing and the walk test.

## 5. Controls

| Input | Action |
|---|---|
| Left click | Grab the mouse (the window must have focus) |
| Mouse | Look |
| `W` `A` `S` `D` | Walk |
| `Shift` | Run (150 → 350 units per second, `WALK_SPEED` / `RUN_SPEED` in `crates/engine/src/player.rs`) |
| `Space` | Jump |
| `E` | Open the load door you are looking at and cross it |
| `F` | Toggle free flight (mouse to look, `Space` up, `Shift` down, `Ctrl` fast) |
| `Esc` | Release the mouse |
| Close the window | Quit |

The HUD line in the corner is the same list. The start objective is printed for `--demo` runs, so
you know which way the route goes.

## 6. Engine options that are useful for the demo

Full list: `crates/engine/src/config.rs`.

| Option | Effect |
|---|---|
| `--assets <dir>` | The converted asset folder (defaults to `modern_assets`, which will not exist) |
| `--demo alftand\|blackreach\|riverwood` | The three named starts: the Alftand entrance, straight into Blackreach, and the Helgen road south-west of Riverwood |
| `--walk` | First-person player instead of the free-flight camera |
| `--demo-tour <dir>` | Scripted run: walks the route of the demo the run started in, door by door — Riverwood's eight doorways, or Alftand's four, which is also the route a run with no `--demo` follows — and screenshots every place and door. With `--walk` it walks each doorway rather than activating the door itself, pressing `E` and photographing the frames around the crossing, and finishes by holding `W` for four seconds to check the player walks on the ground. Good for checking a build without playing it. Section 4.4 |
| `--tour-doors N` | With `--demo-tour`: walk only the first `N` doors of the route, then stop and print `tour SMOKE after N crossings`. A smoke tour for iteration, never a sign-off. Section 4.4 |
| `--shots <file>` `[--shots-out <dir>]` | Render the camera poses in a shots file to PNGs and exit — see `docs/design/reference-shots.md` |
| `--terrain-radius N` | Distance of the terrain-only ring in cells beyond the full-detail grid (default 8). It is the main frame-rate knob on a wide view: 8 costs roughly half the frame rate of no ring at all |
| `--stream-radius N` | Full-detail grid radius around the camera (default 2) |
| `--start-position X Y Z`, `--start-yaw R` | Start anywhere, in Creation units and radians |
| `--worldspace 0x3c` | Start worldspace (60 is Tamriel, `0x1EE62` is Blackreach) |
| `--allow-incomplete-assets` | Start despite an incomplete or stale conversion — for looking at what did convert, not for a normal run |

Benchmark and acceptance options (`--headless`, `--benchmark-*`, `--accept-*`, the `*-fixture`
switches) belong to `scripts/phase2-*.ps1` and the roadmap docs, not to the demo.

## 7. Reference shots

The engine can render exact camera poses so a frame can be compared against a screenshot of the
real game:

```powershell
target\release\engine.exe --assets "<converted>" --shots <poses.json> --shots-out <output-dir>
```

The shots file format (degrees, Creation units, one of `worldspace_id` / `interior_cell_id` per
shot) is in [`docs/design/reference-shots.md`](../design/reference-shots.md). A run renders each
shot once streaming settles, writes `<output-dir>/shots.log`, and exits. To compare against a
reference image:

```powershell
python tools/compare_shots.py --help
python tools/research/exposure_stats.py --shots <poses.json> --render <output-dir>
```

## 8. Troubleshooting

**Where are the logs?** There is no log file: the engine logs to the console (Bevy's `LogPlugin`),
so start it from a terminal and read the window you started it from. `RUST_LOG` overrides the
level, for example `$env:RUST_LOG = "wgpu=error,bevy_render=info"`. The converter prints progress
and a final `Converted X, reused Y, skipped Z in N ms (complete: ...)` line, and writes its full
report to `--report-json`.

**`converted asset set is missing skyrim_world.db: <path>`** — `--assets` points at the wrong folder,
or the conversion never ran.

**`asset conversion is incomplete or stale; reconvert assets with converter schema 18`** — the
manifest is missing, has `"complete": false` (some input produced a warning), or was written by an
older converter. The engine requires exactly the converter schema it was built with, so a converter
change that bumps the schema makes every existing asset folder stale, however little changed on
your side of it — **schema 18 is the glossiness → roughness mapping
(`crates/converter/src/material.rs`), and a folder converted before it has to be reconverted before
the engine will open a window on it**. Look at `<converted>\conversion-manifest.json`
(`schema_version`, `complete`, `failures`, `inputs_by_kind`) and at the conversion report for the
warning, then convert again as in section 4.2:

```powershell
target\release\converter.exe `
  "<SkyrimSE>\Data" `
  "<converted>" `
  --cpu-jobs 16 --io-jobs 2 `
  --report-json "<converted>-conversion-report.json"
```

A re-run into an existing folder reuses the extraction cache and the per-asset manifest, so it
takes the "Re-conversion" time in section 2 rather than the cold one. The engine's required
converter schema is a constant in `crates/engine/src/app.rs` (`converter_schema_version`), kept in
step with `converter::cache::CONVERTER_SCHEMA_VERSION` by hand.

**`asset integration report did not pass`** — `<converted>\integration-report.json` has
`"passed": false` or an old schema. Its `issues` list names the specific models or textures.

**`Access is denied` while publishing** — the converter renames its staging folder over the output
at the end, and that fails if anything holds a file in the output open. That is almost always a
**running engine** on the same asset folder (or a SQLite client holding `skyrim_world.db`). Close
it and run again; the staging folder is still there, so
`converter.exe ... --resume-staging <staging dir>` republishes without redoing the work.

**The build fails looking for `link.exe`, `cl.exe` or a `.lib`** — the MSVC C++ build tools are not
installed (or not the current ones). See section 2.

**`cargo` says it is waiting for a file lock** — another cargo build is running against the same
`target/`. Wait for it; do not delete the lock or the directory.

**The window opens black, or the engine exits immediately** — check the console first: missing
assets and a bad manifest both exit before rendering. If the console is clean, the GPU/driver is
the suspect: update the driver, and confirm the machine has a Vulkan or D3D12 driver exposed to
`wgpu`.

**A few seconds of stutter at the start, and when crossing a door** — cells stream in the
background on a commit budget. `--stream-radius` and `--terrain-radius` both cost frame rate.

**The demo start is not where you expect** — `--demo alftand` uses the exact arrival point the game
itself stores for the Alftand01 exit door, which is *outside* the ruins, facing away from the
entrance. The auto-load door back in is behind you. `--demo riverwood` starts on the Helgen road
south-west of the village, looking up the street: the first doorway (Sven's) is about 1,100 units
ahead.

## 9. Where things live

| | |
|---|---|
| Converter CLI | `crates/converter/src/main.rs` (usage line at `usage()`) |
| Engine CLI and defaults | `crates/engine/src/config.rs` (the `--demo` starts at `DemoStart::named`) |
| Player controller and controls | `crates/engine/src/player.rs` |
| Door crossing | `crates/engine/src/doors.rs`, `crates/engine/src/transition.rs` |
| Load-door and `Idle` clips | `crates/engine/src/door_animation.rs`, `crates/engine/src/model_animation.rs` |
| Cell streaming | `crates/engine/src/streaming.rs` |
| Per-space lighting and fog | `crates/engine/src/world/lighting.rs`, `update_atmosphere` in `crates/engine/src/app.rs` |
| Scripted tour | `crates/engine/src/demo_tour.rs` (the routes are the constants at the top) |
| Demo route research | `docs/research/worldspace-transition-demo.md` |
| Demo design | `docs/design/blackreach-demo.md`, `docs/design/riverwood-demo.md` |
| Reference-shot contract | `docs/design/reference-shots.md` |
| Phase 2 profiling and acceptance | `docs/roadmap/02-profiling.md`, `scripts/phase2-*.ps1` |

The `play-blackreach-*.cmd` files and `play-riverwood-demo.cmd` in the repository root are the
lead's own launchers for one particular machine, with hard-coded paths. `tools/demo/` is the
portable version: use those.
