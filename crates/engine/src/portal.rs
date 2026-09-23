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
//! # The doorway image
//!
//! The window is a render target of the portal camera, sampled by the quad at the screen position
//! of each of its fragments, and it is made to be indistinguishable from the room behind it:
//!
//! * it is the size of the main camera's own target ([`resize_portal_target`], so the doorway has
//!   the same pixel density as the room around it, resize for resize);
//! * it carries the destination's scene-referred light rather than a clipped 8-bit copy of it
//!   ([`PORTAL_TEXTURE_FORMAT`]), so the main camera's tonemapper and bloom finish the doorway
//!   exactly as they finish the room - walked through, the same surface reads the same.
//!
//! Both are the *camera's* alone: the quad's material is unlit and does not tonemap what it
//! samples, because the frame it is composited into is tonemapped once, by the main camera.
//!
//! # Wiring
//!
//! `app.run` adds `PortalPlugin` for interactive runs, after `StreamingPlugin` (it needs
//! `ActiveCell`, `EngineConfig`, `RenderOrigin` and `StreamingWorld`). Nothing else changes: the
//! destination cells are moved off the main camera's layers rather than the camera being granted a
//! new one, and the portal camera, its render target and the quad are all spawned here.
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
//! One portal at a time (the nearest door). The destination view is lit and cleared by the global
//! atmosphere of the space the player is in, not by the destination's own. Water surfaces of a
//! pre-streamed cell sample the main camera's reflection texture, which does not show the
//! destination.

