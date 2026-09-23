# The door over the doorway image, and the destination's own sun

**Status:** design, 2026-09-23 (lead-072, Doorways). Written to be implemented as it stands; the
implementation brief is `tasks/deepseek/impl-073-doorway-image.md`.

**Scope.** Two faults in the doorway the portal draws through an open load door
(`crates/engine/src/portal.rs`):

1. **The swung leaf disappears behind the doorway image.** The user's own note: *"the door should
   animate and render over/on top of the portal"*. impl-066 established that Riverwood's house doors
   do animate and are drawn, and that what remains is geometric
   (`docs/handoff/impl-066-portal-matches-the-world.md`).
2. **The doorway image is lit by no directional light at all.** Not the leak impl-066 assumed; the
   opposite, and it is a render-layer fact rather than a lighting-data one.

A third fault that looked like the alignment half of (1) is **not** a defect; the measurement is in
`docs/research/portal-frames.md`.

---

## 1. Why the leaf disappears

The doorway image is a **quad** - an opaque mesh of the main camera's view, standing in the doorway's
plane (`PORTAL_QUAD_OFFSET = 0`, `portal.rs:207`), sampling the portal camera's render target by the
main camera's screen-space UV (`shaders/portal.wgsl`). The door's own leaf is an opaque mesh of the
same view. Both are drawn in one pass with one depth buffer, so **the nearer of the two wins per
pixel**; `RenderLayers` chooses which camera sees an entity, not which pass it is drawn in, and there
is nothing layer-shaped in between.

The quad's plane is the *centre of the door model's bounding box* along the door's front axis
(`doorway_in_frame`, `portal.rs:530`). For Riverwood's `FarmhouseLDoor01` that is 13.5 units to the
`DoorBlack` side of the leaf's own plane (impl-066's note, from the model's own bounds). A leaf that
swings past that plane - away from the player standing in front of the door - is therefore behind the
quad and covered by it: the player sees the room through the doorway and no door.

Three fixes that do **not** work, and why (all considered and rejected in impl-066 as well):

| idea | why not |
|---|---|
| a depth bias on the quad or the leaf | a bias is expressed in depth-texture units; the leaf can sit ~96 units behind the plane (a full leaf width at 90 degrees), and this is a screen-space, view-dependent distance |
| recessing the quad into the wall | it stops covering the opening at close range - the blank-doorway bug `PORTAL_QUAD_OFFSET`'s comment records |
| the quad stops writing depth | then *any* source-space geometry behind the doorway plane paints over the destination - for an exterior house that is the shell's own inner walls, so the doorway shows the house instead of the room behind it |

## 2. The design: the doorway's mirror

**Draw the door a second time, into the destination image, at the door's mapped pose.**

The portal camera is placed by the rigid map `crate::transition::portal_pose`:

```
M(p) = arrival_position + M_rot * (p - door_position)
M_rot = door_to_arrival_rotation(door_frame(door_rotation, outward), arrival_rotation)
```

with `arrival_frame` giving the arrival position and rotation (`transition.rs:262`, `:287`, `:303`),
and `M(door_position) == arrival_position` by construction. The portal camera renders the destination
from exactly `M(main camera pose)` (`portal.rs:1245-1290`), and the quad samples that image by the
**main camera's** screen-space UV. Therefore a point drawn at `M(P)` is rendered on the pixel where
`P` is - which is why the swap at the doorway is invisible (design section 4.2 of
`animated-doors-and-seamless-crossing.md`), and why an independent audit of the shader, the
projection and Bevy's own uniform (the alignment audit summarised in `docs/research/portal-frames.md`,
against `bevy_render-0.19.0/src/view/view.wgsl`'s `frag_coord_to_uv`) found no term that could shift
or scale it: the UV is a plain screen-space lookup from the drawing camera's viewport, and
`portal_projection` writes only `clip.z`.

