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
//! from the door's state ([`crate::doors::DoorState`]): **the portal renders through a door that is
//! opening, open or closing** ([`portal_shows_through`], which `update_portal` picks its door by -
//! a closing door is one whose doorway is still the window, because its leaf is drawn swinging
//! across it), and
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
//! * it covers the doorway's own rectangle of the screen and nothing more ([`doorway_render_rect`]):
//!   the portal camera projects only that rectangle of the main view (a `SubCameraView`), at one
//!   texel per window pixel, into a target the size of the rectangle ([`resize_portal_target`]),
//!   so the doorway has the same pixel density as the room around it and the rest of the frame
//!   is neither shaded nor, past the frustum it narrows ([`fit_portal_view`]), drawn;
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
    doors::{
        DOORWAY_FLOOR_PROBE_DEPTH, DoorAnchor, DoorDestination, DoorLeaf, DoorState,
        DoorwayFloorsMeasured, LoadDoor,
    },
    render::TerrainMaterial,
    streaming::{ActiveCell, RenderOrigin, StreamingWorld},
    transition::{
        CrossingHeld, DOOR_PRESTREAM_RADIUS, DoorMap, destination_is_resident, destination_keys,
        distance_in_front_of_door, door_map,
    },
    world::{
        components::{
            CELL_SIZE, CellRef, ExpectedModelBounds, ExteriorCellGrid, InstanceBounds,
            StreamedCellRoot, StreamingCamera, WaterSurface,
        },
        database::CellKey,
        lighting::{SpaceKey, SpaceLightingCatalog, space_key},
    },
};
use bevy::{
    animation::AnimationTargetId,
    app::AnimationSystems,
    asset::embedded_asset,
    camera::primitives::Aabb,
    camera::{
        CameraUpdateSystems, ClearColorConfig, Hdr, RenderTarget, SubCameraView,
        primitives::Frustum,
        visibility::{RenderLayers, VisibilitySystems},
    },
    core_pipeline::{
        prepass::DepthPrepass,
        tonemapping::{DebandDither, Tonemapping},
    },
    light::{CascadeShadowConfig, CascadeShadowConfigBuilder, Cascades, SimulationLightSystems},
    math::{Affine3A, Vec3A, bounding::Aabb3d, primitives::ViewFrustum},
    mesh::{Indices, PrimitiveTopology},
    pbr::{ExtendedMaterial, MaterialExtension, MaterialPlugin},
    picking::mesh_picking::ray_cast::{
        Backfaces, MeshRayCast, MeshRayCastSettings, RayCastVisibility, ray_aabb_intersection_3d,
        ray_mesh_intersection,
    },
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        occlusion_culling::OcclusionCulling,
        render_resource::{AsBindGroup, PipelineCache, TextureFormat},
    },
    shader::ShaderRef,
    world_serialization::WorldAssetRoot,
};
use std::{
    collections::{HashMap, HashSet},
    f32::consts::PI,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
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

/// The step the doorway's screen rectangle is rounded out to, in window pixels, before the portal
/// target is sized to it ([`doorway_render_rect`]).
///
/// A target of another size is another texture (`Image` has no resize), and the doorway's
/// rectangle moves by a pixel or two with every step the player takes: sized to the exact
/// rectangle, the target would be reallocated nearly every frame of a walk up to a door. Rounded
/// out to 64 it changes only when an edge crosses a 64-pixel line, and the rectangle it renders is
/// never smaller than the doorway it is sampled by.
const PORTAL_RECT_QUANTUM: u32 = 64;

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

/// How many shadow cascades the doorway's own sun ([`PortalDestinationSun`]) is spawned with,
/// before [`place_destination_sun`] has seen the engine sun's: Bevy's own default, four, which is
/// also `app::SUN_SHADOW_CASCADES`.
///
/// **The count is always the engine sun's.** Bevy 0.19 cannot draw two directional lights whose
/// cascade counts differ: `check_dir_light_mesh_visibility` sizes one per-thread scratch list per
/// cascade of the light it is on, shares the lists between lights, and then reads every thread's
/// list at every cascade index of the current light - so a light with more cascades than the one
/// before it reads past the end of a list the other sized (`bevy_light-0.19.0/src/lib.rs:406`,
/// `:477`). Two cascades here and four on the engine sun panicked the first frame a doorway
/// opened ("index out of bounds: the len is 2 but the index is 2"). What makes the doorway's
/// shadows cheaper is therefore the *reach* of its cascades, not their number
/// ([`PORTAL_SUN_SHADOW_DISTANCE`]).
const PORTAL_SUN_SHADOW_CASCADES: usize = 4;

/// How far from the portal camera the doorway's sun casts shadows, in Creation units (about 43 m).
///
/// This is what makes the doorway's shadows cheap. Each cascade is a shadow pass over every caster
/// in its box, and the doorway's sun used to copy the engine sun's set whole - cascades reaching
/// 17,378 units - which is a second full shadow workload every frame a doorway is open, for a
/// window that covers a piece of the screen (research-175, section 3). What a doorway shows is
/// mostly near: the room behind a house door, the street in front of it. The same number of
/// cascades ([`PORTAL_SUN_SHADOW_CASCADES`] says why it cannot be fewer) over a sixth of the reach
/// draws a fraction of the casters, at finer texels.
///
/// The portal camera stands as far behind the destination doorway as the player stands in front of
/// the source one, so the distance is measured from where the player's eye maps to, as the engine
/// sun's is from the eye. A doorway the portal draws is within [`DOOR_PRESTREAM_RADIUS`], and what
/// is seen through it is mostly within a room's or a street's length of the doorway; past this the
/// destination is lit without shadows, which through a doorway is a few pixels of distant ground.
const PORTAL_SUN_SHADOW_DISTANCE: f32 = 3000.0;

/// How long the destination cells of a doorway keep their destination role after the portal stops
/// drawing through its door for a reason that is not the door's, in seconds (impl-225).
///
/// The portal stops drawing through a door whenever [`select_portal_door`] no longer picks it: the
/// player stepped behind the door's plane or out of its reach, or looked so that another rule
/// dropped it. Each time, the destination cells went `Destination -> Hidden` and back the moment
/// the doorway was picked again, and a role change walks the root's whole subtree
/// ([`isolate_cells`]; 879 nodes each way at the Riverwood Trader, impl-223). Held, a turn away and
/// back re-layers nothing. The hold ends at once when the door stops showing a way through (it
/// closes), when its far side is no longer resident, when it is no longer a door of the active
/// space (a crossing), and when another door's destination is drawn instead.
pub(crate) const DESTINATION_HOLD_SECONDS: f64 = 2.0;

/// The far bound of the doorway sun's first cascade, in Creation units (about 8.6 m): the doorway
/// and the first steps past it, where a shadow is largest on screen and needs the finest texels.
const PORTAL_SUN_FIRST_CASCADE: f32 = 600.0;

/// The near bound of the doorway sun's first cascade, in Creation units: the engine sun's own
/// 10 cm (`app::sun_shadow_cascades`). Nothing nearer the portal camera than the doorway is drawn
/// anyway - its projection is clipped there ([`portal_projection`]).
const PORTAL_SUN_SHADOW_NEAR: f32 = 7.0;

/// The overlap between the doorway sun's cascades: the engine sun's own proportion
/// (`app::sun_shadow_cascades`), so a shadow fades from one cascade into the next the same way on
/// both sides of the doorway.
const PORTAL_SUN_CASCADE_OVERLAP: f32 = 0.2;

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

/// Where a run wants the camera to start: `--start-position` / `--start-yaw`, or the position a
/// `--demo <name>` starts at, as a transform in the run's own render space.
///
/// [`app::setup_world`](crate::app) reads this instead of the configuration, so the world's startup
/// carries no portal branch: the seam is a general one - "start the camera here, for whatever
/// reason a run has" - and a run with no start of its own simply leaves it unset and gets the
/// default placement.
#[derive(Resource, Debug, Clone, Copy)]
pub struct StartPose(pub Transform);

/// Eye height above a start position, matching the player controller's standing eye height.
const START_EYE_HEIGHT: f32 = 120.0;

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
        // An automated run's window starts parked, with its taskbar entry, and comes on screen
        // while it has focus (`crate::window_parking`).
        if app
            .world()
            .get_resource::<EngineConfig>()
            .is_some_and(EngineConfig::window_offscreen)
        {
            app.add_plugins(crate::window_parking::WindowParkingPlugin);
        }
        // The sort of run this is, read from the configuration: a pure function of it, so this
        // plugin and `app::run` cannot disagree (H7). The shots run is a value rather than a flag -
        // `app::run` built it before the window existed, because it sizes that window (H3) - and it
        // is handed over as a resource. The start pose is built here for the same reason: it is a
        // portal idea (H5) that `app::setup_world` reads as a plain resource.
        let (interactive, walking, demo_tour, shots, start_pose) = {
            let config = app.world().resource::<EngineConfig>();
            let start_pose = config.start_position.map(|position| {
                let origin = app.world().resource::<RenderOrigin>().0;
                StartPose(
                    Transform::from_translation(
                        crate::streaming::render_position(Vec3::from_array(position), origin)
                            + Vec3::Y * START_EYE_HEIGHT,
                    )
                    .with_rotation(crate::transition::arrival_camera_rotation(
                        [0.0, 0.0, config.start_yaw],
                    )),
                )
            });
            (
                config.interactive(),
                config.walks(),
                config.portal.demo_tour.clone(),
                app.world()
                    .get_resource::<crate::shots::ShotsRun>()
                    .cloned(),
                start_pose,
            )
        };
        if let Some(pose) = start_pose {
            app.insert_resource(pose);
        }
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
                    // The portal picks its door from the roles of the previous frame and publishes
                    // the destination cells; the isolation below reveals them in this same frame,
                    // which is what the roles would otherwise need the next frame for. It also
                    // measures the rectangle of the screen the doorway covers.
                    update_portal,
                    // After it, in the same frame: a doorway on screen but behind a wall is not
                    // rendered either (impl-201).
                    skip_occluded_doorway,
                    // After it, in the same frame: the target is sized to the rectangle just
                    // measured, and a resize repoints the camera's target and the quad's material
                    // at one new image, so the frame the camera projects that rectangle is the
                    // frame it renders into a target of its size and the quad maps through it.
                    resize_portal_target,
                    // After it, so the doorway's mirror is the door the portal picked this frame,
                    // in the frame the quad stands in its doorway.
                    place_door_mirror,
                    // The doorway is drawn with the atmosphere of the space it is looking into
                    // in the same frame the door is picked.
                    update_destination_atmosphere,
                    // The doorway's own sun onto the engine sun's direction. Here rather than in
                    // `PostUpdate`: it is a light, and the light extraction of this frame is the
                    // frame it should be right for. After the atmosphere, whose illuminance
                    // decides whether it casts shadows this frame.
                    place_destination_sun,
                    // After it, so the leaf of the door the portal just picked is gone in the same
                    // frame as the quad that replaces it - and one frame after it is dropped, the
                    // leaf is back. Before the isolation, which is about cells rather than doors.
                    show_load_door_leaves,
                    isolate_cells,
                    // After it: the destination is revealed, so its floor can be found.
                    anchor_on_measured_floors,
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
            )
            // After Bevy has written the portal camera's aspect ratio and frustum from the whole
            // projection, and before the visibility checks and the cascades read them.
            .add_systems(
                PostUpdate,
                fit_portal_view
                    .after(CameraUpdateSystems)
                    .after(VisibilitySystems::UpdateFrusta)
                    .before(VisibilitySystems::CheckVisibility)
                    .before(SimulationLightSystems::UpdateDirectionalLightCascades),
            )
            // After Bevy has built every shadowed sun's cascades for every active camera, and
            // before it culls shadow casters against them: each sun keeps only the cascades of
            // the views that draw it (impl-226).
            .add_systems(
                PostUpdate,
                keep_cascades_of_views_that_draw_their_light
                    .after(SimulationLightSystems::UpdateDirectionalLightCascades)
                    .before(SimulationLightSystems::UpdateLightFrusta),
            );
        // The doorway composited by depth (impl-211's spike, the default since impl-227): the quad
        // above gets the composite's material. `--portal-depth-composite=off` keeps the plain
        // quad, for comparison.
        if app
            .world()
            .resource::<EngineConfig>()
            .portal
            .depth_composite
        {
            app.add_plugins(depth_composite::DepthCompositePlugin);
        }
        // One log line per doorway opened: the frame times and the pipelines created over the
        // frames after it ([`measure_portal_opens`]). The counts are read in the render world.
        let counts = PipelineCounts::default();
        app.insert_resource(counts.clone())
            .add_systems(Update, measure_portal_opens.after(PortalFrame));
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .insert_resource(counts)
                .add_systems(Render, count_pipelines.in_set(RenderSystems::Cleanup));
        }
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
    /// The rectangle of the main view the portal target holds, in window pixels: `x`, `y` of its
    /// top-left corner, then its width and height ([`doorway_render_rect`]). A fragment of the
    /// quad samples the target at its own position within this rectangle. Zero - no rectangle
    /// measured, as in a run whose main camera has no size yet - means the whole main view.
    #[uniform(102)]
    doorway_rect: Vec4,
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
pub(crate) struct PortalState {
    roles: HashMap<Entity, CellRole>,
    destination: Vec<CellKey>,
    /// The door the portal is rendering through: the quad stands in its doorway. A door with no
    /// animation of its own has no leaf that can be swung out of the way, so this is what hides its
    /// whole model ([`show_load_door_leaves`]); an animated door keeps its frame, and its leaves
    /// are [`crate::door_animation`]'s to draw or hide.
    ///
    /// `None` whenever no portal is up - no camera to place, no door it draws through in range
    /// ([`portal_shows_through`]), or a run without the portal at all - which is when every load
    /// door draws its own leaf.
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
    /// The rectangle of the main view the doorway was last drawn in ([`doorway_render_rect`]):
    /// what the portal camera projects, what [`resize_portal_target`] sizes the target to and what
    /// the quad maps its fragments through.
    ///
    /// Kept while no doorway is drawn - the portal camera is off then and nothing samples the
    /// target - so a doorway that leaves the screen and comes back does not reallocate it. `None`
    /// until a doorway is measured, and whenever the main camera has no size to measure it
    /// against: the target then follows the main camera's whole view.
    render_rect: Option<URect>,
    /// The doorway the portal is drawing through this frame ([`OpenDoorway`]), or `None` exactly
    /// when [`open_door`](Self::open_door) is. Written by `update_portal` and read by nothing in
    /// this module: it is what other modules need to know about the doorway without a second
    /// opinion about where it is.
    doorway: Option<OpenDoorway>,
    /// The door whose destination [`destination`](Self::destination) is, and since when it has not
    /// been drawn through ([`DESTINATION_HOLD_SECONDS`]).
    hold: Option<DestinationHold>,
    /// The isolation's churn over the run, for the log ([`IsolationChurn`]).
    churn: IsolationChurn,
}

/// The door the portal last drew through and its destination cells, kept for
/// [`DESTINATION_HOLD_SECONDS`] after the portal stops drawing through it (impl-225).
#[derive(Debug, Clone, PartialEq)]
struct DestinationHold {
    door: Entity,
    destination: Vec<CellKey>,
    /// The clock time (seconds) the portal stopped drawing through the door, or `None` while it
    /// still does.
    lost_at: Option<f64>,
}

/// How often [`isolate_cells`] re-layered a resident cell and saw a cell come back with a new root,
/// over the run: impl-225's churn counts, logged at debug level with running totals.
#[derive(Debug, Default)]
struct IsolationChurn {
    /// Role changes of a root that already had a role: each one walks the root's whole subtree.
    relayer_walks: u64,
    /// The nodes those walks visited.
    relayer_nodes: u64,
    /// Roots of a cell that had been seen before under another root: a despawn and a respawn.
    respawns: u64,
    /// The last root seen for each cell.
    roots: HashMap<CellIdentity, Entity>,
}

impl IsolationChurn {
    /// A root's role changed from `from` (`None` for a root seen for the first time) to `to`, and
    /// its subtree of `nodes` nodes was walked.
    fn walked(&mut self, identity: CellIdentity, from: Option<CellRole>, to: CellRole, nodes: u64) {
        let Some(from) = from else {
            return;
        };
        self.relayer_walks += 1;
        self.relayer_nodes += nodes;
        debug!(
            cell = ?identity,
            ?from,
            ?to,
            nodes,
            walks = self.relayer_walks,
            walked_nodes = self.relayer_nodes,
            respawns = self.respawns,
            "portal churn: cell re-layered"
        );
    }

    /// A root seen for the first time: a respawn when its cell had another root before.
    fn appeared(&mut self, identity: CellIdentity, root: Entity) {
        if self
            .roots
            .insert(identity, root)
            .is_some_and(|previous| previous != root)
        {
            self.respawns += 1;
            debug!(
                cell = ?identity,
                walks = self.relayer_walks,
                walked_nodes = self.relayer_nodes,
                respawns = self.respawns,
                "portal churn: cell respawned with a new root"
            );
        }
    }
}

/// The doorway the portal is drawing through, as `update_portal` placed it this frame: the door,
/// the one [`DoorMap`] the window, the camera and the crossing are built on, and the quad standing
/// in the doorway (its translation is the doorway's centre, its scale the opening's width and
/// height, and it faces the side the player stands on).
///
/// `pub(crate)` for `crate::portal_spill`, which lights the two sides of the doorway from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OpenDoorway {
    /// The source door: the load door in the active space whose doorway the window stands in.
    pub(crate) door: Entity,
    /// The source door -> destination map.
    pub(crate) map: DoorMap,
    /// The doorway quad's transform, in the source space.
    pub(crate) quad: Transform,
}

impl PortalState {
    /// The portal draws through `door` this frame, into `destination`: the destination role goes to
    /// those cells, and any hold of another door's ends.
    fn draw_destination(&mut self, door: Entity, destination: Vec<CellKey>) {
        self.destination.clone_from(&destination);
        self.hold = Some(DestinationHold {
            door,
            destination,
            lost_at: None,
        });
    }

    /// The portal draws through no door this frame. The destination of the door it last drew
    /// through is kept for [`DESTINATION_HOLD_SECONDS`] from the first such frame, `now`, while
    /// `still_holds(door)` says that door still shows a way through into it; otherwise it is
    /// dropped at once. A run without a clock (`now` of `None`) holds nothing.
    fn lose_destination(&mut self, now: Option<f64>, still_holds: impl FnOnce(Entity) -> bool) {
        let kept = match (&mut self.hold, now) {
            (Some(hold), Some(now)) if still_holds(hold.door) => {
                let lost_at = *hold.lost_at.get_or_insert(now);
                (now - lost_at < DESTINATION_HOLD_SECONDS).then(|| hold.destination.clone())
            }
            _ => None,
        };
        match kept {
            Some(destination) => self.destination = destination,
            None => {
                if self.hold.take().is_some_and(|hold| hold.lost_at.is_some()) {
                    debug!("portal: held destination released");
                }
                self.destination.clear();
            }
        }
    }

    /// The doorway the portal is drawing through this frame, if any ([`OpenDoorway`]). Set only
    /// while the portal draws through a door ([`portal_shows_through`]), including while that
    /// doorway is off screen.
    pub(crate) fn open_doorway(&self) -> Option<OpenDoorway> {
        self.doorway
    }