use crate::{
    config::EngineConfig,
    doors::{DoorDestination, DoorState, LoadDoor},
    streaming::{ActiveCell, RenderOrigin, StreamingWorld},
    transition::{
        CrossingHeld, DOOR_PRESTREAM_RADIUS, arrival_frame, destination_is_resident,
        destination_keys, distance_in_front_of_door, door_frame, door_is_open, portal_pose,
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
    asset::embedded_asset,
    camera::{ClearColorConfig, RenderTarget, visibility::RenderLayers},
    core_pipeline::{prepass::DepthPrepass, tonemapping::Tonemapping},
    pbr::{ExtendedMaterial, MaterialExtension, MaterialPlugin},
    prelude::*,
    render::{
        occlusion_culling::OcclusionCulling,
        render_resource::{AsBindGroup, TextureFormat},
    },
    shader::ShaderRef,
};
use std::{collections::HashMap, f32::consts::PI};

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

/// Registers the portal shader and the systems that isolate cells, close load doors and render the
/// destination through the nearest doorway.
///
/// Add it for interactive runs, after [`StreamingPlugin`](crate::streaming::StreamingPlugin).
pub struct PortalPlugin;

impl Plugin for PortalPlugin {
    fn build(&self, app: &mut App) {
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
                    // A crossing changes `ActiveCell` in this set, and the cell just entered has to
                    // be visible in the frame it is entered in.
                    .after(crate::transition::DoorTransition),
            );
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
/// The plane is the destination's side of the doorway: it passes through the arrival point with the
/// arrival facing as its normal, which is the image of the source door's plane under the mapping.
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
    doors: impl IntoIterator<Item = (Entity, Vec3, &'a LoadDoor, Option<&'a DoorState>)>,
    destination_is_resident: impl Fn(&DoorDestination) -> bool,
    active: &ActiveSpace,
    distance_in_front: impl Fn(Entity) -> f32,
) -> Option<Entity> {
    let mut best: Option<(Entity, f32)> = None;
    for (entity, position, door, state) in doors {
        let distance = position.distance(camera);
        if distance > DOOR_PRESTREAM_RADIUS {
            continue;
        }
        if !door_is_open(state) {
            continue;
        }
        if !destination_is_resident(&door.destination) {
            continue;
        }
        // The destination is already drawn in the main view: nothing to look into.
        if destination_keys(&door.destination)
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
}

/// Gives the portal camera the atmosphere of the space behind the doorway it is showing.
///
/// The room on the far side of a door is lit and fogged by its own records: Alftand01's fog is
/// `(153, 210, 238)` and reaches 9,000 units, while the Tamriel it is entered from is fogged by the
/// terrain ring at a completely different distance, and Blackreach's backdrop is teal where the
/// Alftand cavern it opens off is not. Rendering the destination with the *source's* atmosphere is
/// what made a doorway look like a hole into the room the player is already standing in
/// (`docs/research/visual-gaps-spec.md`, gap 2).
///
/// **What could not be per camera: the sun.** The engine has one [`DirectionalLight`] entity for
/// the whole world, and every view of a frame is lit by it, so the doorway of an interior seen from
/// an exterior is still in the exterior's sun. The destination's *own* sun is what the engine keeps
/// for the space it is standing in - which is the case that matters, since a player inside a
/// doorway is either in the source space or the destination one, never in both. The ambient, the
/// fog and the clear colour are the three that can differ, and they are the three that carry most
/// of a space's look.
fn update_destination_atmosphere(
    state: Res<PortalState>,
    catalog: Option<Res<SpaceLightingCatalog>>,
    config: Option<Res<EngineConfig>>,
    doors: Query<&LoadDoor>,
    mut camera: Query<(&mut Camera, &mut AmbientLight, &mut DistanceFog), With<PortalCamera>>,
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
    // Written on a change of destination and not every frame: the three components are read through
    // change detection, and a doorway standing open for a minute would otherwise mark a camera and
    // its view changed sixty times a second for no reason. Nothing else moves them - the catalog is
    // read once at startup and the radii are the run's.
    if *applied == Some(destination) {
        return;
    }
    *applied = Some(destination);
    let atmosphere = crate::app::space_atmosphere(catalog.as_deref(), destination);
    camera.clear_color = ClearColorConfig::Custom(atmosphere.backdrop);
    *ambient = crate::app::ambient_light(&atmosphere);
    *fog = crate::app::atmosphere_fog(&atmosphere, config.stream_radius, config.terrain_radius);
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
///
/// An **animated** door is none of these, and never becomes one. Its frame *is* the doorway, and
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
        || (portal_shows_this_door && !animated);
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

    // How far in front of each door the camera is, along the direction that door faces. Its door
    // frame is built here as [`update_portal`] builds it for the door it picks, and the number is
    // the same one its projection clips at ([`distance_in_front_of_door`]).
    let distance_in_front = |entity: Entity| -> f32 {
        let Ok((_, global, _, door, ..)) = doors.get(entity) else {
            return f32::NEG_INFINITY;
        };
        distance_in_front_of_door(
            global.translation(),
            door_frame(global.rotation(), door.outward),
            camera_position,
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
        .map(|(entity, global, _, door, door_state, ..)| {
            (entity, global.translation(), door, door_state)
        });
    let target = select_portal_door(
        camera_position,
        candidates,
        |destination| destination_is_resident(destination, &streaming),
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

    let Ok((_, global, local, door, _, instance_bounds, expected_bounds)) = doors.get(target)
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
    if *shown != Some(door.ref_id) {
        *shown = Some(door.ref_id);
        info!(door = format_args!("{:08X}", door.ref_id), destination = %door.label.trim_end_matches(['\0', ' ']), "portal: looking through a load door");
    }
    let door_position = global.translation();
    let door_rotation = global.rotation();
    let frame = door_frame(door_rotation, door.outward);
    let front = frame * Vec3::NEG_Z;
    let (arrival_position, arrival_rotation) = arrival_frame(&door.destination, origin.0);
    let (portal_position, portal_rotation) = portal_pose(
        door_position,
        frame,
        arrival_position,
        arrival_rotation,
        camera_position,
        camera_rotation,
    );
    let clip_plane = doorway_clip_plane(
        portal_position,
        portal_rotation,
        arrival_position,
        arrival_rotation * Vec3::NEG_Z,
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

    state.destination = destination_keys(&door.destination);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        streaming::{creation_rotation_to_bevy, creation_to_bevy, render_position},
        transition::door_to_arrival_rotation,
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
        let candidates = [(portal_camera, door_position, &door, Some(&open))];
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
                |_| true,
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
                [(portal_camera, door_position, &model_only, Some(&open))],
                |_| true,
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
            destination_keys(&interior_door(INTERIOR_ALFTAND01).destination),
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
            &streaming
        ));
        assert!(!destination_is_resident(
            &exterior_door().destination,
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
            (near_door, Vec3::new(0.0, 0.0, -300.0), &near, Some(&open)),
            (far_door, Vec3::new(0.0, 0.0, -700.0), &far, Some(&open)),
            (
                unready_door,
                Vec3::new(0.0, 0.0, -50.0),
                &unready,
                Some(&open),
            ),
            (
                behind_door,
                Vec3::new(0.0, 0.0, -10.0),
                &outside,
                Some(&open),
            ),
        ];
        let ready = |destination: &DoorDestination| destination.interior_cell_id != Some(4);
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
    /// this door, it is holding a crossing, its model is drawn, what the row is).
    type DoorCase = (Option<DoorState>, bool, bool, bool, bool, &'static str);

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
        let cases: [DoorCase; 14] = [
            (
                None,
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
                true,
                "a closed door draws its leaf",
            ),
            (
                Some(Closed),
                false,
                true,
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
                true,
                "a door mid-swing draws its leaf",
            ),
            (
                Some(Opening),
                false,
                true,
                false,
                true,
                "THE REGRESSED CASE: an animating door the portal renders through stays drawn",
            ),
            (
                Some(Closing),
                false,
                true,
                false,
                true,
                "a door closing behind the player is drawn coming back",
            ),
            (
                Some(Open { animated: true }),
                false,
                true,
                false,
                true,
                "an animated open door the portal renders through stays drawn",
            ),
            (
                Some(Open { animated: true }),
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
                "a door with no clip has no leaf that can move: its model is the hole",
            ),
            (
                Some(Open { animated: false }),
                false,
                true,
                false,
                false,
                "including the door the portal is rendering through",
            ),
            (
                Some(Open { animated: false }),
                false,
                true,
                true,
                true,
                "unless its crossing is held: there is no window to be a hole in then",
            ),
            (
                Some(Closing),
                false,
                false,
                true,
                true,
                "and a held crossing draws whatever the state is",
            ),
            (
                Some(Open { animated: false }),
                true,
                true,
                false,
                false,
                "an auto-load marker is an invisible reference, portal or not",
            ),
            (
                Some(Open { animated: false }),
                true,
                true,
                true,
                true,
                "and it is drawn while its crossing is held, like any other door",
            ),
        ];

        let mut app = portal_app();
        let mut spawned = Vec::new();
        for (state, auto_load, _, waiting, _, _) in cases.iter() {
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
        for ((_, _, portal, _, drawn, what), (entity, mesh)) in cases.iter().zip(&spawned) {
            if *portal {
                continue;
            }
            assert_eq!(visibility_of(&app, *entity), wanted(*drawn), "{what}");
            assert_eq!(leaf_is_drawn(&app, *mesh), *drawn, "{what}");
        }

        // And with the portal rendering through each of its own doors in turn.
        for ((_, _, portal, _, drawn, what), (entity, mesh)) in cases.iter().zip(&spawned) {
            if !*portal {
                continue;
            }
            portal_shows(&mut app, Some(*entity));
            update(&mut app, 1);
            assert_eq!(visibility_of(&app, *entity), wanted(*drawn), "{what}");
            assert_eq!(leaf_is_drawn(&app, *mesh), *drawn, "{what}");
        }
    }
}
