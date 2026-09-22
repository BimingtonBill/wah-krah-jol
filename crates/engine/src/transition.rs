//! Load-door crossings between cells and worldspaces. See docs/design/blackreach-demo.md and
//! docs/design/animated-doors-and-seamless-crossing.md.
//!
//! Three jobs, all of them cheap enough to run every frame:
//!
//! * **Pre-stream.** Every load door within [`DOOR_PRESTREAM_RADIUS`] of the camera puts its
//!   destination into [`PrestreamCells`], which the streaming plan merges into its wanted set, so
//!   the destination is already resident before the camera gets there. That is the whole of "no
//!   loading screen": the crossing itself is a camera move.
//! * **Cross.** On [`CrossDoor`] - the player walking through a doorway ([`crate::player`]) or
//!   walking into an auto-load marker - carry the player's own pose through the door's rigid
//!   door -> arrival map and set [`ActiveCell`] from the door's link. Nothing is snapped, so the
//!   doorway the portal was showing and the room the player ends up in are the same view from the
//!   same pose (design section 4.2). [`ActivateDoor`] keeps the old behaviour for scripted runs:
//!   it puts the camera down on the `XTEL` arrival point instead.
//! * **Hold.** A crossing whose destination is not streamed in yet waits for it (design section
//!   4.4), so a crossing can never land in an empty space.
//!
//! All three run in [`DoorTransition`], which the streaming plan is ordered after, so the plan sees
//! the camera's new cell and the door's pre-stream in the frame they change.
//!
//! # The map, and why both sides use it
//!
//! [`door_frame`], [`arrival_frame`], [`door_to_arrival_rotation`] and [`portal_pose`] are the one
//! definition of "where the destination is, seen from this side of the door". The portal camera
//! (`crate::portal::update_portal`) is placed by them, the portal picks the door it opens by them,
//! the player's plane trigger measures the doorway with them, and [`crossing_pose`] carries the
//! player through them. Nothing else may re-derive the mapping: the swap is invisible exactly
//! because both sides compute the same pose from the same inputs.

use crate::{
    doors::{ActivateDoor, DoorCrossed, DoorDestination, DoorState, LoadDoor},
    player::EYE_HEIGHT,
    profiling::ProfilingState,
    streaming::{
        ActiveCell, PrestreamCells, RenderOrigin, StreamingWorld, creation_to_bevy,
        render_position, reposition_cell_roots,
    },
    world::{
        components::{CELL_SIZE, ExteriorCellGrid, StreamingCamera},
        database::CellKey,
    },
};
use bevy::prelude::*;
use std::f32::consts::PI;

/// A load door closer to the camera than this has its destination streamed in.
pub const DOOR_PRESTREAM_RADIUS: f32 = 800.0;

/// A door into another worldspace pre-streams this many cells around its arrival point.
const DOOR_PRESTREAM_GRID_RADIUS: i32 = 1;

/// The transition systems. The streaming plan runs after this set: a crossing this frame has to
/// be visible to the plan in the same frame.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DoorTransition;

/// Registers the load-door messages and the systems that use them.
///
/// [`StreamingPlugin`](crate::streaming::StreamingPlugin) adds this plugin; it can also be added
/// on its own, by a test or a tool that drives crossings without a world database.
pub struct TransitionPlugin;

impl Plugin for TransitionPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ActivateDoor>()
            .add_message::<CrossDoor>()
            .add_message::<OpenDoor>()
            .add_message::<DoorCrossed>()
            .init_resource::<PrestreamCells>()
            .init_resource::<PendingCrossing>()
            .add_systems(
                Update,
                // Cross, then plan from where the camera ended up, so the door just left does not
                // pre-stream the cell just entered. Opening a door is not here: `E` runs the
                // door's own animation ([`crate::door_animation`]), which is what the crossing
                // then reads.
                (apply_door_crossings, plan_door_prestream)
                    .chain()
                    .in_set(DoorTransition),
            );
    }
}

/// A load door the player asked to open (`E`), written by [`crate::player`].
///
/// The door opens and nothing else happens: the player walks through the doorway, and the crossing
/// fires when their feet reach its plane ([`CrossDoor`]). This is deliberately *not* an
/// [`ActivateDoor`], which still crosses on the spot.
///
/// Read by [`crate::door_animation`], which starts the door's own `Open` clip - or, for a door
/// whose model carries no clip at all, promotes it to
/// [`DoorState::Open { animated: false }`](crate::doors::DoorState::Open) in the same frame, which
/// is the doorway opening in one frame that a static door has always had.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenDoor {
    pub door: Entity,
}

/// The name of the component that used to mark a door as open. **Retired**: a door's own
/// [`DoorState`] is the one answer to that question now ([`door_is_open`]), `E` runs the door's
/// animation into it ([`crate::door_animation`]), and nothing inserts a second marker beside it.
///
/// The alias is here only because `crate::demo_tour`'s walk-through still reads
/// `door_is_open(open)` off an `Option<&DoorOpen>` query. Aliased, that call reads the real door
/// state rather than a component that could drift from it. It goes together with the two lines
/// `demo_tour.rs` needs - `DoorState` in its import and `Option<&DoorState>` in its door query.
pub type DoorOpen = DoorState;

/// Whether a load door is open: the doorway is a way through, its leaf is not drawn
/// (`crate::portal::show_load_door_leaves`) and the player's plane trigger fires on it.
///
/// The answer is the door's own [`DoorState::is_open`] - `Opening` (the swing is under way and the
/// doorway is already wide enough) or `Open` - and a door with no state at all counts as closed, so
/// a run that does not add [`crate::door_animation`] behaves as it did before doors had states.
pub fn door_is_open(state: Option<&DoorState>) -> bool {
    state.is_some_and(|state| state.is_open())
}

/// Cross a load door by walking through it: the player's pose is carried through the door's rigid
/// door -> arrival map ([`crossing_pose`]) rather than snapped to the `XTEL` arrival point.
///
/// Written by the player's doorway trigger and by an auto-load marker on contact
/// ([`crate::player`]); read by [`apply_door_crossings`].
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossDoor {
    pub door: Entity,
}

/// A crossing that has been asked for but cannot be made yet, because the destination is not
/// streamed in (design section 4.4). Held until the frame it becomes resident; dropped when the
/// door it belongs to unloads first.
///
/// While it is held the door draws its own leaf closed ([`CrossingHeld`]): the portal has no window
/// to show through it - the destination is not streamed in, which is what the wait is for - and a
/// doorway with neither a window nor a leaf in it is a hole in the world.
#[derive(Resource, Default)]
struct PendingCrossing {
    request: Option<CrossingRequest>,
}

/// Marks the door whose crossing is waiting for its destination to stream in: the one
/// [`PendingCrossing`] holds. Put on and taken off by [`apply_door_crossings`], read by the portal
/// and by the door animation, which both have to leave that door looking shut until the crossing
/// can be made.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossingHeld;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CrossingRequest {
    door: Entity,
    style: CrossingStyle,
}

/// How a crossing places the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossingStyle {
    /// Carry the player's own pose through the door -> arrival map: the walking crossing, and the
    /// one that makes the swap invisible (design section 4.2).
    Mapped,
    /// Put the camera down on the `XTEL` arrival point, facing the arrival heading. What a
    /// crossing has always done, and what a scripted run asks for with [`ActivateDoor`].
    Snap,
}