    /// Whether the cell `entity` belongs to is part of the **active space**: the cells the
    /// streaming plan holds because the camera is in them, and the only ones the player walks in
    /// and takes doors from.
    ///
    /// A cell of another role is a space of its own, whose raw coordinates can lie anywhere over
    /// the player's: its meshes are drawn by the portal camera (`CellRole::Destination`) or hidden,
    /// and its references keep their components either way, so its floors, walls, load doors and
    /// invisible auto-load markers are all found by anything that walks the world. Nothing a
    /// visibility query looks at tells the two apart - `MeshRayCast`'s `RayCastVisibility::Visible`
    /// reads `InheritedVisibility`, which a destination cell keeps, and a legitimate auto-load
    /// marker is hidden on purpose.
    ///
    /// `true` for an entity that no cell root owns at all: the roles are keyed by the roots
    /// [`isolate_cells`] classifies, and geometry outside a streamed cell - and every run without a
    /// portal, where nothing is isolated - is the player's to walk in. The walk up the `ChildOf`
    /// chain is what finds the root; the map is the previous frame's, which is the space the camera
    /// is standing in now (the controller runs before the isolation, as `PlayerPlugin` and
    /// `PortalPlugin` register them).
    pub(crate) fn is_in_active_space(&self, entity: Entity, parents: &Query<&ChildOf>) -> bool {
        let mut cursor = Some(entity);
        while let Some(current) = cursor {
            if let Some(role) = self.roles.get(&current) {
                return *role == CellRole::Active;
            }
            cursor = parents.get(current).ok().map(ChildOf::parent);
        }
        true
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// The four corners of the doorway quad in world space, for the quad's `transform`.
///
/// The quad is [`setup_portal_quad`]'s unit plane facing `+Z`, scaled by [`update_portal`] to the
/// doorway's size, so its corners are the unit square's under that transform.
fn doorway_corners(transform: &Transform) -> [Vec3; 4] {
    [
        Vec3::new(-0.5, -0.5, 0.0),
        Vec3::new(0.5, -0.5, 0.0),
        Vec3::new(0.5, 0.5, 0.0),
        Vec3::new(-0.5, 0.5, 0.0),
    ]
    .map(|corner| transform.transform_point(corner))
}

/// The rectangle of the window the doorway covers, in the window's pixels (origin top left, `y`
/// down, the way Bevy measures viewports), or `None` when the doorway covers none of it.
///
/// `corners` is the doorway's outline in world space ([`doorway_corners`]), `view` the main
/// camera's world pose, `projection` its projection, and `window` the size of what that camera
/// draws into. The outline is clipped against the camera's near plane before it is projected: a
/// doorway the player stands beside has corners on both sides of the eye, and a corner behind the
/// eye projects to the *opposite* side of the screen, so projecting the four corners as they are
/// would put the doorway where it is not. The clipping is done in clip space, where the near plane
/// is `w = near` under Bevy's reverse-Z perspective and the map from the world is linear, so the
/// points it adds on the edges are the edges' own. The projected outline's bounding box is then
/// clamped to the window.
///
/// `None` is every case with nothing to draw: the whole doorway behind the eye, off to a side of
/// the screen, or seen with no area at all (a camera standing in the doorway's own plane).
/// [`update_portal`] takes the no-portal path for all of them - the portal camera would render a
/// whole frame nothing samples - and the rectangle is what a doorway rendered at its own size
/// needs next.
fn doorway_screen_rect(
    corners: [Vec3; 4],
    view: &Transform,
    projection: &Projection,
    window: Vec2,
) -> Option<Rect> {
    if window.x <= 0.0 || window.y <= 0.0 {
        return None;
    }
    let clip_from_world = projection.get_clip_from_view() * view.to_matrix().inverse();
    // The near plane in clip `w`: `w` is the view depth under a perspective projection, and 1
    // under an orthographic one, which has no eye to be behind.
    let near = match projection {
        Projection::Perspective(perspective) => perspective.near,
        _ => 0.0,
    }
    .max(f32::EPSILON);
    let clip = corners.map(|corner| clip_from_world * corner.extend(1.0));

    // Sutherland-Hodgman against the one plane `w >= near`: a convex outline stays one convex
    // polygon, with at most one corner more than it had.
    let mut kept: Vec<Vec4> = Vec::with_capacity(clip.len() + 1);
    for (index, &current) in clip.iter().enumerate() {
        let next = clip[(index + 1) % clip.len()];
        let current_in = current.w >= near;
        if current_in {
            kept.push(current);
        }
        if current_in != (next.w >= near) {
            kept.push(current.lerp(next, (near - current.w) / (next.w - current.w)));
        }
    }
    if kept.is_empty() {
        return None;
    }

    let mut low = Vec2::splat(f32::INFINITY);
    let mut high = Vec2::splat(f32::NEG_INFINITY);
    for point in kept {
        let ndc = point.truncate().truncate() / point.w;
        let pixel = Vec2::new(ndc.x + 1.0, 1.0 - ndc.y) * 0.5 * window;
        low = low.min(pixel);
        high = high.max(pixel);
    }
    let rect = Rect {
        min: low,
        max: high,
    }
    .intersect(Rect::from_corners(Vec2::ZERO, window));
    (!rect.is_empty()).then_some(rect)
}

/// The rectangle of the window the portal camera renders for a doorway covering `rect`
/// ([`doorway_screen_rect`]): `rect` rounded **out** to [`PORTAL_RECT_QUANTUM`] on every edge and
/// clamped to the window, or `None` when nothing of it is left.
///
/// Out, so every pixel the quad covers is inside it; in whole pixels, so the target's texels sit
/// on the window's pixels one for one and the doorway lines up with the room around it to the
/// pixel. The clamp comes after the rounding, so an edge at the window's own edge stays there
/// rather than rounding out past it: a rectangle touching the right or bottom edge has a size that
/// is not a multiple of the quantum, and it is still a stable size, because the window's is.
fn doorway_render_rect(rect: Rect, window: UVec2) -> Option<URect> {
    if rect.is_empty() {
        return None;
    }
    let quantum = PORTAL_RECT_QUANTUM as f32;
    let min = (rect.min / quantum).floor().max(Vec2::ZERO) * quantum;
    let max = ((rect.max / quantum).ceil() * quantum).min(window.as_vec2());
    let rect = URect::from_corners(min.as_uvec2(), max.max(min).as_uvec2());
    (rect.width() > 0 && rect.height() > 0).then_some(rect)
}

/// The portal camera's sub-view for a doorway rendered in `rect` of a main view `window` pixels
/// big: the cone through that rectangle of the main camera's frustum.
///
/// Bevy builds the projection of a sub-view as the off-axis frustum through the rectangle
/// (`PerspectiveProjection::get_clip_from_view_for_sub`) and then applies the projection's oblique
/// clip plane to it exactly as it does to the whole view, so the doorway's clip survives. What
/// lands at a pixel of the target is what the full projection lands at the matching pixel of the
/// rectangle - the same sight line, drawn by the same camera - which is why the quad can sample
/// the target by its own position in the rectangle.
fn doorway_sub_view(rect: URect, window: UVec2) -> SubCameraView {
    SubCameraView {
        full_size: window,
        offset: rect.min.as_vec2(),
        size: rect.size(),
    }
}

/// The rectangle as the quad's material carries it ([`PortalExtension::doorway_rect`]): its corner
/// and its size, in window pixels; zero for the whole main view.
fn doorway_rect_uniform(rect: Option<URect>) -> Vec4 {
    rect.map_or(Vec4::ZERO, |rect| {
        Vec4::new(
            rect.min.x as f32,
            rect.min.y as f32,
            rect.width() as f32,
            rect.height() as f32,
        )
    })
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

/// The doorway quad's transform for a door placed at `door_position`/`door_rotation` whose front
/// comes from `frame`: [`portal_quad_extents`]' size and centre, the quad standing in the doorway's
/// own plane and facing the side the player stands on.
///
/// A door with a [`DoorAnchor`] built on thresholds has its quad's bottom edge at that threshold
/// ([`anchored_threshold`], [`quad_on_threshold`]) instead of at the box's bottom: the portal camera
/// and the map's pivot stand the destination's threshold there, so that is where the destination
/// floor meets the doorway in the image. A quad reaching below it looks under the destination
/// floor, at the portal camera's clear colour (the band under Gerdur's House's doorway,
/// research-205), and a quad stopping above it leaves the source's own geometry showing between
/// the two floors.
///
/// One definition for the quad [`update_portal`] draws and the doorway it asks
/// [`select_portal_door`] about, so the doorway the pick measures on screen is the one drawn.
fn doorway_quad_transform(
    door_position: Vec3,
    door_rotation: Quat,
    frame: Quat,
    scale: Vec3,
    instance_bounds: Option<&InstanceBounds>,
    expected_bounds: Option<&ExpectedModelBounds>,
    anchor: Option<&DoorAnchor>,
) -> Transform {
    let (size, centre) = portal_quad_extents(
        instance_bounds,
        expected_bounds,
        door_rotation,
        frame,
        scale,
    );
    let (size, centre) = anchor
        .and_then(|anchor| anchored_threshold(door_rotation, frame, scale, anchor))
        .map_or((size, centre), |threshold| {
            quad_on_threshold(size, centre, threshold)
        });
    let front = frame * Vec3::NEG_Z;
    Transform {
        translation: door_position + frame * centre + front * PORTAL_QUAD_OFFSET,
        // `Plane3d` faces `+Z` and the player stands on the door's front (`-Z`).
        rotation: frame * Quat::from_rotation_y(PI),
        scale: Vec3::new(size.x, size.y, 1.0),
    }
}

/// The height in `frame` (over the door's reference) of the threshold a [`DoorAnchor`] stands its
/// source doorway on, or `None` when the anchor was built on the doorways' box *centres* and has no
/// threshold to give.
///
/// The anchor carries one height, [`DoorAnchor::source_anchor_height`]: the threshold when both
/// doorways were read as ones a floor meets, the box centre's height otherwise
/// (`crate::doors::doorway_anchor`). The two are told apart by the centre's own height. A threshold
/// is the box's bottom or a floor at most `ANCHOR_SUNK_CAP` above it, on a doorway at least
/// `ANCHOR_MIN_DOORWAY_HEIGHT` tall, so it stands well off its box centre; only a door sunk by very
/// nearly half its height could bring the two within [`THRESHOLD_FROM_CENTRE`], and that door keeps
/// the box's quad, as it did before.
///
/// The point read is the pivot `crate::transition::source_doorway_centre` places, in `frame`, so the
/// quad's bottom and the portal camera's anchor are one point.
fn anchored_threshold(
    door_rotation: Quat,
    frame: Quat,
    scale: Vec3,
    anchor: &DoorAnchor,
) -> Option<f32> {
    let box_centre = door_rotation * (Vec3::from_array(anchor.source_box_centre) * scale);
    if (anchor.source_anchor_height - box_centre.y).abs() <= THRESHOLD_FROM_CENTRE {
        return None;
    }
    let pivot = Vec3::new(box_centre.x, anchor.source_anchor_height, box_centre.z);
    let height = (frame.inverse() * pivot).y;
    height.is_finite().then_some(height)
}

/// How close to the box centre's height an anchor's height has to be to be read as the centre
/// rule's rather than a threshold ([`anchored_threshold`]), in units.
const THRESHOLD_FROM_CENTRE: f32 = 0.5;

/// The quad of `size` and `centre` (in the door's frame) with its bottom edge moved to `threshold`
/// and its top, sides and depth unchanged. A threshold that leaves less than [`MIN_PORTAL_SIZE`] of
/// doorway under the top keeps the box's quad.
///
/// No overlap below the threshold. The map stands the destination's threshold on this one, so a
/// pixel of the quad above it looks down onto the destination floor beyond the doorway's plane, and
/// a pixel below it looks under that floor, at nothing: the portal camera's clear colour. What
/// stands below the threshold on the source side (the sill, the ground) is drawn by the main
/// camera, as beside the quad's other edges.
fn quad_on_threshold(size: Vec2, centre: Vec3, threshold: f32) -> (Vec2, Vec3) {
    let top = centre.y + size.y * 0.5;
    if top - threshold < MIN_PORTAL_SIZE {
        return (size, centre);
    }
    (
        Vec2::new(size.x, top - threshold),
        Vec3::new(centre.x, (top + threshold) * 0.5, centre.z),
    )
}

/// How many frames the portal may draw through a door whose floors the probe cannot find before
/// [`anchor_on_measured_floors`] gives up on it and keeps the database's anchor: about four seconds
/// at the demo's frame rate, time for the destination's floor to stream in and be validated.
const FLOOR_PROBE_ATTEMPTS: u32 = 240;

/// How far above and below the height an anchor already has the floor probe looks, in units: the
/// window [`DoorAnchor::on_measured_floors`] accepts a floor in.
const FLOOR_PROBE_REACH: f32 = crate::doors::ANCHOR_SUNK_CAP;

/// The meshes the floor probe at a doorway passes through: the quad, water and the doorway's mirror.
type NotAFloor = Or<(With<PortalQuad>, With<WaterSurface>, With<PortalDoorMirror>)>;

/// The door the portal is drawing through, re-anchored on the floors at its two doorways
/// ([`DoorAnchor::on_measured_floors`]): the floor just in front of the source doorway and the
/// floor just inside the destination one, each found by a ray cast straight down onto the resident
/// meshes of its own space, as the player's ground probe finds the ground.
///
/// Only a door whose anchor stands on thresholds ([`anchored_threshold`]); a centre anchor, an
/// unanchored door and a door already measured ([`DoorwayFloorsMeasured`]) are left alone. The
/// destination point is the source point's image through the door's own map, one
/// [`DOORWAY_FLOOR_PROBE_DEPTH`] behind the doorway's plane, so the two probes stand either side of
/// the one doorway. The door's own model (its leaf and frame), the doorway's mirror, the quad and
/// water are not floors; the source probe sees only the active space and the destination probe only
/// the destination cells the portal is drawing, whose raw coordinates can lie anywhere over the
/// active space's.
///
/// After [`isolate_cells`], which reveals the destination: its meshes can be hit once the visibility
/// systems have run on them, which is the frame after they are revealed, so a door is tried again
/// every frame the portal draws through it until both probes find a floor, or until
/// [`FLOOR_PROBE_ATTEMPTS`]. The re-anchored map takes effect from the next frame's
/// [`update_portal`], and the crossing, the mirror and the player's doorway plane all read it from
/// the same component.
#[allow(clippy::too_many_arguments)]
fn anchor_on_measured_floors(
    mut commands: Commands,
    state: Res<PortalState>,
    mut doors: Query<
        (&GlobalTransform, &Transform, &LoadDoor, &mut DoorAnchor),
        Without<DoorwayFloorsMeasured>,
    >,
    mut ray_cast: MeshRayCast,
    parents: Query<&ChildOf>,
    load_doors: Query<(), With<LoadDoor>>,
    not_floors: Query<(), NotAFloor>,
    mut attempts: Local<HashMap<Entity, u32>>,
) {
    let Some(doorway) = state.open_doorway() else {
        return;
    };
    let Ok((global, local, door, mut anchor)) = doors.get_mut(doorway.door) else {
        return;
    };
    let map = doorway.map;
    let door_position = global.translation();
    if anchored_threshold(global.rotation(), map.frame, local.scale, &anchor).is_none() {
        commands.entity(doorway.door).insert(DoorwayFloorsMeasured);
        return;
    }
    // The role of the cell a mesh belongs to, or `None` for a mesh that is no floor at all.
    let role_of = |entity: Entity| -> Option<CellRole> {
        let mut cursor = Some(entity);
        while let Some(current) = cursor {
            if load_doors.contains(current) || not_floors.contains(current) {
                return None;
            }
            if let Some(role) = state.roles.get(&current) {
                return Some(*role);
            }
            cursor = parents.get(current).ok().map(ChildOf::parent);
        }
        None
    };
    let mut floor_under = |point: Vec3, height: f32, role: CellRole| -> Option<f32> {
        let filter = |entity: Entity| role_of(entity) == Some(role);
        let settings = MeshRayCastSettings::default()
            .with_filter(&filter)
            .with_visibility(RayCastVisibility::Visible)
            .always_early_exit();
        let origin = Vec3::new(point.x, height + FLOOR_PROBE_REACH, point.z);
        let hits = ray_cast.cast_ray(Ray3d::new(origin, Dir3::NEG_Y), &settings);
        let (_, hit) = hits.first()?;
        (hit.distance <= 2.0 * FLOOR_PROBE_REACH).then_some(hit.point.y)
    };
    let front = map.frame * Vec3::NEG_Z;
    let source_point = map.pivot + front * DOORWAY_FLOOR_PROBE_DEPTH;
    let (destination_point, _) = map.pose(
        map.pivot - front * DOORWAY_FLOOR_PROBE_DEPTH,
        Quat::IDENTITY,
    );
    let source = floor_under(source_point, map.pivot.y, CellRole::Active);
    let destination = floor_under(
        destination_point,
        map.arrival_position.y,
        CellRole::Destination,
    );
    let tried = attempts.entry(doorway.door).or_default();
    *tried += 1;
    let (Some(source), Some(destination)) = (source, destination) else {
        if *tried >= FLOOR_PROBE_ATTEMPTS {
            attempts.remove(&doorway.door);
            commands.entity(doorway.door).insert(DoorwayFloorsMeasured);
            info!(
                door = format_args!("{:08X}", door.ref_id),
                source_found = source.is_some(),
                destination_found = destination.is_some(),
                "portal: no floor at the doorway; the database's thresholds are kept"
            );
        }
        return;
    };
    attempts.remove(&doorway.door);
    commands.entity(doorway.door).insert(DoorwayFloorsMeasured);
    // Both as heights over each side's own reference: the source over the door's, the destination
    // over its own - the anchor's height there plus how far the floor stands off the anchor point.
    let source_floor = source - door_position.y;
    let destination_floor =
        anchor.destination.anchor_height + (destination - map.arrival_position.y);
    let before = (
        anchor.source_anchor_height,
        anchor.destination.anchor_height,
    );
    match anchor.on_measured_floors(source_floor, destination_floor) {
        Some(measured) => {
            info!(
                door = format_args!("{:08X}", door.ref_id),
                source_threshold = before.0,
                destination_threshold = before.1,
                source_floor,
                destination_floor,
                step = (destination_floor - before.1) - (source_floor - before.0),
                "portal: doorway re-anchored on the floors at its two doorways"
            );
            *anchor = measured;
        }
        None => info!(
            door = format_args!("{:08X}", door.ref_id),
            source_floor,
            destination_floor,
            "portal: the floors at the doorway are too far off its thresholds; kept"
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

/// Whether the portal draws its window through a door in this state: `Opening`, `Open` and
/// `Closing`.
///
/// This is the portal's own question about a [`DoorState`], and it is deliberately **wider** than
/// [`DoorState::is_open`] (the helper the crossing and the player's doorway trigger ask through
/// `crate::transition::door_is_open`, which must stay "fully open"). A `Closing` door is one the
/// player is watching shut: the `Close` clip swings the leaf back across the doorway and
/// `crate::door_animation` draws it there, and the far side belongs *behind* that leaf for the
/// whole swing - clearing the doorway the frame the swing starts emptied the doorway under the
/// closing leaf, so the player closing a door saw the source side through it (the user, in play,
/// 2026-09-25: "when closing a door, the portal stops rendering before it can fully close"). The
/// same holds while a door is `Opening`: the far side is there from the first frame of the swing,
/// not from the one the clip finishes on.
///
/// What makes `Closed` different is that nothing is drawn over the doorway any more: the window
/// stands in the only opening a door has, so a `Closed` door would put the destination image behind
/// its own closed leaf. A door with no [`DoorState`] at all counts as closed, like everywhere else.
fn portal_shows_through(state: Option<&DoorState>) -> bool {
    state.is_some_and(|state| {
        matches!(
            state,
            DoorState::Opening | DoorState::Open { .. } | DoorState::Closing
        )
    })
}

/// Whether a door in this state is one whose own animation moves its model - and therefore one with
/// a leaf that is drawn or hidden on its own account rather than by the portal
/// ([`crate::door_animation`]): the swing of an `Opening` or `Closing` door, and an `Open` one that
/// has a clip of its own.
///
/// False for a door with no clip at all, whose own model *is* its leaf (`Open { animated: false }`
/// is the state `DoorState::hides_whole_reference` answers true for), and for a closed one.
fn door_has_its_own_swing(state: Option<&DoorState>) -> bool {
    matches!(
        state,
        Some(DoorState::Opening | DoorState::Closing | DoorState::Open { animated: true })
    )
}

/// How much better aimed, in degrees, one on-screen doorway has to be than another before it wins
/// over a nearer one: within this the view cannot tell the two sight lines apart, and distance
/// decides. The slack the player's own door target takes
/// ([`crate::player::TARGET_AIM_SLACK_DEGREES`]).
const PORTAL_AIM_SLACK_DEGREES: f32 = crate::player::TARGET_AIM_SLACK_DEGREES;

/// The door the portal renders through: of the doors whose doorway the portal draws a window in
/// ([`portal_shows_through`] - `Opening`, `Open` or `Closing`) in the active space, whose
/// destination is resident and not itself part of the active space, and whose plane the camera is
/// on the front side of (a [`distance_in_front_of_door`] of at least
/// [`MIN_PORTAL_DOOR_DISTANCE`]), the one whose doorway the player **looks at**.
///
/// `aim` answers, per door, how far off the middle of the view its doorway is - the angle in
/// radians between the view's forward and the doorway's centre - or `None` when the doorway covers
/// none of the screen ([`doorway_screen_rect`]). A doorway on screen beats every doorway off it;
/// among those on screen the best aimed wins, the rule the player's own door target uses
/// (`crate::player::target_door`), with the nearer door taking a tie within
/// [`PORTAL_AIM_SLACK_DEGREES`]. With no doorway on screen the nearest door is kept, as before, so
/// its leaf, mirror and destination stay up while the player looks away and the frame it comes back
/// into view is the frame it is drawn in.
///
/// Nearest alone picked the wrong door wherever two open doorways stand close together: at the
/// Riverwood Trader the upper door `00070E69`, 179.8 units from the camera and 52 degrees above the
/// view, beat the front door `0001341F` the player was looking into from 220.5, so the front
/// doorway got no window and showed the door's own plug (research-193, the user's capture `01`).
/// The far door the portal hides and the mirror it places both follow this pick.
///
/// A doorway the portal draws in is the gate that makes the window a doorway: the quad stands in
/// the only opening a door has, so rendering through a `Closed` one would put the destination image
/// behind the door's own leaf. A door mid-swing keeps the window, because its leaf is drawn *over*
/// the window for the whole swing ([`portal_shows_through`] has the rest).
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
    aim: impl Fn(Entity) -> Option<f32>,
) -> Option<Entity> {
    let slack = PORTAL_AIM_SLACK_DEGREES.to_radians();
    // The best so far: the door, how far off the view's middle its doorway is (`None` when it is
    // off screen) and how far away it stands.
    let mut best: Option<(Entity, Option<f32>, f32)> = None;
    for (entity, position, door, state, anchor) in doors {
        let distance = position.distance(camera);
        if distance > DOOR_PRESTREAM_RADIUS {
            continue;
        }
        if !portal_shows_through(state) {
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
        let angle = aim(entity);
        let better = match best {
            None => true,
            Some((_, best_angle, best_distance)) => match (angle, best_angle) {
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => distance < best_distance,
                // Better aimed by more than the slack wins outright; within it the view cannot
                // tell the two apart, and the nearer one wins.
                (Some(angle), Some(best_angle)) => {
                    angle < best_angle - slack
                        || ((angle - best_angle).abs() <= slack && distance < best_distance)
                }
            },
        };
        if better {
            best = Some((entity, angle, distance));
        }
    }
    best.map(|(entity, ..)| entity)
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
///
/// Its **`Transform`**, not its `GlobalTransform`: the player moves the camera earlier in this same
/// `Update`, and `GlobalTransform` is only propagated in `PostUpdate`, so reading it here placed the
/// portal camera where the main camera was a frame ago - the doorway image trailed every move and
/// turn by one frame (the user saw it in play, 2026-09-24). The main camera is a root entity (spawned
/// on its own by `app::setup_world`), so its `Transform` is its world pose.
///
/// Its **`Camera`** is optional and read for one thing: the size of the window it draws into, which
/// [`doorway_screen_rect`] measures the doorway against. A main camera without one (the unit tests'
/// cameras), or one whose size `camera_system` has not filled in yet, is taken to see the doorway,
/// which is what the portal did before it asked.
type MainCameraQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        &'static Transform,
        &'static Projection,
        Option<&'static Camera>,
    ),
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

/// Grows or shrinks the portal render target to the rectangle of the screen the doorway covers
/// ([`PortalState::render_rect`]), so the doorway is drawn one texel per window pixel - the
/// resolution of the room around it - and no larger than the doorway is.
///
/// Until a doorway has been measured the target follows the main camera's whole view, from its
/// *computed* target info (`Camera::computed.target_info`, filled by `camera_system` from the
/// camera's `RenderTarget`): the physical size of whatever that camera draws into. On the first
/// frame of a run that is not computed yet, and a run with no window has none at all: both keep
/// [`PORTAL_TEXTURE_FALLBACK_SIZE`]. Either size is held to [`PORTAL_TEXTURE_MAX_SIZE`].
///
/// **Nothing is allocated unless the size changes**, and what the decision is keyed on is the size
/// of the image the resource already points at - `Image::size`, not a copy of the last request, so
/// there is one source of truth for "how big is the target" and an unchanged rectangle does no
/// work at all; the rectangle is rounded to [`PORTAL_RECT_QUANTUM`] for exactly this. A size that
/// does change is a new image, with the camera's `RenderTarget` and the quad's material repointed
/// at it in the same frame: the material's bind group is rebuilt from the changed asset, and the
/// camera would otherwise render into the new texture while the doorway still sampled the old one.
/// The rectangle the quad maps through ([`PortalExtension::doorway_rect`]) is written in that same
/// frame too, and only when it changed.
fn resize_portal_target(
    state: Option<Res<PortalState>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<PortalMaterial>>,
    mut texture: ResMut<PortalTexture>,
    main: MainCameraTargetQuery,
    mut targets: Query<&mut RenderTarget, With<PortalCamera>>,
    quad: Query<&MeshMaterial3d<PortalMaterial>, With<PortalQuad>>,
) {
    // The doorway's own rectangle when [`update_portal`] has measured one this run, and the main
    // camera's whole view until it has.
    let rect = state.and_then(|state| state.render_rect);
    let size =
        match rect {
            Some(rect) => portal_target_size(rect.size(), PORTAL_TEXTURE_MAX_SIZE),
            None => main.single().ok().and_then(|camera| {
                camera.computed.target_info.as_ref().and_then(|info| {
                    portal_target_size(info.physical_size, PORTAL_TEXTURE_MAX_SIZE)
                })
            }),
        };
    let resized = size
        .filter(|&size| images.get(&texture.0).map(Image::size) != Some(size))
        .map(|size| {
            let image = images.add(portal_target_image(size));
            texture.0 = image.clone();
            if let Ok(mut target) = targets.single_mut() {
                *target = RenderTarget::Image(image.clone().into());
            }
            debug!(
                width = size.x,
                height = size.y,
                "portal: render target resized to the doorway's rectangle"
            );
            image
        });

    // The rectangle the quad maps its fragments through changes with the target's contents, in the
    // same frame; the material is only touched when something did change, since a changed material
    // is a rebuilt bind group.
    let doorway_rect = doorway_rect_uniform(rect);
    let Ok(handle) = quad.single() else {
        return;
    };
    let stale = materials
        .get(handle)
        .is_some_and(|material| material.extension.doorway_rect != doorway_rect);
    if (resized.is_some() || stale)
        && let Some(mut material) = materials.get_mut(handle)
    {
        if let Some(image) = resized {
            material.extension.portal_texture = Some(image);
        }
        material.extension.doorway_rect = doorway_rect;
    }
}

/// Fits the portal camera's culling and shadow frustum to the view it actually renders, after
/// Bevy's camera update and before anything reads them.
///
/// Two of Bevy's own systems see the portal camera's *projection* without its sub-view:
///
/// * `camera_system` sets the projection's aspect ratio to the render target's, which is now the
///   doorway rectangle's and not the main view's. The sub-view's projection does not read it, but
///   the directional-light cascades are built from `Projection::get_frustum_corners`, which does:
///   the destination sun's cascades would cover a frustum as narrow as the doorway is, centred on
///   the view axis rather than on the doorway. The main camera's aspect ratio is put back, so the
///   cascades cover the main view's frustum - the one the sub-view is a piece of - as they did
///   before the doorway was rendered at its own size.
/// * `update_frusta` builds the culling frustum from the whole projection, so everything in the
///   main view's cone would still be extracted, sorted and have its vertices run. The frustum is
///   rebuilt from the sub-view's projection instead: the cone through the doorway's rectangle,
///   with the oblique doorway plane as its near plane, so what cannot be seen through the doorway
///   is not drawn at all.
fn fit_portal_view(
    main: Query<&Projection, (With<StreamingCamera>, Without<PortalCamera>)>,
    mut portal: Query<
        (&Camera, &GlobalTransform, &mut Projection, &mut Frustum),
        With<PortalCamera>,
    >,
) {
    let Ok((camera, transform, mut projection, mut frustum)) = portal.single_mut() else {
        return;
    };
    if let (Ok(Projection::Perspective(main)), Projection::Perspective(portal)) =
        (main.single(), projection.as_ref())
        && portal.aspect_ratio != main.aspect_ratio
        && let Projection::Perspective(portal) = projection.as_mut()
    {
        portal.aspect_ratio = main.aspect_ratio;
    }
    let (true, Some(sub_view)) = (camera.is_active, camera.sub_camera_view.as_ref()) else {
        return;
    };
    *frustum = sub_view_frustum(&projection, sub_view, transform);
}

/// The culling frustum of a camera that renders `sub_view` of `projection`: the frustum of the
/// sub-view's own clip matrix, with the projection's far distance.
fn sub_view_frustum(
    projection: &Projection,
    sub_view: &SubCameraView,
    transform: &GlobalTransform,
) -> Frustum {
    let clip_from_world =
        projection.get_clip_from_view_for_sub(sub_view) * transform.to_matrix().inverse();
    Frustum(ViewFrustum::from_clip_from_world_custom_far(
        &clip_from_world,
        &transform.translation(),
        &transform.back(),
        projection.far(),
    ))
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
        // The destination is drawn from the doorway's own clip plane to the shared far plane, and
        // handed over untonemapped ([`portal_camera_output`]).
        portal_camera_output(),
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
        // The graphics settings that are part of the main view's pipeline key (SSAO, contact
        // shadows, the shadow filter, TAA's prepass and jitter) and its exposure, written by
        // `crate::graphics_settings` so the doorway's pipelines stay the main view's.
        crate::graphics_settings::GraphicsCamera::Portal,
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
        portal_sun_cascades(PORTAL_SUN_SHADOW_CASCADES),
        Transform::default(),
        RenderLayers::layer(DESTINATION_LAYER),
    ));
}

/// How the portal camera writes its image: HDR, untonemapped and undithered.
///
/// **Not tonemapped:** the quad's material hands the image to the main camera's tonemapper, and
/// tonemapping it here as well would darken the doorway against the room around it. What it hands
/// over is not a clipped 8-bit copy of the destination either - the target is float
/// ([`PORTAL_TEXTURE_FORMAT`]), so the values above white reach that tonemapper intact.
///
/// **`Hdr`** makes this view's mesh pipeline key the main camera's. Bevy adds the tonemap and
/// dither bits (`TONEMAP_IN_SHADER | TONEMAP_METHOD_NONE | DEBAND_DITHER`) only to a view that is
/// not HDR, and those bits were the whole difference between the two views, so every material
/// variant a destination showed was specialized and compiled again on the first open of each
/// doorway (research-197, `local/research/portal-first-open-hitch-2026-09-25.md`): 38 pipelines
/// and 18 frames over 25 ms on the Riverwood route's first door. With the bits gone the doorway
/// reuses what the main view already compiled: 11 pipelines and 3 frames over 25 ms since, and
/// those 11 are the view's own tonemapping and blit pipelines plus material and shadow variants no
/// view had drawn before (the room's own materials, which walking in would compile anyway). The
/// image does not change: the view renders into
/// an `Rgba16Float` intermediate - the format of the target - and with `Tonemapping::None` Bevy's
/// tonemapping pass returns before drawing, so what reaches the target is a straight copy of the
/// scene-referred values.
///
/// **`DebandDither::Disabled`**, so no pass of this view dithers the doorway: the main camera
/// dithers it along with the room around it. (`Camera3d` requires `DebandDither::Enabled`.)
fn portal_camera_output() -> (Hdr, Tonemapping, DebandDither) {
    (Hdr, Tonemapping::None, DebandDither::Disabled)
}

/// How many frames after a doorway opens [`measure_portal_opens`] watches.
const OPEN_WINDOW_FRAMES: u32 = 60;

/// A frame longer than this, in milliseconds, is counted as a hitch by [`measure_portal_opens`].
const OPEN_HITCH_MS: f32 = 25.0;

/// The render world's pipeline cache, as two numbers the main world can read: every render and
/// compute pipeline ever queued, and those still compiling. Written once a frame by
/// [`count_pipelines`] (the two worlds share the one allocation), read by [`measure_portal_opens`].
#[derive(Resource, Clone, Default)]
struct PipelineCounts(Arc<PipelineCountsInner>);

#[derive(Default)]
struct PipelineCountsInner {
    created: AtomicU32,
    waiting: AtomicU32,
}

impl PipelineCounts {
    fn read(&self) -> (u32, u32) {
        (
            self.0.created.load(Ordering::Relaxed),
            self.0.waiting.load(Ordering::Relaxed),
        )
    }
}

/// Render world: publishes the pipeline cache's size and its waiting count, after this frame's
/// queue has been processed.
fn count_pipelines(cache: Res<PipelineCache>, counts: Res<PipelineCounts>) {
    let created = u32::try_from(cache.pipelines().count()).unwrap_or(u32::MAX);
    let waiting = u32::try_from(cache.waiting_pipelines().count()).unwrap_or(u32::MAX);
    counts.0.created.store(created, Ordering::Relaxed);
    counts.0.waiting.store(waiting, Ordering::Relaxed);
}

/// The frames after one doorway opened, as [`measure_portal_opens`] adds them up.
#[derive(Debug, Default, Clone, PartialEq)]
struct OpenWindow {
    ref_id: u32,
    frames: u32,
    start_created: u32,
    last_created: u32,
    max_waiting: u32,
    frames_waiting: u32,
    total_ms: f32,
    max_ms: f32,
    hitches: u32,
}

impl OpenWindow {
    fn new(ref_id: u32, created: u32) -> Self {
        Self {
            ref_id,
            start_created: created,
            last_created: created,
            ..default()
        }
    }

    fn add_frame(&mut self, frame_ms: f32, created: u32, waiting: u32) {
        self.frames += 1;
        self.total_ms += frame_ms;
        self.max_ms = self.max_ms.max(frame_ms);
        if frame_ms > OPEN_HITCH_MS {
            self.hitches += 1;
        }
        self.last_created = created;
        self.max_waiting = self.max_waiting.max(waiting);
        if waiting > 0 {
            self.frames_waiting += 1;
        }
    }

    fn created(&self) -> u32 {
        self.last_created.saturating_sub(self.start_created)
    }
}

/// Logs one line per doorway opened: over the [`OPEN_WINDOW_FRAMES`] frames after the portal
/// starts rendering through a door, the frame times and how many pipelines the render world
/// created and waited on. A doorway whose view needs pipelines the main view has not compiled pays
/// for them in these frames, on the first open of each destination (`local/research/`
/// `portal-first-open-hitch-2026-09-25.md`, research-197).
fn measure_portal_opens(
    state: Res<PortalState>,
    counts: Res<PipelineCounts>,
    time: Res<Time<Real>>,
    doors: Query<&LoadDoor>,
    mut last_door: Local<Option<Entity>>,
    mut window: Local<Option<OpenWindow>>,
    mut opens: Local<u32>,
) {
    let (created, waiting) = counts.read();
    if let Some(open) = window.as_mut() {
        open.add_frame(time.delta_secs() * 1000.0, created, waiting);
        if open.frames >= OPEN_WINDOW_FRAMES {
            info!(
                open = *opens,
                door = format_args!("{:08X}", open.ref_id),
                frames = open.frames,
                mean_ms = open.total_ms / open.frames as f32,
                max_ms = open.max_ms,
                hitches = open.hitches,
                pipelines_created = open.created(),
                max_waiting = open.max_waiting,
                frames_waiting = open.frames_waiting,
                "portal: doorway open"
            );
            *window = None;
        }
    }
    let door = state.open_door;
    if door.is_some() && door != *last_door && window.is_none() {
        *opens += 1;
        let ref_id = door
            .and_then(|door| doors.get(door).ok())
            .map_or(0, |door| door.ref_id);
        *window = Some(OpenWindow::new(ref_id, created));
    }
    *last_door = door;
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
    let atmosphere = crate::atmosphere::space_atmosphere(catalog.as_deref(), destination);
    // The camera's three are written on a change of destination and not every frame: they are read
    // through change detection, and a doorway standing open for a minute would otherwise mark a
    // camera and its view changed sixty times a second for no reason. Nothing else moves them - the
    // catalog is read once at startup and the radii are the run's.
    if *applied != Some(destination) {
        *applied = Some(destination);
        camera.clear_color = ClearColorConfig::Custom(atmosphere.backdrop);
        *ambient = crate::atmosphere::ambient_light(&atmosphere);
        *fog = crate::atmosphere::atmosphere_fog(
            &atmosphere,
            config.stream_radius,
            config.terrain_radius,
        );
    }
    // The sun is written whenever it is not the sun this destination wants, and not only on a
    // change of destination: `crate::atmosphere::update_atmosphere` writes *every*
    // `DirectionalLight` in the world when the space the player stands in changes - this one
    // included, since it cannot know whose it is - and a change of the active space is exactly what
    // a crossing is. A change-only write would leave the doorway carrying the space the player came
    // from until the portal retargeted, and a write on every frame would mark the light changed
    // sixty times a second. Comparing is what makes this one line self-healing instead.
    for mut sun in &mut suns {
        if sun.color != atmosphere.sun.color {
            sun.color = atmosphere.sun.color;
        }
        if sun.illuminance != atmosphere.sun.illuminance {
            sun.illuminance = atmosphere.sun.illuminance;
        }
    }
}

/// The doorway sun's own shadow cascades: `count` of them over [`PORTAL_SUN_SHADOW_DISTANCE`],
/// rather than the engine sun's set, which reaches six times as far.
fn portal_sun_cascades(count: usize) -> CascadeShadowConfig {
    CascadeShadowConfigBuilder {
        num_cascades: count.max(1),
        minimum_distance: PORTAL_SUN_SHADOW_NEAR,
        maximum_distance: PORTAL_SUN_SHADOW_DISTANCE,
        first_cascade_far_bound: PORTAL_SUN_FIRST_CASCADE,
        overlap_proportion: PORTAL_SUN_CASCADE_OVERLAP,
    }
    .build()
}

/// Puts the doorway's own sun ([`PortalDestinationSun`]) where the engine's sun is: the same
/// direction, shadows on when the engine sun's are, and as many cascades as it has.
///
/// The *direction* is the engine sun's and nothing else's. It is a run-wide constant
/// (`app::setup_world` builds it from a rotation), and copying the component rather than naming
/// that rotation again is what keeps one sun in the world: a doorway whose shadow fell the other
/// way from the room around it would be a doorway drawn at a wall angle of its own. What is the
/// *destination's* is the tint and the illuminance, and that is
/// [`update_destination_atmosphere`]'s.
///
/// It casts shadows when it lights the destination at all ([`crate::atmosphere::sun_casts_shadows`]:
/// an interior's sun gives none, and its four cascades were drawn for nothing - research-228), but
/// the cascades are the doorway's own ([`portal_sun_cascades`]): the engine sun's *count*, which
/// Bevy needs every shadowed directional light to share ([`PORTAL_SUN_SHADOW_CASCADES`]), over the
/// doorway's much shorter reach ([`PORTAL_SUN_SHADOW_DISTANCE`]). Bevy budgets cascades per *view*
/// (`bevy_pbr-0.19.0/src/render/light.rs:1323-1353`), so each set is drawn in its own camera's view
/// alone and the main camera's are untouched. The count is matched in the same write as the shadows
/// are switched on, so the doorway's sun never casts with a count of its own.
fn place_destination_sun(
    config: Option<Res<EngineConfig>>,
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
        // Rebuilt only when the count differs: writing the component unconditionally would mark
        // the light changed every frame.
        let count = engine_cascades.map_or(PORTAL_SUN_SHADOW_CASCADES, |c| c.bounds.len());
        if cascades.bounds.len() != count {
            *cascades = portal_sun_cascades(count);
        }
        let shadows = config.as_deref().map_or(light.illuminance > 0.0, |config| {
            crate::atmosphere::sun_casts_shadows(light.illuminance, config)
        });
        if light.shadow_maps_enabled != shadows {
            light.shadow_maps_enabled = shadows;
        }
    }
}

/// Drops every directional light's cascades for the views that do not draw that light: a view
/// whose `RenderLayers` do not meet the light's.
///
/// **Why.** Bevy builds a shadowed sun's cascades for *every* active camera
/// (`bevy_light-0.19.0/src/cascade.rs:195-214`), and then culls every shadow caster on the sun's
/// layers against each of those cascade frusta (`check_dir_light_mesh_visibility`,
/// `bevy_light-0.19.0/src/lib.rs:336`), marking what it finds `ViewVisibility` - so extracted to
/// the render world. Only the render world asks whether the view draws the light at all
/// (`bevy_pbr-0.19.0/src/render/light.rs:1743-1771`), and skips the ones it does not. The
/// doorway's sun ([`PortalDestinationSun`], layer 2) therefore culled the whole resident
/// destination against the *main* camera's and the water reflection's cascades every frame a door
/// stood open - including every frame its doorway was behind the player or behind a wall and the
/// portal camera was off, when no view drew any of it - and the engine sun (layer 0) culled the
/// active space against the portal camera's cascades while a doorway was drawn. Neither set of
/// cascades was ever rendered.
///
/// The rule is the render world's own - the light's layers, none meaning layer 0, against the
/// view's - so a cascade it would have rendered is never dropped. A cascade of an entity that is
/// not a camera is left alone.
fn keep_cascades_of_views_that_draw_their_light(
    cameras: Query<Option<&RenderLayers>, With<Camera>>,
    mut lights: Query<(&mut Cascades, Option<&RenderLayers>), With<DirectionalLight>>,
) {
    for (mut cascades, light_layers) in &mut lights {
        let light_layers = light_layers.unwrap_or_default();
        let draws = |view: &Entity| {
            cameras
                .get(*view)
                .ok()
                .is_none_or(|view_layers| view_layers.unwrap_or_default().intersects(light_layers))
        };
        if cascades.cascades.keys().all(draws) {
            continue;
        }
        cascades.cascades.retain(|view, _| draws(view));
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
        let animated = door_has_its_own_swing(door_state);
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
        let (mirror_nodes, door_nodes) = (
            scene_nodes(mirror_root, &children, &targets),
            scene_nodes(mirror.door, &children, &targets),
        );
        if mirror_nodes != door_nodes {
            debug!(
                door = format_args!("{:08X}", door.ref_id),
                mirror_nodes,
                door_nodes,
                "portal: doorway mirror waits for its scene to match the door's"
            );
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
            doorway_rect: doorway_rect_uniform(None),
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
    let animated = door_has_its_own_swing(door_state);
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

/// What a cell root's role asks of the layers of everything under it: `None` keeps (or restores)
/// the layers a node was spawned with.
fn layers_for(role: CellRole) -> Option<RenderLayers> {
    match role {
        CellRole::Active => None,
        CellRole::Destination => Some(RenderLayers::layer(DESTINATION_LAYER)),
        CellRole::Hidden => Some(RenderLayers::none()),
    }
}

/// Gives one node under a cell root the layers its root's role asks for, recording the layers it
/// had in [`PortalOriginalLayers`] the first time it is moved and putting them back when the role
/// asks for none. Idempotent: a node already on the wanted layers is not written, so its
/// `Changed<RenderLayers>` - which `crate::shadow_layers` re-extracts the shadow entity on - only
/// fires on a real move.
fn relayer_node(
    commands: &mut Commands,
    entity: Entity,
    wanted: Option<&RenderLayers>,
    nodes: &Query<(Option<&RenderLayers>, Option<&PortalOriginalLayers>)>,
) {
    let Ok((current, original)) = nodes.get(entity) else {
        return;
    };
    match wanted {
        Some(wanted) => {
            if current != Some(wanted) {
                if original.is_none() {
                    commands
                        .entity(entity)
                        .try_insert(PortalOriginalLayers(current.cloned().unwrap_or_default()));
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

/// The cell root `entity` hangs under - the first of its ancestors (itself included) that has a
/// role this frame - and whether a node on the way up, other than `entity` itself and the root, is
/// in `marked`. `None` for an entity no cell root owns.
fn owning_root(
    entity: Entity,
    roles: &HashMap<Entity, CellRole>,
    parents: &Query<&ChildOf>,
    marked: &HashSet<Entity>,
) -> Option<(Entity, bool)> {
    let mut covered = false;
    let mut cursor = Some(entity);
    while let Some(current) = cursor {
        if roles.contains_key(&current) {
            return Some((current, covered));
        }
        if current != entity && marked.contains(&current) {
            covered = true;
        }
        cursor = parents.get(current).ok().map(ChildOf::parent);
    }
    None
}

/// The changes since the last frame that can leave a node under a cell root on the wrong layers
/// without its root's role changing: nodes that gained (or lost) children - a glTF scene finishes
/// spawning frames after its reference - and nodes whose layers something else wrote or removed.
#[derive(bevy::ecs::system::SystemParam)]
struct IsolationChanges<'w, 's> {
    new_children: Query<'w, 's, Entity, Changed<Children>>,
    new_layers: Query<'w, 's, Entity, Changed<RenderLayers>>,
    removed_layers: RemovedComponents<'w, 's, RenderLayers>,
    parents: Query<'w, 's, &'static ChildOf>,
}

/// Moves every resident cell that is not part of the active space off the main camera's layers, and
/// puts the portal's destination on the portal camera's layer.
///
/// Every root's role is recomputed each frame (a few hundred roots in a village), but a root's
/// descendants are walked only when something can have put them on the wrong layers:
///
/// - the root is new or its role changed: its whole subtree is re-layered, in that one frame (a
///   shadow entity whose layers change at the wrong moment is dropped by Bevy - impl-191);
/// - a node under it gained children (a glTF scene finishing its spawn): the subtree under that
///   node is walked;
/// - a node's own layers were written or removed by something else: that node alone is checked.
///
/// While nothing moves, no subtree is walked at all.
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
    mut changes: IsolationChanges,
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
    let previous = std::mem::take(&mut state.roles);
    // The roots whose whole subtree is re-layered this frame.
    let mut walked = HashSet::new();
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
        if !previous.contains_key(&root) {
            state.churn.appeared(identity, root);
        }

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

        if previous.get(&root) != Some(&role) {
            let wanted = layers_for(role);
            let mut count = 0;
            for entity in children.iter_descendants(root) {
                relayer_node(&mut commands, entity, wanted.as_ref(), &nodes);
                count += 1;
            }
            walked.insert(root);
            state
                .churn
                .walked(identity, previous.get(&root).copied(), role, count);
        }
    }

    // Nodes that gained children under a root that was not walked: the subtree under each, once. A
    // node with an ancestor below the root in the same set is covered by that ancestor's walk - a
    // scene spawned in one frame marks every node of it that has children.
    let grown: HashSet<Entity> = changes.new_children.iter().collect();
    for &node in &grown {
        let Some((root, covered)) = owning_root(node, &state.roles, &changes.parents, &grown)
        else {
            continue;
        };
        if covered || walked.contains(&root) {
            continue;
        }
        let wanted = layers_for(state.roles[&root]);
        if node != root {
            relayer_node(&mut commands, node, wanted.as_ref(), &nodes);
        }
        for entity in children.iter_descendants(node) {
            relayer_node(&mut commands, entity, wanted.as_ref(), &nodes);
        }
    }

    // Nodes whose layers were written or removed since the last frame - this system's own writes
    // among them, which come back here once as a no-op - each checked on its own.
    let unmarked = HashSet::new();
    let relayered: Vec<Entity> = changes
        .new_layers
        .iter()
        .chain(changes.removed_layers.read())
        .collect();
    for node in relayered {
        let Some((root, _)) = owning_root(node, &state.roles, &changes.parents, &unmarked) else {
            continue;
        };
        if node == root || walked.contains(&root) {
            continue;
        }
        relayer_node(
            &mut commands,
            node,
            layers_for(state.roles[&root]).as_ref(),
            &nodes,
        );
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
    time: Option<Res<Time>>,
    mut shown: Local<Option<u32>>,
) {
    // This frame's portal, from scratch: whenever the camera below cannot be placed there is no
    // doorway rendering the destination, and the door that was open for it has to close again in
    // this frame. Clearing before the early returns is what makes that true of every one of them.
    state.open_door = None;
    state.destination_door = None;
    state.doorway = None;
    let (Some(active), Some(config), Some(origin), Some(streaming)) =
        (active, config, origin, streaming)
    else {
        return;
    };
    let Ok((main_transform, main_projection, main_camera)) = main.single() else {
        return;
    };
    let Ok((mut camera_transform, mut camera_projection, mut camera)) = portal_camera.single_mut()
    else {
        return;
    };
    let Ok((mut quad_transform, mut quad_visibility)) = quad.single_mut() else {
        return;
    };
    let camera_position = main_transform.translation;
    let camera_rotation = main_transform.rotation;
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

    // How far off the middle of the view each door's doorway is, or `None` when the doorway covers
    // none of the screen: the doorway measured is the quad the portal would draw in it, so the
    // door picked is the one whose window the player would see. A main camera with no size to
    // measure against sees every doorway, which is what the portal did before it asked.
    let window = main_camera.and_then(Camera::physical_viewport_size);
    let forward = camera_rotation * Vec3::NEG_Z;
    let aim = |entity: Entity| -> Option<f32> {
        let (_, global, local, door, _, instance_bounds, expected_bounds, anchor) =
            doors.get(entity).ok()?;
        let map = door_map(
            global.translation(),
            global.rotation(),
            local.scale,
            door,
            anchor,
            origin.0,
        );
        let quad = doorway_quad_transform(
            global.translation(),
            global.rotation(),
            map.frame,
            local.scale,
            instance_bounds,
            expected_bounds,
            anchor,
        );
        if let Some(window) = window {
            doorway_screen_rect(
                doorway_corners(&quad),
                main_transform,
                main_projection,
                window.as_vec2(),
            )?;
        }
        Some(
            (quad.translation - camera_position)
                .try_normalize()
                .map_or(0.0, |direction| forward.angle_between(direction)),
        )
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
        aim,
    );

    // Whether the door the portal last drew through still shows a way into the same destination:
    // a door of the active space, open or swinging, with its far side resident and not part of the
    // active space. What [`select_portal_door`] asks of it beyond that - being in front of it, in
    // reach, or picked over another door - is what the destination hold rides out.
    let now = time.map(|time| time.elapsed_secs_f64());
    let still_holds = |held: Entity| {
        let Ok((_, _, _, door, door_state, _, _, anchor)) = doors.get(held) else {
            return false;
        };
        let in_active_space = state
            .roles
            .get(&parents.get(held).map(ChildOf::parent).unwrap_or(held))
            == Some(&CellRole::Active);
        in_active_space
            && portal_shows_through(door_state)
            && destination_is_resident(&door.destination, anchor, &streaming)
            && !destination_keys(&door.destination, anchor)
                .iter()
                .any(|key| space.contains(*key))
    };

    let Some(target) = target else {
        if shown.take().is_some() {
            info!("portal: no door in view");
        }
        let held = state
            .hold
            .as_ref()
            .is_some_and(|hold| still_holds(hold.door));
        state.lose_destination(now, |_| held);
        *quad_visibility = Visibility::Hidden;
        camera.is_active = false;
        return;
    };

    let Ok((_, global, local, door, _, instance_bounds, expected_bounds, anchor)) =
        doors.get(target)
    else {
        let held = state
            .hold
            .as_ref()
            .is_some_and(|hold| still_holds(hold.door));
        state.lose_destination(now, |_| held);
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
    *quad_transform = doorway_quad_transform(
        door_position,
        door_rotation,
        frame,
        local.scale,
        instance_bounds,
        expected_bounds,
        anchor,
    );
    // Published for the systems that dress the doorway (`crate::portal_spill`): read only.
    state.doorway = Some(OpenDoorway {
        door: target,
        map,
        quad: *quad_transform,
    });

    // A doorway the main camera cannot see costs nothing. The portal camera renders a whole frame
    // of the destination, shadows and all, and a doorway behind the player or off a side of the
    // screen samples none of it, so the camera stops and the quad is hidden for this frame - the
    // no-portal path's two writes. The rest of the portal stays exactly as it is: the door keeps
    // its leaf hidden and its mirror (a scene that takes frames to spawn again), the destination
    // cells stay on the portal camera's layer and the destination door stays hidden, so the frame
    // the doorway comes back into view is the frame it is drawn in, with nothing to rebuild. None
    // of those can be seen from here: they are the doorway's, and the doorway is off screen.
    //
    // A doorway that is on screen is rendered at its own size: the portal camera projects only the
    // rectangle of the main view the doorway covers ([`doorway_render_rect`], [`doorway_sub_view`])
    // and [`resize_portal_target`] sizes the target to it, so a doorway a tenth of the screen wide
    // shades a tenth of the pixels. A main camera with no size to measure against keeps the whole
    // view, which is what the portal did before it asked.
    let on_screen = match window {
        None => {
            camera.sub_camera_view = None;
            state.render_rect = None;
            true
        }
        Some(window) => {
            let rect = doorway_screen_rect(
                doorway_corners(&quad_transform),
                main_transform,
                main_projection,
                window.as_vec2(),
            )
            .and_then(|rect| doorway_render_rect(rect, window));
            if let Some(rect) = rect {
                camera.sub_camera_view = Some(doorway_sub_view(rect, window));
                state.render_rect = Some(rect);
            }
            rect.is_some()
        }
    };
    camera.is_active = on_screen;
    *quad_visibility = if on_screen {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    state.draw_destination(target, destination_keys(&door.destination, anchor));
}

/// The sight lines [`skip_occluded_doorway`] tests a doorway with: a grid this many points wide
/// and this many high.
const OCCLUSION_GRID: usize = 4;

/// How far past each edge of the doorway the sight-line grid reaches, as a fraction of the
/// doorway's width or height. A doorway coming out from behind a wall edge shows the grid's outer
/// row or column first, so the test sees it coming before any pixel of the doorway itself is
/// uncovered.
const OCCLUSION_MARGIN: f32 = 0.25;

/// How far toward the eye, off the doorway's plane, the sight-line grid stands, in Creation units.
///
/// The points past the doorway's edges would otherwise lie on - or inside - the wall the door is
/// set into, and every sight line to them would stop on that wall: blocked, whether the doorway is
/// in plain view or not, and useless for seeing it come out. Held off the plane, a point is blocked
/// only by something standing between the eye and the doorway.
const OCCLUSION_STANDOFF: f32 = 32.0;

/// Within this distance of the doorway's plane the doorway is never skipped, in Creation units: the
/// grid would stand at or behind the eye, and a doorway this close is the one the player is about
/// to walk through.
const OCCLUSION_MIN_DISTANCE: f32 = 160.0;

/// How often a doorway in view is tested for being hidden: every this many frames. A hidden doorway
/// is tested every frame, because the frame it comes back is the frame it has to be drawn in.
const OCCLUSION_RETEST_FRAMES: u32 = 4;

/// How many tests in a row have to find every sight line blocked before the doorway stops being
/// drawn: about a fifth of a second at 60 frames a second. The hysteresis is on the way *out*
/// only; the way back is immediate.
const OCCLUSION_CONFIRM_TESTS: u32 = 3;

/// How far short of its grid point a sight line stops, in Creation units, so a surface the point
/// itself lies on does not count as standing in front of it.
const OCCLUSION_RAY_SLACK: f32 = 2.0;

/// Whether the doorway the portal draws through is hidden behind the view's own geometry, with the
/// hysteresis that keeps the decision from chattering ([`skip_occluded_doorway`]).
#[derive(Debug, Default)]
struct DoorwayOcclusion {
    /// The door the state below is about; a different door starts from "in view".
    door: Option<Entity>,
    /// Whether the doorway is skipped this frame.
    hidden: bool,
    /// Tests in a row that found every sight line blocked while the doorway was drawn.
    blocked_tests: u32,
    /// Frames left before a doorway in view is tested again.
    wait: u32,
    /// Tests run for the current door: whether its end is worth a log line.
    door_tests: u32,
    /// Frames the doorway view was skipped for, over the run: the log's count.
    skipped_frames: u64,
    /// Sight-line tests run, their total and their longest time: the log's cost of the test.
    tests: u64,
    test_time: Duration,
    longest_test: Duration,
}

impl DoorwayOcclusion {
    /// No doorway to test this frame: the next one starts from "in view".
    fn forget(&mut self) {
        self.door = None;
        self.door_tests = 0;
        self.hidden = false;
        self.blocked_tests = 0;
        self.wait = 0;
    }

    /// This frame's decision for the doorway of `door`: `true` to skip it. `blocked` runs the
    /// sight-line test, and is only called on the frames that need it: every frame while the
    /// doorway is hidden - it is drawn again in the first frame one sight line is clear - and every
    /// [`OCCLUSION_RETEST_FRAMES`] frames while it is drawn, where [`OCCLUSION_CONFIRM_TESTS`]
    /// blocked tests in a row hide it.
    fn update(&mut self, door: Entity, blocked: impl FnOnce() -> bool) -> bool {
        if self.door != Some(door) {
            self.forget();
            self.door = Some(door);
        }
        if self.hidden {
            self.door_tests += 1;
            if !blocked() {
                self.hidden = false;
                self.blocked_tests = 0;
                self.wait = OCCLUSION_RETEST_FRAMES - 1;
            }
            return self.hidden;
        }
        if self.wait > 0 {
            self.wait -= 1;
            return false;
        }
        self.wait = OCCLUSION_RETEST_FRAMES - 1;
        self.door_tests += 1;
        if blocked() {
            self.blocked_tests += 1;
            self.hidden = self.blocked_tests >= OCCLUSION_CONFIRM_TESTS;
        } else {
            self.blocked_tests = 0;
        }
        self.hidden
    }
}

/// The points the sight lines to a doorway end at: an [`OCCLUSION_GRID`] square grid over the
/// doorway quad `quad` ([`doorway_quad_transform`]) grown by [`OCCLUSION_MARGIN`] on every side,
/// held [`OCCLUSION_STANDOFF`] off the doorway's plane on the side of `eye`.
fn doorway_occlusion_samples(
    quad: &Transform,
    eye: Vec3,
) -> [Vec3; OCCLUSION_GRID * OCCLUSION_GRID] {
    let normal = quad.rotation * Vec3::Z;
    let toward_eye = if normal.dot(eye - quad.translation) >= 0.0 {
        normal
    } else {
        -normal
    };
    let reach = 0.5 + OCCLUSION_MARGIN;
    let step = 2.0 * reach / (OCCLUSION_GRID - 1) as f32;
    std::array::from_fn(|index| {
        let (column, row) = (index % OCCLUSION_GRID, index / OCCLUSION_GRID);
        let local = Vec3::new(
            -reach + step * column as f32,
            -reach + step * row as f32,
            0.0,
        );
        quad.transform_point(local) + toward_eye * OCCLUSION_STANDOFF
    })
}

/// A mesh that can hide a doorway: its model-to-world transform and its mesh.
struct Occluder<'a> {
    transform: Affine3A,
    aabb: Aabb3d,
    mesh: &'a Mesh,
}

/// The distance along `ray` to the nearest front face of `mesh` placed by `transform`, the way
/// Bevy's `MeshRayCast` measures it (`bevy_picking`'s `ray_intersection_over_mesh`, which is not
/// public): triangle lists only, back faces culled - a single-sided wall seen from behind is not
/// drawn, so it hides nothing.
fn ray_hits_mesh(mesh: &Mesh, transform: &Affine3A, ray: Ray3d) -> Option<f32> {
    if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
        return None;
    }
    let positions = mesh
        .try_attribute(Mesh::ATTRIBUTE_POSITION)
        .ok()?
        .as_float3()?;
    let hit = match mesh.try_indices().ok() {
        Some(Indices::U16(indices)) => ray_mesh_intersection(
            ray,
            transform,
            positions,
            None,
            Some(indices),
            None,
            Backfaces::Cull,
        ),
        Some(Indices::U32(indices)) => ray_mesh_intersection(
            ray,
            transform,
            positions,
            None,
            Some(indices),
            None,
            Backfaces::Cull,
        ),
        None => ray_mesh_intersection::<u32>(
            ray,
            transform,
            positions,
            None,
            None,
            None,
            Backfaces::Cull,
        ),
    };
    hit.map(|hit| hit.distance)
}

/// Whether every sight line from `eye` to the `samples` is blocked by one of the `occluders`
/// before it reaches its point.
///
/// Line by line, the lines nearest the middle of the grid first, each against the occluders in the
/// order given (nearest the eye first), its bounding box before its triangles. A blocked line stops
/// at the first occluder that blocks it, and the whole test stops at the first line nothing blocks:
/// a doorway behind a wall costs the triangles of the first mesh in the way of each line, and a
/// doorway in view the triangles along one or two open lines rather than all of them.
fn sight_lines_blocked(eye: Vec3, samples: &[Vec3], occluders: &[Occluder]) -> bool {
    if samples.is_empty() {
        return false;
    }
    let middle = samples.iter().copied().sum::<Vec3>() / samples.len() as f32;
    let mut order: Vec<Vec3> = samples.to_vec();
    order.sort_by(|a, b| {
        a.distance_squared(middle)
            .total_cmp(&b.distance_squared(middle))
    });
    order.into_iter().all(|sample| {
        let offset = sample - eye;
        let length = offset.length() - OCCLUSION_RAY_SLACK;
        let Ok(direction) = Dir3::new(offset) else {
            // A point at the eye itself: nothing can stand in front of it.
            return false;
        };
        let ray = Ray3d::new(eye, direction);
        length > 0.0
            && occluders.iter().any(|occluder| {
                ray_aabb_intersection_3d(ray, &occluder.aabb, &occluder.transform)
                    .is_some_and(|near| near < length)
                    && ray_hits_mesh(occluder.mesh, &occluder.transform, ray)
                        .is_some_and(|distance| distance < length)
            })
    })
}

/// The world-space box of a mesh's local `aabb` placed by `transform`.
fn world_box(aabb: &Aabb, transform: &Affine3A) -> (Vec3A, Vec3A) {
    let centre = transform.transform_point3a(aabb.center);
    let matrix = transform.matrix3;
    let half = Vec3A::new(
        matrix.row(0).abs().dot(aabb.half_extents),
        matrix.row(1).abs().dot(aabb.half_extents),
        matrix.row(2).abs().dot(aabb.half_extents),
    );
    (centre - half, centre + half)
}

/// Every mesh a doorway can be hidden behind, with what it takes to test it.
type OccluderQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static Mesh3d,
        &'static Aabb,
        &'static GlobalTransform,
        &'static InheritedVisibility,
        Option<&'static RenderLayers>,
        Option<&'static MeshMaterial3d<StandardMaterial>>,
        Has<MeshMaterial3d<TerrainMaterial>>,
    ),
>;

/// Forgets the doorway [`skip_occluded_doorway`] was testing, with one log line for a doorway that
/// was tested at all: the run's totals so far, which is the cost of the test and what it saved.
fn end_occlusion_session(occlusion: &mut DoorwayOcclusion) {
    if occlusion.door_tests > 0 {
        info!(
            door_tests = occlusion.door_tests,
            tests = occlusion.tests,
            mean_test_us = occlusion.test_time.as_micros() as u64 / occlusion.tests.max(1),
            longest_test_us = occlusion.longest_test.as_micros() as u64,
            skipped_frames = occlusion.skipped_frames,
            "portal: doorway occlusion test ends for this doorway"
        );
    }
    occlusion.forget();
}

/// Stops the doorway view while the doorway is on screen but hidden behind the view's own
/// geometry: a wall, a house corner, a hill.
///
/// [`update_portal`] has already skipped a doorway off the screen (impl-188); one on it can still
/// be covered, and then the portal camera renders the whole far side, and its four shadow cascades,
/// for a quad the depth test throws away. This runs right after it, in the same frame, and takes
/// the same no-portal path's two writes when the doorway is hidden: the portal camera stops and
/// the quad is hidden. Everything else stays as it is - the door's leaf, its mirror, the
/// destination cells resident on the portal camera's layer - so the frame a sight line clears is
/// the frame the doorway is drawn again, with nothing to rebuild.
///
/// **The test.** Sight lines from the eye to a grid of points over the doorway
/// ([`doorway_occlusion_samples`]), grown past its edges and held off its plane toward the eye, are
/// cast against the meshes the main camera draws in the active space ([`sight_lines_blocked`]).
/// The doorway is hidden only when every line is blocked. What counts as a blocker is kept to what
/// certainly hides what is behind it: opaque [`StandardMaterial`] meshes and terrain - no
/// alpha-tested foliage, no blended or additive effects, no water - drawn by the main camera and
/// not the door's own model or its mirror. Each mesh's world box is tested against the box around
/// the lines first, so a test reads the triangles of the few meshes actually in the way.
///
/// **The rate.** A doorway in view is tested every [`OCCLUSION_RETEST_FRAMES`] frames and hidden
/// after [`OCCLUSION_CONFIRM_TESTS`] blocked tests in a row; a hidden doorway is tested every frame
/// and drawn again in the first frame any line is clear ([`DoorwayOcclusion`]). The test uses this
/// frame's camera pose, so there is no frame of lag on the way back. Near the doorway
/// ([`OCCLUSION_MIN_DISTANCE`]) it is never skipped.
#[allow(clippy::too_many_arguments)]
fn skip_occluded_doorway(
    state: Res<PortalState>,
    main: MainCameraQuery,
    mut portal_camera: Query<&mut Camera, With<PortalCamera>>,
    mut quad: Query<(&Transform, &mut Visibility), With<PortalQuad>>,
    occluders: OccluderQuery,
    parents: Query<&ChildOf>,
    mirrors: Query<(), With<PortalDoorMirror>>,
    meshes: Res<Assets<Mesh>>,
    materials: Res<Assets<StandardMaterial>>,
    mut occlusion: Local<DoorwayOcclusion>,
) {
    let (
        Some(door),
        Ok((main_transform, ..)),
        Ok(mut camera),
        Ok((quad_transform, mut quad_visibility)),
    ) = (
        state.open_door,
        main.single(),
        portal_camera.single_mut(),
        quad.single_mut(),
    )
    else {
        end_occlusion_session(&mut occlusion);
        return;
    };
    let eye = main_transform.translation;
    let normal = quad_transform.rotation * Vec3::Z;
    if !camera.is_active
        || normal.dot(eye - quad_transform.translation).abs() < OCCLUSION_MIN_DISTANCE
    {
        // Off screen (the portal is already skipped) or close enough to walk through.
        end_occlusion_session(&mut occlusion);
        return;
    }
    if occlusion.door.is_some_and(|tested| tested != door) {
        end_occlusion_session(&mut occlusion);
    }
    let samples = doorway_occlusion_samples(quad_transform, eye);
    let was_hidden = occlusion.hidden;
    let mut tested = false;
    let started = Instant::now();
    let hidden = occlusion.update(door, || {
        tested = true;
        // The box around every sight line: a mesh outside it cannot be in the way of any.
        let (low, high) = samples.iter().fold((eye, eye), |(low, high), &sample| {
            (low.min(sample), high.max(sample))
        });
        let (low, high) = (Vec3A::from(low), Vec3A::from(high));
        // Whether anything up the parent chain rules the mesh out: the door itself (its leaf, its
        // frame), the doorway's mirror, or a cell outside the active space.
        let excluded = |entity: Entity| {
            let mut cursor = Some(entity);
            while let Some(current) = cursor {
                if current == door || mirrors.contains(current) {
                    return true;
                }
                if let Some(role) = state.roles.get(&current) {
                    return *role != CellRole::Active;
                }
                cursor = parents.get(current).ok().map(ChildOf::parent);
            }
            false
        };
        let main_layer = RenderLayers::layer(0);
        let mut candidates: Vec<(f32, Occluder)> = occluders
            .iter()
            .filter_map(
                |(entity, mesh, aabb, transform, visibility, layers, standard, terrain)| {
                    if !visibility.get() || !layers.unwrap_or(&main_layer).intersects(&main_layer) {
                        return None;
                    }
                    let transform = transform.affine();
                    let (box_low, box_high) = world_box(aabb, &transform);
                    if box_low.cmpgt(high).any() || box_high.cmplt(low).any() {
                        return None;
                    }
                    let opaque = terrain
                        || standard
                            .and_then(|material| materials.get(&material.0))
                            .is_some_and(|material| material.alpha_mode == AlphaMode::Opaque);
                    // The parent walk last: it is the dearest test, and only what is in the way and
                    // opaque gets that far.
                    if !opaque || excluded(entity) {
                        return None;
                    }
                    let mesh = meshes.get(&mesh.0)?;
                    Some((
                        (box_low + box_high).distance_squared(Vec3A::from(eye) * 2.0),
                        Occluder {
                            transform,
                            aabb: Aabb3d::new(aabb.center, aabb.half_extents),
                            mesh,
                        },
                    ))
                },
            )
            .collect();
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
        let occluders: Vec<Occluder> = candidates
            .into_iter()
            .map(|(_, occluder)| occluder)
            .collect();
        sight_lines_blocked(eye, &samples, &occluders)
    });
    if tested {
        let elapsed = started.elapsed();
        occlusion.tests += 1;
        occlusion.test_time += elapsed;
        occlusion.longest_test = occlusion.longest_test.max(elapsed);
    }
    if hidden {
        occlusion.skipped_frames += 1;
        camera.is_active = false;
        *quad_visibility = Visibility::Hidden;
    }
    if hidden != was_hidden {
        info!(
            skipped_frames = occlusion.skipped_frames,
            tests = occlusion.tests,
            mean_test_us = occlusion.test_time.as_micros() as u64 / occlusion.tests.max(1),
            longest_test_us = occlusion.longest_test.as_micros() as u64,
            "portal: doorway {}",
            if hidden {
                "hidden behind the view, not drawn"
            } else {
                "back in view"
            }
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The depth composite (impl-211's spike, made the default by impl-227)
// ---------------------------------------------------------------------------------------------

/// The doorway composited by depth instead of by the quad's rectangle: the default doorway since
/// impl-227 (impl-211 was the spike), with `--portal-depth-composite=off` as the escape hatch that
/// brings back the plain quad for comparison.
///
/// The plain quad writes its own plane's depth, so everything of the source space behind that
/// plane is painted over by the rectangle - including the parts of the doorway that stand behind
/// it (the jamb's inner faces, the underside of a lintel), which is how the destination's beams
/// come to cross the exterior frame at grazing angles. Here the quad writes the depth of what the
/// portal camera actually drew at each pixel instead, so the source geometry that is nearer
/// occludes it at whatever shape the opening really has:
///
/// * the portal camera's depth buffer is copied, after its main pass, into an image the quad's
///   material binds ([`copy_portal_depth`], a system in the render world's `Core3d` schedule);
/// * the quad's shader (`portal.wgsl` under `PORTAL_DEPTH_COMPOSITE`) turns that depth back into a
///   view-space point with the inverse of the portal camera's own clip matrix (oblique near plane
///   and doorway sub-view included), and projects it with the main camera's;
/// * the written depth is never in front of the doorway plane, and never further behind it than
///   the door's **slab** ([`doorway_slab`]) - a depth measured along the plane's normal, so the
///   same slab holds at every viewing angle;
/// * the quad is left out of the main camera's depth prepass (it writes no depth there), since the
///   prepass would otherwise store the plane's depth and the main pass's `GreaterEqual` test would
///   then reject every fragment the composite pushed behind it.
///
/// # The filler off, then a slab measured per doorway
///
/// A load door's model carries the plug the game hides the void behind a closed door with -
/// `FarmhouseLDoor01`'s `DoorBlack` (an open box of near-black boards: its back 22.5 units behind
/// the doorway plane, its side walls the depth of the box), the Dwemer doors' `Plane02` and
/// `Plane04`, the small Nordic door's `Object05:7`, Whiterun's `DoorToBlack` and Riften's
/// `RiftenDoorBG`. Behind the plane, a written depth that reaches the plug lets the plug win: the
/// doorway goes black at a large slab (impl-211: 24 and 64 did, on every Riverwood house), and at
/// any slab its side and top walls draw as black strips down the jambs and across the lintel
/// (Honningbrew's `WRShackDoor01`, the dark band under Sven's lintel).
///
/// So, two steps, both general rules rather than a list of models:
///
/// 1. **The filler is taken off the main camera** while its doorway is drawn
///    ([`hide_doorway_filler`]): every static mesh of the door's own model - not under its
///    [`DoorLeaf`] - whose box centre stands behind the doorway plane ([`is_doorway_filler`]). It is
///    moved to a layer no camera draws, not hidden: the doorway's floor probe ray-casts by
///    visibility and can land on it.
/// 2. **The slab is measured per doorway** ([`measure_doorway_slab`]): rays from the doorway plane
///    straight back through the opening (a grid over its middle and a row along its sill) against
///    everything the main view still draws of the source space, both faces; the slab is the
///    nearest hit less [`COMPOSITE_SLAB_MARGIN`], at most [`COMPOSITE_SLAB_CAP`]. The filler is not
///    the only thing behind a doorway: the rays behind Riverwood's `FarmhouseLDoor01` doorways
///    meet the house's own geometry about 29 units back (slab 27), and the hillside behind
///    Chillfurrow Farm's door rises above the room's floor (slab 11, found by the sill row).
///
/// The written depth then carries the destination's own depth pulled [`COMPOSITE_TIE_BIAS`]
/// nearer, so the source's doorstep, flush with the room's floor stood on it, does not win the
/// tie.
pub(crate) mod depth_composite {
    use super::{
        PortalCamera, PortalQuad, PortalState, PortalTexture, doorway_rect_uniform,
        fit_portal_view, setup_portal_quad, world_box,
    };
    use crate::doors::DoorLeaf;
    use bevy::{
        asset::{AssetId, RenderAssetUsages},
        camera::{
            CameraUpdateSystems,
            primitives::Aabb,
            visibility::{RenderLayers, VisibilitySystems},
        },
        core_pipeline::{Core3d, Core3dSystems, core_3d::main_transparent_pass_3d},
        math::{Affine3A, Vec3A},
        mesh::MeshVertexBufferLayoutRef,
        mesh::{Indices, PrimitiveTopology},
        pbr::{
            ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline,
            MaterialPlugin,
        },
        picking::mesh_picking::ray_cast::{Backfaces, ray_mesh_intersection},
        prelude::*,
        render::{
            Extract, ExtractSchedule, RenderApp,
            render_asset::RenderAssets,
            render_resource::{
                AsBindGroup, Extent3d, RenderPipelineDescriptor, SpecializedMeshPipelineError,
                TextureDimension, TextureFormat, TextureUsages,
            },
            renderer::{RenderContext, ViewQuery},
            sync_world::RenderEntity,
            texture::GpuImage,
            view::ViewDepthTexture,
        },
        shader::ShaderRef,
    };

    /// The slab of a doorway with nothing behind its opening, and the most any doorway gets, in
    /// Creation units along the doorway plane's normal: how far behind the plane the source space
    /// may still occlude the destination. Anything of the source further behind than this - the
    /// far side of the house's shell, a tree behind it - is behind the written depth and stays
    /// hidden, as it is under the plain quad.
    ///
    /// A doorway's reveal is as deep as the wall it is set in. The plugs of the load-door models
    /// the game uses most stand 14 to 28 units behind their doorway planes (the reveal ends at the
    /// plug), so 32 lets a doorway occlude by about a wall's depth and no more.
    pub(crate) const COMPOSITE_SLAB_CAP: f32 = 32.0;

    /// How far in front of the door model's own backing geometry the slab stops, in units: the
    /// card is flat, and a written depth that ends a little short of it never ties with it.
    pub(crate) const COMPOSITE_SLAB_MARGIN: f32 = 2.0;

    /// How much nearer, in units along the view axis, the composite writes the destination's
    /// depth than it is: a source surface within this of a destination surface loses to it.
    ///
    /// A doorstep is where the two spaces meet flush - the map stands the destination's floor on
    /// the source threshold, and the step's own top is that threshold - so behind the doorway
    /// plane the two are one surface drawn twice, and the bias gives the tie to the room. Checked
    /// at Gerdur's House (the user's capture of 2026-09-25, door open): with it the sill meets the
    /// room's floor with no band of porch boards over it (`local/t227/gerdur-threshold-on.png`).
    pub(crate) const COMPOSITE_TIE_BIAS: f32 = 4.0;

    /// The probe rays per side of the doorway ([`slab_probe_rays`]): a 4x4 grid.
    const SLAB_PROBE_GRID: usize = 4;

    /// The part of the doorway's width and height the probe grid spans, around the middle: the
    /// outer 15% at each edge is left out, so the rays go through the opening and not down the
    /// jambs' own faces, which are the reveal the slab is there to keep.
    const SLAB_PROBE_SPAN: f32 = 0.7;

    /// How high above the doorway's bottom edge the sill row of probe rays starts, as a part of
    /// the doorway's height ([`slab_probe_rays`]).
    ///
    /// The quad's bottom edge is the threshold the destination floor is stood on, so source ground
    /// that rises behind the doorway plane - the terrain inside a house's footprint on a slope,
    /// which the house's shell hides in the game - stands *above* the destination's floor there.
    /// Within the slab it would occlude the floor of the room: Chillfurrow Farm's doorway drew a
    /// band of the hillside across its threshold with a slab of 32. The sill row finds it.
    const SILL_PROBE_HEIGHT: f32 = 0.02;

    /// The probe rays of a doorway quad: from points of its plane on a grid over the middle of the
    /// opening and a row just above its sill, along the plane's normal away from the side the quad
    /// faces (the player's).
    pub(crate) fn slab_probe_rays(quad: &Transform) -> Vec<Ray3d> {
        let Ok(behind) = Dir3::new(quad.rotation * Vec3::NEG_Z) else {
            return Vec::new();
        };
        let step = SLAB_PROBE_SPAN / (SLAB_PROBE_GRID - 1) as f32;
        let first = -SLAB_PROBE_SPAN * 0.5;
        let mut rays = Vec::with_capacity(SLAB_PROBE_GRID * (SLAB_PROBE_GRID + 1));
        for row in 0..SLAB_PROBE_GRID {
            for column in 0..SLAB_PROBE_GRID {
                let local = Vec3::new(first + step * column as f32, first + step * row as f32, 0.0);
                rays.push(Ray3d::new(quad.transform_point(local), behind));
            }
        }
        // And a row along the sill, for ground that rises behind the doorway.
        for column in 0..SLAB_PROBE_GRID {
            let local = Vec3::new(first + step * column as f32, SILL_PROBE_HEIGHT - 0.5, 0.0);
            rays.push(Ray3d::new(quad.transform_point(local), behind));
        }
        rays
    }

    /// The slab from the probe rays' hits ([`slab_probe_rays`], a distance behind the plane each):
    /// the nearest hit less [`COMPOSITE_SLAB_MARGIN`], never below zero (a card in the plane
    /// itself: the plain quad's depth) and never above [`COMPOSITE_SLAB_CAP`] (nothing hit at all).
    pub(crate) fn doorway_slab(hits: impl IntoIterator<Item = f32>) -> f32 {
        hits.into_iter()
            .filter(|hit| hit.is_finite())
            .map(|hit| hit - COMPOSITE_SLAB_MARGIN)
            .fold(COMPOSITE_SLAB_CAP, f32::min)
            .max(0.0)
    }

    /// How many frames a doorway keeps its measured slab before [`measure_doorway_slab`] takes it
    /// again: half a second at the demo's frame rate.
    const SLAB_REMEASURE_FRAMES: u32 = 30;

    /// The slab of the doorway the portal draws through ([`doorway_slab`]), what it was measured
    /// on (the door and its doorway quad) and how many frames ago.
    #[derive(Resource, Debug)]
    struct DoorwaySlab {
        measured: Option<(Entity, Transform)>,
        frames: u32,
        slab: f32,
    }

    impl Default for DoorwaySlab {
        fn default() -> Self {
            Self {
                measured: None,
                frames: 0,
                slab: COMPOSITE_SLAB_CAP,
            }
        }
    }

    /// How far behind the doorway plane a mesh's box centre has to stand, in units, for
    /// [`is_doorway_filler`] to call it the door model's filler.
    const FILLER_BEHIND: f32 = 0.5;

    /// Whether a static mesh of a door's own model, with its world box centred on `centre`, is the
    /// model's **filler** - the plug the game hides the void behind a closed load door with - for
    /// the doorway quad `quad`: its centre stands behind the doorway plane, on the side away from
    /// the player.
    pub(crate) fn is_doorway_filler(centre: Vec3, quad: &Transform) -> bool {
        let behind = quad.rotation * Vec3::NEG_Z;
        (centre - quad.translation).dot(behind) > FILLER_BEHIND
    }

    /// The layer [`hide_doorway_filler`] moves the filler to: one no camera renders. A layer rather
    /// than [`Visibility::Hidden`], because the filler still has to be *there* for everything that
    /// ray-casts the world by visibility: `DoorBlack`'s floor is the source floor
    /// [`super::anchor_on_measured_floors`] stands the doorway on (hidden, the probe fell 22 units
    /// through to the ground below Sven's House and the doorway image slid down with it).
    const FILLER_LAYER: usize = 31;

    /// The door-model meshes [`hide_doorway_filler`] moved off the cameras, with the layers each
    /// had (`None`: no [`RenderLayers`], which is layer 0).
    #[derive(Resource, Debug, Default)]
    struct HiddenFiller(Vec<(Entity, Option<RenderLayers>)>);

    /// Takes the filler ([`is_doorway_filler`]) of the door the portal draws through off the main
    /// camera, for as long as it does, and gives every other mesh it moved its layers back.
    ///
    /// Only a door with a leaf ([`DoorLeaf`]) is looked at: its frame stays drawn while it is
    /// open, and the leaf is `crate::door_animation`'s. A door with no animation of its own has
    /// its whole model hidden already ([`super::show_load_door_leaves`]).
    ///
    /// A cell that stops being active has its layers rewritten by [`super::isolate_cells`] every
    /// frame, so a filler mesh given back while its cell is a destination is put on the
    /// destination's layer again the next frame.
    #[allow(clippy::type_complexity)]
    fn hide_doorway_filler(
        mut commands: Commands,
        state: Option<Res<PortalState>>,
        mut hidden: ResMut<HiddenFiller>,
        children: Query<&Children>,
        leaves: Query<(), With<DoorLeaf>>,
        meshes: Query<(&GlobalTransform, &Aabb, Option<&RenderLayers>), With<Mesh3d>>,
        names: Query<&Name>,
    ) {
        let mut filler = Vec::new();
        if let Some(doorway) = state.and_then(|state| state.open_doorway()) {
            let mut statics = Vec::new();
            let mut has_leaf = false;
            let mut stack = vec![doorway.door];
            while let Some(entity) = stack.pop() {
                if leaves.contains(entity) {
                    has_leaf = true;
                    continue;
                }
                if meshes.contains(entity) {
                    statics.push(entity);
                }
                if let Ok(children) = children.get(entity) {
                    stack.extend(children.iter());
                }
            }
            if has_leaf {
                filler.extend(statics.into_iter().filter(|entity| {
                    meshes.get(*entity).is_ok_and(|(transform, aabb, _)| {
                        is_doorway_filler(
                            transform.transform_point(Vec3::from(aabb.center)),
                            &doorway.quad,
                        )
                    })
                }));
            }
        }
        // Give back what is no longer filler of the doorway drawn. A mesh despawned since (its
        // cell unloaded) has nothing to give back to.
        hidden.0.retain(|(entity, original)| {
            if filler.contains(entity) {
                return true;
            }
            if meshes.contains(*entity) {
                match original {
                    Some(layers) => {
                        commands.entity(*entity).try_insert(layers.clone());
                    }
                    None => {
                        commands.entity(*entity).try_remove::<RenderLayers>();
                    }
                }
            }
            false
        });
        for entity in filler {
            if hidden.0.iter().any(|(hidden, _)| *hidden == entity) {
                continue;
            }
            if let Ok((_, _, layers)) = meshes.get(entity) {
                hidden.0.push((entity, layers.cloned()));
                commands
                    .entity(entity)
                    .try_insert(RenderLayers::layer(FILLER_LAYER));
                debug!(
                    ?entity,
                    name = names.get(entity).map(Name::as_str).unwrap_or(""),
                    "portal: doorway filler taken off the cameras"
                );
            }
        }
    }

    /// The layers the main camera draws (`app::setup_world`): what the slab probe counts as drawn.
    const MAIN_VIEW_LAYERS: [usize; 2] = [0, 1];

    /// The distance along `ray` to the nearest face of `mesh` placed by `transform`, **either**
    /// side: a surface the main view draws double-sided stops the written depth whichever way its
    /// triangles wind, and a probe that culled back faces would measure straight past it.
    fn ray_hits_either_face(mesh: &Mesh, transform: &Affine3A, ray: Ray3d) -> Option<f32> {
        if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
            return None;
        }
        let positions = mesh
            .try_attribute(Mesh::ATTRIBUTE_POSITION)
            .ok()?
            .as_float3()?;
        let hit = match mesh.try_indices().ok() {
            Some(Indices::U16(indices)) => ray_mesh_intersection(
                ray,
                transform,
                positions,
                None,
                Some(indices),
                None,
                Backfaces::Include,
            ),
            Some(Indices::U32(indices)) => ray_mesh_intersection(
                ray,
                transform,
                positions,
                None,
                Some(indices),
                None,
                Backfaces::Include,
            ),
            None => ray_mesh_intersection::<u32>(
                ray,
                transform,
                positions,
                None,
                None,
                None,
                Backfaces::Include,
            ),
        };
        hit.map(|hit| hit.distance)
    }

    /// Measures [`DoorwaySlab`] for the doorway the portal draws through this frame: the probe
    /// rays ([`slab_probe_rays`]) against everything the main camera draws of the active space -
    /// the house the door is set in as well as the door's own model - except the doorway quad and
    /// anything under a [`DoorLeaf`] (a leaf swings; the doorway's mirror is the destination
    /// image's), both faces of every triangle ([`ray_hits_either_face`]). The door model's filler
    /// is off the main camera by then ([`hide_doorway_filler`]), so what the rays find is the
    /// first thing of the source space that really stands behind the opening.
    ///
    /// Taken again when the door or its quad changes, and every [`SLAB_REMEASURE_FRAMES`] frames
    /// while it stays - the house around a door is a glTF scene that can arrive after the door.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn measure_doorway_slab(
        state: Option<Res<PortalState>>,
        mut slab: ResMut<DoorwaySlab>,
        meshes: Query<(
            Entity,
            &Mesh3d,
            &Aabb,
            &GlobalTransform,
            &InheritedVisibility,
            Option<&RenderLayers>,
        )>,
        mesh_assets: Res<Assets<Mesh>>,
        parents: Query<&ChildOf>,
        leaves: Query<(), With<DoorLeaf>>,
        quads: Query<(), With<PortalQuad>>,
    ) {
        let Some(state) = state else {
            return;
        };
        let Some(doorway) = state.open_doorway() else {
            slab.measured = None;
            return;
        };
        slab.frames += 1;
        if slab.measured == Some((doorway.door, doorway.quad))
            && slab.frames < SLAB_REMEASURE_FRAMES
        {
            return;
        }
        let rays = slab_probe_rays(&doorway.quad);
        // The box the rays sweep, out to the cap: only a mesh whose box meets it can stop one.
        let reach = COMPOSITE_SLAB_CAP + COMPOSITE_SLAB_MARGIN;
        let (mut low, mut high) = (Vec3A::splat(f32::INFINITY), Vec3A::splat(f32::NEG_INFINITY));
        for ray in &rays {
            for point in [ray.origin, ray.get_point(reach)] {
                low = low.min(point.into());
                high = high.max(point.into());
            }
        }
        let main_layers = RenderLayers::from_layers(&MAIN_VIEW_LAYERS);
        let under_leaf = |entity: Entity| {
            let mut cursor = Some(entity);
            while let Some(current) = cursor {
                if leaves.contains(current) {
                    return true;
                }
                cursor = parents.get(current).ok().map(ChildOf::parent);
            }
            false
        };
        let mut blockers = Vec::new();
        for (entity, mesh, aabb, transform, visibility, layers) in &meshes {
            if !visibility.get()
                || quads.contains(entity)
                || layers.is_some_and(|layers| !layers.intersects(&main_layers))
            {
                continue;
            }
            let transform = transform.affine();
            let (min, max) = world_box(aabb, &transform);
            if min.cmpgt(high).any() || max.cmplt(low).any() {
                continue;
            }
            if under_leaf(entity) || !state.is_in_active_space(entity, &parents) {
                continue;
            }
            if let Some(mesh) = mesh_assets.get(&mesh.0) {
                blockers.push((mesh, transform));
            }
        }
        let hits = rays
            .iter()
            .filter_map(|ray| {
                blockers
                    .iter()
                    .filter_map(|(mesh, transform)| ray_hits_either_face(mesh, transform, *ray))
                    .reduce(f32::min)
            })
            .collect::<Vec<_>>();
        let measured = doorway_slab(hits.iter().copied());
        if slab.measured.map(|(door, _)| door) != Some(doorway.door)
            || (measured - slab.slab).abs() > 0.5
        {
            info!(
                door = ?doorway.door,
                hits = hits.len(),
                slab = measured,
                "portal: doorway depth slab measured"
            );
        }
        slab.measured = Some((doorway.door, doorway.quad));
        slab.frames = 0;
        slab.slab = measured;
    }

    /// The format the portal camera's depth is copied into: Bevy's own 3D depth format, which a
    /// texture-to-texture copy requires the two sides to share.
    const PORTAL_DEPTH_FORMAT: TextureFormat = TextureFormat::Depth32Float;

    /// The copy of the portal camera's depth the quad samples.
    #[derive(Resource)]
    struct PortalDepthTexture(Handle<Image>);

    /// On the portal camera: the image its depth is copied into after its main pass.
    #[derive(Component, Clone)]
    struct PortalDepthCopy(Handle<Image>);

    /// [`PortalDepthCopy`] in the render world.
    #[derive(Component)]
    struct PortalDepthCopyTarget(AssetId<Image>);

    /// The doorway quad's material under the composite: the default's three bindings plus the
    /// portal camera's depth, the inverse of its clip matrix, and the slab.
    #[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
    pub struct PortalDepthExtension {
        #[texture(100)]
        #[sampler(101)]
        portal_texture: Option<Handle<Image>>,
        #[uniform(102)]
        doorway_rect: Vec4,
        #[texture(103, sample_type = "depth")]
        portal_depth: Option<Handle<Image>>,
        #[uniform(104)]
        portal_view_from_clip: Mat4,
        /// `x`: the doorway's slab ([`DoorwaySlab`]), in units along the doorway plane's normal;
        /// `y`: [`COMPOSITE_TIE_BIAS`].
        #[uniform(105)]
        composite: Vec4,
    }

    impl MaterialExtension for PortalDepthExtension {
        fn fragment_shader() -> ShaderRef {
            "embedded://engine/shaders/portal.wgsl".into()
        }

        fn specialize(
            _pipeline: &MaterialExtensionPipeline,
            descriptor: &mut RenderPipelineDescriptor,
            _layout: &MeshVertexBufferLayoutRef,
            _key: MaterialExtensionKey<Self>,
        ) -> Result<(), SpecializedMeshPipelineError> {
            // The depth prepass (and the shadow pass, which is the same pipeline; Bevy labels it
            // `prepass_pipeline`, and `StandardMaterial` relabels it `pbr_prepass_pipeline`) writes no depth
            // for the quad: its plane's depth there would make the main pass reject every
            // composite fragment that lies behind it.
            if descriptor
                .label
                .as_deref()
                .is_some_and(|label| label.ends_with("prepass_pipeline"))
            {
                if let Some(depth) = descriptor.depth_stencil.as_mut() {
                    depth.depth_write_enabled = Some(false);
                }
            } else if let Some(fragment) = descriptor.fragment.as_mut() {
                fragment.shader_defs.push("PORTAL_DEPTH_COMPOSITE".into());
            }
            Ok(())
        }
    }

    type PortalDepthMaterial = ExtendedMaterial<StandardMaterial, PortalDepthExtension>;

    /// Registers the composite. Added by [`super::PortalPlugin`] to every run with a doorway,
    /// unless `--portal-depth-composite=off` asked for the plain quad.
    pub(crate) struct DepthCompositePlugin;

    impl Plugin for DepthCompositePlugin {
        fn build(&self, app: &mut App) {
            app.add_plugins(MaterialPlugin::<PortalDepthMaterial>::default())
                .init_resource::<DoorwaySlab>()
                .init_resource::<HiddenFiller>()
                .add_systems(Startup, setup_depth_composite.after(setup_portal_quad))
                .add_systems(
                    PostUpdate,
                    (
                        hide_doorway_filler
                            .after(TransformSystems::Propagate)
                            .before(VisibilitySystems::VisibilityPropagate),
                        // The door model's meshes where this frame draws them.
                        measure_doorway_slab
                            .after(TransformSystems::Propagate)
                            .after(VisibilitySystems::VisibilityPropagate),
                        sync_depth_composite
                            .after(CameraUpdateSystems)
                            .after(fit_portal_view),
                    )
                        .chain(),
                );
            if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
                render_app
                    .add_systems(ExtractSchedule, extract_portal_depth_copy)
                    .add_systems(
                        Core3d,
                        copy_portal_depth
                            .after(main_transparent_pass_3d)
                            .in_set(Core3dSystems::MainPass),
                    );
            }
        }
    }

    /// A depth image the portal camera's depth can be copied into and the quad can sample.
    fn portal_depth_image(size: UVec2) -> Image {
        let mut image = Image::new_uninit(
            Extent3d {
                width: size.x,
                height: size.y,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            PORTAL_DEPTH_FORMAT,
            RenderAssetUsages::RENDER_WORLD,
        );
        image.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST;
        image
    }

    /// Swaps the quad's default material for the composite's, and points the portal camera's
    /// depth copy at a first image.
    fn setup_depth_composite(
        mut commands: Commands,
        texture: Res<PortalTexture>,
        mut images: ResMut<Assets<Image>>,
        mut materials: ResMut<Assets<PortalDepthMaterial>>,
        quad: Query<(Entity, &MeshMaterial3d<super::PortalMaterial>), With<PortalQuad>>,
        base_materials: Res<Assets<super::PortalMaterial>>,
        camera: Query<Entity, With<PortalCamera>>,
    ) {
        let size = images
            .get(&texture.0)
            .map(Image::size)
            .unwrap_or(super::PORTAL_TEXTURE_FALLBACK_SIZE);
        let depth = images.add(portal_depth_image(size));
        commands.insert_resource(PortalDepthTexture(depth.clone()));
        if let Ok(camera) = camera.single() {
            commands
                .entity(camera)
                .insert(PortalDepthCopy(depth.clone()));
        }
        let Ok((entity, handle)) = quad.single() else {
            return;
        };
        let base = base_materials
            .get(handle)
            .map(|material| material.base.clone())
            .unwrap_or_default();
        let material = materials.add(PortalDepthMaterial {
            base,
            extension: PortalDepthExtension {
                portal_texture: Some(texture.0.clone()),
                doorway_rect: doorway_rect_uniform(None),
                portal_depth: Some(depth),
                portal_view_from_clip: Mat4::IDENTITY,
                composite: Vec4::new(COMPOSITE_SLAB_CAP, COMPOSITE_TIE_BIAS, 0.0, 0.0),
            },
        });
        commands
            .entity(entity)
            .remove::<MeshMaterial3d<super::PortalMaterial>>()
            .insert(MeshMaterial3d(material));
    }

    /// Keeps the composite's material and depth image in step with the portal camera, every
    /// frame, after the camera's matrices are final: the depth image follows the colour target's
    /// size (a depth copy must cover the whole texture), and the material carries the target, the
    /// doorway rectangle and the inverse of the clip matrix the portal camera renders with this
    /// frame, and the doorway's slab ([`measure_doorway_slab`]). The material is written only when
    /// one of them changed.
    #[allow(clippy::too_many_arguments)]
    fn sync_depth_composite(
        state: Option<Res<PortalState>>,
        texture: Res<PortalTexture>,
        depth: Option<ResMut<PortalDepthTexture>>,
        mut images: ResMut<Assets<Image>>,
        mut materials: ResMut<Assets<PortalDepthMaterial>>,
        mut camera: Query<(&Camera, &mut PortalDepthCopy), With<PortalCamera>>,
        quad: Query<&MeshMaterial3d<PortalDepthMaterial>, With<PortalQuad>>,
        slab: Res<DoorwaySlab>,
    ) {
        let Some(mut depth) = depth else {
            return;
        };
        let Some(size) = images.get(&texture.0).map(Image::size) else {
            return;
        };
        if images.get(&depth.0).map(Image::size) != Some(size) {
            depth.0 = images.add(portal_depth_image(size));
        }
        let Ok((camera, mut copy)) = camera.single_mut() else {
            return;
        };
        if copy.0 != depth.0 {
            copy.0 = depth.0.clone();
        }
        let view_from_clip = camera.clip_from_view().inverse();
        let doorway_rect = doorway_rect_uniform(state.and_then(|state| state.render_rect));
        let composite = Vec4::new(slab.slab, COMPOSITE_TIE_BIAS, 0.0, 0.0);
        let Ok(handle) = quad.single() else {
            return;
        };
        let stale = materials.get(handle).is_some_and(|material| {
            let extension = &material.extension;
            extension.portal_texture.as_ref() != Some(&texture.0)
                || extension.portal_depth.as_ref() != Some(&depth.0)
                || extension.doorway_rect != doorway_rect
                || extension.portal_view_from_clip != view_from_clip
                || extension.composite != composite
        });
        if stale && let Some(mut material) = materials.get_mut(handle) {
            material.extension.portal_texture = Some(texture.0.clone());
            material.extension.portal_depth = Some(depth.0.clone());
            material.extension.doorway_rect = doorway_rect;
            material.extension.portal_view_from_clip = view_from_clip;
            material.extension.composite = composite;
        }
    }

    fn extract_portal_depth_copy(
        mut commands: Commands,
        cameras: Extract<Query<(RenderEntity, &PortalDepthCopy)>>,
    ) {
        for (entity, copy) in &cameras {
            commands
                .entity(entity)
                .insert(PortalDepthCopyTarget(copy.0.id()));
        }
    }

    /// Copies the portal camera's depth buffer, after its main pass, into the image the quad
    /// samples. Runs in every camera's `Core3d` schedule and does nothing for a view without
    /// [`PortalDepthCopyTarget`] - every view but the portal camera's. A frame whose depth image
    /// has not caught up with a resized target is skipped: the copy must cover the whole texture.
    fn copy_portal_depth(
        view: ViewQuery<(&ViewDepthTexture, &PortalDepthCopyTarget)>,
        images: Res<RenderAssets<GpuImage>>,
        mut ctx: RenderContext,
    ) {
        let (depth, target) = view.into_inner();
        let Some(image) = images.get(target.0) else {
            return;
        };
        let size = depth.texture.size();
        if image.texture.size() != size {
            return;
        }
        ctx.command_encoder().copy_texture_to_texture(
            depth.texture.as_image_copy(),
            image.texture.as_image_copy(),
            size,
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::f32::consts::{FRAC_PI_2, PI};

        /// `FarmhouseLDoor01`'s doorway as a quad: 96 by 176, facing the player on `+Z`.
        fn farmhouse_quad() -> Transform {
            Transform::from_xyz(0.0, 88.0, 0.0).with_scale(Vec3::new(96.0, 176.0, 1.0))
        }

        #[test]
        fn the_slab_stops_short_of_the_nearest_card_and_is_capped_without_one() {
            assert_eq!(
                doorway_slab([]),
                COMPOSITE_SLAB_CAP,
                "nothing behind the opening"
            );
            assert_eq!(doorway_slab([22.5, 30.0]), 22.5 - COMPOSITE_SLAB_MARGIN);
            assert_eq!(
                doorway_slab([100.0]),
                COMPOSITE_SLAB_CAP,
                "a far hit is capped"
            );
            assert_eq!(
                doorway_slab([1.0]),
                0.0,
                "a card in the plane: the plain quad"
            );
            assert_eq!(doorway_slab([f32::NAN, 14.0]), 14.0 - COMPOSITE_SLAB_MARGIN);
        }

        #[test]
        fn the_probe_rays_leave_the_plane_through_the_middle_of_the_opening_away_from_the_player() {
            let quad = farmhouse_quad();
            let rays = slab_probe_rays(&quad);
            assert_eq!(rays.len(), SLAB_PROBE_GRID * (SLAB_PROBE_GRID + 1));
            for ray in &rays {
                assert!(ray.origin.z.abs() < 1e-4, "from the doorway plane");
                assert!(
                    (*ray.direction - Vec3::NEG_Z).length() < 1e-5,
                    "away from the player"
                );
                assert!(ray.origin.x.abs() <= 48.0 * SLAB_PROBE_SPAN + 1e-3);
                assert!(ray.origin.y >= 176.0 * SILL_PROBE_HEIGHT - 1e-3);
                assert!(ray.origin.y <= 88.0 + 88.0 * SLAB_PROBE_SPAN + 1e-3);
            }
            // A turned doorway turns its rays with it.
            let turned = quad.with_rotation(Quat::from_rotation_y(FRAC_PI_2));
            for ray in slab_probe_rays(&turned) {
                assert!((*ray.direction - Vec3::NEG_X).length() < 1e-5);
            }
        }

        /// `FarmhouseLDoor01` in its own frame, with the doorway plane through the model box's
        /// centre (13.5 behind the front of the leaf's box): `DoorBlack` (-36..0, centred 4.5
        /// behind the plane) is filler; the leaf (-4..9) and anything in front of the plane is not.
        #[test]
        fn the_filler_is_what_stands_behind_the_doorway_plane() {
            let quad =
                Transform::from_xyz(0.0, 88.0, -13.5).with_scale(Vec3::new(96.0, 176.0, 1.0));
            assert!(
                is_doorway_filler(Vec3::new(0.0, 88.0, -18.0), &quad),
                "DoorBlack"
            );
            assert!(
                !is_doorway_filler(Vec3::new(0.0, 88.0, 2.5), &quad),
                "the leaf's box"
            );
            assert!(
                !is_doorway_filler(Vec3::new(0.0, 88.0, -13.6), &quad),
                "a frame straddling the plane"
            );
            // Turned half a turn, the player stands on the other side and so does the filler.
            let turned = quad.with_rotation(Quat::from_rotation_y(PI));
            assert!(is_doorway_filler(Vec3::new(0.0, 88.0, 2.5), &turned));
            assert!(!is_doorway_filler(Vec3::new(0.0, 88.0, -18.0), &turned));
        }

        /// The black card of `FarmhouseLDoor01`'s `DoorBlack`, 22.5 units behind the doorway plane
        /// and facing the player, measured the way [`measure_doorway_slab`] measures it.
        #[test]
        fn a_backing_card_behind_the_doorway_sets_the_slab() {
            let card = Plane3d::new(Vec3::Z, Vec2::new(48.0, 88.0)).mesh().build();
            let placed = Affine3A::from_translation(Vec3::new(0.0, 88.0, -22.5));
            let hits = slab_probe_rays(&farmhouse_quad())
                .into_iter()
                .filter_map(|ray| ray_hits_either_face(&card, &placed, ray))
                .collect::<Vec<_>>();
            assert_eq!(
                hits.len(),
                SLAB_PROBE_GRID * (SLAB_PROBE_GRID + 1),
                "every ray hits it"
            );
            let slab = doorway_slab(hits);
            assert!(
                (slab - (22.5 - COMPOSITE_SLAB_MARGIN)).abs() < 1e-3,
                "{slab}"
            );
            // The same card turned away from the player (a double-sided surface seen from its back)
            // stops the slab just the same.
            let turned = Affine3A::from_rotation_translation(
                Quat::from_rotation_y(PI),
                Vec3::new(0.0, 88.0, -22.5),
            );
            let hits = slab_probe_rays(&farmhouse_quad())
                .into_iter()
                .filter_map(|ray| ray_hits_either_face(&card, &turned, ray));
            let slab = doorway_slab(hits);
            assert!(
                (slab - (22.5 - COMPOSITE_SLAB_MARGIN)).abs() < 1e-3,
                "{slab}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        doors::DoorCrossed,
        player::{
            EYE_HEIGHT, Player, eye_from_feet, player_auto_doors, player_door, player_walk,
            player_walks_through_doors,
        },
        profiling::ProfilingState,
        render::{TerrainMaterial, WaterMaterial, WaterReflectionTexture},
        streaming::{
            StreamingMetrics, StreamingPlugin, creation_rotation_to_bevy, creation_to_bevy,
            render_position,
        },
        transition::{
            CrossDoor, OpenDoor, TransitionPlugin, arrival_frame, door_frame, door_is_open,
            door_to_arrival_rotation, portal_pose,
        },
        world::{
            cache::CellCache,
            database::{AssetCatalog, WorldDatabase},
        },
    };
    use bevy::{
        asset::AssetPlugin, camera::CameraProjection, camera::RenderTargetInfo,
        camera::visibility::VisibilityPlugin, transform::TransformPlugin,
    };
    use std::{collections::VecDeque, path::Path, path::PathBuf, time::Duration};

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

    // -----------------------------------------------------------------------------------------
    // The fixture `update_portal` needs to place a camera at all: a door whose destination the
    // streamer has really loaded. Every other app this module builds leaves `StreamingWorld`
    // empty, and `update_portal` places nothing over a door whose destination is not resident
    // (`a_portal_that_stops_placing_its_camera_closes_its_door` is that case, on purpose).
    // -----------------------------------------------------------------------------------------

    /// The interior the fixture's door leads into.
    const FIXTURE_INTERIOR_CELL: u32 = 99;

    /// The world database the camera-pose test streams from: one Tamriel cell holding one load
    /// door, and the interior that door's `XTEL` leads into.
    ///
    /// The same two cells as `crate::transition`'s own fixture (`write_fixture`), copied here
    /// because that one is private to that module's tests: `StreamingWorld`'s map is private to
    /// `streaming`, so the only way a test in this module can have a *resident* destination - which
    /// is what `update_portal` selects a door by - is to let the real streamer load one. The door
    /// reference stands at creation 8200, -12200, 50 of the grid (2, -3) cell, which renders at
    /// (8, 50, -88) with the origin on that grid.
    fn write_door_fixture(directory: &Path) -> (PathBuf, PathBuf) {
        let database_path = directory.join("world.db");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute_batch(&format!(
                r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
                INSERT INTO schema_info VALUES({version});
                CREATE TABLE worldspaces(id INTEGER PRIMARY KEY,editor_id TEXT NOT NULL,parent_world INTEGER,flags INTEGER NOT NULL);
                CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER,interior_name TEXT,flags INTEGER NOT NULL);
                CREATE INDEX idx_cells_grid ON cells(worldspace_id,grid_x,grid_y);
                CREATE TABLE land(cell_id INTEGER PRIMARY KEY,heightmap BLOB NOT NULL);
                CREATE TABLE statics(id INTEGER PRIMARY KEY,editor_id TEXT,model_path TEXT,flags INTEGER NOT NULL,
                    bounds_min_x REAL NOT NULL DEFAULT -64,bounds_min_y REAL NOT NULL DEFAULT -64,bounds_min_z REAL NOT NULL DEFAULT -64,
                    bounds_max_x REAL NOT NULL DEFAULT 64,bounds_max_y REAL NOT NULL DEFAULT 64,bounds_max_z REAL NOT NULL DEFAULT 64,
                    bounds_valid INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER NOT NULL,worldspace_id INTEGER,base_form_id INTEGER NOT NULL,
                    is_exterior INTEGER NOT NULL,pos_x REAL NOT NULL,pos_y REAL NOT NULL,pos_z REAL NOT NULL,local_x REAL,local_y REAL,
                    rot_x REAL NOT NULL,rot_y REAL NOT NULL,rot_z REAL NOT NULL,scale REAL NOT NULL DEFAULT 1.0);
                CREATE INDEX idx_references_cell ON "references"(cell_id);
                CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
                CREATE TABLE door_links(ref_id INTEGER PRIMARY KEY,destination_ref_id INTEGER NOT NULL,
                    pos_x REAL NOT NULL,pos_y REAL NOT NULL,pos_z REAL NOT NULL,
                    rot_x REAL NOT NULL,rot_y REAL NOT NULL,rot_z REAL NOT NULL,
                    destination_cell_id INTEGER,destination_worldspace_id INTEGER);
                CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,editor_id TEXT,diffuse_path TEXT,normal_path TEXT,glow_path TEXT,
                    height_path TEXT,environment_path TEXT,mask_path TEXT,specular_path TEXT,detail_path TEXT);
                CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,editor_id TEXT,texture_set_id INTEGER,
                    material_type INTEGER,friction REAL,restitution REAL);
                CREATE TABLE waters(id INTEGER PRIMARY KEY,editor_id TEXT,opacity INTEGER,flags INTEGER NOT NULL,
                    shallow_color INTEGER,deep_color INTEGER,reflection_color INTEGER,flow_normal_path TEXT,data BLOB NOT NULL);

                INSERT INTO worldspaces VALUES(60,'Tamriel',0,0);
                INSERT INTO cells VALUES(10,60,2,-3,NULL,0);
                INSERT INTO cells VALUES(99,NULL,NULL,NULL,'Alftand01',0);
                INSERT INTO "references" VALUES(30,10,60,20,1,8200,-12200,50,8,88,0,0,0,1.0);
                INSERT INTO exterior_spatial VALUES(30,8200,8200,-12200,-12200,50,50,10,60);
                INSERT INTO "references" VALUES(31,99,NULL,21,0,-947.038,3958.835,591.917,NULL,NULL,0,0,0,1.0);
                INSERT INTO door_links VALUES(30,31,-947.038,3958.835,591.917,0,0,2.96989,99,NULL);"#,
                version = shared::WORLD_DATABASE_SCHEMA_VERSION
            ))
            .unwrap();
        drop(connection);

        let cache_path = directory.join("cell_cache.rkyv");
        let cache = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: Vec::new(),
        };
        std::fs::write(
            &cache_path,
            rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap(),
        )
        .unwrap();
        (database_path, cache_path)
    }

    /// The fixture in an app: the streamer, the crossing that pre-streams the door's destination,
    /// a main camera standing 200 units in front of the door, and the portal's own camera and
    /// quad - the entities `update_portal` writes.
    fn doorway_camera_app(directory: &Path) -> (App, Entity, Entity) {
        let (database_path, cache_path) = write_door_fixture(directory);
        let config = EngineConfig {
            worldspace_id: TAMRIEL,
            start_grid: (2, -3),
            stream_radius: 0,
            unload_radius: 1,
            ..EngineConfig::default()
        };
        let mut app = App::new();
        // `VisibilityPlugin` is the isolation's: it is what turns a `Visibility` into the
        // `InheritedVisibility` the leaf of a door is drawn by. `MeshPlugin` is beside it for the
        // skinned-mesh bounds asset the visibility systems read - the same pair `portal_app` adds,
        // and in the same order: `MeshPlugin` registers an asset, so it has to come after the
        // plugin that owns the `AssetServer`.
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::mesh::MeshPlugin,
            TransformPlugin,
            VisibilityPlugin,
        ))
        .init_asset::<Mesh>()
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .init_asset::<TerrainMaterial>()
        .init_asset::<WaterMaterial>()
        .insert_resource(config)
        .insert_resource(RenderOrigin(IVec2::new(2, -3)))
        .insert_resource(ActiveCell {
            worldspace_id: TAMRIEL,
            interior: None,
        })
        .insert_resource(WorldDatabase::open(&database_path).unwrap())
        .insert_resource(AssetCatalog::open(&database_path).unwrap())
        .insert_resource(CellCache::open(&cache_path).unwrap())
        .insert_resource(WaterReflectionTexture(Handle::default()))
        .init_resource::<ProfilingState>()
        .init_resource::<PortalState>()
        .init_resource::<MoveCamera>()
        // The crossing layer pre-streams what a door leads into; without it nothing but the cell
        // the camera stands in would ever be loaded and no destination would become resident.
        .add_plugins((StreamingPlugin, TransitionPlugin))
        .add_systems(Update, (show_load_door_leaves, isolate_cells).chain());
        add_update_portal(&mut app);
        app.add_systems(Update, move_the_camera.before(update_portal));

        let eye = Vec3::new(8.0, 50.0, -288.0);
        let camera = app
            .world_mut()
            .spawn((
                Transform::from_translation(eye),
                GlobalTransform::from_translation(eye),
                Projection::Perspective(PerspectiveProjection::default()),
                StreamingCamera,
            ))
            .id();
        let portal_camera = app
            .world_mut()
            .spawn((
                PortalCamera,
                Transform::default(),
                Projection::Perspective(PerspectiveProjection::default()),
                Camera::default(),
            ))
            .id();
        app.world_mut()
            .spawn((PortalQuad, Transform::default(), Visibility::default()));
        (app, camera, portal_camera)
    }

    /// Where the player's hands put the camera this frame. A system rather than a write between
    /// frames, so the move is made where the player makes it: inside `Update`, before the portal's
    /// own systems, with `GlobalTransform` still holding the pose `PostUpdate` propagated.
    #[derive(Resource, Default)]
    struct MoveCamera(Option<Transform>);

    fn move_the_camera(
        mut move_to: ResMut<MoveCamera>,
        mut cameras: Query<&mut Transform, With<StreamingCamera>>,
    ) {
        let Some(pose) = move_to.0.take() else {
            return;
        };
        let Ok(mut camera) = cameras.single_mut() else {
            return;
        };
        *camera = pose;
    }

    /// Frames until the fixture's streamer has caught up, with what it was still waiting for in
    /// the failure: a world database answers on its own thread, so a fixture that never settles has
    /// to say so rather than hang.
    fn run_until(app: &mut App, what: &str, mut condition: impl FnMut(&App) -> bool) {
        for _ in 0..500 {
            if condition(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
            app.update();
        }
        let metrics = app.world().resource::<StreamingMetrics>();
        panic!(
            "timed out waiting for {what}: requests={} responses={} failed={} resident={} loading={}",
            metrics.requests_submitted,
            metrics.responses_received,
            metrics.failed_cells,
            metrics.resident_cells,
            metrics.loading_cells,
        );
    }

    /// The single load door of the fixture app, once the streamer has spawned its cell.
    fn fixture_door(app: &mut App) -> Entity {
        let doors = |app: &App| -> Vec<Entity> {
            app.world()
                .iter_entities()
                .filter(|entity| entity.get::<LoadDoor>().is_some())
                .map(|entity| entity.id())
                .collect()
        };
        run_until(app, "the fixture's load door", |app| !doors(app).is_empty());
        let doors = doors(app);
        assert_eq!(doors.len(), 1, "the fixture has one load door");
        doors[0]
    }

    /// Where `update_portal` must place the portal camera for a main camera at `pose`: the door's
    /// own map, read out of the world exactly as the system reads it.
    fn door_map_pose(app: &App, door: Entity, pose: Transform) -> (Vec3, Quat) {
        let world = app.world();
        let global = *world.get::<GlobalTransform>(door).unwrap();
        let local = *world.get::<Transform>(door).unwrap();
        let row = world.get::<LoadDoor>(door).unwrap().clone();
        let anchor = world.get::<DoorAnchor>(door).cloned();
        let origin = world.resource::<RenderOrigin>().0;
        door_map(
            global.translation(),
            global.rotation(),
            local.scale,
            &row,
            anchor.as_ref(),
            origin,
        )
        .pose(pose.translation, pose.rotation)
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
                |_| front(&door),
                |_| Some(0.0)
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
                |_| front(&model_only),
                |_| Some(0.0)
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

    /// The window [`doorway_screen_rect`] is measured against: square, so the default
    /// projection's aspect ratio of 1 is the window's.
    const SCREEN: Vec2 = Vec2::new(1000.0, 1000.0);

    /// A doorway `size` wide and high, centred on `centre` and facing along `normal`, as
    /// `update_portal` lays the quad out, and the rectangle a camera at the origin looking down
    /// `-Z` sees it in.
    fn doorway_on_screen(centre: Vec3, normal: Vec3, size: Vec2) -> Option<Rect> {
        let quad = Transform::from_translation(centre)
            .with_rotation(Quat::from_rotation_arc(Vec3::Z, normal))
            .with_scale(size.extend(1.0));
        doorway_screen_rect(
            doorway_corners(&quad),
            &Transform::IDENTITY,
            &Projection::Perspective(PerspectiveProjection::default()),
            SCREEN,
        )
    }

    /// Where a point `x` to the side at depth `depth` lands on [`SCREEN`], in pixels from its left
    /// edge, under the default projection.
    fn screen_x(x: f32, depth: f32) -> f32 {
        let half_width = depth * (PerspectiveProjection::default().fov * 0.5).tan();
        (x / half_width + 1.0) * 0.5 * SCREEN.x
    }

    #[test]
    fn a_doorway_in_front_of_the_camera_covers_its_own_rectangle_of_the_screen() {
        let rect = doorway_on_screen(
            Vec3::new(0.0, 0.0, -500.0),
            Vec3::Z,
            Vec2::new(200.0, 300.0),
        )
        .expect("a doorway straight ahead is on screen");
        let expected_x = (screen_x(-100.0, 500.0), screen_x(100.0, 500.0));
        // `y` runs down the window, and the doorway is taller than it is wide.
        let expected_y = (
            SCREEN.y - screen_x(150.0, 500.0),
            SCREEN.y - screen_x(-150.0, 500.0),
        );
        assert!((rect.min.x - expected_x.0).abs() < 0.01, "{rect:?}");
        assert!((rect.max.x - expected_x.1).abs() < 0.01, "{rect:?}");
        assert!((rect.min.y - expected_y.0).abs() < 0.01, "{rect:?}");
        assert!((rect.max.y - expected_y.1).abs() < 0.01, "{rect:?}");
        assert!(
            rect.min.cmpgt(Vec2::ZERO).all() && rect.max.cmplt(SCREEN).all(),
            "the whole doorway is on screen, so nothing is clamped: {rect:?}"
        );
    }

    #[test]
    fn a_doorway_behind_the_camera_is_not_on_screen() {
        assert_eq!(
            doorway_on_screen(
                Vec3::new(0.0, 0.0, 500.0),
                Vec3::NEG_Z,
                Vec2::new(200.0, 300.0)
            ),
            None
        );
    }

    #[test]
    fn a_doorway_off_to_the_side_is_not_on_screen() {
        // In front of the camera, but far outside its field of view to the right.
        assert_eq!(
            doorway_on_screen(
                Vec3::new(2000.0, 0.0, -500.0),
                Vec3::Z,
                Vec2::new(200.0, 300.0)
            ),
            None
        );
    }

    #[test]
    fn a_doorway_partly_on_screen_is_clamped_to_the_window() {
        // Straddling the right edge of the view.
        let edge = 500.0 * (PerspectiveProjection::default().fov * 0.5).tan();
        let rect = doorway_on_screen(
            Vec3::new(edge, 0.0, -500.0),
            Vec3::Z,
            Vec2::new(200.0, 300.0),
        )
        .expect("half the doorway is on screen");
        assert!((rect.min.x - screen_x(edge - 100.0, 500.0)).abs() < 0.01);
        assert_eq!(rect.max.x, SCREEN.x, "clamped to the window's right edge");

        // A doorway the camera stands beside, with its far end ahead and its near end behind the
        // eye: only the part in front of the eye is on screen. Projected without the clip, the
        // corners behind the eye would land on the *left* of the screen and the rectangle would
        // cover the whole width.
        let rect = doorway_on_screen(
            Vec3::new(50.0, 0.0, -150.0),
            Vec3::X,
            Vec2::new(500.0, 300.0),
        )
        .expect("the part ahead of the eye is on screen");
        assert!(
            (rect.min.x - screen_x(50.0, 400.0)).abs() < 0.01,
            "the doorway starts where its far end is: {rect:?}"
        );
        assert_eq!(rect.max.x, SCREEN.x, "and runs off the right edge");
        assert_eq!((rect.min.y, rect.max.y), (0.0, SCREEN.y));
    }

    #[test]
    fn a_camera_in_the_doorways_plane_sees_no_doorway() {
        // Standing in the doorway and looking through it: every corner is level with the eye.
        assert_eq!(
            doorway_on_screen(Vec3::ZERO, Vec3::Z, Vec2::new(200.0, 300.0)),
            None
        );
        // Standing in the doorway's plane beside it and looking along the plane: edge on, a line
        // with no width. The corners are written out rather than rotated into place, so the plane
        // passes through the eye exactly.
        let edge_on = [
            Vec3::new(0.0, -150.0, -50.0),
            Vec3::new(0.0, -150.0, -550.0),
            Vec3::new(0.0, 150.0, -550.0),
            Vec3::new(0.0, 150.0, -50.0),
        ];
        assert_eq!(
            doorway_screen_rect(
                edge_on,
                &Transform::IDENTITY,
                &Projection::Perspective(PerspectiveProjection::default()),
                SCREEN,
            ),
            None
        );
    }

    /// The doorway's rectangle is rounded out to the quantum on every edge - never in, so every
    /// pixel of the doorway is rendered - and held to the window, whose own edges it keeps.
    #[test]
    fn a_doorway_rectangle_is_rounded_out_and_clamped_to_the_window() {
        let window = UVec2::new(1280, 720);
        let rect = |min: Vec2, max: Vec2| Rect { min, max };
        assert_eq!(
            doorway_render_rect(
                rect(Vec2::new(100.3, 70.2), Vec2::new(300.9, 500.1)),
                window
            ),
            Some(URect::new(64, 64, 320, 512)),
            "every edge rounds outwards to a multiple of 64"
        );
        assert_eq!(
            doorway_render_rect(
                rect(Vec2::new(64.0, 128.0), Vec2::new(192.0, 256.0)),
                window
            ),
            Some(URect::new(64, 128, 192, 256)),
            "a rectangle already on the grid is itself"
        );
        assert_eq!(
            doorway_render_rect(
                rect(Vec2::new(1200.5, 650.0), Vec2::new(1280.0, 720.0)),
                window
            ),
            Some(URect::new(1152, 640, 1280, 720)),
            "an edge at the window's edge stays there, not on the next multiple past it"
        );
        let full = UVec2::new(1920, 1080);
        assert_eq!(
            doorway_render_rect(rect(Vec2::ZERO, full.as_vec2()), full),
            Some(URect::new(0, 0, 1920, 1080)),
            "a doorway filling the screen is the whole window"
        );
        assert_eq!(
            doorway_render_rect(rect(Vec2::new(10.0, 10.0), Vec2::new(10.0, 40.0)), window),
            None,
            "a rectangle with no width renders nothing"
        );

        // A small move of the doorway is not a new size: the target is only reallocated when an
        // edge crosses a line of the grid.
        let before = doorway_render_rect(
            rect(Vec2::new(401.0, 99.0), Vec2::new(611.0, 530.0)),
            window,
        );
        let after = doorway_render_rect(
            rect(Vec2::new(405.0, 101.0), Vec2::new(615.0, 533.0)),
            window,
        );
        assert_eq!(before, after);

        // The target is the rectangle's size, and the ceiling still holds: a 4K doorway filling
        // the screen is drawn at the ceiling, one factor for both axes.
        let uhd = UVec2::new(3840, 2160);
        let whole = doorway_render_rect(rect(Vec2::ZERO, uhd.as_vec2()), uhd).unwrap();
        assert_eq!(
            portal_target_size(whole.size(), PORTAL_TEXTURE_MAX_SIZE),
            Some(PORTAL_TEXTURE_MAX_SIZE)
        );
        assert_eq!(
            portal_target_size(URect::new(64, 64, 320, 512).size(), PORTAL_TEXTURE_MAX_SIZE),
            Some(UVec2::new(256, 448)),
            "under the ceiling, one texel per window pixel"
        );
    }

    /// The sub-view is the rectangle of the main view, and a point lands on the same window pixel
    /// through it as through the main projection: the doorway image is the room behind it, pixel
    /// for pixel, with the doorway's oblique clip plane still applied.
    #[test]
    fn the_doorways_sub_view_lands_every_point_on_the_main_views_pixel() {
        let window = UVec2::new(1280, 720);
        let rect = URect::new(320, 128, 640, 512);
        let sub = doorway_sub_view(rect, window);
        assert_eq!(sub.full_size, window);
        assert_eq!(sub.offset, Vec2::new(320.0, 128.0));
        assert_eq!(sub.size, UVec2::new(320, 384));
        assert_eq!(
            doorway_rect_uniform(Some(rect)),
            Vec4::new(320.0, 128.0, 320.0, 384.0)
        );
        assert_eq!(doorway_rect_uniform(None), Vec4::ZERO);

        let main = PerspectiveProjection {
            aspect_ratio: window.x as f32 / window.y as f32,
            ..default()
        };
        // The portal camera's projection: the main one with an oblique doorway plane, 150 units
        // in front of the eye and tilted, as a doorway seen at an angle is.
        let clip_plane = Vec3::new(0.3, 0.1, -1.0).normalize().extend(-150.0);
        let Projection::Perspective(portal) =
            portal_projection(&Projection::Perspective(main.clone()), clip_plane, 150.0)
        else {
            panic!("a perspective projection stays one");
        };
        let full = portal.get_clip_from_view();
        let through_rect = portal.get_clip_from_view_for_sub(&sub);
        let pixel = |clip: Vec4, size: UVec2| {
            let ndc = clip.truncate().truncate() / clip.w;
            Vec2::new(ndc.x + 1.0, 1.0 - ndc.y) * 0.5 * size.as_vec2()
        };
        for point in [
            Vec3::new(-60.0, 40.0, -600.0),
            Vec3::new(-20.0, -10.0, -400.0),
            Vec3::new(-110.0, 90.0, -2000.0),
        ] {
            let on_window = pixel(full * point.extend(1.0), window);
            let in_rect = pixel(through_rect * point.extend(1.0), rect.size());
            assert!(
                (in_rect + rect.min.as_vec2() - on_window).length() < 1e-2,
                "{point}: window pixel {on_window}, rectangle pixel {in_rect}"
            );
        }
    }

    /// The culling frustum of the doorway view is the cone through its rectangle: a point the
    /// main view sees outside the doorway's rectangle is culled, one inside it is kept.
    #[test]
    fn the_doorway_views_frustum_is_the_cone_through_its_rectangle() {
        let window = UVec2::new(1280, 720);
        // The left half of the screen, top to bottom.
        let rect = URect::new(0, 0, 640, 720);
        let projection = Projection::Perspective(PerspectiveProjection {
            aspect_ratio: window.x as f32 / window.y as f32,
            ..default()
        });
        let transform = GlobalTransform::IDENTITY;
        let narrowed = sub_view_frustum(&projection, &doorway_sub_view(rect, window), &transform);
        let whole = projection.compute_frustum(&transform);
        let seen = |frustum: &Frustum, point: Vec3| {
            frustum.intersects_sphere(
                &bevy::camera::primitives::Sphere {
                    center: point.into(),
                    radius: 1.0,
                },
                true,
            )
        };
        let left = Vec3::new(-100.0, 0.0, -500.0);
        let right = Vec3::new(100.0, 0.0, -500.0);
        assert!(
            seen(&whole, left) && seen(&whole, right),
            "the main view sees both"
        );
        assert!(seen(&narrowed, left), "the doorway's half is kept");
        assert!(!seen(&narrowed, right), "the other half is culled");
    }

    /// With a doorway measured, the target is the doorway's rectangle and the quad maps through
    /// it; a rectangle that moves within the same size is a new mapping and not a new image.
    #[test]
    fn the_portal_target_is_the_doorways_rectangle_once_one_is_measured() {
        let (mut app, _, camera, quad, _) = resize_app(UVec2::new(1600, 900));
        app.insert_resource(PortalState {
            render_rect: Some(URect::new(64, 128, 448, 768)),
            ..default()
        });
        update(&mut app, 1);
        let target = target_image(&app);
        assert_eq!(target_size_of(&app, &target), UVec2::new(384, 640));
        assert_eq!(camera_target_of(&app, camera), target);
        assert_eq!(quad_texture_of(&app, quad), target);
        let rect_of = |app: &App| {
            let handle = app
                .world()
                .entity(quad)
                .get::<MeshMaterial3d<PortalMaterial>>()
                .unwrap();
            app.world()
                .resource::<Assets<PortalMaterial>>()
                .get(handle)
                .unwrap()
                .extension
                .doorway_rect
        };
        assert_eq!(rect_of(&app), Vec4::new(64.0, 128.0, 384.0, 640.0));

        app.world_mut().resource_mut::<PortalState>().render_rect =
            Some(URect::new(128, 128, 512, 768));
        let images = app.world().resource::<Assets<Image>>().len();
        update(&mut app, 1);
        assert_eq!(target_image(&app), target, "the same size keeps its image");
        assert!(
            app.world().resource::<Assets<Image>>().len() <= images,
            "and allocates none"
        );
        assert_eq!(rect_of(&app), Vec4::new(128.0, 128.0, 384.0, 640.0));
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
        // Every doorway equally well in view: distance is what is left to decide by.
        let on_screen = |_: Entity| Some(0.0);

        assert_eq!(
            select_portal_door(camera, doors, ready, &space, front, on_screen),
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
            select_portal_door(camera, far_only, ready, &space, |_| 100.0, on_screen),
            None
        );

        // A door still swinging open, one whose clip has run out and left its leaves in the
        // doorway, and one swinging **shut**: the portal draws its window through all three.
        // `portal_shows_through` is the question it asks of a state, and on a closing door it
        // answers where `is_open` does not (see
        // `a_closing_door_keeps_its_window_and_still_is_not_open`).
        for state in [
            DoorState::Opening,
            DoorState::Open { animated: true },
            DoorState::Closing,
        ] {
            assert!(
                portal_shows_through(Some(&state)),
                "{state:?} is a doorway the portal draws through"
            );
            let mut with_a_swinging_near_door = doors;
            with_a_swinging_near_door[0].3 = Some(&state);
            assert_eq!(
                select_portal_door(
                    camera,
                    with_a_swinging_near_door,
                    ready,
                    &space,
                    front,
                    on_screen
                ),
                Some(near_door),
                "the nearest such doorway is looked through: {state:?}"
            );
        }

        // A closed door is not, however close and ready its destination is: the window stands in
        // the only opening a door has, so the destination image would be behind its leaf. A door
        // with no state at all counts as closed, like everywhere else.
        for state in [None, Some(&DoorState::Closed)] {
            assert!(
                !portal_shows_through(state),
                "the state under test is one the portal must not render through: {state:?}"
            );
            let mut with_a_closed_near_door = doors;
            with_a_closed_near_door[0].3 = state;
            assert_eq!(
                select_portal_door(
                    camera,
                    with_a_closed_near_door,
                    ready,
                    &space,
                    front,
                    on_screen
                ),
                Some(far_door),
                "the open door behind the closed one is the one to look through: {state:?}"
            );
        }
    }

    /// **The user's capture `01`** (research-193, fault 1): the Riverwood Trader has two load doors
    /// into one interior, the front door `0001341F` and the upper door `00070E69` 224 units above
    /// it, and both were open. The player stood at creation `(21914.93, -45562.18, 1.02)` looking
    /// into the front doorway (heading 142.85 degrees); the upper one is nearer - 179.8 units
    /// against 220.5 - and stands 52 degrees above the view, off the screen. The portal has to look
    /// through the doorway in view, not the nearest one.
    #[test]
    fn the_portal_looks_through_the_doorway_in_view_not_the_nearest_one() {
        let camera = creation_to_bevy(Vec3::new(21914.928, -45562.176, 1.017));
        let heading = 142.849_f32.to_radians();
        let forward = creation_to_bevy(Vec3::new(heading.sin(), heading.cos(), 0.0));
        let space = ActiveSpace {
            interior: None,
            worldspace_id: TAMRIEL,
            center: IVec2::new(5, -12),
            radius: 1,
        };
        let front_door = LoadDoor {
            ref_id: 0x0001_341F,
            ..interior_door(0x0001_33C9)
        };
        let upper_door = LoadDoor {
            ref_id: 0x0007_0E69,
            ..interior_door(0x0001_33C9)
        };
        let front_entity = Entity::from_raw_u32(1).unwrap();
        let upper_entity = Entity::from_raw_u32(2).unwrap();
        let front_position = creation_to_bevy(Vec3::new(22022.217, -45713.05, -118.811));
        let upper_position = creation_to_bevy(Vec3::new(21995.43, -45684.742, 105.017));
        assert!((front_position.distance(camera) - 220.5).abs() < 0.5);
        assert!((upper_position.distance(camera) - 179.8).abs() < 0.5);

        // The doorways' centres: `FarmhouseLDoor01`'s opening is 176 units tall on its origin.
        let doorway = |entity: Entity| {
            if entity == front_entity {
                front_position + Vec3::Y * 88.0
            } else {
                upper_position + Vec3::Y * 88.0
            }
        };
        let angle = |entity: Entity| forward.angle_between(doorway(entity) - camera);
        assert!(
            angle(upper_entity) > 50.0_f32.to_radians(),
            "the upper doorway is far above the view: {} degrees",
            angle(upper_entity).to_degrees()
        );
        assert!(
            angle(front_entity) < 15.0_f32.to_radians(),
            "the front doorway is ahead: {} degrees",
            angle(front_entity).to_degrees()
        );

        let open = DoorState::Open { animated: true };
        let doors = [
            (front_entity, front_position, &front_door, Some(&open), None),
            (upper_entity, upper_position, &upper_door, Some(&open), None),
        ];
        let ready = |_: &DoorDestination, _: Option<&DoorAnchor>| true;
        let in_front = |_: Entity| 100.0;

        // As the engine measures it: the upper doorway covers none of the screen.
        let on_screen = |entity: Entity| (entity == front_entity).then(|| angle(entity));
        assert_eq!(
            select_portal_door(camera, doors, ready, &space, in_front, on_screen),
            Some(front_entity),
            "the doorway in view is looked through, though the other door is nearer"
        );

        // Both doorways on screen (a wide view): the better aimed one still wins.
        assert_eq!(
            select_portal_door(camera, doors, ready, &space, in_front, |entity| Some(
                angle(entity)
            )),
            Some(front_entity)
        );

        // Neither doorway on screen: the nearest is kept, as before, so a doorway the player looks
        // away from keeps its window and comes back into view already drawn.
        assert_eq!(
            select_portal_door(camera, doors, ready, &space, in_front, |_| None),
            Some(upper_entity)
        );

        // Two doorways the view cannot tell apart - within the slack of one another - go to the
        // nearer door.
        let slack = PORTAL_AIM_SLACK_DEGREES.to_radians();
        assert_eq!(
            select_portal_door(camera, doors, ready, &space, in_front, |entity| Some(
                if entity == front_entity {
                    0.2
                } else {
                    0.2 + slack * 0.5
                }
            )),
            Some(upper_entity)
        );
    }

    /// The one state the portal and the crossing answer differently on: a door on its way shut.
    ///
    /// The player closing a door (`E`) and a door closing itself behind them (2026-09-25, both) play
    /// the `Close` clip, about 0.6 s, with the leaf drawn swinging back across the doorway
    /// (`crate::door_animation`). The window belongs behind that leaf for the whole swing - the door
    /// reaching `Closed` is what ends it - while no crossing may be walked through a door that is on
    /// its way shut. So the portal asks [`portal_shows_through`] and the crossing keeps asking
    /// [`DoorState::is_open`] through `door_is_open`, and this is the only state where the two
    /// differ: without it the doorway emptied itself under the closing leaf, which is what the user
    /// saw in play.
    #[test]
    fn a_closing_door_keeps_its_window_and_still_is_not_open() {
        assert!(
            portal_shows_through(Some(&DoorState::Closing)),
            "the portal keeps drawing the doorway the leaf is swinging shut across"
        );
        assert!(
            !door_is_open(Some(&DoorState::Closing)),
            "and the crossing may not be walked through it: `is_open` is the crossing's question"
        );
        for state in [DoorState::Opening, DoorState::Open { animated: true }] {
            assert!(
                portal_shows_through(Some(&state)),
                "{state:?} is a doorway the portal draws through"
            );
            assert_eq!(
                portal_shows_through(Some(&state)),
                door_is_open(Some(&state)),
                "the two agree wherever the door is on its way open: {state:?}"
            );
        }
        assert!(
            !portal_shows_through(Some(&DoorState::Closed))
                && !door_is_open(Some(&DoorState::Closed)),
            "a closed door is a closed door to both"
        );
        assert!(
            !portal_shows_through(None),
            "a door with no state at all counts as closed, like everywhere else"
        );
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
    /// portal only ever picks a door it draws a window through ([`portal_shows_through`]) - so its
    /// leaf has to be back the frame the portal lets it go. This runs the real [`update_portal`] -
    /// here one that cannot place a camera at all,
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

    /// The doorway image is drawn from where the camera is *this* frame, not where it was last
    /// frame.
    ///
    /// `update_portal` read the main camera's `GlobalTransform`, which `TransformPlugin` only
    /// propagates in `PostUpdate`, so the portal camera stood where the main camera had been a
    /// frame earlier: every move and turn trailed one frame behind in the window (the user, in
    /// play, 2026-09-24; fixed in `63af9d3`). The main camera is a root entity, so its `Transform`
    /// is its world pose.
    ///
    /// Nothing else in this module can see that: no other test moves the main camera between
    /// updates, and `spawn_camera` gives it a `Transform` and a `GlobalTransform` with the *same*
    /// value - the one case where reading either one gives the same answer. Here the camera is
    /// moved where the player moves it (in `Update`, before the portal's systems) and the portal
    /// camera is read back against the door's own map.
    #[test]
    fn the_portal_camera_is_placed_from_this_frames_camera_pose() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, camera, portal_camera) = doorway_camera_app(directory.path());
        // A portal places nothing over a door whose destination is not there: the fixture's door
        // pre-streams the interior it leads into, which the streamer loads on its own thread.
        run_until(&mut app, "the door's destination to be resident", |app| {
            app.world()
                .resource::<StreamingWorld>()
                .is_resident(&CellKey::Interior(FIXTURE_INTERIOR_CELL))
        });
        let door = fixture_door(&mut app);
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: false });
        update(&mut app, 2);

        // The camera is where the test stood it up, so the portal is up and its camera is placed
        // from that pose: what follows is about the pose alone, not about a portal that never ran.
        let standing = *app.world().entity(camera).get::<Transform>().unwrap();
        let portal = *app
            .world()
            .entity(portal_camera)
            .get::<Transform>()
            .unwrap();
        let (placed, placed_rotation) = door_map_pose(&app, door, standing);
        assert_eq!(
            app.world().resource::<PortalState>().open_door,
            Some(door),
            "the portal is rendering through the fixture's door"
        );
        assert!(
            (portal.translation - placed).length() < 1.0e-3
                && portal.rotation.abs_diff_eq(placed_rotation, 1.0e-5),
            "the portal camera stands at {:?} looking {:?}; the camera it follows is at {standing:?}, \
             which the door's map takes to {placed:?}",
            portal.translation,
            portal.rotation
        );

        // The player walks and turns. The camera moves in this frame's `Update`, and its
        // `GlobalTransform` is still the pose the last `PostUpdate` propagated.
        let moved = Transform::from_translation(Vec3::new(8.0, 60.0, -250.0))
            .with_rotation(Quat::from_rotation_y(0.3));
        app.world_mut().insert_resource(MoveCamera(Some(moved)));
        update(&mut app, 1);

        let portal = *app
            .world()
            .entity(portal_camera)
            .get::<Transform>()
            .unwrap();
        let (want_position, want_rotation) = door_map_pose(&app, door, moved);
        assert!(
            (portal.translation - want_position).length() < 1.0e-3
                && portal.rotation.abs_diff_eq(want_rotation, 1.0e-5),
            "the portal camera stands at {:?} looking {:?}: the camera moved to {moved:?} this \
             frame, and the doorway image is drawn from the pose a frame behind it instead",
            portal.translation,
            portal.rotation
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

    /// A door whose anchor stands on a threshold, raised to a floor 45 units above its box's
    /// bottom, and the same door anchored on its box centre (the centre rule) and with no anchor.
    fn threshold_door() -> (Vec3, Quat, Vec3, ExpectedModelBounds, DoorAnchor) {
        let bounds = ExpectedModelBounds {
            min: Vec3::new(-60.0, -5.0, -20.0),
            max: Vec3::new(60.0, 300.0, 44.0),
        };
        let box_centre = (bounds.min + bounds.max) * 0.5;
        let anchor = DoorAnchor {
            tier: crate::doors::DoorAnchorTier::SameModel,
            source_box_centre: box_centre.to_array(),
            source_anchor_height: 40.0,
            destination: crate::doors::DoorwayGeometry {
                position: [0.0; 3],
                rotation: [0.0; 3],
                scale: 1.0,
                box_centre: box_centre.to_array(),
                anchor_height: 40.0,
            },
            destination_grid: None,
            facings: crate::doors::DoorwayFacings::Kept,
        };
        (
            Vec3::new(-400.0, 260.0, 900.0),
            creation_rotation_to_bevy([0.0, 0.0, 0.7]),
            Vec3::ONE,
            bounds,
            anchor,
        )
    }

    /// The bottom and top edges of a doorway quad, in world height.
    fn quad_bottom_and_top(quad: &Transform) -> (f32, f32) {
        (
            quad.translation.y - quad.scale.y * 0.5,
            quad.translation.y + quad.scale.y * 0.5,
        )
    }

    /// Gerdur's House (impl-206): a door set into the ground has its anchor, and so the portal
    /// camera's pivot, on the floor 45 units above its box's bottom. The quad's bottom edge is that
    /// threshold - the point `source_doorway_centre` places - and not the box's bottom, which would
    /// show the destination from under its floor; the top, sides and plane are the box's.
    #[test]
    fn an_anchored_quad_stands_on_the_threshold_the_camera_is_anchored_on() {
        let (position, rotation, scale, bounds, anchor) = threshold_door();
        let frame = rotation;
        let boxed =
            doorway_quad_transform(position, rotation, frame, scale, None, Some(&bounds), None);
        let anchored = doorway_quad_transform(
            position,
            rotation,
            frame,
            scale,
            None,
            Some(&bounds),
            Some(&anchor),
        );
        let pivot = crate::transition::source_doorway_centre(position, rotation, scale, &anchor);
        let (bottom, top) = quad_bottom_and_top(&anchored);
        let (box_bottom, box_top) = quad_bottom_and_top(&boxed);
        assert!(
            (bottom - pivot.y).abs() < 1.0e-3,
            "bottom {bottom}, pivot {}",
            pivot.y
        );
        assert!((bottom - (position.y + 40.0)).abs() < 1.0e-3);
        assert!((box_bottom - (position.y - 5.0)).abs() < 1.0e-3);
        assert!((top - box_top).abs() < 1.0e-3, "the top stays the box's");
        assert_eq!(anchored.scale.x, boxed.scale.x, "the width stays the box's");
        assert_eq!(anchored.rotation, boxed.rotation);
        let depth = |quad: &Transform| (quad.translation - position).dot(frame * Vec3::NEG_Z);
        assert!(
            (depth(&anchored) - depth(&boxed)).abs() < 1.0e-3,
            "same plane"
        );
        let plan = |v: Vec3| (frame.inverse() * (v - position)).x;
        assert!((plan(anchored.translation) - plan(boxed.translation)).abs() < 1.0e-3);
    }

    /// **impl-218, Gerdur's House** (door `00013423` onto `00013407`, one `FarmhouseLDoor01`): on
    /// the database's thresholds the quad's bottom and the map's pivot stood 13.45 units above the
    /// door, on the `XTEL` arrival, while the porch is 0.30 above it - the room's floor stood 13.15
    /// units over the porch in the doorway, with the doorway's own void black under its edge. Once
    /// the anchor is on the measured floors, the quad's bottom is the porch, and a point on the porch
    /// just outside the doorway is carried onto the room's floor just inside it: one floor line.
    #[test]
    fn gerdurs_doorway_quad_and_map_stand_on_the_measured_floors() {
        let bounds = ExpectedModelBounds {
            min: Vec3::new(-48.0, 0.0, -36.0),
            max: Vec3::new(48.0, 176.0, 9.0),
        };
        let box_centre = (bounds.min + bounds.max) * 0.5;
        let database = DoorAnchor {
            tier: crate::doors::DoorAnchorTier::SameModel,
            source_box_centre: box_centre.to_array(),
            source_anchor_height: 13.445_751,
            destination: crate::doors::DoorwayGeometry {
                position: [-511.666, -292.434_66, 0.0],
                rotation: [0.0, 0.0, PI],
                scale: 1.0,
                box_centre: box_centre.to_array(),
                anchor_height: 0.0,
            },
            destination_grid: None,
            facings: crate::doors::DoorwayFacings::SameModel {
                turn: PI - 2.356_194_5,
            },
        };
        let (porch, room) = (0.298_178, -2.5e-5);
        let measured = database.on_measured_floors(porch, room).unwrap();
        // The outside door, as the streamer places it (render origin at its own cell).
        let position = Vec3::new(1234.0, -18.646_841, -567.0);
        let rotation = creation_rotation_to_bevy([0.0, 0.0, 2.356_194_5]);
        let frame = rotation;
        let bottom = |anchor: &DoorAnchor| {
            let quad = doorway_quad_transform(
                position,
                rotation,
                frame,
                Vec3::ONE,
                None,
                Some(&bounds),
                Some(anchor),
            );
            quad_bottom_and_top(&quad).0 - position.y
        };
        assert!((bottom(&database) - 13.445_751).abs() < 1.0e-3);
        assert!(
            (bottom(&measured) - porch).abs() < 1.0e-3,
            "{}",
            bottom(&measured)
        );
        // The map, as `door_map` builds it under an anchor: pivot and arrival are the two doorways'
        // anchor points, turned by the doorways' own frames.
        let pivot =
            crate::transition::source_doorway_centre(position, rotation, Vec3::ONE, &measured);
        let arrival = crate::transition::destination_doorway_centre(&measured, true, IVec2::ZERO);
        let arrival_rotation = frame * Quat::from_rotation_y(-(PI - 2.356_194_5));
        let front = frame * Vec3::NEG_Z;
        let on_porch = Vec3::new(pivot.x, position.y + porch, pivot.z) + front * 8.0;
        let behind = on_porch - front * 16.0;
        let (landed, _) = portal_pose(
            pivot,
            frame,
            arrival,
            arrival_rotation,
            behind,
            Quat::IDENTITY,
        );
        assert!(
            (landed.y - room).abs() < 1.0e-3,
            "the porch is carried onto the room's floor: {}",
            landed.y
        );
    }

    /// A door with no anchor, and one anchored on its box centre (the centre rule, which has no
    /// threshold to give), keep today's quad exactly: the box's.
    #[test]
    fn an_unanchored_or_centre_anchored_quad_keeps_the_box() {
        let (position, rotation, scale, bounds, mut anchor) = threshold_door();
        let boxed = doorway_quad_transform(
            position,
            rotation,
            rotation,
            scale,
            None,
            Some(&bounds),
            None,
        );
        let (size, centre) = portal_quad_extents(None, Some(&bounds), rotation, rotation, scale);
        assert_eq!(boxed.scale, Vec3::new(size.x, size.y, 1.0));
        assert!(
            boxed
                .translation
                .abs_diff_eq(position + rotation * centre, 1.0e-3)
        );
        anchor.source_anchor_height = (rotation * Vec3::from_array(anchor.source_box_centre)).y;
        let centred = doorway_quad_transform(
            position,
            rotation,
            rotation,
            scale,
            None,
            Some(&bounds),
            Some(&anchor),
        );
        assert_eq!(centred, boxed);
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

        // And while it swings **shut**: the same model, the same leaves, mid-swing the other way,
        // with the portal drawing its window through the doorway the leaf is coming back into. The
        // state is one the portal draws through (`portal_shows_through`), so this is the frame the
        // user's report is about - the doorway stayed a doorway under the closing leaf.
        app.world_mut()
            .entity_mut(animated)
            .insert(DoorState::Closing);
        portal_shows(&mut app, Some(animated));
        update(&mut app, 1);
        assert_eq!(
            visibility_of(&app, animated),
            Visibility::Inherited,
            "a closing door's frame and leaves are drawn too: it is still the swing"
        );
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

    /// Puts `entity` on `layers` behind the change detection's back: nothing that reads
    /// `Changed<RenderLayers>` sees it, so only a walk of the subtree it is in would put it back.
    /// What the tests below tell a walked root from one that was left alone by.
    fn set_layers_unseen(app: &mut App, entity: Entity, layers: RenderLayers) {
        let mut entity = app.world_mut().entity_mut(entity);
        let mut current = entity.get_mut::<RenderLayers>().unwrap();
        *current.bypass_change_detection() = layers;
    }

    #[test]
    fn a_cell_whose_role_did_not_change_is_not_walked() {
        let mut app = portal_app();
        spawn_camera(&mut app, Vec3::ZERO);
        let (_, hidden_mesh) = spawn_cell(&mut app, 0x0005_6C1B, None, None, None);
        update(&mut app, 2);
        assert_eq!(layers_of(&app, hidden_mesh), RenderLayers::none());

        // A frame in which nothing about the cell changed: its mesh is not looked at, so a layer
        // put on it unseen stays where it was put.
        set_layers_unseen(&mut app, hidden_mesh, RenderLayers::layer(0));
        update(&mut app, 3);
        assert_eq!(
            layers_of(&app, hidden_mesh),
            RenderLayers::layer(0),
            "a cell that kept its role is not walked"
        );
    }

    #[test]
    fn a_cell_whose_role_changed_is_walked_once() {
        let mut app = portal_app();
        spawn_camera(&mut app, Vec3::ZERO);
        let (_, active_mesh) = spawn_cell(&mut app, INTERIOR_ALFTAND01, None, None, None);
        let (other_root, other_mesh) = spawn_cell(&mut app, 0x0005_6C1B, None, None, None);
        let (water, _) = spawn_mesh(&mut app, other_root, Some(RenderLayers::layer(1)));
        update(&mut app, 2);
        assert!(on_main_camera(&app, active_mesh));
        assert!(!on_main_camera(&app, other_mesh));

        // The player crosses into the other cell: both roles change, and both subtrees are
        // re-layered in the one frame the crossing is seen in.
        *app.world_mut().resource_mut::<ActiveCell>() = ActiveCell {
            worldspace_id: TAMRIEL,
            interior: Some(0x0005_6C1B),
        };
        update(&mut app, 1);
        assert!(on_main_camera(&app, other_mesh));
        assert_eq!(layers_of(&app, water), RenderLayers::layer(1));
        assert!(
            app.world()
                .entity(water)
                .get::<PortalOriginalLayers>()
                .is_none()
        );
        assert_eq!(layers_of(&app, active_mesh), RenderLayers::none());

        // And only in that frame: after the one frame in which its own writes come back to it as
        // changed layers (each checked alone, and already right), both cells are left alone.
        update(&mut app, 1);
        set_layers_unseen(&mut app, active_mesh, RenderLayers::layer(0));
        update(&mut app, 3);
        assert_eq!(
            layers_of(&app, active_mesh),
            RenderLayers::layer(0),
            "a role change is walked once, not every frame after it"
        );
    }

    #[test]
    fn a_scene_that_finishes_spawning_under_a_settled_cell_is_isolated() {
        let mut app = portal_app();
        spawn_camera(&mut app, Vec3::ZERO);
        let (root, _) = spawn_cell(&mut app, 0x0005_6C1B, None, None, None);
        // A reference whose glTF scene has not spawned yet: a node with no children.
        let reference = app
            .world_mut()
            .spawn((Transform::default(), Visibility::default(), ChildOf(root)))
            .id();
        update(&mut app, 3);

        // The scene arrives frames later, two levels deep, while the cell's role stays the same.
        let scene = app
            .world_mut()
            .spawn((
                Transform::default(),
                Visibility::default(),
                ChildOf(reference),
            ))
            .id();
        let (deep_mesh, _) = spawn_mesh(&mut app, scene, None);
        let (water, _) = spawn_mesh(&mut app, scene, Some(RenderLayers::layer(1)));
        update(&mut app, 1);
        assert_eq!(layers_of(&app, deep_mesh), RenderLayers::none());
        assert_eq!(layers_of(&app, water), RenderLayers::none());

        // Something else giving a node of the settled cell layers of its own is caught too.
        let (late_layered, _) = spawn_mesh(&mut app, reference, None);
        update(&mut app, 1);
        app.world_mut()
            .entity_mut(late_layered)
            .insert(RenderLayers::layer(1));
        update(&mut app, 1);
        assert_eq!(layers_of(&app, late_layered), RenderLayers::none());

        // Entering the cell gives every one of them back the layers it had.
        *app.world_mut().resource_mut::<ActiveCell>() = ActiveCell {
            worldspace_id: TAMRIEL,
            interior: Some(0x0005_6C1B),
        };
        update(&mut app, 1);
        assert!(on_main_camera(&app, deep_mesh));
        assert_eq!(layers_of(&app, water), RenderLayers::layer(1));
        assert!(
            on_main_camera(&app, late_layered),
            "the layers it had when the isolation first moved it"
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
                 window - the target is only ever written for a door the portal draws through \
                 (`portal_shows_through`), which a closed one is not",
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
            source_anchor_height: TOWER_BOX[1],
            destination: crate::doors::DoorwayGeometry {
                position: [2879.831, 2_718.83, -1828.0],
                rotation: [0.0, 0.0, 1.0],
                scale: 1.0,
                box_centre: TOWER_BOX,
                anchor_height: TOWER_BOX[1],
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

    /// A door on its way shut keeps its mirror for the whole swing, and drops it when it has shut.
    ///
    /// The closing leaf swings back **across** the doorway's plane, so part of it stands on the far
    /// side of that plane, where the quad covers the door's own instance: the mirror is what draws
    /// that part of it inside the doorway image, exactly as it does while the door opens. It used to
    /// be dropped the frame the `Close` clip started - the mirror was made only for a door that was
    /// `Opening` or `Open { animated: true }` - so the far half of a closing leaf vanished.
    #[test]
    fn a_closing_door_keeps_its_mirror_until_it_has_shut() {
        let mut app = portal_app();
        add_mirror_systems(&mut app);
        let case = mirror_case(tower_door());
        let (door, _) = spawn_mirrorable_door(&mut app, case.door.clone(), DoorState::Closing);
        let transform = Transform {
            translation: case.position,
            rotation: case.rotation,
            scale: DOOR_SCALE,
        };
        app.world_mut()
            .entity_mut(door)
            .insert((transform, GlobalTransform::from(transform)));
        portal_shows(&mut app, Some(door));
        update(&mut app, 1);
        assert_eq!(
            mirror_of(&mut app)
                .expect("a closing door is one the portal draws through")
                .1,
            door,
            "the mirror is the closing door's own second instance"
        );

        // Shut: the doorway is a closed door, the leaf has stopped swinging, and the mirror goes
        // with the swing that needed it.
        app.world_mut().entity_mut(door).insert(DoorState::Closed);
        update(&mut app, 1);
        assert!(
            mirror_of(&mut app).is_none(),
            "a closed door has no swing left to draw in the window"
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

    /// Each sun keeps the cascades of the views that draw it and loses the rest (impl-226): the
    /// doorway's sun (layer 2) none of the main camera's (layers 0 and 1) or the water reflection's
    /// (no layers: layer 0), the engine's sun (no layers) none of the portal camera's. With the
    /// portal camera off Bevy builds it no cascades at all, so the doorway's sun is left with none
    /// and culls nothing - the open-door-behind-the-player case.
    #[test]
    fn a_sun_keeps_only_the_cascades_of_the_views_that_draw_it() {
        use bevy::ecs::system::RunSystemOnce;
        let mut world = World::new();
        let main = world
            .spawn((
                Camera::default(),
                RenderLayers::from_layers(&MAIN_CAMERA_LAYERS),
            ))
            .id();
        let water = world.spawn(Camera::default()).id();
        let portal = world
            .spawn((Camera::default(), RenderLayers::layer(DESTINATION_LAYER)))
            .id();
        let cascades_for = |views: &[Entity]| Cascades {
            cascades: views.iter().map(|view| (*view, Vec::new())).collect(),
        };
        let engine_sun = world
            .spawn((
                DirectionalLight::default(),
                cascades_for(&[main, water, portal]),
            ))
            .id();
        let doorway_sun = world
            .spawn((
                DirectionalLight::default(),
                cascades_for(&[main, water, portal]),
                RenderLayers::layer(DESTINATION_LAYER),
            ))
            .id();
        let doorway_sun_off = world
            .spawn((
                DirectionalLight::default(),
                cascades_for(&[main, water]),
                RenderLayers::layer(DESTINATION_LAYER),
            ))
            .id();
        world
            .run_system_once(keep_cascades_of_views_that_draw_their_light)
            .unwrap();
        let views = |world: &World, light: Entity| {
            let mut views: Vec<Entity> = world
                .get::<Cascades>(light)
                .unwrap()
                .cascades
                .keys()
                .copied()
                .collect();
            views.sort();
            views
        };
        let mut expected = vec![main, water];
        expected.sort();
        assert_eq!(views(&world, engine_sun), expected);
        assert_eq!(views(&world, doorway_sun), vec![portal]);
        assert!(views(&world, doorway_sun_off).is_empty());
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

    /// The doorway's sun takes the engine sun's direction and whether it casts shadows, and keeps
    /// shadow cascades of its own - the engine sun's count, over the near distance a doorway shows -
    /// rather than a copy of the engine sun's: one sun in the world, drawn in two views.
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
        // The cascades are the doorway's own, not a copy of the engine sun's: as many (Bevy cannot
        // draw two shadowed directional lights with different counts), over the near distance a
        // doorway shows, and the engine sun's set is left as it was.
        let engine_default = CascadeShadowConfig::default();
        let (bounds, overlap, minimum) = cascades(&app, destination_sun);
        assert_eq!(bounds.len(), engine_default.bounds.len());
        assert_eq!(bounds.first(), Some(&PORTAL_SUN_FIRST_CASCADE));
        assert!((bounds.last().unwrap() - PORTAL_SUN_SHADOW_DISTANCE).abs() < 0.01);
        assert_eq!(overlap, PORTAL_SUN_CASCADE_OVERLAP);
        assert_eq!(minimum, PORTAL_SUN_SHADOW_NEAR);
        assert_eq!(
            cascades(&app, engine_sun),
            (
                engine_default.bounds.clone(),
                engine_default.overlap_proportion,
                engine_default.minimum_distance,
            ),
            "the engine sun's cascades are its own and untouched"
        );

        // An engine sun with another count takes the doorway's with it, over the doorway's reach.
        app.world_mut()
            .entity_mut(engine_sun)
            .insert(CascadeShadowConfig::from(CascadeShadowConfigBuilder {
                num_cascades: 2,
                ..default()
            }));
        update(&mut app, 1);
        let (bounds, ..) = cascades(&app, destination_sun);
        assert_eq!(bounds.len(), 2);
        assert!((bounds.last().unwrap() - PORTAL_SUN_SHADOW_DISTANCE).abs() < 0.01);

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
        use crate::atmosphere::space_atmosphere;
        use crate::world::lighting::fixtures::{BLACKREACH, real_spaces};
        let (_directory, catalog) = real_spaces();
        let tamriel = space_atmosphere(Some(&catalog), space_key(TAMRIEL, None));
        let blackreach = space_atmosphere(Some(&catalog), space_key(BLACKREACH, None));
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

        // `crate::atmosphere::update_atmosphere` writes every `DirectionalLight` in the world when
        // the space the player stands in changes - this one included, since it cannot know whose it
        // is. That write is repaired rather than left standing: the doorway keeps the destination's
        // sun.
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

    // -----------------------------------------------------------------------------------------
    // The player controller in a world with a destination: only the active space collides and has
    // usable doors (impl-166). The findings these come from were written from the outside against
    // the code, so each of them is reproduced here through the systems a real frame runs, in the
    // order it runs them.
    // -----------------------------------------------------------------------------------------

    /// The player's own systems, in `PlayerPlugin`'s order, over a world with the real cell
    /// isolation: [`portal_app`]'s cells, classified by `isolate_cells` *behind* the controller -
    /// which is the order `PortalPlugin` runs it in, after `PlayerPlugin`. One frame is therefore
    /// exactly a real frame's, down to the role a step reads being the one the previous frame's
    /// isolation wrote: the space the camera is standing in now.
    fn player_app() -> App {
        let mut app = portal_app();
        app.insert_resource(ButtonInput::<KeyCode>::default())
            .init_resource::<ProfilingState>()
            .init_resource::<CameraSteps>()
            .init_resource::<Crossings>()
            .init_resource::<Opened>()
            .add_message::<CrossDoor>()
            .add_message::<OpenDoor>()
            .add_message::<DoorCrossed>()
            .add_systems(
                Update,
                (
                    advance_camera,
                    player_walk,
                    player_door,
                    player_auto_doors,
                    player_walks_through_doors,
                    collect_crossings,
                    collect_opened,
                )
                    .chain()
                    .before(isolate_cells),
            );
        app
    }

    /// Where the test camera is put this frame: one eye position per frame, written the way
    /// [`player_walk`] writes it - the `Transform` alone, with bevy's propagation filling in the
    /// `GlobalTransform` in `PostUpdate`, a frame later.
    #[derive(Resource, Default)]
    struct CameraSteps(VecDeque<Vec3>);

    /// The doors the controller asked to cross, in order.
    #[derive(Resource, Default)]
    struct Crossings(Vec<Entity>);

    /// The doors `E` was pressed on, in order.
    #[derive(Resource, Default)]
    struct Opened(Vec<Entity>);

    fn advance_camera(
        mut steps: ResMut<CameraSteps>,
        mut camera: Query<&mut Transform, With<StreamingCamera>>,
    ) {
        let Ok(mut transform) = camera.single_mut() else {
            return;
        };
        if let Some(eye) = steps.0.pop_front() {
            transform.translation = eye;
        }
    }

    fn collect_crossings(mut crossings: ResMut<Crossings>, mut requests: MessageReader<CrossDoor>) {
        for request in requests.read() {
            crossings.0.push(request.door);
        }
    }

    fn collect_opened(mut opened: ResMut<Opened>, mut requests: MessageReader<OpenDoor>) {
        for request in requests.read() {
            opened.0.push(request.door);
        }
    }

    /// Walks the test camera along a path, one eye position per frame.
    fn walk_camera(app: &mut App, eye_positions: impl IntoIterator<Item = Vec3>) {
        for eye in eye_positions {
            app.world_mut()
                .resource_mut::<CameraSteps>()
                .0
                .push_back(eye);
            app.update();
        }
    }

    /// An interior cell root the isolation can classify, with nothing in it.
    fn spawn_cell_root(app: &mut App, cell_id: u32) -> Entity {
        app.world_mut()
            .spawn((
                StreamedCellRoot,
                CellRef(cell_id),
                StreamedCellKey(CellKey::Interior(cell_id)),
                Transform::default(),
                Visibility::default(),
            ))
            .id()
    }

    /// An interior cell root with a real floor in it, `y` units up in the render space every cell
    /// shares: something for the walking probe to ray-cast.
    fn spawn_floor_cell(app: &mut App, cell_id: u32, y: f32) -> Entity {
        let root = spawn_cell_root(app, cell_id);
        let floor = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Plane3d::default().mesh().size(4000.0, 4000.0));
        app.world_mut().spawn((
            Mesh3d(floor),
            Transform::from_xyz(0.0, y, 0.0),
            ChildOf(root),
        ));
        root
    }

    /// A load door of `cell`, standing at `position` in the render space the cells share, placed as
    /// transform propagation would have left it.
    fn spawn_cell_door(
        app: &mut App,
        cell: Entity,
        position: Vec3,
        auto_load: bool,
        open: bool,
    ) -> Entity {
        let mut door = exterior_door();
        door.auto_load = auto_load;
        let transform = Transform::from_translation(position);
        let mut entity = app.world_mut().spawn((
            transform,
            GlobalTransform::from(transform),
            Visibility::default(),
            door,
            ChildOf(cell),
        ));
        if open {
            entity.insert(DoorState::Open { animated: false });
        }
        entity.id()
    }

    /// The camera the player drives, with the [`Player`] `attach_player` gives it. Yaw zero looks
    /// along `-Z`, as `Player::look_rotation` has it.
    fn spawn_player_camera(app: &mut App, eye: Vec3, yaw: f32) -> Entity {
        app.world_mut()
            .spawn((
                Transform::from_translation(eye).with_rotation(Quat::from_rotation_y(yaw)),
                GlobalTransform::from_translation(eye),
                StreamingCamera,
                Player { yaw, ..default() },
            ))
            .id()
    }

    /// The portal's destination this frame - the cells [`update_portal`] publishes for the door it
    /// is rendering through, which the isolation then moves off the main camera's layers.
    fn portal_shows_cell(app: &mut App, key: CellKey) {
        app.world_mut().resource_mut::<PortalState>().destination = vec![key];
    }

    fn eye_of(app: &App, camera: Entity) -> Vec3 {
        app.world()
            .entity(camera)
            .get::<Transform>()
            .expect("the camera is still there")
            .translation
    }

    /// A destination cell's geometry does not hold the player up.
    ///
    /// The walking probe's ray cast is Bevy's, with `RayCastVisibility::Visible`: it tests inherited
    /// visibility, and the isolation leaves a destination cell visible - it only moves it onto the
    /// portal camera's layer, which no visibility query looks at. So the raw coordinates of an
    /// interior the portal is drawing through are in play for the player: a floor of that cell that
    /// happens to sit above this one's is nearer to the eye than the floor under the player's feet.
    #[test]
    fn a_destination_cells_floor_does_not_hold_the_player_up() {
        let mut app = player_app();
        spawn_floor_cell(&mut app, INTERIOR_ALFTAND01, 0.0);
        spawn_floor_cell(&mut app, FIXTURE_INTERIOR_CELL, 30.0);
        portal_shows_cell(&mut app, CellKey::Interior(FIXTURE_INTERIOR_CELL));
        let camera = spawn_player_camera(&mut app, eye_from_feet(Vec3::ZERO), 0.0);
        // The first frame is the isolation's; the walk that reads the roles it wrote is a later one.
        update(&mut app, 4);

        let eye = eye_of(&app, camera);
        assert!(
            (eye.y - EYE_HEIGHT).abs() < 0.5,
            "the player stands on the floor of the cell they are in, not on the destination's \
             floor thirty units above it: eye {eye:?}"
        );

        // The mesh is not merely somewhere else: the same cell made the *active* space - which is
        // what the player would find if the portal were drawing through a doorway into the other
        // one - holds them up thirty units higher.
        app.world_mut().resource_mut::<ActiveCell>().interior = Some(FIXTURE_INTERIOR_CELL);
        app.world_mut()
            .resource_mut::<PortalState>()
            .destination
            .clear();
        update(&mut app, 3);
        let eye = eye_of(&app, camera);
        assert!(
            (eye.y - (30.0 + EYE_HEIGHT)).abs() < 0.5,
            "the same floor holds the player up once it is the space they are in: eye {eye:?}"
        );
    }

    /// `E` opens the door of the room the player is in, not the door of a pre-streamed cell.
    ///
    /// A pre-streamed cell keeps its door components, and its doors are drawn nowhere near the
    /// space the player stands in - but they are still load doors in the same world, whose own
    /// coordinates can put one of them nearer to the camera than the door in the room, and nearer
    /// is what targeting takes. Here the cell is only pre-streamed, not the portal's destination
    /// (the other case the tests below cover), so nothing of it is drawn at all.
    #[test]
    fn a_prestreamed_cells_door_is_not_the_one_e_opens() {
        let mut app = player_app();
        let here = spawn_cell_root(&mut app, INTERIOR_ALFTAND01);
        let there = spawn_cell_root(&mut app, FIXTURE_INTERIOR_CELL);
        // The camera looks along `-Z`: the pre-streamed cell's door stands 100 units in front of
        // it and the room's own door 200, both straight ahead and inside the range and cone.
        let elsewhere = spawn_cell_door(
            &mut app,
            there,
            Vec3::new(1000.0, 120.0, 900.0),
            false,
            false,
        );
        let ours = spawn_cell_door(
            &mut app,
            here,
            Vec3::new(1000.0, 120.0, 800.0),
            false,
            false,
        );
        spawn_player_camera(&mut app, Vec3::new(1000.0, 120.0, 1000.0), 0.0);
        update(&mut app, 2);

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyE);
        update(&mut app, 1);
        assert_eq!(
            app.world().resource::<Opened>().0,
            vec![ours],
            "`E` opens the door of the room the player is in, not the pre-streamed cell's door \
             standing nearer to the camera ({elsewhere:?})"
        );
    }

    /// An auto-load marker of a pre-streamed cell crosses nothing, and a marker of the active space
    /// still takes the player - hidden or not.
    ///
    /// Skyrim's invisible `AutoLoadDoor01` markers fire on contact, which is why they are here: the
    /// marker belongs to a cell that is pre-streamed for a door the player may walk through, and
    /// the walk into it is *by coordinates* alone, so a marker anywhere in the world can be walked
    /// into. The second half is the control the test needs: an auto-load door's own model is hidden
    /// by design ([`show_load_door_leaves`] hides an auto-load door's leaf), so visibility is not
    /// what tells a foreign marker from the player's own.
    #[test]
    fn a_foreign_cells_auto_load_marker_crosses_nothing() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let walk = [
            400.0_f32, 300.0, 200.0, 100.0, 40.0, 20.0, 0.0, -20.0, -100.0, -400.0,
        ];
        let eyes = || {
            walk.into_iter()
                .map(|z| eye_from_feet(base + Vec3::new(0.0, 0.0, z)))
        };

        // The pre-streamed cell's marker, alone in the walk: the player walks through a door that
        // is not in the space they are in, and nothing happens.
        let mut app = player_app();
        let there = spawn_cell_root(&mut app, FIXTURE_INTERIOR_CELL);
        portal_shows_cell(&mut app, CellKey::Interior(FIXTURE_INTERIOR_CELL));
        spawn_cell_door(&mut app, there, base, true, false);
        spawn_player_camera(
            &mut app,
            eye_from_feet(base + Vec3::new(0.0, 0.0, 400.0)),
            0.0,
        );
        walk_camera(&mut app, eyes());
        assert!(
            app.world().resource::<Crossings>().0.is_empty(),
            "a marker of a cell the player is not in is not a door they walked into: {:?}",
            app.world().resource::<Crossings>().0
        );

        // The active space's own marker, hidden as an auto-load door's leaf always is: still walked
        // into.
        let mut app = player_app();
        let here = spawn_cell_root(&mut app, INTERIOR_ALFTAND01);
        let ours = spawn_cell_door(&mut app, here, base, true, false);
        app.world_mut().entity_mut(ours).insert(Visibility::Hidden);
        spawn_player_camera(
            &mut app,
            eye_from_feet(base + Vec3::new(0.0, 0.0, 400.0)),
            0.0,
        );
        walk_camera(&mut app, eyes());
        assert_eq!(
            app.world().resource::<Crossings>().0,
            vec![ours],
            "a hidden auto-load marker of the active space is still walked into"
        );
    }

    /// An open door of a pre-streamed cell is not walked through.
    ///
    /// The doorway trigger crosses the player where the doorway's own plane meets their walk, and a
    /// door the portal is drawing through has a doorway of the destination standing in the same
    /// world - at coordinates that can cross the walk the player is making in the room they are in.
    #[test]
    fn a_foreign_cells_open_door_is_not_walked_through() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        // A door with no outward direction of its own faces `-Z`, so this is the walk through it:
        // in from the front of its plane and out the other side.
        let walk = [-400.0_f32, -300.0, -100.0, -20.0, 0.0, 20.0, 100.0, 400.0];
        let eyes = || {
            walk.into_iter()
                .map(|z| eye_from_feet(base + Vec3::new(0.0, 0.0, z)))
        };

        let mut app = player_app();
        let there = spawn_cell_root(&mut app, FIXTURE_INTERIOR_CELL);
        portal_shows_cell(&mut app, CellKey::Interior(FIXTURE_INTERIOR_CELL));
        spawn_cell_door(&mut app, there, base, false, true);
        spawn_player_camera(
            &mut app,
            eye_from_feet(base + Vec3::new(0.0, 0.0, -400.0)),
            0.0,
        );
        walk_camera(&mut app, eyes());
        assert!(
            app.world().resource::<Crossings>().0.is_empty(),
            "the destination doorway is not a way out of the room the player is in: {:?}",
            app.world().resource::<Crossings>().0
        );

        // The same doorway, in the room the player is in, is what the walk is for.
        let mut app = player_app();
        let here = spawn_cell_root(&mut app, INTERIOR_ALFTAND01);
        let ours = spawn_cell_door(&mut app, here, base, false, true);
        spawn_player_camera(
            &mut app,
            eye_from_feet(base + Vec3::new(0.0, 0.0, -400.0)),
            0.0,
        );
        walk_camera(&mut app, eyes());
        assert_eq!(
            app.world().resource::<Crossings>().0,
            vec![ours],
            "the doorway of the room the player is in is walked through"
        );
    }

    /// research-197: the portal camera's mesh pipeline key has to be the main camera's, or every
    /// material the destination shows is compiled again the first time a doorway opens onto it.
    /// Bevy adds the tonemap and dither bits only to a view that is not HDR; the main camera is
    /// HDR (`app::setup_world`), so this one must be too - and still hand its image over
    /// untonemapped and undithered, which the main camera then does for it.
    #[test]
    fn the_portal_camera_renders_with_the_main_views_pipelines_and_leaves_tonemapping_to_it() {
        let SunApp {
            app, portal_camera, ..
        } = sun_app();
        let camera = app.world().entity(portal_camera);
        assert!(
            camera.contains::<Hdr>(),
            "an HDR view, like the main camera: no TONEMAP_IN_SHADER or DEBAND_DITHER in its key"
        );
        assert_eq!(
            camera.get::<Tonemapping>(),
            Some(&Tonemapping::None),
            "not tonemapped here: the main camera tonemaps the doorway with the room around it"
        );
        assert_eq!(
            camera.get::<DebandDither>(),
            Some(&DebandDither::Disabled),
            "not dithered here either"
        );
        let RenderTarget::Image(target) = camera.get::<RenderTarget>().expect("a render target")
        else {
            panic!("the portal camera renders into the portal texture");
        };
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&target.handle)
            .expect("the portal texture");
        assert_eq!(
            image.texture_descriptor.format,
            TextureFormat::Rgba16Float,
            "the target is the HDR view's own format, so the copy into it is exact"
        );
    }

    /// The first-open log line adds up the frames after a doorway opens: pipelines created since
    /// the open, the most waiting at once, and the frames over the hitch threshold.
    #[test]
    fn an_open_window_counts_the_pipelines_and_hitches_after_the_open() {
        let mut open = OpenWindow::new(0x0001_CBB0, 100);
        open.add_frame(16.0, 100, 0);
        open.add_frame(OPEN_HITCH_MS + 10.0, 120, 7);
        open.add_frame(30.0, 138, 2);
        open.add_frame(16.0, 138, 0);
        assert_eq!(open.frames, 4);
        assert_eq!(open.created(), 38);
        assert_eq!(open.max_waiting, 7);
        assert_eq!(open.frames_waiting, 2);
        assert_eq!(open.hitches, 2);
        assert_eq!(open.max_ms, OPEN_HITCH_MS + 10.0);
        assert_eq!(OpenWindow::new(1, 50).created(), 0);
    }

    /// Runs one frame of the occlusion decision for `door` with the sight lines `blocked` or not,
    /// and says whether the test ran and what was decided.
    fn occlusion_frame(
        occlusion: &mut DoorwayOcclusion,
        door: Entity,
        blocked: bool,
    ) -> (bool, bool) {
        let mut tested = false;
        let hidden = occlusion.update(door, || {
            tested = true;
            blocked
        });
        (tested, hidden)
    }

    #[test]
    fn a_doorway_in_view_is_hidden_only_after_several_blocked_tests_in_a_row() {
        let door = Entity::from_raw_u32(7).unwrap();
        let mut occlusion = DoorwayOcclusion::default();
        let mut frames = Vec::new();
        for _ in 0..(OCCLUSION_CONFIRM_TESTS * OCCLUSION_RETEST_FRAMES) {
            frames.push(occlusion_frame(&mut occlusion, door, true));
        }
        // Tested on the first frame and then every `OCCLUSION_RETEST_FRAMES` frames while drawn;
        // every frame once hidden.
        let tests: Vec<usize> = frames
            .iter()
            .take(((OCCLUSION_CONFIRM_TESTS - 1) * OCCLUSION_RETEST_FRAMES + 1) as usize)
            .enumerate()
            .filter_map(|(frame, (tested, _))| tested.then_some(frame))
            .collect();
        let expected: Vec<usize> = (0..OCCLUSION_CONFIRM_TESTS as usize)
            .map(|test| test * OCCLUSION_RETEST_FRAMES as usize)
            .collect();
        assert_eq!(tests, expected);
        // Drawn until the last of the confirming tests, hidden from that frame on.
        let first_hidden = frames.iter().position(|(_, hidden)| *hidden);
        assert_eq!(first_hidden, expected.last().copied());
        assert!(
            frames[first_hidden.unwrap()..]
                .iter()
                .all(|(tested, hidden)| *tested && *hidden)
        );
    }

    #[test]
    fn one_clear_test_resets_the_count_toward_hiding() {
        let door = Entity::from_raw_u32(7).unwrap();
        let mut occlusion = DoorwayOcclusion::default();
        let mut test = 0;
        for _ in 0..100 {
            // Blocked, blocked, clear, blocked, ...: never enough in a row.
            let blocked = test % OCCLUSION_CONFIRM_TESTS != OCCLUSION_CONFIRM_TESTS - 1;
            let (tested, hidden) = occlusion_frame(&mut occlusion, door, blocked);
            if tested {
                test += 1;
            }
            assert!(!hidden);
        }
    }

    #[test]
    fn a_hidden_doorway_is_tested_every_frame_and_drawn_the_frame_a_line_clears() {
        let door = Entity::from_raw_u32(7).unwrap();
        let mut occlusion = DoorwayOcclusion::default();
        while !occlusion_frame(&mut occlusion, door, true).1 {}
        for _ in 0..10 {
            assert_eq!(occlusion_frame(&mut occlusion, door, true), (true, true));
        }
        assert_eq!(occlusion_frame(&mut occlusion, door, false), (true, false));
        // Back in view, it takes the whole confirmation again to hide it.
        let mut frames = 0;
        while !occlusion_frame(&mut occlusion, door, true).1 {
            frames += 1;
        }
        assert!(frames >= ((OCCLUSION_CONFIRM_TESTS - 1) * OCCLUSION_RETEST_FRAMES) as usize);
    }

    #[test]
    fn another_door_or_a_forgotten_one_starts_in_view() {
        let (door, other) = (
            Entity::from_raw_u32(7).unwrap(),
            Entity::from_raw_u32(8).unwrap(),
        );
        let mut occlusion = DoorwayOcclusion::default();
        while !occlusion_frame(&mut occlusion, door, true).1 {}
        assert_eq!(occlusion_frame(&mut occlusion, other, true), (true, false));
        let mut occlusion = DoorwayOcclusion::default();
        while !occlusion_frame(&mut occlusion, door, true).1 {}
        occlusion.forget();
        assert_eq!(occlusion_frame(&mut occlusion, door, true), (true, false));
    }

    #[test]
    fn the_sight_line_grid_covers_the_doorway_and_its_margin_on_the_eyes_side() {
        // A doorway 200 wide and 300 high, facing +Z and facing -Z: the grid is on the eye's side
        // of either.
        let eye = Vec3::new(50.0, 20.0, 800.0);
        for rotation in [Quat::IDENTITY, Quat::from_rotation_y(PI)] {
            let quad = Transform::from_scale(Vec3::new(200.0, 300.0, 1.0)).with_rotation(rotation);
            let samples = doorway_occlusion_samples(&quad, eye);
            assert_eq!(samples.len(), OCCLUSION_GRID * OCCLUSION_GRID);
            for sample in samples {
                assert!((sample.z - OCCLUSION_STANDOFF).abs() < 1e-3, "{sample}");
            }
            let (low, high) = samples
                .iter()
                .fold((Vec3::MAX, Vec3::MIN), |(low, high), &sample| {
                    (low.min(sample), high.max(sample))
                });
            let reach = 0.5 + OCCLUSION_MARGIN;
            assert!((high.x - 200.0 * reach).abs() < 1e-3);
            assert!((low.x + 200.0 * reach).abs() < 1e-3);
            assert!((high.y - 300.0 * reach).abs() < 1e-3);
            assert!((low.y + 300.0 * reach).abs() < 1e-3);
        }
    }

    /// A box-shaped occluder of `size` standing at `centre`.
    fn box_occluder(mesh: &Mesh, centre: Vec3, size: Vec3) -> Occluder<'_> {
        Occluder {
            transform: Affine3A::from_scale_rotation_translation(size, Quat::IDENTITY, centre),
            aabb: Aabb3d::new(Vec3::ZERO, Vec3::splat(0.5)),
            mesh,
        }
    }

    #[test]
    fn a_wall_between_the_eye_and_the_doorway_blocks_every_sight_line() {
        let mesh = Mesh::from(Cuboid::new(1.0, 1.0, 1.0));
        let quad = Transform::from_scale(Vec3::new(200.0, 300.0, 1.0));
        let eye = Vec3::new(0.0, 0.0, 1000.0);
        let samples = doorway_occlusion_samples(&quad, eye);
        let halfway = Vec3::new(0.0, 0.0, 500.0);

        // A wall halfway, wider and taller than the grid's cone at that depth.
        let wall = box_occluder(&mesh, halfway, Vec3::new(600.0, 600.0, 20.0));
        assert!(sight_lines_blocked(eye, &samples, &[wall]));

        // Nothing in the way, or a wall behind the doorway: every line is open.
        assert!(!sight_lines_blocked(eye, &samples, &[]));
        let behind = box_occluder(
            &mesh,
            Vec3::new(0.0, 0.0, -200.0),
            Vec3::new(600.0, 600.0, 20.0),
        );
        assert!(!sight_lines_blocked(eye, &samples, &[behind]));

        // A post in front of the middle of the doorway hides some of it, not all.
        let post = box_occluder(&mesh, halfway, Vec3::new(40.0, 1000.0, 40.0));
        assert!(!sight_lines_blocked(eye, &samples, &[post]));

        // A wall covering the doorway but not its margin leaves the outer lines open: the doorway
        // is about to come out from behind it.
        let narrow = box_occluder(&mesh, halfway, Vec3::new(130.0, 180.0, 20.0));
        assert!(!sight_lines_blocked(eye, &samples, &[narrow]));
    }

    #[test]
    fn a_wall_seen_from_behind_hides_nothing() {
        let quad = Transform::from_scale(Vec3::new(200.0, 300.0, 1.0));
        let eye = Vec3::new(0.0, 0.0, 1000.0);
        let samples = doorway_occlusion_samples(&quad, eye);
        let wall = |mesh| Occluder {
            transform: Affine3A::from_scale_rotation_translation(
                Vec3::splat(1000.0),
                Quat::IDENTITY,
                Vec3::new(0.0, 0.0, 500.0),
            ),
            aabb: Aabb3d::new(Vec3::ZERO, Vec3::new(0.5, 0.5, 0.01)),
            mesh,
        };
        // A single-sided sheet turned away from the eye is a back face the renderer culls; turned
        // toward it, it hides everything behind it.
        let away = Mesh::from(Plane3d::new(Vec3::NEG_Z, Vec2::splat(0.5)));
        assert!(!sight_lines_blocked(eye, &samples, &[wall(&away)]));
        let toward = Mesh::from(Plane3d::new(Vec3::Z, Vec2::splat(0.5)));
        assert!(sight_lines_blocked(eye, &samples, &[wall(&toward)]));
    }

    // --- impl-225: the destination hold ---

    fn held_door(app: &mut App) -> Entity {
        app.world_mut().spawn_empty().id()
    }

    #[test]
    fn a_destination_is_held_while_the_doorway_is_out_of_view_and_let_go_after() {
        let mut app = App::new();
        let door = held_door(&mut app);
        let keys = vec![CellKey::Interior(INTERIOR_ALFTAND01)];
        let mut state = PortalState::default();
        state.draw_destination(door, keys.clone());
        assert_eq!(state.destination, keys);

        // The player turns away (or steps behind the door's plane) at t = 10 s.
        state.lose_destination(Some(10.0), |_| true);
        assert_eq!(state.destination, keys, "kept the first frame");
        state.lose_destination(Some(10.0 + DESTINATION_HOLD_SECONDS - 0.1), |_| true);
        assert_eq!(state.destination, keys, "kept for the whole hold");

        // And turns back: drawn again, the hold starts over from the next time it is lost.
        state.draw_destination(door, keys.clone());
        state.lose_destination(Some(20.0), |_| true);
        state.lose_destination(Some(20.0 + DESTINATION_HOLD_SECONDS - 0.1), |_| true);
        assert_eq!(
            state.destination, keys,
            "a fresh hold after the doorway was drawn again"
        );

        state.lose_destination(Some(20.0 + DESTINATION_HOLD_SECONDS), |_| true);
        assert!(
            state.destination.is_empty(),
            "let go once the hold has run out"
        );
        assert!(state.hold.is_none());
        state.lose_destination(Some(20.5), |_| true);
        assert!(state.destination.is_empty(), "and not picked up again");
    }

    #[test]
    fn a_door_that_closes_ends_the_hold_at_once() {
        let mut app = App::new();
        let door = held_door(&mut app);
        let keys = vec![CellKey::Interior(INTERIOR_ALFTAND01)];
        let mut state = PortalState::default();
        state.draw_destination(door, keys.clone());
        state.lose_destination(Some(1.0), |_| true);
        assert_eq!(state.destination, keys);
        // The door shut (or its far side unloaded, or the player crossed): no way through any more.
        state.lose_destination(Some(1.1), |held| held != door);
        assert!(state.destination.is_empty());
        assert!(state.hold.is_none());

        // Straight from drawing, too: a door that closes in the frame it is lost holds nothing.
        state.draw_destination(door, keys);
        state.lose_destination(Some(2.0), |_| false);
        assert!(state.destination.is_empty());
    }

    #[test]
    fn another_doors_destination_replaces_a_held_one_at_once() {
        let mut app = App::new();
        let (first, second) = (held_door(&mut app), held_door(&mut app));
        let (first_keys, second_keys) = (
            vec![CellKey::Interior(INTERIOR_ALFTAND01)],
            vec![CellKey::Interior(0x0005_6C1B)],
        );
        let mut state = PortalState::default();
        state.draw_destination(first, first_keys.clone());
        state.lose_destination(Some(1.0), |_| true);
        assert_eq!(state.destination, first_keys);
        state.draw_destination(second, second_keys.clone());
        assert_eq!(
            state.destination, second_keys,
            "the new destination at once"
        );
        // Losing the new one holds the new one - and asks about that door, not the old one.
        state.lose_destination(Some(1.5), |held| held == second);
        assert_eq!(state.destination, second_keys);
    }

    #[test]
    fn a_run_without_a_clock_holds_no_destination() {
        let mut app = App::new();
        let door = held_door(&mut app);
        let mut state = PortalState::default();
        state.draw_destination(door, vec![CellKey::Interior(INTERIOR_ALFTAND01)]);
        state.lose_destination(None, |_| true);
        assert!(state.destination.is_empty());
    }

    #[test]
    fn a_held_destination_is_not_re_layered_when_the_doorway_comes_back() {
        let mut app = portal_app();
        spawn_camera(&mut app, Vec3::ZERO);
        let (_, mesh) = spawn_cell(&mut app, 0x0005_6C1B, None, None, None);
        let door = held_door(&mut app);
        let keys = vec![CellKey::Interior(0x0005_6C1B)];
        app.world_mut()
            .resource_mut::<PortalState>()
            .draw_destination(door, keys.clone());
        update(&mut app, 2);
        let destination = RenderLayers::layer(DESTINATION_LAYER);
        assert_eq!(layers_of(&app, mesh), destination);
        let walks = |app: &App| app.world().resource::<PortalState>().churn.relayer_walks;
        let before = walks(&app);

        // Out of view for most of the hold, and back: the cell keeps its role throughout.
        for time in [0.0, 0.5, 1.0, 1.5] {
            app.world_mut()
                .resource_mut::<PortalState>()
                .lose_destination(Some(time), |_| true);
            update(&mut app, 1);
            assert_eq!(layers_of(&app, mesh), destination, "held at {time} s");
        }
        app.world_mut()
            .resource_mut::<PortalState>()
            .draw_destination(door, keys);
        update(&mut app, 1);
        assert_eq!(
            walks(&app),
            before,
            "turning away and back re-layered nothing"
        );

        // Lost for longer than the hold: the cell is hidden, in one walk.
        for time in [10.0, 10.0 + DESTINATION_HOLD_SECONDS] {
            app.world_mut()
                .resource_mut::<PortalState>()
                .lose_destination(Some(time), |_| true);
            update(&mut app, 1);
        }
        assert_eq!(layers_of(&app, mesh), RenderLayers::none());
        assert_eq!(walks(&app), before + 1);
    }

    #[test]
    fn a_cell_that_comes_back_with_a_new_root_is_counted_as_a_respawn() {
        let mut app = portal_app();
        spawn_camera(&mut app, Vec3::ZERO);
        let (root, _) = spawn_cell(&mut app, 0x0005_6C1B, None, None, None);
        update(&mut app, 2);
        let respawns = |app: &App| app.world().resource::<PortalState>().churn.respawns;
        assert_eq!(respawns(&app), 0, "a first spawn is not a respawn");
        app.world_mut().entity_mut(root).despawn();
        update(&mut app, 1);
        spawn_cell(&mut app, 0x0005_6C1B, None, None, None);
        update(&mut app, 1);
        assert_eq!(respawns(&app), 1);
    }
}
