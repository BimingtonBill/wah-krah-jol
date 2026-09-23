//! Portals through load doors: pre-streamed destination cells are kept off the main camera's
//! layers, only the door the portal renders through stops drawing its leaf, and the destination
//! behind that door is rendered through the doorway as a window. Every other load door draws its
//! own leaf, so a doorway the portal is not showing is a closed door and not a hole.
//!
//! # Why the isolation is needed
//!
//! `transition::plan_door_prestream` puts the destination behind every load door within
//! [`DOOR_PRESTREAM_RADIUS`] into `PrestreamCells`, and `streaming::spawn_cell` spawns those cells
//! exactly like the active ones: an interior root sits at the render origin and its references keep
//! the interior's absolute creation coordinates, while an exterior of another worldspace is placed
//! relative to the *current* [`RenderOrigin`]. Neither is where the player stands, and both are
//! drawn, so a pre-streamed cell appears as floating, misplaced geometry - and, because
//! `player::PlayerPlugin` ray-casts with `RayCastVisibility::Visible`, the player also collides
//! with it. Interiors are the worst case: while the camera is inside `Alftand01` the pre-streamed
//! Tamriel grid is placed around the render origin, which is right where the interior's own
//! references are.
//!
//! Every cell root is therefore classified each frame against the active space the streaming plan
//! uses (`streaming::cell_within_unload_radius`):
//!
//! | role | layers | visibility | seen by |
//! |---|---|---|---|
//! | [`CellRole::Active`] | its own (0, or 1 for water) | unchanged | main camera, reflection camera |
//! | [`CellRole::Destination`] | [`DESTINATION_LAYER`] | unchanged | the portal camera only |
//! | [`CellRole::Hidden`] | none | hidden | nothing |
//!
//! [`RenderLayers`] is what keeps a cell out of the main camera, which renders layers 0 and 1
//! (`app::setup_world`). Layers are read per mesh entity, not inherited, and a glTF scene spawns
//! its meshes over several frames as assets load, so the layers are re-applied to every descendant
//! of a non-active root every frame and a cell's original layers are remembered in
//! [`PortalOriginalLayers`] and restored when it becomes active. The ray cast ignores render
//! layers, so a cell that is not the portal's destination is *also* hidden with `Visibility` -
//! that one *is* inherited, so it covers meshes that appear later. The portal's destination stays
//! visible: the portal camera renders layers, and a hidden entity is skipped by every camera.
//!
//! # Doors
//!
//! A load door's own leaf is the other half of the same frame, and which frame is a doorway comes
//! from the door's state ([`crate::doors::DoorState`]): **the portal renders through an open door
//! only** (`update_portal` picks its door from the open ones), and
//! [`show_load_door_leaves`] hides a door's whole model only when there is no leaf that could be
//! drawn over the quad - a door with no animation of its own, and an auto-load marker, which has no
//! leaf at all. An **animated** door keeps its model and its leaves are
//! [`crate::door_animation`]'s: drawn while the `Open` clip swings them, hidden once it is open if
//! the swing did not clear the opening. `update_portal` publishes the door it picked in
//! [`PortalState`] and `show_load_door_leaves` puts that on the door roots in the same frame, so a
//! doorway is an opening with the destination in it or a closed door, and never a window drawn
//! over a leaf. Retargeting the portal swaps the two between one frame and the next.
//!
//! A leaf that swings **past** the doorway's plane, away from the player, is behind the quad and
//! covered by it - the one case the rule above cannot draw, because the door is source-space
//! geometry and the window is a piece of the destination's image. [`mirror_door_nodes`] draws that
//! leaf for real: a second instance of the door's own model, on the portal camera's layer, at the
//! *mapped* pose of the door - the same rigid map the portal camera itself is placed by
//! ([`portal_pose`]) - with each drawn node carrying the local transform of the node it is the
//! second instance of. The quad samples the destination image by screen position, so a point at
//! `M(P)` is drawn on the pixel `P` is: the mirror is the door, seen through the doorway, standing
//! where the door is, with the door's own swing on it. Drawn *in* the destination image, it is
//! depth-tested against the destination's own geometry like everything else there, and it can only
//! appear inside the opening, because the quad is only visible where no nearer source geometry
//! covers it. What it draws is the part of the model the door's own animation moves - the leaf -
//! and not the model's static filler (see that system's note on the game's black plugs).
//!
//! Which side of a door is its front comes from the door's own link data rather than from its
//! model: [`door_frame`](crate::transition::door_frame) builds the frame the view is mapped through
//! from the door's outward direction, which the database reads off the link that leads back into
//! the door. Door models disagree about which of their own axes is their front (see
//! [`LoadDoor::outward`]), so the model's frame is only the fallback for a door nothing leads back
//! to. The doorway itself is the model's geometry, so its box is measured in the model's own frame
//! and then laid out in the front frame - the same one the view is mapped through - so that the
//! window covers the opening a player walks in through rather than standing edge-on to it
//! ([`doorway_in_frame`]). That frame, the door -> arrival map built on it and the distance in
//! front of the door are all defined in `crate::transition`, which the player's crossing (and the
//! boundary between one cell and the next) uses too: the portal and the crossing must agree on
//! where the destination is, or the swap at the doorway shows something else.
//!
//! A door the data supports carries a [`DoorAnchor`] ([`crate::doors::doorway_anchor`]), and the
//! *one* map `crate::transition`'s [`door_map`] builds from it is what places every part of this
//! module at once: the portal camera, the clip plane, the quad's frame and size, the doorway mirror
//! and the destination cells. The map pivots on the source **doorway's centre** and lands on the
//! destination **doorway's centre**, so the quad stands in the real doorway (not in a frame 11.6
//! degrees oblique to it) and the clip plane is the destination doorway's own plane - which is why
//! [`PortalState::destination_door`]'s model has to be hidden while the window is up. A door
//! without an anchor draws exactly what it always did.
//!
//! # The doorway image
//!
//! The window is a render target of the portal camera, sampled by the quad at the screen position
//! of each of its fragments, and it is made to be indistinguishable from the room behind it:
//!
//! * it is the size of the main camera's own target ([`resize_portal_target`], so the doorway has
//!   the same pixel density as the room around it, resize for resize);
//! * it carries the destination's scene-referred light rather than a clipped 8-bit copy of it
//!   ([`PORTAL_TEXTURE_FORMAT`]), so the main camera's tonemapper and bloom finish the doorway
//!   exactly as they finish the room - walked through, the same surface reads the same;
//! * it is lit and fogged by the *destination's* own records ([`update_destination_atmosphere`])
//!   rather than by the space the player is standing in - ambient, fog, clear colour and a sun,
//!   which is a `DirectionalLight` of its own on the portal camera's layer
//!   ([`PortalDestinationSun`]), because Bevy lights a view from the lights whose render layers
//!   meet that camera's and the engine's one sun carries no layers at all.
//!
//! Those are the *camera's* alone: the quad's material is unlit and does not tonemap what it
//! samples, because the frame it is composited into is tonemapped once, by the main camera.
//!
//! # Wiring
//!
//! `app.run` adds `PortalPlugin` once, for every run that opened the world, right after
//! `StreamingPlugin` (it needs `ActiveCell`, `EngineConfig`, `RenderOrigin` and `StreamingWorld`).
//! Everything the portal feature is comes through that one call:
//!
//! * the crossing ([`crate::transition::TransitionPlugin`]) in every such run - crossing a load door
//!   is not a run mode, and a benchmark run's schedule depends on it;
//! * the player ([`crate::player::PlayerPlugin`]) in a walked run, the demo tour
//!   ([`crate::demo_tour::DemoTourPlugin`]) and the shots run ([`crate::shots::ShotsPlugin`]) in the
//!   runs that asked for them;
//! * the doorway image, the door and model animation ([`crate::door_animation`],
//!   [`crate::model_animation`]) and the cell isolation in the runs that are looked at rather than
//!   measured.
//!
//! Which of those a run gets is decided here, from `EngineConfig` alone
//! ([`EngineConfig::interactive`](crate::config::EngineConfig::interactive)), so the portal block in
//! `app.rs` is one line and this plugin holds the rest. Nothing else about the rendering changes:
//! the destination cells are moved off the main camera's layers rather than the camera being granted
//! a new one, and the portal camera, its render target and the quad are all spawned here.
//!
//! `streaming::spawn_cell` inserts [`StreamedCellKey`] on the root it returns:
//!
//! ```ignore
//! root_commands.insert(crate::portal::StreamedCellKey(payload.key));
//! ```
//!
//! That is what makes the isolation exact. Without it a root's worldspace is not recoverable from
//! `CellRef` and `ExteriorCellGrid` alone, so two exterior cells at the same grid in different
//! worldspaces are told apart only by the active-space radius; the demo route does not collide
//! (Blackreach arrives at grid 5,4 while AlftandWorld has no cell there).
//!
//! # What is not done here
//!
//! One portal at a time (the nearest door). The destination view's lighting is per destination -
//! ambient, fog and clear colour on the portal camera, and a sun of its own on the portal camera's
//! layer (the engine's sun reaches no view but the main camera's and the reflection camera's, and
//! never reached this one) - but it is the *space's* sun, not the part of the destination that
//! stands in it: the direction is the engine sun's, and only the tint and the illuminance are the
//! destination's. Water surfaces of a pre-streamed cell sample the main camera's reflection
//! texture, which does not show the destination.

use crate::{
    config::EngineConfig,
    doors::{DoorAnchor, DoorDestination, DoorLeaf, DoorState, LoadDoor},
    streaming::{ActiveCell, RenderOrigin, StreamingWorld},
    transition::{
        CrossingHeld, DOOR_PRESTREAM_RADIUS, DoorMap, destination_is_resident, destination_keys,
        distance_in_front_of_door, door_is_open, door_map,
    },
    world::{
        components::{
            CELL_SIZE, CellRef, ExpectedModelBounds, ExteriorCellGrid, InstanceBounds,
            StreamedCellRoot, StreamingCamera,
        },
        database::CellKey,
        lighting::{SpaceKey, SpaceLightingCatalog, space_key},
    },
};
use bevy::{
    animation::AnimationTargetId,
    app::AnimationSystems,
    asset::embedded_asset,
    camera::{ClearColorConfig, RenderTarget, visibility::RenderLayers},
    core_pipeline::{prepass::DepthPrepass, tonemapping::Tonemapping},
    light::CascadeShadowConfig,
    pbr::{ExtendedMaterial, MaterialExtension, MaterialPlugin},
    prelude::*,
    render::{
        occlusion_culling::OcclusionCulling,
        render_resource::{AsBindGroup, TextureFormat},
    },
    shader::ShaderRef,
    world_serialization::WorldAssetRoot,
};
use std::{
    collections::{HashMap, HashSet},
    f32::consts::PI,
};

/// The rendering layer pre-streamed destination cells are moved to. The main camera renders layers
/// 0 and 1 and the portal camera renders this one, so a destination is drawn in exactly one of the
/// two views.
pub const DESTINATION_LAYER: usize = 2;

/// The layers `app::setup_world` gives the main camera. A cell that belongs to the active space
/// has to end up on one of these.
#[cfg(test)]
const MAIN_CAMERA_LAYERS: [usize; 2] = [0, 1];

/// The portal camera renders before the main camera that samples its target, like the water
/// reflection camera one step later.
const PORTAL_CAMERA_ORDER: isize = -2;

/// The layer of the doorway quad. The main camera renders layers 0 and 1, and the water reflection
/// camera renders only layer 0, so a doorway quad on layer 1 - the layer the water surfaces are on
/// for the same reason - is in the view and not in the reflection of it.
const PORTAL_QUAD_LAYER: usize = 1;

/// The size the portal render target is built with before the main camera's own target size is
/// known.
///
/// [`resize_portal_target`] takes up the main camera's target size on the first frame
/// `camera_system` has computed it, which is the frame after the camera exists, and follows every
/// resize after that. This is what the target is until then, and what it keeps in a run with no
/// window at all (a headless run has no target info to read): the portal camera draws nothing
/// until a door is open, and no door is open in the frame a run starts in.
const PORTAL_TEXTURE_FALLBACK_SIZE: UVec2 = UVec2::new(1024, 576);

/// The largest the portal target may get, on either axis: the doorway image is drawn one for one
/// with the window's pixels up to this, and scaled down past it.
///
/// A doorway is one piece of the frame and the room around it is drawn at the main camera's own
/// target size, so matching that size is what makes the two indistinguishable - and a target
/// larger than the view cannot show more than one pixel per pixel of the view. The ceiling is what
/// stops a window far larger than any of the engine's own runs from allocating an absurd texture:
/// a maximised 8K display is 7680x4320, and at `Rgba16Float` that would be 265 MB for the target
/// plus the same again for the texture the camera renders into, for a doorway that covers a
/// fraction of the screen. 2560x1440 is 29.5 MB. Both axes take one factor, so a doorway keeps the
/// window's pixel aspect ratio instead of being stretched along an axis.
const PORTAL_TEXTURE_MAX_SIZE: UVec2 = UVec2::new(2560, 1440);

/// What the portal camera renders into, and so what the doorway quad samples.
///
/// **Float, not 8-bit.** `Rgba16Float` holds the values the destination's own shaders produced,
/// above white included. An 8-bit target clamps every value over 1.0 to 1.0 as the portal camera
/// writes it, and the main camera's tonemapper then maps all of them - a sunlit wall, a light
/// pool, a glow - to one flat value: the doorway reads as a washed-out page next to the same room
/// walked into, because the values that should have rolled off the top of the tonemapper's curve
/// arrived at its ceiling instead. The portal camera is still not tonemapped and still has no
/// bloom of its own: what it writes is the destination's scene-referred light, and the *main*
/// camera's tonemapper and bloom finish the doorway exactly as they finish the room around it.
///
/// **No second view format.** An sRGB view would encode the values on write and decode them on
/// sample - a round trip through 8-bit precision, which is the thing this format is here to avoid.
/// A float texture's view format *is* its format.
const PORTAL_TEXTURE_FORMAT: TextureFormat = TextureFormat::Rgba16Float;

/// The doorway a door without converted bounds gets, in Creation units.
const DEFAULT_PORTAL_SIZE: Vec2 = Vec2::new(200.0, 300.0);

/// A doorway smaller than this on either axis is treated as a missing measurement.
const MIN_PORTAL_SIZE: f32 = 8.0;

/// How far in front of the doorway's own plane the quad sits. Zero: the doorway's plane is where
/// the window is exactly the size of the opening, and where the crossing fires
/// (`crate::player::player_walks_through_doors`), so the last frame with a window is the swap.
///
/// A standoff used to hold the quad eight units forward - over-covering the opening, with the wall
/// hiding the excess - and it left the doorway blank for the last ~16 units of the walk: the window
/// runs from the quad toward the eye, so a camera closer than the quad has walked past it, and the
/// destination is drawn nowhere else. The walk-through frames bracket it: a doorway that is black
/// over 17.9 to 8.4 units at eight, and never at zero.
const PORTAL_QUAD_OFFSET: f32 = 0.0;

/// A camera nearer than this to a door's plane - inside it, or behind it - has no portal through
/// it: the doorway's clip plane would pass through the eye, where the window has no content.
///
/// The window is what carries the destination, so the frame this drops in is also the last frame
/// the player may still be on the near side of the doorway without the swap having been made:
/// [`crate::player::player_walks_through_doors`] fires the crossing at this same distance in front
/// of the plane rather than a step later, which is the one step that would otherwise show neither
/// the window nor the destination (a black frame in the doorway).
pub(crate) const MIN_PORTAL_DOOR_DISTANCE: f32 = 1.0;

/// Registers the crossing, the portal shader, and the systems that isolate cells, close load doors
/// and render the destination through the nearest doorway.
///
/// Add it for every run that opened the world, after
/// [`StreamingPlugin`](crate::streaming::StreamingPlugin): the crossing is not a run mode - every
/// run that streams cells crosses load doors - and the rest of it is registered from here by asking
/// the engine's own configuration what sort of run this is
/// ([`EngineConfig::interactive`](crate::config::EngineConfig::interactive)). It belongs to one
/// `add_plugins` call, so that a merge from `main` has one portal line to keep.
pub struct PortalPlugin;

/// The portal's own frame, in order: the door it renders through, the doorway's mirror, the
/// destination's sun and atmosphere, the leaves of the doors it draws, and the isolation of the
/// cells.
///
/// One set, because the whole of it has to be right for the frame it is a picture of: the frame's
/// crossing is applied before it ([`crate::transition::DoorTransition`]), and so is a door state a
/// crossing ends in - `crate::door_animation`'s arrival opening, which is what the far door of a
/// mapped crossing is drawn as in the frame the player arrives in it.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortalFrame;

impl Plugin for PortalPlugin {
    fn build(&self, app: &mut App) {
        // The crossing and the pre-stream plan: every run that opened the world, which is what
        // `StreamingPlugin` adding this used to mean. A benchmark run keeps that part of its
        // schedule, and a fixture run does not gain it.
        app.add_plugins(crate::transition::TransitionPlugin);
        // The sort of run this is, read from the configuration: a pure function of it, so this
        // plugin and `app::run` cannot disagree (H7). The shots run is a value rather than a flag -
        // `app::run` built it before the window existed, because it sizes that window (H3) - and it
        // is handed over as a resource.
        let (interactive, walking, demo_tour, shots) = {
            let config = app.world().resource::<EngineConfig>();
            (
                config.interactive(),
                config.walks(),
                config.portal.demo_tour.clone(),
                app.world().get_resource::<crate::shots::ShotsRun>().cloned(),
            )
        };
        if walking {
            // The player drives the `StreamingCamera` itself. Every other run keeps the scripted
            // `fly_camera`, which `app::run` registers (H4).
            app.add_plugins(crate::player::PlayerPlugin);
        }
        if interactive {
            // The demo's scripted tour and the objective line a walked demo shows: the plugin adds
            // whichever of the two this run has.
            app.add_plugins(crate::demo_tour::DemoTourPlugin {
                output_dir: demo_tour,
            });
        }
        if let Some(run) = shots {
            app.add_plugins(crate::shots::ShotsPlugin { run });
        }
        if !interactive {
            return;
        }
        embedded_asset!(app, "shaders/portal.wgsl");
        app.add_plugins(MaterialPlugin::<PortalMaterial>::default())
            .init_resource::<PortalState>()
            // The quad reads PortalTexture, which the camera setup inserts through commands:
            // chaining adds the sync point that applies them in between.
            .add_systems(Startup, (setup_portal_camera, setup_portal_quad).chain())
            .add_systems(
                Update,
                (
                    // Ahead of the portal: a resize repoints the camera's target and the quad's
                    // material at one new image, and the frame that follows has to be the one that
                    // renders into it, or the doorway shows a frame of the old size stretched.
                    resize_portal_target,
                    // The portal picks its door from the roles of the previous frame and publishes
                    // the destination cells; the isolation below reveals them in this same frame,
                    // which is what the roles would otherwise need the next frame for.
                    update_portal,
                    // After it, so the doorway's mirror is the door the portal picked this frame,
                    // in the frame the quad stands in its doorway.
                    place_door_mirror,
                    // The doorway's own sun onto the engine sun's direction. Here rather than in
                    // `PostUpdate`: it is a light, and the light extraction of this frame is the
                    // frame it should be right for.
                    place_destination_sun,
                    // After it, so the doorway is drawn with the atmosphere of the space it is
                    // looking into in the same frame the door is picked.
                    update_destination_atmosphere,
                    // After it, so the leaf of the door the portal just picked is gone in the same
                    // frame as the quad that replaces it - and one frame after it is dropped, the
                    // leaf is back. Before the isolation, which is about cells rather than doors.
                    show_load_door_leaves,
                    isolate_cells,
                )
                    .chain()
                    .in_set(PortalFrame)
                    // A crossing changes `ActiveCell` in this set, and the cell just entered has to
                    // be visible in the frame it is entered in.
                    .after(crate::transition::DoorTransition),
            )
            // The mirror's pose *is* the door's, so it is copied after the animation has advanced
            // the door's nodes and before the transforms propagate: an animation system that ran
            // after this one would leave the mirror a frame behind the door it is a second
            // instance of. `PostUpdate` is where both of those run.
            .add_systems(
                PostUpdate,
                mirror_door_nodes
                    .after(AnimationSystems)
                    .before(TransformSystems::Propagate),
            );
        // The state of every load door - opened by `E`, swung by the model's own `Open` clip, and
        // read back by the portal and the crossing - and a model's own looping `Idle` clip on every
        // reference that is not a door: the mill wheel the Riverwood finale looks across the river
        // at, and the dust on a log pile. Both interactive runs only, and each needs nothing but
        // the asset server.
        app.add_plugins(crate::door_animation::DoorAnimationPlugin);
        app.add_plugins(crate::model_animation::ModelAnimationPlugin);
    }
}

/// The cell a reference root belongs to, carried by the root `streaming::spawn_cell` returns.
///
/// The streaming side inserts this; without it the isolation falls back to matching a root's
/// `CellRef` (interiors, which have unique cell ids) and its `ExteriorCellGrid` (exteriors, where
/// the worldspace is not recoverable).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamedCellKey(pub CellKey);

/// The render target the portal camera draws into. Public so a tool can inspect it.
#[derive(Resource, Clone)]
pub struct PortalTexture(pub Handle<Image>);

/// The second camera that renders a destination cell.
#[derive(Component)]
struct PortalCamera;

/// The sun the doorway image is lit by: a second [`DirectionalLight`], on the portal camera's
/// layer, carrying the destination space's own sun ([`update_destination_atmosphere`]) in the
/// engine sun's direction ([`place_destination_sun`]).
///
/// **Why the engine's sun cannot light this view.** Bevy builds each view's directional lights by
/// intersecting the light's own `RenderLayers` with the *camera's*
/// (`bevy_pbr-0.19.0/src/render/light.rs:1642-1654`; `ExtractedDirectionalLight::render_layers` is
/// the light entity's component, `:124`, `:815`). The engine's one sun (`app::setup_world`) is
/// spawned with no `RenderLayers` at all, which is layer 0, and the portal camera is
/// `RenderLayers::layer(DESTINATION_LAYER)`, layer 2 - so before this entity existed the doorway
/// was lit by no directional light whatever space stood behind it: a daylit Riverwood street seen
/// from inside a house was drawn with ambient and fog alone.
///
/// **Why this is not a change to the engine's sun.** The main camera (layers 0 and 1) and the water
/// reflection camera (layer 0) never see this light, and this light never sees the engine's - one
/// directional light per view either way, so `MAX_DIRECTIONAL_LIGHTS` is untouched, and the shadow
/// cascades are budgeted per *view*, which is why the doorway can have a cascade set of its own
/// without taking one from the main camera (`sun_shadow_cascades`'s note in `app.rs` says the same).
#[derive(Component)]
struct PortalDestinationSun;

/// The doorway's mirror of the door the portal is rendering through: a second instance of the
/// door's own model, drawn in the destination image only ([`mirror_door_nodes`]).
///
/// Spawned for the door in [`PortalState::open_door`] that has an animation of its own, and for no
/// other door - see [`place_door_mirror`]. It stands where the door's own model cannot be drawn: on
/// the far side of the doorway's plane, where the quad covers it.
#[derive(Component, Debug, Clone, Copy)]
struct PortalDoorMirror {
    /// The load door this is the second instance of: the entity carrying [`LoadDoor`] and
    /// [`DoorState`], whose own model is the source of every pose copied onto the mirror.
    door: Entity,
    /// Whether the one-per-mirror log line has been written. What it counts - the nodes and meshes
    /// the scene spawned - is only known a frame or more after the entity is made.
    logged: bool,
}

