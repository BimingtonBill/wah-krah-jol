# Portal performance: ideas, and which track owns them

**Status:** ideas, 2026-09-23. Collected in conversation between the user and Claude; nothing here
is measured or built yet. The first step for any of them is a profile, because no frame-time
number from 2026-09-23 is trustworthy: the machine was running builds, conversions and several
engine instances at once all day.

## Why the portal is expensive now

A load door that is open draws the room behind it with a second camera. impl-066 (2026-09-23)
made that image full window resolution (up to 2560x1440) in a 16-bit float format so it would stop
looking soft and clipped, and impl-082 gave the doorway its own sun with shadow maps. Both were
right for how it looks, and both mean an open door now costs close to a second full frame.

## Accepted - portal track

These belong to the portal work (`PortalPlugin`).

1. **Render the portal at the doorway's size, not the window's.** A doorway usually covers a small
   part of the screen. Size the portal image to the doorway's on-screen footprint, with the same
   pixel density as the main view, so it stays as sharp as impl-066 made it without drawing pixels
   nobody sees.
2. **Only render the portal when it can be seen.** Skip it when the doorway is off-screen, too
   small to matter, or the player is walking away.
3. **Cheaper shadows in the portal view.** The doorway's sun has its own shadow cascades. Reuse
   the main view's, or use fewer cascades for the portal, since it is always a smaller image.

## The user's ideas - portal track

4. **Render only what the player can see through the doorway, the way the Portal games do.**
   Restrict the portal camera to the doorway's shape: cull everything outside the cone from the
   eye through the doorway frame, clip everything between the doorway and the portal camera (an
   oblique near plane at the doorway), and only shade the pixels inside the doorway (a stencil or
   scissor). Today the portal camera draws the whole destination view and the doorway quad samples
   it. Check what it culls before starting.
5. **Close a door automatically when the player walks away, so its destination can be unloaded.**
   Doors already close (the `Closing` state in `door_animation.rs`), but only when the player
   presses `E` again. An open door keeps its destination streamed and its portal rendering, so an
   open door left behind costs memory and frame time for nothing. Close it with its own animation
   once the player is far enough away or has not looked at it for a while; then the destination can
   be unloaded. The distance should be well beyond the 800-unit prestream radius below, so a
   player turning back never sees a door close in their face.
6. **Prepare an interior before the player opens its door.** This exists in part:
   `plan_door_prestream` (`transition.rs`) starts streaming a destination when the player comes
   within 800 units of its door (`docs/design/animated-doors-and-seamless-crossing.md`). What could
   be added: start earlier or predict which door the player is heading for; warm the destination's
   shaders and GPU buffers, not just its cell data, so the first frame through the doorway does not
   hitch; and cap how many interiors are being prepared at once.

## Not portal - the upstream (Phase 2) track

**Level of detail and distant terrain.** Skyrim's own simplified distant models (the converter's
`lod` table, empty around Riverwood today), tree impostors, and a proper distant-terrain system
belong upstream, not in the portal plugin. The terrain ring that exists now is **for testing
only**; it is not the design for distant terrain.

## How we work - maybe later

Two ideas that would speed up the project's own work rather than the game:

- **A shared compile cache.** Every coding worker builds the engine in its own copy of the
  project, from a ~11 GB copy of compiled output, and a release build takes 4-5 minutes. A shared
  cache (sccache is the usual tool) would let those builds reuse each other's compiled pieces.
- **Convert only what a change affects.** When the converter's schema version goes up, every model
  is rebuilt, even if the change only touches one material field. Today's schema-19 bump rebuilt
  all 22,761 models and took about 17 minutes. The cache could track which part of the converter
  each output depends on.
