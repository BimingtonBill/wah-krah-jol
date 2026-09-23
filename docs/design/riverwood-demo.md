# Riverwood demo: the data behind a second walkable demo

**Status:** research, 2026-09-23 (research-055). Read from the converted database
(`$OPENSKYRIM_CONVERTED_DIR/skyrim_world.db`, converter schema 17 / DB schema 5) and from
`Skyrim.esm`'s `LAND` records. Nothing was rendered: every number below is a data measurement,
and the poses are proposals to be refined by rendering, exactly as `research-018` did for Alftand
(`docs/research/reference-shot-poses.md`).

**Tool:** `tools/research/riverwood_demo.py` (new; read-only). Subcommands used here: `survey`,
`interiors`, `doorbases`, `buildings`, `roads`, `extras`, `gaps`, `gridcount`, `invalid`, `snow`,
`modelsnear`, `glb`, `river`, `route`, `ground`, `vhgt`, `look`, `map`, `around`, `cellstats`,
`spaces`, `notes`, `assets`. Positions are Creation units, the space `DemoStart`,
`--start-position` and the shots file use.

**Conventions confirmed for this doc**

- Creation heading: `yaw = atan2(dx, dy)`, 0 = north (+Y), 90 = east (+X), clockwise seen from
  above — the `XTEL`/`GetAngle Z` convention the engine uses for arrivals
  (`docs/design/reference-shots.md`). Pitch is Skyrim's player X angle, positive down.
- What the engine streams is decided by **position**, not by `cell_id`: an exterior cell's
  references are selected from the `exterior_spatial` R-tree by the 4096-unit grid box
  (`crates/engine/src/streaming.rs:628-647`). Riverwood's exterior load doors are stored in
  Tamriel's persistent cell `00000D74` (grid 0,0, 17,122 references), and they load correctly
  because their *positions* are in the village's grid box. No engine change is needed for this.
- Terrain: `LAND`/`VHGT`, decoded as `8 * (header_float + accumulated_delta_bytes)`
  (`crates/converter/src/esm/cell_cache.rs:111`). Checked against the seven village door
  thresholds: the decoder lands within 5-36 units of every door's own z, and the alternative
  `header_float + 8*delta` reading is 170-190 units out at the same points. The decoded heights
  are therefore the ground the engine draws.
- Water: the Riverwood cells' `XCLW` is **-250** with water type `000E717C` "RiverWaterFlowNE"
  (`crates/engine/src/streaming.rs:1133-1140` draws a 4096-unit plane at that height in every cell
  whose own samples dip below it).
- Player speed 150 u/s walking, 350 running (`crates/engine/src/player.rs:76-78`); 1 unit ≈ 1.43 cm.

---

## 1. Where Riverwood is

Tamriel (worldspace 60, `0x3C`). The village occupies four exterior cells plus its edges; all are
**exterior** cells with a grid coordinate and an `EDID` (no display name — exteriors have none):

| cell | grid | `EDID` | references (by position) |
|---|---|---|---|
| `00009732` | (4, -12) | `Riverwood` | 319 |
| `00009731` | (5, -12) | `Riverwood04` | 441 |
| `00009712` | (5, -11) | `Riverwood02` | 307 |
| `00009713` | (4, -11) | `Riverwood01` | 130 |
| `00009730` | (6, -12) | `Riverwood05` | 58 |
| `00009711` | (6, -11) | `Riverwood03` | 81 |
| `00009733` | (3, -12) | `RiverwoodEdge01` | 201 |
| `00009752` | (3, -13) | `Riverwood06` | 117 |
| `000096F3` | (5, -10) | `RiverwoodBridge` | 199 |
| `00000D74` | (0, 0) | *(Tamriel persistent cell; holds all 570 exterior load doors)* | 157 in this area |

Grid range: **x 3..6, y -13..-10** (`RiverwoodEdge01`/`Riverwood06` west, `Riverwood03`/`Riverwood05`
east, `RiverwoodBridge` north). Cell size 4096; the village's built-up part is x 20,300..24,000,
y -48,200..-44,300.

**Village centre:** ≈ **(22,100, -46,130)**, mean of the six house-door positions. The walkable
band through the village is at z ≈ **-95 to -175**; the river surface is at -250 and the channel
bed at -320..-348.

**Landmarks** (shell reference → door reference):