/// The quad in the doorway that shows the portal camera's image: a window, not a wall.
///
/// The walking player must be able to pass it - the doorway it stands in is the way through, and
/// the quad is the image of the room on the far side - so [`crate::player`]'s walk probe skips it
/// exactly as it skips a water surface. The geometry behind it (the wall the door is set into, the
/// leaf of a door that is still closed) is what stops them.
#[derive(Component)]
pub(crate) struct PortalQuad;

/// The material of the doorway quad: it samples the portal render target by screen position, so
/// the doorway is a window rather than a picture.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
pub struct PortalExtension {
    #[texture(100)]
    #[sampler(101)]
    portal_texture: Option<Handle<Image>>,
}

impl MaterialExtension for PortalExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/portal.wgsl".into()
    }
}

/// The quad's material: the portal image, unlit, sampled through screen-space UVs.
pub type PortalMaterial = ExtendedMaterial<StandardMaterial, PortalExtension>;

/// What a cell root is to the view this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellRole {
    /// Part of the active space: drawn, and collided with.
    Active,
    /// The pre-streamed destination the portal renders through: drawn by the portal camera only.
    Destination,
    /// Any other resident cell: drawn by no camera, and not collided with.
    Hidden,
}

/// The roles of the current frame, the destination cells of the current portal, and the door it is
/// rendering through.
#[derive(Resource, Default)]
struct PortalState {
    roles: HashMap<Entity, CellRole>,
    destination: Vec<CellKey>,
    /// The door the portal is rendering through: the quad stands in its doorway. A door with no
    /// animation of its own has no leaf that can be swung out of the way, so this is what hides its
    /// whole model ([`show_load_door_leaves`]); an animated door keeps its frame, and its leaves
    /// are [`crate::door_animation`]'s to draw or hide.
    ///
    /// `None` whenever no portal is up - no camera to place, no open door in range, or a run
    /// without the portal at all - which is when every load door draws its own leaf.
    open_door: Option<Entity>,
    /// The **destination** door of the doorway the portal is drawing through: the reference the
    /// open door's link lands at, when it is spawned in one of the resident cells.
    ///
    /// Its model is hidden while the portal renders through the pair, and for one reason: under a
    /// doorway anchor the window's clip plane is the *destination doorway's* own plane, so the
    /// destination door - which stands in exactly that plane, with its own leaf closed until the
    /// player opens it from the far side - would be drawn across the whole aperture, a closed door
    /// where the room should be. Today's map hides that leaf by accident, because the clip plane
    /// stands tens of units inside the room and the destination door is inside the clipped slab;
    /// moving the clip to the doorway brings it back, and this is the deliberate answer to it
    /// (`docs/research/portal-door-alignment.md` section 9.3). Nothing is hidden on a door whose
    /// map did not move - its destination door is clipped away as it always was.
    destination_door: Option<Entity>,
}

/// The layers an entity had before the isolation moved it off the main camera's.
#[derive(Component, Clone, PartialEq)]
struct PortalOriginalLayers(RenderLayers);

/// The visibility a cell root had before the isolation hid it.
#[derive(Component, Clone, Copy)]
struct PortalHiddenCell(Visibility);

/// The active space: the cells the streaming plan holds because the camera is in them, defined
/// exactly as `streaming::cell_within_unload_radius` defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveSpace {
    interior: Option<u32>,
    worldspace_id: u32,
    /// The camera's grid in `worldspace_id`.
    center: IVec2,
    radius: i32,
}

impl ActiveSpace {
    /// The camera's grid, from its render-space position and the render origin, as
    /// `streaming::plan_cells` computes it.
    fn camera_grid(camera: Vec3, origin: IVec2) -> IVec2 {
        let global_x = camera.x + origin.x as f32 * CELL_SIZE;
        let global_y = -camera.z + origin.y as f32 * CELL_SIZE;
        IVec2::new(
            (global_x / CELL_SIZE).floor() as i32,
            (global_y / CELL_SIZE).floor() as i32,
        )
    }

    fn of(active: &ActiveCell, radius: i32, camera: Vec3, origin: IVec2) -> Self {
        Self {
            interior: active.interior,
            worldspace_id: active.worldspace_id,
            center: Self::camera_grid(camera, origin),
            radius,
        }
    }

    fn contains(&self, key: CellKey) -> bool {
        match (key, self.interior) {
            (CellKey::Interior(cell_id), Some(interior)) => cell_id == interior,
            (CellKey::Interior(_), None) => false,
            (CellKey::Exterior { .. }, Some(_)) => false,
            (
                CellKey::Exterior {
                    worldspace_id,
                    grid_x,
                    grid_y,
                },
                None,
            ) => {
                worldspace_id == self.worldspace_id
                    && (grid_x - self.center.x).abs() <= self.radius
                    && (grid_y - self.center.y).abs() <= self.radius
            }
        }
    }
}

/// What a cell root says about which cell it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellIdentity {
    /// The root carries [`StreamedCellKey`], so the cell is known exactly.
    Key(CellKey),
    /// A root without the marker and without an [`ExteriorCellGrid`] is an interior, and interior
    /// cell ids are unique.
    InteriorCell(u32),
    /// A root without the marker is an exterior whose worldspace is not in the components it
    /// carries; only its grid is known.
    ExteriorGrid(IVec2),
}

impl CellIdentity {
    fn of(cell_id: u32, grid: Option<&ExteriorCellGrid>, key: Option<&StreamedCellKey>) -> Self {
        match (key, grid) {
            (Some(key), _) => Self::Key(key.0),
            (None, Some(grid)) => Self::ExteriorGrid(grid.0),
            (None, None) => Self::InteriorCell(cell_id),
        }
    }

    fn is_active(self, space: &ActiveSpace) -> bool {
        match self {
            Self::Key(key) => space.contains(key),
            Self::InteriorCell(cell_id) => space.interior == Some(cell_id),
            Self::ExteriorGrid(grid) => {
                space.interior.is_none()
                    && (grid.x - space.center.x).abs() <= space.radius
                    && (grid.y - space.center.y).abs() <= space.radius
            }
        }
    }

    fn is_destination(self, destination: &[CellKey]) -> bool {
        destination.iter().any(|key| match (self, *key) {
            (Self::Key(identity), key) => identity == key,
            (Self::InteriorCell(cell_id), CellKey::Interior(key)) => cell_id == key,
            // Without the marker an exterior root's worldspace is unknown, so the grid decides.
            (Self::ExteriorGrid(grid), CellKey::Exterior { grid_x, grid_y, .. }) => {
                grid == IVec2::new(grid_x, grid_y)
            }
            _ => false,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The doorway quad, clip plane and projection
// ---------------------------------------------------------------------------------------------

/// The `near_clip_plane` of the portal camera's projection, in its own view space.
///
/// The plane is the destination's side of the doorway: it passes through the destination doorway
/// (the link's `XTEL` arrival point, or the destination *doorway's* own centre under a
/// [`DoorAnchor`]) with that doorway's facing as its normal, which is the image of the source
/// doorway's plane under the mapping - exactly so under an anchor, where the map takes one doorway
/// onto the other, and the point of the whole change: on today's map the plane stands tens of units
/// inside the room and starts the visible destination there.
/// Bevy clips everything on the camera's side of it (`PerspectiveProjection::near_clip_plane`), so
/// the window shows the destination room and not the destination geometry the portal camera stands
/// among.
///
/// `w` is the *negative* distance from the camera to the plane along the normal, the sign
/// `PerspectiveProjection` expects - its default near plane is `(0, 0, -1, -near)`. `-w` is
/// therefore the distance in front of the doorway, and negative behind it.
fn doorway_clip_plane(
    portal_translation: Vec3,
    portal_rotation: Quat,
    doorway_point: Vec3,
    doorway_normal: Vec3,
) -> Vec4 {
    let view_from_world = portal_rotation.inverse();
    let normal = (view_from_world * doorway_normal).normalize();
    let distance = normal.dot(view_from_world * (doorway_point - portal_translation));
    normal.extend(-distance)
}

/// The portal camera's projection: the main camera's, clipped at the doorway.
///
/// Both the oblique plane and the near plane are put at the doorway. The oblique plane is the exact
/// one - it cuts the frustum along the doorway rather than perpendicular to the view axis - but
/// `PerspectiveProjection::adjust_perspective_matrix_for_clip_plane` skips the adjustment when the
/// plane's *normal* is exactly the view axis' `-Z`, which is exactly a camera looking straight at a
/// doorway. The near plane covers that case (and agrees with the oblique one everywhere else), so
/// either way the destination geometry between the portal camera and the doorway is clipped.
///
/// Neither touches the clip-space `w` or the projected `x`/`y` of anything beyond the doorway, so a
/// fragment of the quad samples the pixel of the destination on the same sight line and the doorway
/// lines up with the main view. A projection that is not perspective is passed through unchanged:
/// there is nothing to align, and screen-space UVs still line it up.
fn portal_projection(main: &Projection, clip_plane: Vec4, doorway_distance: f32) -> Projection {
    match main {
        Projection::Perspective(perspective) => Projection::Perspective(PerspectiveProjection {
            near: perspective.near.max(doorway_distance),
            near_clip_plane: clip_plane,
            ..*perspective
        }),
        other => other.clone(),
    }
}

/// The doorway the quad covers: its size and its centre in the frame the door's front comes from
/// ([`door_frame`]).
///
/// The box itself is measured in the model's own frame ([`measured_doorway_box`]) and then laid
/// out in the frame the door faces, so the quad is one thing in one frame - the door's front
/// decides where it stands, which way it faces and how big it is ([`doorway_in_frame`]). A door
/// with no usable bounds at all (the invisible `AutoLoadDoor01` markers among them) gets
/// [`DEFAULT_PORTAL_SIZE`] standing on the reference's origin.
fn portal_quad_extents(
    instance_bounds: Option<&InstanceBounds>,
    expected_bounds: Option<&ExpectedModelBounds>,
    door_rotation: Quat,
    frame: Quat,
    scale: Vec3,
) -> (Vec2, Vec3) {
    match measured_doorway_box(instance_bounds, expected_bounds, door_rotation, scale) {
        Some((min, max)) => doorway_in_frame(min, max, door_rotation, frame),
        None => (
            DEFAULT_PORTAL_SIZE,
            Vec3::new(0.0, DEFAULT_PORTAL_SIZE.y * 0.5, 0.0),
        ),
    }
}

/// The doorway box as the frame the door's front comes from sees it: the size of the opening in
/// that frame's own plane, and the offset of the doorway's centre in that frame.
///
/// The doorway is the model's geometry, so its box is measured in the model's own axes; the side a
/// player walks in from is the door's *front*, which comes from the link data and disagrees with
/// the model's own axes for most doors ([`LoadDoor::outward`]). The box is therefore placed in
/// world space with the reference's rotation and read back in the frame's axes. That gives the
/// silhouette of the whole box - not just of the model's own `X`/`Y` face - because where the two
/// frames disagree by a quarter turn the face a player walks through is the box's `X`/`Z` side,
/// and a window measured off the model's face alone would stand edge-on to the doorway.
///
/// The offset keeps the box centre's depth in the frame: the plane the quad stands in is the one
/// through the doorway's centre, which is the plane [`crate::player::player_walks_through_doors`]
/// fires the crossing on. With one frame for both ([`door_frame`] returning the model's own
/// rotation, which is every door nothing leads back to) this is the box centre exactly, and the
/// quad is placed as it was before the front came from the link data.
fn doorway_in_frame(min: Vec3, max: Vec3, door_rotation: Quat, frame: Quat) -> (Vec2, Vec3) {
    let to_frame = frame.inverse() * door_rotation;
    let mut low = Vec2::splat(f32::INFINITY);
    let mut high = Vec2::splat(f32::NEG_INFINITY);
    for x in [min.x, max.x] {
        for y in [min.y, max.y] {
            for z in [min.z, max.z] {
                let corner = (to_frame * Vec3::new(x, y, z)).truncate();
                low = low.min(corner);
                high = high.max(corner);
            }
        }
    }
    let depth = (to_frame * ((min + max) * 0.5)).z;
    (
        high - low,
        Vec3::new((low.x + high.x) * 0.5, (low.y + high.y) * 0.5, depth),
    )
}

/// The doorway's box in the door's own frame - the corner the model's bounds reach in each axis -
/// or `None` when the base has no usable bounds at all, the invisible `AutoLoadDoor01` markers
/// among them.
///
/// This is the one place that decides how big a door's doorway is, from the same two sources: the
/// converted model's bounds (model space, scaled by the reference) and, failing those, the placed
/// reference's [`InstanceBounds`] turned back into the door's frame. The auto-load trigger volume
/// in [`crate::player`] measures its opening with this function too
/// ([`measured_portal_extents`]), so "walking into the door" and "looking through it" are the same
/// doorway, and a door with no usable bounds gets no box and no volume of its own.
fn measured_doorway_box(
    instance_bounds: Option<&InstanceBounds>,
    expected_bounds: Option<&ExpectedModelBounds>,
    door_rotation: Quat,
    scale: Vec3,
) -> Option<(Vec3, Vec3)> {
    let measured = match (expected_bounds, instance_bounds) {
        (Some(bounds), _) => {
            let scaled = |v: Vec3| Vec3::new(v.x * scale.x, v.y * scale.y, v.z * scale.z);
            Some((scaled(bounds.min), scaled(bounds.max)))
        }
        (None, Some(bounds)) => {
            let inverse = door_rotation.inverse();
            let mut min = Vec3::splat(f32::INFINITY);
            let mut max = Vec3::splat(f32::NEG_INFINITY);
            for x in [bounds.min.x, bounds.max.x] {
                for y in [bounds.min.y, bounds.max.y] {
                    for z in [bounds.min.z, bounds.max.z] {
                        let corner = inverse * Vec3::new(x, y, z);
                        min = min.min(corner);
                        max = max.max(corner);
                    }
                }
            }
            Some((min, max))
        }
        (None, None) => None,
    };
    let (min, max) = measured?;
    // A box the wrong way round measures the same doorway as its own mirror image.
    let (min, max) = (min.min(max), min.max(max));
    (min.is_finite()
        && max.is_finite()
        && ((min + max) * 0.5).is_finite()
        && max.x - min.x >= MIN_PORTAL_SIZE
        && max.y - min.y >= MIN_PORTAL_SIZE)
        .then_some((min, max))
}

/// The doorway's size and centre in the door's own frame, or `None` when the base has no usable
/// bounds at all - the invisible `AutoLoadDoor01` markers among them, which is why their doorway
/// falls back to [`DEFAULT_PORTAL_SIZE`].
///
/// This is the model's own view of the box [`measured_doorway_box`] measures: the width and height
/// of its `X`/`Y` face and its centre. The auto-load trigger volume in [`crate::player`] builds
/// itself out of this, so a door's crossing volume is the model's doorway; the portal's window is
/// the same box seen in the frame the door faces ([`portal_quad_extents`]), which is the model's
/// own frame for a door whose link data agrees with its model.
pub(crate) fn measured_portal_extents(
    instance_bounds: Option<&InstanceBounds>,
    expected_bounds: Option<&ExpectedModelBounds>,
    door_rotation: Quat,
    scale: Vec3,
) -> Option<(Vec2, Vec3)> {
    let (min, max) = measured_doorway_box(instance_bounds, expected_bounds, door_rotation, scale)?;
    Some((Vec2::new(max.x - min.x, max.y - min.y), (min + max) * 0.5))
}

/// The door the portal renders through: the nearest *open* one in the active space whose
/// destination is resident and not itself part of the active space, and whose plane the camera is
/// on the front side of (a [`distance_in_front_of_door`] of at least
/// [`MIN_PORTAL_DOOR_DISTANCE`]).
///
/// Open is the gate that makes the window a doorway: the quad stands in the only opening a door
/// has, so rendering through a closed one would put the destination image behind the door's own
/// leaf, and on an animated door - whose leaves stay drawn while it swings - nothing would be seen
/// at all. A door with no [`DoorState`] at all counts as closed, like everywhere else.
fn select_portal_door<'a>(
    camera: Vec3,
    doors: impl IntoIterator<
        Item = (
            Entity,
            Vec3,
            &'a LoadDoor,
            Option<&'a DoorState>,
            Option<&'a DoorAnchor>,
        ),
    >,
    destination_is_resident: impl Fn(&DoorDestination, Option<&DoorAnchor>) -> bool,
    active: &ActiveSpace,
    distance_in_front: impl Fn(Entity) -> f32,
) -> Option<Entity> {
    let mut best: Option<(Entity, f32)> = None;
    for (entity, position, door, state, anchor) in doors {
        let distance = position.distance(camera);
        if distance > DOOR_PRESTREAM_RADIUS {
            continue;
        }
        if !door_is_open(state) {
            continue;
        }
        if !destination_is_resident(&door.destination, anchor) {
            continue;
        }
        // The destination is already drawn in the main view: nothing to look into. Under an anchor
        // the destination is the cell of the destination *reference*, which is what the crossing
        // lands in.
        if destination_keys(&door.destination, anchor)
            .iter()
            .any(|key| active.contains(*key))
        {
            continue;
        }
        if distance_in_front(entity) < MIN_PORTAL_DOOR_DISTANCE {
            continue;
        }
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((entity, distance));
        }
    }
    best.map(|(entity, _)| entity)
}

// ---------------------------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------------------------

/// A resident cell root: the identity the isolation classifies, and the visibility it hid the root
/// with before.
type CellRootQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static CellRef,
        Option<&'static ExteriorCellGrid>,
        Option<&'static StreamedCellKey>,
        Option<&'static Visibility>,
        Option<&'static PortalHiddenCell>,
    ),
    With<StreamedCellRoot>,
>;

/// The main camera: the pose the portal camera follows, and the projection it copies.
type MainCameraQuery<'world, 'state> = Query<
    'world,
    'state,
    (&'static GlobalTransform, &'static Projection),
    (
        With<StreamingCamera>,
        Without<PortalCamera>,
        Without<PortalQuad>,
    ),
>;

/// A load door reference with everything that places its doorway, and whether it is open.
type LoadDoorQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static GlobalTransform,
        &'static Transform,
        &'static LoadDoor,
        Option<&'static DoorState>,
        Option<&'static InstanceBounds>,
        Option<&'static ExpectedModelBounds>,
        Option<&'static DoorAnchor>,
    ),
    (Without<PortalCamera>, Without<PortalQuad>),
>;

/// The portal camera, and the doorway quad it draws through.
type PortalCameraQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        &'static mut Transform,
        &'static mut Projection,
        &'static mut Camera,
    ),
    With<PortalCamera>,
>;

type PortalQuadQuery<'world, 'state> = Query<
    'world,
    'state,
    (&'static mut Transform, &'static mut Visibility),
    (With<PortalQuad>, Without<PortalCamera>),
>;

/// The main camera's own [`Camera`] component: what `camera_system` fills the render target's size
/// into, and so what [`resize_portal_target`] sizes the portal target from.
type MainCameraTargetQuery<'world, 'state> = Query<
    'world,
    'state,
    &'static Camera,
    (
        With<StreamingCamera>,
        Without<PortalCamera>,
        Without<PortalQuad>,
    ),
>;

/// The portal render target for a given size: a fresh image in [`PORTAL_TEXTURE_FORMAT`].
///
/// `Image` has no resize, so a target of another size is another texture - [`resize_portal_target`]
/// makes one and repoints the camera and the quad at it, rather than resizing the image in place.
fn portal_target_image(size: UVec2) -> Image {
    Image::new_target_texture(size.x, size.y, PORTAL_TEXTURE_FORMAT, None)
}

/// The size the portal target takes for a main camera rendering at `main` pixels, or `None` when
/// there is no size to follow.
///
/// One factor for both axes, so the doorway image keeps the main view's pixel aspect ratio: a cap
/// applied per axis on its own would stretch the doorway along whichever axis was not capped.
/// Sizes at or under the ceiling are followed exactly - the factor is then 1 - and anything larger
/// is scaled down uniformly.
///
/// `None` for a target with a zero axis, which is a minimized window rather than a size: a 0x0
/// texture is not a render target, so the caller keeps the one it has until the window comes back.
fn portal_target_size(main: UVec2, ceiling: UVec2) -> Option<UVec2> {
    if main.x == 0 || main.y == 0 {
        return None;
    }
    let main = main.as_vec2();
    let ceiling = ceiling.as_vec2();
    let scale = (ceiling / main).min_element().min(1.0);
    // Rounded rather than truncated, and clamped to the ceiling afterwards: the scale is a float,
    // so a size that should land exactly on the cap can land a fraction over it.
    let scaled = (main * scale).round().max(Vec2::ONE);
    Some(scaled.min(ceiling).as_uvec2())
}

/// Grows or shrinks the portal render target to the size of the main camera's own, so the doorway
/// is drawn at the resolution of the room around it.
///
/// The size comes from the main camera's *computed* target info (`Camera::computed.target_info`,
/// filled by `camera_system` from the camera's `RenderTarget`): the physical size of whatever that
/// camera draws into - the window with its scale factor, or an image - recomputed whenever the
/// window is resized. Reading it rather than the `Window` component follows the camera that is
/// actually drawn, and costs one frame of lag at worst: the camera is computed in `PostUpdate`,
/// this runs in `Update`. On the first frame of a run the size is not computed yet, and a run with
/// no window has none at all: both keep [`PORTAL_TEXTURE_FALLBACK_SIZE`].
///
/// **Nothing is allocated unless the size changes**, and what the decision is keyed on is the size
/// of the image the resource already points at - `Image::size`, not a copy of the last request, so
/// there is one source of truth for "how big is the target" and an unchanged window does no work
/// at all. A size that does change is a new image, with the camera's `RenderTarget` and the quad's
/// material repointed at it in the same frame: the material's bind group is rebuilt from the
/// changed asset, and the camera would otherwise render into the new texture while the doorway
/// still sampled the old one.
fn resize_portal_target(
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<PortalMaterial>>,
    mut texture: ResMut<PortalTexture>,
    main: MainCameraTargetQuery,
    mut targets: Query<&mut RenderTarget, With<PortalCamera>>,
    quad: Query<&MeshMaterial3d<PortalMaterial>, With<PortalQuad>>,
) {
    let Ok(camera) = main.single() else {
        return;
    };
    let size = camera
        .computed
        .target_info
        .as_ref()
        .and_then(|info| portal_target_size(info.physical_size, PORTAL_TEXTURE_MAX_SIZE));
    let Some(size) = size else {
        return;
    };
    if images.get(&texture.0).map(Image::size) == Some(size) {
        return;
    }
    let image = images.add(portal_target_image(size));
    texture.0 = image.clone();
    if let Ok(mut target) = targets.single_mut() {
        *target = RenderTarget::Image(image.clone().into());
    }
    if let Ok(handle) = quad.single()
        && let Some(mut material) = materials.get_mut(handle)
    {
        material.extension.portal_texture = Some(image);
    }
    info!(
        width = size.x,
        height = size.y,
        "portal: render target resized to the size of the main camera's"
    );
}