So a private instance of the door's model, on `DESTINATION_LAYER`, with its root at `M(door root
pose)` and every descendant carrying the source node's own *local* transform, has these properties by
construction:

* every mesh of it draws exactly where the real door draws, on screen;
* it is in the destination image, so it appears **over the doorway image** wherever the doorway shows
  that part of the world - which is the user's request, and the reason to draw it in the destination
  space rather than as a screen-space overlay;
* the quad is only visible where no nearer *source*-space geometry covers it, so the mirror can never
  appear outside the opening;
* it is depth-tested against the destination's own geometry like everything else in that image.

Costs and consequences, accepted:

* The mirror's *lighting* is the destination's (it is drawn by the portal camera). The real leaf's
  part in front of the plane is lit by the source's. A leaf straddling the plane therefore has a
  lighting step across it, at the plane. This is physically defensible (a door leaf is lit by the
  space it is standing in) and it is a second-order effect next to the leaf vanishing.
* It is one extra scene instance per open doorway - at most one, because `update_portal` keeps one
  portal at a time - reusing the scene handle the door already has (`streaming.rs:1259-1274`), so no
  asset is loaded twice and no mesh is duplicated in memory.
* The mirror sits at the arrival point, which is typically 60-70 units in front of the destination
  door, so its *depth* in the destination image is that much nearer than the real leaf's. Visible
  only through the opening, and only against destination geometry within that band.

### Implementation notes (the brief carries these in full)

* spawn on the frame `PortalState::open_door` becomes a door in `Opening`/`Open { animated: true }`;
  despawn when it stops being that door or the door is gone.
* `RenderLayers::layer(DESTINATION_LAYER)` on **every descendant**, re-applied every frame while the
  scene is still spawning (`RenderLayers` is per mesh entity and is not inherited;
  `isolate_cells`, `portal.rs:1060`, is the existing pattern for that).
* `DoorLeaf { door }` on every node of the mirror. That is what makes `door_animation`'s leaf rule
  draw it exactly when the real leaf is drawn, and what makes the walk probe skip it
  (`doors::mesh_is_out_of_the_way`) - the mirror stands where the player lands, so it must never be
  solid.
* copy local `Transform`s in `PostUpdate`, after `AnimationSystems` and before
  `TransformSystem::TransformPropagate`; no second `AnimationPlayer`, no assumption about how the
  door is animated.
* `try_insert`/`try_remove`, not `insert`/`remove`: a crossing despawns a cell's worth of entities in
  the frame it happens in, and a command queued against one of them is a panic
  (`door_animation.rs:1319-1323`; and the crash the lead is fixing is exactly this class, in
  `portal.rs`'s `isolate_cells`).

## 3. The doorway image has no sun

`portal.rs:876-882` states that the engine's one `DirectionalLight` "lights every view of a frame",
which is why the destination's sun could not be per camera. Bevy does not work that way:

* `bevy_pbr-0.19.0/src/render/light.rs:1642-1654` builds each view's directional lights with
  `filter(|l| l.render_layers.intersects(view_layers))`, where `view_layers` is the **camera's**
  `RenderLayers` (`:1613`, `maybe_layers.unwrap_or_default()`) and the light's side is its own
  `RenderLayers` component (`:124`, `:815`; layer 0 when absent);
* the engine's sun is spawned at `app.rs:2117` with no `RenderLayers`, so it is layer 0;
* the portal camera is `RenderLayers::layer(DESTINATION_LAYER)` = layer 2.

So the portal camera is lit by **no directional light whatsoever**: a daylit Riverwood street seen
from inside a house is drawn with ambient and fog only, and nothing in the doorway image gets the
destination's sun. The fix is a `DirectionalLight` of the portal camera's own, on
`DESTINATION_LAYER`, whose colour and illuminance come from the destination's space
(`space_atmosphere(...).sun`, already computed in `update_destination_atmosphere`, `portal.rs:883`),
with the engine sun's rotation and shadow configuration copied from it. `MAX_DIRECTIONAL_LIGHTS` is 10
on desktop, and cascade counts are budgeted per *view*, so a second sun costs the main camera nothing.

The existing comment must be corrected in the same change: the doorway image is not in the source's
sun, it is in no sun.

## 4. What a run has to show

None of this is settled by unit tests. When the lead runs the engine:

* **the mirror**: a Riverwood house doorway walked through with the door open - the frames
  `local/demo/<run>/walk-through/NN/f*` around each swap. The leaf must be visible in the doorway
  while it swings, including the frames where it used to vanish. The run's log carries one line per
  mirror (`engine::portal door=... mirror placed ...`), and that line is the cheap check that the
  mechanism ran at all.
* **the sun**: the same doorway seen from *inside* a house (a daylit street through the opening)
  against the same street walked into. The doorway image should be the same brightness as the street
  it becomes, and it should have the street's shadows in it. `tools/research/exposure_stats.py`
  measures the frames; `tools/research/portal_frame_alignment.py` measures the alignment of the
  doorway image against the room it becomes (it should stay at a zero-pixel offset, as it is today).