/// Requests the destination of every load door the camera is close to.
///
/// An interior destination is one cell. An exterior destination is the grid around the arrival
/// point in the destination worldspace - the arrival point, not the destination door, which can
/// be hundreds of units away (see `docs/research/worldspace-transition-demo.md` section 2.2).
fn plan_door_prestream(
    camera: Query<&Transform, With<StreamingCamera>>,
    doors: Query<(&GlobalTransform, &LoadDoor)>,
    mut prestream: ResMut<PrestreamCells>,
) {
    prestream.clear();
    let Ok(camera) = camera.single() else {
        return;
    };
    let camera = camera.translation;
    for (transform, door) in &doors {
        if transform.translation().distance_squared(camera) > DOOR_PRESTREAM_RADIUS.powi(2) {
            continue;
        }
        if let Some(cell_id) = door.destination.interior_cell_id {
            prestream.request_interior(cell_id);
            continue;
        }
        let Some(worldspace_id) = door.destination.worldspace_id else {
            continue;
        };
        let arrival = creation_to_bevy(Vec3::from_array(door.destination.arrival_position));
        let grid = IVec2::new(
            (arrival.x / CELL_SIZE).floor() as i32,
            (-arrival.z / CELL_SIZE).floor() as i32,
        );
        for y in -DOOR_PRESTREAM_GRID_RADIUS..=DOOR_PRESTREAM_GRID_RADIUS {
            for x in -DOOR_PRESTREAM_GRID_RADIUS..=DOOR_PRESTREAM_GRID_RADIUS {
                prestream.request_exterior(worldspace_id, grid + IVec2::new(x, y));
            }
        }
    }
}

/// The camera rotation for a Creation-engine `XTEL` arrival rotation (or any Creation heading the
/// player should look along).
///
/// A Creation heading is measured *clockwise* from north (`+Y`) seen from above: the player at
/// heading `z` looks along Creation `(sin z, cos z, 0)`. Every `XTEL` on the Alftand route puts the
/// arrival point in front of the destination door along exactly that direction. In runtime space
/// that is `(sin z, 0, -cos z)`, which a camera (looking down its `-Z`) reaches by turning `-z`
/// about the up axis. The object convention (`creation_rotation_to_bevy`) turns the other way, so
/// reusing it (with or without a half turn) only matched some doors: Alftand01 and Alftand02
/// arrived looking out of the room, and their portals rendered the clear colour. Arrival pitch
/// and roll are ignored; they are zero on load doors.
pub(crate) fn arrival_camera_rotation(rotation: [f32; 3]) -> Quat {
    Quat::from_rotation_y(-rotation[2])
}

// ---------------------------------------------------------------------------------------------
// The door -> arrival map
// ---------------------------------------------------------------------------------------------

/// The frame the portal looks at a door in, and the frame a crossing is measured in: the door's
/// front is its `-Z`, as a door model's frame has it, but which way that points comes from the
/// door's own [`LoadDoor::outward`] whenever the database knows it.
///
/// `outward` is world-space evidence - the direction from the door toward where the link that leads
/// back into it puts the arriving player ([`crate::doors::outward_from_return_link`]) - and door
/// models disagree about which of their own axes is their front, so it wins over `door_rotation`
/// (the reference's own rotation). A Creation heading `z` faces `(sin z, cos z, 0)`, and a runtime
/// camera reaches that direction along its `-Z` by turning `-z` about the up axis
/// ([`arrival_camera_rotation`]), which is the frame built here.
///
/// Without an outward direction the model's frame is all there is, and the portal behaves as it did
/// before the door links were read.
pub(crate) fn door_frame(door_rotation: Quat, outward: Option<[f32; 3]>) -> Quat {
    let Some(outward) = outward else {
        return door_rotation;
    };
    let (east, north) = (outward[0], outward[1]);
    if !east.is_finite() || !north.is_finite() || east.hypot(north) <= 0.0 {
        return door_rotation;
    }
    Quat::from_rotation_y(-east.atan2(north))
}

/// How far in front of the door a point stands, along the door's front direction: positive on
/// the side the door faces, negative behind it, in Creation units.
///
/// The door's plane is the one the doorway stands in, and [`door_to_arrival_rotation`] carries it
/// onto the arrival doorway: the frame's `-Z` (the front) maps to the arrival facing reversed, so
/// this signed distance is the same `-w` the doorway clip plane gives, which is what the portal
/// camera's projection is built with. Picking the door from a different number than the projection
/// clips at would let the portal show a destination through a doorway its own camera is behind.
pub(crate) fn distance_in_front_of_door(door_position: Vec3, frame: Quat, point: Vec3) -> f32 {
    (point - door_position).dot(frame * Vec3::NEG_Z)
}

/// The render-space pose of a door's `XTEL` arrival frame: an interior is at its absolute creation
/// coordinates, an exterior relative to the render origin it is streamed at.
pub(crate) fn arrival_frame(destination: &DoorDestination, origin: IVec2) -> (Vec3, Quat) {
    let arrival = Vec3::from_array(destination.arrival_position);
    let position = if destination.interior_cell_id.is_some() {
        creation_to_bevy(arrival)
    } else {
        render_position(arrival, origin)
    };
    (
        position,
        arrival_camera_rotation(destination.arrival_rotation),
    )
}

/// The rotation that carries the source door's frame onto the arrival frame.
///
/// Both frames use the same convention: their forward (`-Z`) is the direction the frame faces - for
/// the door the side the player walks in from, for the arrival frame the way the arriving player
/// faces ([`arrival_camera_rotation`] builds the camera's rotation from it). A player standing in
/// front of the door maps to the same distance on the far side of the arrival point, and a view
/// aimed at the door maps to a view aimed along the arrival facing, which is the direction a
/// crossing sends the camera:
///
/// ```text
/// M = T(arrival) * R(arrival) * Y180 * R(door)^-1 * T(-door)
/// ```
pub(crate) fn door_to_arrival_rotation(door_rotation: Quat, arrival_rotation: Quat) -> Quat {
    arrival_rotation * Quat::from_rotation_y(PI) * door_rotation.inverse()
}

/// The pose `camera_position`/`camera_rotation` carried through [`door_to_arrival_rotation`]: the
/// portal camera's pose, and the pose a crossing gives the player.
///
/// `frame` is the door's frame as [`door_frame`] gives it: the front is `-Z`, and looking into the
/// door - the way a camera standing in front of it looks - is `+Z`, which the mapping carries onto
/// the arrival facing. A pose at the door's own position lands on the arrival point
/// (`M(door) = arrival`), so the pose the portal was rendering from is the pose the player ends up
/// at when they walk out of the doorway.
///
/// The map is rigid and about the up axis, so it moves poses without stretching the destination,
/// keeps every height, and turns a look without tilting it: a camera keeps its pitch and its roll
/// and has its heading carried onto the arrival heading.
pub(crate) fn portal_pose(
    door_position: Vec3,
    frame: Quat,
    arrival_position: Vec3,
    arrival_rotation: Quat,
    camera_position: Vec3,
    camera_rotation: Quat,
) -> (Vec3, Quat) {
    let map = door_to_arrival_rotation(frame, arrival_rotation);
    (
        arrival_position + map * (camera_position - door_position),
        map * camera_rotation,
    )
}