fn setup_portal_camera(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let image = images.add(portal_target_image(PORTAL_TEXTURE_FALLBACK_SIZE));
    commands.insert_resource(PortalTexture(image.clone()));
    commands.spawn((
        Name::new("Portal camera"),
        Camera3d::default(),
        Camera {
            order: PORTAL_CAMERA_ORDER,
            is_active: false,
            ..default()
        },
        // The destination is drawn from the doorway's own clip plane to the shared far plane. Not
        // tonemapped: the quad's material hands the image to the main camera's tonemapper, and
        // tonemapping it here as well would darken the doorway against the room around it. What it
        // hands over is not a clipped 8-bit copy of the destination either - the target is float
        // ([`PORTAL_TEXTURE_FORMAT`]), so the values above white reach that tonemapper intact.
        Tonemapping::None,
        RenderTarget::Image(image.clone().into()),
        Projection::Perspective(PerspectiveProjection::default()),
        Transform::default(),
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::layer(DESTINATION_LAYER),
        // The destination's own atmosphere, written every frame the doorway is open by
        // [`update_destination_atmosphere`]. All three are components on this camera and not on the
        // main one, which is what lets a doorway open off a lit hall into a black cave and show the
        // cave: `AmbientLight` overrides `GlobalAmbientLight` for this view alone, `DistanceFog` is
        // per view, and so is the clear colour. They start at the engine's daylight defaults - this
        // camera draws nothing until a portal is up.
        AmbientLight::default(),
        DistanceFog::default(),
        PortalCamera,
    ));
    // The doorway's own sun, next to the camera it lights and on that camera's layer alone. Off
    // until [`update_destination_atmosphere`] gives it the destination's own - the frame a doorway
    // first opens in is the frame that writes it - and pointing nowhere in particular until
    // [`place_destination_sun`] has the engine's sun to copy: `app::setup_world` spawns that one,
    // and no doorway is drawn before it has.
    commands.spawn((
        Name::new("Portal destination sun"),
        PortalDestinationSun,
        DirectionalLight {
            color: Color::WHITE,
            illuminance: 0.0,
            shadow_maps_enabled: false,
            ..default()
        },
        CascadeShadowConfig::default(),
        Transform::default(),
        RenderLayers::layer(DESTINATION_LAYER),
    ));
}

/// Gives the portal camera the atmosphere of the space behind the doorway it is showing, and the
/// doorway's own sun ([`PortalDestinationSun`]) that space's own sun.
///
/// The room on the far side of a door is lit and fogged by its own records: Alftand01's fog is
/// `(153, 210, 238)` and reaches 9,000 units, while the Tamriel it is entered from is fogged by the
/// terrain ring at a completely different distance, and Blackreach's backdrop is teal where the
/// Alftand cavern it opens off is not. Rendering the destination with the *source's* atmosphere is
/// what made a doorway look like a hole into the room the player is already standing in
/// (`docs/research/visual-gaps-spec.md`, gap 2).
///
/// The sun was the one piece this used to leave out, on the reading that the engine's single
/// [`DirectionalLight`] lights every view of a frame. It does not: Bevy selects a view's
/// directional lights by render layers ([`PortalDestinationSun`] has the mechanism and the lines),
/// and the engine's sun has none, so what the doorway image was missing was any directional light
/// at all - a daylit street seen from inside a house was drawn with ambient and fog alone. The
/// destination's own sun is now a light of this camera's own view, which is also the case that
/// matters: a player at a doorway is in the source space or the destination one, never in both.
fn update_destination_atmosphere(
    state: Res<PortalState>,
    catalog: Option<Res<SpaceLightingCatalog>>,
    config: Option<Res<EngineConfig>>,
    doors: Query<&LoadDoor>,
    mut camera: Query<(&mut Camera, &mut AmbientLight, &mut DistanceFog), With<PortalCamera>>,
    mut suns: Query<&mut DirectionalLight, With<PortalDestinationSun>>,
    mut applied: Local<Option<SpaceKey>>,
) {
    let Ok((mut camera, mut ambient, mut fog)) = camera.single_mut() else {
        return;
    };
    let Some(config) = config else {
        return;
    };
    // The door the portal is rendering through, and so the space the camera is standing in. The
    // portal clears it whenever it has no window to draw, which is also when this camera is
    // inactive: there is nothing to keep in step while no doorway is open.
    let Some(destination) = state
        .open_door
        .and_then(|door| doors.get(door).ok())
        .map(|door| {
            space_key(
                door.destination.worldspace_id.unwrap_or_default(),
                door.destination.interior_cell_id,
            )
        })
    else {
        return;
    };
    let atmosphere = crate::app::space_atmosphere(catalog.as_deref(), destination);
    // The camera's three are written on a change of destination and not every frame: they are read
    // through change detection, and a doorway standing open for a minute would otherwise mark a
    // camera and its view changed sixty times a second for no reason. Nothing else moves them - the
    // catalog is read once at startup and the radii are the run's.
    if *applied != Some(destination) {
        *applied = Some(destination);
        camera.clear_color = ClearColorConfig::Custom(atmosphere.backdrop);
        *ambient = crate::app::ambient_light(&atmosphere);
        *fog = crate::app::atmosphere_fog(&atmosphere, config.stream_radius, config.terrain_radius);
    }
    // The sun is written whenever it is not the sun this destination wants, and not only on a
    // change of destination: `app::update_atmosphere` writes *every* `DirectionalLight` in the
    // world when the space the player stands in changes - this one included, since it cannot know
    // whose it is - and a change of the active space is exactly what a crossing is. A change-only
    // write would leave the doorway carrying the space the player came from until the portal
    // retargeted, and a write on every frame would mark the light changed sixty times a second.
    // Comparing is what makes this one line self-healing instead.
    for mut sun in &mut suns {
        if sun.color != atmosphere.sun.color {
            sun.color = atmosphere.sun.color;
        }
        if sun.illuminance != atmosphere.sun.illuminance {
            sun.illuminance = atmosphere.sun.illuminance;
        }
    }
}

/// Puts the doorway's own sun ([`PortalDestinationSun`]) where the engine's sun is: the same
/// direction, and the same shadow settings.
///
/// The *direction* is the engine sun's and nothing else's. It is a run-wide constant
/// (`app::setup_world` builds it from a rotation), and copying the component rather than naming
/// that rotation again is what keeps one sun in the world: a doorway whose shadow fell the other
/// way from the room around it would be a doorway drawn at a wall angle of its own. What is the
/// *destination's* is the tint and the illuminance, and that is
/// [`update_destination_atmosphere`]'s.
///
/// `shadow_maps_enabled` and the cascades come with it, so the doorway keeps the shadows the room
/// around it has. That costs a cascade set in the portal camera's view - Bevy budgets cascades per
/// *view* (`bevy_pbr-0.19.0/src/render/light.rs:1323-1353`) and allocates the shadow map for the
/// maximum over views, which is four either way - and the main camera's own set is untouched. A
/// light whose shadows turned out to cost more than that is turned off here rather than in
/// `app.rs`, which is not this module's to change.
fn place_destination_sun(
    engine: Query<
        (&Transform, &DirectionalLight, Option<&CascadeShadowConfig>),
        Without<PortalDestinationSun>,
    >,
    mut sun: Query<
        (
            &mut Transform,
            &mut DirectionalLight,
            &mut CascadeShadowConfig,
        ),
        With<PortalDestinationSun>,
    >,
) {
    // The one that is not ours. There is one directional light in an engine run - the fixtures
    // spawn one of their own, and no run has two - and the first that is not the doorway's is it.
    let Some((engine_transform, engine_light, engine_cascades)) = engine.iter().next() else {
        return;
    };
    for (mut transform, mut light, mut cascades) in &mut sun {
        if transform.rotation != engine_transform.rotation {
            transform.rotation = engine_transform.rotation;
        }
        if light.shadow_maps_enabled != engine_light.shadow_maps_enabled {
            light.shadow_maps_enabled = engine_light.shadow_maps_enabled;
        }
        if let Some(engine_cascades) = engine_cascades {
            // `CascadeShadowConfig` is not `PartialEq`, so the three fields are compared rather than
            // the component: writing it unconditionally would mark the light changed every frame.
            let same = cascades.bounds == engine_cascades.bounds
                && cascades.overlap_proportion == engine_cascades.overlap_proportion
                && cascades.minimum_distance == engine_cascades.minimum_distance;
            if !same {
                *cascades = engine_cascades.clone();
            }
        }
    }
}

/// One entity of a doorway's mirror, whichever part of the model it is: the components the walk
/// needs are the same for a node of the scene and for a mesh under it.
type MirrorNodeQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Has<Mesh3d>,
        &'static mut Transform,
        &'static mut Visibility,
        Option<&'static RenderLayers>,
        Option<&'static DoorLeaf>,
    ),
>;

/// The pose a doorway's mirror takes: the door's own reference pose carried through the door ->
/// destination map ([`DoorMap::pose`]), the same map the portal camera is placed by.
///
/// The map is rigid, so a root here with every node of the model's own hierarchy under it makes the
/// mirror the door's own model *seen through the doorway*: a point at `M(P)` is drawn on the pixel
/// `P` is, which is what the quad's screen-space sampling comes to. `M(source doorway) = destination
/// doorway` by construction, so the translation is the destination doorway's own and only the
/// rotation and the scale are the door's. Nothing about the map is written out here - it is the one
/// [`door_map`] `update_portal` builds - and a second opinion about where a destination is would be
/// a mirror drawn beside the doorway.
fn mirror_root_pose(
    map: &DoorMap,
    door_position: Vec3,
    door_rotation: Quat,
    door_scale: Vec3,
) -> Transform {
    let (translation, rotation) = map.pose(door_position, door_rotation);
    Transform {
        translation,
        rotation,
        scale: door_scale,
    }
}

/// A load door as [`place_door_mirror`] reads it: the reference's own pose, the row that says where
/// it leads, whether its own animation moves its model, and the scene handle to instance.
///
/// `Without<PortalDoorMirror>` is what lets the system that owns this read a `Transform` while the
/// query that places the mirror writes one: a door is never a mirror of itself, and Bevy needs it
/// said rather than assumed.
type MirroredDoorQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        &'static Transform,
        &'static GlobalTransform,
        &'static LoadDoor,
        Option<&'static DoorState>,
        Option<&'static WorldAssetRoot>,
        Option<&'static DoorAnchor>,
    ),
    Without<PortalDoorMirror>,
>;

/// Spawns, places and drops the doorway's mirror of the door the portal is rendering through.
///
/// One mirror at a time, and only for the door in [`PortalState::open_door`] that has an animation
/// of its own: a door with no clip has no leaf that can be swung out of the doorway - the portal
/// hides its whole model instead ([`drawn_door_visibility`]) - and an auto-load marker is an
/// invisible reference with no leaf at all. The scene spawns from the handle the door's own
/// instance came from (`WorldAssetRoot`, the one `streaming::spawn_cell` loaded once), so the model
/// is instantiated a second time and never read again.
///
/// The root is placed at [`mirror_root_pose`] every frame rather than once at spawn: a model is
/// rebased with its cell when the render origin moves, and a mirror placed once would be left
/// standing at the old origin.
///
/// **Not a reference.** The mirror is a fresh entity with a scene root on it and nothing else: no
/// `MeshHandle`, no `WorldTransform`, no `FormId` and no [`LoadDoor`]. `streaming.rs` measures and
/// validates a reference's model through the components it spawns a scene with, so a mirror holding
/// them would be measured as a reference - and `model_animation.rs` hands a model's idle clip to
/// every reference whose model has one, which for a mirror would be a second animation on top of
/// the door's own swing.
///
/// Dropped the frame the portal stops rendering through this door, or the door stops being one
/// whose own animation moves it, or the door entity goes with its cell. `try_despawn` and not
/// `despawn`: a crossing despawns a whole cell's worth of doors in the frame it happens in, and a
/// command against an entity that frame despawned is a panic (`door_animation.rs` has the same
/// note on the same hazard).
fn place_door_mirror(
    mut commands: Commands,
    state: Res<PortalState>,
    origin: Option<Res<RenderOrigin>>,
    doors: MirroredDoorQuery,
    mut mirrors: Query<(Entity, &mut PortalDoorMirror, &mut Transform)>,
) {
    // What this frame wants, or `None` for every reason there is to want nothing: no door in the
    // doorway, the door entity gone, a model with no swing of its own to draw, or no scene to
    // instance.
    let wanted = state.open_door.and_then(|door| {
        let (local, global, row, door_state, scene, anchor) = doors.get(door).ok()?;
        let animated = matches!(
            door_state,
            Some(DoorState::Opening | DoorState::Open { animated: true })
        );
        let scene = scene.filter(|_| animated && !row.auto_load)?;
        let origin = origin.as_ref()?;
        let door_rotation = global.rotation();
        let map = door_map(
            global.translation(),
            door_rotation,
            local.scale,
            row,
            anchor,
            origin.0,
        );
        Some((
            door,
            row.ref_id,
            mirror_root_pose(&map, global.translation(), door_rotation, local.scale),
            scene.0.clone(),
        ))
    });

    if let Some((entity, mirror, mut transform)) = mirrors.iter_mut().next() {
        if let Some((door, _, pose, _)) = wanted
            && door == mirror.door
        {
            if *transform != pose {
                *transform = pose;
            }
            return;
        }
        // Another door's mirror, or nobody's: it goes, and the wanted door gets one of its own
        // below - in this same frame, so no frame draws a doorway with the wrong door's leaf in it.
        commands.entity(entity).try_despawn();
    }
    let Some((door, ref_id, pose, scene)) = wanted else {
        return;
    };
    commands.spawn((
        Name::new(format!("Doorway mirror {ref_id:08X}")),
        PortalDoorMirror {
            door,
            logged: false,
        },
        WorldAssetRoot(scene),
        // On the root as well as on the nodes the walk marks below, and for the same two reasons:
        // the walk probe skips anything under a `DoorLeaf`, and `update_door_leaves` draws or hides
        // the mirror with the door's own leaf - so a door whose leaf is hidden when it is open (a
        // swing that did not clear the opening) hides the whole mirror with it.
        DoorLeaf { door },
        pose,
        Visibility::default(),
        RenderLayers::layer(DESTINATION_LAYER),
    ));
}

/// What a walk of a mirror found, for the one-per-mirror log line and for the check that the mirror
/// is the door's own model and not something else.
#[derive(Default)]
struct MirrorCount {
    /// Nodes of the model's scene (every entity the loader gave an `AnimationTargetId`).
    nodes: usize,
    /// Mesh entities, drawn or not: the node has an id, the mesh under it does not.
    meshes: usize,
    /// Mesh entities that are drawn - the door's own leaf, not the model's static filler.
    drawn_meshes: usize,
}

/// Draws the doorway's mirror: every drawn node of the second instance is given the door's own local
/// transform, and only the part of the model the door's animation moves is drawn at all.
///
/// **The pairing.** The mirror and the door are instances of one asset, so a node's
/// [`AnimationTargetId`] - a hash of its name path from the scene root (`bevy_gltf-0.19.0/src/
/// loader/mod.rs:1558`) - is the same in both, and it is the key the two are paired by. That is
/// stricter than a walk order: it cannot pair a node with the wrong node of the same model, and it
/// does not care which order the two hierarchies were spawned in. A node whose id the door's model
/// does not have is not paired and is not copied, and - not being part of the leaf - is not drawn
/// either, so a scene that is not the door's model draws nothing rather than something wrong.
///
/// **What is drawn: the leaf, not the model's static filler.** A load door's model is not only the
/// door: it carries the *plug* the game hides the room behind. `FarmhouseLDoor01`'s `DoorBlack` is
/// a 96x176x36 open box behind the leaf, and the Dwemer large load door's `Plane02` is a flat
/// 267x357 plane 6 units behind its two leaves - both static, both closed against the player, both
/// invisible from outside because the doorway quad and the wall cover them. Mapped into the
/// destination image they would *not* be covered: the map puts the door's own origin on the arrival
/// point, which for the demo route's doors is 56 units inside the room (`door_links` 0x1CBB0
/// arrives at -510.2, -198.9 against the door it leads to at -511.5, -254.9), so the plug lands
/// *beyond* the doorway's clip plane and in front of everything the doorway is meant to show - the
/// doorway would be a black rectangle with a leaf over it. The mirrors therefore draw the model's
/// moving part and hide the rest, and the moving part is what the doorway image is actually
/// missing: whatever the source view does not already draw, because it is behind the quad's plane.
/// The nodes that move are exactly the ones [`crate::door_animation`] marks [`DoorLeaf`] on the
/// door itself - its clips' targets - so this is the engine's own answer to "which part of a door
/// is the door" and not a second one.
///
/// **Ordering.** The pose copy has to see the animation's output and be seen by the transform
/// propagation, so it runs in `PostUpdate` after [`AnimationSystems`] and before
/// [`TransformSystems::Propagate`] (the plugin's wiring says the same). The copy is only written
/// when it differs, so a door standing still marks nothing changed.
#[allow(clippy::too_many_arguments)]
fn mirror_door_nodes(
    mut commands: Commands,
    mut mirrors: Query<(Entity, &mut PortalDoorMirror)>,
    doors: Query<&LoadDoor>,
    leaves: Query<&DoorLeaf>,
    children: Query<&Children>,
    targets: Query<&AnimationTargetId>,
    mut nodes: MirrorNodeQuery,
) {
    for (mirror_root, mut mirror) in &mut mirrors {
        let Ok(door) = doors.get(mirror.door) else {
            // The door is gone; `place_door_mirror` drops the mirror in this frame or the next and
            // there is nothing here to copy a pose from.
            continue;
        };
        // The nodes of the door's own model that its animation moves, by id: the ones
        // `door_animation::mark_leaf_nodes` marked as the door's leaf. Empty until the door's clips
        // have resolved, which is also the only frame in which the mirror would have nothing to
        // draw.
        let moved: HashSet<AnimationTargetId> = std::iter::once(mirror.door)
            .chain(children.iter_descendants(mirror.door))
            .filter(|node| leaves.get(*node).is_ok_and(|leaf| leaf.door == mirror.door))
            .filter_map(|node| targets.get(node).ok().copied())
            .collect();
        // The door's own nodes, by the same ids: what the mirror's nodes are paired with.
        let source: HashMap<AnimationTargetId, Entity> = std::iter::once(mirror.door)
            .chain(children.iter_descendants(mirror.door))
            .filter_map(|node| targets.get(node).ok().map(|target| (*target, node)))
            .collect();
        // The two instances are one model, node for node, or this is not the door the portal is
        // rendering through - a scene the asset has not finished spawning, or an asset swapped
        // under the door. That frame is skipped rather than half applied: the mirror keeps the pose
        // it has, which is the door's pose of a frame ago.
        if scene_nodes(mirror_root, &children, &targets)
            != scene_nodes(mirror.door, &children, &targets)
        {
            continue;
        }

        let mut count = MirrorCount::default();
        walk_mirror(
            mirror_root,
            false,
            mirror.door,
            &moved,
            &source,
            &children,
            &targets,
            &mut nodes,
            &mut commands,
            &mut count,
        );
        if !mirror.logged && count.nodes > 0 {
            mirror.logged = true;
            let position = nodes
                .get(mirror_root)
                .map(|(_, transform, ..)| transform.translation)
                .unwrap_or_default();
            info!(
                door = format_args!("{:08X}", door.ref_id),
                position = ?position,
                nodes = count.nodes,
                meshes = count.meshes,
                drawn_meshes = count.drawn_meshes,
                "portal: doorway mirror drawn from the door's own model"
            );
        }
    }
}

/// The nodes of a model's scene under `root`: every entity the glTF loader gave an
/// [`AnimationTargetId`], which is every node and no mesh.
fn scene_nodes(
    root: Entity,
    children: &Query<&Children>,
    targets: &Query<&AnimationTargetId>,
) -> usize {
    std::iter::once(root)
        .chain(children.iter_descendants(root))
        .filter(|node| targets.get(*node).is_ok())
        .count()
}

/// Walks one entity of a mirror: its layer, its pose and its visibility from the door's own model,
/// the leaf marking that keeps the walk probe off it, and the hiding of the model's static filler -
/// then the same for everything under it.
///
/// `under_leaf` is whether this entity is inside a node the door's own animation moves: the leaf
/// and its meshes are drawn, and everything else in the model is hidden ([`mirror_door_nodes`] has
/// why). Hiding a *mesh* cannot take a drawn mesh with it, because a mesh entity's children are its
/// own primitives rather than scene nodes: a node of the model is never hidden by this - an
/// ancestor of the leaf has to stay visible for the leaf to be - which is why the decision is made
/// on the mesh entity itself.
#[allow(clippy::too_many_arguments)]
fn walk_mirror(
    entity: Entity,
    under_leaf: bool,
    door: Entity,
    moved: &HashSet<AnimationTargetId>,
    source: &HashMap<AnimationTargetId, Entity>,
    children: &Query<&Children>,
    targets: &Query<&AnimationTargetId>,
    nodes: &mut MirrorNodeQuery,
    commands: &mut Commands,
    count: &mut MirrorCount,
) {
    let target = targets.get(entity).ok().copied();
    let paired = target.and_then(|target| source.get(&target).copied());
    let drawn = under_leaf || target.is_some_and(|target| moved.contains(&target));
    if target.is_some() {
        count.nodes += 1;
    }
    // Read what this entity is before taking a borrow that lasts: the branches below need the query
    // again, one of them for two entities at once.
    let Ok((is_mesh, _, _, layers, leaf)) = nodes.get(entity) else {
        return;
    };
    let (layers, has_leaf) = (layers.cloned(), leaf.is_some());
    // `RenderLayers` is read per mesh entity and is not inherited, and the scene's meshes arrive
    // with the instance rather than with the root: every entity of the mirror is put on the portal
    // camera's layer, every frame, until it is there.
    let destination_layers = RenderLayers::layer(DESTINATION_LAYER);
    if layers.as_ref() != Some(&destination_layers) {
        commands.entity(entity).try_insert(destination_layers);
    }
    if is_mesh {
        count.meshes += 1;
        let wanted = if drawn {
            count.drawn_meshes += 1;
            // The door's own state decides whether its leaf is drawn (`update_door_leaves` writes
            // the marking's visibility, and the mesh inherits it); this only undoes the hiding
            // below. The mesh is marked with the rest of the leaf when it is not marked yet, so a
            // mesh stands under a `DoorLeaf` however deep the model nests it.
            if !has_leaf {
                commands.entity(entity).try_insert(DoorLeaf { door });
            }
            Visibility::Inherited
        } else {
            // The model's static filler: the frame's backing, and the plug across the doorway. It is
            // not the part of the door the doorway image is missing, and drawn in that image it
            // would cover it. `Visibility` is what hides it from the portal camera *and* from the
            // walk probe, which skips render layers but not visibility.
            Visibility::Hidden
        };
        if let Ok((_, _, mut visibility, ..)) = nodes.get_mut(entity)
            && *visibility != wanted
        {
            *visibility = wanted;
        }
    } else {
        // Every node of the mirror carries the door's leaf marking, the model's root included: the
        // walk probe skips any mesh with a [`DoorLeaf`] above it, and the mirror stands exactly
        // where the player lands after a crossing, so no part of it may stop them.
        if !has_leaf {
            commands.entity(entity).try_insert(DoorLeaf { door });
        }
        if let Some(paired) = paired
            && let Ok([mirror_node, source_node]) = nodes.get_many_mut([entity, paired])
        {
            // The door's own local pose and its drawing state, not a re-derivation: the animation
            // writes the door's nodes, and this is those nodes' output carried to the mirror's.
            let (_, mut mirror_transform, mut mirror_visibility, ..) = mirror_node;
            let (_, source_transform, source_visibility, ..) = source_node;
            if *mirror_transform != *source_transform {
                *mirror_transform = *source_transform;
            }
            // Only the nodes the door's own animation marking covers: those are the ones
            // `update_door_leaves` writes, and it writes the same answer. Copying the rest - the
            // model's static filler - would fight that system for no gain.
            if target.is_some_and(|target| moved.contains(&target))
                && *mirror_visibility != *source_visibility
            {
                *mirror_visibility = *source_visibility;
            }
        }
    }
    if let Ok(node_children) = children.get(entity) {
        for child in node_children.iter() {
            walk_mirror(
                child, drawn, door, moved, source, children, targets, nodes, commands, count,
            );
        }
    }
}

