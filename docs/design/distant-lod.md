# Distant LOD: terrain, object and tree level-of-detail beyond the streamed cells

**Status: design, not implemented.** Scope: how the engine draws the world past the full-detail streamed cells using Skyrim's own distant LOD data. Non-goal: widening the full-detail stream radius (a wider full-resolution ring is a development testing aid, not a design — it does not scale, and the LOD data exists precisely so it is unnecessary).

Roadmap anchor: `docs/roadmap/02-core-engine.md:5` ("zero-loading-screen spatial streaming … massive Skyrim render distances"); acceptance constraints come from `docs/roadmap/02-acceptance.md:54-60` (average FPS ≥ 60, frame P95 ≤ 16.67 ms, memory growth ≤ 0.5 GiB, zero streaming failures, signed visual review).

---

## 1. Problem

The engine streams full-detail cells within `stream_radius` (default 2, `crates/engine/src/config.rs:9`, `:46`) and draws **nothing** beyond them: `plan_cells` only ever selects cells inside the radius (`crates/engine/src/streaming.rs:170-249`) and the renderer's far plane therefore shows sky and fog. Skyrim itself shows terrain, buildings, bridges and trees to roughly 20 000–250 000 units (5–60 cells) using three separate **precomputed, file-driven** systems. This design consumes that data instead of inventing a renderer-side replacement.

Two facts make this cheap today:

1. The converter already **extracts every LOD file into its VFS** and already **converts every LOD texture to KTX2** (verified on a converted asset set: `vfs/meshes/terrain/**` and `textures/terrain/**` are populated, including the object and tree atlases).
2. The LOD meshes are NIFs of the same SSE generation the converter already parses, and the converter's own header parser (`crates/converter/src/mesh.rs:761`) already matches their header layout byte for byte.

## 2. What Skyrim ships (measured, not quoted)