/// A Creation-engine point from a render-space one: the inverse of `streaming::render_position`
/// for a point of the space `target` names. An interior is at absolute creation coordinates, an
/// exterior sits relative to the render origin it was streamed with.
fn creation_from_render(position: Vec3, target: SpaceTarget, origin: IVec2) -> Vec3 {
    let absolute = match target {
        SpaceTarget::Interior(_) => position,
        SpaceTarget::Exterior(_) => Vec3::new(
            position.x + origin.x as f32 * CELL_SIZE,
            position.y,
            position.z - origin.y as f32 * CELL_SIZE,
        ),
    };
    Vec3::from_array(shared::coordinates::runtime_to_creation_vector(
        absolute.to_array(),
    ))
}

/// Where a crossing puts the player: their own pose carried through the same rigid map the portal
/// camera is placed by ([`portal_pose`]), design section 4.2.
///
/// The result is a Creation-engine **feet** position, which [`switch_space`] turns into the new
/// render frame - it re-bases the render origin exactly once, for an exterior destination - and the
/// render rotation the camera takes: the player's pitch, with their heading carried onto the
/// arrival heading.
///
/// The map itself is the design's, in Creation axes: with `z_door` the heading the door faces and
/// `z_arrival` the arrival heading,
///
/// ```text
/// alpha = z_arrival - z_door - PI
/// C'    = C_arrival + S(alpha) (C_feet - C_door)      z' = z_player + alpha
/// ```
///
/// where `S` is a Creation heading rotation (clockwise seen from above, the way a heading turns),
/// and the pitch is untouched. `the_crossing_maps_the_pose_the_design_specifies` and
/// `the_four_real_route_links_still_round_trip` check exactly that against this implementation,
/// which computes it through [`portal_pose`] so that the crossing and the portal can never drift
/// apart.
#[allow(clippy::too_many_arguments)]
pub(crate) fn crossing_pose(
    door_position: Vec3,
    frame: Quat,
    arrival_position: Vec3,
    arrival_rotation: Quat,
    feet: Vec3,
    rotation: Quat,
    target: SpaceTarget,
    origin: IVec2,
) -> (Vec3, Quat) {
    let (mapped, mapped_rotation) = portal_pose(
        door_position,
        frame,
        arrival_position,
        arrival_rotation,
        feet,
        rotation,
    );
    (
        creation_from_render(mapped, target, origin),
        mapped_rotation,
    )
}

/// The cells a destination is made of: the interior, or the grid around an exterior arrival point
/// that [`plan_door_prestream`] streams.
pub(crate) fn destination_keys(destination: &DoorDestination) -> Vec<CellKey> {
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
pub(crate) fn destination_is_resident(
    destination: &DoorDestination,
    streaming: &StreamingWorld,
) -> bool {
    match destination_keys(destination).first() {
        Some(key) => streaming.is_resident(key),
        None => false,
    }
}

/// Whether a crossing into `destination` can be made now: the destination has to be there.
///
/// A run without a [`StreamingWorld`] - a test, or a tool that drives crossings itself - has
/// nothing to wait for and everything is ready.
fn destination_is_ready(destination: &DoorDestination, streaming: Option<&StreamingWorld>) -> bool {
    streaming.is_none_or(|streaming| destination_is_resident(destination, streaming))
}

/// A place to move the camera into: an interior cell, or an exterior worldspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpaceTarget {
    Interior(u32),
    Exterior(u32),
}

impl SpaceTarget {
    /// The space a door's `XTEL` leads into. An interior wins when a link somehow names both,
    /// which is the order a crossing has always checked them in.
    pub(crate) fn of_destination(destination: &DoorDestination) -> Option<Self> {
        match (destination.interior_cell_id, destination.worldspace_id) {
            (Some(cell_id), _) => Some(Self::Interior(cell_id)),
            (None, Some(worldspace_id)) => Some(Self::Exterior(worldspace_id)),
            (None, None) => None,
        }
    }
}

/// Moves the camera into `target` at a Creation-engine position, the way a door crossing and a
/// reference shot both have to: sets [`ActiveCell`], and for an exterior moves the
/// [`RenderOrigin`] to the position's cell and re-places every spawned cell root (exactly what a
/// rebase does, which leaves the camera near the origin of the new worldspace).
///
/// Returns where the camera stands in render coordinates. A crossing adds its eye height to that;
/// a shot is a camera and takes it as it is.
pub(crate) fn switch_space(
    target: SpaceTarget,
    creation_position: Vec3,
    active: &mut ActiveCell,
    origin: &mut RenderOrigin,
    roots: &mut Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
) -> Vec3 {
    match target {
        SpaceTarget::Interior(cell_id) => {
            // An interior root sits at the render origin and its references carry the interior's
            // absolute creation coordinates, so the camera does too and the origin must not move.
            active.interior = Some(cell_id);
            creation_to_bevy(creation_position)
        }
        SpaceTarget::Exterior(worldspace_id) => {
            active.worldspace_id = worldspace_id;
            active.interior = None;
            // The destination may have been pre-streamed, so its roots were placed for the origin
            // the camera is leaving. Take the origin to its cell and re-place them.
            let position_in_world = creation_to_bevy(creation_position);
            origin.0 = IVec2::new(
                (position_in_world.x / CELL_SIZE).floor() as i32,
                (-position_in_world.z / CELL_SIZE).floor() as i32,
            );
            reposition_cell_roots(origin.0, roots);
            render_position(creation_position, origin.0)
        }
    }
}

