//! Portals through load doors: pre-streamed destination cells are kept off the main camera's
//! layers, load doors stop drawing their leaf, and the destination behind the nearest door is
//! rendered through the doorway as a window. See `tasks/deepseek/impl-009-door-portals.md`.
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
//! # What the lead wires
//!
//! `app.run` adds `PortalPlugin` for interactive runs, after `StreamingPlugin` (it needs
//! `ActiveCell`, `EngineConfig`, `RenderOrigin` and `StreamingWorld`), e.g. inside the
//! `if walk || demo_tour.is_some()` block next to the atmosphere systems. Nothing else changes: the
//! destination cells are moved off the main camera's layers rather than the camera being granted a
//! new one, and the portal camera, its render target and the quad are all spawned here.
//!
//! `streaming::spawn_cell` should insert [`StreamedCellKey`] on the root it returns:
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
    doors::{DoorDestination, LoadDoor},
    streaming::{ActiveCell, RenderOrigin, StreamingWorld, creation_to_bevy, render_position},
    transition::DOOR_PRESTREAM_RADIUS,
    world::{
        components::{
            CELL_SIZE, CellRef, ExpectedModelBounds, ExteriorCellGrid, InstanceBounds,
            StreamedCellRoot, StreamingCamera,
        },
        database::CellKey,
    },
};
use bevy::{
    asset::embedded_asset,
    camera::{RenderTarget, visibility::RenderLayers},
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

/// The portal render target. `water.wgsl` samples its reflection texture through screen-space UVs
/// at this size; the portal does the same, and because both cameras carry the same projection the
/// scale cancels in the normalized coordinates a quad fragment samples.
const PORTAL_TEXTURE_WIDTH: u32 = 1024;
const PORTAL_TEXTURE_HEIGHT: u32 = 576;

/// Mirrors `transition::DOOR_PRESTREAM_GRID_RADIUS` (private there): an exterior destination is the
/// grid of this radius around its arrival point.
const DOOR_PRESTREAM_GRID_RADIUS: i32 = 1;

/// The doorway a door without converted bounds gets, in Creation units.
const DEFAULT_PORTAL_SIZE: Vec2 = Vec2::new(200.0, 300.0);

/// A doorway smaller than this on either axis is treated as a missing measurement.
const MIN_PORTAL_SIZE: f32 = 8.0;

/// How far in front of the door reference the quad sits, so it does not z-fight with the door's
/// own frame.
const PORTAL_QUAD_OFFSET: f32 = 8.0;

/// A camera nearer than this to a door's plane - inside it, or behind it - has no portal through
/// it: the doorway's clip plane would pass through the eye, where the window has no content.
const MIN_PORTAL_DOOR_DISTANCE: f32 = 1.0;

/// Registers the portal shader and the systems that isolate cells, hide doors and render the
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
                    hide_load_door_meshes,
                    // The portal picks its door from the roles of the previous frame and publishes
                    // the destination cells; the isolation below reveals them in this same frame,
                    // which is what the roles would otherwise need the next frame for.
                    update_portal,
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

/// The quad in the doorway that shows the portal camera's image.
#[derive(Component)]
struct PortalQuad;

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

/// The roles of the current frame and the destination cells of the current portal.
#[derive(Resource, Default)]
struct PortalState {
    roles: HashMap<Entity, CellRole>,
    destination: Vec<CellKey>,
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
// The door -> arrival mapping
// ---------------------------------------------------------------------------------------------

/// The render-space pose a crossing puts the camera at, exactly as
/// `transition::apply_door_crossings` converts the `XTEL` arrival frame: an interior is at its
/// absolute creation coordinates, an exterior relative to the render origin.
fn arrival_frame(destination: &DoorDestination, origin: IVec2) -> (Vec3, Quat) {
    let arrival = Vec3::from_array(destination.arrival_position);
    let position = if destination.interior_cell_id.is_some() {
        creation_to_bevy(arrival)
    } else {
        render_position(arrival, origin)
    };
    (
        position,
        crate::transition::arrival_camera_rotation(destination.arrival_rotation),
    )
}

/// The rotation that carries the source door's frame onto the arrival frame.
///
/// Both frames use the same convention: their forward (`-Z`) is the direction the frame faces - for
/// the door the side the player walks in from, for the arrival frame the way the arriving player
/// faces (`transition::apply_door_crossings` sets the camera's rotation from it). A player standing
/// in front of the door maps to the same distance on the far side of the arrival point, and a view
/// aimed at the door maps to a view aimed along the arrival facing, which is the direction the
/// crossing sends the camera:
///
/// ```text
/// M = T(arrival) * R(arrival) * Y180 * R(door)^-1 * T(-door)
/// ```
fn door_to_arrival_rotation(door_rotation: Quat, arrival_rotation: Quat) -> Quat {
    arrival_rotation * Quat::from_rotation_y(PI) * door_rotation.inverse()
}

/// The portal camera's pose: the main camera's pose carried through [`door_to_arrival_rotation`].
///
/// A camera at the door's own position lands on the arrival point (`M(door) = arrival`), so
/// pressing E where the portal showed the destination lands the player on the pose the portal was
/// rendering from; walking in maps to the view travelling into the destination room at the same
/// speed.
fn portal_pose(
    door_position: Vec3,
    door_rotation: Quat,
    arrival_position: Vec3,
    arrival_rotation: Quat,
    camera_position: Vec3,
    camera_rotation: Quat,
) -> (Vec3, Quat) {
    let map = door_to_arrival_rotation(door_rotation, arrival_rotation);
    (
        arrival_position + map * (camera_position - door_position),
        map * camera_rotation,
    )
}

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

/// The cells a destination is made of: the interior, or the grid around an exterior arrival point
/// that `transition::plan_door_prestream` streams.
fn destination_keys(destination: &DoorDestination) -> Vec<CellKey> {
    if let Some(cell_id) = destination.interior_cell_id {
        return vec![CellKey::Interior(cell_id)];
    }
    let Some(worldspace_id) = destination.worldspace_id else {
        return Vec::new();
    };
    let arrival = creation_to_bevy(Vec3::from_array(destination.arrival_position));
    let grid = IVec2::new(
        (arrival.x / CELL_SIZE).floor() as i32,
        (-arrival.z / CELL_SIZE).floor() as i32,
    );
    let mut keys = Vec::with_capacity(((DOOR_PRESTREAM_GRID_RADIUS * 2 + 1).pow(2)) as usize);
    for y in -DOOR_PRESTREAM_GRID_RADIUS..=DOOR_PRESTREAM_GRID_RADIUS {
        for x in -DOOR_PRESTREAM_GRID_RADIUS..=DOOR_PRESTREAM_GRID_RADIUS {
            keys.push(CellKey::Exterior {
                worldspace_id,
                grid_x: grid.x + x,
                grid_y: grid.y + y,
            });
        }
    }
    keys
}

/// Whether a destination is streamed in and able to be rendered through the doorway.
///
/// An exterior destination is resident once the cell of its arrival point is: that is the cell the
/// camera lands in, and the rest of the grid streams with it.
fn destination_is_resident(destination: &DoorDestination, streaming: &StreamingWorld) -> bool {
    match destination_keys(destination).first() {
        Some(key) => streaming.is_resident(key),
        None => false,
    }
}

/// The doorway the quad covers: its size and its centre in the door's own frame.
///
/// A door reference carries the converted model's bounds in model space
/// ([`ExpectedModelBounds`], which is what the converter measured in the door's own frame) and, in
/// world space, as the [`InstanceBounds`] of the placed reference. The model-space box is exact
/// after the reference's scale, so it is preferred; the world-space box is axis-aligned in world
/// space rather than in the door's frame, so rotating it back can only over-estimate a door placed
/// at an angle - a doorway that is too large is hidden by the wall around it, while a doorway that
/// is too small would leave a hole. The doorway's width is the local `X` and its height the local
/// `Y`; a door with no usable bounds at all (the invisible `AutoLoadDoor01` markers among them)
/// gets [`DEFAULT_PORTAL_SIZE`] standing on the reference's origin.
fn portal_quad_extents(
    instance_bounds: Option<&InstanceBounds>,
    expected_bounds: Option<&ExpectedModelBounds>,
    door_rotation: Quat,
    scale: Vec3,
) -> (Vec2, Vec3) {
    measured_portal_extents(instance_bounds, expected_bounds, door_rotation, scale).unwrap_or((
        DEFAULT_PORTAL_SIZE,
        Vec3::new(0.0, DEFAULT_PORTAL_SIZE.y * 0.5, 0.0),
    ))
}

/// The doorway's size and centre in the door's own frame, or `None` when the base has no usable
/// bounds at all - the invisible `AutoLoadDoor01` markers among them, which is why their doorway
/// falls back to [`DEFAULT_PORTAL_SIZE`].
///
/// This is the one place that decides how big a door's doorway is, from the same two sources:
/// the converted model's bounds (model space, scaled by the reference) and, failing those, the
/// placed reference's [`InstanceBounds`] turned back into the door's frame. The auto-load trigger
/// volume in [`crate::player`] measures itself with this function too, so "walking into the door"
/// means the same doorway the portal draws through.
pub(crate) fn measured_portal_extents(
    instance_bounds: Option<&InstanceBounds>,
    expected_bounds: Option<&ExpectedModelBounds>,
    door_rotation: Quat,
    scale: Vec3,
) -> Option<(Vec2, Vec3)> {
    let extents = |min: Vec3, max: Vec3| {
        let size = (max - min).abs();
        ((size.x, size.y), (min + max) * 0.5)
    };
    let measured = match (expected_bounds, instance_bounds) {
        (Some(bounds), _) => {
            let scaled = |v: Vec3| Vec3::new(v.x * scale.x, v.y * scale.y, v.z * scale.z);
            Some(extents(scaled(bounds.min), scaled(bounds.max)))
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
            Some(extents(min, max))
        }
        (None, None) => None,
    };
    match measured {
        Some(((width, height), centre))
            if width.is_finite()
                && height.is_finite()
                && centre.is_finite()
                && width >= MIN_PORTAL_SIZE
                && height >= MIN_PORTAL_SIZE =>
        {
            Some((Vec2::new(width, height), centre))
        }
        _ => None,
    }
}

/// The door the portal renders through: the nearest one in the active space whose destination is
/// resident and not itself part of the active space, and whose plane the camera is on the front
/// side of (a [`doorway_clip_plane`] distance of at least [`MIN_PORTAL_DOOR_DISTANCE`]).
fn select_portal_door<'a>(
    camera: Vec3,
    doors: impl IntoIterator<Item = (Entity, Vec3, &'a LoadDoor)>,
    destination_is_resident: impl Fn(&DoorDestination) -> bool,
    active: &ActiveSpace,
    distance_in_front: impl Fn(Entity) -> f32,
) -> Option<Entity> {
    let mut best: Option<(Entity, f32)> = None;
    for (entity, position, door) in doors {
        let distance = position.distance(camera);
        if distance > DOOR_PRESTREAM_RADIUS {
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

/// A load door reference with everything that places its doorway.
type LoadDoorQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static GlobalTransform,
        &'static Transform,
        &'static LoadDoor,
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

fn setup_portal_camera(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let image = images.add(Image::new_target_texture(
        PORTAL_TEXTURE_WIDTH,
        PORTAL_TEXTURE_HEIGHT,
        TextureFormat::Rgba8Unorm,
        Some(TextureFormat::Rgba8UnormSrgb),
    ));
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
        // tonemapping it here as well would darken the doorway against the room around it.
        Tonemapping::None,
        RenderTarget::Image(image.clone().into()),
        Projection::Perspective(PerspectiveProjection::default()),
        Transform::default(),
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::layer(DESTINATION_LAYER),
        PortalCamera,
    ));
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

/// Opens every load door: a reference with a `LoadDoor` stops drawing its meshes, so the doorway is
/// an opening rather than a closed leaf. `Visibility` is inherited, so this covers the meshes of
/// the glTF scene that the asset loader spawns under the root a frame or more later.
fn hide_load_door_meshes(
    mut commands: Commands,
    doors: Query<(Entity, &Visibility), With<LoadDoor>>,
) {
    for (door, visibility) in &doors {
        if !matches!(visibility, Visibility::Hidden) {
            commands.entity(door).insert(Visibility::Hidden);
        }
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
                        .insert((PortalHiddenCell(original), Visibility::Hidden));
                }
            }
            CellRole::Active | CellRole::Destination => {
                if let Some(hidden) = hidden {
                    commands.entity(root).insert(hidden.0);
                    commands.entity(root).remove::<PortalHiddenCell>();
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
                            commands
                                .entity(entity)
                                .insert(PortalOriginalLayers(current.cloned().unwrap_or_default()));
                        }
                        commands.entity(entity).insert(wanted.clone());
                    }
                }
                None => {
                    if let Some(original) = original {
                        commands.entity(entity).insert(original.0.clone());
                        commands.entity(entity).remove::<PortalOriginalLayers>();
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

    // The clip plane's distance is what decides whether the camera is on the side the door faces,
    // so it is the same number that goes into the projection below.
    let distance_in_front = |entity: Entity| -> f32 {
        let Ok((_, global, _, door, ..)) = doors.get(entity) else {
            return f32::NEG_INFINITY;
        };
        let (arrival_position, arrival_rotation) = arrival_frame(&door.destination, origin.0);
        let (portal_position, portal_rotation) = portal_pose(
            global.translation(),
            global.rotation(),
            arrival_position,
            arrival_rotation,
            camera_position,
            camera_rotation,
        );
        -doorway_clip_plane(
            portal_position,
            portal_rotation,
            arrival_position,
            arrival_rotation * Vec3::NEG_Z,
        )
        .w
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
        .map(|(entity, global, _, door, ..)| (entity, global.translation(), door));
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

    let Ok((_, global, local, door, instance_bounds, expected_bounds)) = doors.get(target) else {
        state.destination.clear();
        *quad_visibility = Visibility::Hidden;
        camera.is_active = false;
        return;
    };
    if *shown != Some(door.ref_id) {
        *shown = Some(door.ref_id);
        info!(door = format_args!("{:08X}", door.ref_id), destination = %door.label.trim_end_matches(['\0', ' ']), "portal: looking through a load door");
    }
    let door_position = global.translation();
    let door_rotation = global.rotation();
    let (arrival_position, arrival_rotation) = arrival_frame(&door.destination, origin.0);
    let (portal_position, portal_rotation) = portal_pose(
        door_position,
        door_rotation,
        arrival_position,
        arrival_rotation,
        camera_position,
        camera_rotation,
    );
    let front = door_rotation * Vec3::NEG_Z;
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

    let (size, centre) =
        portal_quad_extents(instance_bounds, expected_bounds, door_rotation, local.scale);
    quad_transform.translation =
        door_position + door_rotation * centre + front * PORTAL_QUAD_OFFSET;
    // `Plane3d` faces `+Z` and the player stands on the door's front (`-Z`).
    quad_transform.rotation = door_rotation * Quat::from_rotation_y(PI);
    quad_transform.scale = Vec3::new(size.x, size.y, 1.0);
    *quad_visibility = Visibility::Inherited;

    state.destination = destination_keys(&door.destination);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::creation_rotation_to_bevy;
    use bevy::{
        asset::AssetPlugin, camera::CameraProjection, camera::visibility::VisibilityPlugin,
        transform::TransformPlugin,
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
        }
    }

    /// An app with the visibility and transform systems the isolation relies on.
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
        .add_systems(
            Update,
            (hide_load_door_meshes, update_portal, isolate_cells).chain(),
        );
        app
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
        let doors = [
            (near_door, Vec3::new(0.0, 0.0, -300.0), &near),
            (far_door, Vec3::new(0.0, 0.0, -700.0), &far),
            (unready_door, Vec3::new(0.0, 0.0, -50.0), &unready),
            (behind_door, Vec3::new(0.0, 0.0, -10.0), &outside),
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
        )];
        assert_eq!(
            select_portal_door(camera, far_only, ready, &space, |_| 100.0),
            None
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
        let (size, centre) =
            portal_quad_extents(None, Some(&model), quarter_turn, Vec3::new(0.5, 2.0, 1.0));
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
        let (size, centre) = portal_quad_extents(Some(&bounds), None, quarter_turn, Vec3::ONE);
        assert!(
            (size.x - 200.0).abs() < 1.0e-3 && (size.y - 300.0).abs() < 1.0e-3,
            "the 200-unit axis lies across the world's z here: {size:?}"
        );
        assert!((centre.y - 150.0).abs() < 1.0e-3, "{centre:?}");
        let (size, _) = portal_quad_extents(Some(&bounds), None, Quat::IDENTITY, Vec3::ONE);
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
            let (size, centre) =
                portal_quad_extents(instance.as_ref(), expected, Quat::IDENTITY, Vec3::ONE);
            assert_eq!(size, DEFAULT_PORTAL_SIZE);
            assert!((centre.y - DEFAULT_PORTAL_SIZE.y * 0.5).abs() < 1.0e-3);
        }
    }

    #[test]
    fn a_load_door_stops_drawing_its_meshes() {
        let mut app = portal_app();
        let door = app
            .world_mut()
            .spawn((
                Transform::default(),
                GlobalTransform::default(),
                Visibility::default(),
                interior_door(INTERIOR_ALFTAND01),
            ))
            .id();
        let (mesh, _) = spawn_mesh(&mut app, door, None);

        update(&mut app, 2);

        assert_eq!(
            *app.world().entity(door).get::<Visibility>().unwrap(),
            Visibility::Hidden
        );
        assert!(
            !app.world()
                .entity(mesh)
                .get::<InheritedVisibility>()
                .unwrap()
                .get(),
            "the door leaf is gone, so the doorway is open"
        );
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
}
