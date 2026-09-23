# Backlog: main engine work and portal plugin work

**Status:** collected 2026-09-23 from the session's notes, the user's ideas, the reference comparison
sheets (`local/reference/cmp-103/`) and the design docs. Nothing here is scheduled; it is the list to
pick from. Each line says where its detail lives.

**The split.** *Main engine* is work every user of the engine benefits from, whether or not doors
are portals: rendering, the converter, streaming, distance, future gameplay. It belongs on the
Phase 2 track (upstream contributions, `docs/pr/two-tracks.md`) or a later upstream phase, and is
built on `phase2/*` branches first. *Portal plugin* is the seamless-door feature and its demos: it
lives on the `portal` branch, in `PortalPlugin` once the refactor lands, and is offered upstream
later as a Phase 4 proposal.

---

## Main engine

### Things that look wrong now (seen in the comparison sheets or in play)

- **Water renders as a flat white sheet** - the Riverwood river, the lake at the Guardian Stones.
  The biggest eyesore left. Hypothesis, unverified: the sky-fill ambient term on the water plane.
- **Interiors darker than Skyrim's where the light is firelight** - the Sleeping Giant's common room
  (the hearth gives almost no light), and Alftand's deep interiors. Houses lit by windows and
  sconces are now close.
- **Carved rune stones glow white** at the Guardian Stones; they should read as stone.
- **Snow is flat and lumpy** on the Alftand exteriors. Directional snow (`cfe4893`) is built but
  its calibration waits for real data.
- **Ice reads as rock.** Needs environment maps: `docs/design/environment-map-publishing.md`.
- **No weather or overcast sky.**

### Rendering features

- **Eye adaptation (auto exposure).** Brightness jumps when moving between dark and bright places.
  Engine-wide; the portal's crossing moment (below) would use it.
- **Graceful fade-in of streamed content** (the user's idea): anything that finishes streaming in
  view - and NPCs, when they exist - fades in instead of popping. Engine-wide; the portal's vignette
  (below) is built to hide it.
- **Lighting scope** - upstream issue #24 decides whether the fork's lighting work goes upstream.

### Distance and performance

- **Distant level of detail** - Skyrim's own distant terrain, objects and trees:
  `docs/design/distant-lod.md`. Step 1 (reading `.btr`/`.bto`) is running on the Phase 2 track
  (impl-514-p2). The terrain ring the portal demos use is a **testing aid only**.
- **Faster conversions** - `docs/research/incremental-conversion.md` (research-151). Two halves, each
  worth little alone: rebuild only the models a change affects (schema 16 would have rebuilt 3.7%),
  and stop repeating the work every run pays (extraction, hashing, copying, rebuilding the
  database). Handed to the Phase 2 track. Also: converter output has never been checked to be
  byte-for-byte deterministic, which the design depends on.

### Upstream contributions already written (held until #23/#24/#25 get answers)

The ordered list is `docs/pr/two-tracks.md`, "Phase 2 track": pruned textures, empty models, one
schema constant, material publication, vertex alpha, terrain samplers and seams, shadow cascades,
renderable base types, script schema. Three need a GPU run on upstream-schema assets first
(terrain, seams, shadows). Then the acceptance campaign.

### Later phases (upstream's roadmap)

- NPCs and skinned meshes, physics, combat (Phase 4, `docs/roadmap/04-gameplay-and-physics.md`).
- Scripting and UI (Phase 3), audio.

---

## Portal plugin

### Structure

- **The `PortalPlugin` refactor** - approved, planned in `docs/design/portal-plugin.md`. Portal code
  moves behind one plugin with its own options block; atmosphere and the terrain ring move to their
  own files. Next in line now that the alignment fix has landed.

### Correctness

- **Done 2026-09-23:** doorway alignment (impl-103 + impl-152; the user confirmed it on many random
  transitions), the door drawn over the doorway, a full-resolution HDR doorway, the doorway's own sun.
- **Parked (the user, 2026-09-23): non-obvious transitions** - cave mouths, ladders, trapdoors and
  the like, about a quarter of crossing directions, which keep the old landing. How these should
  behave is an intentional design decision that needs careful planning of its own, not a fix; do
  not work on them (including visual idea 7's camouflage) until that planning happens.
- **Dwemer load doors draw a closed-looking panel (`Plane02`) over the open doorway.**
- **Open question for the user:** Dwemer route doors only twitch 5-9 degrees in the real data; keep
  that, or borrow the 86-88 degree swing of their non-load twins?
- **A destination's lights can faintly reach the room you are in** (GPU light clustering ignores
  render layers; impl-015's note).
- **The first frame after a crossing uses the old side's atmosphere** (`docs/research/portal-frames.md`
  section 3).
- **The camera can clip into the door frame** for a frame while crossing.

### Performance (`docs/design/portal-performance.md`)

1. Render the doorway image at the doorway's on-screen size, not the window's.
2. Only render it when the doorway can be seen.
3. Cheaper shadows in the doorway view.
4. Render only what can be seen *through* the doorway, as the Portal games do (the user's idea).
5. Close a door automatically once the player walks well away, so its destination can unload (the
   user's idea).
6. Prepare an interior before its door is opened - shaders and GPU buffers too, not only cell data
   (the user's idea; the cell part exists).

### Look and feel (`docs/design/portal-visuals.md`)

- Never show an empty doorway; eye adaptation across the threshold; a bright doorway from a dark room
  and a dark one from a sunny street; daylight spilling across the floor; eased door swings (a creak
  and an ambient crossfade once there is audio); fog and haze blended across the threshold.
- **The vignette on entering a new space** (the user's idea): a brief, subtle darkening at the edges
  of the view that hides content fading in, and can be the same moment as the eyes adjusting.

### Test and demo tooling

- The tour's saved doorway image fails since the doorway went HDR ("Cannot save screenshot ...
  Rgba16Float").
- The tour does not catch crossing timing races (impl-076's brief is written).
- A doorway check that is not circular: compare the view through the doorway with the view from the
  same pose inside, rather than with the game's landing point.
- The door-opening reference shots are unreliable at oblique angles (Sven's House caught closed).
- The tour's walk-in distance is not doorway-aware (`demo_tour.rs`).
- Reference poses: RW-04 is inside a pine tree; RW-07's Skyrim picture is a damaged file; the inn and
  whole-village poses need one more fit; most Alftand/Blackreach poses are only approximate.

### Later

- Offer the portal work upstream as a **Phase 4 proposal**: an issue with the design and a short
  demo capture, then PRs cut from `portal`.