/// Moves the camera through a load door.
///
/// Two styles, and the difference between them is the whole feature (design section 4.2):
///
/// * A [`CrossDoor`] - the player walking through a doorway, or into an auto-load marker - carries
///   the player's own pose through the door -> arrival map ([`crossing_pose`]). The camera ends up
///   exactly where the portal camera was rendering the destination from, so the swap frame is the
///   same view of the destination from the same pose.
/// * An [`ActivateDoor`] - a scripted run, `--demo-tour` among them - puts the camera down on the
///   `XTEL` arrival point facing the arrival heading, which is what a crossing has always done.
///
/// Either way the crossing waits until the destination is streamed in (design section 4.4): the
/// request is held and applied in the frame the destination becomes resident, which is normally
/// the frame it was asked for, since the pre-stream has been asking for it all along.
#[allow(clippy::too_many_arguments)]
fn apply_door_crossings(
    mut commands: Commands,
    mut requests: MessageReader<CrossDoor>,
    mut activations: MessageReader<ActivateDoor>,
    mut pending: ResMut<PendingCrossing>,
    doors: Query<(&GlobalTransform, &LoadDoor)>,
    streaming: Option<Res<StreamingWorld>>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut active: ResMut<ActiveCell>,
    mut origin: ResMut<RenderOrigin>,
    mut roots: Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    mut crossed: MessageWriter<DoorCrossed>,
    mut profiler: ResMut<ProfilingState>,
) {
    // One crossing at a time. A frame can carry a walk through one doorway and a marker firing
    // beside it, or two markers at a junction, and only one of them can be crossed: the door the
    // eye is nearest is the one the player is walking into, and it is the same door whichever order
    // the requests arrive in.
    let eye = camera.single().ok().map(|transform| transform.translation);
    let mut chosen: Option<(Entity, CrossingStyle, f32)> = None;
    let consider = |entity: Entity,
                    style: CrossingStyle,
                    chosen: &mut Option<(Entity, CrossingStyle, f32)>| {
        let distance = eye
            .and_then(|eye| {
                doors
                    .get(entity)
                    .ok()
                    .map(|(global, _)| global.translation().distance(eye))
            })
            .unwrap_or(f32::INFINITY);
        let nearer = chosen.is_none_or(|(chosen_entity, _, nearest)| {
            distance < nearest || (distance == nearest && entity < chosen_entity)
        });
        if nearer {
            *chosen = Some((entity, style, distance));
        }
    };
    for request in requests.read() {
        consider(request.door, CrossingStyle::Mapped, &mut chosen);
    }
    for request in activations.read() {
        consider(request.door, CrossingStyle::Snap, &mut chosen);
    }
    if let Some((door, style, _)) = chosen {
        if let Some(replaced) = pending.request.filter(|previous| previous.door != door) {
            // A newer request takes the hold over: the door it left is not the one waiting any
            // more, and it has to look like the door it is again.
            commands.entity(replaced.door).remove::<CrossingHeld>();
        }
        pending.request = Some(CrossingRequest { door, style });
    }
    let Some(request) = pending.request else {
        return;
    };
    let Ok((camera_position, camera_rotation)) = camera
        .single()
        .map(|transform| (transform.translation, transform.rotation))
    else {
        // No camera to move: the request stays pending until there is one.
        return;
    };
    let Ok((door_transform, door)) = doors.get(request.door) else {
        // The door was unloaded before its crossing could be made.
        pending.request = None;
        commands.entity(request.door).remove::<CrossingHeld>();
        return;
    };
    let Some(target) = SpaceTarget::of_destination(&door.destination) else {
        pending.request = None;
        commands.entity(request.door).remove::<CrossingHeld>();
        return;
    };
    if !destination_is_ready(&door.destination, streaming.as_deref()) {
        // Held for its destination (design section 4.4). The door draws shut until it arrives:
        // there is no window to look through yet, because the destination is not streamed in.
        commands.entity(request.door).insert_if_new(CrossingHeld);
        return;
    }
    let (creation_feet, rotation) = match request.style {
        CrossingStyle::Snap => (
            Vec3::from_array(door.destination.arrival_position),
            arrival_camera_rotation(door.destination.arrival_rotation),
        ),
        CrossingStyle::Mapped => {
            let (arrival_position, arrival_rotation) = arrival_frame(&door.destination, origin.0);
            crossing_pose(
                door_transform.translation(),
                door_frame(door_transform.rotation(), door.outward),
                arrival_position,
                arrival_rotation,
                crate::player::feet_from_eye(camera_position),
                camera_rotation,
                target,
                origin.0,
            )
        }
    };
    let translation = switch_space(target, creation_feet, &mut active, &mut origin, &mut roots);
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    // The mapped feet are the player's own, one eye height below the camera; the snapped arrival
    // is where the player's feet land. Walking would snap an eye left on the floor back up, but
    // flying (and the demo tour) never does.
    camera.translation = translation + Vec3::Y * EYE_HEIGHT;
    camera.rotation = rotation;
    pending.request = None;
    commands.entity(request.door).remove::<CrossingHeld>();
    profiler.increment("doors/crossed", 1);
    profiler.event(format!("{:08X}", door.ref_id), "door_crossed", None);
    crossed.write(DoorCrossed {
        from_ref_id: door.ref_id,
        label: door.label.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::EngineConfig,
        doors::DoorDestination,
        render::{TerrainMaterial, WaterMaterial, WaterReflectionTexture},
        streaming::{StreamingMetrics, StreamingPlugin, StreamingWorld, creation_rotation_to_bevy},
        world::{
            cache::CellCache,
            components::StreamedCellRoot,
            database::{AssetCatalog, CellKey, WorldDatabase},
        },
    };
    use bevy::{
        asset::{AssetApp, AssetPlugin},
        transform::TransformPlugin,
    };
    use std::{path::Path, time::Duration};

    /// Skyrim.esm's Alftand route: (destination door position, `XTEL` arrival point, arrival
    /// heading). The game puts the arriving player in front of the destination door, facing away
    /// from it, so the arrival camera must look from the door towards the arrival point.
    ///
    /// The same four links are the "real links" regression: they are the doors the demo walks
    /// through, so a change to the mapping that breaks them breaks the demo.
    const ROUTE_LINKS: [([f32; 2], [f32; 2], f32); 4] = [
        ([-1287.818, 4470.613], [-947.038, 3958.835], 2.969_887), // -> Alftand01
        ([2861.837, 2780.051], [2879.831, 2718.83], 2.879_793),   // -> Alftand02
        ([3755.335, 3092.286], [3693.815, 3074.645], -1.832_596), // -> AlftandWorld
        ([21149.7, 18530.96], [21088.56, 18512.04], -1.870_796),  // -> Blackreach
    ];

    #[test]
    fn arrival_camera_faces_away_from_the_destination_door() {
        for (door, arrival, yaw) in ROUTE_LINKS {
            let away = (creation_to_bevy(Vec3::new(arrival[0], arrival[1], 0.0))
                - creation_to_bevy(Vec3::new(door[0], door[1], 0.0)))
            .normalize();
            let forward = arrival_camera_rotation([0.0, 0.0, yaw]) * Vec3::NEG_Z;
            assert!(
                forward.dot(away) > 0.9,
                "yaw {yaw}: camera looks along {forward}, away from the door is {away}"
            );
        }
    }

    /// The angle between two headings, however they are wrapped.
    fn heading_difference(left: f32, right: f32) -> f32 {
        (left - right + PI).rem_euclid(2.0 * PI) - PI
    }

    /// One crossing, computed twice: the way [`apply_door_crossings`] computes it, and the way
    /// design section 4.2 states it (`alpha = z_arrival - z_door - PI`, the feet carried by a
    /// heading rotation of `alpha` about the door, `alpha` added to the heading, the pitch kept).
    ///
    /// `source_interior` picks the convention the source space places references in - an interior
    /// carries absolute creation coordinates, an exterior is relative to the render origin - so the
    /// crossing has to undo exactly the one it was given, and put the result into the destination's
    /// own convention (which the design formula, in Creation coordinates, is free of).
    #[allow(clippy::too_many_arguments)]
    fn assert_crossing_matches_the_design(
        destination: &DoorDestination,
        source_interior: bool,
        origin: IVec2,
        door: [f32; 3],
        door_heading: f32,
        feet: [f32; 3],
        player_heading: f32,
        pitch: f32,
    ) {
        let target = SpaceTarget::of_destination(destination).expect("a usable destination");
        let in_render_space = |creation: Vec3| {
            if source_interior {
                creation_to_bevy(creation)
            } else {
                render_position(creation, origin)
            }
        };
        let (arrival_position, arrival_rotation) = arrival_frame(destination, origin);
        let frame = door_frame(
            Quat::IDENTITY,
            Some([door_heading.sin(), door_heading.cos(), 0.0]),
        );
        let (creation_feet, rotation) = crossing_pose(
            in_render_space(Vec3::from_array(door)),
            frame,
            arrival_position,
            arrival_rotation,
            in_render_space(Vec3::from_array(feet)),
            Quat::from_euler(EulerRot::YXZ, -player_heading, pitch, 0.0),
            target,
            origin,
        );

        let arrival_heading = destination.arrival_rotation[2];
        let alpha = arrival_heading - door_heading - PI;
        let (sin, cos) = alpha.sin_cos();
        let offset = Vec3::from_array(feet) - Vec3::from_array(door);
        let expected_feet = Vec3::from_array(destination.arrival_position)
            + Vec3::new(
                offset.x * cos + offset.y * sin,
                -offset.x * sin + offset.y * cos,
                offset.z,
            );
        assert!(
            creation_feet.abs_diff_eq(expected_feet, 1.0e-1),
            "the feet land at {creation_feet:?}, the design's map says {expected_feet:?}"
        );

        let (yaw, mapped_pitch, _) = rotation.to_euler(EulerRot::YXZ);
        assert!(
            heading_difference(-yaw, player_heading + alpha).abs() < 1.0e-4,
            "the player faces heading {}, the design's map says {}",
            -yaw,
            player_heading + alpha
        );
        assert!(
            (mapped_pitch - pitch).abs() < 1.0e-4,
            "the pitch is carried through: {mapped_pitch} != {pitch}"
        );
    }

    /// The crossing is the design's map, for every kind of source and destination space, at a
    /// render origin that is not zero, and with a pitch the player is looking up or down by.
    #[test]
    fn the_crossing_maps_the_pose_the_design_specifies() {
        let interior = interior_destination(0x0001_52C3);
        let exterior = exterior_destination(0x0001_EE62, [21088.559, 18512.045, 2434.0]);
        let cases: [(&LoadDoor, bool, IVec2); 4] = [
            (&interior, true, IVec2::ZERO),
            (&interior, false, IVec2::new(19, 18)),
            (&exterior, true, IVec2::new(19, 18)),
            (&exterior, false, IVec2::new(0, 0)),
        ];
        for (door, source_interior, origin) in cases {
            for (door_heading, player_heading, pitch) in [
                (0.7_f32, 0.7 + PI, 0.3_f32),
                (2.9, -1.2, -0.25),
                (0.0, PI, 0.0),
            ] {
                assert_crossing_matches_the_design(
                    &door.destination,
                    source_interior,
                    origin,
                    [1200.0, -4300.0, 240.0],
                    door_heading,
                    [1150.0, -4270.0, 240.0],
                    player_heading,
                    pitch,
                );
            }
        }
    }

    /// A player walking through a doorway comes out on the arrival point facing the arrival
    /// heading, keeps their eye height, and the map is rigid - it is a pose moved, not a room
    /// stretched.
    #[test]
    fn a_walk_through_the_doorway_keeps_the_eye_height_and_the_walking_direction() {
        let load_door = interior_destination(0x0001_52C3);
        let destination = &load_door.destination;
        let origin = IVec2::new(19, 18);
        // An interior, so a reference of it renders at its absolute creation coordinates.
        let door = Vec3::new(
            destination.arrival_position[0] + 900.0,
            destination.arrival_position[1] - 400.0,
            destination.arrival_position[2],
        );
        let door_heading: f32 = 0.6;
        let frame = door_frame(
            Quat::IDENTITY,
            Some([door_heading.sin(), door_heading.cos(), 0.0]),
        );
        let (arrival, arrival_rotation) = arrival_frame(destination, origin);
        let target = SpaceTarget::of_destination(destination).unwrap();

        // The player's feet stand in the doorway, 40 units below the door's own origin, walking in
        // against the front: a pose with a pitch, not just a yaw.
        let feet_creation = door - Vec3::new(0.0, 0.0, 40.0);
        let feet = creation_to_bevy(feet_creation);
        let rotation = Quat::from_euler(EulerRot::YXZ, -(door_heading + PI), 0.1, 0.0);
        let (mapped_feet, mapped_rotation) = crossing_pose(
            creation_to_bevy(door),
            frame,
            arrival,
            arrival_rotation,
            feet,
            rotation,
            target,
            origin,
        );
        let mapped = creation_to_bevy(mapped_feet);

        // The eye is still one eye height above the feet: the map is a yaw about the up axis, so
        // it cannot change how high the player stands, and the whole pose moves together.
        let (mapped_eye, _) = portal_pose(
            creation_to_bevy(door),
            frame,
            arrival,
            arrival_rotation,
            feet + Vec3::Y * EYE_HEIGHT,
            rotation,
        );
        assert!(
            (mapped_eye - (mapped + Vec3::Y * EYE_HEIGHT)).length() < 1.0e-2,
            "the eye at {mapped_eye:?} is not {EYE_HEIGHT} above the mapped feet {mapped:?}"
        );
        assert!(
            (mapped_feet.z - (destination.arrival_position[2] - 40.0)).abs() < 1.0e-2,
            "40 units below the arrival point stay 40 units below it: {mapped_feet:?}"
        );

        // Walking in maps the direction the player walks onto the arrival facing: the player looks
        // along the arrival heading after the crossing, whatever the door's own heading was.
        let (yaw, pitch, _) = mapped_rotation.to_euler(EulerRot::YXZ);
        assert!(
            heading_difference(-yaw, destination.arrival_rotation[2]).abs() < 1.0e-4,
            "the player faces {} after walking in, the arrival heading is {}",
            -yaw,
            destination.arrival_rotation[2]
        );
        assert!((pitch - 0.1).abs() < 1.0e-4, "the pitch is kept: {pitch}");

        // Rigid: a step across the doorway is the same step in the destination room, so two
        // players who walk through side by side come out the same distance apart.
        let aside = (frame * Vec3::NEG_Z).cross(Vec3::Y).normalize() * 100.0;
        let (other, _) = portal_pose(
            creation_to_bevy(door),
            frame,
            arrival,
            arrival_rotation,
            feet + aside,
            rotation,
        );
        assert!(
            ((other - mapped).length() - 100.0).abs() < 1.0e-2,
            "100 units apart walking in, {} apart arriving",
            (other - mapped).length()
        );
    }

    /// The four real route links, as the demo walks them: the link data says where a door is, where
    /// the player who comes through it lands and which way they face, and the link's own evidence
    /// says which side of the door its front is ([`crate::doors::outward_from_return_link`] - the
    /// arrival point is in front of the door).
    ///
    /// Walking in from that front lands the player on the arrival point facing the arrival heading,
    /// which is the pose the old snap put them at, reached without moving them. The map also undoes
    /// itself, so a crossing back the other way returns the pose it was given.
    #[test]
    fn the_four_real_route_links_still_round_trip() {
        for (door, arrival, arrival_heading) in ROUTE_LINKS {
            let outward = crate::doors::outward_from_return_link(
                [door[0], door[1], 0.0],
                [arrival[0], arrival[1], 0.0],
                [0.0, 0.0, arrival_heading],
            )
            .expect("the route's links say which way their doors face");
            let door_heading = outward[0].atan2(outward[1]);
            let frame = door_frame(Quat::IDENTITY, Some(outward));
            let position = creation_to_bevy(Vec3::new(door[0], door[1], 0.0));
            let arrival_position = creation_to_bevy(Vec3::new(arrival[0], arrival[1], 0.0));
            let arrival_rotation = arrival_camera_rotation([0.0, 0.0, arrival_heading]);

            // The player stands in the doorway, walking in against the door's front.
            let walking_heading = door_heading + PI;
            let rotation = Quat::from_euler(EulerRot::YXZ, -walking_heading, 0.2, 0.0);
            let (mapped_position, mapped_rotation) = portal_pose(
                position,
                frame,
                arrival_position,
                arrival_rotation,
                position,
                rotation,
            );
            assert!(
                mapped_position.abs_diff_eq(arrival_position, 1.0e-2),
                "standing at the door lands on the arrival point: {mapped_position:?} != {arrival_position:?}"
            );
            let (yaw, pitch, _) = mapped_rotation.to_euler(EulerRot::YXZ);
            assert!(
                heading_difference(-yaw, arrival_heading).abs() < 1.0e-4,
                "walking in faces the arrival heading: {} != {arrival_heading}",
                -yaw
            );
            assert!(
                (pitch - 0.2).abs() < 1.0e-4,
                "the pitch the player was looking at is carried: {pitch}"
            );

            // Round trip: the map undone is the pose it was given, for a pose that is not the
            // door's own.
            let start = position + frame * Vec3::new(60.0, 30.0, 180.0);
            let (mapped, _) = portal_pose(
                position,
                frame,
                arrival_position,
                arrival_rotation,
                start,
                rotation,
            );
            let map = door_to_arrival_rotation(frame, arrival_rotation);
            let back = position + map.inverse() * (mapped - arrival_position);
            assert!(
                back.abs_diff_eq(start, 1.0e-2),
                "undoing the crossing puts the pose back: {back:?} != {start:?}"
            );
        }
    }

    fn interior_destination(cell_id: u32) -> LoadDoor {
        LoadDoor {
            ref_id: 0x30,
            destination: DoorDestination {
                destination_ref_id: 0x31,
                interior_cell_id: Some(cell_id),
                worldspace_id: None,
                arrival_position: [-947.038, 3958.835, 591.917],
                arrival_rotation: [0.0, 0.0, 2.96989],
            },
            label: "Alftand01".into(),
            auto_load: false,
            outward: None,
        }
    }

    fn exterior_destination(worldspace_id: u32, arrival: [f32; 3]) -> LoadDoor {
        LoadDoor {
            ref_id: 0x5704B,
            destination: DoorDestination {
                destination_ref_id: 0x699E8,
                interior_cell_id: None,
                worldspace_id: Some(worldspace_id),
                arrival_position: arrival,
                arrival_rotation: [0.0, 0.0, -1.8708],
            },
            label: "Blackreach".into(),
            auto_load: false,
            outward: None,
        }
    }

    /// A door entity whose `GlobalTransform` is where a spawned reference's would be.
    fn spawn_door(app: &mut App, position: Vec3, door: LoadDoor) -> Entity {
        app.world_mut()
            .spawn((
                Transform::from_translation(position),
                GlobalTransform::from_translation(position),
                door,
            ))
            .id()
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

    #[test]
    fn prestreams_only_the_destinations_of_doors_within_reach() {
        let mut app = App::new();
        app.add_plugins(TransitionPlugin)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .init_resource::<ProfilingState>();
        let camera = spawn_camera(&mut app, Vec3::ZERO);
        spawn_door(
            &mut app,
            Vec3::new(0.0, 0.0, -700.0),
            interior_destination(99),
        );
        spawn_door(
            &mut app,
            Vec3::new(0.0, 0.0, -900.0),
            interior_destination(98),
        );
        // A door to Blackreach, arriving at grid (5, 4): 21088.559, 18512.045 in Creation units.
        spawn_door(
            &mut app,
            Vec3::new(0.0, 0.0, -100.0),
            exterior_destination(614, [21088.559, 18512.045, 2434.0]),
        );
        app.update();

        let prestream = app.world().resource::<PrestreamCells>();
        assert!(prestream.contains(&CellKey::Interior(99)));
        assert!(
            !prestream.contains(&CellKey::Interior(98)),
            "a door 900 units away is outside the pre-stream radius"
        );
        for grid_y in 3..=5 {
            for grid_x in 4..=6 {
                assert!(prestream.contains(&CellKey::Exterior {
                    worldspace_id: 614,
                    grid_x,
                    grid_y,
                }));
            }
        }
        assert!(!prestream.contains(&CellKey::Exterior {
            worldspace_id: 614,
            grid_x: 3,
            grid_y: 3,
        }));

        // Walking away from every door drops every request.
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = Vec3::new(0.0, 0.0, -5000.0);
        app.update();

        let prestream = app.world().resource::<PrestreamCells>();
        assert!(!prestream.contains(&CellKey::Interior(99)));
        assert!(!prestream.contains(&CellKey::Exterior {
            worldspace_id: 614,
            grid_x: 5,
            grid_y: 4,
        }));
    }

    #[test]
    fn crossing_into_an_interior_lands_on_the_arrival_point_and_keeps_the_origin() {
        let mut app = App::new();
        app.add_plugins(TransitionPlugin);
        app.insert_resource(RenderOrigin(IVec2::new(19, 18)))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .init_resource::<ProfilingState>();
        let camera = spawn_camera(
            &mut app,
            Vec3::new(78049.18 - 19.0 * CELL_SIZE, -5859.11, -76985.0),
        );
        let exterior_root = app
            .world_mut()
            .spawn((ExteriorCellGrid(IVec2::new(19, 18)), Transform::default()))
            .id();
        let interior_root = app
            .world_mut()
            .spawn((StreamedCellRoot, Transform::default()))
            .id();
        let reference = Vec3::new(-9223.65, -516.0, 8792.0);
        let reference_entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(reference),
                ChildOf(interior_root),
            ))
            .id();
        let door = spawn_door(&mut app, Vec3::ZERO, interior_destination(0x152C3));

        app.world_mut().write_message(ActivateDoor { door });
        app.update();

        assert_eq!(
            *app.world().resource::<ActiveCell>(),
            ActiveCell {
                worldspace_id: 60,
                interior: Some(0x152C3),
            },
            "entering an interior keeps the worldspace it was entered from"
        );
        assert_eq!(
            app.world().resource::<RenderOrigin>().0,
            IVec2::new(19, 18),
            "an interior is placed at absolute creation coordinates"
        );
        let camera_transform = app.world().entity(camera).get::<Transform>().unwrap();
        // The camera is the eye, one eye height above the arrival (the feet).
        let expected = creation_to_bevy(Vec3::from_array([-947.038, 3958.835, 591.917]))
            + Vec3::Y * crate::player::EYE_HEIGHT;
        assert!(camera_transform.translation.abs_diff_eq(expected, 1.0e-4));
        assert!(
            camera_transform
                .rotation
                .abs_diff_eq(arrival_camera_rotation([0.0, 0.0, 2.96989]), 1.0e-6),
            "the camera takes the arrival rotation"
        );
        assert_eq!(
            app.world()
                .entity(exterior_root)
                .get::<Transform>()
                .unwrap()
                .translation,
            Vec3::ZERO,
            "an interior crossing does not move exterior roots"
        );
        assert_eq!(
            app.world()
                .entity(reference_entity)
                .get::<Transform>()
                .unwrap()
                .translation,
            reference,
            "nor the interior's references"
        );
    }

    #[test]
    fn crossing_into_an_exterior_moves_the_origin_and_re_places_the_cell_roots() {
        let mut app = App::new();
        app.add_plugins(TransitionPlugin);
        app.insert_resource(RenderOrigin(IVec2::new(0, 0)))
            .insert_resource(ActiveCell {
                worldspace_id: 0x69857,
                interior: None,
            })
            .init_resource::<ProfilingState>();
        let camera = spawn_camera(&mut app, Vec3::new(-4419.67, 1304.83, -740.95));
        // The destination cell, pre-streamed while the camera was still in AlftandWorld.
        let arrival_root = app
            .world_mut()
            .spawn((ExteriorCellGrid(IVec2::new(5, 4)), Transform::default()))
            .id();
        let left_behind_root = app
            .world_mut()
            .spawn((ExteriorCellGrid(IVec2::new(0, 0)), Transform::default()))
            .id();
        let door = spawn_door(
            &mut app,
            Vec3::ZERO,
            exterior_destination(0x1EE62, [21088.559, 18512.045, 2434.0]),
        );

        app.world_mut().write_message(ActivateDoor { door });
        app.update();

        assert_eq!(
            *app.world().resource::<ActiveCell>(),
            ActiveCell {
                worldspace_id: 0x1EE62,
                interior: None,
            }
        );
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(5, 4));
        let camera_transform = app.world().entity(camera).get::<Transform>().unwrap();
        // Eye height above the arrival point of (21088.559, 18512.045, 2434) with grid (5, 4) as
        // the origin.
        assert!(
            camera_transform.translation.abs_diff_eq(
                Vec3::new(608.559, 2434.0 + crate::player::EYE_HEIGHT, -2128.045),
                1.0e-2
            ),
            "camera landed at {:?}",
            camera_transform.translation
        );
        assert_eq!(
            app.world()
                .entity(arrival_root)
                .get::<Transform>()
                .unwrap()
                .translation,
            Vec3::ZERO,
            "the arrival cell now sits on the render origin"
        );
        assert_eq!(
            app.world()
                .entity(left_behind_root)
                .get::<Transform>()
                .unwrap()
                .translation,
            Vec3::new(-5.0 * CELL_SIZE, 0.0, 4.0 * CELL_SIZE),
            "a root of the worldspace just left moves with the origin, and is unloaded next"
        );
    }

    #[test]
    fn the_arrival_rotation_faces_the_camera_where_the_player_should_face() {
        // Creation-engine actors face +Y and a yaw turns them clockwise: at yaw z they face
        // (sin z, cos z). Objects and the arrival camera now share that convention, so the object
        // rotation of a yaw and the arrival camera rotation must agree.
        for yaw in [0.0_f32, 2.96989, -1.8708, 1.2] {
            let rotation = creation_rotation_to_bevy([0.0, 0.0, yaw]);
            assert!(
                rotation.abs_diff_eq(arrival_camera_rotation([0.0, 0.0, yaw]), 1.0e-5)
                    || rotation.abs_diff_eq(-arrival_camera_rotation([0.0, 0.0, yaw]), 1.0e-5),
                "yaw {yaw}: object and arrival rotations differ"
            );
            let expected = creation_to_bevy(Vec3::new(yaw.sin(), yaw.cos(), 0.0));
            assert!(
                (rotation * Vec3::NEG_Z).abs_diff_eq(expected, 1.0e-5),
                "yaw {yaw}: {:?} != {expected:?}",
                rotation * Vec3::NEG_Z
            );
        }
    }

    #[derive(Resource, Default)]
    struct CapturedCrossings(Vec<DoorCrossed>);

    fn capture_crossings(
        mut crossings: MessageReader<DoorCrossed>,
        mut captured: ResMut<CapturedCrossings>,
    ) {
        captured.0.extend(crossings.read().cloned());
    }

    /// The doors that exist this frame. The test waits on a commit, which happens a few frames
    /// after the request because the world database answers on its own thread.
    #[derive(Resource, Default)]
    struct DoorWatch(Vec<(Entity, LoadDoor)>);

    fn watch_doors(mut watch: ResMut<DoorWatch>, doors: Query<(Entity, &LoadDoor)>) {
        watch.0 = doors
            .iter()
            .map(|(entity, door)| (entity, door.clone()))
            .collect();
    }

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

    /// The two-cell fixture the crossing test walks through: one Tamriel cell holding a door
    /// whose `XTEL` leads into the interior `Alftand01`, and that interior with one reference.
    ///
    /// `version` is the constant the engine's own schema check reads, so the fixture follows it
    /// if it moves again.
    fn write_fixture(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
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

    /// The two-cell fixture in an app: the world database and the streaming plugin, with the
    /// camera standing at `eye` in the exterior cell (2, -3) whose door leads into the interior 99.
    ///
    /// The reference at 8200, -12200, 50 of that cell renders at (8, 50, -88) while the origin is
    /// the cell, so an eye at (8, 50, -288) is 200 units from the door: inside the pre-stream
    /// radius and still inside the cell.
    fn crossing_fixture(directory: &Path, eye: Vec3) -> (App, Entity) {
        let (database_path, cache_path) = write_fixture(directory);
        let config = EngineConfig {
            worldspace_id: 60,
            start_grid: (2, -3),
            stream_radius: 0,
            unload_radius: 1,
            ..EngineConfig::default()
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(TransformPlugin)
            .add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_asset::<StandardMaterial>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            .insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::new(2, -3)))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .insert_resource(WorldDatabase::open(&database_path).unwrap())
            .insert_resource(AssetCatalog::open(&database_path).unwrap())
            .insert_resource(CellCache::open(&cache_path).unwrap())
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .init_resource::<ProfilingState>()
            .init_resource::<CapturedCrossings>()
            .init_resource::<DoorWatch>()
            .add_plugins(StreamingPlugin)
            .add_systems(Update, (capture_crossings, watch_doors));
        let camera = spawn_camera(&mut app, eye);
        (app, camera)
    }

    /// The single load door of the fixture app, once it has spawned.
    fn fixture_door(app: &mut App) -> (Entity, LoadDoor) {
        run_until(app, "the door of the exterior cell", |app| {
            !app.world().resource::<DoorWatch>().0.is_empty()
        });
        let watch = app.world().resource::<DoorWatch>();
        assert_eq!(watch.0.len(), 1, "one reference of one cell is a door");
        watch.0[0].clone()
    }

    /// Move the camera, the way walking it there would.
    fn move_camera(app: &mut App, camera: Entity, eye: Vec3) {
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = eye;
        app.update();
    }

    #[test]
    fn a_crossing_streams_its_destination_before_the_camera_arrives() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, camera) = crossing_fixture(directory.path(), Vec3::new(8.0, 50.0, -288.0));

        let (door, load_door) = fixture_door(&mut app);
        assert_eq!(load_door.ref_id, 30);
        assert_eq!(load_door.destination.interior_cell_id, Some(99));
        assert_eq!(
            load_door.label, "Alftand01",
            "the label comes from the destination cell's interior_name"
        );

        // The camera approaches the door, so the interior is streamed in before the crossing.
        run_until(
            &mut app,
            "the interior destination to become resident",
            |app| {
                app.world()
                    .resource::<StreamingWorld>()
                    .is_resident(&CellKey::Interior(99))
            },
        );
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.failed_cells, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);

        app.world_mut().write_message(ActivateDoor { door });
        app.update();

        // The cross runs before the plan, so the cell the camera left is already unloaded in the
        // frame it crossed in; one frame later and this would hold whatever the order was.
        assert!(
            !app.world()
                .resource::<StreamingWorld>()
                .is_resident(&CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 2,
                    grid_y: -3,
                }),
            "the cell the camera left is unloaded in the crossing frame"
        );
        app.update();

        assert_eq!(
            *app.world().resource::<ActiveCell>(),
            ActiveCell {
                worldspace_id: 60,
                interior: Some(99),
            }
        );
        let camera_transform = *app.world().entity(camera).get::<Transform>().unwrap();
        let arrival = creation_to_bevy(Vec3::from_array([-947.038, 3958.835, 591.917]))
            + Vec3::Y * crate::player::EYE_HEIGHT;
        assert!(
            camera_transform.translation.abs_diff_eq(arrival, 1.0e-3),
            "camera at {:?}, expected {arrival:?}",
            camera_transform.translation
        );
        assert!(
            camera_transform
                .rotation
                .abs_diff_eq(arrival_camera_rotation([0.0, 0.0, 2.96989]), 1.0e-6)
        );
        let captured = app.world().resource::<CapturedCrossings>();
        assert_eq!(
            captured.0,
            vec![DoorCrossed {
                from_ref_id: 30,
                label: "Alftand01".into(),
            }]
        );
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.origin_rebases, 0, "an interior never rebases");
        assert_eq!(metrics.streaming_invariant_failures, 0);

        // Leaving the interior the way a return crossing does: the exterior becomes active again
        // and the camera lands in it, far enough from the door that it does not pre-stream the
        // interior back in. The interior the camera has left unloads without leaving a root.
        *app.world_mut().resource_mut::<ActiveCell>() = ActiveCell {
            worldspace_id: 60,
            interior: None,
        };
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = Vec3::new(3908.0, 0.0, -3988.0);
        run_until(&mut app, "the cell the camera returns to", |app| {
            app.world()
                .resource::<StreamingWorld>()
                .is_resident(&CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 2,
                    grid_y: -3,
                })
        });
        assert!(
            !app.world()
                .resource::<StreamingWorld>()
                .is_resident(&CellKey::Interior(99)),
            "the interior the camera left is unloaded"
        );
        for _ in 0..5 {
            app.update();
        }
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.failed_cells, 0, "every requested cell exists");
        assert_eq!(metrics.orphaned_cell_roots, 0);
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);
    }

    /// A crossing whose destination is not streamed in yet is held, and made in the frame the
    /// destination becomes resident (design section 4.4): a crossing can never put the player down
    /// in a space that is not there, which is the loading pause this is meant to avoid.
    #[test]
    fn a_crossing_waits_for_its_destination_to_stream_in() {
        let directory = tempfile::tempdir().unwrap();
        // 1000 units from the door - past the 800-unit pre-stream radius, so the interior behind
        // it has not been asked for - and still inside the cell the camera stands in.
        let (mut app, camera) = crossing_fixture(directory.path(), Vec3::new(8.0, 50.0, -1088.0));
        let (door, load_door) = fixture_door(&mut app);
        assert_eq!(load_door.destination.interior_cell_id, Some(99));
        assert!(
            !app.world()
                .resource::<StreamingWorld>()
                .is_resident(&CellKey::Interior(99)),
            "the interior behind the door is not streamed in from here"
        );

        // The player walks through the doorway while the destination is missing: the crossing is
        // held, and nothing moves.
        app.world_mut().write_message(CrossDoor { door });
        app.update();
        assert!(
            app.world().resource::<ActiveCell>().interior.is_none(),
            "a crossing into a cell that is not streamed in waits for it"
        );
        assert!(
            app.world().get::<CrossingHeld>(door).is_some(),
            "and the door it waits on is marked, so it draws shut while there is no window either"
        );
        assert!(app.world().resource::<CapturedCrossings>().0.is_empty());
        assert_eq!(
            app.world()
                .entity(camera)
                .get::<Transform>()
                .unwrap()
                .translation,
            Vec3::new(8.0, 50.0, -1088.0),
            "and the camera stays where the player stands"
        );

        // Walking up to the door puts the interior into the pre-stream, and the held crossing is
        // made in the frame it becomes resident.
        move_camera(&mut app, camera, Vec3::new(8.0, 50.0, -288.0));
        run_until(&mut app, "the held crossing to be made", |app| {
            !app.world().resource::<CapturedCrossings>().0.is_empty()
        });
        assert_eq!(
            *app.world().resource::<ActiveCell>(),
            ActiveCell {
                worldspace_id: 60,
                interior: Some(99),
            }
        );
        assert_eq!(
            app.world().resource::<CapturedCrossings>().0.len(),
            1,
            "one held request is one crossing, not one per frame"
        );
        assert!(
            app.world().get::<CrossingHeld>(door).is_none(),
            "and the hold is taken off the door the frame its crossing is made"
        );
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.failed_cells, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);
    }

    /// Two doorways can be crossed in one frame - a marker beside a door, two markers at a
    /// junction - and only one crossing can be made. It is the door the eye is nearest, whatever
    /// order the requests arrive in: the request read last used to be the one that won, which put
    /// the player through the wrong door whenever two were asked for together.
    #[test]
    fn the_nearest_of_two_doors_asked_for_in_one_frame_is_the_one_crossed() {
        let directory = tempfile::tempdir().unwrap();
        let eye = Vec3::new(8.0, 50.0, -288.0);
        let (mut app, _camera) = crossing_fixture(directory.path(), eye);
        let (near, load_door) = fixture_door(&mut app);
        run_until(
            &mut app,
            "the interior destination to become resident",
            |app| {
                app.world()
                    .resource::<StreamingWorld>()
                    .is_resident(&CellKey::Interior(99))
            },
        );

        // A second door of the same cell, directly away from the camera so that it can only be
        // further off than the first.
        let near_position = app
            .world()
            .entity(near)
            .get::<GlobalTransform>()
            .unwrap()
            .translation();
        let away = (near_position - eye).normalize_or_zero();
        let far_position = near_position + away * 500.0;
        let far = app
            .world_mut()
            .spawn((
                Transform::from_translation(far_position),
                GlobalTransform::from_translation(far_position),
                LoadDoor {
                    ref_id: 31,
                    ..load_door.clone()
                },
            ))
            .id();

        // The near door is asked for first and the far one second: the last request read used to
        // be the one crossed, which is the wrong door here. Both are made in the one frame; the
        // capture is read the frame after, the way the tour reads its own crossings.
        app.world_mut().write_message(CrossDoor { door: near });
        app.world_mut().write_message(CrossDoor { door: far });
        run_until(&mut app, "the nearer door's crossing", |app| {
            !app.world().resource::<CapturedCrossings>().0.is_empty()
        });

        let captured = &app.world().resource::<CapturedCrossings>().0;
        assert_eq!(captured.len(), 1, "one frame is one crossing: {captured:?}");
        assert_eq!(
            captured[0].from_ref_id, load_door.ref_id,
            "the door the eye is nearest is the one crossed"
        );
        assert_eq!(
            *app.world().resource::<ActiveCell>(),
            ActiveCell {
                worldspace_id: 60,
                interior: Some(99),
            },
            "and the crossing is made, into the interior the near door leads to"
        );
    }
}
