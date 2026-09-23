# Portal visuals: making the doorway pleasing, not jarring

**Status:** ideas, 2026-09-23, from a brainstorm between the user and Claude. The user approved all
of them for the record; none is built or scheduled yet. Performance ideas are separate, in
`portal-performance.md`.

**The principle:** the best portal effect is one nobody notices. Most of what follows hides seams
rather than decorating them, and the visible effects are ones the eye already expects from real
doorways.

## Hide the seams

1. **Fix what is measured as wrong.** Alignment (`docs/research/portal-door-alignment.md`, the fix
   in impl-103), the first frame after a crossing drawn with the old side's atmosphere
   (`docs/research/portal-frames.md` section 3), and the doorway's own sun (impl-082). Every
   mismatch fixed is one less effect needed to cover it.
2. **Never show an empty doorway.** Load the destination early enough that the doorway never
   flashes black or half-loaded. Ties into "prepare an interior before its door is opened" in
   `portal-performance.md`.
3. **Never let the camera clip through walls while crossing.** As the player steps through, the
   near plane can pass inside the door frame for a frame or two and show the inside of the wall.

## Borrow from real eyes

4. **Eye adaptation.** Stepping out of Alvor's House into daylight, the frame's brightness roughly
   doubles in one frame today (mean luma 39 to 84 in `local/demo/riverwood2/`). Eyes squint and
   settle; a short, gentle exposure adjustment would read as natural rather than as a glitch.
5. **Doorways that glow or darken.** From a dim room, an open door onto daylight looks blown-out
   bright - in life and in Skyrim's own screenshots. From a sunny street, a doorway into a house
   reads dark. Realistic, and it softens detail at the threshold.
6. **Light spilling through.** Daylight falling across the floor just inside a door; firelight on
   the doorstep outside. It stitches the two spaces together visually.

## Graceful failure

7. **Use the glare and shadow of (5) as camouflage** for the doors whose two sides the data cannot
   line up - about a quarter of directions (ladders, trapdoors, cave mouths; the "arrival anchor"
   tier in the alignment report). A mismatch that sits inside glare or shadow is not seen.

## Feel

8. **Door motion.** Eased swings instead of a constant mechanical speed. Later, when there is
   audio: a creak, and an ambient crossfade from street to room.
9. **Fog and haze continuity.** Interiors carry their own fog colour; exteriors have distance haze.
   Blend them across the threshold rather than switching at the crossing.

## The user's ideas

10. **Scripted things fade in gracefully.** NPCs and other scripted or streamed-in content should
    fade in rather than pop into existence. There are no NPCs yet (Phase 4,
    `docs/roadmap/04-gameplay-and-physics.md`), so this is a requirement for when they arrive -
    and it applies equally to any object that finishes streaming while in view.
11. **A vignette that covers the fade-in when entering a new space.** A brief, good-looking
    vignette - the edges of the view darkening and relaxing - as the player crosses into a new
    space, so content fading in at the edges is obscured. It fits naturally with (4): the vignette
    and the exposure settling can be one moment, "the eyes adjusting to a new room". It must stay
    subtle and short, since the crossing is otherwise meant to be seamless.
