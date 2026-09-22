# Running the Alftand → Blackreach demo

This is the contribution guide for the demo the project uses to show what the engine can do: on
foot from the Alftand entrance in the Pale, down four load doors, into Blackreach, with no loading
screen anywhere. It is the fastest way to get a real, running OpenSkyrim on your machine.

Everything here runs from **your own copy of Skyrim Special Edition**: this repository contains no
Bethesda data, and none may ever be committed. The converter reads your install read-only and
writes a converted asset folder somewhere of your choosing.

Paths are placeholders: `<SkyrimSE>` is your install (`.../steamapps/common/Skyrim Special
Edition`), `<converted>` is the folder the converter writes.

- Design and history: [`docs/design/blackreach-demo.md`](../design/blackreach-demo.md)
- The route, in the game's own data: [`docs/research/worldspace-transition-demo.md`](../research/worldspace-transition-demo.md)

## 1. What the demo is

| | |
|---|---|
| Start | Outside the Alftand entrance in the Pale (Tamriel, cell grid 19,18) — or straight into Blackreach with `--demo blackreach` |
| Route | Tamriel → `Alftand01` → `Alftand02` → `AlftandWorld` → Blackreach, four load doors |
| Crossing | Walking into a load door crosses it. The destination was streamed in while you approached, so there is no loading screen |
| Also | Mouse look, walking, running, jumping, flight mode, and seeing through a doorway into the next interior |

### What works

- Seamless interior and worldspace crossings, in both directions, with the arrival camera placed
  where the game's own `XTEL` data puts the player.
- Walking on the converted terrain and meshes: WASD, Shift to run, Space to jump, F for free flight.
- Load doors open with **E**, and the auto-load doors (the ones the game fires on contact) also
  cross on contact.
- Skyrim's own object placement, rotations, scales and bounds; per-reference point lights from
  `LIGH` records (`crates/engine/src/lights.rs`), terrain out to `--terrain-radius` cells with
  distance fog, and the reference-shot camera (`--shots`).

### What does not work yet

Nothing below is a mystery: each is a known, recorded gap in `AGENTS.md`'s handoff log or in
`docs/research/`.

- **No NPCs, creatures, animation or skinning.** Nothing walks but you. Phase 4.
- **No physics beyond walking**, no combat, no item pickup, no inventory, quests or menus. There is
  no physics engine: walking and the ground height come from ray casting against the streamed
  meshes and reference bounds.
- **No audio.**
- **No saves.** Every run starts from the demo's start point.
- **No glow or emissive.** Blackreach's glowing crystals do not light up.
- **No per-cell lighting templates (`XCLL`/`LTMP`) or interior fog.** Interiors are lit by ambient
  light plus the converted `LIGH` point lights, so the lighting is flat, and a light overexposes
  right next to it (inverse-square; see `EXPOSURE_CALIBRATION` in `crates/engine/src/lights.rs`).
- **No weather and no snow shader.** The sky is a plain gradient; roofs render bronze.
- **Ice reads as rock.**
- **Loosely placed `BlackPlane01` shapes leave black voids.**
- **The terrain layer blending is blocky** (`docs/research/terrain-blending-seams.md`, not yet fixed).
- **Doors do not animate** when they open: the crossing is instant, the mesh stays put.
- **Two Blackreach models fail conversion validation** (`WispAmbush`, `FrostSpiderAmbush01`) and
  never spawn; one 4334-unit-radius `FalmerCityLight02NS` floods the Blackreach ceiling orange.
- **Scripts are converted but not used for gameplay**: `.pex` becomes Luau, and the Papyrus runtime
  exists, but no quest or dialogue logic runs yet.
- A portal destination's lights can faintly reach the active space, because GPU light clustering
  ignores render layers.

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
relative to the repository, so it works from a checkout anywhere.

```powershell
pwsh -File tools/demo/play-demo.ps1 -Assets "<converted>" -Start alftand     # or blackreach
pwsh -File tools/demo/play-demo.ps1 -Assets "<converted>" -Start blackreach
```

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
schema (`15`) and `"complete": true`, and `integration-report.json` has the current database schema
(`5`) and `"passed": true`.

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
| `--demo alftand\|blackreach` | The two named starts |
| `--walk` | First-person player instead of the free-flight camera |
| `--demo-tour <dir>` | Scripted run: walks the whole route by itself, screenshots every place and door, then holds `W` in Blackreach. Good for checking a build without playing it |
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

**`asset conversion is incomplete or stale; reconvert assets with converter schema 15`** — the
manifest is missing, has `"complete": false` (some input produced a warning), or was written by an
older converter. Look at `<converted>\conversion-manifest.json` (`complete`, `failures`,
`inputs_by_kind`) and at the conversion report for the warning; reconvert after fixing it. The
engine's required converter schema is a constant in `crates/engine/src/app.rs`
(`converter_schema_version`), kept in step with `converter::cache::CONVERTER_SCHEMA_VERSION` by
hand — a converter change that bumps it makes every existing asset folder stale.

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
entrance. The auto-load door back in is behind you.

## 9. Where things live

| | |
|---|---|
| Converter CLI | `crates/converter/src/main.rs` (usage line at `usage()`) |
| Engine CLI and defaults | `crates/engine/src/config.rs` |
| Player controller and controls | `crates/engine/src/player.rs` |
| Door crossing | `crates/engine/src/doors.rs`, `crates/engine/src/transition.rs` |
| Cell streaming | `crates/engine/src/streaming.rs` |
| Demo route research | `docs/research/worldspace-transition-demo.md` |
| Demo design | `docs/design/blackreach-demo.md` |
| Reference-shot contract | `docs/design/reference-shots.md` |
| Phase 2 profiling and acceptance | `docs/roadmap/02-profiling.md`, `scripts/phase2-*.ps1` |

The two `play-blackreach-*.cmd` files in the repository root are the lead's own launchers for one
particular machine, with hard-coded paths. `tools/demo/` is the portable version: use those.