Everything below was measured on the already-extracted vanilla data inside a converted asset set (which mirrors the game's BSAs) or from the shipped INI presets in the installation folder. Counts are per worldspace unless stated.

### 2.1 Inventory

| System | Path pattern | Naming | Levels present (Tamriel) | Whole-game count |
|---|---|---|---|---|
| Terrain LOD | `meshes/terrain/<ws>/<ws>.<level>.<x>.<y>.btr` | `<x>,<y>` = **cell coordinates of the block's south-west cell**, multiples of `<level>` | 4 → 2304 blocks, 8 → 576, 16 → 144, 32 → 36 | 9584 `.btr` |
| Object LOD | `meshes/terrain/<ws>/objects/<ws>.<level>.<x>.<y>.bto` | same convention | 4 → 517, 8 → 152, 16 → 48 (**no level 32**) | 1079 `.bto` |
| Tree LOD types | `meshes/terrain/<ws>/trees/<ws>.lst` | one entry per tree type | — | 9 `.lst` |
| Tree LOD instances | `meshes/terrain/<ws>/trees/<ws>.<level>.<x>.<y>.btt` | level 4 only | 4 → 329 | 386 `.btt` |
| LOD textures (terrain) | `textures/terrain/<ws>/<ws>.<level>.<x>.<y>.dds` + `_n.dds` | one diffuse + one normal per block | — | 3061 × 2 for Tamriel |
| LOD texture (objects) | `textures/terrain/<ws>/objects/<ws>.objects.dds` + `_n` | atlas | — | 1 + 1 |
| LOD texture (trees) | `textures/terrain/<ws>/trees/<ws>TreeLOD.dds` | atlas, one per worldspace | — | 1 |
| LOD grid metadata | `lodsettings/<ws>.lod` | 16 bytes | — | 12 |

All LOD **meshes** live in `Skyrim - Meshes1.bsa`; `Skyrim - Meshes0.bsa` contains none (string counts over the archives: 9584 `.btr`, 1079 `.bto`, 386 `.btt`, 9 `.lst` in archive 1, effectively zero in archive 0). All LOD **textures** are already converted and published as KTX2 (Tamriel terrain alone: 6122 KTX2 files, 538 MB).

### 2.2 Formats (byte-verified)

**`.btr` and `.bto` are NIFs.** Header of `tamriel.32.0.0.btr`: `Gamebryo File Format, Version 20.2.0.7`, version `0x14020007`, endian little, user version 12, block count 10, **Bethesda version 100**, then **three** sized strings (u8 length + bytes), a **u16** block-type count, u32-length block-type strings, then per-block type index (u16) and size (u32). That is exactly what `crates/converter/src/mesh.rs:761` (`parse_skyrim_header`) reads.

Block types are a small, fixed set:

| File | Block types present |
|---|---|
| `.btr` (sample: 10 blocks) | `BSMultiBoundNode`, `BSTriShape`, `BSLightingShaderProperty`, `BSShaderTextureSet`, `BSMultiBound`, `BSMultiBoundAABB` |
| `.bto` (sample: 12 blocks) | `NiNode`, `BSMultiBoundNode`, `BSSubIndexTriShape`, `BSLightingShaderProperty`, `BSShaderTextureSet`, `BSMultiBound`, `BSMultiBoundAABB` |

Both are the ordinary shape/property blocks the converter already handles (`BSTriShape`, `BSSubIndexTriShape`, `BSLODTriShape`, `BSShaderTextureSet` are all in the vendored reader's dispatcher, `vendor/project-wormhole-nif/src/nif_block.rs:278`); the only new block, `BSMultiBoundNode` (plus its bound companions), is currently a stub that falls through to `NifBlock::Unhandled` (`vendor/project-wormhole-nif/src/nif_block.rs:527-530`, stubs at `:755-770`).

**Block sizes are not uniform**: level-4 blocks range 14–36 KB, level-16 ~36 KB, level-32 up to 82 KB, so the LOD mesh's vertex density is data-dependent, not a fixed grid per block. Treat the mesh as opaque geometry.

**`.lod` files are 16 bytes**: `int16 origin_x, int16 origin_y, int32 cells_per_side, int32 min_level, int32 max_level`. Measured: Tamriel `(-96, -96)`, 256, 4, 32; Solstheim `(-64, -64)`, 256, 4, 32; Blackreach `(-23, -9)`, 32, 4, 32. Shipped `.btr` block coordinates for Tamriel start exactly at the origin and cover 192 cells (`-96 … +92` in steps of 4), which is why **block availability must be taken from the files, not derived from the `.lod` extents** (open question 5).

**`.lst` is a table of billboard types** (Tamriel: 1092 bytes): `u32 count` (34), then `count` × 32-byte entries; the entries decode as `u32 index, f32 size_x, f32 size_y, f32 u0, f32 v0, f32 u1, f32 v1, f32 unused` — the last float is 0 in every sampled entry and each `(u0,v0,u1,v1)` is a sane sub-rectangle of the atlas (field *names* are inferred; see open question 4).

**`.btt` holds instanced billboards** (sample `tamriel.4.-12.-12.btt`: 4492 bytes; leading words `9, 24, 5`, then floats `-49049.4, -46474.7, 773.1, 0.781, 1.02` — recognizable Creation-unit world positions). The exact record layout is **not** resolved here; it is small, self-contained, and cross-checkable against the database (open question 3).

### 2.3 Distances

Shipped presets in the installation folder (primary source; these are the SSE values, **not** the Classic values that circulate on wikis):

| Setting | Low | Medium | High | Ultra | Engine units | Cells |
|---|---|---|---|---|---|---|
| `fBlockLevel0Distance` | 15000 | 20000 | 35000 | 60000 | 20000 | 4.9 |
| `fBlockLevel1Distance` | 25000 | 32000 | 70000 | 90000 | 32000 | 7.8 |
| `fBlockMaximumDistance` | 100000 | 100000 | 250000 | 250000 | 100000 | 24.4 |
| `fSplitDistanceMult` | 0.5 | 1.1 | 1.5 | 1.5 | — | — |
| `fTreeLoadDistance` | 12500 | 75000 | 75000 | 75000 | 75000 | 18.3 |

(`fLODFadeOutMultObjects` is also shipped: 9 on High, 30 on Ultra.) The engine's `stream_radius = 2` corresponds to Skyrim's own full-detail grid size; the LOD bands therefore begin where the streamer ends.

## 3. What the engine does today

| Concern | Current behaviour | Location |
|---|---|---|
| Cell selection | square ring around the camera cell, `stream_radius`; unload at `unload_radius` | `crates/engine/src/streaming.rs:170`, `:222` |
| DB access | one background worker thread, request/response channel, read-only SQLite | `crates/engine/src/world/database.rs:54`, `:202` |
| Cell payload | `CellKey` + `ReferenceRow[]` (position, rotation, scale, bounds, `model_path`) | `crates/engine/src/world/database.rs:22`, `:32`, `:46` |
| Spawn | one root per cell with `StreamedCellRoot` + `ExteriorCellGrid`; children = terrain quadrants, water, glTF scenes per reference | `crates/engine/src/streaming.rs:409` |
| Placement | `cell_translation` = `(grid - origin) * CELL_SIZE`; `CELL_SIZE = 4096` | `crates/engine/src/streaming.rs:1258`, `crates/engine/src/world/components.rs:3` |
| Floating origin | camera-cell shift rebases every entity carrying `ExteriorCellGrid` | `crates/engine/src/streaming.rs:1608-1645` |
| Rendering | Bevy GPU preprocessing / indirect draws, `DepthPrepass`, HZB occlusion; terrain uses `ExtendedMaterial<StandardMaterial, TerrainExtension>` with 6 KTX2 layers | `crates/engine/src/render.rs:27`, `:167`, `:276`; `crates/engine/src/shaders/terrain.wgsl` |
| Plugin wiring | `VercidiumRendererPlugin`, `StreamingPlugin` | `crates/engine/src/app.rs:127`, `:134` |
| Converter input filter | only `dds`, `nif`, `pex` enter conversion; `.btr/.bto/.btt/.lst` are skipped | `crates/converter/src/pipeline.rs:963-970` |
| Empty-geometry guard | a NIF that declares geometry but yields none is a hard error | `crates/converter/src/mesh.rs:87-95`, `:437` |
| `lod` table | created, **never written** anywhere in the workspace | `crates/converter/src/esm/exporter.rs:63` |
| STAT records | `flags` (u32 record flags, so `0x8000` Has Distant LOD is present) and `MODL` are stored; **MNAM is not** | `crates/converter/src/esm/exporter.rs:50`, `:177-183` |
| Versions | `WORLD_DATABASE_SCHEMA_VERSION = 3`, converter manifest schema 14, cell cache 3 | `crates/shared/src/lib.rs:7`; `crates/converter/src/cache.rs` |
| Test fixtures | deterministic synthetic NIF/ESM/BSA/DDS/PEX writers, SSE bsver 100 | `crates/dummy-content/src/nif.rs:16`, `:42` |
| Acceptance | required-test list, expected-scenario list, per-scenario screenshots and visual sign-off | `scripts/phase2-acceptance.ps1:225-241`, `:350-351`, `crates/converter/src/bin/runtime-asset-audit.rs:59`, `:134` |

## 4. Options

### A. Convert Skyrim's precomputed LOD as-is
`.btr`/`.bto` → GLB per block (textures already converted); `.lst`/`.btt` → generated billboard meshes plus instance rows.

### B. Generate LOD at conversion time from LAND + STAT MNAM (xLODGen-like)
Decimate `LAND` heightmaps into block meshes, **bake per-block LOD textures** from LTEX/TXST layers, merge every static whose STAT has the Has Distant LOD flag and MNAM LOD models into per-block meshes, and render tree billboards from TREE models.

### C. Hybrid
A for everything vanilla; B's machinery only for blocks a plugin actually overrides ("dirty blocks"), starting where it is cheapest and most visible (object LOD from MNAM; terrain silhouette from `LAND`).

| | A. As-is | B. Generate | C. Hybrid (A now, B for dirty blocks) |
|---|---|---|---|
| New format code | NIF variant: 1 node block + 3 bound blocks; `.lod`, `.lst`, `.btt` readers | `LAND` decimation, geometry merge, texture baker, billboard renderer | A's readers + geometry regeneration; no texture baker in the first increment |
| New renderer code | streaming tier, LOD materials, band rules | same, plus a generated-mesh ingestion path | same as A |
| Fidelity vs vanilla | **exact** — this is the data the reference screenshots came from | differs (bake quality, atlas layout, vertex-colour policy); no ground truth to review against | exact until a mod touches a block, approximate there |
| Mod compatibility | weak: mod-changed `LAND` and mod-added statics are wrong/invisible beyond the ring | best | good for overridden cells, exact elsewhere |
| Cost of failure | low (a block renders wrong) | high (an entire generated asset set is wrong with no reference) | medium |
| Risk | low — formats are now measured; only `.btt`/`.lst` layouts are unresolved, and small | high — large new subsystems, long conversion times, `fSplitDistanceMult`/HD-LOD/alpha-threshold behaviours to reverse-engineer | medium — deferred |
| Time to first horizon | small | large | small, then incremental |

## 5. Recommendation

**Adopt C, staged: implement A completely (terrain, objects, trees), then add the dirty-block regeneration of B incrementally, starting with object LOD from STAT MNAM.**

Reasons:

1. **A is the only option whose output can be reviewed against ground truth.** The Phase 2 acceptance gates are threshold comparisons plus a signed visual review; converting the shipped LOD gives the same distant content the reference screenshots show, so "correct" is defined. B's first release has no oracle.
2. **The expensive half of B is the texture baker, not the geometry.** Terrain LOD diffuse and normal maps are *already published as KTX2*; regenerating geometry from `LAND` is tractable, regenerating the textures is not (atlas packing, mip policy, layer blending). C defers exactly the expensive part.
3. **A's format risk is now bounded.** The header matches the converter's existing parser; the block set is small; the one unsupported node block is a dispatcher entry, not a new parser. `.btt`/`.lst` remain unresolved but are tiny and validated by ground truth (billboard positions must match TREE/STAT references the database already contains).
4. **Mod compatibility degrades gracefully, not functionally.** A mod that edits `LAND` or adds statics makes the *distance* view stale in those cells only; the full-detail view inside the ring is already authoritative, and the converter can identify overridden cells exactly (per-plugin records are in the database), so C's regeneration has a precise work list rather than a global rebuild.
5. **The far horizon never being empty is what the roadmap asks for.** With 36 level-32 and 144 level-16 blocks permanently resident, the horizon is populated even while finer blocks load — that is the zero-loading-screen property, and it is cheap to implement under either option.

## 6. Design

### 6.1 What the converter must produce

**Meshes (files, mirroring the existing `model_path` convention — paths in the DB, bytes on disk):**

| Output | Source | Notes |
|---|---|---|
| `meshes/terrain/<ws>/<ws>.<level>.<x>.<y>.glb` | `.btr` | block-local coordinates with the SW corner at the origin, run through the existing `CREATION_TO_RUNTIME_ROTATION`; vertex colours preserved (level 4 has them) |
| `meshes/terrain/<ws>/objects/<ws>.<level>.<x>.<y>.glb` | `.bto` | same; atlas UVs are already baked into the source mesh |
| `meshes/terrain/<ws>/trees/<ws>.<n>.glb` | generated from `.lst` entry *n* | two double-sided quads crossing at 90°, 1×1 in local space, UVs = the entry's atlas rectangle |
| `textures/terrain/**` | already produced | no work; verified present as KTX2 |

`converted_model_path`/`converted_texture_path` already map `meshes/**` → `.glb` and `textures/**` → `.ktx2` (`crates/engine/src/streaming.rs:1281`, `crates/engine/src/world/database.rs:127`), so the GLBs must reference the atlas/diffuse/normal by their normalised texture path and the runtime asset audit (`crates/converter/src/bin/runtime-asset-audit.rs:59`) will enforce that those files exist.

**Database (schema version 3 → 4):** the existing `lod(cell_id, lod_level, mesh_data)` table has the wrong shape — LOD blocks are not cells and mesh bytes do not belong in rows — and it is dead today. Replace it:

```sql
CREATE TABLE lod_grid (            -- from lodsettings/<ws>.lod
    worldspace_id INTEGER PRIMARY KEY,
    origin_x INTEGER NOT NULL, origin_y INTEGER NOT NULL,
    levels TEXT NOT NULL           -- e.g. "4,8,16,32"
);
CREATE TABLE lod_block (
    worldspace_id INTEGER NOT NULL, kind TEXT NOT NULL,   -- 'terrain' | 'objects'
    level INTEGER NOT NULL, block_x INTEGER NOT NULL, block_y INTEGER NOT NULL,
    mesh_path TEXT NOT NULL, bounds_min_x REAL, bounds_min_y REAL, bounds_min_z REAL,
    bounds_max_x REAL, bounds_max_y REAL, bounds_max_z REAL,
    PRIMARY KEY (worldspace_id, kind, level, block_x, block_y)
);
CREATE TABLE lod_tree_type (       -- from .lst
    worldspace_id INTEGER NOT NULL, tree_index INTEGER NOT NULL,
    mesh_path TEXT NOT NULL, size_x REAL NOT NULL, size_y REAL NOT NULL,
    u0 REAL NOT NULL, v0 REAL NOT NULL, u1 REAL NOT NULL, v1 REAL NOT NULL,
    PRIMARY KEY (worldspace_id, tree_index)
);
CREATE TABLE lod_tree_instance (   -- from .btt
    worldspace_id INTEGER NOT NULL, block_x INTEGER NOT NULL, block_y INTEGER NOT NULL,
    tree_index INTEGER NOT NULL, pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
    rotation REAL NOT NULL DEFAULT 0, scale REAL NOT NULL DEFAULT 1
);
CREATE INDEX idx_lod_tree_block ON lod_tree_instance(worldspace_id, block_x, block_y);
```

`block_x/block_y` are the SW cell coordinates (the data's own convention). Bounds are required: the LOD spawn path validates them the way references are validated (`crates/engine/src/streaming.rs:926`), and HZB occlusion needs them.

### 6.2 Distance bands and block selection

Bands are a config list, defaults from the shipped **Medium** preset (High/Ultra available by CLI):

| Band | Terrain | Objects | Trees |
|---|---|---|---|
| full detail (`stream_radius` = 2, ±8192) | streamed cells | streamed references | streamed references |
| ≤ 20000 | `.btr` level 4 | `.bto` level 4 | billboards (≤ 75000) |
| ≤ 32000 | level 8 | level 8 | |
| ≤ 100000 | level 16 | level 16 | |
| ≤ 250000 | level 32 | — (vanilla ships none) | |

Rules:

- **Block coordinate for cell `(cx, cy)` at level `L`** is `(cx.div_euclid(L) * L, cy.div_euclid(L) * L)` — verified against the shipped file names (all coordinates are multiples of the level).
- **Residency** is decided by distance from the camera to the block's cell rectangle, with hysteresis (unload at 1.1 × the band distance) mirroring the existing cell stream/unload radii.
- **A block is drawn unless it is fully inside the resident full-detail rectangle.** Fully covered blocks are skipped, so the LOD never competes with full-detail geometry for the same pixels.
- **Every coarser level stays resident underneath the finer one.** The horizon is therefore never empty while a finer block loads or after a block load fails; this is the concrete mechanism behind "zero loading screens" at the LOD scale, and it bounds a missing block's visual cost to one LOD step.
- **Commit budget is shared** with cell streaming (`max_cell_commits_per_frame`, `max_commit_micros_per_frame`, `crates/engine/src/streaming.rs:35`, `:48-49`) so LOD spawns cannot create a frame spike that breaks the P95 gate.
- **Tree instances are read with their level-4 block**, so tree LOD loads and unloads with the terrain band it belongs to.

### 6.3 Where it plugs in

| Change | Location |
|---|---|
| New `LodPlugin` (plan / collect / unload / validate), registered after `StreamingPlugin` | new `crates/engine/src/lod.rs`; `crates/engine/src/app.rs:134` |
| `DatabaseRequest::LoadLod { generation, worldspace_id, kind, level, block_x, block_y }` and a `LodBlockPayload` (paths, bounds, tree instances) handled by the same worker | `crates/engine/src/world/database.rs:54`, `:202` |
| Config: `lod_enabled`, `lod_bands`, `tree_distance`, `lod_depth_offset`, `lod_unload_scale`, plus CLI (`--lod`, `--lod-distances`, `--no-lod`) | `crates/engine/src/config.rs:5`, `:94-115` |
| New component `LodBlockRoot { kind, level, anchor: IVec2 }` **and** widening the rebase query to `Or<(With<ExteriorCellGrid>, With<LodBlockRoot>)>` so LOD blocks follow the floating origin | `crates/engine/src/world/components.rs:104`; `crates/engine/src/streaming.rs:1611` |
| Reuse `cell_translation`'s convention for block placement (anchor × `CELL_SIZE`, minus the render origin) | `crates/engine/src/streaming.rs:1258` |
| LOD materials: plain `StandardMaterial` — terrain LOD = diffuse + normal KTX2, `cull_mode: None`, high roughness, lit; objects = atlas + vertex colour; trees = atlas with `AlphaMode::Mask(0.5)`, `double_sided`, `unlit` | `crates/engine/src/render.rs` (no new WGSL) |
| No renderer change for batching: LOD blocks are ordinary `Mesh3d`/scene entities, so Bevy 0.19 GPU preprocessing batches identical billboard meshes and draws blocks indirectly; HZB occlusion applies as for any mesh | `crates/engine/src/render.rs:30` |
| New metrics `lod_blocks_resident/loading/failed/culled`, `lod_tree_instances`, `lod_max_commit_micros`, and an invariant validator mirroring `validate_streaming_lifecycle` | `crates/engine/src/streaming.rs:73`, `:1646` |

**Why no new shader:** the 6-layer `TerrainExtension` exists for full-detail terrain splatting; distant terrain LOD needs one baked diffuse (plus `_n`) and no splat weights, which `StandardMaterial` already provides. The only thing a shader would buy is SSE's distance-dependent lowering (below), which is deferred behind a measurement.

### 6.4 Transitions

> **Updated 2026-09-24, after the first real-data renders of step 3.** Lowering alone, mechanism (a)
> below, was not enough, and the plain `StandardMaterial` of 6.3 was wrong for terrain blocks:
>
> - **LOD is clipped under full-detail terrain.** A coarse block spans a valley as a chord that rises
>   above the real ground. Lowering by 32-96 units still let blocks cut through nearby terrain and
>   water: a black river at Riverwood, a grey slab across the Guardian Stones. Blocks now draw with
>   `LodTerrainMaterial`, an extension that discards fragments over every cell whose full-detail
>   terrain is visible, in the main pass and in the depth prepass (`ClipMask`, a 32 x 32-cell bit
>   window around the camera). This is Skyrim's own rule: LOD is hidden under loaded cells. The
>   lowering stays, for level-over-level overlap. Object blocks (step 4a) use the same clip.
> - **Terrain blocks shade from a model-space normal map.** The converted `.btr` meshes have no
>   vertex normals; their `_n` texture holds model-space normals, swizzled as NifSkope's
>   `sk_msn.frag` reads them (`.rbg`). In render space the texel is `(r, g, -b)`. Read as a
>   tangent-space map, it lit far peaks near-black.
>
> Commit `e75704b` on `phase2/lod-engine-terrain`; before and after in
> `<workspace>/shots-lod/cmp-clip2.jpg` (local).


- **Z-fighting with full-detail cells.** SSE solves this by lowering distant LOD in a distance-dependent shader. Two mechanisms here, in order: **(a)** bake a small constant downward offset into the converted LOD terrain meshes (`lod_depth_offset`, start at 32 units ≈ 0.8 % of a cell) so full-detail terrain always wins the depth test where they overlap; **(b)** if the visual review still finds fighting or a visible step at the ring boundary, add a `TerrainLodMaterial` with `Material::depth_bias` or a vertex offset proportional to view distance, matching SSE more closely. Mechanism (a) costs nothing beyond a constant and is measurable in the `rural` scenario screenshots.
- **Popping.** LOD levels switch at the band distances and do not blend — this is vanilla behaviour, and the nested-residency rule means a switch is always a *refinement*, never a hole. If review rejects the hard switch, the cheapest fix is an alpha fade over the last ~10 % of a band on the block material; that is a per-band material variant, not a systemic change.
- **Seams against full-detail cells.** The full-detail terrain is generated from `LAND`; the LOD terrain is the shipped precomputed mesh, so heights differ at the boundary. Mitigations, in order: keep level 4 as the first band (finest available), skip fully covered blocks, lower the LOD (6.4a) so the full-detail surface wins where they meet, and rely on the existing fog for the residual step. The existing acceptance checkpoint "rural: no terrain seams" is the gate; a `distant-lod` checkpoint must be added for the new scenario.
- **Tree LOD staleness.** Billboards are precomputed from the vanilla tree references; a mod that adds, removes or moves trees changes only the full-detail view. This matches Skyrim's own behaviour and is the accepted limitation of option A.

### 6.5 Acceptance impact

- The Phase 2 gates are unchanged in kind but the *workload* changes, so: add a `distant-lod` scenario to the campaign's expected scenarios (`scripts/phase2-acceptance.ps1:350-351`), add its fixture-driven robustness test to the required list (`:225-241`), capture its screenshot, extend the visual-review checkpoint set, and refresh the baseline (`-UpdateBaseline`) on target hardware in the same PR that enables LOD by default.
- LOD failures must be **counted**, not silently absorbed: a missing `.glb` should increment `lod_asset_failures` and be reported like `asset_load_failures`, otherwise "zero streaming failures" stops meaning anything.
- Until the whole chain lands, keep LOD behind `--lod` (default off) so existing acceptance numbers stay comparable — the same discipline already used for development-only visual features.

## 7. Open questions, each with the measurement that settles it

1. **Does the vendored reader plus `open_nif_resilient` consume a `.btr`/`.bto` unmodified?** Run `cargo run -p converter --bin nif-audit -- <one .btr> <one .bto>` (or `MeshConverter::inspect_nif`) on a level-4 `.btr` and a level-16 `.bto` from the local asset tree. Expect `fallback_blocks["BSMultiBoundNode"] > 0` and `scene_node_count == 0` (children hang under a block that is currently `Unhandled`). This settles whether the reader work is a dispatcher entry or a new parser.
2. **What shape data do the LOD shapes actually carry?** Inspect the converted GLB attribute lists for a level-4 vs level-16 `.btr`, and for a `.bto`: vertex colours (level 4 terrain and object LOD are expected to have them), UV sets, and whether the object atlas `.ktx2` is referenced with the source path. Settles the material policy in 6.3.
3. **What is the `.btt` record layout?** Write the parser against the measured sample (leading `9, 24, 5`, then Creation-unit positions), then assert that every instance within a block matches a TREE/STAT reference position in `skyrim_world.db` for that block within tolerance. A high match rate both validates the parser and gives the tree-tier fixtures a ground truth. Measurement needed before the tree tier (PR 4) can be called done.
4. **Are the `.lst` field names right?** Sample the tree atlas at each entry's `(u0,v0,u1,v1)` rectangle and require non-empty alpha and a plausible aspect ratio against `size_x`/`size_y`.
5. **What does the third `.lod` field mean?** Tamriel's shipped blocks cover 192 cells while the field is 256. Compare the field against the enumerated block coordinates per worldspace and per level; until then, drive residency from file/table availability, never from the extents.
6. **What does a resident LOD set cost?** After PR 3 lands: count resident blocks and measure peak memory and frame time at the Medium and Ultra band settings in the benchmark and rural scenarios. Settles the default band distances and whether level 32 should be resident or horizon-only.
7. **Does the depth offset (6.4a) suffice?** Compare `rural` screenshots with LOD on at 32 units versus 0 units and versus a depth-biased material. Settles whether mechanism (b) is needed.
8. **Does the runtime asset audit accept LOD GLBs?** Run `runtime-asset-audit` over a converted set that includes LOD meshes; watch for empty/near-empty blocks (the 14 KB `.btr` samples) tripping the bounds check at `crates/converter/src/bin/runtime-asset-audit.rs:134`.

## 8. PR plan

Each step is testable without game data; the fixture writers in `crates/dummy-content` already emit SSE-version NIFs (`:16`, `:42`) and can grow LOD-shaped emitters.

1. **PR 1 — converter: read LOD mesh containers.** Teach the vendored reader `BSMultiBoundNode` as a node (children + bound reference) and its bound companions as skippable leaves; add `.btr`/`.bto` to the pipeline extension filter and asset kinds (`crates/converter/src/pipeline.rs:963`, `:417`); convert them to GLB under `meshes/terrain/...` with vertex colours and atlas/diffuse/normal URIs; add `dummy-content` emitters for a `.btr`-shaped and a `.bto`-shaped NIF with crossing shape hierarchy; unit tests: fixture converts, unsupported-root regression test, and a level-4 vertex-colour assertion. Also run a real `.btr`/`.bto` through `nif-audit` as a documented manual check.
2. **PR 2 — converter: LOD database contract and tree billboards.** Add the schema above, read `lodsettings/<ws>.lod` into `lod_grid`, populate `lod_block` for terrain and object blocks and `lod_tree_type`/`lod_tree_instance` from `.lst`/`.btt`, generate the crossed-quad billboard GLBs, bump `WORLD_DATABASE_SCHEMA_VERSION` to 4 and the manifest schema; update the engine's stale-asset rejection and integration report expectations. Tests: fixture ESM + fixture LOD tree, `.lst`/`.btt` parsers over synthetic bytes, migration/rejection tests (the engine already has the version-mismatch pattern at `crates/engine/src/world/database.rs:186`).
3. **PR 3 — engine: LOD streaming tier (terrain).** `LodPlugin`, DB request/payload, band selection with hysteresis, nested residency, skip-fully-covered rule, block placement and rebasing, `StandardMaterial` LOD terrain, depth offset, metrics and invariant validator, `--lod`/`--lod-distances`, and a `--lod-fixture` scenario that builds a synthetic LOD asset tree. No game data: the fixture writes a tiny asset set with the same layout. Gates: existing streaming tests unchanged; new tests for band selection (pure function of camera cell + block table), rebase locality, and unload hysteresis.
4. **PR 4 — engine: object blocks and tree billboards.** Object LOD scenes, tree instance spawn from the block payload (batched by Bevy preprocessing), per-block readiness, failure metrics, unload rules. Extends the fixture and adds a density/overdraw check (billboards are known overdraw).
5. **PR 5 — acceptance, docs, default on.** Add the `distant-lod` scenario and its robustness test to `scripts/phase2-acceptance.ps1`, extend the visual-review checkpoints, refresh the baseline, update `docs/roadmap/02-core-engine.md` (deliverable 2.4) and the acceptance doc, and flip the default to on.
6. **PR 6 (follow-up, not required for the horizon) — dirty-block regeneration.** Use per-plugin record provenance to find cells a non-vanilla plugin overrides, regenerate object LOD for their blocks from STAT MNAM LOD models, and regenerate terrain block *geometry* from `LAND` while reusing the shipped block texture (silhouette correct, shading approximate). This is where option B is paid for, only where it is needed.

## 9. Web claims: relied on, corroborated, corrected

Corroborated from the repository or the installation:

- `.btr`/`.bto` are NIFs, SSE Bethesda version 100, version 20.2.0.7 / user version 12 — **confirmed** byte for byte (header dump), including that the converter's own header parser already matches.
- Terrain LOD named `<ws>.<level>.<cell>.<cell>` with `level` ∈ {4,8,16,32}, the last two being the block's SW cell coordinates — **confirmed**, plus the new detail that the coordinates are absolute cell coordinates in steps of the level, and that Terrain levels 4/8/16/32 are all shipped for Tamriel (2304/576/144/36).
- Object LOD has levels 4/8/16 only — **confirmed** (517/152/48 for Tamriel; no level 32).
- Tree LOD is not meshes: `.lst` (types + atlas UV rectangles) + `.btt` (instances) + one `<ws>TreeLOD.dds` atlas per worldspace — **confirmed**; a `.lst` entry is 32 bytes and the type count is a leading u32.
- Texture paths/atlases (`Textures\Terrain\<ws>\...`, `<ws>.Objects.dds`) — **confirmed**, and extended: each terrain block also ships a `_n` normal map, the block textures are 256×256 uncompressed with 8 mips, and all of it is already converted to KTX2.
- SSE's terrain LOD is *lowered* at distance to avoid z-fighting, "baked into a shader" — **relied on** for the transition design (mechanism a/b in 6.4), but the shader itself could not be inspected here; treated as a behaviour to reproduce, not a mechanism to copy.
- Distances come from `[TerrainManager]` — **confirmed and corrected**: the shipped SSE presets differ from the Classic values in circulation (`Medium` is 20000/32000/100000 with `fSplitDistanceMult` 1.1 and `fTreeLoadDistance` 75000; `High` is 35000/70000/250000/1.5; `Ultra` raises level 0 to 60000 and level 1 to 90000).
- The LOD meshes live in the mesh BSAs and the textures in the texture BSAs — **confirmed and localised**: all LOD meshes are in `Skyrim - Meshes1.bsa`; `Skyrim - Meshes0.bsa` holds none.
- STAT flags `0x8000` Has Distant LOD / `0x20000` Uses High-Detail LOD Texture and MNAM LOD paths — **partially confirmed**: the flag bits are preserved in the database (`statics.flags` stores the raw u32 record flags) but **MNAM is not parsed or stored**, which is exactly why option B's object half needs converter work.
- "No open-source Skyrim engine reimplementation that consumes `.btr`/`.bto` was found" — consistent with what is in this repository and its vendored reader, which has stubs for the LOD node blocks.

Not relied on (unverified here, and not needed): the exact `.btt` field order (research found none either), `uGridsToLoad`'s default (not present in the shipped INIs; the engine's `stream_radius` is its equivalent), occlusion data (TVDT), and map-only LOD (`uLockedObjectMapLOD`).

Where the research and the install disagreed, the install wins: the SSE `Medium` preset is **not** the Classic one, and vanilla *does* ship terrain LOD level 32 (36 blocks for Tamriel) even though object LOD stops at 16.

---

---

_Phase 2 track design, written by DeepSeek research-513 (2026-09-23) and spot-checked by Claude against the shipped files: `tamriel.lod` bytes (-96,-96, 256, 4, 32), the `.btr` header (20.2.0.7, user version 12, Bethesda version 100), and the Tamriel counts (3060 `.btr` = 2304+576+144+36; 717 `.bto` = 517+152+48). Not yet proposed upstream (the send hold applies)._
