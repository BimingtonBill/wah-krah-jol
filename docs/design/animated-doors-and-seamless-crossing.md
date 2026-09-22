# Animated doors and seamless cell-to-cell walking

**Status:** design, 2026-09-22 (DeepSeek, research-027). For the lead's review before any coding
task is written.

**Scope.** Two user requests:

1. **Load doors that open with their real animation** - the model's own `Open`/`Close`
   controller sequence, not a hard-coded swing.
2. **Walking through an open door from one cell into the next without noticing** - no teleport
   jump, no loading pause, no change in where the player looks.

The engine already has half of this: a portal camera renders the destination through a doorway
(`crates/engine/src/portal.rs`), a crossing moves the camera to the `XTEL` arrival
(`crates/engine/src/transition.rs`), the player walks, opens on `E` and crosses auto-load markers
on contact (`crates/engine/src/player.rs`), and load doors draw closed unless the portal uses them
(impl-022). This document designs the rest.

**Citation convention.** Repository files use `path:line`. Bevy is outside the checkout, so it is
cited as `crate-version/src/file.rs#Lline` - `#L` rather than `:line` because
`tools/check_citations.py` resolves only repo-relative paths and would report every registry path
as broken. The files are in the read-only registry listed in `tasks/deepseek/context.md`:
`<user profile>/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/<crate>/`. Claims are
marked **verified** (bytes or code read while writing this) or **inferred**.

---

## 0. The short version

* **The data path is one new converter step**: a NIF's `NiControllerManager` -> `Open`/`Close`
  `NiControllerSequence`s -> per-axis euler curves in `NiTransformData` -> **baked, sampled glTF
  animation clips** in the same `.glb` the engine already loads. No new runtime format.
* **The engine plays the clip** with Bevy's `AnimationPlayer` on the door model's own root, with
  an `AnimationGraph` the engine builds itself - `bevy_gltf` inserts a player but **no graph**
  (`bevy_gltf-0.19.0/src/loader/mod.rs#L1085`), and `advance_animations` needs both
  (`bevy_animation-0.19.0/src/lib.rs#L1035`).
* **The crossing stops being a teleport.** `portal.rs` already owns the rigid map from a door
  frame to its `XTEL` arrival frame (`portal.rs:380`, `portal.rs:390`). The crossing applies that
  same map to the **player's current eye pose** instead of snapping to the arrival point. Because
  the portal camera is placed by the same function, the doorway image and the real destination
  agree in the swap frame by construction.
* **Two concrete seamlessness bugs to fix**: the door leaf is un-hidden within 1 unit of the plane
  (`portal.rs:142`, `portal.rs:875`), and the whole-screen ambient/clear colour/fog changes with
  the active space (`app.rs:1395`), so the swap frame pops.
* **A finding the lead must know before promising the user anything.** The three real doors on the
  current Alftand demo route are all `DweDoorLarge01Load`
  (`Dungeons\Dwemer\Door\DwemerLargeDoorLoad01.nif`), whose own `Open` sequence rotates its two
  leaves by **5.4 degrees and 8.7 degrees**. The fourth route door is an invisible
  `AutoLoadDoor01` marker with no model and no animation. On this route "the real animation" will
  look almost static (section 1.4). A Nordic or Imperial door swings 120-135 degrees.

---

## 1. Door animation in the data

### 1.1 Where it lives in a NIF

A Skyrim SE door model that opens is a Gamebryo NIF (version 20.2.0.7, user version 12) whose
block table carries, **verified** by reading the files (section 1.3):

| block | role |
|---|---|
| `NiControllerManager` | one per model; holds the list of sequences and a palette |
| `NiControllerSequence` | one per named animation: `Open`, `Close` |
| `NiMultiTargetTransformController` | the controller the sequences bind their interpolators to; lists the animated nodes as *extra targets* |
| `NiTransformInterpolator` | one per (node, sequence): the **rest** transform plus a reference to the key data |
| `NiTransformData` | the keys themselves |
| `NiDefaultAVObjectPalette` | node **names** -> block indices |
| `NiTextKeyExtraData` | named time marks inside a sequence, e.g. sound events |

The vendored parser (`vendor/project-wormhole-nif`) has type definitions for the first four and
for the palette and text keys - `nif_block.rs:882` (`NiControllerManager`), `:891`
(`NiTimeController`), `:903` (`NiControllerSequence`), `:918` (`NiSequence`), `:927`
(`ControlledBlock`), `:1054` (`NiMultiTargetTransformController`), `:1296`
(`NiTextKeyExtraData`), `:1520` (`NiDefaultAVObjectPalette`) - **but it does not parse the key
data**: `NiTransformController` (`nif_block.rs:1205`), `NiTransformData` (`nif_block.rs:1209`)
and `NiTransformInterpolator` (`nif_block.rs:1213`) are `// TODO` structs with no fields, and the
block-type match has no arm for them, so they fall to `_ => Ok((i, NifBlock::Unhandled))`
(`nif_block.rs:527`).