| landmark | model | reference position | notes |
|---|---|---|---|
| Riverwood Trader | `Farmhouse02.NIF` (+`Farmhouse02Walkway`) | `00012DF6` (22,176, -45,867) | door `0001341F` (22,022, -45,713); a second, upper door `00070E69` at (21,995, -45,685) z 105 |
| Sleeping Giant Inn | `Farmhouse\Inn01.nif` | `0002D763` (23,864, -44,503), 1944×1224 | door `00013424` (23,320, -44,530) |
| Alvor and Sigrid's House (with the smithy) | `Farmhouse\Smith01.nif` | `00018FE2` (21,002, -44,734), 1672×1000 | door `00013420` (21,365, -44,736); `SignRiverwoodBlacksmith01` at (20,595, -45,534); `FXSmokeRiverwoodSmith01` at (20,542, -45,376) |
| Hod and Gerdur's House | `Farmhouse04.nif` | `00034EC1` (23,564, -47,612) | door `00013423` (23,556, -47,255) |
| Sven's House | `Farmhouse06.nif` | `0001C5AA` (20,906, -46,558) | door `0001CBB0` (20,670, -46,394) |
| Faendal's House | `Farmhouse05.nif` | `00019053` (21,640, -48,385) | door `00012DFE` (21,663, -48,130) |
| **The mill** | `Farmhouse\Lumbermill01.nif` | `0004F743` (19,277, -44,311) z 91 | the building's model reaches 1152 units below its origin, so the floor at 91 is ~227 above the bank (-136) |
| **The water wheel** | `Farmhouse\Lumbermill01WaterWheel01.nif` | `000A1FF8` (19,650, -43,941) z 91 | model offset +135..+639 in x: the wheel's rim spans x 19,785..20,289 and dips to z -266, i.e. **16 units below the water surface**. `FXMistMillWheel01` (20,022, -44,225), `FXRapids` (20,121, -44,235, -250), `FXSplashHeavyWater` (19,868, -43,368, -246), six `FXSawdustOnThefloor*` on the deck |
| **The river** | `XCLW` plane at -250 + water type `000E717C` | — | the channel at y -44,311 is x 19,600..21,100 (1,500 units wide), running south-west to north-east: x 19,250..19,900 at y -45,500, x 22,300..22,900 at y -42,000, x 24,100..25,750 at y -40,000 |
| **The bridge** | `Landscape\Bridges\Bridge01.nif` | `00022469` (24,283, -39,517) in `RiverwoodBridge` | the road bridge 4,400 units north-east of the village, over the same river; the only bridge model within 8,000 units |
| The crossroads and signs | `RoadSignPost` + 7 `RoadSign*` | `0003AFAC` (18,501, -46,744) | the Helgen/Riverwood road junction south-west of the village |
| **The Guardian Stones** | `Clutter\PowerShrines\PowerShrine01\` `WizardStone/WarriorStone/ThiefStone` | `000E7BD6/D/DD`, ~(2,500, -59,300), grid (0, -15) | not a cell: three exterior references at z 1,428-1,435 on a bluff, **≈ 22,000 units (315 m) south-west of the village**, across the river |
| Bleak Falls Barrow | `Dungeons\Nordic\Doors\Animated\LargeDoor\Ruins_LargeDoor01.nif` | `00015DA5` (-3,043, -43,338, 6,081) | the barrow's exterior door, ≈ 22,400 units west and **6,200 units above** the village; the interior `BleakFallsBarrow01` (`000371DE`) is the largest in the region (2,512 refs, 71 light refs) |

So the Guardian Stones and Bleak Falls Barrow are **not** cells of their own and not part of a
village stroll; either is a separate trip (315 m and a 6,200-unit climb respectively).

---

## 2. The interiors reachable from the village

All seven entries below are reached through a load door standing in the village's grid box, and
every one has a matching door inside leading back out. Positions are the **door reference** (where
the player stands, Tamriel) and the **`XTEL` arrival** the door writes. `rot_z` is the arrival
heading in radians; "arrival" for the outward door is the position in Tamriel the player lands on.

| interior | cell | refs | lights | outward door ref → arrival in Tamriel | inward arrival (cell-local) |
|---|---|---|---|---|---|
| Riverwood Sleeping Giant Inn | `000133C6` | 484 | 8 | `00013424` (23,320, -44,530) → (23,259.77, -44,476.16, -8.25) rot_z -0.8418 | (-250.34, -360.40, 0.00) rot_z -0.0084 |
| Riverwood Riverwood Trader | `000133C9` | 232 | 5 | `0001341F` (22,022, -45,713) → (21,949.96, -45,679.83, -118.27) rot_z -0.7834 | (-190.75, -472.09, 0.00) rot_z 0.2416 |
| " (upper door) | " | " | " | `00070E69` (21,995, -45,685, **z 105**) → (21,953.83, -45,643.79, 105.02) rot_z -0.8188 | (-197.00, -403.19, 240.77) rot_z 0.0416 |
| Riverwood Alvors House | `000133C8` | 230 | 6 | `00013420` (21,365, -44,736) → (21,405.76, -44,778.38, -82.26) rot_z 2.3750 | (-512.82, -205.79, 240.00) rot_z -0.0334 |
| Riverwood Gerdurs House | `000133C7` | 218 | 6 | `00013423` (23,556, -47,255) → (23,508.91, -47,196.68, -5.20) rot_z -0.7668 | (-505.79, -176.74, 0.00) rot_z 0.0416 |
| Riverwood Faendals House | `000133CA` | 157 | 5 | `00012DFE` (21,663, -48,130) → (21,666.54, -48,054.64, 35.47) rot_z 0.0832 | (-513.57, -196.98, -16.00) rot_z 0.0166 |
| Riverwood Svens House | `0001CB84` | 145 | 6 | `0001CBB0` (20,670, -46,394) → (20,644.04, -46,318.41, -121.86) rot_z -0.4834 | (-510.17, -198.88, -16.00) rot_z -0.0334 |
| Embershard Mine | `000B6BE6` | 1,141 | 39 | `000B6F29` (16,375, -54,616, z 1,350) → (16,197.99, -53,979.91, 1,348.92) rot_z -0.2788; main door `000B6CCE` (8,889, -58,186) → (8,849.19, -58,119.75, 1,446.46) rot_z -0.6800 | (2,472.6, 1,303.8, 9,245.1); (-5,861.3, 1,561.1, 8,820.8) |

**Auto-load:** none of the village doors are auto-load. All seven house doors use the same base,
`00029CB0` "FarmhouseLDoor01" (`Architecture\Farmhouse\FarmhouseLDoor01.nif`), whose `Open` and
`Close` clips are both in the converted glb — they open with `E`, as the engine's `--walk` already
does. The two **Embershard Mine** doors are the auto-load kind (`AutoLoadMarker01.nif`, and the
main mine door uses `MineDoorLoad01.nif`); the second one, at (16,375, -54,616), is an **invisible
marker on the hillside** north-west of the village that drops the player into the mine.

**Interior sizes** (for camera poses): the inn is the big one (x -1,542..1,549, y -936..1,447,
z -256..1,362); the five houses are single-storey-plus-attic shells 700-1,500 units across.

**Interior lighting** (`space_lighting`, schema 17): all six village interiors resolve to the same
lighting template `000A1196` — ambient `0x30302D`, directional `0x504641`, fog `0x2E341F`,
fog_near 100, fog_far 4,000, `has_sky = 0`. Embershard Mine uses template `000E7C5E` (ambient
`0x1F1F1F`, fog `0x477787`, fog_near 750). The Tamriel worldspace row is climate `00000812`,
weather `00012F89`, ambient `0xDCDCCB`, fog `0x836C12`, fog_near 0, fog_far 100,000, `has_sky = 1`,
sun_illuminance 0.7147. Exterior *cells* have no `space_lighting` row of their own — the worldspace
row is what a Riverwood demo needs.

---

## 3. The route

Ten legs, **8,096 units (≈ 116 m)**, four interior visits, all of it on ground above the river's
-250 water plane. Verified by sampling the terrain every 100 units along every leg
(`riverwood_demo.py route`): **0 of 77 sampled steps is below the water level**, and the ground
runs -95 to -207. Waypoint positions and headings:

| # | position (x, y) | ground z | leg to next: heading / length | beat |
|---|---|---|---|---|
| wp0 | 19,600, -46,650 | -153 | 76.5° / 1,100 (yaw for `DemoStart`) | **Start** on the Helgen road south-west of the village; `RoadChunkM01` `0002C699` is at (19,600, -46,624), the road sign at (18,501, -46,744) is behind |
| wp1 | 20,670, -46,394 | -149 | 61.6° / 830 | **Sven's House** — door `0001CBB0` (interior `0001CB84`) |
| wp2 | 21,400, -46,000 | -132 | 65.2° / 685 | the village street, Trader ahead |
| wp3 | 22,022, -45,713 | -121 | 355.5° / 767 | **Riverwood Trader** — door `0001341F` (interior `000133C9`) |
| wp4 | 21,962, -44,948 | -165 | 289.6° / 634 | the street's north end, Alvor's ahead |
| wp5 | 21,365, -44,736 | -158 | 4.6° / 437 | **Alvor's house and smithy** — door `00013420` (interior `000133C8`) |
| wp6 | 21,400, -44,300 | -167 | 82.9° / 806 | the north lane past the smithy |
| wp7 | 22,200, -44,200 | -144 | 106.4° / 1,168 | the lane east to the inn |
| wp8 | 23,320, -44,530 | -95 | 257.7° / 328 | **Sleeping Giant Inn** — door `00013424` (interior `000133C6`, 484 refs, 8 lights) |
| wp9 | 23,000, -44,600 | -123 | 296.6° / 1,342 | turn back west/north-west |
| wp10 | 21,800, -44,000 | -207 | — | **Finale**: the river bank at the water's edge. Look west at yaw **268.4°**, pitch -5.7° to put the mill's water wheel (centre ≈ 20,037, -44,050, hub z 91, 1,763 units away) in frame, the mill behind it and the rapids in front |

Walking the whole route takes ≈ 54 s at 150 u/s, ≈ 23 s running, plus the four door visits.

`DemoStart` entry (the `--demo` name and the exact shape of `docs/design/blackreach-demo.md`'s
sibling entry in `crates/engine/src/config.rs:334-344`):

```rust
// On the Helgen road south-west of Riverwood, looking up the street at Sven's house and the
// village. Creation foot position (the engine adds START_EYE_HEIGHT) and door-heading yaw.
"riverwood" => Some(Self {
    worldspace_id: 0x3c,
    position: [19600.0, -46650.0, -153.0],
    yaw: 1.336_0,
}),
```

`--start-position 19600 -46650 -153 --start-yaw 1.3360` runs the same start with no code change;
`grid_of` puts the start in grid (4, -12) `Riverwood` (`crates/engine/src/config.rs:358-360`).

**Where the mill is, and why the route stops short of it.** The mill is on the far bank: at
y -44,311 the river spans x 19,600..21,100 and the mill's dry tongue (x 18,400..20,000) is separated
from the village side by water on the south-east (x 19,250..19,900 at y -45,500), the west
(x < 18,400) and the north (the wide water at y -41,000..-42,000). Every on-foot approach I sampled
crosses water: `(20,500,-45,500) → (19,900,-45,800) → (19,500,-46,300) → (18,900,-45,500)` is wet
for 6 of 10 steps and dips to -312; the northern approach from the inn through (22,000, -43,000) is
wet at -312. So the route ends on the bank looking across (~1,760 units, the wheel is 718 units
across, clearly visible) rather than wading.

If a "walk to the mill" beat is wanted, the engine makes it *physically possible* today because
there is no water collision or swimming: the player walks on the river bed with the water plane at
eye level cutting the view. That is worth one deliberate test, not a default part of the route.

---

## 4. What will look wrong

Counted over the 5×5 grid box the engine actually keeps resident around the village
(x 12,288..32,768, y -57,344..-40,960; grids 3..7 × -14..-10): **1,950 references**, 2 light refs,
8 load doors, 14 NPC refs.

1. **No NPCs.** 12 NPC references stand in the four route cells and 14 in the stream box
   (`GuardRiverwoodImperial01-03`, `GuardRiverwoodSonsOfSkyrim01-03` at the crossroads
   (18,400..18,650, -47,100..-47,300), `EncCow` at (22,652, -46,740), two `EncChicken`,
   `EncHorseSaddledBrown`). The converter has the `npcs` table (6,461 `NPC_` records) but nothing
   spawns or animates an actor; Riverwood's six guards, the innkeeper and the crowd of townsfolk
   will simply be absent. This is the single largest difference from the game's own screenshots.
2. **No grass.** The `GRAS` records are in the database as raw records (33 of them) and no grass
   reference is placed anywhere: the village's ground cover — the thing that makes Riverwood's
   streets read as a village rather than a car park — will be missing entirely. 1,021 of the
   1,950 references are `Landscape\Plants\*` scattered props (clover ×111, thickets ×72, sword fern
   ×55+41+36+32+27+23+17, pine shrub ×44+19+13, thistle ×25, dead shrub ×21, vine maple ×21,
   potato/cabbage/leek/mountain-flower planters), so the ground will look sparse but not bare.
3. **Trees render, but statically.** 189 `Landscape\Trees\*` references (38 `TreePineForest03`, 28
   `04`, 25 `05`+`02`, 25 `02`, 16 `01`, stumps and cut logs) — all converted, all with valid
   bounds, and `TreePineForest03.glb` carries the branch bones (`TrunkBone`, `BranchBoneLow01..`)
   but **0 animations**: the converter does not export the sway, so the forest will be perfectly
   still.
4. **The mill's water wheel will not turn, and its mist will not show.** The wheel's glb has one
   animation, named **`Idle`** (1 channel), on a node chain `Lumbermill01WaterWheel01 → SawWaterWheel03
   → L1_SawWaterWheelHub` plus a `PArray07-Emitter`; the engine's only clip playback is the door
   machine, which looks clips up by the names `Open`/`Close` on door references
   (`crates/engine/src/door_animation.rs:300-330`), so nothing drives `Idle`. Worse, the FX cards around the mill and the street are
   **empty scenes**: `FXMistMillWheel01.glb`, `FXMistStreet01.glb` (12 refs),
   `FXSmokeRiverwoodSmith01.glb` and `FXMotesForest01.glb` all have 0 nodes and 0 meshes, so the
   wheel's spray, the street mist, the smithy's chimney smoke and the forest motes never draw.
   `MillLogPile.glb` does carry `LoadDust`/`PileDust` — also unplayed.
5. **The river will be flat, still and too wide at the cell seams.** The plane is drawn per whole
   cell at -250 with water type `000E717C` "RiverWaterFlowNE", whose `waters.flow_normal_path` is
   NULL in the converted database (the Dragonborn `01001232` "RiverWaterFlow" has
   `water/riverflow.dds`, so the column works and this record simply resolves to none). No flow
   map, no current, no shoreline clipping — the plane's cell-aligned edges are visible wherever a
   neighbouring cell has no water or no dip (cells (6,-11) `Riverwood03` and (6,-12) `Riverwood05`
   have **no water plane at all**, and (5,-12) `Riverwood04` has one with 0 of 1,089 samples below
   it). Also `terrain_reaches_water` (`crates/engine/src/streaming.rs:1046-1048`) is a
   per-cell any-sample test, so the plane is full-cell: 12 of the 13 water-bearing cells in this
   area have a terrain sample within 2 units of the water level (the thirteenth within 34), i.e.
   z-fighting shoreline strips.
6. **The village is unlit after dark.** Only **2 `LIGH` references** exist in the whole village
   (one each in grids (5,-12) and (4,-12)); the inn's 8, the Trader's 5 and the four houses'
   ~23 interior light references are all inside. The engine's point lights are the only light
   source besides the sun, and the demo is a daylight scene, so this mostly matters if the launch
   is at a fixed time of day.
7. **Snow: nothing will be white, and the data will make a naive shader cover the wrong things.**
   814 of the 1,847 references that have a `statics` row carry a `DNAM` max angle (90° on almost
   all), but only **70 of them carry a real `material_object`** (the `*Snow` bases:
   `RockL05Snow`, `RockCliff04SnowLight`, `MountainPeak01_LightSN` … with MATOs `00025129`,
   `0002871B`, `00045AB3`, `0004CCBE`, `0004CCC9`) and those sit on the box's outer edges. The
   six house shells themselves are the **plain** bases with no MATO — the snowy bases
   (`Farmhouse02Snow` `00028794`, `0006F14C`…) share the same NIF but are used elsewhere — so
   Riverwood's roofs stay bare, as in the game. **Contract hazard:** the other 744 references have
   `material_object = 0`, not NULL, although the column is documented "NULL when the static has
   none" (`crates/converter/src/esm/exporter.rs:71`); an engine filter keyed on `material_object`
   must test `> 0` or the snow spec's "per-reference material assignment keyed on
   `material_object`" (`docs/research/visual-gaps-spec.md`, gap 3) will snow-plaster fences, ivy,
   firewood, road chunks and markers across the whole village.
8. **No LOD, and the ring decides what the skyline looks like.** Every grid in the box has a
   `land` row and **no `lod` row** (0 of the 35 grids in and around the box), so distant terrain is
   the terrain-only ring only: at the default `--terrain-radius 8` (32,768 units) the surrounding
   slopes are drawn but the peaks are not — the un-named grid (7,-13) east of `Riverwood05` reaches
   11,224 units and the Throat of the World is further east still. The bridge at (24,283, -39,517)
   is 1.7 cells from the village centre, well inside the ring.

Two things that *will* work, worth saying because the Alftand demo could not show them: the seven
village doors all have `Open`/`Close` clips in their glb, so the door state machine and the swing
animation have real data here; and the water plane is real geometry the portal/lighting work has
never been asked to handle in daylight.

---

## 5. Numbers for the lead

**Per grid, in the stream box** (references by position, then lights, load doors, NPC refs, and
references whose model fails the bounds gate):

| grid | `EDID` | refs | lights | doors | NPCs | `bounds_valid = 0` |
|---|---|---|---|---|---|---|
| (4, -12) | `Riverwood` | 319 | 1 | 0 | 7 | 13 |
| (5, -12) | `Riverwood04` | 441 | 1 | 5 | 3 | 19 |
| (5, -11) | `Riverwood02` | 307 | 0 | 2 | 2 | 24 |
| (4, -11) | `Riverwood01` | 130 | 0 | 0 | 0 | 12 |
| (6, -11) | `Riverwood03` | 81 | 0 | 0 | 0 | 1 |
| (6, -12) | `Riverwood05` | 58 | 0 | 0 | 0 | 3 |
| (3, -12) | `RiverwoodEdge01` | 201 | 0 | 0 | 0 | 2 |
| (3, -13) | `Riverwood06` | 117 | 0 | 0 | 0 | 1 |
| (5, -10) | `RiverwoodBridge` | 199 | 0 | 0 | 1 | 8 |
| **route's four cells** (4,-12)(5,-12)(5,-11)(4,-11) | | **1,197** | **2** | **7** | **12** | **68** |
| **whole stream box** (grids 3..7 × -14..-10) | | **1,950** | **2** | **8** | **14** | **79** |

- **Door links:** 8 in the box (the 7 village doors + Embershard's back-door marker 5,000 units
  west at (16,375, -54,616)). Each is a two-way pair in `door_links`; the seven village interior
  cells hold exactly one door each (the Trader holds two, its ground and balcony doors).
- **Interiors:** 1,466 references across the six house interiors (484 + 232 + 230 + 218 + 157 +
  145), 36 light references, 7 doors.
- **Models that fail validation today: none of the buildings, trees, rocks or props.** The 79
  flagged references are markers and FX cards only: `PlaneMarker` (17, no model),
  `FXMistStreet01` (12), `DragonPerchRockL02` (9), `WallLeanMarker` (5), `RailLeanMarker` (5),
  `DragonPerchTower` (3), `FXMotesForest01` (3), `WeaponRackMidACTIVATOR` (3), `DragonMarker`,
  `BucketCarryPourMarker`, `FXSmokeRiverwoodSmith01`, `FXSmokeChimney02`, `FXMistMillWheel01` (1
  each). The four FX ones I opened are **empty glb scenes**, so the gate is right about them.
- **Models present:** all 182 distinct models referenced in the village box have a converted glb
  under `$OPENSKYRIM_CONVERTED_DIR/meshes` (no missing asset).
- **126 references have no `statics` row**: 7 `IDLM`, 6 `MISC`, 5 `ARMO`, 4 `WEAP`, 3 `NPC_`,
  3 `APPA`, 2 `LIGH`, 1 `SOUN`, 1 `ALCH` — items, idle markers and the two exterior lights.
- **Space lighting:** all six interior templates resolve, and the Tamriel worldspace has a
  complete row (see §2). `LTMP`/`CNAM` chains are already resolved in `space_lighting`.

---

## 6. What to build

1. **`DemoStart` entry `"riverwood"`** in `crates/engine/src/config.rs` (the snippet in §3) plus a
   `play-riverwood-demo.cmd` beside the Blackreach launcher, and one line in
   `docs/demo/README.md`. No engine flag is missing: `--demo riverwood --walk` needs only the
   entry. `--start-position/--start-yaw` already reproduces it without a build.
2. **A scripted tour**, if the lead wants the same self-checking run as `--demo-tour`: the
   waypoints, door refs and per-leg headings in §3 are the input, and the tour's crossing
   acceptance is the same four-door check the Alftand route uses (here: 4 interiors, each opening
   with `E`, arriving and returning).
3. **Reference shots.** The two interiors worth a pose are the inn's common room and the Trader's
   shop floor; the exteriors below are measured eye positions (**ground + 120**, the engine's eye
   height) with yaw/pitch computed from the geometry (`riverwood_demo.py look`). `worldspace_id`
   60 for all; `hfov` 75 as the Alftand set uses; poses are *proposals* — render, compare, refine.

   | name | eye (x, y, z) | yaw | pitch | what it should show |
   |---|---|---|---|---|
   | `RW-01-village-from-the-south` | 18,700, -46,700, -62 | 75.5 | -0.6 | the road from Helgen, the village and its roofs ahead |
   | `RW-02-street-to-the-inn` | 21,400, -46,000, -11 | 52.6 | -0.9 | the street north-east, Trader on the right, inn ahead |
   | `RW-03-trader-front` | 21,962, -44,948, -45 | 175.5 | -2.0 | the Riverwood Trader's front and its sign, looking back south |
   | `RW-04-inn-front` | 23,000, -44,600, -3 | 77.7 | -16.2 | the Sleeping Giant Inn's front, sign, walkway |
   | `RW-05-mill-wheel-across-the-river` | 21,800, -44,000, -87 | 268.4 | -5.7 | the water wheel, the mill and the rapids across the water (1,763 units) |
   | `RW-06-river-to-the-bridge` | 23,600, -43,900, 72 | 8.9 | +0.9 | north along the river to the bridge (4,436 units) |
   | `RW-07-village-from-the-south-hill` | 21,663, -48,130, 128 | 8.4 | +2.8 | the whole village from Faendal's doorstep |
   | `RW-08-guardian-stones` | 3,800, -58,300, 1,398 | 230.2 | -1.9 | the three Guardian Stones on their bluff (1,563 units) |
   | `RW-09-inn-common-room` | cell `000133C6`, -250, -360, 120 | 0 | -5 | inside the inn, from its own door's arrival point (refine by rendering) |
   | `RW-10-trader-shop-floor` | cell `000133C9`, -191, -470, 120 | 0 | -5 | inside the Trader from its door's arrival point (refine by rendering) |

   UESP has no Riverwood screenshots in `local/reference/uesp/` yet (the folder holds Alftand and
   Blackreach only). The CC BY-SA files to fetch for these are the place and interior shots of
   `Riverwood`, `Sleeping Giant Inn`, `Riverwood Trader`, `Guardian Stones` and `Bleak Falls
   Barrow`; downloading them is a user decision, as before.
4. **Optional, cheap, high visual value:** play the `Idle` clip on non-door references (the mill
   wheel is the only one in the village; the wheel and its `PArray07` emitter are what make the
   finale read as a working mill). The converter already exports it; only the player is missing.
5. **Optional:** a `--demo riverwood-stones` start at the Guardian Stones
   (worldspace 60, position ≈ 2,480, -59,477, 1,387, yaw 230.2°) if a "Vista" demo is wanted
   without the 315 m walk.

---

## 7. What is verified, inferred and unknown

**Verified against the data (DB queries, `Skyrim.esm` `LAND`/`CELL` bytes, glb JSON chunks):**
every cell id, grid, name and reference count; the six house doors, their interior cells, both
`XTEL` arrivals per door; the base records and the absence of auto-load in the village; the mill,
wheel, wheel bounds and its `Idle` clip; the road chunks; the river's course to ±50 units at the
sampled latitudes; the terrain decoder (cross-checked against seven door thresholds and against
`crates/converter/src/esm/cell_cache.rs:111`); the water level and type; the model counts, trees,
plants, markers, NPCs, lights and bounds failures; the `space_lighting` rows; the empty FX glbs;
the 814/70/744 `material_object` split; the route being dry at every 100-unit step.

**Inferred (not rendered):** that the water plane will read as a river rather than a flat sheet
from the route's viewpoints; that the bank pose at wp10 has a clear line to the wheel (the river
bank and the smithy's bounding box were checked, but the smithy's rotated shape and the
riverbank fences were not); that the four interiors' rooms are big enough for the ground-floor
arrival pose to see the room (their bounding boxes are, but no ray was cast); that the
`DemoStart` yaw puts the street in front of the camera.

**Unknown / not checked:** whether the Guardian Stones' bluff and the Bleak Falls approach are
walkable at all (no terrain sampling was done outside the village box); what UESP's Riverwood
reference screenshots actually frame; whether the engine's `--walk` controller can step over the
village's walkways and fences (only the route's terrain was checked, not the colliders); the
exact `hfov` UESP used, i.e. the first render's framing will need the same fit pass research-018
did.

---

## 8. What the first build and the first renders showed (Claude, 2026-09-23)

Everything above is research-055's data work. This section is what happened when it was built and
run, and it is the part to trust about *behaviour*.

**Built.** `--demo riverwood` is in `crates/engine/src/config.rs` with research-055's position and
yaw; `play-riverwood-demo.cmd` sits beside the Blackreach launcher. `--demo-tour` is no longer
Alftand-only: `crates/engine/src/demo_tour.rs` keys a route off the `--demo` name, and Riverwood's
is the eight doors of section 3's four houses, entered and left again (impl-056).

**The crossings work, including the return trips.** The tour walks each doorway rather than
teleporting: it presses `E`, holds `W`, and photographs every frame either side of the swap. Across
the crossings measured so far the mean luma steps 171 → 189 entering the Trader and 174 → 186
entering Alvor's, with **no dark frame at any swap** - the same result the Alftand route gives, now
also in daylight and now also *leaving* an interior.

**One tour bug, fixed.** The walk-through tried two standoffs, 60 and 320 units. Sixty never works
on these doors (the player ends up against the leaf), and inside Alvor's House the far wall is
nearer than 320, so the first run stopped at door `000133FB` with neither standoff walkable. A
third, 160, is now tried last, so the Alftand route still walks in from exactly where it did.

**Two visual gaps the data work could not predict**, both dataset-wide rather than Riverwood's:

1. **Everything reads wet.** `docs/research/specular-gloss-mapping.md`: the converter mapped
   Skyrim's Blinn-Phong glossiness *exponent* as a linear percentage, so the village's timber,
   posts, thatch and barrels were published at roughness 0.2 and 14,825 materials at 0.0.
   `local/reference/rw1/_crop-gloss.png` is the porch outside the Sleeping Giant Inn.
2. **The pine forest is a spray of dots.** `docs/research/foliage-alpha-test.md`: the trees are
   published correctly as alpha-`MASK`, but Bevy tests `vertex_color.a * texture.a` against the
   cutoff and Skyrim's tree NIFs carry a `COLOR_0` whose alpha is a shader parameter, not opacity.
   `local/reference/rw_sweep/_crop-tree.png`. This is the single biggest difference from the game's
   own screenshots of a forest village - bigger than the missing NPCs.

Section 4's predictions held otherwise: no NPCs, no grass, the mill wheel still, the FX cards
empty, the river a flat sheet, and no LOD beyond the terrain ring.

**The poses need a fit pass, and section 6's eye heights are the reason.** All eight exterior poses
were computed at ground + 120, the player's eye height. UESP's "place" screenshots are free-camera
shots from well above the rooftops (`SR-place-Riverwood.jpg` is an aerial; `SR-place-Sleeping_Giant_Inn.jpg`
looks down on the inn at about 30°), so an eye-height camera cannot frame them however the yaw is
chosen. `RW-01`, `RW-02`, `RW-04` and `RW-05` stand inside vegetation or at porch level as a
result. `RW-07` (the village from Faendal's doorstep) and `RW-09` (the inn's common room) are
already close, and are the two worth keeping as they are. The twelve poses live in
`tools/reference/riverwood_shots.json`; the first renders are `local/reference/rw1/`, and a sweep
of elevated candidates is `local/reference/rw_sweep/`.