fn setup_portal_quad(
    mut commands: Commands,
    texture: Res<PortalTexture>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<PortalMaterial>>,
) {
    // A vertical 1x1 plane facing +Z; Plane3d::default() is a floor (+Y normal), which left the
    // doorway quad lying flat, edge-on to the player and invisible.
    let mesh = meshes.add(Plane3d::new(Vec3::Z, Vec2::splat(0.5)).mesh());
    let material = materials.add(PortalMaterial {
        base: StandardMaterial {
            unlit: true,
            cull_mode: None,
            double_sided: true,
            ..default()
        },
        extension: PortalExtension {
            portal_texture: Some(texture.0.clone()),
        },
    });
    commands.spawn((
        Name::new("Portal doorway"),
        PortalQuad,
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::default(),
        Visibility::Hidden,
        RenderLayers::layer(PORTAL_QUAD_LAYER),
    ));
}

/// Puts every load door's model where [`drawn_door_visibility`] says it goes: hidden where the
/// doorway is an opening with no leaf in it, drawn everywhere else.
///
/// `Visibility` is inherited, so this covers the meshes of the glTF scene that the asset loader
/// spawns under the root a frame or more later - the leaf that has not arrived yet is drawn closed
/// when it does, and the leaf of the door the portal shows never arrives on screen at all.
///
/// The write goes straight to the component rather than through `Commands`: a crossing despawns a
/// whole cell's worth of doors in the frame it happens in, and a command queued against a door that
/// the same frame despawns is applied to a dead entity, which Bevy treats as a panic. A query
/// cannot return a despawned door, and writing to one that is despawned later in the frame costs
/// nothing.
///
/// A door with no [`Visibility`] at all has no model and no light to draw - `streaming::spawn_cell`
/// gives the component to every reference that has either - so there is nothing for the portal to
/// open and the query leaves it alone.
///
/// **Ordered within [`PortalFrame`].** This is not the only writer of a door's `DoorState`, and one
/// of the others runs in the frame the state changes: `crate::door_animation`'s arrival opening is
/// ordered before [`PortalFrame`], so the answer for the door the player arrives behind is read off
/// the state they arrive in, not the closed one of the frame before
/// (`crate::transition::OpenDestinationDoor`).
fn show_load_door_leaves(
    state: Res<PortalState>,
    held: Query<(), With<CrossingHeld>>,
    mut doors: Query<(Entity, &LoadDoor, Option<&DoorState>, &mut Visibility)>,
) {
    for (door, load_door, door_state, mut visibility) in &mut doors {
        let wanted = drawn_door_visibility(
            load_door.auto_load,
            door_state,
            state.open_door == Some(door),
            held.contains(door),
            state.destination_door == Some(door),
        );
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
}

/// Whether a load door's model is drawn, from the four facts that decide it: whether it is an
/// auto-load marker, where its state is, whether the portal is rendering through it this frame, and
/// whether it is holding a crossing.
///
/// The entries below are the rows of the test
/// `a_doors_model_is_drawn_unless_it_is_the_hole_the_doorway_needs`. [`Visibility::Hidden`] is the
/// answer for exactly one kind of door: one whose doorway is an opening with nothing in it to draw.
///
/// * An **auto-load door**: an invisible marker with no leaf at all (its base is `AutoLoadDoor01`
///   and friends), so it is always hidden. The doorway a marker stands in is drawn by the door
///   beside it or by nothing.
/// * An open door with **no animation of its own** ([`DoorState::hides_whole_reference`]): it has
///   no leaf that can swing out of the opening, so the whole model has to go for the doorway to be
///   the way through that `E` promised - and its model *is* its leaf, so there is nothing else of
///   it that could stay. That covers the door [`update_portal`] picked this frame as well as the
///   open door the player is walking into - which is the state the portal itself stops rendering
///   in, one unit in front of the doorway plane (`MIN_PORTAL_DOOR_DISTANCE`), and where a leaf put
///   back would be in the player's face exactly as they walked into it (design section 4.7). This
///   is the only case that hides a whole reference, and it is kept because the alternative - a
///   closed door drawn over a doorway the crossing is about to use, or an opening with neither leaf
///   nor window in it - is worse than a door that is honestly a hole once it is asked to open.
/// * The door the portal is **rendering through**, when it is a door without an animation: the quad
///   stands in its doorway, and the model would be drawn over it.
/// * The **destination** door of the pair the portal is rendering through
///   ([`PortalState::destination_door`]): the other end of the same doorway, whose closed leaf
///   stands in the window's own clip plane under a doorway anchor and would cover the room the
///   window exists to show. The frame around it goes with it, and the destination wall's own
///   opening is behind the source doorway anyway.
///
/// An **animated** door is none of these except the last, and never becomes one of the others.
/// (As the destination of the pair it is hidden whatever its own state, because its leaf stands in
/// the window's clip plane.) Otherwise its frame *is* the doorway, and
/// its leaves are [`crate::door_animation`]'s to draw or hide: a leaf that swung clear stays drawn,
/// a leaf a narrow clip left in the opening is hidden when the door is `Open`, and the swing itself
/// is drawn - so nothing here may hide its model, not even while the portal renders through it
/// (a stationary leaf mid-swing in front of the window is what an opening door looks like). This is
/// the case the Riverwood house doors are in, and the run's own log says so: they all have
/// `FarmhouseLDoor01`, whose `Open`/`Close` clips are in the converted model
/// (`tools/research/door_animation_nif.py glb`), and a demo tour of that route resolves a clip and a
/// blown-up swing for each of them (`engine::door_animation`, "load door's own swing scaled up to
/// open the doorway", door `0001CBB0`, 18 to 90 degrees) with none falling back to the static path.
/// Where the swung leaf and the window overlap on screen the **depth buffer** decides, not this
/// function: the quad is an opaque mesh of the main view and so is the door, drawn in one pass with
/// one depth buffer, so the leaf wins wherever it is in front of the doorway's own plane - the
/// leaf drawn over the window is what the doorway of an opening door looks like.
///
/// A door whose crossing is **held** ([`CrossingHeld`]) draws whatever else is true: the crossing
/// is waiting for a destination that is not streamed in, so the portal has no window to show
/// through the doorway either, and a doorway with neither is a hole in the world (design section
/// 4.4).
fn drawn_door_visibility(
    auto_load: bool,
    door_state: Option<&DoorState>,
    portal_shows_this_door: bool,
    waiting: bool,
    portal_destination: bool,
) -> Visibility {
    // Whether the door has an animation of its own, and therefore a leaf that is drawn or hidden on
    // its own account: a door with no clip never enters `Opening` or `Closing`, and
    // `Open { animated: false }` is a door whose model is the leaf.
    let animated = matches!(
        door_state,
        Some(DoorState::Opening | DoorState::Closing | DoorState::Open { animated: true })
    );
    let opening_with_nothing_in_it = auto_load
        || door_state.is_some_and(|state| state.hides_whole_reference())
        || (portal_shows_this_door && !animated)
        || portal_destination;
    if opening_with_nothing_in_it && !waiting {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    }
}

/// Moves every resident cell that is not part of the active space off the main camera's layers, and
/// puts the portal's destination on the portal camera's layer.
#[allow(clippy::too_many_arguments)]
fn isolate_cells(
    mut commands: Commands,
    active: Option<Res<ActiveCell>>,
    config: Option<Res<EngineConfig>>,
    origin: Option<Res<RenderOrigin>>,
    camera: Query<&Transform, With<StreamingCamera>>,
    roots: CellRootQuery,
    children: Query<&Children>,
    nodes: Query<(Option<&RenderLayers>, Option<&PortalOriginalLayers>)>,
    mut state: ResMut<PortalState>,
) {
    let (Some(active), Some(config), Some(origin)) = (active, config, origin) else {
        return;
    };
    let Ok(camera) = camera.single() else {
        return;
    };
    let space = ActiveSpace::of(&active, config.unload_radius, camera.translation, origin.0);
    let destination = state.destination.clone();
    state.roles.clear();
    for (root, cell, grid, key, visibility, hidden) in &roots {
        let identity = CellIdentity::of(cell.0, grid, key);
        let role = if identity.is_active(&space) {
            CellRole::Active
        } else if identity.is_destination(&destination) {
            CellRole::Destination
        } else {
            CellRole::Hidden
        };
        state.roles.insert(root, role);

        match role {
            CellRole::Hidden => {
                if !matches!(visibility, Some(Visibility::Hidden)) || hidden.is_none() {
                    // Keep the visibility a previous frame recorded, so a root that something else
                    // unhid is hidden again without forgetting what to restore.
                    let original = hidden.map_or_else(
                        || visibility.copied().unwrap_or_default(),
                        |hidden| hidden.0,
                    );
                    commands
                        .entity(root)
                        .try_insert((PortalHiddenCell(original), Visibility::Hidden));
                }
            }
            CellRole::Active | CellRole::Destination => {
                if let Some(hidden) = hidden {
                    commands.entity(root).try_insert(hidden.0);
                    commands.entity(root).try_remove::<PortalHiddenCell>();
                }
            }
        }

        let wanted = match role {
            CellRole::Active => None,
            CellRole::Destination => Some(RenderLayers::layer(DESTINATION_LAYER)),
            CellRole::Hidden => Some(RenderLayers::none()),
        };
        for entity in children.iter_descendants(root) {
            let Ok((current, original)) = nodes.get(entity) else {
                continue;
            };
            match &wanted {
                Some(wanted) => {
                    if current != Some(wanted) {
                        if original.is_none() {
                            commands.entity(entity).try_insert(PortalOriginalLayers(
                                current.cloned().unwrap_or_default(),
                            ));
                        }
                        commands.entity(entity).try_insert(wanted.clone());
                    }
                }
                None => {
                    if let Some(original) = original {
                        commands.entity(entity).try_insert(original.0.clone());
                        commands.entity(entity).try_remove::<PortalOriginalLayers>();
                    }
                }
            }
        }
    }
}

/// Places the portal camera in the destination of the nearest usable door and puts the doorway quad
/// where that door is.
#[allow(clippy::too_many_arguments)]
fn update_portal(
    mut state: ResMut<PortalState>,
    active: Option<Res<ActiveCell>>,
    config: Option<Res<EngineConfig>>,
    origin: Option<Res<RenderOrigin>>,
    streaming: Option<Res<StreamingWorld>>,
    main: MainCameraQuery,
    doors: LoadDoorQuery,
    parents: Query<&ChildOf>,
    mut portal_camera: PortalCameraQuery,
    mut quad: PortalQuadQuery,
    mut shown: Local<Option<u32>>,
) {
    // This frame's portal, from scratch: whenever the camera below cannot be placed there is no
    // doorway rendering the destination, and the door that was open for it has to close again in
    // this frame. Clearing before the early returns is what makes that true of every one of them.
    state.open_door = None;
    state.destination_door = None;
    let (Some(active), Some(config), Some(origin), Some(streaming)) =
        (active, config, origin, streaming)
    else {
        return;
    };
    let Ok((main_transform, main_projection)) = main.single() else {
        return;
    };
    let Ok((mut camera_transform, mut camera_projection, mut camera)) = portal_camera.single_mut()
    else {
        return;
    };
    let Ok((mut quad_transform, mut quad_visibility)) = quad.single_mut() else {
        return;
    };
    let camera_position = main_transform.translation();
    let camera_rotation = main_transform.rotation();
    let space = ActiveSpace::of(&active, config.unload_radius, camera_position, origin.0);

    // How far in front of each door the camera is, along the direction that door faces. The map it
    // is read off is the one [`update_portal`] builds for the door it picks - the doorway anchor
    // where the door has one, the reference origin and the link-derived frame otherwise - and the
    // pivot it is measured from is the plane its projection clips at
    // ([`distance_in_front_of_door`]).
    let distance_in_front = |entity: Entity| -> f32 {
        let Ok((_, global, local, door, .., anchor)) = doors.get(entity) else {
            return f32::NEG_INFINITY;
        };
        let map = door_map(
            global.translation(),
            global.rotation(),
            local.scale,
            door,
            anchor,
            origin.0,
        );
        distance_in_front_of_door(map.pivot, map.frame, camera_position)
    };

    // Only doors of cells that are in the active space can be looked through: a door of a
    // pre-streamed cell is drawn nowhere near the space the player stands in.
    let candidates = doors
        .iter()
        .filter(|(entity, ..)| {
            state
                .roles
                .get(&parents.get(*entity).map(ChildOf::parent).unwrap_or(*entity))
                == Some(&CellRole::Active)
        })
        .map(|(entity, global, _, door, door_state, _, _, anchor)| {
            (entity, global.translation(), door, door_state, anchor)
        });
    let target = select_portal_door(
        camera_position,
        candidates,
        |destination, anchor| destination_is_resident(destination, anchor, &streaming),
        &space,
        distance_in_front,
    );

    let Some(target) = target else {
        if shown.take().is_some() {
            info!("portal: no door in view");
        }
        state.destination.clear();
        *quad_visibility = Visibility::Hidden;
        camera.is_active = false;
        return;
    };

    let Ok((_, global, local, door, _, instance_bounds, expected_bounds, anchor)) =
        doors.get(target)
    else {
        state.destination.clear();
        *quad_visibility = Visibility::Hidden;
        camera.is_active = false;
        return;
    };
    // The doorway opens here: `show_load_door_leaves` hides this door's leaf in this same frame,
    // with the quad below standing in the doorway it leaves. Nothing past this point fails, so the
    // leaf and the quad go up and down together.
    state.open_door = Some(target);
    // The destination door draws nothing this frame: its leaf stands in the window's own clip
    // plane under a doorway anchor and would cover the room behind it (`PortalState::destination_door`).
    // A door whose map did not move keeps its destination door drawn - the clip stands inside the
    // room, and hides it exactly as it always did.
    // Only a destination door the portal is drawing is hidden: one standing in the active space is
    // what the player sees with their own eyes, and stays drawn.
    state.destination_door = anchor.and_then(|_| {
        doors.iter().find_map(|(entity, _, _, entry, ..)| {
            let active = state
                .roles
                .get(&parents.get(entity).map(ChildOf::parent).unwrap_or(entity))
                == Some(&CellRole::Active);
            (entity != target && !active && entry.ref_id == door.destination.destination_ref_id)
                .then_some(entity)
        })
    });
    if *shown != Some(door.ref_id) {
        *shown = Some(door.ref_id);
        info!(
            door = format_args!("{:08X}", door.ref_id),
            destination = %door.label.trim_end_matches(['\0', ' ']),
            anchor = ?anchor.map(|anchor| anchor.tier),
            "portal: looking through a load door"
        );
    }
    // The map the whole feature is one definition of: the portal camera below, the doorway quad,
    // the clip plane and - through `crate::transition`'s own call - the crossing and the mirror.
    let map = door_map(
        global.translation(),
        global.rotation(),
        local.scale,
        door,
        anchor,
        origin.0,
    );
    let door_position = global.translation();
    let door_rotation = global.rotation();
    let frame = map.frame;
    let front = frame * Vec3::NEG_Z;
    let (portal_position, portal_rotation) = map.pose(camera_position, camera_rotation);
    // The window is clipped at the *destination doorway's* plane, which under an anchor is exactly
    // the image of the source doorway's plane: the room is drawn from the doorway, not from a plane
    // tens of units inside it.
    let (destination_point, destination_normal) = map.destination_plane();
    let clip_plane = doorway_clip_plane(
        portal_position,
        portal_rotation,
        destination_point,
        destination_normal,
    );

    camera_transform.translation = portal_position;
    camera_transform.rotation = portal_rotation;
    *camera_projection = portal_projection(main_projection, clip_plane, -clip_plane.w);
    camera.is_active = true;

    // The doorway itself is the model's, so its box is measured in the model's own frame and then
    // laid out in the frame the door's front comes from: one frame for the quad's position, its
    // orientation and its size. The quad goes where the doorway's own plane is, which is also the
    // plane `crate::player::player_walks_through_doors` fires the crossing on.
    //
    // No standoff: the window is this quad and nothing else (the destination cell is on the portal
    // camera's layer), so it can only carry the destination while it is in front of the camera. A
    // quad held forward of the doorway plane - eight units was enough - is passed by the camera
    // while the player is still walking the last stretch to it, and the doorway shows the wall the
    // door is set into for those units. With the quadrant the doorway's plane, the crossing fires
    // exactly where the window would end, so no frame of the walk shows anything but the
    // destination through it. The cost is that the window covers exactly the measured opening, so a
    // door whose bounds under-measure its doorway would show a sliver of the wall's reveal; the fix
    // for one of those is a better measurement, not a quad held out in front of it.
    let (size, centre) = portal_quad_extents(
        instance_bounds,
        expected_bounds,
        door_rotation,
        frame,
        local.scale,
    );
    quad_transform.translation = door_position + frame * centre + front * PORTAL_QUAD_OFFSET;
    // `Plane3d` faces `+Z` and the player stands on the door's front (`-Z`).
    quad_transform.rotation = frame * Quat::from_rotation_y(PI);
    quad_transform.scale = Vec3::new(size.x, size.y, 1.0);
    *quad_visibility = Visibility::Inherited;

    state.destination = destination_keys(&door.destination, anchor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        streaming::{creation_rotation_to_bevy, creation_to_bevy, render_position},
        transition::{arrival_frame, door_frame, door_to_arrival_rotation, portal_pose},
    };
    use bevy::{
        asset::AssetPlugin, camera::CameraProjection, camera::RenderTargetInfo,
        camera::visibility::VisibilityPlugin, transform::TransformPlugin,
    };

    const INTERIOR_ALFTAND01: u32 = 0x0001_52C3;
    const TAMRIEL: u32 = 60;

    /// A door like the Alftand02 -> AlftandWorld one of the demo route: an exterior destination.
    fn exterior_door() -> LoadDoor {
        LoadDoor {
            ref_id: 0x9256A,
            destination: DoorDestination {
                destination_ref_id: 0x699E8,
                interior_cell_id: None,
                worldspace_id: Some(0x0006_9857),
                arrival_position: [3693.815, 3074.645, 290.530],
                arrival_rotation: [0.0, 0.0, -1.83260],
            },
            label: "AlftandWorld".into(),
            auto_load: false,
            outward: None,
        }
    }

    /// A door like the Alftand01 -> Alftand02 one: an interior destination.
    fn interior_door(cell_id: u32) -> LoadDoor {
        LoadDoor {
            ref_id: 0x92809,
            destination: DoorDestination {
                destination_ref_id: 0x5704B,
                interior_cell_id: Some(cell_id),
                worldspace_id: None,
                arrival_position: [2879.831, 2_718.83, -1828.0],
                arrival_rotation: [0.0, 0.0, 2.87979],
            },
            label: "Alftand02".into(),
            auto_load: false,
            outward: None,
        }
    }

    /// An app with the visibility and transform systems the doors and the isolation rely on.
    ///
    /// [`update_portal`] is not among them: it needs a door whose destination says it is resident,
    /// and residency lives in `StreamingWorld` behind a map private to `streaming`, so no app a
    /// test can build here would ever place a portal - it would return before picking anything and
    /// write `None` over the target every frame. The door tests set [`PortalState::open_door`]
    /// where it would, and [`portal_app_running_update_portal`] adds the real system back, in the
    /// plugin's own order, to check that it is `update_portal` that owns that field.
    fn portal_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::mesh::MeshPlugin,
            TransformPlugin,
            VisibilityPlugin,
        ))
        .insert_resource(EngineConfig {
            worldspace_id: TAMRIEL,
            stream_radius: 1,
            unload_radius: 1,
            start_grid: (0, 0),
            ..EngineConfig::default()
        })
        .insert_resource(ActiveCell {
            worldspace_id: TAMRIEL,
            interior: Some(INTERIOR_ALFTAND01),
        })
        .insert_resource(RenderOrigin(IVec2::new(19, 18)))
        .init_resource::<PortalState>()
        .add_systems(Update, (show_load_door_leaves, isolate_cells).chain());
        app
    }

    /// Adds the real [`update_portal`] to a [`portal_app`], ahead of the door system and in the
    /// order `PortalPlugin` registers them. Called between frames by the one test that needs the
    /// system that really owns [`PortalState::open_door`] to run and clear it.
    fn add_update_portal(app: &mut App) {
        app.add_systems(Update, update_portal.before(show_load_door_leaves));
    }

    fn spawn_camera(app: &mut App, position: Vec3) -> Entity {
        app.world_mut()
            .spawn((
                Transform::from_translation(position),
                GlobalTransform::from_translation(position),
                StreamingCamera,
            ))
            .id()
    }

    /// A cell root with one mesh of its own, the way `streaming::spawn_cell` builds one: the root
    /// carries the cell's identity and a reference child carries a mesh that the asset loader may
    /// add a frame later.
    fn spawn_cell(
        app: &mut App,
        cell_id: u32,
        grid: Option<IVec2>,
        key: Option<CellKey>,
        mesh_layers: Option<RenderLayers>,
    ) -> (Entity, Entity) {
        let mut root = app.world_mut().spawn((
            StreamedCellRoot,
            CellRef(cell_id),
            Transform::default(),
            Visibility::default(),
        ));
        if let Some(grid) = grid {
            root.insert(ExteriorCellGrid(grid));
        }
        if let Some(key) = key {
            root.insert(StreamedCellKey(key));
        }
        let root = root.id();
        let (mesh, _) = spawn_mesh(app, root, mesh_layers);
        (root, mesh)
    }

    fn spawn_mesh(app: &mut App, parent: Entity, layers: Option<RenderLayers>) -> (Entity, Entity) {
        let mut entity = app
            .world_mut()
            .spawn((Mesh3d(Handle::default()), ChildOf(parent)));
        if let Some(layers) = layers {
            entity.insert(layers);
        }
        let mesh = entity.id();
        (mesh, parent)
    }

    /// A load door of a cell, with one mesh under it: the reference root the asset loader hangs the
    /// door's glTF scene from, and a mesh the scene spawns a frame or more later.
    fn spawn_door(app: &mut App, door: LoadDoor) -> (Entity, Entity) {
        let entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                GlobalTransform::default(),
                Visibility::default(),
                door,
            ))
            .id();
        let (mesh, _) = spawn_mesh(app, entity, None);
        (entity, mesh)
    }

    /// Whether the door draws its leaf, through the inheritance the renderer itself uses.
    fn leaf_is_drawn(app: &App, mesh: Entity) -> bool {
        app.world()
            .entity(mesh)
            .get::<InheritedVisibility>()
            .unwrap()
            .get()
    }

    fn visibility_of(app: &App, door: Entity) -> Visibility {
        *app.world().entity(door).get::<Visibility>().unwrap()
    }

    /// The door the portal is rendering through, written where [`update_portal`] writes it.
    ///
    /// A portal only picks a door whose destination says it is resident, and residency lives in
    /// `StreamingWorld` behind a private map that a test in this module cannot fill, so the target
    /// is set here directly. The tests below are about what the portal's choice *shows*; that a
    /// running portal makes this choice is what `--demo-tour` and the shots check at runtime.
    fn portal_shows(app: &mut App, door: Option<Entity>) {
        app.world_mut().resource_mut::<PortalState>().open_door = door;
    }

    fn layers_of(app: &App, entity: Entity) -> RenderLayers {
        app.world()
            .entity(entity)
            .get::<RenderLayers>()
            .cloned()
            .unwrap_or_default()
    }

    fn on_main_camera(app: &App, entity: Entity) -> bool {
        layers_of(app, entity).intersects(&RenderLayers::from_layers(&MAIN_CAMERA_LAYERS))
    }

    fn update(app: &mut App, times: usize) {
        for _ in 0..times {
            app.update();
        }
    }

    #[test]
    fn a_camera_in_front_of_the_door_maps_behind_the_arrival_point_looking_along_it() {
        // The pose `translate::apply_door_crossings` produces: for the interior Alftand01, the
        // XTEL arrival in absolute creation coordinates with the arrival rotation.
        let door_rotation = creation_rotation_to_bevy([0.0, 0.0, 1.0]);
        let door_position = Vec3::new(500.0, 120.0, 200.0);
        let arrival_position = creation_to_bevy(Vec3::new(-947.038, 3958.835, 591.917));
        let arrival_rotation = creation_rotation_to_bevy([0.0, 0.0, 2.96989]);
        let front = door_rotation * Vec3::NEG_Z;
        // 150 units in front of the door, looking at it.
        let camera_position = door_position + front * 150.0;
        let camera_rotation = door_rotation * Quat::from_rotation_y(PI);

        let (position, rotation) = portal_pose(
            door_position,
            door_rotation,
            arrival_position,
            arrival_rotation,
            camera_position,
            camera_rotation,
        );

        let arrival_facing = arrival_rotation * Vec3::NEG_Z;
        assert!(
            (position - (arrival_position - arrival_facing * 150.0)).length() < 1.0e-3,
            "portal camera at {position:?}"
        );
        assert!(
            ((rotation * Vec3::NEG_Z) - arrival_facing).length() < 1.0e-5,
            "the portal camera faces {:?}, the arriving player faces {arrival_facing:?}",
            rotation * Vec3::NEG_Z
        );
    }

    #[test]
    fn a_camera_at_the_door_lands_exactly_where_the_crossing_puts_it() {
        let door_rotation = creation_rotation_to_bevy([0.0, 0.0, -1.83260]);
        let door_position = Vec3::new(-4419.67, 740.95, 1304.83);
        let origin = IVec2::new(5, 4);
        let (arrival_position, arrival_rotation) =
            arrival_frame(&exterior_door().destination, origin);
        // The same arrival a crossing computes for an exterior destination: relative to the origin
        // the destination is streamed at.
        assert_eq!(
            arrival_position,
            render_position(Vec3::new(3693.815, 3074.645, 290.530), origin)
        );

        let (position, _) = portal_pose(
            door_position,
            door_rotation,
            arrival_position,
            arrival_rotation,
            door_position,
            Quat::IDENTITY,
        );
        assert!(
            position.abs_diff_eq(arrival_position, 1.0e-4),
            "standing in the doorway the portal renders from the arrival point {arrival_position:?}, got {position:?}"
        );

        // The mapping is rigid: it moves poses without stretching the destination.
        let offset = Vec3::new(80.0, 40.0, -90.0);
        let (moved, _) = portal_pose(
            door_position,
            door_rotation,
            arrival_position,
            arrival_rotation,
            door_position + offset,
            Quat::IDENTITY,
        );
        assert!(
            (moved
                - (arrival_position
                    + door_to_arrival_rotation(door_rotation, arrival_rotation) * offset))
                .length()
                < 1.0e-3
        );
        assert!(((moved - arrival_position).length() - offset.length()).abs() < 1.0e-2);
    }

    #[test]
    fn the_doorways_clip_distance_is_the_distance_in_front_of_the_door() {
        let door_position = Vec3::new(10.0, 0.0, -20.0);
        let door_rotation = creation_rotation_to_bevy([0.0, 0.0, 0.7]);
        let arrival_position = Vec3::new(-400.0, 130.0, 900.0);
        let arrival_rotation = creation_rotation_to_bevy([0.0, 0.0, -1.2]);
        let camera_position = door_position + (door_rotation * Vec3::NEG_Z) * 220.0;
        let (portal_position, portal_rotation) = portal_pose(
            door_position,
            door_rotation,
            arrival_position,
            arrival_rotation,
            camera_position,
            Quat::IDENTITY,
        );
        let plane = doorway_clip_plane(
            portal_position,
            portal_rotation,
            arrival_position,
            arrival_rotation * Vec3::NEG_Z,
        );
        assert!(
            (-plane.w - 220.0).abs() < 1.0e-2,
            "the doorway is 220 units in front of the camera, got {}",
            -plane.w
        );

        // Standing behind the door the distance is negative, which is what keeps the portal off.
        let behind = door_position + (door_rotation * Vec3::NEG_Z) * -50.0;
        let (portal_position, portal_rotation) = portal_pose(
            door_position,
            door_rotation,
            arrival_position,
            arrival_rotation,
            behind,
            Quat::IDENTITY,
        );
        let plane = doorway_clip_plane(
            portal_position,
            portal_rotation,
            arrival_position,
            arrival_rotation * Vec3::NEG_Z,
        );
        assert!(
            -plane.w < 0.0,
            "behind the door the distance in front is negative, got {}",
            -plane.w
        );
    }

    /// The ruined tower door's shape: its model points one way, the link that leads back into it
    /// says the other, and the portal has to open the side the link data names. The door model's
    /// own frame faces west; a door that leads back into it puts its arrival 32 units east, which
    /// is the side the player comes from.
    #[test]
    fn a_door_is_opened_from_the_side_its_link_data_gives_not_its_model_axis() {
        let door_position = Vec3::new(500.0, 120.0, 200.0);
        // A model frame that faces west: `Y(90) * -Z` is runtime -X, which is Creation -x.
        let model = Quat::from_rotation_y(core::f32::consts::FRAC_PI_2);
        assert!((model * Vec3::NEG_Z).abs_diff_eq(Vec3::NEG_X, 1.0e-5));
        let outward = [1.0, 0.0, 0.0];
        let frame = door_frame(model, Some(outward));
        assert!(
            (frame * Vec3::NEG_Z).abs_diff_eq(Vec3::X, 1.0e-5),
            "the frame faces the door's outward direction, not the model's"
        );

        let camera_position = door_position + Vec3::X * 300.0;
        assert!(
            distance_in_front_of_door(door_position, frame, camera_position) > 299.0,
            "the camera east of the door stands in front of it"
        );
        assert!(
            distance_in_front_of_door(door_position, door_frame(model, None), camera_position)
                < 0.0,
            "on the model's own axis the same camera is behind the door, which is the side the \
             portal used to open"
        );

        // The portal picks the door for that camera, and would not with only the model's frame.
        let door = LoadDoor {
            outward: Some(outward),
            ..interior_door(INTERIOR_ALFTAND01)
        };
        let space = ActiveSpace {
            interior: Some(0x0005_6C1B),
            worldspace_id: TAMRIEL,
            center: IVec2::ZERO,
            radius: 1,
        };
        let portal_camera = Entity::from_raw_u32(1).unwrap();
        let open = DoorState::Open { animated: false };
        let candidates = [(portal_camera, door_position, &door, Some(&open), None)];
        let front = |door: &LoadDoor| {
            distance_in_front_of_door(
                door_position,
                door_frame(model, door.outward),
                camera_position,
            )
        };
        assert_eq!(
            select_portal_door(
                camera_position,
                candidates,
                |_, _| true,
                &space,
                |_| front(&door)
            ),
            Some(portal_camera)
        );
        let model_only = LoadDoor {
            outward: None,
            ..door.clone()
        };
        assert_eq!(
            select_portal_door(
                camera_position,
                [(portal_camera, door_position, &model_only, Some(&open), None)],
                |_, _| true,
                &space,
                |_| front(&model_only)
            ),
            None,
            "behind the door on its model's axis there is no window to look through"
        );

        // Standing in front and looking into the door - west, along the frame's `+Z` - the portal
        // camera looks along the arrival heading: the tower door's link arrives facing 92 degrees.
        let arrival_position = creation_to_bevy(Vec3::new(-947.038, 3958.835, 591.917));
        let arrival_rotation = creation_rotation_to_bevy([0.0, 0.0, 92.0_f32.to_radians()]);
        let (position, rotation) = portal_pose(
            door_position,
            frame,
            arrival_position,
            arrival_rotation,
            camera_position,
            frame * Quat::from_rotation_y(PI),
        );
        let arrival_facing = arrival_rotation * Vec3::NEG_Z;
        assert!(
            (position - (arrival_position - arrival_facing * 300.0)).length() < 1.0e-3,
            "300 units in front of the door maps to 300 behind the arrival point, got {position:?}"
        );
        assert!(
            (rotation * Vec3::NEG_Z).abs_diff_eq(arrival_facing, 1.0e-5),
            "looking into the door maps onto the arrival heading, got {:?}",
            rotation * Vec3::NEG_Z
        );
    }

    /// The number that decides which side of a door the camera is on is the number its portal
    /// camera's projection clips at: the door's plane is carried onto the arrival doorway by the
    /// same mapping the camera is. `select_portal_door` and `update_portal` read this one number,
    /// so a door the portal picks is never one its own camera is standing behind.
    #[test]
    fn the_front_distance_is_the_distance_the_portal_camera_clips_at() {
        let door_position = Vec3::new(-120.0, 460.0, 880.0);
        let arrival_position = Vec3::new(700.0, 30.0, -240.0);
        let arrival_rotation = creation_rotation_to_bevy([0.0, 0.0, -2.4]);
        for frame in [
            // From the link data, from the model, and from link data that turns the model a
            // quarter turn - the three frames the portal can be mapping through.
            door_frame(
                Quat::from_rotation_y(core::f32::consts::FRAC_PI_2),
                Some([1.0, 0.0, 0.0]),
            ),
            door_frame(Quat::from_rotation_y(core::f32::consts::FRAC_PI_2), None),
            door_frame(
                creation_rotation_to_bevy([0.0, 0.0, 0.7]),
                Some([0.0, -1.0, 0.0]),
            ),
        ] {
            let front = frame * Vec3::NEG_Z;
            for camera in [
                door_position + front * 220.0,
                door_position - front * 50.0,
                door_position + Vec3::new(30.0, 40.0, 60.0),
            ] {
                let (portal_position, portal_rotation) = portal_pose(
                    door_position,
                    frame,
                    arrival_position,
                    arrival_rotation,
                    camera,
                    Quat::IDENTITY,
                );
                let plane = doorway_clip_plane(
                    portal_position,
                    portal_rotation,
                    arrival_position,
                    arrival_rotation * Vec3::NEG_Z,
                );
                let distance = distance_in_front_of_door(door_position, frame, camera);
                assert!(
                    (-plane.w - distance).abs() < 1.0e-2,
                    "the door is {distance} in front, the clip plane says {}",
                    -plane.w
                );
            }
        }
    }

    /// A main-camera perspective projection and the same one for a portal camera looking through a
    /// doorway at `doorway_point` with `doorway_normal`, in the portal camera's own view space.
    fn portal_clip_case(doorway_point: Vec3, doorway_normal: Vec3, fov: f32) -> (Mat4, Mat4, f32) {
        let main = Projection::Perspective(PerspectiveProjection {
            fov,
            aspect_ratio: 1.0,
            near: 0.1,
            far: 100_000.0,
            near_clip_plane: Vec4::new(0.0, 0.0, -1.0, -0.1),
        });
        let plane = doorway_clip_plane(Vec3::ZERO, Quat::IDENTITY, doorway_point, doorway_normal);
        let Projection::Perspective(perspective) = &main else {
            unreachable!()
        };
        let plain = perspective.get_clip_from_view();
        let Projection::Perspective(perspective) = portal_projection(&main, plane, -plane.w) else {
            unreachable!()
        };
        assert_eq!(perspective.fov, fov);
        assert_eq!(perspective.aspect_ratio, 1.0);
        assert_eq!(perspective.near_clip_plane, plane);
        assert!(
            perspective.near >= -plane.w,
            "the near plane is at least the doorway distance"
        );
        (plain, perspective.get_clip_from_view(), -plane.w)
    }

    #[test]
    fn geometry_between_the_portal_camera_and_the_doorway_is_clipped() {
        let ndc = |matrix: Mat4, point: Vec3| {
            let clip = matrix * point.extend(1.0);
            clip.truncate() / clip.w
        };
        let clipped = |matrix: Mat4, point: Vec3| {
            let clip = matrix * point.extend(1.0);
            if clip.w <= 0.0 {
                return true;
            }
            !(0.0..=1.0).contains(&(clip.z / clip.w))
        };

        // A camera looking straight at a doorway. The oblique adjustment is skipped - its early-out
        // tests only the plane's normal, and a doorway seen square on has the view axis as its
        // normal - so the near plane, put at the same distance, is what clips.
        let (plain, square, distance) = portal_clip_case(
            Vec3::new(0.0, 0.0, -150.0),
            Vec3::NEG_Z,
            core::f32::consts::FRAC_PI_2,
        );
        assert!((distance - 150.0).abs() < 1.0e-3, "{distance}");
        let between = Vec3::new(0.0, 0.0, -50.0);
        assert!(
            clipped(square, between),
            "geometry between the portal camera and the doorway must be clipped: {:?}",
            ndc(square, between)
        );
        assert!(!clipped(plain, between), "without the doorway it is drawn");

        // A doorway seen at an angle, where the oblique plane itself does the cutting: this point
        // is inside the frustum's sides and only the doorway plane can reject it.
        let (_, angled, distance) = portal_clip_case(
            Vec3::new(0.0, 0.0, -150.0),
            Vec3::new(0.0, 0.5, -0.866_025_4),
            core::f32::consts::FRAC_PI_3,
        );
        assert!((distance - 129.9).abs() < 1.0e-1, "{distance}");
        let beside = Vec3::new(0.0, 30.0, -60.0);
        assert!(
            clipped(angled, beside),
            "a point on the camera's side of a tilted doorway is clipped: {:?}",
            ndc(angled, beside)
        );

        // Everything beyond the doorway stays inside the frustum, and lands on the same pixel it
        // did without the doorway: that is what makes the doorway line up with the main view.
        for point in [
            Vec3::new(0.0, 0.0, -151.0),
            Vec3::new(200.0, -140.0, -900.0),
            Vec3::new(-1500.0, 700.0, -4000.0),
        ] {
            let far = ndc(square, point);
            assert!(
                !clipped(square, point),
                "{point:?} is beyond the doorway but got depth {far:?}"
            );
            let plain_far = ndc(plain, point);
            assert!(
                (plain_far.x - far.x).abs() < 1.0e-5 && (plain_far.y - far.y).abs() < 1.0e-5,
                "{point:?} projects to {plain_far:?} without the doorway and {far:?} with it"
            );
        }
    }

    #[test]
    fn a_destination_is_the_interior_or_the_grid_around_an_exterior_arrival_point() {
        assert_eq!(
            destination_keys(&interior_door(INTERIOR_ALFTAND01).destination, None),
            vec![CellKey::Interior(INTERIOR_ALFTAND01)]
        );

        // The Blackreach door of the demo route arrives at 21088.559, 18512.045, 2434.0, grid 5,4.
        let keys = destination_keys(
            &LoadDoor {
                ref_id: 0x6998D,
                destination: DoorDestination {
                    destination_ref_id: 0x4E504,
                    interior_cell_id: None,
                    worldspace_id: Some(0x0001_EE62),
                    arrival_position: [21088.559, 18512.045, 2434.0],
                    arrival_rotation: [0.0, 0.0, -1.87080],
                },
                label: "Blackreach".into(),
                auto_load: false,
                outward: None,
            }
            .destination,
            None,
        );
        assert_eq!(keys.len(), 9);
        assert!(keys.contains(&CellKey::Exterior {
            worldspace_id: 0x0001_EE62,
            grid_x: 5,
            grid_y: 4,
        }));
        assert!(!keys.contains(&CellKey::Exterior {
            worldspace_id: 0x0001_EE62,
            grid_x: 3,
            grid_y: 3,
        }));
    }

    #[test]
    fn a_destination_is_only_ready_once_its_arrival_cell_is_resident() {
        let streaming = StreamingWorld::default();
        assert!(!destination_is_resident(
            &interior_door(INTERIOR_ALFTAND01).destination,
            None,
            &streaming
        ));
        assert!(!destination_is_resident(
            &exterior_door().destination,
            None,
            &streaming
        ));
    }

    #[test]
    fn the_portal_takes_the_nearest_door_whose_destination_is_ready() {
        let camera = Vec3::ZERO;
        let space = ActiveSpace {
            interior: Some(INTERIOR_ALFTAND01),
            worldspace_id: TAMRIEL,
            center: IVec2::ZERO,
            radius: 1,
        };
        let near = LoadDoor {
            ref_id: 1,
            ..interior_door(2)
        };
        let far = LoadDoor {
            ref_id: 2,
            ..interior_door(3)
        };
        let unready = LoadDoor {
            ref_id: 3,
            ..interior_door(4)
        };
        let outside = LoadDoor {
            ref_id: 4,
            destination: DoorDestination {
                interior_cell_id: None,
                worldspace_id: None,
                ..interior_door(5).destination
            },
            ..interior_door(5)
        };
        let near_door = Entity::from_raw_u32(1).unwrap();
        let far_door = Entity::from_raw_u32(2).unwrap();
        let unready_door = Entity::from_raw_u32(3).unwrap();
        let behind_door = Entity::from_raw_u32(4).unwrap();
        let beyond_door = Entity::from_raw_u32(5).unwrap();
        let open = DoorState::Open { animated: false };
        let doors = [
            (
                near_door,
                Vec3::new(0.0, 0.0, -300.0),
                &near,
                Some(&open),
                None,
            ),
            (
                far_door,
                Vec3::new(0.0, 0.0, -700.0),
                &far,
                Some(&open),
                None,
            ),
            (
                unready_door,
                Vec3::new(0.0, 0.0, -50.0),
                &unready,
                Some(&open),
                None,
            ),
            (
                behind_door,
                Vec3::new(0.0, 0.0, -10.0),
                &outside,
                Some(&open),
                None,
            ),
        ];
        let ready = |destination: &DoorDestination, _: Option<&DoorAnchor>| {
            destination.interior_cell_id != Some(4)
        };
        // The camera is behind the last door's plane, where the window has no content.
        let front = |entity: Entity| if entity == behind_door { -5.0 } else { 100.0 };

        assert_eq!(
            select_portal_door(camera, doors, ready, &space, front),
            Some(near_door),
            "the nearer door with a ready destination wins"
        );

        // A door beyond the pre-stream radius is not a candidate even when it is the only one.
        let far_only = [(
            beyond_door,
            Vec3::new(0.0, 0.0, -(DOOR_PRESTREAM_RADIUS + 1.0)),
            &near,
            Some(&open),
            None,
        )];
        assert_eq!(
            select_portal_door(camera, far_only, ready, &space, |_| 100.0),
            None
        );

        // A door still swinging, and one whose clip has run out and left its leaves in the doorway,
        // are both open: `is_open` is the question the portal asks of a state.
        for state in [DoorState::Opening, DoorState::Open { animated: true }] {
            assert!(door_is_open(Some(&state)), "{state:?} is an open door");
            let mut with_an_open_near_door = doors;
            with_an_open_near_door[0].3 = Some(&state);
            assert_eq!(
                select_portal_door(camera, with_an_open_near_door, ready, &space, front),
                Some(near_door),
                "an open door is looked through: {state:?}"
            );
        }

        // A closed door is not, however close and ready its destination is: the window stands in
        // the only opening a door has, so the destination image would be behind its leaf. Every
        // state that is not `Opening` or `Open` counts as closed, and so does no state at all.
        for state in [None, Some(&DoorState::Closed), Some(&DoorState::Closing)] {
            assert!(
                !door_is_open(state),
                "the state under test is one the portal must not render through: {state:?}"
            );
            let mut with_a_closed_near_door = doors;
            with_a_closed_near_door[0].3 = state;
            assert_eq!(
                select_portal_door(camera, with_a_closed_near_door, ready, &space, front),
                Some(far_door),
                "the open door behind the closed one is the one to look through: {state:?}"
            );
        }
    }

    #[test]
    fn a_doorway_is_measured_from_the_doors_own_bounds() {
        // A door whose model measures 200 x 300, scaled to 100 wide and 150 tall by the reference.
        // The model-space box is the door's own frame, so no rotation of the reference moves it.
        let model = ExpectedModelBounds {
            min: Vec3::new(-100.0, 0.0, -10.0),
            max: Vec3::new(100.0, 150.0, 10.0),
        };
        let quarter_turn = Quat::from_rotation_y(core::f32::consts::FRAC_PI_2);
        let (size, centre) = portal_quad_extents(
            None,
            Some(&model),
            quarter_turn,
            quarter_turn,
            Vec3::new(0.5, 2.0, 1.0),
        );
        assert!(
            (size.x - 100.0).abs() < 1.0e-3 && (size.y - 300.0).abs() < 1.0e-3,
            "{size:?}"
        );
        assert!((centre.y - 150.0).abs() < 1.0e-3, "{centre:?}");

        // Where both are carried the model box wins: an axis-aligned world box of a turned door
        // would measure the door's diagonal.
        let (size, _) = portal_quad_extents(
            Some(&InstanceBounds {
                min: Vec3::splat(-500.0),
                max: Vec3::splat(500.0),
            }),
            Some(&model),
            quarter_turn,
            quarter_turn,
            Vec3::new(0.5, 2.0, 1.0),
        );
        assert!(
            (size.x - 100.0).abs() < 1.0e-3 && (size.y - 300.0).abs() < 1.0e-3,
            "{size:?}"
        );

        // A reference with only a placed world box: the box is turned back into the door's frame,
        // which measures a door on an axis exactly and over-estimates one at an angle.
        let bounds = InstanceBounds {
            min: Vec3::new(-20.0, 0.0, -100.0),
            max: Vec3::new(20.0, 300.0, 100.0),
        };
        let (size, centre) =
            portal_quad_extents(Some(&bounds), None, quarter_turn, quarter_turn, Vec3::ONE);
        assert!(
            (size.x - 200.0).abs() < 1.0e-3 && (size.y - 300.0).abs() < 1.0e-3,
            "the 200-unit axis lies across the world's z here: {size:?}"
        );
        assert!((centre.y - 150.0).abs() < 1.0e-3, "{centre:?}");
        let (size, _) = portal_quad_extents(
            Some(&bounds),
            None,
            Quat::IDENTITY,
            Quat::IDENTITY,
            Vec3::ONE,
        );
        assert!(
            (size.x - 40.0).abs() < 1.0e-3,
            "unrotated the box itself is the doorway: {size:?}"
        );

        // A marker with no converted bounds, and a degenerate one, get the default doorway.
        for (instance, expected) in [
            (None, None),
            (
                Some(InstanceBounds {
                    min: Vec3::ZERO,
                    max: Vec3::new(0.0, 1.0, 0.0),
                }),
                None,
            ),
        ] {
            let (size, centre) = portal_quad_extents(
                instance.as_ref(),
                expected,
                Quat::IDENTITY,
                Quat::IDENTITY,
                Vec3::ONE,
            );
            assert_eq!(size, DEFAULT_PORTAL_SIZE);
            assert!((centre.y - DEFAULT_PORTAL_SIZE.y * 0.5).abs() < 1.0e-3);
        }
    }

    /// The doorway quad is one thing in one frame. The doorway is the model's geometry and the side
    /// a player walks in from is the link's, and where the two disagree by a quarter turn - which
    /// [`LoadDoor::outward`] says a fifth of Skyrim.esm's load doors do - they are two different
    /// planes: a quad measured in the model's frame and turned in the front frame, or the other way
    /// round, stands edge-on to the opening the player walks in through.
    ///
    /// Laid out in the front frame, the window covers that opening, and it still stands on the point
    /// the crossing fires on - the doorway's own centre, which is what the portal and
    /// [`crate::player`]'s trigger have to agree on.
    #[test]
    fn the_doorway_quad_is_laid_out_in_the_frame_the_doors_front_comes_from() {
        // A model whose doorway is 200 wide, 305 tall and 64 deep, its opening 12 units off the
        // reference along the model's own `z` (the demo's dwemer doors hang theirs eight units off).
        let bounds = ExpectedModelBounds {
            min: Vec3::new(-100.0, -5.0, -20.0),
            max: Vec3::new(100.0, 300.0, 44.0),
        };
        let centre_of_the_box = Vec3::new(0.0, 147.5, 12.0);
        // The model's own front is Creation north, so the other three directions are a quarter, a
        // half and three quarters of a turn away from it.
        let model = Quat::IDENTITY;

        for (outward, size) in [
            // On the model's axis: the doorway as the model measures it.
            ([0.0, 1.0, 0.0], Vec2::new(200.0, 305.0)),
            // A quarter turn: the aperture a player walking along the frame's front sees is the
            // box's `x`/`z` side, 64 units wide - and a window still measured off the model's own
            // `x`/`y` face would be a 200-wide slab standing across the walk.
            ([1.0, 0.0, 0.0], Vec2::new(64.0, 305.0)),
            // Half a turn - the commonest disagreement of all, and the one the Ruined Tower door
            // has: the box's own face, turned round.
            ([0.0, -1.0, 0.0], Vec2::new(200.0, 305.0)),
            ([-1.0, 0.0, 0.0], Vec2::new(64.0, 305.0)),
        ] {
            let frame = door_frame(model, Some(outward));
            let (measured, centre) =
                portal_quad_extents(None, Some(&bounds), model, frame, Vec3::ONE);
            assert!(
                measured.abs_diff_eq(size, 1.0e-3),
                "outward {outward:?}: the doorway is {size:?}, measured {measured:?}"
            );
            assert!(
                (frame * centre).abs_diff_eq(model * centre_of_the_box, 1.0e-3),
                "outward {outward:?}: the quad stands at {:?}, the doorway is at {:?} - and that \
                 point is the one the crossing's plane passes through",
                frame * centre,
                model * centre_of_the_box
            );
        }
    }

    /// A door draws its own leaf - closed - until the portal renders through it, and the portal
    /// renders through exactly one door at a time.
    ///
    /// The target is written where [`update_portal`] writes it (see [`portal_shows`]): *which* door
    /// a running portal picks is a question about the door states, and
    /// `the_portal_takes_the_nearest_door_whose_destination_is_ready` asks that one. This is what
    /// the choice shows.
    #[test]
    fn the_portal_opens_one_door_and_closes_it_again() {
        let mut app = portal_app();
        let (a, a_mesh) = spawn_door(&mut app, interior_door(0x0005_6C1B));
        let (b, b_mesh) = spawn_door(&mut app, interior_door(0x0005_6C1C));

        update(&mut app, 1);
        assert!(
            leaf_is_drawn(&app, a_mesh) && leaf_is_drawn(&app, b_mesh),
            "with no portal up, every doorway is a closed door"
        );

        // A is the door the portal picked: its leaf is gone in the frame the portal takes it, so
        // there is no frame in which the doorway is a hole with a leaf in it or a leaf with a hole.
        portal_shows(&mut app, Some(a));
        update(&mut app, 1);
        assert_eq!(visibility_of(&app, a), Visibility::Hidden);
        assert!(!leaf_is_drawn(&app, a_mesh), "A's doorway is an opening");
        assert!(
            leaf_is_drawn(&app, b_mesh),
            "a door the portal does not render through keeps its leaf"
        );

        // Retargeting to B closes A and opens B in that same frame.
        portal_shows(&mut app, Some(b));
        update(&mut app, 1);
        assert!(!leaf_is_drawn(&app, b_mesh));
        assert!(
            leaf_is_drawn(&app, a_mesh),
            "A is closed again in the frame the portal leaves it"
        );

        // Turning the portal off closes the last door too.
        portal_shows(&mut app, None);
        update(&mut app, 1);
        assert!(
            leaf_is_drawn(&app, a_mesh) && leaf_is_drawn(&app, b_mesh),
            "no portal, no open doorway"
        );
    }

    /// An auto-load door is an invisible marker, not a leaf: nothing the portal does draws it.
    #[test]
    fn an_auto_load_marker_never_draws() {
        let mut app = portal_app();
        let mut marker = interior_door(0x0005_6C1B);
        marker.auto_load = true;
        let (marker, marker_mesh) = spawn_door(&mut app, marker);
        let (door, door_mesh) = spawn_door(&mut app, interior_door(0x0005_6C1C));

        // Even as the door the portal is showing, where a real door's leaf would be hidden - and
        // the real door beside it, which the portal is not showing, keeps its leaf.
        portal_shows(&mut app, Some(marker));
        update(&mut app, 1);
        assert_eq!(visibility_of(&app, marker), Visibility::Hidden);
        assert!(!leaf_is_drawn(&app, marker_mesh));
        assert!(leaf_is_drawn(&app, door_mesh));

        // And with the portal on a real door - the player has opened it, which is the only kind a
        // running portal renders through - only that door is an opening.
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: false });
        portal_shows(&mut app, Some(door));
        update(&mut app, 1);
        assert!(!leaf_is_drawn(&app, door_mesh));
        assert!(!leaf_is_drawn(&app, marker_mesh));
        assert_eq!(visibility_of(&app, marker), Visibility::Hidden);
    }

    /// The door the portal showed is closed again as soon as no portal is rendering, whichever way
    /// it stops: the door here is a closed one - the target is written directly, and a running
    /// portal only ever picks an open door - so its leaf has to be back the frame the portal lets
    /// it go. This runs the real [`update_portal`] - here one that cannot place a camera at all,
    /// since the app has no `StreamingWorld` and so no resident destination - so it is also the
    /// test that the target a running portal has to publish is the one the door system reads.
    #[test]
    fn a_portal_that_stops_placing_its_camera_closes_its_door() {
        let mut app = portal_app();
        let (door, mesh) = spawn_door(&mut app, interior_door(0x0005_6C1B));
        portal_shows(&mut app, Some(door));
        update(&mut app, 1);
        assert!(
            !leaf_is_drawn(&app, mesh),
            "the portal is rendering through it"
        );

        add_update_portal(&mut app);
        update(&mut app, 1);
        assert_eq!(
            app.world().resource::<PortalState>().open_door,
            None,
            "a portal that cannot place its camera renders through no door"
        );
        assert!(
            leaf_is_drawn(&app, mesh),
            "so the door draws its leaf again, in that same frame"
        );
    }

    /// The quad stands in the doorway's own plane, and that plane is the one the player's crossing
    /// fires on: the window is drawn exactly up to the frame of the swap, and no frame after it.
    ///
    /// This is the one invariant between the portal and the crossing that the swap depends on. The
    /// portal stands the quad on the doorway box's own centre (`measured_doorway_box`, laid out by
    /// [`doorway_in_frame`]) and the crossing builds its plane from the doorway volume
    /// `auto_door_trigger` builds out of the same box; they have to name the same point.
    #[test]
    fn the_window_stands_in_the_plane_the_crossing_fires_on() {
        let door_rotation = creation_rotation_to_bevy([0.0, 0.0, 0.7]);
        let position = Vec3::new(-400.0, 260.0, 900.0);
        let scale = Vec3::new(0.5, 2.0, 1.0);
        let bounds = ExpectedModelBounds {
            min: Vec3::new(-100.0, -5.0, -20.0),
            max: Vec3::new(100.0, 300.0, 44.0),
        };
        let (_, centre) =
            portal_quad_extents(None, Some(&bounds), door_rotation, door_rotation, scale);
        let window = position + door_rotation * centre;
        let trigger =
            crate::player::auto_door_trigger(position, door_rotation, scale, None, Some(&bounds));
        assert!(
            trigger.centre().abs_diff_eq(window, 1.0e-3),
            "the doorway volume is centred on {:?}, the window stands on {:?}",
            trigger.centre(),
            window
        );
        assert_eq!(
            PORTAL_QUAD_OFFSET, 0.0,
            "the window does not stand off the plane"
        );
    }

    /// A door the player has opened keeps its leaf hidden as they walk into it, even where the
    /// portal has stopped rendering - half a unit in front of the plane, inside
    /// [`MIN_PORTAL_DOOR_DISTANCE`] - and a door nobody opened still draws its own leaf (design
    /// section 4.7).
    #[test]
    fn an_opened_door_stays_open_with_the_camera_in_the_doorway() {
        let mut app = portal_app();
        let (door, mesh) = spawn_door(&mut app, interior_door(0x0005_6C1B));
        let (closed, closed_mesh) = spawn_door(&mut app, interior_door(0x0005_6C1C));
        // The player has walked up to the doorway and through it: half a unit in front of its
        // plane, which is where the portal gives up and clears the door it was showing.
        let camera = spawn_camera(&mut app, Vec3::new(0.0, 0.0, -0.5));
        add_update_portal(&mut app);
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: false });
        update(&mut app, 1);

        let camera_position = app
            .world()
            .entity(camera)
            .get::<GlobalTransform>()
            .unwrap()
            .translation();
        assert!(
            distance_in_front_of_door(Vec3::ZERO, Quat::IDENTITY, camera_position)
                < MIN_PORTAL_DOOR_DISTANCE,
            "the camera is inside the gap the portal does not render through"
        );
        assert_eq!(
            app.world().resource::<PortalState>().open_door,
            None,
            "so no portal is up for this door"
        );
        assert!(
            !leaf_is_drawn(&app, mesh),
            "the opened door's leaf is gone anyway: the doorway is the way through it"
        );
        assert!(
            leaf_is_drawn(&app, closed_mesh),
            "a door nobody opened draws its own leaf"
        );
        assert_eq!(visibility_of(&app, closed), Visibility::Inherited);
    }

    /// A door whose crossing is waiting for its destination to stream in draws its own model: the
    /// portal has no window to show through the doorway while it waits - the destination is exactly
    /// what is missing - so a door hidden here would be a hole in the world where the doorway is.
    #[test]
    fn a_door_holding_its_crossing_draws_its_own_model() {
        let mut app = portal_app();
        let (door, mesh) = spawn_door(&mut app, interior_door(0x0005_6C1B));
        app.world_mut()
            .entity_mut(door)
            .insert((DoorState::Open { animated: false }, CrossingHeld));
        update(&mut app, 1);
        assert_eq!(
            visibility_of(&app, door),
            Visibility::Inherited,
            "a door waiting for its destination is drawn as the door it is"
        );
        assert!(leaf_is_drawn(&app, mesh));

        // The hold is over: it is an open static door again, and its model goes.
        app.world_mut().entity_mut(door).remove::<CrossingHeld>();
        update(&mut app, 1);
        assert_eq!(visibility_of(&app, door), Visibility::Hidden);
        assert!(!leaf_is_drawn(&app, mesh));
    }

    /// An animated door is not the portal's to hide. Its frame *is* the doorway - a model hidden
    /// while the quad stands in it would take the door with it - and its leaves are
    /// [`crate::door_animation`]'s to draw and hide: the swing is exactly what the player asked to
    /// see, and a doorway that emptied itself the moment it was asked to open would be no better
    /// than the teleport this feature replaces.
    #[test]
    fn an_animated_doors_model_is_not_the_portals_to_hide() {
        let mut app = portal_app();
        let (animated, animated_mesh) = spawn_door(&mut app, interior_door(0x0005_6C1B));
        let (still, still_mesh) = spawn_door(&mut app, interior_door(0x0005_6C1C));

        // Both open, the animated one mid-swing and the portal rendering through it.
        app.world_mut()
            .entity_mut(animated)
            .insert(DoorState::Opening);
        app.world_mut()
            .entity_mut(still)
            .insert(DoorState::Open { animated: false });
        portal_shows(&mut app, Some(animated));
        update(&mut app, 1);

        assert_eq!(
            visibility_of(&app, animated),
            Visibility::Inherited,
            "the animated door's frame and leaves are drawn: its animation is the opening"
        );
        assert!(leaf_is_drawn(&app, animated_mesh));
        assert_eq!(
            visibility_of(&app, still),
            Visibility::Hidden,
            "and a static door with no leaf to swing out of the way is still a hole"
        );
        assert!(!leaf_is_drawn(&app, still_mesh));

        // Open with the clip finished, the animated door is still its own model's business: the
        // leaves are `door_animation`'s answer (hidden when the swing left them in the opening,
        // drawn when it swung them clear), never this system's.
        app.world_mut()
            .entity_mut(animated)
            .insert(DoorState::Open { animated: true });
        portal_shows(&mut app, None);
        update(&mut app, 1);
        assert_eq!(visibility_of(&app, animated), Visibility::Inherited);
        assert!(leaf_is_drawn(&app, animated_mesh));
    }

    #[test]
    fn a_prestreamed_cell_is_off_the_main_camera_until_it_becomes_active() {
        let mut app = portal_app();
        spawn_camera(&mut app, Vec3::ZERO);
        let (active_root, active_mesh) = spawn_cell(&mut app, INTERIOR_ALFTAND01, None, None, None);
        // The interior behind the door the player walks to: pre-streamed, and not the portal's
        // destination, because no door in an active cell leads to it yet.
        let (prestream_root, prestream_mesh) = spawn_cell(&mut app, 0x0005_6C1B, None, None, None);
        // A water surface of that cell belongs to the reflection layer when it is active.
        let (water, _) = spawn_mesh(&mut app, prestream_root, Some(RenderLayers::layer(1)));

        update(&mut app, 2);

        assert!(
            on_main_camera(&app, active_mesh),
            "the active cell is drawn"
        );
        assert_eq!(
            *app.world().entity(active_root).get::<Visibility>().unwrap(),
            Visibility::default()
        );
        assert!(
            !on_main_camera(&app, prestream_mesh),
            "a pre-streamed cell must not be drawn where the player stands"
        );
        assert!(!on_main_camera(&app, water));
        assert_eq!(
            *app.world()
                .entity(prestream_root)
                .get::<Visibility>()
                .unwrap(),
            Visibility::Hidden,
            "and the ray cast must not hit it either"
        );

        // A mesh of the same cell appearing a frame later - the glTF scene loads over several
        // frames - is hidden too.
        let (late, _) = spawn_mesh(&mut app, prestream_root, None);
        update(&mut app, 1);
        assert!(!on_main_camera(&app, late));

        // Entering the cell shows it again, meshes and layers alike.
        *app.world_mut().resource_mut::<ActiveCell>() = ActiveCell {
            worldspace_id: TAMRIEL,
            interior: Some(0x0005_6C1B),
        };
        update(&mut app, 2);
        assert!(on_main_camera(&app, prestream_mesh));
        assert!(on_main_camera(&app, late));
        assert_eq!(
            layers_of(&app, water),
            RenderLayers::layer(1),
            "a water surface keeps the layer it was spawned with"
        );
        assert_eq!(
            *app.world()
                .entity(prestream_root)
                .get::<Visibility>()
                .unwrap(),
            Visibility::default()
        );
        assert!(
            !on_main_camera(&app, active_mesh),
            "and the cell left is not"
        );
    }

    #[test]
    fn a_cell_of_another_worldspace_stays_hidden_when_its_grid_matches_the_active_one() {
        let mut app = portal_app();
        *app.world_mut().resource_mut::<ActiveCell>() = ActiveCell {
            worldspace_id: TAMRIEL,
            interior: None,
        };
        spawn_camera(&mut app, Vec3::ZERO);
        // The camera is at the render origin, so its grid is the origin's own.
        let grid = IVec2::new(19, 18);
        let (tamriel_root, tamriel_mesh) = spawn_cell(
            &mut app,
            TAMRIEL * 100,
            Some(grid),
            Some(CellKey::Exterior {
                worldspace_id: TAMRIEL,
                grid_x: grid.x,
                grid_y: grid.y,
            }),
            None,
        );
        // AlftandWorld's door 4 arrives at Blackreach grid (5,4) while AlftandWorld's own cells sit
        // at grid (0,0) and lower: the grids of two worldspaces can coincide, and only the key says
        // which one is the active space.
        let (blackreach_root, blackreach_mesh) = spawn_cell(
            &mut app,
            0x0001_EE62 * 100,
            Some(grid),
            Some(CellKey::Exterior {
                worldspace_id: 0x0001_EE62,
                grid_x: grid.x,
                grid_y: grid.y,
            }),
            None,
        );

        update(&mut app, 2);

        assert!(on_main_camera(&app, tamriel_mesh));
        assert_eq!(
            *app.world()
                .entity(tamriel_root)
                .get::<Visibility>()
                .unwrap(),
            Visibility::default()
        );
        assert!(!on_main_camera(&app, blackreach_mesh));
        assert_eq!(
            *app.world()
                .entity(blackreach_root)
                .get::<Visibility>()
                .unwrap(),
            Visibility::Hidden
        );
    }

    // -----------------------------------------------------------------------------------------
    // The doorway image: its size and its range (notes 1 and 2 of the brief)
    // -----------------------------------------------------------------------------------------

    /// A camera whose computed target is `size`: what `camera_system` leaves on a camera that
    /// draws into a target of that many physical pixels.
    fn camera_targeting(size: UVec2) -> Camera {
        let mut camera = Camera::default();
        camera.computed.target_info = Some(RenderTargetInfo {
            physical_size: size,
            scale_factor: 1.0,
        });
        camera
    }

    /// An app with the three things [`resize_portal_target`] repoints - the target resource, the
    /// portal camera's `RenderTarget` and the quad's material - plus a main camera the test moves.
    fn resize_app(target: UVec2) -> (App, Entity, Entity, Entity, Handle<Image>) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>()
            .init_asset::<PortalMaterial>();
        let fallback = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(portal_target_image(PORTAL_TEXTURE_FALLBACK_SIZE));
        app.insert_resource(PortalTexture(fallback.clone()));
        let material = app
            .world_mut()
            .resource_mut::<Assets<PortalMaterial>>()
            .add(PortalMaterial::default());
        let camera = app
            .world_mut()
            .spawn((
                PortalCamera,
                Camera::default(),
                RenderTarget::Image(fallback.clone().into()),
            ))
            .id();
        let quad = app
            .world_mut()
            .spawn((PortalQuad, MeshMaterial3d(material)))
            .id();
        let main = app
            .world_mut()
            .spawn((StreamingCamera, camera_targeting(target)))
            .id();
        app.add_systems(Update, resize_portal_target);
        (app, main, camera, quad, fallback)
    }

    /// The image the portal's target resource points at.
    fn target_image(app: &App) -> Handle<Image> {
        app.world().resource::<PortalTexture>().0.clone()
    }

    /// The size of a target image, from the asset itself.
    fn target_size_of(app: &App, image: &Handle<Image>) -> UVec2 {
        app.world()
            .resource::<Assets<Image>>()
            .get(image)
            .expect("the portal target image")
            .size()
    }

    /// The image the portal camera renders into.
    fn camera_target_of(app: &App, camera: Entity) -> Handle<Image> {
        match app.world().entity(camera).get::<RenderTarget>().unwrap() {
            RenderTarget::Image(target) => target.handle.clone(),
            other => panic!("the portal camera draws into {other:?}"),
        }
    }

    /// The image the doorway quad's material samples.
    fn quad_texture_of(app: &App, quad: Entity) -> Handle<Image> {
        let handle = app
            .world()
            .entity(quad)
            .get::<MeshMaterial3d<PortalMaterial>>()
            .unwrap();
        app.world()
            .resource::<Assets<PortalMaterial>>()
            .get(handle)
            .expect("the quad's material")
            .extension
            .portal_texture
            .clone()
            .expect("a portal texture")
    }

    /// Sets the size the main camera's own target is.
    fn main_target_size(app: &mut App, main: Entity, size: UVec2) {
        app.world_mut()
            .entity_mut(main)
            .get_mut::<Camera>()
            .unwrap()
            .computed
            .target_info = Some(RenderTargetInfo {
            physical_size: size,
            scale_factor: 1.0,
        });
    }

    /// The size the doorway is drawn at follows the main camera's own target, resize for resize,
    /// and an unchanged size is not a resize: no image is created, and the camera and the quad keep
    /// reading the one they have.
    #[test]
    fn the_portal_target_follows_the_main_camera_and_is_only_rebuilt_when_it_changes() {
        let (mut app, main, camera, quad, fallback) = resize_app(UVec2::new(1600, 900));
        assert_eq!(
            target_size_of(&app, &fallback),
            PORTAL_TEXTURE_FALLBACK_SIZE,
            "a run starts with the fallback target"
        );

        // The first frame the main camera's target is known: the doorway is drawn at the window's
        // own resolution, and both ends of it - the camera that writes it and the quad that samples
        // it - are on the new image in that same frame.
        update(&mut app, 1);
        let window = target_image(&app);
        assert_ne!(
            window, fallback,
            "the fallback is replaced by the window's size"
        );
        assert_eq!(target_size_of(&app, &window), UVec2::new(1600, 900));
        assert_eq!(camera_target_of(&app, camera), window);
        assert_eq!(quad_texture_of(&app, quad), window);

        // Frames that change nothing allocate nothing: the same asset, and not one new image added
        // to the assets at all.
        let images = app.world().resource::<Assets<Image>>().len();
        update(&mut app, 4);
        assert_eq!(
            target_image(&app),
            window,
            "a repeated size must not ask for a new image"
        );
        assert_eq!(
            app.world().resource::<Assets<Image>>().len(),
            images,
            "and must not allocate one either"
        );

        // A resized window is a new target of the new size, repointed the same way.
        main_target_size(&mut app, main, UVec2::new(1280, 720));
        update(&mut app, 1);
        let smaller = target_image(&app);
        assert_ne!(
            smaller, window,
            "the window changed size, so the target did"
        );
        assert_eq!(target_size_of(&app, &smaller), UVec2::new(1280, 720));
        assert_eq!(camera_target_of(&app, camera), smaller);
        assert_eq!(quad_texture_of(&app, quad), smaller);

        // A window larger than the ceiling is scaled down to it, both axes by one factor.
        main_target_size(&mut app, main, UVec2::new(3840, 2160));
        update(&mut app, 1);
        assert_eq!(
            target_size_of(&app, &target_image(&app)),
            PORTAL_TEXTURE_MAX_SIZE
        );
        assert_eq!(camera_target_of(&app, camera), target_image(&app));
        assert_eq!(quad_texture_of(&app, quad), target_image(&app));
    }

    /// A window that is minimized has no size to follow - its target is 0x0 and a 0x0 texture is
    /// not a render target - so the doorway keeps the image it has until the window comes back.
    #[test]
    fn a_minimized_window_leaves_the_portal_target_where_it_was() {
        let (mut app, main, camera, _, _) = resize_app(UVec2::new(1600, 900));
        update(&mut app, 1);
        let window = target_image(&app);

        main_target_size(&mut app, main, UVec2::ZERO);
        update(&mut app, 3);
        assert_eq!(target_image(&app), window);
        assert_eq!(target_size_of(&app, &window), UVec2::new(1600, 900));
        assert_eq!(camera_target_of(&app, camera), window);

        // Half a window is no size either.
        main_target_size(&mut app, main, UVec2::new(1920, 0));
        update(&mut app, 2);
        assert_eq!(target_image(&app), window);

        // Restoring it is a resize like any other.
        main_target_size(&mut app, main, UVec2::new(1600, 900));
        update(&mut app, 1);
        assert_eq!(target_image(&app), window, "the size it already has");
    }

    /// The target size for a main camera of a given resolution: followed exactly up to the ceiling,
    /// scaled by one factor past it, and `None` when the target has no size at all.
    #[test]
    fn the_target_size_follows_the_window_and_clamps_a_large_one() {
        let ceiling = PORTAL_TEXTURE_MAX_SIZE;
        let aspect = |size: UVec2| size.x as f32 / size.y as f32;
        assert_eq!(
            portal_target_size(UVec2::new(1600, 900), ceiling),
            Some(UVec2::new(1600, 900)),
            "the window's own size, one for one"
        );
        assert_eq!(
            portal_target_size(UVec2::new(1024, 576), ceiling),
            Some(UVec2::new(1024, 576)),
            "smaller than the ceiling is followed too, not scaled up"
        );
        assert_eq!(
            portal_target_size(ceiling, ceiling),
            Some(ceiling),
            "exactly the ceiling"
        );
        assert_eq!(
            portal_target_size(UVec2::new(3840, 2160), ceiling),
            Some(ceiling),
            "a 4K window is clamped to the ceiling"
        );
        assert_eq!(
            portal_target_size(UVec2::new(7680, 4320), ceiling),
            Some(ceiling),
            "and so is an 8K one"
        );
        assert_eq!(
            portal_target_size(UVec2::ZERO, ceiling),
            None,
            "a minimized window has no size to follow"
        );
        assert_eq!(
            portal_target_size(UVec2::new(1920, 0), ceiling),
            None,
            "nor has a target with one empty axis"
        );

        // A window the ceiling does not fit keeps its pixel aspect ratio: one factor for both axes.
        let ultrawide = portal_target_size(UVec2::new(3440, 1440), ceiling).unwrap();
        assert_eq!(ultrawide.x, ceiling.x, "the long axis is at the ceiling");
        assert!(ultrawide.y < ceiling.y);
        assert!(
            (aspect(ultrawide) - aspect(UVec2::new(3440, 1440))).abs() < 0.01,
            "an ultrawide doorway must not come out stretched: {ultrawide:?}"
        );
        let portrait = portal_target_size(UVec2::new(1080, 7680), ceiling).unwrap();
        assert_eq!(
            portrait.y, ceiling.y,
            "a tall window is capped on its long axis"
        );
        assert!(portrait.x <= ceiling.x);
        assert!(
            (aspect(portrait) - aspect(UVec2::new(1080, 7680))).abs() < 0.01,
            "{portrait:?}"
        );

        // The same answer every time: the system's "is this a change?" test is what keeps the
        // target from being rebuilt every frame, and it compares against this function's answer.
        assert_eq!(
            portal_target_size(UVec2::new(3440, 1440), ceiling),
            Some(ultrawide)
        );
    }

    /// The doorway image is not an 8-bit one. It holds the destination's scene-referred values -
    /// above white included - so the main camera's tonemapper gets the same range through the
    /// doorway that it gets when the player walks into the room, and a bright destination rolls off
    /// its curve instead of arriving flattened at white (note 2 of the brief).
    #[test]
    fn the_doorway_image_is_float_and_is_not_viewed_as_srgb() {
        let image = portal_target_image(UVec2::new(64, 32));
        assert_eq!(image.size(), UVec2::new(64, 32));
        assert_eq!(
            image.texture_descriptor.format, PORTAL_TEXTURE_FORMAT,
            "the target is what a camera's main pass writes into"
        );
        assert_eq!(
            PORTAL_TEXTURE_FORMAT,
            TextureFormat::Rgba16Float,
            "four half-float channels at 8 bytes a pixel: a value over 1.0 survives in the target \
             rather than clamping, and every target the engine runs on can render into it"
        );
        assert!(
            !PORTAL_TEXTURE_FORMAT.is_srgb(),
            "an sRGB target would hold an encoded copy of the values, which is the 8-bit problem"
        );
        assert!(
            image.texture_view_descriptor.is_none(),
            "no second view format: a float target must be neither encoded on write nor decoded on \
             sample, or the round trip is what the quad's sampling loses"
        );
    }

    /// One row of the door-visibility table: (`state`, `auto_load`, the portal is rendering through
    /// this door, it is holding a crossing, it is the **destination** door of the pair the portal is
    /// rendering through, its model is drawn, what the row is).
    type DoorCase = (
        Option<DoorState>,
        bool,
        bool,
        bool,
        bool,
        bool,
        &'static str,
    );

    /// The whole table of when a load door's model is drawn, run through the system that writes it.
    ///
    /// The row that regressed is in here: an **animated** door the portal is rendering through is
    /// **drawn**. A model hidden there takes the door's frame and its swinging leaf out of the
    /// doorway the window stands in, so the player sees an empty hole where the door they just
    /// opened is - and the doorway of the room they are in stops looking like a doorway.
    /// `an_animated_doors_model_is_not_the_portals_to_hide` is the same case read through the
    /// leaf's inherited visibility, which is what the renderer looks at.
    #[test]
    fn a_doors_model_is_drawn_unless_it_is_the_hole_the_doorway_needs() {
        use DoorState::{Closed, Closing, Open, Opening};
        let cases: [DoorCase; 16] = [
            (
                None,
                false,
                false,
                false,
                false,
                true,
                "a door whose state has not been written yet is the closed door it looks like",
            ),
            (
                Some(Closed),
                false,
                false,
                false,
                false,
                true,
                "a closed door draws its leaf",
            ),
            (
                Some(Closed),
                false,
                true,
                false,
                false,
                false,
                "a door the portal is somehow showing while closed has nothing to hide behind the \
                 window - the target is only ever written for an open door",
            ),
            (
                Some(Opening),
                false,
                false,
                false,
                false,
                true,
                "a door mid-swing draws its leaf",
            ),
            (
                Some(Opening),
                false,
                true,
                false,
                false,
                true,
                "THE REGRESSED CASE: an animating door the portal renders through stays drawn",
            ),
            (
                Some(Closing),
                false,
                true,
                false,
                false,
                true,
                "a door closing behind the player is drawn coming back",
            ),
            (
                Some(Open { animated: true }),
                false,
                true,
                false,
                false,
                true,
                "an animated open door the portal renders through stays drawn",
            ),
            (
                Some(Open { animated: true }),
                false,
                false,
                false,
                false,
                true,
                "and one the portal is not rendering through is drawn too",
            ),
            (
                Some(Open { animated: false }),
                false,
                false,
                false,
                false,
                false,
                "a door with no clip has no leaf that can move: its model is the hole",
            ),
            (
                Some(Open { animated: false }),
                false,
                true,
                false,
                false,
                false,
                "including the door the portal is rendering through",
            ),
            (
                Some(Open { animated: false }),
                false,
                true,
                true,
                false,
                true,
                "unless its crossing is held: there is no window to be a hole in then",
            ),
            (
                Some(Closing),
                false,
                false,
                true,
                false,
                true,
                "and a held crossing draws whatever the state is",
            ),
            (
                Some(Open { animated: false }),
                true,
                true,
                false,
                false,
                false,
                "an auto-load marker is an invisible reference, portal or not",
            ),
            (
                Some(Open { animated: false }),
                true,
                true,
                true,
                false,
                true,
                "and it is drawn while its crossing is held, like any other door",
            ),
            (
                Some(Closed),
                false,
                false,
                false,
                true,
                false,
                "the DESTINATION door of the pair the portal renders through draws nothing: its \
                 leaf stands in the window's own clip plane and would cover the room",
            ),
            (
                Some(Open { animated: true }),
                false,
                false,
                false,
                true,
                false,
                "whatever its own state is, and even when its own animation moves it",
            ),
        ];

        let mut app = portal_app();
        let mut spawned = Vec::new();
        for (state, auto_load, _, waiting, _, _, _) in cases.iter() {
            let mut door = interior_door(0x0005_6C1B);
            door.auto_load = *auto_load;
            let (entity, mesh) = spawn_door(&mut app, door);
            if let Some(state) = state {
                app.world_mut().entity_mut(entity).insert(*state);
            }
            if *waiting {
                app.world_mut().entity_mut(entity).insert(CrossingHeld);
            }
            spawned.push((entity, mesh));
        }
        let wanted = |drawn: bool| {
            if drawn {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            }
        };

        // With no portal up, every row that is not the portal's to hide answers from its own state.
        portal_shows(&mut app, None);
        update(&mut app, 1);
        for ((_, _, portal, _, destination, drawn, what), (entity, mesh)) in
            cases.iter().zip(&spawned)
        {
            if *portal || *destination {
                continue;
            }
            assert_eq!(visibility_of(&app, *entity), wanted(*drawn), "{what}");
            assert_eq!(leaf_is_drawn(&app, *mesh), *drawn, "{what}");
        }

        // And then with each row holding the role it is about - the door the portal renders
        // through, the far end of that doorway, or neither.
        for ((_, _, portal, _, destination, drawn, what), (entity, mesh)) in
            cases.iter().zip(&spawned)
        {
            if !*portal && !*destination {
                continue;
            }
            portal_shows(&mut app, portal.then_some(*entity));
            app.world_mut()
                .resource_mut::<PortalState>()
                .destination_door = destination.then_some(*entity);
            update(&mut app, 1);
            assert_eq!(visibility_of(&app, *entity), wanted(*drawn), "{what}");
            assert_eq!(leaf_is_drawn(&app, *mesh), *drawn, "{what}");
        }
    }

    /// The **destination** door of the pair the portal is drawing through draws nothing while the
    /// window is up, and comes back the frame the window goes.
    ///
    /// Under a doorway anchor the window's clip plane is the destination doorway's own plane
    /// (`DoorMap::destination_plane`), which is exactly where the destination door stands: drawn, its
    /// closed leaf fills the aperture and the room behind it is invisible. Today's map hid that leaf
    /// by accident, because its clip plane stands tens of units inside the room; this rule is the
    /// deliberate answer to the clip moving, and it takes only the destination door - the door the
    /// portal renders *through* keeps its own model, frame and swing.
    #[test]
    fn the_destination_door_of_the_pair_draws_nothing_while_the_window_is_up() {
        let mut app = portal_app();
        let (source, source_mesh) = spawn_door(&mut app, interior_door(0x0005_6C1B));
        let (destination, destination_mesh) = spawn_door(&mut app, interior_door(0x0001_52C3));
        app.world_mut()
            .entity_mut(source)
            .insert(DoorState::Opening);
        app.world_mut()
            .entity_mut(destination)
            .insert(DoorState::Closed);

        // No portal: both doors are their own business, and the closed one draws its leaf.
        update(&mut app, 1);
        assert!(leaf_is_drawn(&app, source_mesh));
        assert!(leaf_is_drawn(&app, destination_mesh));

        // With the window up through the source door, its own model stays - and the far end of the
        // same doorway is not drawn into it.
        portal_shows(&mut app, Some(source));
        app.world_mut()
            .resource_mut::<PortalState>()
            .destination_door = Some(destination);
        update(&mut app, 1);
        assert!(
            leaf_is_drawn(&app, source_mesh),
            "the door the portal renders through is an animated one: its frame is the doorway"
        );
        assert!(
            !leaf_is_drawn(&app, destination_mesh),
            "the destination door's leaf stands in the window's own clip plane"
        );

        // The portal drops the pair: the destination door is a door again in the same frame.
        portal_shows(&mut app, None);
        app.world_mut()
            .resource_mut::<PortalState>()
            .destination_door = None;
        update(&mut app, 1);
        assert!(leaf_is_drawn(&app, destination_mesh));
    }

    // -----------------------------------------------------------------------------------------
    // The doorway's mirror: the door drawn over the doorway image (the user's own note)
    // -----------------------------------------------------------------------------------------

    /// The scene a door's model spawns, as far as a mirror is concerned: the root the glTF loader
    /// makes for a scene, the node the door's `Open` clip moves - the leaf - with its mesh under it,
    /// and the model's static filler beside it: the game's black plug across the doorway
    /// (`DoorBlack` in `FarmhouseLDoor01`, `Plane02` in the Dwemer large load door).
    ///
    /// Every *node* carries the [`AnimationTargetId`] the loader hashes from its name path and every
    /// *mesh* carries none, which is exactly the distinction the mirror's walk draws between a node
    /// of the model and a mesh under one. The names are the loader's own for the Riverwood doors.
    struct DoorScene {
        /// The scene root: where the loader puts the animation player.
        root: Entity,
        /// The node the door's clips move.
        leaf: Entity,
        /// The leaf's own mesh.
        leaf_mesh: Entity,
        /// The static filler's mesh: the plug in the doorway.
        plug_mesh: Entity,
    }

    /// The id the loader hashes for a node, from its name path.
    fn scene_target(path: &[&str]) -> AnimationTargetId {
        AnimationTargetId::from_iter(path.iter().copied())
    }

    /// Spawns a model's scene under `parent`, with the loader's own names, ids and rest pose: the
    /// leaf hinged 88 units along the doorway's own plane (where `FarmhouseLDoor01` hinges it) with
    /// its mesh 96 units out along the leaf - the far edge of the door, which a swing moves.
    fn spawn_scene(app: &mut App, parent: Entity) -> DoorScene {
        let node = |app: &mut App, path: &[&str], parent: Entity, transform: Transform| {
            app.world_mut()
                .spawn((
                    Name::new(path[path.len() - 1].to_string()),
                    scene_target(path),
                    transform,
                    Visibility::default(),
                    ChildOf(parent),
                ))
                .id()
        };
        let root = node(
            app,
            &["Creation-to-glTF basis"],
            parent,
            Transform::default(),
        );
        let model = node(
            app,
            &["Creation-to-glTF basis", "FarmhouseLDoor01"],
            root,
            Transform::default(),
        );
        let leaf = node(
            app,
            &["Creation-to-glTF basis", "FarmhouseLDoor01", "Door"],
            model,
            Transform::from_xyz(48.0, 4.0, 88.0),
        );
        // The leaf's own mesh, out at the far edge of the door: a node's transform that a swing
        // carries a long way, which is what makes the tests' comparisons bite. `spawn_scene` gives
        // it no id, as the loader does not, so its own pose is the model's rest pose in both
        // instances and the parent's swing is what moves it.
        let leaf_mesh = app
            .world_mut()
            .spawn((
                Mesh3d(Handle::default()),
                Transform::from_xyz(-96.0, 0.0, 0.0),
                Visibility::default(),
                ChildOf(leaf),
            ))
            .id();
        let plug = node(
            app,
            &["Creation-to-glTF basis", "FarmhouseLDoor01", "DoorBlack"],
            model,
            Transform::from_xyz(0.0, 0.0, -18.0),
        );
        let plug_mesh = app
            .world_mut()
            .spawn((Mesh3d(Handle::default()), ChildOf(plug)))
            .id();
        DoorScene {
            root,
            leaf,
            leaf_mesh,
            plug_mesh,
        }
    }

    /// A load door with a model's scene on it and a state its own animation moves: the door the
    /// portal is rendering through, holding the scene handle `streaming::spawn_cell` would have
    /// loaded for it.
    fn spawn_mirrorable_door(
        app: &mut App,
        door: LoadDoor,
        state: DoorState,
    ) -> (Entity, DoorScene) {
        app.init_asset::<WorldAsset>();
        let (entity, _) = spawn_door(app, door);
        let handle = app
            .world_mut()
            .resource_mut::<Assets<WorldAsset>>()
            .add(WorldAsset::new(World::default()));
        app.world_mut()
            .entity_mut(entity)
            .insert((state, WorldAssetRoot(handle)));
        let scene = spawn_scene(app, entity);
        (entity, scene)
    }

    /// The portal's mirror systems, wired the way `PortalPlugin` wires them: the two `Update`
    /// systems behind the door transition like every other portal system, and the pose copy in
    /// `PostUpdate`, after the animation has advanced the door's nodes and before the transforms
    /// propagate.
    fn add_mirror_systems(app: &mut App) {
        app.init_asset::<WorldAsset>()
            .add_systems(
                Update,
                (place_door_mirror, place_destination_sun)
                    .chain()
                    .after(crate::transition::DoorTransition),
            )
            .add_systems(
                PostUpdate,
                mirror_door_nodes
                    .after(AnimationSystems)
                    .before(TransformSystems::Propagate),
            );
    }

    /// The doorway's mirror, if there is one, and the door it is the second instance of.
    fn mirror_of(app: &mut App) -> Option<(Entity, Entity)> {
        let mut query = app.world_mut().query::<(Entity, &PortalDoorMirror)>();
        query
            .iter(app.world())
            .next()
            .map(|(entity, mirror)| (entity, mirror.door))
    }

    /// The scene handle an entity spawns its model from.
    fn scene_handle_of(app: &App, entity: Entity) -> Handle<WorldAsset> {
        app.world()
            .entity(entity)
            .get::<WorldAssetRoot>()
            .map(|root| root.0.clone())
            .expect("a scene root")
    }

    /// Whether an entity is drawn, through the inheritance the renderer itself uses.
    fn is_drawn(app: &App, entity: Entity) -> bool {
        app.world()
            .entity(entity)
            .get::<InheritedVisibility>()
            .expect("a visible entity")
            .get()
    }

    /// A node's own transform, and where the hierarchy puts it.
    fn local_of(app: &App, entity: Entity) -> Transform {
        *app.world().entity(entity).get::<Transform>().unwrap()
    }

    fn world_of(app: &App, entity: Entity) -> Vec3 {
        app.world()
            .entity(entity)
            .get::<GlobalTransform>()
            .unwrap()
            .translation()
    }

    /// A camera's view matrix for a pose: the inverse of the camera's own world transform, which
    /// is how Bevy builds it.
    fn view_of(position: Vec3, rotation: Quat) -> Mat4 {
        Mat4::from(
            GlobalTransform::from(Transform::from_translation(position).with_rotation(rotation))
                .affine()
                .inverse(),
        )
    }

    /// The door pose and destination a mirror test stands on, and the anchor the door carries - the
    /// `None` every door without one has, and a doorway anchor whose source doorway faces 11.5
    /// degrees from the door's link-derived frame.
    struct MirrorCase {
        door: LoadDoor,
        position: Vec3,
        rotation: Quat,
        anchor: Option<DoorAnchor>,
    }

    const CAMERA_STANDOFF: f32 = 220.0;
    const TOWER_BOX: [f32; 3] = [0.0, 88.0, -13.5];
    /// The render origin `portal_app` holds, which a case's map is built with.
    const ORIGIN: IVec2 = IVec2::new(19, 18);
    /// The scale the door of a case is spawned with, which the anchor's pivot obeys.
    const DOOR_SCALE: Vec3 = Vec3::new(1.0, 1.0, 0.5);

    /// The Ruined Tower's shape: the model's own axis faces a quarter turn from the direction its
    /// link data gives, so the frame the map is built in is neither - the case a map built from the
    /// model's axes instead of the link's would get wrong.
    fn tower_door() -> LoadDoor {
        LoadDoor {
            outward: Some([1.0, 0.0, 0.0]),
            ..interior_door(INTERIOR_ALFTAND01)
        }
    }

    /// The pose of a door in a test: a placement a camera stands [`CAMERA_STANDOFF`] units in front
    /// of, with the map its destination makes built by [`case_map`] - the same call `update_portal`
    /// and `place_door_mirror` make.
    fn mirror_case(door: LoadDoor) -> MirrorCase {
        MirrorCase {
            door,
            position: Vec3::new(-420.0, 130.0, 900.0),
            rotation: Quat::from_rotation_y(core::f32::consts::FRAC_PI_2),
            anchor: None,
        }
    }

    /// The same door with a doorway anchor on it, of the shape Sven's House is: the source
    /// doorway's own facing is 11.5 degrees from the door's link-derived frame (which is exactly the
    /// error the anchor exists to remove), and the destination is a doorway of its own - an
    /// interior, so at its absolute creation coordinates - 88 units up and 13.5 off its reference.
    fn anchored_mirror_case(door: LoadDoor) -> MirrorCase {
        let mut case = mirror_case(door);
        case.anchor = Some(DoorAnchor {
            tier: crate::doors::DoorAnchorTier::SameModel,
            source_box_centre: TOWER_BOX,
            destination: crate::doors::DoorwayGeometry {
                position: [2879.831, 2_718.83, -1828.0],
                rotation: [0.0, 0.0, 1.0],
                scale: 1.0,
                box_centre: TOWER_BOX,
            },
            destination_grid: None,
            facings: crate::doors::DoorwayFacings::Known {
                source: core::f32::consts::FRAC_PI_2 - 0.2,
                destination: 1.0 + 0.2,
            },
        });
        case
    }

    /// The door -> destination map the portal builds for a case: `update_portal`'s own call, from
    /// the door's live placement, its anchor and the render origin.
    fn case_map(case: &MirrorCase) -> DoorMap {
        door_map(
            case.position,
            case.rotation,
            DOOR_SCALE,
            &case.door,
            case.anchor.as_ref(),
            ORIGIN,
        )
    }

    /// A [`MirrorCase`]'s door spawned in an app with its scene and the mirror the portal makes of
    /// it, swung to `swing` - the frame the mirror's copy is read in.
    ///
    /// The mirror's own instance of the scene is spawned by hand: nothing in a test runs the world
    /// instance spawner, which is what `WorldAssetRoot` asks for in a run.
    fn mirrored_door(
        app: &mut App,
        case: &MirrorCase,
        swing: Transform,
    ) -> (Entity, DoorScene, Entity, DoorScene) {
        let (door, scene) = spawn_mirrorable_door(app, case.door.clone(), DoorState::Opening);
        let transform = Transform {
            translation: case.position,
            rotation: case.rotation,
            scale: DOOR_SCALE,
        };
        app.world_mut()
            .entity_mut(door)
            .insert((transform, GlobalTransform::from(transform)));
        if let Some(anchor) = case.anchor.clone() {
            app.world_mut().entity_mut(door).insert(anchor);
        }
        portal_shows(app, Some(door));
        update(app, 1);
        let (mirror, mirrored) = mirror_of(app).expect("the portal is rendering through a door");
        assert_eq!(mirrored, door);
        let mirror_scene = spawn_scene(app, mirror);
        // The door's own marking of what its animation moves: what `door_animation::mark_leaf_nodes`
        // puts on the nodes a door's clips have curves for, and the answer the mirror reads for
        // "which part of this model is the leaf".
        app.world_mut()
            .entity_mut(scene.leaf)
            .insert(DoorLeaf { door });
        // The swing, written where the door's own animation writes it: the leaf's own transform.
        app.world_mut().entity_mut(scene.leaf).insert(swing);
        update(app, 1);
        (door, scene, mirror, mirror_scene)
    }

    /// The mirror is the door's own model under the door -> destination map the portal camera is
    /// placed by: every node of the second instance stands where the map puts the door's own node,
    /// and its leaf carries the door's own swing.
    ///
    /// Both shapes of destination are covered - an interior, whose frame is at its absolute
    /// creation coordinates, and an exterior, whose is placed relative to the render origin - and
    /// the door's front comes from its link data rather than from its model. The last case is the
    /// same door **with a doorway anchor**, where the map is built from the two doorways' own
    /// geometry and the source doorway faces 11.5 degrees from the door's link-derived frame.
    #[test]
    fn the_mirror_is_the_door_under_the_door_to_destination_map() {
        for case in [
            mirror_case(tower_door()),
            mirror_case(exterior_door()),
            anchored_mirror_case(tower_door()),
        ] {
            let mut app = portal_app();
            add_mirror_systems(&mut app);
            let swing = Transform {
                translation: Vec3::new(48.0, 4.0, 88.0),
                rotation: Quat::from_rotation_z(1.1),
                scale: Vec3::ONE,
            };
            let (_, scene, _, mirror_scene) = mirrored_door(&mut app, &case, swing);

            // The swing reached the mirror's leaf: the copy is what put it there, not the rest pose
            // the instance was spawned with.
            assert_eq!(local_of(&app, scene.leaf), swing);
            assert_eq!(
                local_of(&app, mirror_scene.leaf),
                swing,
                "the mirror's leaf carries the door's own local pose"
            );

            // Every entity the two instances share stands where the map puts the door's own - the
            // leaf's mesh included, which is the one a swing carries furthest.
            for (source, mirrored) in [
                (scene.root, mirror_scene.root),
                (scene.leaf, mirror_scene.leaf),
                (scene.leaf_mesh, mirror_scene.leaf_mesh),
            ] {
                let source = app.world().entity(source).get::<GlobalTransform>().unwrap();
                let mirrored = app
                    .world()
                    .entity(mirrored)
                    .get::<GlobalTransform>()
                    .unwrap();
                let (want_position, want_rotation) =
                    case_map(&case).pose(source.translation(), source.rotation());
                assert!(
                    (mirrored.translation() - want_position).length() < 1.0e-3,
                    "a mirrored node stands at {:?}, the map puts its source at {want_position:?}",
                    mirrored.translation()
                );
                assert!(
                    (mirrored.rotation() * Vec3::NEG_Z)
                        .abs_diff_eq(want_rotation * Vec3::NEG_Z, 1.0e-5),
                    "and looks the way the map turns its source"
                );
            }
        }
    }

    /// The decisive one: a node of the mirror lands on the clip-space position the door's own node
    /// lands on, under the main camera's projection and under the portal camera's.
    ///
    /// That identity is what the quad's screen-space UV sampling comes to - the quad samples the
    /// destination image at each fragment's own screen position - and so what makes the mirror the
    /// door standing in the doorway rather than a copy beside it. It is checked with the real
    /// [`door_map`] each case's portal uses and the real [`portal_projection`], not with a second
    /// copy of either: the portal camera is placed at `M(main camera)`, its `fov` and aspect are the
    /// main camera's, and the oblique clip plane - the destination doorway's own plane - writes only
    /// the clip `z`.
    ///
    /// The anchored case is the one that matters for "the interior lines up with the doorway": the
    /// mirror is drawn where the door's own leaf is seen, under the doorway map, from a camera that
    /// is 11.5 degrees off the door's link-derived axis because the doorway's own facing says so.
    #[test]
    fn the_mirrors_leaf_lands_on_the_pixel_the_doors_leaf_lands_on() {
        for case in [
            mirror_case(tower_door()),
            anchored_mirror_case(tower_door()),
        ] {
            let mut app = portal_app();
            add_mirror_systems(&mut app);
            let swing = Transform {
                translation: Vec3::new(48.0, 4.0, 88.0),
                rotation: Quat::from_rotation_z(1.1),
                scale: Vec3::ONE,
            };
            let (_, scene, _, mirror_scene) = mirrored_door(&mut app, &case, swing);

            let main = Projection::Perspective(PerspectiveProjection {
                fov: 60.0_f32.to_radians(),
                aspect_ratio: 16.0 / 9.0,
                near: 0.1,
                far: 100_000.0,
                near_clip_plane: Vec4::new(0.0, 0.0, -1.0, -0.1),
            });
            let map = case_map(&case);
            let camera_position = case.position + map.frame * Vec3::NEG_Z * CAMERA_STANDOFF;
            let camera_rotation = map.frame * Quat::from_rotation_y(PI);
            let (portal_position, portal_rotation) = map.pose(camera_position, camera_rotation);
            let (plane_point, plane_normal) = map.destination_plane();
            let clip_plane =
                doorway_clip_plane(portal_position, portal_rotation, plane_point, plane_normal);
            let Projection::Perspective(main_perspective) = &main else {
                unreachable!()
            };
            let Projection::Perspective(portal_perspective) =
                portal_projection(&main, clip_plane, -clip_plane.w)
            else {
                unreachable!()
            };
            let from_main =
                main_perspective.get_clip_from_view() * view_of(camera_position, camera_rotation);
            let from_portal =
                portal_perspective.get_clip_from_view() * view_of(portal_position, portal_rotation);
            let ndc = |matrix: Mat4, point: Vec3| {
                let clip = matrix * point.extend(1.0);
                assert!(clip.w > 0.0, "{point:?} is behind the camera");
                clip.truncate() / clip.w
            };

            // The leaf's mesh, at the far edge of the door: a point the swing carries 96 units, so a
            // mirror that had not been given this frame's pose lands somewhere else on the screen.
            let door_pixel = ndc(from_main, world_of(&app, scene.leaf_mesh));
            let mirror_pixel = ndc(from_portal, world_of(&app, mirror_scene.leaf_mesh));
            assert!(
                (door_pixel.x - mirror_pixel.x).abs() < 1.0e-4
                    && (door_pixel.y - mirror_pixel.y).abs() < 1.0e-4,
                "the door's leaf lands on {door_pixel:?} and the mirror's on {mirror_pixel:?}"
            );
            assert!(
                door_pixel.x.abs() < 1.0
                    && door_pixel.y.abs() < 1.0
                    && (0.0..=1.0).contains(&door_pixel.z),
                "the leaf under test is in the view: {door_pixel:?}"
            );
            assert_eq!(
                portal_perspective.fov, main_perspective.fov,
                "the doorway image is the main camera's own view, so the two agree on fov"
            );
        }
    }

    /// The mirror draws the part of the model the door's own animation moves and nothing else, and
    /// every entity it is made of is on the portal camera's layer.
    ///
    /// The layer is what keeps the second instance out of the main camera (the door's own leaf is
    /// drawn there, and a copy of it would double whatever is in front of the quad), and the
    /// [`DoorLeaf`] marking is what keeps the walk probe off it: the mirror stands exactly where the
    /// player lands after a crossing. The hiding is what keeps the game's black plug - static, and
    /// closed against the player - out of the doorway image, where it would cover it.
    ///
    /// The two go together: the plug is hidden *and* unmarked, because a [`DoorLeaf`] would have
    /// `update_door_leaves` unhide it a frame after this walk hid it.
    #[test]
    fn the_mirror_draws_the_leaf_and_hides_the_models_filler() {
        let mut app = portal_app();
        add_mirror_systems(&mut app);
        let case = mirror_case(tower_door());
        let swing = Transform::from_rotation(Quat::from_rotation_z(0.2));
        let (door, scene, mirror, mirror_scene) = mirrored_door(&mut app, &case, swing);

        // The leaf: on the portal camera's layer, drawn, and marked so the walk probe skips it.
        for entity in [mirror_scene.root, mirror_scene.leaf, mirror_scene.leaf_mesh] {
            assert_eq!(
                layers_of(&app, entity),
                RenderLayers::layer(DESTINATION_LAYER),
                "every entity of the mirror is on the portal camera's layer"
            );
        }
        for entity in [mirror_scene.root, mirror_scene.leaf, mirror_scene.leaf_mesh] {
            assert!(
                app.world().entity(entity).get::<DoorLeaf>().is_some(),
                "every drawn entity of the mirror is a leaf of the door it is a second instance of"
            );
            assert_eq!(
                app.world().entity(entity).get::<DoorLeaf>().unwrap().door,
                door
            );
        }
        assert!(
            is_drawn(&app, mirror_scene.leaf_mesh),
            "the leaf's mesh is drawn in the doorway image"
        );

        // The filler: on the layer like everything else, and hidden - from the portal camera and
        // from the walk probe, which skips render layers but not visibility.
        assert_eq!(
            layers_of(&app, mirror_scene.plug_mesh),
            RenderLayers::layer(DESTINATION_LAYER)
        );
        assert!(
            !is_drawn(&app, mirror_scene.plug_mesh),
            "the model's black plug is not drawn in the doorway image"
        );
        assert!(
            app.world()
                .entity(mirror_scene.plug_mesh)
                .get::<DoorLeaf>()
                .is_none(),
            "and it is not marked as the door's leaf"
        );
        assert!(
            is_drawn(&app, scene.plug_mesh),
            "the door's own plug is untouched: what the portal draws over its doorway is the \
             quad's business, not the mirror's"
        );

        // A mirror is not a reference: `streaming.rs` measures a reference's model through the
        // components it spawns a scene with, and `model_animation.rs` looks for `MeshHandle`.
        let world = app.world();
        let mirror_root = world.entity(mirror);
        assert!(
            mirror_root
                .get::<crate::world::components::MeshHandle>()
                .is_none()
        );
        assert!(
            mirror_root
                .get::<crate::world::components::WorldTransform>()
                .is_none()
        );
        assert!(mirror_root.get::<LoadDoor>().is_none());
        assert_eq!(
            scene_handle_of(&app, mirror),
            scene_handle_of(&app, door),
            "the mirror instantiates the scene handle the door already has; it loads nothing"
        );
    }

    /// The pose copy runs after the animation has advanced the door's nodes and before the
    /// transforms propagate, so a frame in which the door's leaf moves is a frame in which the
    /// mirror's has moved with it.
    ///
    /// The animation is a stand-in: a system in Bevy's own [`AnimationSystems`] set, writing the
    /// door's leaf the way `animate_targets` does, with a different pose every frame. A copy that
    /// ran before that system would leave the mirror a frame of swing behind the door, which is
    /// what this fails on.
    #[test]
    fn the_mirror_swings_with_the_door_in_the_same_frame() {
        #[derive(Component)]
        struct Swung;

        fn swing_step(mut step: Local<u32>, mut swung: Query<&mut Transform, With<Swung>>) {
            *step += 1;
            let angle = 0.05 * *step as f32;
            for mut transform in &mut swung {
                transform.rotation = Quat::from_rotation_z(angle);
            }
        }

        let mut app = portal_app();
        add_mirror_systems(&mut app);
        let case = mirror_case(tower_door());
        let (_, scene, _, mirror_scene) = mirrored_door(&mut app, &case, Transform::default());
        app.world_mut().entity_mut(scene.leaf).insert(Swung);
        app.add_systems(PostUpdate, swing_step.in_set(AnimationSystems));

        for frame in 1..=3 {
            update(&mut app, 1);
            assert_ne!(
                local_of(&app, scene.leaf).rotation,
                Quat::IDENTITY,
                "the stand-in animation moved the door's leaf in frame {frame}"
            );
            assert_eq!(
                local_of(&app, mirror_scene.leaf),
                local_of(&app, scene.leaf),
                "the mirror's leaf carries this frame's swing, not the last one's"
            );
        }
    }

    /// A mirror is made for the door the portal is rendering through and for no other: not for a
    /// door whose model has no swing of its own, not for an auto-load marker, not while the portal
    /// is showing nothing - and it follows the portal to another door in the frame it retargets.
    #[test]
    fn the_mirror_is_the_door_the_portal_shows_and_no_other() {
        let mut app = portal_app();
        add_mirror_systems(&mut app);
        let case = mirror_case(tower_door());
        let (still, _) = spawn_mirrorable_door(
            &mut app,
            interior_door(0x0005_6C1B),
            DoorState::Open { animated: false },
        );
        let marker = LoadDoor {
            auto_load: true,
            ..interior_door(0x0005_6C1C)
        };
        let (marker, _) = spawn_mirrorable_door(&mut app, marker, DoorState::Opening);
        let (closed, _) =
            spawn_mirrorable_door(&mut app, interior_door(0x0005_6C1D), DoorState::Closed);
        let (first, _) = spawn_mirrorable_door(&mut app, case.door.clone(), DoorState::Opening);
        let (second, _) = spawn_mirrorable_door(&mut app, case.door.clone(), DoorState::Opening);

        // No portal, then a door with no leaf of its own, then a marker, then a closed door: none
        // of them is a doorway with a swing to draw.
        for door in [None, Some(still), Some(marker), Some(closed)] {
            portal_shows(&mut app, door);
            update(&mut app, 1);
            assert!(
                mirror_of(&mut app).is_none(),
                "{door:?} is not a door whose own animation moves it"
            );
        }

        // The door the portal is rendering through, mid-swing: the mirror is that door's.
        portal_shows(&mut app, Some(first));
        update(&mut app, 1);
        let (_, door) = mirror_of(&mut app).expect("the portal is rendering through a door");
        assert_eq!(door, first);

        // Retargeting: the frame the portal leaves one door it is rendering the other, and the
        // mirror moved with it rather than staying where the old door is.
        portal_shows(&mut app, Some(second));
        update(&mut app, 1);
        let (mirror, door) = mirror_of(&mut app).expect("the portal is still showing a door");
        assert_eq!(door, second);
        // The door's own pose carried through the map: `M(door position)` is the map's arrival, so
        // the mirror's root stands exactly on the destination doorway.
        assert_eq!(
            local_of(&app, mirror).translation,
            case_map(&case).arrival_position,
            "the mirror stands on the destination doorway of the door the portal is showing"
        );

        // The portal gone, and the door entity despawned under a mirror: both drop it.
        portal_shows(&mut app, None);
        update(&mut app, 1);
        assert!(mirror_of(&mut app).is_none());

        portal_shows(&mut app, Some(second));
        update(&mut app, 1);
        assert!(mirror_of(&mut app).is_some());
        app.world_mut().entity_mut(second).despawn();
        update(&mut app, 1);
        assert!(
            mirror_of(&mut app).is_none(),
            "the door the mirror is a second instance of is gone"
        );
    }

    // -----------------------------------------------------------------------------------------
    // The doorway's own sun: the destination's space lighting the destination's image
    // -----------------------------------------------------------------------------------------

    /// An app with the two cameras and the two suns: the portal camera and its own sun from
    /// [`setup_portal_camera`], the main camera on the layers `app::setup_world` gives it, and the
    /// engine's one sun - which, spawned as `app.rs` spawns it, carries no `RenderLayers` at all.
    struct SunApp {
        app: App,
        main_camera: Entity,
        engine_sun: Entity,
        portal_camera: Entity,
        destination_sun: Entity,
    }

    /// The engine sun's own numbers, distinct from any space's, so a test can see who wrote what.
    const ENGINE_SUN_COLOR: Color = Color::srgb(1.0, 0.25, 0.0);
    const ENGINE_SUN_ILLUMINANCE: f32 = 1234.0;

    fn sun_app() -> SunApp {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>()
            .init_resource::<PortalState>()
            .insert_resource(EngineConfig {
                worldspace_id: TAMRIEL,
                stream_radius: 1,
                unload_radius: 1,
                start_grid: (0, 0),
                ..EngineConfig::default()
            })
            .insert_resource(RenderOrigin(IVec2::new(19, 18)))
            .add_systems(Startup, setup_portal_camera)
            .add_systems(
                Update,
                (place_destination_sun, update_destination_atmosphere).chain(),
            );
        update(&mut app, 1);

        let main_camera = app
            .world_mut()
            .spawn((
                StreamingCamera,
                Camera::default(),
                RenderLayers::from_layers(&MAIN_CAMERA_LAYERS),
                Transform::default(),
            ))
            .id();
        let engine_sun = app
            .world_mut()
            .spawn((
                Name::new("Engine sun"),
                DirectionalLight {
                    color: ENGINE_SUN_COLOR,
                    illuminance: ENGINE_SUN_ILLUMINANCE,
                    shadow_maps_enabled: true,
                    ..default()
                },
                CascadeShadowConfig::default(),
                Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)),
            ))
            .id();
        let portal_camera = entity_with::<PortalCamera>(&mut app).expect("the portal camera");
        let destination_sun =
            entity_with::<PortalDestinationSun>(&mut app).expect("the doorway's sun");
        SunApp {
            app,
            main_camera,
            engine_sun,
            portal_camera,
            destination_sun,
        }
    }

    /// The first entity carrying a component.
    fn entity_with<T: Component>(app: &mut App) -> Option<Entity> {
        let mut query = app.world_mut().query_filtered::<Entity, With<T>>();
        query.iter(app.world()).next()
    }

    /// A door whose destination is a space of its own, and the space's key.
    fn door_to_space(worldspace_id: u32, interior: Option<u32>) -> LoadDoor {
        LoadDoor {
            destination: DoorDestination {
                destination_ref_id: 0x699E8,
                interior_cell_id: interior,
                worldspace_id: interior.is_none().then_some(worldspace_id),
                arrival_position: [3693.815, 3074.645, 290.530],
                arrival_rotation: [0.0, 0.0, -1.83260],
            },
            label: "a space".into(),
            auto_load: false,
            outward: None,
            ..interior_door(INTERIOR_ALFTAND01)
        }
    }

    /// The portal camera is lit by the sun of its own layer and by no other light, and the main
    /// camera by the engine's - which is the whole mechanism: Bevy builds a view's directional
    /// lights by intersecting the light's layers with the *camera's*, and before the doorway had a
    /// sun of its own the engine's one (no layers at all, so layer 0) reached neither the portal
    /// camera nor, therefore, the doorway image.
    #[test]
    fn the_doorway_is_lit_by_a_sun_of_its_own_layer() {
        let SunApp {
            app,
            main_camera,
            engine_sun,
            portal_camera,
            destination_sun,
        } = sun_app();

        assert!(
            layers_of(&app, portal_camera).intersects(&layers_of(&app, destination_sun)),
            "the portal camera renders the layer the doorway's sun is on"
        );
        assert!(
            !layers_of(&app, main_camera).intersects(&layers_of(&app, destination_sun)),
            "and the main camera does not: the doorway's sun must not light the room around it"
        );
        assert!(
            !layers_of(&app, engine_sun).intersects(&layers_of(&app, portal_camera)),
            "the engine's sun carries no layers - layer 0 - so the portal camera never saw it, \
             which is the state this test is here to keep from coming back"
        );
        assert!(
            layers_of(&app, engine_sun).intersects(&layers_of(&app, main_camera)),
            "and the main camera keeps it"
        );
    }

    /// The doorway's sun takes the engine sun's direction and shadow settings and nothing else
    /// does: one sun in the world, drawn in two views.
    #[test]
    fn the_doorway_sun_points_where_the_engines_does() {
        let SunApp {
            mut app,
            engine_sun,
            destination_sun,
            ..
        } = sun_app();
        update(&mut app, 1);

        let engine = *app.world().entity(engine_sun).get::<Transform>().unwrap();
        let destination = *app
            .world()
            .entity(destination_sun)
            .get::<Transform>()
            .unwrap();
        assert_eq!(destination.rotation, engine.rotation);
        assert_eq!(
            app.world()
                .entity(destination_sun)
                .get::<DirectionalLight>()
                .unwrap()
                .shadow_maps_enabled,
            app.world()
                .entity(engine_sun)
                .get::<DirectionalLight>()
                .unwrap()
                .shadow_maps_enabled
        );
        let cascades = |app: &App, entity: Entity| {
            let cascades = app
                .world()
                .entity(entity)
                .get::<CascadeShadowConfig>()
                .unwrap();
            (
                cascades.bounds.clone(),
                cascades.overlap_proportion,
                cascades.minimum_distance,
            )
        };
        assert_eq!(cascades(&app, destination_sun), cascades(&app, engine_sun));

        // Moving the engine sun moves the doorway's with it, and the engine's own is left as it
        // was found.
        let turned = Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.4, -0.9, 0.0));
        app.world_mut().entity_mut(engine_sun).insert(turned);
        update(&mut app, 1);
        assert_eq!(
            app.world()
                .entity(destination_sun)
                .get::<Transform>()
                .unwrap()
                .rotation,
            turned.rotation
        );
        assert_eq!(
            app.world()
                .entity(engine_sun)
                .get::<Transform>()
                .unwrap()
                .rotation,
            turned.rotation
        );
    }

    /// The doorway image is lit by the *destination's* sun: the record's tint and illuminance in
    /// the doorway's own light, with the engine's sun left exactly as the space the player stands
    /// in left it.
    #[test]
    fn the_doorway_sun_carries_the_destinations_own_sun() {
        use crate::world::lighting::fixtures::{BLACKREACH, real_spaces};
        let (_directory, catalog) = real_spaces();
        let tamriel = crate::app::space_atmosphere(Some(&catalog), space_key(TAMRIEL, None));
        let blackreach = crate::app::space_atmosphere(Some(&catalog), space_key(BLACKREACH, None));
        assert!(
            tamriel.sun.illuminance > 0.0 && blackreach.sun.illuminance == 0.0,
            "the fixture's day and cave are the two cases this test is about"
        );

        let SunApp {
            mut app,
            engine_sun,
            destination_sun,
            ..
        } = sun_app();
        app.insert_resource(catalog);
        let day = door_to_space(TAMRIEL, None);
        let cave = door_to_space(BLACKREACH, None);
        let (day_door, _) = spawn_mirrorable_door(&mut app, day, DoorState::Opening);
        let (cave_door, _) = spawn_mirrorable_door(&mut app, cave, DoorState::Opening);

        portal_shows(&mut app, Some(day_door));
        update(&mut app, 2);
        let light = |app: &App, entity: Entity| {
            let light = app
                .world()
                .entity(entity)
                .get::<DirectionalLight>()
                .unwrap();
            (light.color, light.illuminance)
        };
        assert_eq!(
            light(&app, destination_sun),
            (tamriel.sun.color, tamriel.sun.illuminance),
            "the doorway is lit by the daylight the destination's own weather publishes"
        );
        assert_eq!(
            light(&app, engine_sun),
            (ENGINE_SUN_COLOR, ENGINE_SUN_ILLUMINANCE),
            "and the engine's sun is not the doorway's to write"
        );

        // A cave for a destination: the doorway's sun goes out, the same way the space's own sun
        // does when the player walks in.
        portal_shows(&mut app, Some(cave_door));
        update(&mut app, 2);
        assert_eq!(
            light(&app, destination_sun),
            (blackreach.sun.color, blackreach.sun.illuminance)
        );
        assert_eq!(
            light(&app, engine_sun),
            (ENGINE_SUN_COLOR, ENGINE_SUN_ILLUMINANCE)
        );

        // `app::update_atmosphere` writes every `DirectionalLight` in the world when the space the
        // player stands in changes - this one included, since it cannot know whose it is. That
        // write is repaired rather than left standing: the doorway keeps the destination's sun.
        app.world_mut()
            .entity_mut(destination_sun)
            .insert(DirectionalLight {
                color: ENGINE_SUN_COLOR,
                illuminance: ENGINE_SUN_ILLUMINANCE,
                ..default()
            });
        update(&mut app, 1);
        assert_eq!(
            light(&app, destination_sun),
            (blackreach.sun.color, blackreach.sun.illuminance),
            "the doorway's sun is written whenever it is not the destination's, not only on a \
             change of destination"
        );
    }
}