**This does not corrupt the parse.** `open_nif_resilient` slices each block with the size from the
header before parsing (`crates/converter/src/mesh.rs:392`), so an unparsed block is skipped, not
misread; unparsed types are counted in `NifParseDiagnostics::fallback_blocks`
(`crates/converter/src/mesh.rs:419`). Adding a parser for these three types is therefore
**additive and safe**.

### 1.2 The `NiTransformData` layout, fitted from the bytes

`nif.xml` documents `NiTransformData` as Rotation / Translation / Scale groups. The rotation group
of every animated door in this install uses `XYZ_ROTATION_KEY` (KeyType 4,
`vendor/project-wormhole-nif/src/nif_enum.rs:54`), whose doc comment is "Separate X, Y, and Z keys
will be stored instead of using quaternions". The layout that consumes **all 645** `NiTransformData`
blocks of the door-ish NIFs in `%OPENSKYRIM_CONVERTED_DIR%\vfs\meshes` exactly is:

```
NiTransformData
  u32  Num Rotation Keys          # number of XYZ key sets (1 in every file checked)
  u32  Rotation Type              # 4 = XYZ_ROTATION_KEY
  repeat Num Rotation Keys * 3:   # X, then Y, then Z
      u32 Num Keys                #   <- a nif.xml KeyGroup (nif_block.rs:944): count first, then type
      u32 Key Type                #   1 linear / 2 quadratic / 3 TBC / 5 constant
      keys: f32 Time, f32 Value   #   + forward/backward (8 B) for quadratic, + T/B/C (12 B) for TBC
  u32  Translation Type           # 0 with no keys in every door checked
  u32  Num Translation Keys       # 0
  u32  Scale Type
  u32  Num Scale Keys
```

**Values are radians about the node's local axis.** `FarmhouseAnimDoor01.nif`'s node `Door01`
swings about Z from `0.0` to `-1.65806` rad = **-95.000 degrees** over 1.0 s - a door swing, and
the calibration for the whole layout. Independent cross-checks:

* the dwemer door's node `Object02` has rest rotation `[1, 0, 0, 7.5e-08]` in the exported GLB (a
  180 degree turn about X) and its X curve is the constant `3.14159` - the rest transform *is* the
  curve value at t = 0;
* `Object38`'s rest quaternion `[-8.7e-08, 6.6e-09, -0.0749, 0.99719]` is an angle of
  `2*asin(0.0749) = 0.150` rad about Z, and its Z curve starts at `-0.14999`.

**Times are seconds, starting at 0.** The sequence's own `Start Time`/`Stop Time` agree with its
text keys: `DwemerSmallDoorLoad01`'s `Open` sequence spans `[0.0, 0.6]` and its text keys are
`start` at 0.0, `Sound: DRSDwemerSmall01Open` at 0.000025, `end` at 0.6. `Cycle Type` is `2`
(Clamp, `nif_enum.rs:957`) for every door sequence checked: **the animation ends holding its last
key.**

Fitted and verified by `tools/research/door_animation_nif.py` (written for this note; `nif`,
`debug`, `census` and `glb` commands). The fitter searches header orders and key widths and reports
the unique layout that consumes every block exactly.

**What I could not confirm**: the two words that follow the rotation curves are always present and
always zero (a further group header with zero keys), and 8 of the 645 blocks are of that shape. The
other 637 carry **more** data after the rotation - 10 to 43 words, whose first words look like
further `(count, type)` group headers and whose payloads contain plausible times (`0.0333, 0.0667,
0.1, ...`). I did **not** fit those groups. They are translation and/or scale keys, **a majority of
doors animate something besides rotation**, and the converter task below must fit them the same way
before it can claim to export a door faithfully. This is the one real hole in the data story; it is
bounded (the fitter is in the tool, and 8/645 blocks are rotation-only).

### 1.3 What is in the demo route's own doors

From `skyrim_world.db` (`tools/research/route_door_models.py`), the four doors of
`demo_tour::ALFTAND_ROUTE` (`crates/engine/src/demo_tour.rs:24`):

| ref | base | model |
|---|---|---|
| `00015D48` | `AutoLoadDoor01` | `AutoLoadMarker01.nif` - invisible, no leaf, **no animation** |
| `00092809` | `DweDoorLarge01Load` | `Dungeons\Dwemer\Door\DwemerLargeDoorLoad01.nif` |
| `0009256A` | `DweDoorLarge01Load` | `Dungeons\Dwemer\Door\DwemerLargeDoorLoad01.nif` |
| `0006998D` | `DweDoorLarge01Load` | `Dungeons\Dwemer\Door\DwemerLargeDoorLoad01.nif` |

`DwemerLargeDoorLoad01.nif` (35 blocks) and `DwemerSmallDoorLoad01.nif` (31 blocks) both carry
`NiControllerManager`, `Open` and `Close` sequences (`Clamp`, weight 1.0, 0.0-0.6 s;
the large door's `Close` is 0.0-0.6333 s), a `NiMultiTargetTransformController`, four
`NiTransformInterpolator`s and four `NiTransformData`s, and a palette mapping `Object37`, `Object38`,
`Object02`, `Object43`, `Plane04` to node blocks. The controlled blocks name those nodes, and the
interpolator's own T/R/S fields are the "invalid" sentinel `-FLT_MAX`, i.e. **the node's rest
transform is the pose and only the keys move it**.

### 1.4 The finding: the route's doors barely move

`DwemerLargeDoorLoad01.nif`, `Open` sequence, the only two animated nodes:

| node | axis | t = 0 | t = 0.6 | swing |
|---|---|---|---|---|
| `Object02` | Z | 0.0 | +0.09348 | **5.36 deg** |
| `Object43` | Z | 0.0 | -0.15249 | **-8.74 deg** |

Their meshes sit ~387 units from their pivots (GLB node transforms), so the free edges travel about
36 and 59 units on a door roughly 800 units across - visible, but nothing like a door opening.

`DwemerSmallDoorLoad01.nif` is the same shape: `Object38` Z `-0.150 -> 0.0`, `Object37` Z
`0.0 -> -0.164`, i.e. two leaves separating by ~9 degrees each.

So: **implementing "the real animation" faithfully means the demo route's doors will look nearly
static.** If the user wants to *see* a door swing, the demo needs a door that swings -
`ImpWoodDoorSingleLoad01` (123 degrees), `ImpJailDoor01/02` (121-123), `TGSecretDoor01` (127),
`DLC2TelMithrynDoor03` (115) - or the route extended.

### 1.5 How much of the install is animated

`tools/research/door_animation_nif.py census`, over `%OPENSKYRIM_CONVERTED_DIR%\vfs\meshes`:

| scope | files | animated | with `Open` **and** `Close` | `NiTransformData` blocks fitted |
|---|---|---|---|---|
| path contains `door` | 768 | 172 | **158** | 645 / 645 |
| `meshes/architecture` | 2,941 | 110 | 86 | 444 / 444 |

Sequence durations over 363 door sequences: min 0.0333 s, **median 0.8333 s**, max 17.9 s. Other
sequence names exist (`Forward`, `Backward`, `Idle`, `Stage1`, `open`, `close`), so `Open`/`Close`
is a convention, not a guarantee, and the engine must not *require* them.

### 1.6 What the converter does with all this today

**Nothing.** `convert_nif_to_glb` (`crates/converter/src/mesh.rs:49`) calls the vendored
`nif_to_model` / `model.to_glb()` (`crates/converter/src/mesh.rs:105`) and then rewrites materials
and texture URIs in the GLB JSON (`crates/converter/src/mesh.rs:142`,
`crates/converter/src/mesh.rs:254`). The vendored exporter has no animation code at all - a search
for `animation` in `vendor/project-wormhole-nif/src/export/gltf.rs` returns nothing - and
`generator`/`animations` are absent from the output. **Verified** by dumping the real GLB:

```
dwemersmalldoorload01.glb: animations: []   nodes: DweSmallDoorLoad01, Object38, Object38:6, ...
```

The good news for the design: the exporter **preserves node names and TRS** (`Object38`,
`Object37`, `Plane04`, each with its rest translation and rotation), and it already wraps the model
in a `Creation-to-glTF basis` root node. An animation clip only has to name those same nodes.

### 1.7 What the engine does today

* The door model is spawned as a scene under the reference entity:
  `WorldAssetRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(path)))`
  (`crates/engine/src/streaming.rs:1203`).
* The portal holds the door->arrival map and places a second camera with it
  (`crates/engine/src/portal.rs:355`, `:380`, `:390`), clipped at the doorway
  (`crates/engine/src/portal.rs:416`).
* A crossing **snaps**: `apply_door_crossings` sets the camera to
  `switch_space(...) + Y * EYE_HEIGHT` and the arrival yaw
  (`crates/engine/src/transition.rs:176`, `:198`-`:202`).
* `E` targets a door within 250 units and 45 degrees (`crates/engine/src/player.rs:470`);
  auto-load markers fire on contact (`crates/engine/src/player.rs:854`, `:527`).
* impl-022 hides a load door's leaf only while the portal is rendering through it
  (`crates/engine/src/portal.rs:753`, and `state.open_door` is cleared before every early return
  at `crates/engine/src/portal.rs:875`).

---

## 2. Design: the data path (NIF -> converter -> glTF -> engine)

```
 .nif  --[converter, new]-->  .glb { animations: [ {name:"Open"}, {name:"Close"} ] }
                                                     |
                            bevy_gltf AnimationClip handles (sub-assets of the same path)
                                                     |
                       AnimationGraph (built by the engine) + AnimationPlayer on the door root
```

### 2.1 Converter: parse the three block types

New module `crates/converter/src/nif_animation.rs`:

```rust
pub struct DoorAnimation { pub name: String, pub duration: f32, pub tracks: Vec<NodeTrack> }
pub struct NodeTrack { pub node: String, pub keys: Vec<(f32, Vec3, Quat, f32)> } // sampled pose
pub fn read_animations(nif: &NifFile) -> Vec<DoorAnimation>;
```

* Walk `NiControllerManager` blocks -> their `controller_sequences` refs -> `NiControllerSequence`
  (name from the header string table, `start_time`/`stop_time`, `cycle_type`).
* For each `ControlledBlock`: node name from the string table, interpolator ref -> `NiTransformData`.
* Decode the rotation group per section 1.2 (and the translation/scale groups, which must be fitted
  first - see the open question in section 1.2); evaluate the euler `(x, y, z)` to a quaternion with
  the runtime basis, i.e. `Rz(z) * Ry(y) * Rx(x)` in the model's own frame.
* **Bake to samples at the union of all key times** of that clip's curves, so the (common) linear
  keys survive exactly and quadratic keys are interpolated between their own key times. Emit
  `LINEAR` interpolation: glTF cannot express an euler curve, so per-node quaternion channels are
  the only faithful encoding anyway.

### 2.2 Converter: write the clips into the GLB

Two options; **recommendation: (a)**, because it keeps every changed file inside
`crates/converter/`:

**(a) Append the animation to the finished GLB in `mesh.rs`.** The converter already parses the GLB
JSON (`crates/converter/src/mesh.rs:254`) and rewrites it
(`crates/converter/src/mesh.rs:142`). Add: one `animations` entry per clip; one `sampler` per node
channel (input = key times, output = quaternions); accessors appended to `bufferViews`/`accessors`
and their bytes appended to the BIN chunk (`write_glb_atomic`, `crates/converter/src/mesh.rs:512`,
already handles the file). Channel target = the glTF node index of the named node, resolved by name
from the scene's node list - names are preserved today (section 1.6), so this is a lookup, not a
change.

**(b) Teach `vendor/project-wormhole-nif`'s exporter to emit animations.** Cleaner in principle,
but `vendor/` is the lead's integration surface and the exporter has no animation structure at all
to extend.

### 2.3 Engine: play a clip

Facts that shape this, all read from the registry sources:

* `Gltf` exposes `animations: Vec<Handle<AnimationClip>>` and
  `named_animations: HashMap<Box<str>, Handle<AnimationClip>>`
  (`bevy_gltf-0.19.0/src/assets.rs#L18`, `#L43`, `#L46`); `GltfAssetLabel::Animation(usize)` loads
  one clip directly from the model path without loading the whole `Gltf` asset
  (`bevy_gltf-0.19.0/src/label.rs#L33`, `#L60`).
* `GltfLoaderSettings::load_animations` defaults to `true`
  (`bevy_gltf-0.19.0/src/loader/mod.rs#L201`, `#L227`), and the loader puts an `AnimationPlayer` on
  each animation root node (`bevy_gltf-0.19.0/src/loader/mod.rs#L1085`) plus `AnimationTargetId` on
  the animated descendants (`bevy_animation-0.19.0/src/lib.rs#L187`, `#L196`, and the loader builds
  those ids from the node-name path at `bevy_gltf-0.19.0/src/loader/mod.rs#L562` and `#L1558`).
* **The loader does not create or attach an `AnimationGraph`** - a search for `AnimationGraph` in
  `bevy_gltf-0.19.0/src/loader/mod.rs` finds nothing. `advance_animations` queries
  `(&mut AnimationPlayer, &AnimationGraphHandle)`
  (`bevy_animation-0.19.0/src/lib.rs#L1035`) and iterates the graph's nodes (`#L1052`). **The engine
  must build the graph and insert `AnimationGraphHandle` on the same entity the loader gave the
  player to.** `AnimationGraph::from_clips(...)` returns `(AnimationGraph, Vec<AnimationNodeIndex>)`
  (`bevy_animation-0.19.0/src/graph.rs#L458`); the node index is what `AnimationPlayer::play` takes
  (`bevy_animation-0.19.0/src/lib.rs#L866`).
* **A door that is open stays open**: `RepeatAnimation` defaults to `Never`
  (`bevy_animation-0.19.0/src/lib.rs#L471`, `#L473`); `ActiveAnimation::update`
  (`bevy_animation-0.19.0/src/lib.rs#L563`) stops at the clip duration and returns **before** the
  wrap-around (`#L581`-`#L587`), and the curves are sampled clamped (`#L382`), so the last key's
  pose persists. Replaying `Open` after it finished is `player.play(node)` then
  `ActiveAnimation::replay` (`bevy_animation-0.19.0/src/lib.rs#L595`) - i.e. restart, not re-open.
* Cross-fading `Open` into `Close` if a reversal is wanted:
  `AnimationTransitions::play(player, node, Duration)`
  (`bevy_animation-0.19.0/src/transition.rs#L33`, `#L78`). For doors it is optional; the clips
  already end where the other begins.

### 2.4 Does the engine need to know a model is animated?

Yes, and it must not guess. Two ways:

* **(recommended) a converter-recorded fact in the database.** One nullable column pair on
  `statics` (`animation_open_clip`, `animation_close_clip`, the glTF clip indices) alongside the
  existing bounds columns. Costs a `shared::WORLD_DATABASE_SCHEMA_VERSION` bump (currently `5`,
  `crates/shared/src/lib.rs:7`) and a reconversion of `skyrim_world.db`. This is the same shape as
  impl-014's `lights` table, so it is routine.
* **no schema change**: load `Animation(0)`/`Animation(1)` speculatively for every door with a
  model. Cheap in count (a few doors per cell) but loads non-existent sub-assets for every static
  door, which logs a failure per door per cell. Not recommended.

The engine only needs "is there an `Open` clip and which index"; the clip's **duration** comes from
`AnimationClip::duration()` (`bevy_animation-0.19.0/src/lib.rs#L254`) rather than from the database.

---

## 3. Door state machine

```
                 E / contact                animation reaches OpenFraction  (default 0.5)
   Closed ─────────────────────► Opening ─────────────────────────────────► Open
      ▲                             │                                        │
      │  Close finished             │ player leaves the trigger box          │ E / contact
      │                             ▼                                        ▼
      └──────────────────────── Closing ◄────────────────────────────────────┘
```

* **Closed** - leaf drawn, portal off, crossing armed but the plane trigger disabled.
* **Opening** - the `Open` clip plays once (`RepeatAnimation::Never`). `t = animation_elapsed /
  duration`.
* **Open** - clip finished and holding. The portal may render through the doorway as soon as
  `t >= OPEN_FRACTION`; the crossing trigger is enabled at the same point. A door *without* an
  animation is promoted to `Open` in the same frame, so behaviour for static doors is exactly
  today's.
* **Closing** - `Close` plays. Trigger disabled immediately, portal off, leaf solid.
* **Auto-load doors** never enter `Opening`: no leaf, no animation, and `player_auto_doors`
  (`crates/engine/src/player.rs:854`) keeps firing on contact.

Simplifications, deliberately:

* **No auto-close.** Skyrim keeps a load door open until it is used again or the cell reloads, and
  a door that closes itself behind a walking player would fight the crossing. `Closing` is entered
  only on a new `E` (today's semantics) or when the door is activated from the far side.
* **One reversal policy**: `E` on an `Open` door starts `Close` from the current pose; because the
  clips are Clamp and start where the other ends, no blending is needed. If that reads badly, add
  `AnimationTransitions` with a 0.1 s fade (section 2.3).
* **Sound is out of scope**, but the text keys that name it (`Sound: DRSDwemerSmall01Open`) are
  parsed and can be carried into `AnimationClip` events for a later audio pass.

---

## 4. The crossing algorithm

### 4.1 The map, and why it is the right one

`portal.rs` already defines the rigid map between a door's frame and its arrival frame, and the
portal camera is placed by it:

```
arrival_frame(destination, origin) -> (A_pos, A_rot)      portal.rs:355
M_rot  = A_rot * Ry(pi) * D_rot^-1                        door_to_arrival_rotation, portal.rs:380
M(p)   = A_pos + M_rot * (p - D_pos)                      portal_pose, portal.rs:390
```

with `D_pos`, `D_rot` the door reference's render pose and `A_rot = Quat::from_rotation_y(-yaw)`
(`crates/engine/src/transition.rs:109`). Properties, verified by the module's own tests
(`portal.rs:1188`, `portal.rs:1222`):

* `M` is rigid: it moves poses, it does not stretch the destination.
* `M(D_pos)` is the `XTEL` arrival point, so a player standing on the door's origin lands on the
  arrival point; a player with their eye `EYE_HEIGHT` above it lands `EYE_HEIGHT` above it,
  because `M_rot` is a yaw and preserves the up axis.
* A camera **looking at** the door maps to a camera looking **along** the arrival facing - i.e.
  walking into the source door means walking away from the destination door, which is what the
  portal already shows and what `arrival_camera_rotation` already produces.
* **It never uses the destination door's position.** The `XTEL` arrival is routinely offset from the
  destination door by hundreds of units, and that is the point: the map keys off the arrival frame,
  so the offset is carried exactly.

### 4.2 The change to `apply_door_crossings`

Today (`crates/engine/src/transition.rs:176`-`:209`): on `ActivateDoor`, snap the camera to
`switch_space(target, XTEL_position) + Y*EYE_HEIGHT` with the arrival yaw.

Seamless version, in **Creation space** so the render origin can change once, at the end:

1. Read the eye pose `E_render` and the player's yaw/pitch.
2. Convert to Creation space. With `origin` the current `RenderOrigin` and
   `creation = runtime_to_creation_vector` (`crates/shared/src/coordinates.rs:20`):
   `runtime_to_creation(E_render + (origin.x*CELL, 0, -origin.y*CELL))` for an exterior and
   `runtime_to_creation(E_render)` for an interior - the inverse of
   `crates/engine/src/streaming.rs:2012` and `:2000`.
3. `z_door = -yaw(D_rot)`, `z_arrival = XTEL.rotation.z`.
4. `alpha = z_arrival - z_door - PI` (radians about Creation up; Creation headings add).
5. New feet: `C' = C_arrival + Rz(alpha) * (C_feet - C_door)`. New heading: `z' = z_player + alpha`.
6. `switch_space(target, C', ...)` (`crates/engine/src/transition.rs:139`) returns the render-space
   position of `C'` and moves `RenderOrigin` if the destination is an exterior. Set
   `camera.translation = that + Y*EYE_HEIGHT`; set the rotation from `z'` with **the player's pitch
   unchanged**.
7. `apply_crossing` (`crates/engine/src/player.rs:461`) keeps doing what it does - zero the
   velocity, drop `grounded` - because a crossing is still a discontinuity for physics even though
   it is none for the view.

`alpha` and the resulting pose are yaw-only, so the pitch does not need to be touched; if pitch and
roll are ever non-zero on an `XTEL`, the same formula generalises to the full quaternion
`M_rot * look_rotation` decomposed with `EulerRot::YXZ`.

**Why the swap is invisible**: the doorway quad in the frame before the swap shows the destination
rendered from `M(E_render)` (`crates/engine/src/portal.rs:965`), and the frame after shows the
destination rendered from exactly `M(E_render)` by the main camera. Same pose, same map, same
frame - so the pixels are continuous. Nothing else in the design is load-bearing for
seamlessness; everything below is what can still break it.

### 4.3 Trigger: fire on the doorway plane, not on a key press

A new message (or a mode on `ActivateDoor`, see section 6) carries "the feet crossed the doorway":

* The doorway is the box `player::auto_door_trigger` already builds
  (`crates/engine/src/player.rs:527`) - the same extents the portal draws through
  (`crates/engine/src/portal.rs:522`), so "walked into the door" and "looked through the door"
  cannot disagree.
* Fire when the feet cross the plane from the door's **front** side to its back side, while the
  state is `Open`, and only once per entry (reuse `AutoDoorLatch`,
  `crates/engine/src/player.rs:577`).
* The front comes from impl-023's link-derived front, not the model axis - so this trigger depends
  on impl-023 landing. If it has not, the trigger fires on the *nearest* face instead, which is
  wrong for the tower door and is exactly the symptom impl-023 fixes.
* **Auto-load markers** (`auto_load == true`) keep firing on contact
  (`crates/engine/src/player.rs:854`) and take the mapped crossing, not the snap: the marker has no
  leaf, and its volume is centred on the reference, so the feet are already on the plane when it
  fires.

### 4.4 Deferring the crossing until the destination is there

`select_portal_door` already gates on `destination_is_resident` (`crates/engine/src/portal.rs:483`,
`:584`); the crossing itself does not. Add the same gate to the crossing: a request whose
destination is not resident is **held** (the door stays open, the player keeps walking) and applied
in the frame residency arrives. `plan_door_prestream` starts the destination streaming at 800 units
(`crates/engine/src/transition.rs:27`), so the wait is normally zero frames, and the guarantee
becomes "no loading pause" rather than "no loading pause in practice".

### 4.5 What is behind the player

After the swap, `update_portal` runs in the same frame, after `DoorTransition`
(`crates/engine/src/portal.rs:174`), and picks the nearest door in the **new** active space - which
is the return door. `plan_door_prestream` keeps the cell just left resident while the player is
within 800 units of it, so the return portal has a destination to render. Both already work; the
design only has to not break them, and to accept that the source cell unloads once the player walks
away - at which point the return doorway is a closed door again (impl-022).

### 4.6 Collision

* **A closed leaf is solid**: the walk probe ray-casts against visible meshes
  (`crates/engine/src/player.rs:629`), and the leaf's meshes are children of the door reference.
  That is today's behaviour and it must stay - walking through a closed door would be a worse bug
  than any teleport.
* **An open leaf must not block the doorway**, and a leaf mid-swing must not trap the player. The
  loader puts `AnimationTargetId` on exactly the animated nodes
  (`bevy_animation-0.19.0/src/lib.rs#L187`); mark the door's animated subtree with a `DoorLeaf`
  component and extend the probe's `skip` filter (`crates/engine/src/player.rs:759`) to drop it
  while the door's state is `Opening` or `Open`. The door's frame and arch are separate nodes and
  stay solid, so the doorway is still a doorway.
* The player's body radius is 32 units (`crates/engine/src/player.rs:82`) and the model's own
  doorway is what the portal measures (`crates/engine/src/portal.rs:522`), so a doorway that
  admits the portal admits the player.

### 4.7 What still pops, and what to do about it

| source | why | fix |
|---|---|---|
| the leaf reappears within 1 unit of the plane | the portal stops placing its camera at `MIN_PORTAL_DOOR_DISTANCE` (`portal.rs:142`), clears `open_door` (`portal.rs:875`) and `show_load_door_leaves` draws every other door closed (`portal.rs:753`) | make "leaf hidden" a property of the **door state** (`Opening`/`Open`), not of the portal being up. One-line change in `show_load_door_leaves` plus a `DoorState` read; needs a test that a door 0.5 units from the camera keeps its leaf hidden |
| ambient, clear colour and fog change for the whole screen | `update_atmosphere` keys off the active space (`crates/engine/src/app.rs:1395`) and `GlobalAmbientLight`/`ClearColor`/`DistanceFog` are global resources (`app.rs:1399`, `:1522`, `:1597`) | ramp them over ~0.2 s after a crossing (`DoorCrossed` already exists), or accept the pop: at the swap frame the destination fills the screen |
| the portal texture is not tonemapped | deliberate (`portal.rs:689`): the quad's unlit material hands the image to the main camera's tonemapper | none - it is what keeps the doorway the same brightness as the room |
| lights of the destination leak into the source through the GPU cluster | known, noted in `portal.rs:67` | out of scope here |

---

## 5. Edge cases

| case | behaviour |
|---|---|
| A door with **no** animation (a static leaf - 596 of the 768 door NIFs) | promoted to `Open` in the frame it is activated; the crossing maps the pose; the leaf is hidden by the *state*, so the doorway is a hole |
| A model with `Open` but no `Close` | `Open` plays once and holds; re-activation restarts it, it never closes |
| A sequence named `open`/`close` in lower case, or `Forward`/`Backward` | the converter records whatever names exist and the engine looks for a **case-insensitive** `"open"`, else the first clip; a door with none is a static door |
| A door that should be *open* when its cell streams in | the state starts `Closed` at the model's rest pose, and the rest pose *is* the closed pose (verified: rest == t = 0 of `Open`). Persisting door state is out of scope; no route door needs it |
| `--shots` sets a camera, not a player | `PlayerPlugin` is only added when `walk` is set, and `walk` excludes shots runs (`crates/engine/src/app.rs:139`, `:185`); the door plugin must be inert without a player, and shots of an open door need the animation driven to a fixed time - out of scope, note it as a gap |
| `--demo-tour` writes `ActivateDoor` and expects a crossing within a couple of seconds (`crates/engine/src/demo_tour.rs:229`) | keep a "cross now" path (section 6); a tour that walks through an animating door is the better acceptance check but a larger change |
| `E` on a door 250 units away | starts the open animation; the player must walk to it. Today `E` teleports. This is the user-visible behaviour change and the point of the feature |
| A crossing while the player is airborne or mid-swing | `apply_crossing` already zeroes the velocity (`crates/engine/src/player.rs:461`); the mapped pose keeps the height, so no snag |
| The destination is not resident yet | the crossing is held open (section 4.4) |
| An exterior destination whose grid differs from the arrival's | `switch_space` re-places roots and rebases (`crates/engine/src/transition.rs:155`); the map is computed in Creation space, so the rebase is applied once, to the *mapped* position |
| Non-yaw `XTEL` rotations | the map generalises to the full quaternion (section 4.2); the arrival-camera helper ignores pitch and roll today (`crates/engine/src/transition.rs:109`) and load-door `XTEL`s have none |

## 6. Implementation plan

Four coding tasks with disjoint owned files. Everything else - `crates/shared/`, `app.rs`,
`streaming.rs`, `demo_tour.rs` - is the lead's integration surface.

| # | task | owned files | acceptance | runtime verification |
|---|---|---|---|---|
| **1** | **Converter: NIF animation -> glTF clips.** `nif_animation.rs` with the section 1.2 decoder (rotation fitted; translation/scale fitted first), name/euler/bake logic, and the GLB writer that appends `animations` + accessors. Unit tests build a synthetic NIF byte fixture (a `NiControllerManager`, two sequences, one node, three euler curves) and assert the decoded clip: clip names, duration 0.6, one channel, sample count and the endpoint quaternion. A second test asserts the GLB JSON has the animation, names the right node index, and that the accessor bytes are inside the BIN chunk. A third runs the same fixture with a NaN/infinite key and asserts it is rejected, not written | `crates/converter/src/nif_animation.rs` (new), `crates/converter/src/mesh.rs`, `crates/converter/src/lib.rs` | `cargo test -p converter`, `cargo clippy -p converter --all-targets -- -D warnings`; `python tools/research/door_animation_nif.py census <vfs>/meshes door` still reports 645/645 | convert the four route models into a staging dir and dump them: `python tools/research/door_animation_nif.py glb <staging>/meshes/dungeons/dwemer/door/dwemerlargedoorload01.glb` shows `animations` with `Open`/`Close` and node names `Object02`/`Object43` |
| **2** | **Converter + DB: record each model's clips.** `statics.animation_open_clip` / `animation_close_clip` (INTEGER, NULL = none) written from task 1's result; schema version 5 -> 6 | `crates/converter/src/esm/**`, `crates/converter/src/bin/**` - **the `crates/shared/src/lib.rs` version constant and the engine's read path are the lead's** | `cargo test -p converter`, `cargo test -p shared`; a migration test on a fixture database | republish, then `python tools/research/route_door_models.py` extended to print the two columns: non-NULL for `00092809`/`0009256A`/`0006998D`, NULL for `00015D48` |
| **3** | **Engine: door animation state machine.** `DoorAnimationPlugin`: on stream-in, resolve the clips by index from the base record, build an `AnimationGraph` per door, insert `AnimationGraphHandle` next to the loader's `AnimationPlayer`, drive the states of section 3, mark the animated subtree `DoorLeaf`, and keep the leaf hidden while `Opening`/`Open` | `crates/engine/src/door_animation.rs` (new), `crates/engine/src/doors.rs` | unit tests with a hand-built `AnimationGraph` + `AnimationPlayer` (no GPU): activate -> `Opening`, drive time -> `Open` at `OPEN_FRACTION`, clip finished -> still `Open`; a static door reaches `Open` in one frame; a re-activation starts `Close`; `DoorLeaf` is set on `AnimationTargetId` entities only | `--demo-tour` on a route with an animating door: the leaf-visibility log and the per-door screenshots show the door open at the crossing frame; `--shots` of a pose facing a door with the animation driven to t = 0.6 |
| **4** | **Engine: seamless crossing.** Move `door_to_arrival_rotation`/`portal_pose`/`arrival_frame` into a shared, tested module; rewrite `apply_door_crossings` to map the pose in Creation space (section 4.2); add the plane trigger and the residency hold; keep a snap path for `--demo-tour` | `crates/engine/src/transition.rs`, `crates/engine/src/portal.rs`, `crates/engine/src/player.rs` | unit tests: the map is rigid; a player standing in the doorway keeps their eye height; walking direction maps to the arrival facing; the plane trigger fires once per entry and only when the state is `Open`; a request whose destination is not resident is held and applied later; **regression**: the four real route links of `transition.rs:237` still round-trip | `--demo-tour` still 4/4 crossings with 0 errors and comparable FPS, plus a *walk-through* variant: open the door, walk forward, and check the frames around the swap have no doorway-sized discontinuity (`tools/research/contact_sheet.py` over them) |

Rationale for the split: 1 and 2 are converter-side but touch different trees; 3 and 4 are
engine-side but 3 owns the new module while 4 edits the three files that own the mapping and the
walk. `doors.rs` goes to 3 because the state machine is what changes its contract; 4 only reads it.

Dependency order: 1 -> 2 -> 3 -> 4 for the full feature, but **4 is independent of 1-3** (it is the
map, and it improves static doors immediately: crossing stops teleporting even with no animation).
Land 4 first if the user wants to see something soon.

## 7. Schema bump and reconversion

* **A converter schema bump is needed only for task 2** (`WORLD_DATABASE_SCHEMA_VERSION` 5 -> 6,
  `crates/shared/src/lib.rs:7`) and it is the lead's call - it buys the engine the ability to know
  a door is animated without loading non-existent assets (section 2.4).
* **The mesh re-export happens either way**: task 1 changes every animated NIF's `.glb`. The
  converter is content-addressed (`conversion-manifest.json`, `conversion_cache`), so unchanged
  models are not re-exported.
* If the lead prefers to avoid the DB bump, task 2 can be dropped and the engine treats "is this
  model animated" as unknown; the cost is a speculative `Animation(0)` load per static door. I
  recommend the bump.
* A full reconversion of `skyrim_world.db` is not otherwise required: the door animation is
  self-contained in the `.glb`.

## 8. Open questions

1. **The route's doors barely move (section 1.4).** Does the user want the demo route changed to a
   swinging door (`ImpWoodDoorSingleLoad01`, 123 degrees), or is a faithful 5-degree dwemer door
   the point? This changes the demo, not the engine.
2. **The translation/scale groups** of `NiTransformData` are not fitted (section 1.2). 637 of 645
   door blocks carry them; the 8 I checked are all-zero. Task 1 must fit them first, or a door that
   slides will be exported as a door that does not move. I could not confirm their field order.
3. **Does Bevy apply an animation's final pose when the graph handle arrives a frame late?** If a
   clip is played before `advance_animations` sees the graph, the first frame shows the rest pose.
   Task 3 must not gate the *crossing* on "the animation started" but on `state == Open`.
4. **`--shots` and animated doors**: a shots run has no player and does not advance an animation to
   a chosen time, so reference screenshots of an open door are not reproducible yet. A shots-file
   field (`"door_open": ["00092809", 0.6]`) would fix it; out of scope unless wanted.
5. **impl-023** must land for the plane trigger's front. The crossing map itself does not depend on
   it.
6. **The 1-unit portal gap** (`portal.rs:142`) leaves a 1-2 frame window where the doorway is
   neither a portal nor the real destination, because `update_portal` refuses to place the camera.
   Section 4.7's state-driven leaf hiding covers the leaf; whether the hole is visible at all needs
   the runtime check in task 4.

## 9. Tools written for this note

* `tools/research/door_animation_nif.py` - BSA reader (`list`, `dump`, `doors`), NIF header and
  animation-block decoder (`nif`, `debug`), a layout fitter, a census (`census`), and a GLB
  inspector (`glb`). The `census` and `glb` commands are the acceptance instruments for tasks 1-2.
* `tools/research/route_door_models.py` - the demo route's door models and links, read-only from
  `skyrim_world.db`.
