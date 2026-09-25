//! First-person player controller for interactive runs (`--walk`). See
//! `docs/design/blackreach-demo.md`.
//!
//! [`PlayerPlugin`] turns the engine's [`StreamingCamera`] into a walking player: mouse look while
//! the cursor is grabbed, WASD movement relative to the current yaw, gravity and the 120-unit eye
//! height, step-up over small ledges, walls that stop motion, `E` to open or close the load door in
//! front, the walk through it ([`player_walks_through_doors`] - the crossing is the engine's, the
//! walk is this controller's), and Skyrim's auto-load doors crossed on contact
//! ([`player_auto_doors`]). One engine unit is one Creation-engine unit (Skyrim's player eye sits
//! about 120 units up, walking is about 150 units/s and running about 350), and Y is up.
//!
//! Controls: left click grabs the cursor, `Escape` releases it, `W`/`A`/`S`/`D` move, `Shift` runs,
//! `Space` jumps, `E` opens the targeted load door and closes it again, `F` toggles a free-flight
//! mode with the old `fly_camera` feel (mouse to look, `Space` up, `Shift` down, `Ctrl` fast).
//! There is nothing to press at an auto-load door: walking into it is the whole interaction.
//!
//! Opening a door is not the same as going through it: `E` opens, and the player walks. That is
//! what makes the crossing invisible - the view never jumps, because the camera is carried through
//! the doorway instead of being put down on the `XTEL` arrival point
//! (`docs/design/animated-doors-and-seamless-crossing.md`, sections 4.2 and 4.3).
//!
//! # Collision: Bevy's mesh ray casting, not the reference bounds
//!
//! Ground and walls are found with Bevy's [`MeshRayCast`] (`bevy_picking`), which works against this
//! engine's meshes:
//!
//! - the streamed references are ordinary main-world `Mesh3d` entities (the glTF world asset is
//!   instantiated as children of the reference entity), so Bevy's `calculate_bounds` gives them an
//!   `Aabb` and the visibility systems give them `InheritedVisibility`;
//! - the glTF loader keeps mesh data in `Assets<Mesh>` (`load_meshes: RenderAssetUsages::default()`
//!   is `MAIN_WORLD | RENDER_WORLD`), which is what `MeshRayCast` reads vertices from;
//! - `VercidiumRendererPlugin` customises `bevy_pbr` (extended materials, GPU preprocessing,
//!   occlusion culling, a water reflection camera) instead of replacing it, so it never removes
//!   those mesh entities.
//!
//! `MeshRayCast` is compiled in because the workspace builds `bevy` with its default features and
//! `3d` pulls in `picking`, which enables `mesh_picking`.
//!
//! Rays are deliberately short. `MeshRayCast` triangle-tests *every* mesh whose `Aabb` the ray
//! crosses, and a terrain quadrant is thousands of triangles, so a single long ray through a
//! streamed grid would cost more than the whole rest of the frame. The ground probe is
//! eye height plus a step (about 220 units) while the player is standing, and grows only by the
//! distance actually fallen; the "is there a world below me at all" probe (one probe per airborne
//! frame, half a cell long) is what tells an unstreamed void from a real drop.
//!
//! Meshes that are still hidden (spawned but not validated yet) are ignored, so ray casting never
//! collides with half-loaded geometry: `RayCastVisibility::Visible` is used rather than `Any`.
//! Water is not a floor either - the player walks the bottom of a lake, not its surface.
//!
//! The movement logic itself only talks to [`CollisionWorld`], which is why the tests below need no
//! GPU and no assets.

use crate::{
    config::EngineConfig,
    demo_hud,
    door_animation::DoorAnimation,
    doors::{DoorAnchor, DoorCrossed, DoorLeaf, DoorState, LoadDoor, mesh_is_out_of_the_way},
    portal::{MIN_PORTAL_DOOR_DISTANCE, PortalQuad, PortalState, measured_portal_extents},
    profiling::ProfilingState,
    streaming::{StreamingWorld, creation_to_bevy},
    transition::{
        CrossDoor, OpenDoor, destination_is_loaded, distance_in_front_of_door, door_is_open,
        source_doorway_centre, source_doorway_frame,
    },
    world::components::{
        CELL_SIZE, ExpectedModelBounds, InstanceBounds, StreamingCamera, WaterSurface,
    },
};
use bevy::{
    input::mouse::AccumulatedMouseMotion,
    picking::mesh_picking::ray_cast::RayMeshHit,
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};
use std::{collections::HashSet, time::Instant};

/// The player's eye height above their feet, in Creation-engine units. Skyrim puts the player's eye
/// about 120 units above the ground.
pub const EYE_HEIGHT: f32 = 120.0;

/// Walking speed, Creation units per second.
pub const WALK_SPEED: f32 = 150.0;
/// Running speed while `Shift` is held, Creation units per second.
pub const RUN_SPEED: f32 = 350.0;
/// Downward acceleration, Creation units per second squared.
pub const GRAVITY: f32 = 1200.0;
/// Upward speed of a jump from the ground. With [`GRAVITY`] this clears about 100 units.
pub const JUMP_SPEED: f32 = 500.0;

/// The highest ledge the player walks up without jumping.
pub const STEP_HEIGHT: f32 = 40.0;
/// How far the ground may drop under a grounded player before they start falling instead of
/// stepping down: this is what keeps a walker attached to stairs and slopes.
pub const SNAP_DOWN: f32 = 60.0;
/// Half the width of the player's body. The wall probes reach this much further than the step the
/// player is about to take, so they stop short of a wall instead of inside it.
pub const BODY_RADIUS: f32 = 32.0;

/// Height above the feet of the lower wall probe.
const KNEE_HEIGHT: f32 = 50.0;
/// Height above the feet of the upper wall probe.
const CHEST_HEIGHT: f32 = 100.0;

/// A load door has to be this close to be usable, in Creation units (a door is about 100 wide).
pub const DOOR_RANGE: f32 = 250.0;

/// The slack the walk-through trigger's volume is grown by before a player counts as inside it: a
/// body radius along the doorway's width and depth and an eye height up and down. A player whose
/// centre is a body radius outside the opening still has their body in it, and the doorway's floor
/// is where their feet are. Both the crossing ([`crosses_the_doorway`], at the point the step
/// crosses the plane) and the refusal to close a door on a player standing in its doorway
/// ([`stands_in_doorway`]) measure against this one volume.
const BODY_SLACK: Vec3 = Vec3::new(BODY_RADIUS, EYE_HEIGHT, BODY_RADIUS);

/// How far in front of the doorway's own plane the crossing is made, in Creation units.
///
/// The window the portal draws through a doorway is only as good as the pose it renders from, and
/// that pose is inside the passage itself for the last few units of the walk: measured on the
/// Alftand route's `DweDoorLarge01Load`, the frame whose eye was 2.4 units short of the plane came
/// out black with the window up (the image gone, not the window), while the frame 2.2 units
/// further back was intact. The swap is therefore made a few units early, which costs nothing: the
/// door -> arrival map is rigid, so the pose the window was rendering from is the pose the player
/// lands on, and the frame before the swap and the frame after it are the same view of the
/// destination. The margin is three or four of those frames' steps, so a slower frame - a step of
/// ten units or more - still cannot land a *window* frame inside the passage.
const DOORWAY_SWAP_DISTANCE: f32 = 8.0;
/// How far off the centre of the view a load door may be and still be targeted.
pub const DOOR_CONE_DEGREES: f32 = 45.0;

/// How much better aimed one load door has to be than another before the aim decides between them
/// rather than the distance ([`target_door`]).
///
/// The aim decides, so this is the width of "the view cannot tell these two apart": a door a whole
/// degree closer to the middle of the view wins wherever it stands, and two doors within a degree
/// of each other are the same thing to look at - one behind the other down a corridor - so the
/// nearer is taken.
///
/// One degree is far tighter than the difference the aim has to separate in practice: the
/// Riverwood Trader's two doors are 3.4 degrees apart from the walk-through's standoff, and any
/// wider a slack would let the nearer one win there again.
pub const TARGET_AIM_SLACK_DEGREES: f32 = 1.0;

/// How deep an auto-load door's trigger volume is, in Creation units: how far in front of and
/// behind the marker the player counts as having walked into it. A doorway is a plane, so the box
/// is only about a step thick.
pub const AUTO_DOOR_TRIGGER_DEPTH: f32 = 60.0;

/// The trigger volume of an auto-load door whose base has no usable bounds - which is every
/// invisible `AutoLoadDoor01` marker: 160 wide, 240 tall and [`AUTO_DOOR_TRIGGER_DEPTH`] deep,
/// centred on the reference's origin.
pub const AUTO_DOOR_MARKER_SIZE: Vec3 = Vec3::new(160.0, 240.0, AUTO_DOOR_TRIGGER_DEPTH);

/// A frame that moved the player further than this did not walk there: a door crossing, a scripted
/// demo-tour move or a `--start-position` put them down. It is one clamped step of the fastest
/// flight ([`FLY_FAST_SPEED`] over [`MAX_STEP_SECONDS`]), which nothing the controller can do in a
/// frame exceeds.
const TELEPORT_STEP: f32 = FLY_FAST_SPEED * MAX_STEP_SECONDS;

/// Free-flight speed, as the old `fly_camera` had it.
pub const FLY_SPEED: f32 = 900.0;
/// Free-flight speed while `Ctrl` is held, as the old `fly_camera` had it.
pub const FLY_FAST_SPEED: f32 = 4000.0;

/// Mouse look sensitivity, radians per logical pixel.
pub const LOOK_SENSITIVITY: f32 = 0.0022;
/// The view never goes past this many degrees up or down, so it cannot flip over at the poles.
pub const PITCH_LIMIT_DEGREES: f32 = 89.0;

/// A frame that hitch (window drag, a cell commit) must not move the player through the world:
/// a step is never simulated for longer than this.
const MAX_STEP_SECONDS: f32 = 0.1;
/// How far below the player to look before deciding that the cell has not streamed yet rather than
/// that the player is over a chasm. Half a cell, because the streamer works in whole cells.
const FALL_LOOKAHEAD: f32 = CELL_SIZE * 0.5;
/// The controls panel's text size once `H` has hidden it down to its one-line reminder.
const CONTROLS_HIDDEN_FONT_SIZE: f32 = 12.0;
/// The one line shown once `H` has hidden the controls panel.
const CONTROLS_HIDDEN_TEXT: &str = "H: show keys";
/// The controls panel's first line while walking: the keys [`PlayerMode::Walk`] answers to.
// ASCII separators: Bevy's default UI font has no middle dot, which rendered as a box.
const WALK_CONTROLS_LINE: &str =
    "Mouse: look (click the window first)  |  WASD: move  |  Shift: run  |  Space: jump";
/// The controls panel's first line while flying: the keys [`PlayerMode::Fly`] answers to instead
/// of the walking ones.
const FLY_CONTROLS_LINE: &str = "Mouse: look (click the window first)  |  WASD: move  |  \
                                  Space / Shift: up / down  |  Ctrl: fast";
/// The controls panel's second line, the same in both modes.
const CONTROLS_LINE_2: &str = "E: open / close door  |  F: walk / fly  |  M: tonemapper  |  \
                                F12: screenshot + note  |  H: hide these keys  |  Esc: release the mouse";
/// Where the door prompt sits: enough below the middle of the screen that it does not sit over
/// where a player naturally looks (the door itself, and whatever is behind it), and close enough
/// to be read without looking away.
const DOOR_PROMPT_TOP: Val = Val::Percent(58.0);
/// The door prompt's text size.
const DOOR_PROMPT_FONT_SIZE: f32 = 20.0;

/// The controls panel's text for the player's current mode: every key that mode answers to, two
/// lines, in the pose tool's own style (`demo_hud`).
fn controls_text(mode: PlayerMode) -> String {
    let first_line = match mode {
        PlayerMode::Walk => WALK_CONTROLS_LINE,
        PlayerMode::Fly => FLY_CONTROLS_LINE,
    };
    format!("{first_line}\n{CONTROLS_LINE_2}")
}

/// How the player moves: on the ground, or free flight.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayerMode {
    /// Gravity, ground following, step-up and walls.
    #[default]
    Walk,
    /// The old `fly_camera` feel: move freely along the view direction, no collision.
    Fly,
}

impl PlayerMode {
    /// The other mode. `F` swaps between them.
    pub fn toggled(self) -> Self {
        match self {
            Self::Walk => Self::Fly,
            Self::Fly => Self::Walk,
        }
    }
}

/// The first-person player, attached to the [`StreamingCamera`] entity: the transform **is** the
/// eye, one [`EYE_HEIGHT`] above the player's feet.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Player {
    /// Rotation about Y, in radians. Zero looks along -Z, as Bevy's `Transform::forward` does.
    pub yaw: f32,
    /// Rotation above the horizon, in radians, clamped to +-89 degrees.
    pub pitch: f32,
    /// Current velocity in Creation units per second. Only Y is driven while walking.
    pub velocity: Vec3,
    /// True while the player is on the ground.
    pub grounded: bool,
    pub mode: PlayerMode,
}

impl Default for Player {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            velocity: Vec3::ZERO,
            grounded: false,
            mode: PlayerMode::Walk,
        }
    }
}

impl Player {
    /// The eye's rotation: yaw about Y, then pitch about the local X, never rolled.
    pub fn look_rotation(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, clamp_pitch(self.pitch), 0.0)
    }

    /// The direction the player is looking, including pitch.
    pub fn forward(&self) -> Vec3 {
        self.look_rotation() * Vec3::NEG_Z
    }
}

/// The eye position of a player whose feet are at `feet`.
pub fn eye_from_feet(feet: Vec3) -> Vec3 {
    feet + Vec3::Y * EYE_HEIGHT
}

/// The feet position of a player whose eye is at `eye`.
pub fn feet_from_eye(eye: Vec3) -> Vec3 {
    eye - Vec3::Y * EYE_HEIGHT
}

/// Keeps the view from reaching straight up or down, where yaw would flip.
pub fn clamp_pitch(pitch: f32) -> f32 {
    let limit = PITCH_LIMIT_DEGREES.to_radians();
    pitch.clamp(-limit, limit)
}

/// The movement keys held in a frame, read once per step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MoveInput {
    /// -1 for `S` (backwards) to 1 for `W` (forwards).
    pub forward: f32,
    /// -1 for `A` (left) to 1 for `D` (right).
    pub right: f32,
    /// `Space`: jump on the ground, ascend in flight.
    pub jump: bool,
    /// `Shift`: run on the ground, descend in flight.
    pub sprint: bool,
    /// `Ctrl`: fast flight, as the old `fly_camera` had it.
    pub fast: bool,
}

impl MoveInput {
    /// Reads the controller's keys out of Bevy's keyboard state.
    pub fn from_keys(keyboard: &ButtonInput<KeyCode>) -> Self {
        Self {
            forward: axis(
                keyboard.pressed(KeyCode::KeyW),
                keyboard.pressed(KeyCode::KeyS),
            ),
            right: axis(
                keyboard.pressed(KeyCode::KeyD),
                keyboard.pressed(KeyCode::KeyA),
            ),
            jump: keyboard.pressed(KeyCode::Space),
            sprint: keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight),
            fast: keyboard.pressed(KeyCode::ControlLeft) || keyboard.pressed(KeyCode::ControlRight),
        }
    }

    /// The horizontal direction this input asks for, in world space.
    pub fn direction(&self, yaw: f32) -> Vec3 {
        movement_direction(self.forward, self.right, yaw)
    }
}

fn axis(positive: bool, negative: bool) -> f32 {
    if positive == negative {
        0.0
    } else if positive {
        1.0
    } else {
        -1.0
    }
}

/// Turns `forward`/`right` input into a horizontal world-space direction for a player facing `yaw`,
/// normalised so that walking diagonally is not faster than walking straight ahead.
pub fn movement_direction(forward: f32, right: f32, yaw: f32) -> Vec3 {
    let local = Vec3::new(right, 0.0, -forward);
    let magnitude = local.length();
    if !magnitude.is_finite() || magnitude <= f32::EPSILON {
        return Vec3::ZERO;
    }
    Quat::from_rotation_y(yaw) * (local.normalize() * magnitude.min(1.0))
}

/// One ray hit in the engine's world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    pub point: Vec3,
    /// Surface normal at the hit. Not guaranteed to be unit length for scaled meshes.
    pub normal: Vec3,
    pub distance: f32,
}

/// The world queries a walking player needs. The real one casts rays into the streamed meshes
/// ([`MeshProbe`]); the tests use a small box world, so the movement logic runs without a GPU or any
/// game data.
pub trait CollisionWorld {
    /// The nearest surface along `direction` within `max_distance`, or `None`.
    fn ray_hit(&mut self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<RayHit>;
}

/// The ground under `eye`, when it is within `reach`.
fn ground_height(world: &mut impl CollisionWorld, eye: Vec3, reach: f32) -> Option<f32> {
    world
        .ray_hit(eye, Vec3::NEG_Y, reach)
        .map(|hit| hit.point.y)
}

/// The normal of the first wall a horizontal motion of `motion` runs into, if any.
fn wall_normal(world: &mut impl CollisionWorld, feet: Vec3, motion: Vec3) -> Option<Vec3> {
    let length = motion.length();
    if !length.is_finite() || length <= f32::EPSILON {
        return None;
    }
    let direction = motion / length;
    let reach = length + BODY_RADIUS;
    // Two heights are enough: the knee ray already clears a step the player is allowed to climb,
    // and together they catch both low rubble and doorframes the eye would look over.
    for height in [KNEE_HEIGHT, CHEST_HEIGHT] {
        if let Some(hit) = world.ray_hit(feet + Vec3::Y * height, direction, reach) {
            return Some(hit.normal);
        }
    }
    None
}

/// Horizontal movement after wall blocking. Sliding along the first surface keeps a wall from
/// feeling like glue: only the part of the motion that goes into the wall is removed.
fn resolve_horizontal(world: &mut impl CollisionWorld, feet: Vec3, desired: Vec3) -> Vec3 {
    let Some(normal) = wall_normal(world, feet, desired) else {
        return desired;
    };
    let slide = Vec3::new(normal.x, 0.0, normal.z).normalize_or_zero();
    if slide.length_squared() <= f32::EPSILON {
        // A face we cannot slide along (a ceiling, or a degenerate normal): just stop.
        return Vec3::ZERO;
    }
    let slid = desired - slide * desired.dot(slide);
    if slid.dot(desired) <= 0.0 {
        // Sliding would push the player back the way they came.
        return Vec3::ZERO;
    }
    if wall_normal(world, feet, slid).is_some() {
        return Vec3::ZERO;
    }
    slid
}

/// What one step did, for the caller to log or show.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalkOutcome {
    /// The eye position after the step.
    pub position: Vec3,
    /// True while the player stands on the ground.
    pub grounded: bool,
    /// No ground was found within reach: the cell has probably not streamed yet, so the vertical
    /// position was held. `F` still toggles flight.
    pub unsupported: bool,
}

/// One frame of walking: walls, gravity, ground following and step-up. `position` is the eye.
///
/// The vertical rule is a single question, asked after the frame's fall has been applied: is the
/// ground within `[feet - SNAP_DOWN, feet + STEP_HEIGHT]`? If it is, the player stands on it (which
/// is both "step up a 30-unit ledge" and "settle 120 units above the floor"); if it is further
/// below, the player keeps falling; if there is nothing below at all within [`FALL_LOOKAHEAD`], the
/// frame's fall is undone and the player holds position.
pub fn walk_step(
    world: &mut impl CollisionWorld,
    player: &mut Player,
    position: Vec3,
    input: MoveInput,
    delta_seconds: f32,
) -> WalkOutcome {
    let dt = clamp_step(delta_seconds);
    let mut feet = feet_from_eye(position);
    let was_grounded = player.grounded;

    let speed = if input.sprint { RUN_SPEED } else { WALK_SPEED };
    feet += resolve_horizontal(world, feet, input.direction(player.yaw) * speed * dt);

    if input.jump && player.grounded {
        player.velocity.y = JUMP_SPEED;
        player.grounded = false;
    }
    if !player.grounded {
        player.velocity.y -= GRAVITY * dt;
    }
    let fall = player.velocity.y * dt;
    feet.y += fall;

    // While standing the probe only has to find the floor under the feet; while falling it has to
    // cover this frame's descent, which is also what stops a fast fall from tunnelling through it.
    let reach = if was_grounded {
        EYE_HEIGHT + STEP_HEIGHT + SNAP_DOWN
    } else {
        EYE_HEIGHT + STEP_HEIGHT + (-fall).max(0.0)
    };
    // A player who was standing steps down small drops and climbs small steps; one who is already
    // falling lands on whatever the probe finds, because the probe only reaches this frame's fall.
    let lowest = if was_grounded {
        -SNAP_DOWN
    } else {
        f32::NEG_INFINITY
    };
    let eye = eye_from_feet(feet);
    let mut unsupported = false;
    // A player on the way up is airborne by definition: the ground they just left is still within
    // the snap distance, and snapping back to it would swallow the jump.
    let rising = player.velocity.y > 0.0;
    match ground_height(world, eye, reach) {
        Some(ground) => {
            let rise = ground - feet.y;
            if !rising && rise >= lowest && rise <= STEP_HEIGHT {
                feet.y = ground;
                player.velocity.y = 0.0;
                player.grounded = true;
            } else {
                // Ground, but too far below the player: keep falling and let a later frame catch it.
                player.grounded = false;
            }
        }
        None => {
            // Nothing within reach. If there is a world further down the player is over a drop and
            // keeps falling; if there is none, the cell has not streamed and the player holds
            // position rather than falling out of the world.
            if ground_height(world, eye, EYE_HEIGHT + FALL_LOOKAHEAD).is_some() {
                player.grounded = false;
            } else {
                feet.y -= fall;
                player.velocity.y = 0.0;
                player.grounded = false;
                unsupported = true;
            }
        }
    }

    WalkOutcome {
        position: eye_from_feet(feet),
        grounded: player.grounded,
        unsupported,
    }
}

/// One frame of free flight: the old `fly_camera` feel, but aimed with mouse look. Flight follows
/// the view in all three axes and ignores the world.
pub fn fly_step(
    player: &mut Player,
    position: Vec3,
    input: MoveInput,
    delta_seconds: f32,
) -> WalkOutcome {
    let dt = clamp_step(delta_seconds);
    let mut local = Vec3::new(input.right, 0.0, -input.forward);
    if input.jump {
        local.y += 1.0;
    }
    if input.sprint {
        local.y -= 1.0;
    }
    let speed = if input.fast {
        FLY_FAST_SPEED
    } else {
        FLY_SPEED
    };
    let position = position + (player.look_rotation() * local).normalize_or_zero() * speed * dt;
    player.velocity = Vec3::ZERO;
    player.grounded = false;
    WalkOutcome {
        position,
        grounded: false,
        unsupported: false,
    }
}

/// A load-door crossing just happened: the streaming side moved and turned the camera. Stop dead and
/// take the yaw the crossing gave the camera - the `XTEL` arrival heading, or the destination
/// *doorway's* own facing on a door whose data carries a doorway anchor
/// ([`crate::doors::DoorAnchor`]) - so the player faces into the room they walked into either way.
/// The pitch is left alone - the player keeps looking where they were looking.
pub fn apply_crossing(player: &mut Player, rotation: Quat) {
    player.velocity = Vec3::ZERO;
    player.grounded = false;
    let (yaw, _, _) = rotation.to_euler(EulerRot::YXZ);
    player.yaw = yaw;
}

/// The [`LoadDoor`] the player is aiming at: the door **in reach** - inside [`DOOR_RANGE`] and
/// within [`DOOR_CONE_DEGREES`] of where they look - whose placement stands nearest the middle of
/// the view, and, between two doors the view cannot tell apart, the nearer of them.
///
/// The aim decides, not the distance, because a door is opened by looking at it: the tour's
/// Riverwood Trader stage failed on the nearest-of-those-in-the-cone rule (impl-182), and the
/// geometry says why it is the wrong question. `E` reaches a door by its **placement**, which
/// stands at the foot of the doorway - 120 units below the eye ([`EYE_HEIGHT`]) - so a player
/// looking straight at a door's leaf is looking 37 degrees *above* the point `E` measures from,
/// and every door is that far off the view's middle. The differences between two doors' aims are
/// therefore small, and "nearest" can easily pick the one being looked past: the Trader's door
/// `0001341F` has a second load door 224 units above it (`00070E69`, its upper storey's doorway),
/// which from the walk-through's 160-unit standoff is 38 units *nearer* and only 3 degrees further
/// off the view. Aiming is what a player means by `E`, and the crosshair they are given is the
/// middle of the view.
///
/// A door's own aim is read as an angle, so two doors the view cannot separate - one behind the
/// other down a corridor, both dead ahead - are decided by distance, as they always were
/// ([`TARGET_AIM_SLACK_DEGREES`] is where "cannot separate" is drawn).
///
/// The margin at the Trader is exactly the two doors' aim difference, a little over three degrees,
/// because both are measured 120 below where the player looks: a player who aims three degrees
/// above the Trader's leaf would take its upper door again. Measuring the aim at the doorways
/// themselves - the placement raised by [`EYE_HEIGHT`], which is what `crate::demo_tour`'s
/// `stand_in_front_of_door` looks at - would put the upper door some sixty degrees off the view
/// instead of three, and is the next step if this thin margin is ever felt; it is left alone here
/// because it also moves the aim for every door whose placement is not at the foot of its doorway.
pub fn target_door<'a>(
    eye: Vec3,
    forward: Vec3,
    doors: impl IntoIterator<Item = (Entity, Vec3, &'a LoadDoor)>,
) -> Option<(Entity, &'a LoadDoor)> {
    let forward = forward.normalize_or_zero();
    if forward.length_squared() < 0.5 {
        return None;
    }
    let cone = DOOR_CONE_DEGREES.to_radians().cos();
    let slack = TARGET_AIM_SLACK_DEGREES.to_radians();
    // The best so far: the entity, its link, how far off the view's middle it is (radians, smaller
    // is better aimed) and how far away it stands.
    let mut best: Option<(Entity, &LoadDoor, f32, f32)> = None;
    for (entity, position, door) in doors {
        let offset = position - eye;
        let distance = offset.length();
        if !distance.is_finite() || distance > DOOR_RANGE {
            continue;
        }
        let Some(direction) = offset.try_normalize() else {
            continue;
        };
        let aim = direction.dot(forward);
        if aim < cone {
            continue;
        }
        let angle = aim.clamp(-1.0, 1.0).acos();
        let better = match best {
            None => true,
            Some((_, _, best_angle, best_distance)) => {
                // Better aimed by more than the slack wins outright; a door within the slack of
                // the best aim is one the view cannot tell from it, and the nearer of those wins.
                angle < best_angle - slack
                    || ((angle - best_angle).abs() <= slack && distance < best_distance)
            }
        };
        if better {
            best = Some((entity, door, angle, distance));
        }
    }
    best.map(|(entity, door, _, _)| (entity, door))
}

/// The box an auto-load door crosses the player in, in the same coordinates as the player's feet.
///
/// The box is the doorway's own: it turns with the reference, and a point is measured in the box's
/// own axes rather than in a world-aligned box around it. For a door placed at an angle the two are
/// not the same thing - the world box around a turned doorway is bigger than the doorway by its
/// whole diagonal, and admitting that would cross a player who walked past the door and not through
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoDoorTrigger {
    /// The middle of the box: what a step has to be moving toward for the door to count as walked
    /// into rather than stepped past.
    centre: Vec3,
    /// The box's own axes: the reference's own rotation.
    rotation: Quat,
    /// How far the box reaches from its middle along each of its own axes.
    half: Vec3,
}

impl AutoDoorTrigger {
    /// Whether a point - the player's feet - is inside the box, edges included.
    pub fn contains(&self, point: Vec3) -> bool {
        let local = self.rotation.inverse() * (point - self.centre);
        local.cmpge(-self.half).all() && local.cmple(self.half).all()
    }

    /// The middle of the box.
    pub fn centre(&self) -> Vec3 {
        self.centre
    }

    /// The same box, grown by `slack` along each of its own axes.
    fn grown(&self, slack: Vec3) -> Self {
        Self {
            half: self.half + slack,
            ..*self
        }
    }
}

/// The volume an auto-load door fires in: its doorway where the base has measurable bounds - the
/// same extents the portal quad uses, so "walking into the door" and "looking through the door"
/// agree - else [`AUTO_DOOR_MARKER_SIZE`] around the reference's origin. The box turns with the
/// door, because the doorway it stands for is the door's own geometry. Which way counts as walking
/// *in* is a separate question, and a door whose link data gives it an outward direction is walked
/// into against that ([`step_is_into_door`]).
pub fn auto_door_trigger(
    position: Vec3,
    rotation: Quat,
    scale: Vec3,
    instance_bounds: Option<&InstanceBounds>,
    expected_bounds: Option<&ExpectedModelBounds>,
) -> AutoDoorTrigger {
    let (half, centre) =
        match measured_portal_extents(instance_bounds, expected_bounds, rotation, scale) {
            Some((size, centre)) => (
                Vec3::new(size.x, size.y, AUTO_DOOR_TRIGGER_DEPTH) * 0.5,
                centre,
            ),
            None => (AUTO_DOOR_MARKER_SIZE * 0.5, Vec3::ZERO),
        };
    AutoDoorTrigger {
        centre: position + rotation * centre,
        rotation,
        half,
    }
}

/// Whether a step from `from` to `to` was the player walking into the door.
///
/// A door whose [`LoadDoor::outward`] is known - the direction its own link data says it faces - is
/// walked into against it: the step has to carry the feet in through that side, whatever it does to
/// the distance from the volume's centre. That is the side the door opens from, and a door model's
/// own axes say it wrongly often enough (see `LoadDoor::outward`) that a brisk step can cross the
/// whole volume and land past its centre in one frame, which the centre rule calls walking *away*.
///
/// Without an outward direction the volume is all there is: the step has to close the distance to
/// its centre. Standing still has no such step by definition, and a step that takes the feet away
/// from the door is not walking into it.
fn step_is_into_door(from: Vec3, to: Vec3, outward: Option<[f32; 3]>, centre: Vec3) -> bool {
    let step = to - from;
    if !step.is_finite() {
        return false;
    }
    let inward = outward
        .map(|outward| -creation_to_bevy(Vec3::from_array(outward)))
        .filter(|inward| inward.is_finite() && inward.length_squared() > 0.5);
    match inward {
        Some(inward) => step.dot(inward) > 0.0,
        None => (centre - to).dot(step) > 0.0,
    }
}

/// Which auto-load doors hold the player's feet, so each of them crosses once per entry.
///
/// A door fires when the feet enter its volume while moving toward the door, and not again until
/// they have left it. Without that latch a player the previous crossing put down beside the return
/// door would be bounced straight back through it. [`AutoDoorLatch::seat`] is how a crossing - or
/// any other move that is not walking - tells the latch that the player is inside a volume without
/// having walked into it.
#[derive(Debug, Default)]
pub struct AutoDoorLatch {
    inside: HashSet<Entity>,
}

impl AutoDoorLatch {
    /// Records where the player is now and reports whether `door` crosses: they are inside its
    /// volume, they were not last time, and the step that took them there was toward the door.
    pub fn entered(&mut self, door: Entity, inside: bool, toward: bool) -> bool {
        let was_inside = if inside {
            !self.inside.insert(door)
        } else {
            self.inside.remove(&door)
        };
        inside && !was_inside && toward
    }

    /// Marks doors as already entered without firing them: the player was put down inside their
    /// volumes, so each one has to be left and walked back into before it crosses.
    pub fn seat(&mut self, doors: impl IntoIterator<Item = Entity>) {
        self.inside.extend(doors);
    }
}

/// A simulation step never covers more than [`MAX_STEP_SECONDS`] of world time.
fn clamp_step(delta_seconds: f32) -> f32 {
    if delta_seconds.is_finite() {
        delta_seconds.clamp(0.0, MAX_STEP_SECONDS)
    } else {
        0.0
    }
}

/// Whether a door's [`GlobalTransform`] has been filled in yet.
///
/// Transform propagation runs in `PostUpdate`, so on the frame its cell commits a door still sits at
/// the render origin - which, after a rebase, is often within the target range of the camera. A door
/// in that state is skipped until the next frame instead of being offered to the player.
fn door_is_placed(transform: &GlobalTransform) -> bool {
    transform.translation() != Vec3::ZERO
        || transform.rotation() != Quat::IDENTITY
        || transform.scale() != Vec3::ONE
}

/// Whether the cell an entity belongs to is part of the active space: the cells the streaming plan
/// holds because the camera is in them, which are the only ones the player walks in and the only
/// ones whose doors they can use.
///
/// A cell the portal has pre-streamed for a doorway is a space of its own, and its raw coordinates
/// can lie anywhere over the player's. Its meshes are drawn by the portal camera alone - a
/// `RenderLayers::layer(DESTINATION_LAYER)` that no visibility query looks at, so `MeshRayCast`'s
/// `RayCastVisibility::Visible` still finds them - or hidden, and its references keep their
/// components either way: without this, an invisible floor or wall of another space is something to
/// stand on and walk into, and its load doors and invisible auto-load markers are real doors. The
/// role itself is the portal's ([`PortalState::is_in_active_space`], the map `isolate_cells`
/// writes), so there is one answer to the question rather than two.
///
/// `true` when there is no portal ([`PortalState`] is only initialized for an interactive run) and
/// for an entity no cell root owns: nothing is isolated then, so nothing is out of the player's
/// reach. The player's systems run before the portal's isolation in the frame, as `PlayerPlugin`
/// and `PortalPlugin` register them, so the roles read here are the ones the previous frame's
/// isolation wrote - the space the camera stands in now.
fn in_active_space(
    portal: Option<&PortalState>,
    entity: Entity,
    parents: &Query<&ChildOf>,
) -> bool {
    portal.is_none_or(|portal| portal.is_in_active_space(entity, parents))
}

/// The walking collision queries, against the meshes the streamer has spawned.
struct MeshProbe<'a, 'w, 's> {
    ray_cast: &'a mut MeshRayCast<'w, 's>,
    /// Meshes that are not something to stand on, whatever they look like from above: the water
    /// surfaces, so the player walks the lake bed rather than its surface.
    skip: &'a dyn Fn(Entity) -> bool,
}

impl CollisionWorld for MeshProbe<'_, '_, '_> {
    fn ray_hit(&mut self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<RayHit> {
        if !max_distance.is_finite() || max_distance <= 0.0 {
            return None;
        }
        let Ok(direction) = Dir3::new(direction) else {
            return None;
        };
        let skip = self.skip;
        // Hidden meshes are the ones still being loaded or that failed validation, so they are left
        // out; water is not a floor.
        let filter = |entity: Entity| !skip(entity);
        let settings = MeshRayCastSettings::default()
            .with_filter(&filter)
            .with_visibility(RayCastVisibility::Visible)
            .always_early_exit();
        let hits = self
            .ray_cast
            .cast_ray(Ray3d::new(origin, direction), &settings);
        let (_, hit): &(Entity, RayMeshHit) = hits.first()?;
        (hit.distance <= max_distance).then_some(RayHit {
            point: hit.point,
            normal: hit.normal,
            distance: hit.distance,
        })
    }
}

/// The load door prompt: a small panel a little below the middle of the screen, in the HUD's
/// shared style ([`demo_hud`]).
#[derive(Component)]
pub(crate) struct DoorPrompt;

/// The controls panel at the bottom-left, in the HUD's shared style: every key a walked demo
/// answers to, always there rather than a one-time hint that faded after 25 seconds and never
/// mentioned `F12` (the user, 2026-09-25). `H` hides it down to [`CONTROLS_HIDDEN_TEXT`]
/// ([`ControlsHidden`], [`player_toggle_controls`]).
#[derive(Component)]
pub(crate) struct ControlsPanel;

/// Whether `H` has hidden the controls panel down to its one-line reminder. A resource, not a
/// field on [`ControlsPanel`], so [`player_toggle_controls`] does not need to query the panel it
/// is not itself drawing.
#[derive(Resource, Default)]
struct ControlsHidden(bool);

impl ControlsHidden {
    /// `H`: shows the panel if it was hidden, hides it if it was not.
    fn toggle(&mut self) {
        self.0 = !self.0;
    }
}

/// The player's own systems, in the order they run: look, walk, then everything the walk did that
/// a door can see. A scripted run that drives the player through the keyboard - the demo tour's
/// walk-through - writes its keys before this set, so the press is in the frame the controller
/// reads it.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlayerInput;

/// Turns the engine's camera into a first-person player. Added only for `--walk`; in that mode the
/// old `fly_camera` system is not registered.
pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<CrossDoor>()
            .add_message::<OpenDoor>()
            .add_message::<DoorCrossed>()
            .init_resource::<ControlsHidden>()
            .add_systems(Startup, setup_player_hud)
            .add_systems(
                Update,
                (
                    attach_player,
                    (
                        player_look,
                        player_walk,
                        player_door,
                        player_auto_doors,
                        player_walks_through_doors,
                    )
                        .chain()
                        .in_set(PlayerInput)
                        // The crossing maps this frame's walk, so the walk comes first: the trigger
                        // reads the feet the controller just placed, and the crossing puts the
                        // camera down before the streaming plan (and the portal) look at it.
                        .before(crate::transition::DoorTransition),
                    // And the player takes the pose the crossing gave the camera, in that frame.
                    player_door_crossed.after(crate::transition::DoorTransition),
                    player_toggle_controls,
                    player_controls_panel,
                    player_cursor_grab,
                ),
            );
    }
}

/// The camera entity before [`attach_player`] has given it a [`Player`].
type UnattachedCameraQuery<'w, 's> =
    Query<'w, 's, (Entity, &'static Transform), (With<StreamingCamera>, Without<Player>)>;

/// Gives the streaming camera a [`Player`], looking wherever the camera already looked, and makes it
/// the camera Bevy draws UI on.
fn attach_player(mut commands: Commands, camera: UnattachedCameraQuery) {
    for (entity, transform) in &camera {
        let (yaw, pitch, _) = transform.rotation.to_euler(EulerRot::YXZ);
        commands.entity(entity).try_insert((
            Player {
                yaw,
                pitch: clamp_pitch(pitch),
                ..default()
            },
            // The UI has no other camera on the primary window; naming this one keeps the prompt
            // and the help line from disappearing if a reflection camera is added later.
            IsDefaultUiCamera,
        ));
    }
}

/// Mouse look, and `F` to switch between walking and flying.
fn player_look(
    keyboard: Res<ButtonInput<KeyCode>>,
    motion: Res<AccumulatedMouseMotion>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mut camera: Query<(&mut Transform, &mut Player), With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok((mut transform, mut player)) = camera.single_mut() else {
        return;
    };
    if keyboard.just_pressed(KeyCode::KeyF) {
        player.mode = player.mode.toggled();
        player.velocity = Vec3::ZERO;
        player.grounded = false;
    }
    // Look only while the cursor is grabbed, so a click into another window does not spin the view.
    if cursor_is_grabbed(&cursors) {
        let delta = motion.delta;
        if delta != Vec2::ZERO {
            // Already a per-frame movement: scaling by delta time would make it frame-rate
            // dependent, as `first_person_view_model` in the Bevy examples notes.
            player.yaw -= delta.x * LOOK_SENSITIVITY;
            player.pitch = clamp_pitch(player.pitch - delta.y * LOOK_SENSITIVITY);
        }
    }
    transform.rotation = player.look_rotation();
    profiler.record_elapsed("player/look", started);
}

/// Walking or flying, once per frame.
///
/// The probe skips the meshes that are drawn but are not something to walk into: every mesh of a
/// cell that is not in the active space ([`in_active_space`]), which is drawn by the portal camera
/// or hidden but is otherwise ordinary geometry of a space of its own; a water surface, so the
/// player walks the lake bed rather than its top; a load door's leaf that its door has opened
/// ([`mesh_is_out_of_the_way`]), because a leaf mid-swing is drawn and the player walking through
/// the doorway it is still swinging out of must not be stopped by it; and the portal's window
/// ([`PortalQuad`]), which stands in the doorway the player is walking through and is a
/// picture of the room beyond rather than a wall.
///
/// `pub(crate)` so that the wiring can be driven end to end in one test: `crate::portal` builds the
/// cells and runs the isolation behind it, and this is the walk whose probes have to stay inside
/// the active space.
#[allow(clippy::too_many_arguments)]
pub(crate) fn player_walk(
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut camera: Query<(&mut Transform, &mut Player), With<StreamingCamera>>,
    mut ray_cast: MeshRayCast,
    water: Query<(), With<WaterSurface>>,
    quad: Query<(), With<PortalQuad>>,
    portal: Option<Res<PortalState>>,
    parents: Query<&ChildOf>,
    leaves: Query<&DoorLeaf>,
    states: Query<&DoorState>,
    mut held: Local<bool>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok((mut transform, mut player)) = camera.single_mut() else {
        return;
    };
    let input = MoveInput::from_keys(&keyboard);
    let outcome = {
        let skip = |entity: Entity| {
            water.get(entity).is_ok()
                || quad.get(entity).is_ok()
                || mesh_is_out_of_the_way(entity, &parents, &leaves, &states)
                || !in_active_space(portal.as_deref(), entity, &parents)
        };
        let mut probe = MeshProbe {
            ray_cast: &mut ray_cast,
            skip: &skip,
        };
        match player.mode {
            PlayerMode::Walk => walk_step(
                &mut probe,
                &mut player,
                transform.translation,
                input,
                time.delta_secs(),
            ),
            PlayerMode::Fly => {
                fly_step(&mut player, transform.translation, input, time.delta_secs())
            }
        }
    };
    transform.translation = outcome.position;
    if outcome.unsupported && !*held {
        warn!("no ground under the player: holding position until the cell streams in (F to fly)");
    }
    *held = outcome.unsupported;
    profiler.record_elapsed("player/move", started);
}

/// What `E` at the load door the player is looking at would do - the one press, three answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoorAction {
    /// Open the door: the doorway is a way through from the first frame of the swing.
    Open,
    /// Open the door once the space behind it is streamed in: the press is answered, and
    /// `crate::door_animation` holds the swing until its destination is there.
    OpenWhenLoaded,
    /// Close the door: the leaf comes back into the doorway from the pose the swing left it.
    Close,
}

impl DoorAction {
    /// The prompt line for a door of this label: the key, what it does, and the door it does it
    /// to.
    fn prompt(self, label: &str) -> String {
        match self {
            DoorAction::Open => format!("E  Open  {label}"),
            DoorAction::OpenWhenLoaded => format!("E  Open  {label} (loading)"),
            DoorAction::Close => format!("E  Close  {label}"),
        }
    }
}

/// Targets the load door in front, opens or closes it on `E`, and shows the prompt while one is
/// targeted and there is something to ask it.
///
/// `E` moves nothing: the player walks through the doorway, and [`player_walks_through_doors`]
/// crosses them when their feet reach its plane. The one press means both directions - `Open` at a
/// closed door and `Close` at an open one - and the door's own state decides which
/// ([`DoorAction`], [`door_action`]).
///
/// An auto-load door is never a target: [`player_auto_doors`] crosses it on contact, so offering
/// `E  Open` for an invisible marker would only be a prompt with no door behind it. Neither is a
/// door that is [`Opening`](DoorState::Opening) or [`Closing`](DoorState::Closing), or an open door
/// whose model has nothing to close it with, or an open one the player is standing inside of (see
/// [`door_action`]).
///
/// The camera is read through its **`Transform`**, not its `GlobalTransform`: the walk moved it
/// earlier in this same `Update`, and `GlobalTransform` is only propagated in `PostUpdate`, so
/// reading that would target from where the player stood a frame ago. The camera is a root entity
/// (spawned on its own by `app::setup_world`), so its `Transform` is its world pose - the same
/// reading the portal makes of it (`portal::MainCameraQuery`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn player_door(
    keyboard: Res<ButtonInput<KeyCode>>,
    camera: Query<(&Transform, &Player), With<StreamingCamera>>,
    doors: PlayerDoorQuery,
    portal: Option<Res<PortalState>>,
    streaming: Option<Res<StreamingWorld>>,
    parents: Query<&ChildOf>,
    mut open: MessageWriter<OpenDoor>,
    mut prompt: Query<(&mut Text, &mut Node), With<DoorPrompt>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok((camera_transform, player)) = camera.single() else {
        return;
    };
    let eye = camera_transform.translation;
    let feet = feet_from_eye(eye);
    let streaming = streaming.as_deref();
    let target = target_door(
        eye,
        player.forward(),
        doors
            .iter()
            .filter(|(entity, transform, _, door, ..)| {
                !door.auto_load
                    && door_is_placed(transform)
                    && in_active_space(portal.as_deref(), *entity, &parents)
            })
            .map(|(entity, transform, _, door, ..)| (entity, transform.translation(), door)),
    )
    // What `E` would do decides whether the door is a target at all: a door mid-swing, or an open
    // one with nothing to close it with, is not something to press `E` at.
    .and_then(|(entity, door)| {
        door_action_at(&doors, entity, feet, streaming).map(|action| (entity, door, action))
    });
    if let Some((entity, _, _)) = target
        && keyboard.just_pressed(KeyCode::KeyE)
    {
        open.write(OpenDoor { door: entity });
    }
    if let Ok((mut text, mut node)) = prompt.single_mut() {
        match target {
            Some((_, door, action)) => {
                let label = action.prompt(&door.label);
                if text.as_str() != label {
                    **text = label;
                }
                node.display = Display::Flex;
            }
            None => node.display = Display::None,
        }
    }
    profiler.record_elapsed("player/door", started);
}

/// Everything `E` at a load door has to read: where it stands, what its model's doorway measures
/// ([`auto_door_trigger`]), its link, its state, its doorway anchor and what its model's animation
/// resolved to.
type PlayerDoorQuery<'world, 'state> = Query<
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
        Option<&'static DoorAnimation>,
    ),
>;

/// What `E` at the door `entity` does right now, from the door's own row and where the player
/// stands: `None` when there is nothing to press `E` for at all.
///
/// A door "in the doorway" is the walk-through trigger's own volume test ([`stands_in_doorway`]),
/// asked only of an open door: see [`door_action`] for why that is where a close is refused.
fn door_action_at(
    doors: &PlayerDoorQuery,
    entity: Entity,
    feet: Vec3,
    streaming: Option<&StreamingWorld>,
) -> Option<DoorAction> {
    let (_, global, local, door, state, instance_bounds, expected_bounds, anchor, animation) =
        doors.get(entity).ok()?;
    let loaded = destination_is_loaded(&door.destination, anchor, streaming);
    let in_doorway = matches!(state, Some(DoorState::Open { .. }))
        && stands_in_doorway(
            &auto_door_trigger(
                global.translation(),
                global.rotation(),
                local.scale,
                instance_bounds,
                expected_bounds,
            ),
            feet,
        );
    // A door whose model has not resolved to an animation yet counts as one with no clips, the
    // same reading `crate::door_animation` gives it: activating it opens the doorway in a frame.
    door_action(
        state,
        animation.is_none_or(DoorAnimation::can_close),
        loaded,
        in_doorway,
    )
}

/// What `E` at a load door does, from its state and where the player stands; `None` for a door
/// there is nothing to ask - no prompt, and the key does nothing.
///
/// * **`Closed`** opens: now when the space behind it is there, and otherwise with the press held
///   until it is ([`DoorAction::OpenWhenLoaded`], `crate::transition::destination_is_loaded`).
///   A door with no destination at all counts as loaded and opens on the spot, as it always has.
/// * **`Open`** closes, unless `can_close` is false - the model has an `Open` clip and no `Close`,
///   which `crate::door_animation` answers by replaying the opening rather than closing - or the
///   player is **standing in the doorway**: the leaf turns solid again the moment the state leaves
///   `Open` ([`mesh_is_out_of_the_way`], which the walk probe reads the same way), so pulling it
///   through the player would shut them into it.
/// * **`Opening`** and **`Closing`** do nothing: the door is on its way somewhere, and the rest
///   pose is the only pose a `Close` clip may run from.
///
/// A door with no [`DoorState`] - a run that does not add the door animation - counts as
/// [`Closed`](DoorState::Closed), which is what [`door_is_open`] says of it too.
fn door_action(
    state: Option<&DoorState>,
    can_close: bool,
    destination_loaded: bool,
    in_doorway: bool,
) -> Option<DoorAction> {
    match state.copied().unwrap_or_default() {
        DoorState::Closed if destination_loaded => Some(DoorAction::Open),
        DoorState::Closed => Some(DoorAction::OpenWhenLoaded),
        DoorState::Open { .. } if in_doorway => None,
        DoorState::Open { .. } if can_close => Some(DoorAction::Close),
        DoorState::Open { .. } | DoorState::Opening | DoorState::Closing => None,
    }
}

/// Whether the player's feet are inside the door's own doorway volume: the walk-through trigger's
/// volume ([`auto_door_trigger`]), grown exactly the way [`crosses_the_doorway`] grows it before a
/// step counts as a walk through the door.
///
/// Standing in the doorway is why a close is refused: the doorway is a way through only while the
/// door is open, and the leaf is drawn solid - and walked into - from the moment `Closing` starts.
fn stands_in_doorway(trigger: &AutoDoorTrigger, feet: Vec3) -> bool {
    trigger.grown(BODY_SLACK).contains(feet)
}

/// A load door reference with what places its trigger volume: the reference's own `Transform` (for
/// its scale), the two bounds the portal measures the same door's doorway from, and the door's own
/// state - which is what says whether it is open.
type DoorTriggerQuery<'world, 'state> = Query<
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
>;

/// Crosses auto-load doors on contact, the way Skyrim's invisible `AutoLoadDoor01` markers work:
/// walk into one and it takes you, with no key to press and no prompt to find.
///
/// The trigger runs on the player's feet and needs a walk: the feet enter the volume while moving
/// toward the door. A frame that moved the player further than any step can ([`TELEPORT_STEP`]) was
/// a crossing, a scripted tour move or a starting position, not a walk, and it seats every volume
/// the player was put down inside instead of firing it - which is what keeps an arrival beside the
/// return door from bouncing straight back through it. A crossing reported by [`DoorCrossed`] seats
/// them the same way, so the two mechanisms cover a crossing that lands the player inside.
///
/// The crossing it asks for is the mapped one ([`CrossDoor`]), like the doorway trigger's: the
/// marker's volume is centred on the reference, so the feet are already in the plane when it fires.
///
/// The feet come from the camera's **`Transform`**, not its `GlobalTransform` (see
/// [`player_door`]), and only a door of the active space is a door at all ([`in_active_space`]): a
/// marker of a pre-streamed cell is an invisible box of another space standing wherever its
/// coordinates happen to lie.
#[allow(clippy::too_many_arguments)]
pub(crate) fn player_auto_doors(
    camera: Query<&Transform, (With<StreamingCamera>, With<Player>)>,
    doors: DoorTriggerQuery,
    portal: Option<Res<PortalState>>,
    parents: Query<&ChildOf>,
    mut crossed: MessageReader<DoorCrossed>,
    mut cross: MessageWriter<CrossDoor>,
    mut latch: Local<AutoDoorLatch>,
    mut last_feet: Local<Option<Vec3>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    let feet = feet_from_eye(camera.translation);
    let put_down = crossed.read().count() > 0;
    let previous = last_feet.replace(feet);
    let teleported =
        put_down || previous.is_none_or(|previous| previous.distance(feet) > TELEPORT_STEP);
    let previous = previous.unwrap_or(feet);

    let mut fired: Option<(Entity, f32)> = None;
    for (entity, global, local, door, _, instance_bounds, expected_bounds, _) in &doors {
        // Only an auto-load door fires on contact, and only once transform propagation has placed
        // it: a door spawned this frame still sits at the render origin, which after a rebase is
        // often right next to the camera. Only a door of the active space fires at all: an
        // invisible marker of a pre-streamed cell stands wherever its own space puts it.
        if !door.auto_load
            || !door_is_placed(global)
            || !in_active_space(portal.as_deref(), entity, &parents)
        {
            continue;
        }
        let trigger = auto_door_trigger(
            global.translation(),
            global.rotation(),
            local.scale,
            instance_bounds,
            expected_bounds,
        );
        let inside = trigger.contains(feet);
        if teleported {
            // Put down inside this volume rather than walked into it.
            if inside {
                latch.seat([entity]);
            }
            continue;
        }
        let toward = step_is_into_door(previous, feet, door.outward, trigger.centre());
        if !latch.entered(entity, inside, toward) {
            continue;
        }
        // One crossing per frame, whichever marker the player is nearest: two markers can hold the
        // feet at once (a junction, or a marker beside a door), and which of them crosses must not
        // depend on the order the doors happen to come in.
        let distance = global.translation().distance(camera.translation);
        if fired.is_none_or(|(_, nearest)| distance < nearest) {
            fired = Some((entity, distance));
        }
    }
    if let Some((door, _)) = fired
        && let Ok((_, _, _, load_door, ..)) = doors.get(door)
    {
        info!(
            door = format_args!("{:08X}", load_door.ref_id),
            destination = %load_door.label,
            "walked into an auto-load door"
        );
        profiler.increment("doors/auto_load", 1);
        cross.write(CrossDoor { door });
    }
    profiler.record_elapsed("player/auto_doors", started);
}

/// Walks the player through an open doorway: the frame the feet cross the doorway's own plane from
/// the side it faces to the side behind it, the crossing fires (design section 4.3).
///
/// * **Open only.** A closed leaf is solid - the walk probe ray-casts the model's meshes - and `E`
///   is what opens it, so a closed door is not somewhere to walk through.
/// * **The doorway's plane, not the reference's.** The plane runs through the centre of the doorway
///   volume the door's model measures ([`auto_door_trigger`]) - the same volume the portal draws
///   through - and a door model need not centre its doorway on its reference (the demo's dwemer
///   doors hang theirs eight units off). A door with a [`DoorAnchor`] puts both in the doorway's own
///   frame instead ([`source_doorway_frame`], [`source_doorway_centre`]), which is the frame and the
///   point the portal's quad carries the destination image in: under the anchored map the two are
///   the same plane. That plane is also where the portal's window ends and the
///   cell swap has to happen - the quad that carries the destination image stands in that plane
///   (`crate::portal`), so the window is drawn up to the frame of the swap and no further - which is
///   what keeps the doorway from showing the wall behind the door for the last step into it.
/// * **Through the doorway, not through its wall.** The plane is infinite; the feet have to cross
///   it inside the doorway's own volume, so walking across the plane beside the door - through an
///   arch in the same wall, say - is not a crossing.
/// * **Once per entry.** A door fires on the step that takes the feet from the front of its plane
///   to the back, so it cannot fire twice on one walk through: the player is behind the plane
///   afterwards, and has to come back out in front of it first.
/// * **No frame without a window.** The step is taken a step *early* where the window the portal
///   draws through would otherwise have ended: at [`MIN_PORTAL_DOOR_DISTANCE`] in front of the
///   doorway's plane (the plane the quad stands in) or in front of the door's own reference plane,
///   whichever the feet reach first. A crossing made one step late leaves exactly one frame with
///   neither the window nor the destination behind the doorway in it - the black frame a
///   walk-through shows - and a crossing made a step early is invisible, because the swap moves the
///   player forward along the walk they are already making.
/// * **Only a walk.** A frame that moved the feet further than any step can was a crossing, a
///   scripted move or a starting position rather than a walk, and fires nothing.
/// * **One door.** Two doorways can be crossed in one frame; the door the eye is nearest is the one
///   the crossing is made for, whatever order the doors come in.
///
/// * **A door of this space only.** A door whose cell is not in the active space is not a way out
///   of the room the player is in, whatever its own coordinates do to their walk: the destination's
///   door stands in the same world, and a pre-streamed cell keeps its doors.
///
/// Auto-load markers are not here at all: they are invisible, have no leaf and no `E`, and
/// [`player_auto_doors`] crosses them on contact.
///
/// The feet come from the camera's **`Transform`**, read in the frame the walk moved it: the portal
/// draws the window through the doorway from the same reading (`portal::MainCameraQuery`), and a
/// trigger one frame behind it makes the crossing a frame after the window ends - the black frame
/// the early swap exists to prevent (see [`DOORWAY_SWAP_DISTANCE`]). `GlobalTransform` is only
/// propagated in `PostUpdate`, so it is always the pose of the frame before.
///
/// `pub(crate)` so that the wiring can be driven end to end in one test: `crate::door_animation`
/// runs the `E` -> `Opening` -> `Open` half of it against a hand-built clip, and this is the half
/// that then has to fire on the doorway's plane.
#[allow(clippy::too_many_arguments)]
pub(crate) fn player_walks_through_doors(
    camera: Query<&Transform, (With<StreamingCamera>, With<Player>)>,
    doors: DoorTriggerQuery,
    portal: Option<Res<PortalState>>,
    parents: Query<&ChildOf>,
    mut crossed: MessageReader<DoorCrossed>,
    mut cross: MessageWriter<CrossDoor>,
    mut last_feet: Local<Option<Vec3>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    let feet = feet_from_eye(camera.translation);
    let previous = last_feet.replace(feet);
    let teleported = crossed.read().count() > 0
        || previous.is_none_or(|previous| previous.distance(feet) > TELEPORT_STEP);
    if teleported {
        // Put down rather than walked - the frame a crossing happens in, a scripted tour move, a
        // starting position. Nothing to read: the feet before and after are in two different
        // spaces.
        return;
    }
    let previous = previous.unwrap_or(feet);

    let mut fired: Option<(Entity, f32)> = None;
    for (entity, global, local, door, open, instance_bounds, expected_bounds, anchor) in &doors {
        if door.auto_load
            || !door_is_open(open)
            || !door_is_placed(global)
            || !in_active_space(portal.as_deref(), entity, &parents)
        {
            continue;
        }
        let position = global.translation();
        let rotation = global.rotation();
        let trigger = auto_door_trigger(
            position,
            rotation,
            local.scale,
            instance_bounds,
            expected_bounds,
        );
        // The doorway's *own* plane: through the centre of the volume the door's model measures,
        // which is not always the reference plane (a dwemer door hangs its leaf eight units off
        // it). That is where the portal's window ends - the quad that carries the destination image
        // stands in that plane, and a camera past it has walked behind the window - so it is where
        // the swap has to happen for the doorway never to show the wall behind the door.
        //
        // A door with a doorway anchor measures the plane in the doorway's own frame and from the
        // doorway's own centre, which is the frame and the point the portal quad is laid out in: the
        // anchored map takes the two doorways onto each other, so the quad and this plane are the
        // same plane on both sides of the crossing. Without an anchor both are what they always
        // were - the model's doorway volume in the door's link-derived frame.
        let frame = source_doorway_frame(rotation, door.outward, anchor);
        let doorway = match anchor {
            Some(anchor) => source_doorway_centre(position, rotation, local.scale, anchor),
            None => trigger.centre(),
        };
        let before = distance_in_front_of_door(doorway, frame, previous);
        let now = distance_in_front_of_door(doorway, frame, feet);
        let reference_front = distance_in_front_of_door(position, frame, feet);
        // The window stops carrying the destination in one of two places, and the crossing is made
        // at whichever comes first so that no frame is left with neither the window nor the
        // destination in it: the pose the window renders from is inside the passage for the last
        // few units before the plane the quad stands in ([`DOORWAY_SWAP_DISTANCE`]), and the portal
        // drops the door `MIN_PORTAL_DOOR_DISTANCE` in front of the door's own reference plane -
        // which the doorway hangs either side of, so which of the two the feet meet first depends
        // on the door. Firing early costs nothing: the swap moves the player forward along the walk
        // they are already making, from a pose the window was rendering from.
        let window = (now - DOORWAY_SWAP_DISTANCE).min(reference_front - MIN_PORTAL_DOOR_DISTANCE);
        if before <= 0.0 || window > 0.0 {
            // Walking away from the door, standing behind its plane, or already through it.
            continue;
        }
        if !crosses_the_doorway(&trigger, previous, feet, before, now) {
            continue;
        }
        // Two doorways can be crossed in one frame - a marker beside a door, or two markers at a
        // junction - and only one crossing can be made: the door the eye is nearest is the one the
        // player is actually walking into, and picks the same door every frame.
        let distance = position.distance(camera.translation);
        if fired.is_none_or(|(_, nearest)| distance < nearest) {
            fired = Some((entity, distance));
        }
    }
    if let Some((door, _)) = fired
        && let Ok((_, _, _, load_door, ..)) = doors.get(door)
    {
        info!(
            door = format_args!("{:08X}", load_door.ref_id),
            destination = %load_door.label,
            "walked through an open load door"
        );
        profiler.increment("doors/walked_through", 1);
        cross.write(CrossDoor { door });
    }
    profiler.record_elapsed("player/through_doors", started);
}

/// Whether the step from `from` to `to` crossed the doorway's plane inside the doorway itself,
/// rather than somewhere along the same plane beside it.
///
/// The point the plane is crossed at is where the step reaches a distance of zero in front of the
/// doorway, and it has to be inside the doorway's volume. That volume is measured from the door's
/// own model and stands on the floor, while what crosses it is the player's feet - so it is widened
/// by a body radius along the doorway's width and depth and by an eye height up and down: a player
/// whose centre is a body radius outside the opening still has their body in it, and the doorway's
/// floor is where their feet are. No further, so crossing the plane beyond the doorway is still not
/// a walk through the door. The volume is the doorway's own box, measured in its own axes
/// ([`AutoDoorTrigger::contains`]), so a door placed at an angle admits its own opening and not the
/// world-aligned box around it.
fn crosses_the_doorway(
    trigger: &AutoDoorTrigger,
    from: Vec3,
    to: Vec3,
    before: f32,
    now: f32,
) -> bool {
    let travelled = before - now;
    if !travelled.is_finite() || travelled <= f32::EPSILON {
        return false;
    }
    let crossing = from.lerp(to, (before / travelled).clamp(0.0, 1.0));
    trigger.grown(BODY_SLACK).contains(crossing)
}

/// A crossing moved the camera: stop the player and take the arrival yaw.
fn player_door_crossed(
    mut crossed: MessageReader<DoorCrossed>,
    mut camera: Query<(&mut Player, &Transform), With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok((mut player, transform)) = camera.single_mut() else {
        return;
    };
    for message in crossed.read() {
        apply_crossing(&mut player, transform.rotation);
        info!(label = %message.label, "crossed a load door");
    }
    profiler.record_elapsed("player/crossing", started);
}

/// `H`: toggles the controls panel between every key and its one-line reminder. Blocked while a
/// note box is open the same way every other key is
/// (`crate::field_notes::block_input_while_typing`, which resets `ButtonInput<KeyCode>` in
/// `PreUpdate`, before this reads it in `Update`).
fn player_toggle_controls(keyboard: Res<ButtonInput<KeyCode>>, mut hidden: ResMut<ControlsHidden>) {
    if keyboard.just_pressed(KeyCode::KeyH) {
        hidden.toggle();
    }
}

/// Fills the controls panel with the keys for the player's current mode, or the one-line
/// [`CONTROLS_HIDDEN_TEXT`] once `H` has hidden it; suppressed like the rest of the HUD in a run
/// whose screenshots must stay clean ([`demo_hud::hidden_for_this_run`]), and also while a
/// `--start-shot` run's own panel is up (`crate::pose_capture::setup_start_shot_hud` hides this
/// one at `Startup`; without this check this system would show it again on the next `Update`).
fn player_controls_panel(
    config: Res<EngineConfig>,
    hidden: Res<ControlsHidden>,
    player: Query<&Player, With<StreamingCamera>>,
    mut panel: Query<(&mut Text, &mut TextFont, &mut Node), With<ControlsPanel>>,
) {
    let Ok((mut text, mut font, mut node)) = panel.single_mut() else {
        return;
    };
    if demo_hud::hidden_for_this_run(&config) || config.portal.start_shot.is_some() {
        node.display = Display::None;
        return;
    }
    let (value, size) = if hidden.0 {
        (CONTROLS_HIDDEN_TEXT.to_owned(), CONTROLS_HIDDEN_FONT_SIZE)
    } else {
        let mode = player
            .single()
            .map_or(PlayerMode::default(), |player| player.mode);
        (controls_text(mode), demo_hud::FONT_SIZE)
    };
    if text.as_str() != value {
        **text = value;
    }
    let size = bevy::text::FontSize::Px(size);
    if font.font_size != size {
        font.font_size = size;
    }
    node.display = Display::Flex;
}

/// Left click grabs the cursor, `Escape` releases it.
///
/// Windows has no cursor lock - `bevy_winit` falls back to confining the cursor to the window - but
/// mouse look still works there, because winit reports raw device motion.
fn player_cursor_grab(
    mouse: Res<ButtonInput<MouseButton>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut cursors: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(mut cursor) = cursors.single_mut() else {
        return;
    };
    let grabbed = cursor.grab_mode != CursorGrabMode::None;
    if !grabbed && mouse.just_pressed(MouseButton::Left) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    } else if grabbed && keyboard.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

/// True while the player holds the cursor.
fn cursor_is_grabbed(cursors: &Query<&CursorOptions, With<PrimaryWindow>>) -> bool {
    cursors
        .iter()
        .any(|cursor| cursor.grab_mode != CursorGrabMode::None)
}

/// Builds the door prompt and the controls panel, in the HUD's shared style
/// ([`demo_hud::panel_node`], [`demo_hud::text`]). Both wrap their panel in an invisible centred
/// or corner-anchored row of their own, always shown, and hide the panel *inside* it instead: a
/// parent whose `Display` is [`Display::None`] hides its children regardless of their own
/// `Display`, which is what suppresses the whole HUD for a run whose screenshots must stay clean
/// ([`demo_hud::hidden_for_this_run`]) without every filling system having to re-check it.
fn setup_player_hud(mut commands: Commands, config: Res<EngineConfig>) {
    let row_display = if demo_hud::hidden_for_this_run(&config) {
        Display::None
    } else {
        Display::Flex
    };
    let (prompt_node, prompt_background) = demo_hud::panel_node(Display::None);
    commands.spawn((
        demo_hud::centered_row(DOOR_PROMPT_TOP, row_display),
        children![(
            DoorPrompt,
            prompt_node,
            prompt_background,
            demo_hud::text(String::new(), DOOR_PROMPT_FONT_SIZE),
        )],
    ));
    // `player_controls_panel` sets the panel's `Display` itself from the first `Update` on, so
    // the spawn's own value only matters for the one Startup frame before it runs.
    let (mut controls_node, controls_background) = demo_hud::panel_node(row_display);
    controls_node.left = demo_hud::MARGIN;
    controls_node.bottom = demo_hud::MARGIN;
    commands.spawn((
        ControlsPanel,
        controls_node,
        controls_background,
        demo_hud::text(controls_text(PlayerMode::default()), demo_hud::FONT_SIZE),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doors::DoorDestination;
    use std::f32::consts::FRAC_PI_2;

    #[test]
    fn the_controls_panel_shows_the_keys_for_the_current_mode() {
        let walk = controls_text(PlayerMode::Walk);
        assert!(walk.contains("Shift: run"), "walking runs: {walk:?}");
        assert!(walk.contains("Space: jump"), "walking jumps: {walk:?}");
        assert!(
            !walk.contains("Ctrl: fast"),
            "flying's keys, not walking's: {walk:?}"
        );

        let fly = controls_text(PlayerMode::Fly);
        assert!(
            fly.contains("Ctrl: fast"),
            "flying is fast on Ctrl: {fly:?}"
        );
        assert!(
            fly.contains("Space / Shift: up / down"),
            "flying goes up and down instead of jumping or running: {fly:?}"
        );
        assert!(
            !fly.contains("Shift: run"),
            "walking's keys, not flying's: {fly:?}"
        );

        // Both modes still open/close doors, switch modes and mention F12 - the user's own ask,
        // 2026-09-25: no UI told them F12 takes a screenshot.
        for text in [&walk, &fly] {
            assert!(text.contains("F12: screenshot"), "{text:?}");
            assert!(text.contains("F: walk / fly"), "{text:?}");
            assert!(text.contains("E: open / close door"), "{text:?}");
            assert!(text.contains("H: hide these keys"), "{text:?}");
        }
    }

    #[test]
    fn h_toggles_the_controls_panel_hidden_state() {
        let mut hidden = ControlsHidden::default();
        assert!(!hidden.0, "the panel starts showing every key");
        hidden.toggle();
        assert!(hidden.0, "H hides it down to the one-line reminder");
        hidden.toggle();
        assert!(!hidden.0, "H again brings every key back");
    }

    /// A world made of axis-aligned boxes: enough to express floors, steps and walls without a
    /// renderer, and the same [`CollisionWorld`] the game's [`MeshProbe`] implements.
    #[derive(Default)]
    struct BoxWorld {
        boxes: Vec<(Vec3, Vec3)>,
    }

    impl BoxWorld {
        /// A floor whose top surface is at `height`, plus a step plateau and a wall the tests add.
        fn flat_ground(height: f32) -> Self {
            Self {
                boxes: vec![(
                    Vec3::new(-1.0e5, height - 1000.0, -1.0e5),
                    Vec3::new(1.0e5, height, 1.0e5),
                )],
            }
        }

        /// A plateau from `x` upwards whose top is `top` above the floor.
        fn plateau(mut self, x: f32, top: f32) -> Self {
            self.boxes
                .push((Vec3::new(x, -1000.0, -1.0e5), Vec3::new(1.0e5, top, 1.0e5)));
            self
        }
    }

    impl CollisionWorld for BoxWorld {
        fn ray_hit(&mut self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<RayHit> {
            let direction = direction.normalize_or_zero();
            if direction.length_squared() < 0.5 {
                return None;
            }
            let mut nearest: Option<RayHit> = None;
            for (min, max) in &self.boxes {
                let Some((distance, normal)) = ray_box(origin, direction, *min, *max, max_distance)
                else {
                    continue;
                };
                if nearest.is_none_or(|hit| distance < hit.distance) {
                    nearest = Some(RayHit {
                        point: origin + direction * distance,
                        normal,
                        distance,
                    });
                }
            }
            nearest
        }
    }

    /// Slab test against a box: the entry distance and the outward normal of the face entered.
    fn ray_box(
        origin: Vec3,
        direction: Vec3,
        min: Vec3,
        max: Vec3,
        max_distance: f32,
    ) -> Option<(f32, Vec3)> {
        let mut near = 0.0_f32;
        let mut far = max_distance;
        let mut normal = Vec3::ZERO;
        for axis in 0..3 {
            let component = direction[axis];
            let (start, low, high) = (origin[axis], min[axis], max[axis]);
            if component.abs() <= f32::EPSILON {
                if start < low || start > high {
                    return None;
                }
                continue;
            }
            let (mut entry, mut exit) = ((low - start) / component, (high - start) / component);
            let mut sign = -1.0;
            if entry > exit {
                std::mem::swap(&mut entry, &mut exit);
                sign = 1.0;
            }
            if entry > near {
                near = entry;
                normal = Vec3::ZERO;
                normal[axis] = sign;
            }
            far = far.min(exit);
            if near > far {
                return None;
            }
        }
        (near <= max_distance).then_some((near, normal))
    }

    /// Runs `walk_step` at 60 Hz and returns the last outcome.
    fn walk(
        world: &mut BoxWorld,
        player: &mut Player,
        mut eye: Vec3,
        input: MoveInput,
        frames: u32,
    ) -> WalkOutcome {
        let mut outcome = WalkOutcome {
            position: eye,
            grounded: false,
            unsupported: false,
        };
        for _ in 0..frames {
            outcome = walk_step(world, player, eye, input, 1.0 / 60.0);
            eye = outcome.position;
        }
        outcome
    }

    fn assert_close(actual: Vec3, expected: Vec3) {
        assert!(
            (actual - expected).length() < 1.0e-5,
            "{actual:?} is not {expected:?}"
        );
    }

    fn forward_input() -> MoveInput {
        MoveInput {
            forward: 1.0,
            ..default()
        }
    }

    /// Walking towards +X. `movement_direction` turns yaw 0 into -Z, so this is a quarter turn back.
    fn facing_positive_x() -> Player {
        Player {
            yaw: -FRAC_PI_2,
            ..default()
        }
    }

    #[test]
    fn movement_follows_the_yaw() {
        assert_close(movement_direction(1.0, 0.0, 0.0), Vec3::NEG_Z);
        assert_close(movement_direction(-1.0, 0.0, 0.0), Vec3::Z);
        assert_close(movement_direction(0.0, 1.0, 0.0), Vec3::X);
        assert_close(movement_direction(0.0, -1.0, 0.0), Vec3::NEG_X);
        assert_close(movement_direction(1.0, 0.0, FRAC_PI_2), Vec3::NEG_X);
        assert_close(movement_direction(1.0, 0.0, -FRAC_PI_2), Vec3::X);
        assert_close(movement_direction(0.0, 0.0, 1.234), Vec3::ZERO);

        let diagonal = movement_direction(1.0, 1.0, 0.0);
        assert!((diagonal.length() - 1.0).abs() < 1.0e-5, "{diagonal:?}");
        assert!(diagonal.x > 0.0 && diagonal.z < 0.0, "{diagonal:?}");
    }

    #[test]
    fn pitch_is_clamped_to_89_degrees() {
        let limit = PITCH_LIMIT_DEGREES.to_radians();
        assert!((clamp_pitch(0.3) - 0.3).abs() < 1.0e-6);
        assert!((clamp_pitch(10.0) - limit).abs() < 1.0e-6);
        assert!((clamp_pitch(-10.0) + limit).abs() < 1.0e-6);

        // Looking as far up as the clamp allows still points up, and never flips over.
        let up = Player {
            pitch: 10.0,
            ..default()
        };
        let facing = up.forward();
        assert!(facing.y < 1.0 && facing.y > 0.999, "{facing:?}");
        assert!(facing.x.abs() < 0.02, "{facing:?}");

        let down = Player {
            pitch: -10.0,
            ..default()
        };
        assert!(down.forward().y < -0.999, "{:?}", down.forward());
        assert!((down.forward() - up.forward()).length() > 1.9);
    }

    #[test]
    fn the_eye_settles_120_units_above_the_ground() {
        let mut world = BoxWorld::flat_ground(64.0);
        let mut player = Player::default();
        let outcome = walk(
            &mut world,
            &mut player,
            Vec3::new(0.0, 500.0, 0.0),
            MoveInput::default(),
            120,
        );
        assert!(outcome.grounded);
        assert!((outcome.position.y - (64.0 + EYE_HEIGHT)).abs() < 1.0e-3);
    }

    #[test]
    fn a_thirty_unit_step_is_climbed() {
        let mut world = BoxWorld::flat_ground(0.0).plateau(100.0, 30.0);
        let mut player = facing_positive_x();
        let eye = Vec3::new(0.0, EYE_HEIGHT, 0.0);
        let outcome = walk(&mut world, &mut player, eye, forward_input(), 120);

        assert!(outcome.position.x > 150.0, "{:?}", outcome.position);
        assert!(outcome.grounded);
        assert!(
            (outcome.position.y - (30.0 + EYE_HEIGHT)).abs() < 1.0e-3,
            "the eye climbed to {:?}",
            outcome.position.y
        );
    }

    #[test]
    fn a_two_hundred_unit_wall_is_not_climbed() {
        let mut world = BoxWorld::flat_ground(0.0).plateau(100.0, 200.0);
        let mut player = facing_positive_x();
        let eye = Vec3::new(0.0, EYE_HEIGHT, 0.0);
        let outcome = walk(&mut world, &mut player, eye, forward_input(), 120);

        assert!(
            outcome.position.x < 100.0,
            "the player walked into the wall: {:?}",
            outcome.position
        );
        // Stopped by the body radius rather than short of the wall, and never rose onto it.
        assert!(
            outcome.position.x > 100.0 - BODY_RADIUS - 3.0,
            "{:?}",
            outcome.position
        );
        assert_eq!(outcome.position.y, EYE_HEIGHT);
    }

    #[test]
    fn walking_off_a_ledge_falls_to_the_floor_below() {
        // A shelf whose top is at y = 0 that stops at x = 100, and a floor 300 units below it: a
        // deeper drop than the snap-down distance, but still inside the lookahead, so the player
        // falls instead of standing in mid-air.
        let mut world = BoxWorld::default();
        world.boxes.push((
            Vec3::new(-1.0e5, -1000.0, -1.0e5),
            Vec3::new(100.0, 0.0, 1.0e5),
        ));
        world.boxes.push((
            Vec3::new(-1.0e5, -1000.0, -1.0e5),
            Vec3::new(1.0e5, -300.0, 1.0e5),
        ));
        let mut player = facing_positive_x();
        let eye = Vec3::new(0.0, EYE_HEIGHT, 0.0);
        let outcome = walk(&mut world, &mut player, eye, forward_input(), 300);

        assert!(outcome.grounded, "{outcome:?}");
        assert!(
            (outcome.position.y - (-300.0 + EYE_HEIGHT)).abs() < 1.0e-3,
            "the player landed at {:?}",
            outcome.position.y
        );
    }

    #[test]
    fn the_player_holds_position_when_there_is_no_ground() {
        let mut world = BoxWorld::default();
        let mut player = Player::default();
        let start = Vec3::new(10.0, 400.0, -3.0);
        let outcome = walk(&mut world, &mut player, start, MoveInput::default(), 120);

        assert!(outcome.unsupported);
        assert!(!outcome.grounded);
        assert_eq!(outcome.position, start);
    }

    #[test]
    fn jumping_leaves_the_ground_and_comes_back_to_it() {
        let mut world = BoxWorld::flat_ground(0.0);
        let mut player = Player::default();
        let mut eye = Vec3::new(0.0, EYE_HEIGHT, 0.0);
        eye = walk(&mut world, &mut player, eye, MoveInput::default(), 5).position;
        assert!(player.grounded);

        let jump = MoveInput {
            jump: true,
            ..default()
        };
        let mut highest = eye.y;
        for frame in 0..60 {
            let input = if frame == 0 {
                jump
            } else {
                MoveInput::default()
            };
            let outcome = walk_step(&mut world, &mut player, eye, input, 1.0 / 60.0);
            eye = outcome.position;
            highest = highest.max(eye.y);
        }

        assert!(
            highest > EYE_HEIGHT + 80.0,
            "the jump only reached {highest}"
        );
        assert!(player.grounded, "{eye:?}");
        assert!((eye.y - EYE_HEIGHT).abs() < 1.0e-3);
    }

    #[test]
    fn a_hitched_frame_does_not_teleport_the_player() {
        let mut world = BoxWorld::flat_ground(0.0);
        let mut player = facing_positive_x();
        let eye = Vec3::new(0.0, EYE_HEIGHT, 0.0);
        let outcome = walk_step(&mut world, &mut player, eye, forward_input(), 5.0);

        let travelled = outcome.position.x - eye.x;
        assert!(
            (travelled - WALK_SPEED * MAX_STEP_SECONDS).abs() < 1.0e-3,
            "a 5 second frame moved the player {travelled} units"
        );
    }

    #[test]
    fn flying_follows_the_view_and_only_the_flight_speed() {
        let mut player = Player {
            pitch: -0.5,
            mode: PlayerMode::Fly,
            ..default()
        };
        let outcome = fly_step(&mut player, Vec3::ZERO, forward_input(), 0.1);

        assert!(outcome.position.z < 0.0, "{:?}", outcome.position);
        assert!(outcome.position.y < 0.0, "{:?}", outcome.position);
        assert!((outcome.position.length() - FLY_SPEED * 0.1).abs() < 1.0e-3);

        let fast = fly_step(
            &mut player,
            Vec3::ZERO,
            MoveInput {
                forward: 1.0,
                fast: true,
                ..default()
            },
            0.1,
        );
        assert!((fast.position.length() - FLY_FAST_SPEED * 0.1).abs() < 1.0e-3);
        assert!(!fast.grounded && !fast.unsupported);
    }

    #[test]
    fn mode_toggles_between_walking_and_flying() {
        assert_eq!(PlayerMode::Walk.toggled(), PlayerMode::Fly);
        assert_eq!(PlayerMode::Fly.toggled(), PlayerMode::Walk);
        assert_eq!(PlayerMode::default(), PlayerMode::Walk);
    }

    /// A door in the view cone is a target, one outside it or out of range is not, and of two doors
    /// the view cannot tell apart - both dead ahead here - the nearer one is taken
    /// (`TARGET_AIM_SLACK_DEGREES`).
    #[test]
    fn a_door_in_the_view_cone_is_targeted() {
        let mut entities = World::new();
        let mut entity = || entities.spawn_empty().id();
        let near = test_door(1, "Alftand");
        let far = test_door(2, "Blackreach");
        let eye = Vec3::new(0.0, 120.0, 0.0);
        let forward = Vec3::NEG_Z;

        let straight_ahead = target_door(
            eye,
            forward,
            [(entity(), Vec3::new(0.0, 120.0, -100.0), &near)],
        )
        .map(|(_, door)| door.label.clone());
        assert_eq!(straight_ahead.as_deref(), Some("Alftand"));

        let behind = target_door(
            eye,
            forward,
            [(entity(), Vec3::new(0.0, 120.0, 100.0), &near)],
        );
        assert!(behind.is_none(), "a door behind the player was targeted");

        let beyond_range = target_door(
            eye,
            forward,
            [(entity(), Vec3::new(0.0, 120.0, -DOOR_RANGE - 1.0), &near)],
        );
        assert!(beyond_range.is_none(), "a door out of range was targeted");

        let in_the_cone = target_door(
            eye,
            forward,
            [(entity(), Vec3::new(-75.0, 120.0, -130.0), &near)],
        );
        assert!(
            in_the_cone.is_some(),
            "a door 30 degrees off centre was missed"
        );

        let beside = target_door(
            eye,
            forward,
            [(entity(), Vec3::new(-150.0, 120.0, -100.0), &near)],
        );
        assert!(
            beside.is_none(),
            "a door 56 degrees to the side was targeted"
        );

        let between = target_door(
            eye,
            forward,
            [
                (entity(), Vec3::new(0.0, 120.0, -200.0), &far),
                (entity(), Vec3::new(0.0, 120.0, -100.0), &near),
            ],
        )
        .map(|(_, door)| door.label.clone());
        assert_eq!(between.as_deref(), Some("Alftand"));

        // Looking at the floor does not pick up a door that is off to the side.
        let looking_down = target_door(
            eye,
            Vec3::new(0.0, -1.0, 0.0),
            [(entity(), Vec3::new(0.0, 120.0, -100.0), &near)],
        );
        assert!(looking_down.is_none());
    }

    /// `E` opens the door the player is aiming at, not whichever door in the cone happens to stand
    /// nearest: the walk-through of the Riverwood Trader's door `0001341F` failed because the
    /// Trader's *upper* door stood 38 units nearer from the standoff (impl-182).
    ///
    /// The numbers are that doorway's, in the frame the walk-through pressed `E` in: the eye 160
    /// units in front of the Trader's placement and 120 above it (`stand_in_front_of_door`), the
    /// door looked at at the foot of its doorway, and the upper door 104 above the eye, 16 to the
    /// side and 124 ahead - `$00070E69` at 162.4 units against the Trader's 200.0.
    #[test]
    fn the_door_the_player_aims_at_beats_a_nearer_one_the_view_glances_past() {
        let mut entities = World::new();
        let mut entity = || entities.spawn_empty().id();
        let trader = test_door(0x0001_341F, "RiverwoodRiverwoodTrader");
        let upper = test_door(0x0007_0E69, "RiverwoodRiverwoodTrader");
        let eye = Vec3::new(0.0, 120.0, 0.0);
        // Looking straight at the Trader's doorway: its leaf stands at the eye's own height, and
        // the placement `E` measures from is the foot of it, 120 below.
        let forward = Vec3::NEG_Z;

        let targeted = target_door(
            eye,
            forward,
            [
                (entity(), Vec3::new(0.0, 0.0, -160.0), &trader),
                (entity(), Vec3::new(16.0, 224.0, -123.7), &upper),
            ],
        )
        .map(|(_, door)| door.ref_id);

        assert_eq!(
            targeted,
            Some(0x0001_341F),
            "the nearer upper door was opened instead of the one being looked at"
        );
    }

    #[test]
    fn a_crossing_stops_the_player_and_takes_the_arrival_yaw() {
        let mut player = Player {
            yaw: 1.0,
            pitch: -0.25,
            velocity: Vec3::new(120.0, -40.0, 7.0),
            grounded: true,
            mode: PlayerMode::Walk,
        };
        apply_crossing(&mut player, Quat::from_euler(EulerRot::YXZ, 0.75, 0.4, 0.0));

        assert_eq!(player.velocity, Vec3::ZERO);
        assert!(!player.grounded);
        assert!((player.yaw - 0.75).abs() < 1.0e-4, "yaw {}", player.yaw);
        assert!(
            (player.pitch + 0.25).abs() < 1.0e-6,
            "the arrival rotation overwrote the player's pitch"
        );
    }

    #[test]
    fn a_door_is_ignored_until_transform_propagation_has_placed_it() {
        // A door spawned this frame still has an identity `GlobalTransform`, which is the render
        // origin - after a rebase that is often right next to the camera.
        assert!(!door_is_placed(&GlobalTransform::default()));
        assert!(!door_is_placed(&GlobalTransform::IDENTITY));
        assert!(door_is_placed(&GlobalTransform::from_translation(
            Vec3::new(0.0, 120.0, -80.0)
        )));
        assert!(door_is_placed(&GlobalTransform::from_xyz(10.0, 0.0, 0.0)));
    }

    fn test_door(ref_id: u32, label: &str) -> LoadDoor {
        LoadDoor {
            ref_id,
            destination: DoorDestination {
                destination_ref_id: ref_id + 100,
                interior_cell_id: Some(ref_id),
                worldspace_id: None,
                arrival_position: [0.0; 3],
                arrival_rotation: [0.0; 3],
            },
            label: label.to_owned(),
            auto_load: false,
            outward: None,
        }
    }

    /// The volume an auto-load marker fires in when its base has no measurable bounds (every
    /// invisible `AutoLoadDoor01`): 160 wide, 240 tall and 60 deep around the reference's origin,
    /// turning with the door.
    #[test]
    fn an_unmeasurable_auto_load_door_gets_the_marker_volume_around_its_origin() {
        let upright = auto_door_trigger(
            Vec3::new(100.0, 0.0, 0.0),
            Quat::IDENTITY,
            Vec3::ONE,
            None,
            None,
        );
        assert_eq!(
            upright.centre(),
            Vec3::new(100.0, 0.0, 0.0),
            "the volume is centred on the door, so a step toward it is a step into the box"
        );
        assert!(
            upright.contains(Vec3::new(100.0, 0.0, 0.0)),
            "the feet at the marker's own origin are inside it"
        );
        assert!(
            upright.contains(Vec3::new(180.0, 120.0, 30.0)),
            "the volume's own corner is the marker's: 160 wide, 240 tall, 60 deep"
        );
        assert!(
            !upright.contains(Vec3::new(100.0, 0.0, 31.0)),
            "a step past the front of the volume is outside it"
        );
        assert!(
            !upright.contains(Vec3::new(181.0, 0.0, 0.0)),
            "and a step past its side is outside it"
        );

        // A door turned a quarter turn measures the same volume in its own axes: 60 units of depth
        // stay 60 units of depth, whatever the world's axes say about them.
        let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let turned = auto_door_trigger(Vec3::ZERO, rotation, Vec3::ONE, None, None);
        for (local, inside) in [
            (Vec3::new(0.0, 0.0, 29.0), true),
            (Vec3::new(0.0, 0.0, 31.0), false),
            (Vec3::new(79.0, 0.0, 0.0), true),
            (Vec3::new(81.0, 0.0, 0.0), false),
            (Vec3::new(0.0, 119.0, 0.0), true),
            (Vec3::new(0.0, 121.0, 0.0), false),
        ] {
            assert_eq!(
                turned.contains(rotation * local),
                inside,
                "{local:?} in the turned door's own axes"
            );
        }
    }

    /// The door's own box, and not the world-aligned box around it: a door placed at 45 degrees has
    /// a world box reaching its diagonal in both axes, and a player crossing the plane at that
    /// corner is walking past the door, not through it.
    #[test]
    fn a_turned_doorway_admits_only_its_own_opening() {
        let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_4);
        let trigger = auto_door_trigger(Vec3::ZERO, rotation, Vec3::ONE, None, None);
        assert_eq!(trigger.centre(), Vec3::ZERO);

        // Inside the door: across its opening, through its thickness, and under its lintel.
        assert!(trigger.contains(Vec3::new(0.0, 0.0, 0.0)));
        assert!(trigger.contains(rotation * Vec3::new(79.0, 0.0, 29.0)));
        assert!(
            !trigger.contains(rotation * Vec3::new(81.0, 0.0, 0.0)),
            "a step past the marker's own width is outside it"
        );

        // The corners of the world-aligned box around the turned marker: 80 across and 30 deep
        // become 77.8 units of `x` and `z` together, and each corner is 110 units from the middle
        // along one of the door's own axes - outside the door. They used to be admitted, which is
        // the over-admission that measuring in the door's own axes avoids.
        let corner = (AUTO_DOOR_MARKER_SIZE.x + AUTO_DOOR_MARKER_SIZE.z)
            * 0.5
            * std::f32::consts::FRAC_1_SQRT_2;
        for corner in [
            Vec3::new(corner, 0.0, corner),
            Vec3::new(corner, 0.0, -corner),
            Vec3::new(-corner, 0.0, corner),
            Vec3::new(-corner, 0.0, -corner),
        ] {
            assert!(
                !trigger.contains(corner),
                "the world box's corner {corner:?} is outside the door"
            );
        }
    }

    /// An auto-load door with bounds fires in its doorway - the extents the portal quad draws - and
    /// not in the marker fallback.
    #[test]
    fn a_measurable_auto_load_door_gets_its_doorway_as_the_volume() {
        let bounds =
            ExpectedModelBounds::new(Vec3::new(-50.0, 0.0, -5.0), Vec3::new(50.0, 200.0, 5.0))
                .unwrap();
        let trigger = auto_door_trigger(
            Vec3::new(0.0, 10.0, 0.0),
            Quat::IDENTITY,
            Vec3::ONE,
            None,
            Some(&bounds),
        );
        assert!(
            trigger
                .centre()
                .abs_diff_eq(Vec3::new(0.0, 110.0, 0.0), 1.0e-3),
            "the doorway's own centre, and not the marker fallback's: {:?}",
            trigger.centre()
        );
        assert!(
            trigger.contains(Vec3::new(50.0, 210.0, 30.0)),
            "the volume's own corner is the doorway's: 100 wide, 200 tall, 60 deep"
        );
        assert!(
            !trigger.contains(Vec3::new(0.0, 110.0, 31.0)),
            "a step past the doorway's front is outside it"
        );
    }

    /// The trigger as the player feels it: walking into the marker crosses it once, standing in it
    /// or walking away does not cross it again, and walking back in does.
    #[test]
    fn an_auto_load_door_crosses_once_per_entry() {
        let door = test_entity(7);
        let mut latch = AutoDoorLatch::default();
        assert!(
            !latch.entered(door, false, true),
            "a step that ends outside the volume is not a crossing"
        );
        assert!(
            latch.entered(door, true, true),
            "walking into the volume crosses the door"
        );
        assert!(
            !latch.entered(door, true, true),
            "still inside: no second crossing"
        );
        assert!(
            !latch.entered(door, true, false),
            "standing still is not walking into it"
        );
        assert!(
            !latch.entered(door, false, false),
            "leaving the volume does not cross it"
        );
        assert!(
            latch.entered(door, true, true),
            "walking back in crosses it again"
        );
    }

    /// Without an outward direction for the door, the step that decides "toward the door" is the one
    /// closing the distance to the volume, not opening it, and nothing at all when the player does
    /// not move.
    #[test]
    fn only_a_step_toward_the_door_counts_as_walking_into_it() {
        let centre = Vec3::ZERO;
        let inside = Vec3::new(0.0, 0.0, 20.0);
        assert!(step_is_into_door(
            Vec3::new(0.0, 0.0, 40.0),
            inside,
            None,
            centre
        ));
        assert!(!step_is_into_door(
            inside,
            Vec3::new(0.0, 0.0, 40.0),
            None,
            centre
        ));
        assert!(
            !step_is_into_door(inside, inside, None, centre),
            "standing still"
        );
        assert!(!step_is_into_door(
            Vec3::new(0.0, 0.0, 20.0),
            Vec3::new(0.0, 0.0, 20.0 + 400.0),
            None,
            centre
        ));
    }

    /// A door whose outward direction is known is walked into against it, whatever the step does to
    /// the distance from the volume's centre - which is the case a brisk step through a thin marker
    /// makes, and the case a door model that points the wrong way hides.
    #[test]
    fn a_door_with_an_outward_direction_is_walked_into_against_it() {
        let centre = Vec3::new(1000.0, 0.0, 1000.0);
        let east = Some([1.0, 0.0, 0.0]); // Creation east is runtime +X.
        let at = |x: f32| centre + Vec3::new(x, 0.0, 0.0);

        // A step from the front (east) that lands past the centre: in, though it is no longer
        // closing on the centre - the fallback rule calls it a step away.
        assert!(step_is_into_door(at(100.0), at(-20.0), east, centre));
        assert!(
            !step_is_into_door(at(100.0), at(-20.0), None, centre),
            "the same step read from the volume's centre alone is not a step toward it"
        );

        // A step away from the door, out of it, and standing still are not walking in.
        assert!(!step_is_into_door(at(-20.0), at(100.0), east, centre));
        assert!(!step_is_into_door(at(-20.0), at(-20.0), east, centre));
        assert!(!step_is_into_door(
            at(20.0),
            Vec3::new(f32::NAN, 0.0, 0.0),
            east,
            centre
        ));
    }

    /// A crossing that lands the player inside the return door's volume must not fire it: they have
    /// to walk out of the volume once first, or arriving would bounce them straight back.
    #[test]
    fn arriving_inside_a_return_door_does_not_cross_until_the_player_leaves() {
        let door = test_entity(8);
        let mut latch = AutoDoorLatch::default();
        latch.seat([door]);
        assert!(
            !latch.entered(door, true, true),
            "being put down inside the volume is not walking into it"
        );
        assert!(
            !latch.entered(door, true, true),
            "and walking on inside it is still not a new entry"
        );
        assert!(!latch.entered(door, false, true), "walking out arms it");
        assert!(
            latch.entered(door, true, true),
            "walking back in crosses the door"
        );
    }

    /// An entity that exists only as an identity in the latch, for the tests that do not run a
    /// world.
    fn test_entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).expect("a test entity index")
    }

    /// A camera driven along a scripted path, one position per frame, the way a player walks.
    #[derive(Resource, Default)]
    struct CameraPath(std::collections::VecDeque<Vec3>);

    /// The doors the door triggers asked to cross, in order.
    #[derive(Resource, Default)]
    struct Crossed(Vec<Entity>);

    fn drive_camera_path(
        mut path: ResMut<CameraPath>,
        mut camera: Query<(&mut Transform, &mut GlobalTransform), With<StreamingCamera>>,
    ) {
        let Ok((mut transform, mut global)) = camera.single_mut() else {
            return;
        };
        if let Some(eye) = path.0.pop_front() {
            transform.translation = eye;
            *global = GlobalTransform::from_translation(eye);
        }
    }

    fn collect_crossings(mut crossed: ResMut<Crossed>, mut requests: MessageReader<CrossDoor>) {
        for message in requests.read() {
            crossed.0.push(message.door);
        }
    }

    /// Just enough app for a door trigger: the player's camera, the doors, and nothing that would
    /// cross a request on to somewhere else.
    fn trigger_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<CrossDoor>()
            .add_message::<DoorCrossed>()
            .init_resource::<ProfilingState>()
            .init_resource::<CameraPath>()
            .init_resource::<Crossed>();
        app
    }

    /// The app the auto-load contact trigger runs in.
    fn auto_door_app() -> App {
        let mut app = trigger_app();
        app.add_systems(
            Update,
            (drive_camera_path, player_auto_doors, collect_crossings).chain(),
        );
        app
    }

    /// The app the doorway plane trigger runs in.
    fn walk_through_app() -> App {
        let mut app = trigger_app();
        app.add_systems(
            Update,
            (
                drive_camera_path,
                player_walks_through_doors,
                collect_crossings,
            )
                .chain(),
        );
        app
    }

    /// Moves the camera the way [`player_walk`] does: the `Transform` alone, leaving the
    /// `GlobalTransform` to bevy's propagation in `PostUpdate` - one frame behind.
    fn move_camera_transform(
        mut path: ResMut<CameraPath>,
        mut camera: Query<&mut Transform, With<StreamingCamera>>,
    ) {
        let Ok(mut transform) = camera.single_mut() else {
            return;
        };
        if let Some(eye) = path.0.pop_front() {
            transform.translation = eye;
        }
    }

    /// A [`walk_through_app`] with bevy's transform propagation running, so a camera moved the way
    /// the controller moves it has its `GlobalTransform` filled in by `PostUpdate`, a frame later -
    /// the order a real frame has, rather than the hand-set `GlobalTransform` the tests above write.
    fn propagated_walk_through_app() -> App {
        let mut app = trigger_app();
        app.add_plugins(bevy::transform::TransformPlugin)
            .add_systems(
                Update,
                (
                    move_camera_transform,
                    player_walks_through_doors,
                    collect_crossings,
                )
                    .chain(),
            );
        app
    }

    /// A streamed door reference, placed as transform propagation would have left it.
    fn spawn_test_door(app: &mut App, position: Vec3, auto_load: bool) -> Entity {
        let mut door = test_door(0x15D48, "Alftand01");
        door.auto_load = auto_load;
        app.world_mut()
            .spawn((
                Transform::from_translation(position),
                GlobalTransform::from_translation(position),
                door,
            ))
            .id()
    }

    /// The same, turned: an auto-load door whose model frame points somewhere other than the
    /// outward direction its link data gives it, which is what most of Skyrim.esm's load door
    /// models do.
    fn spawn_turned_test_door(
        app: &mut App,
        position: Vec3,
        rotation: Quat,
        outward: Option<[f32; 3]>,
    ) -> Entity {
        let mut door = test_door(0x15D48, "Alftand01");
        door.auto_load = true;
        door.outward = outward;
        let transform = Transform::from_translation(position).with_rotation(rotation);
        app.world_mut()
            .spawn((transform, GlobalTransform::from(transform), door))
            .id()
    }

    fn spawn_test_camera(app: &mut App, eye: Vec3) -> Entity {
        app.world_mut()
            .spawn((
                StreamingCamera,
                Player::default(),
                Transform::from_translation(eye),
                GlobalTransform::from_translation(eye),
            ))
            .id()
    }

    /// Walks the test camera through a path, one position per frame.
    fn walk_camera_path(app: &mut App, feet_positions: impl IntoIterator<Item = Vec3>) {
        for feet in feet_positions {
            app.world_mut()
                .resource_mut::<CameraPath>()
                .0
                .push_back(eye_from_feet(feet));
            app.update();
        }
    }

    /// The trigger end to end: a player walking into an invisible marker crosses it exactly once,
    /// and the ordinary load door beyond it never crosses on contact.
    ///
    /// The doors stand away from the render origin: a door whose `GlobalTransform` is still the
    /// identity is one transform propagation has not placed yet, and neither trigger touches it.
    #[test]
    fn walking_into_a_marker_crosses_it_once_and_a_plain_door_never_does() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |z: f32| base + Vec3::new(0.0, 0.0, z);
        let mut app = auto_door_app();
        let marker = spawn_test_door(&mut app, base, true);
        let plain = spawn_test_door(&mut app, at(-600.0), false);
        spawn_test_camera(&mut app, eye_from_feet(at(400.0)));

        // Walk through the marker and then on into the plain door's own volume: the plain door has
        // to be walked into as well, or the test would pass however the trigger treated it.
        walk_camera_path(
            &mut app,
            [
                400.0, 300.0, 200.0, 100.0, 60.0, 40.0, 20.0, 0.0, -20.0, -40.0, -200.0, -400.0,
                -560.0, -580.0, -600.0, -620.0, -700.0,
            ]
            .into_iter()
            .map(at),
        );

        let fired = &app.world().resource::<Crossed>().0;
        assert_eq!(
            fired.iter().filter(|door| **door == marker).count(),
            1,
            "walking into the marker crosses it once, not once per frame: {fired:?}"
        );
        assert!(
            !fired.contains(&plain),
            "an ordinary load door keeps its E key: {fired:?}"
        );
    }

    /// A door whose model points the wrong way still crosses when the player walks in through the
    /// side its link data gives it - and the same walk against the model's own frame does not.
    ///
    /// The marker volume is the model's own box, so here it lies across the walk: the player steps
    /// in through its east side and past its centre in one frame, which the centre rule reads as a
    /// step away from the door. Only the outward direction knows they walked in.
    #[test]
    fn a_door_whose_model_points_the_other_way_crosses_from_its_own_front() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |x: f32| base + Vec3::new(x, 0.0, 0.0);
        // A model frame facing west (runtime -X) with the link data saying the door faces east.
        let model = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        assert!((model * Vec3::NEG_Z).abs_diff_eq(Vec3::NEG_X, 1.0e-5));
        let walk = [200.0, 100.0, -20.0, -200.0];

        let mut app = auto_door_app();
        let door = spawn_turned_test_door(&mut app, base, model, Some([1.0, 0.0, 0.0]));
        spawn_test_camera(&mut app, eye_from_feet(at(200.0)));
        walk_camera_path(&mut app, walk.into_iter().map(at));
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![door],
            "the player walked in from the side the link data gives the door"
        );

        // The same door with no link that leads back into it: nothing says which side its front is
        // on, and this walk does not read as a walk into it.
        let mut blind = auto_door_app();
        let unseen = spawn_turned_test_door(&mut blind, base, model, None);
        spawn_test_camera(&mut blind, eye_from_feet(at(200.0)));
        walk_camera_path(&mut blind, walk.into_iter().map(at));
        assert!(
            !blind.world().resource::<Crossed>().0.contains(&unseen),
            "without an outward direction the volume's centre is all there is to go on"
        );
    }

    /// A crossing that puts the player down inside the return marker's volume: walking on from
    /// there must not fire it until they have left the volume once.
    #[test]
    fn a_crossing_that_lands_inside_the_return_marker_does_not_fire_it() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |z: f32| base + Vec3::new(0.0, 0.0, z);
        let mut app = auto_door_app();
        let return_door = spawn_test_door(&mut app, base, true);
        spawn_test_camera(&mut app, eye_from_feet(at(20.0)));
        app.world_mut().write_message(DoorCrossed {
            from_ref_id: 0x15D48,
            label: "Alftand01".into(),
        });
        app.update();
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "the arrival itself does not cross the door"
        );

        // Still inside the volume and walking on toward the door.
        walk_camera_path(&mut app, [10.0, 0.0, -10.0, -20.0].into_iter().map(at));
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "walking inside the volume the player was put down in does not cross it"
        );

        // Out of the volume, then back into it: that is a walk into the door.
        walk_camera_path(&mut app, [-80.0, -200.0, -80.0, -10.0].into_iter().map(at));
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![return_door],
            "leaving the volume and walking back in crosses it"
        );
    }

    /// A scripted move that puts the player down inside an auto-load volume, with no crossing to
    /// report it - the demo tour's own camera moves, a `--start-position`, a rebase jump - is not a
    /// walk into the door either: the frame moved further than any step can, so the door is seated
    /// and the player has to leave the volume once before it fires.
    #[test]
    fn a_scripted_move_into_the_volume_does_not_fire_the_door() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |z: f32| base + Vec3::new(0.0, 0.0, z);
        let mut app = auto_door_app();
        let door = spawn_test_door(&mut app, base, true);
        spawn_test_camera(&mut app, eye_from_feet(at(2000.0)));

        walk_camera_path(&mut app, [2000.0, 20.0].into_iter().map(at));
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "being put down inside the volume is not walking into the door"
        );

        walk_camera_path(&mut app, [10.0, 0.0, -10.0].into_iter().map(at));
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "walking inside it afterwards is still not a new entry"
        );

        walk_camera_path(&mut app, [-80.0, -200.0, -80.0, -10.0].into_iter().map(at));
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![door],
            "leaving the volume and walking back into it crosses the door"
        );
    }

    /// The doorway trigger end to end: an opened door is crossed by walking through its plane, once
    /// per entry, and only where the doorway is.
    ///
    /// The door's model frame faces `-Z` (`spawn_test_door` gives it no outward direction), so the
    /// side a player walks in from is the negative `z` offset and behind it is the positive one.
    #[test]
    fn walking_through_an_open_door_crosses_it_once_per_entry() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |x: f32, z: f32| base + Vec3::new(x, 0.0, z);
        let mut app = walk_through_app();
        let door = spawn_test_door(&mut app, base, false);
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: false });
        spawn_test_camera(&mut app, eye_from_feet(at(0.0, -400.0)));

        // Walking up to the door and through it: one crossing, on the step that carries the feet
        // from the front of the plane to the back of it.
        walk_camera_path(
            &mut app,
            [
                (0.0, -200.0),
                (0.0, -20.0),
                (0.0, 20.0),
                (0.0, 200.0),
                (0.0, 400.0),
            ]
            .into_iter()
            .map(|(x, z)| at(x, z)),
        );
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![door],
            "walking through an open door crosses it once"
        );

        // On behind it, and then back out in front of it, and through it again: a second entry is
        // a second crossing, and the walk out is not one.
        walk_camera_path(
            &mut app,
            [
                (0.0, 200.0),
                (0.0, 0.0),
                (0.0, -200.0),
                (0.0, -400.0),
                (0.0, -200.0),
                (0.0, -20.0),
                (0.0, 20.0),
            ]
            .into_iter()
            .map(|(x, z)| at(x, z)),
        );
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![door, door],
            "walking back out and in again is a new entry"
        );
    }

    /// A door with a doorway anchor is walked through in the **doorway's own plane**, which is the
    /// plane the portal's quad carries the destination image in and the plane the anchored map
    /// takes onto the destination doorway. The door here has no outward direction, so the frame
    /// without an anchor is the model's own (its plane faces `-Z`); the anchor's doorway faces
    /// `+X`, a quarter turn away, and the crossing has to follow it.
    #[test]
    fn a_doors_walk_through_plane_is_the_doorways_own_under_an_anchor() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let anchor = DoorAnchor {
            tier: crate::doors::DoorAnchorTier::SameModel,
            source_box_centre: [0.0, 88.0, 0.0],
            source_anchor_height: 88.0,
            destination: crate::doors::DoorwayGeometry {
                position: [0.0; 3],
                rotation: [0.0; 3],
                scale: 1.0,
                box_centre: [0.0, 88.0, 0.0],
                anchor_height: 88.0,
            },
            destination_grid: None,
            facings: crate::doors::DoorwayFacings::Known {
                source: core::f32::consts::FRAC_PI_2,
                destination: 0.0,
            },
        };
        // The same walk along the doorway's own axis, at the door's own depth: with the anchor it is
        // through the doorway, without one it runs across the door's model frame and never enters.
        let walk = [200.0_f32, 40.0, -40.0, -200.0];
        let at = |x: f32| base + Vec3::new(x, 0.0, 0.0);

        let mut anchored = walk_through_app();
        let door = spawn_test_door(&mut anchored, base, false);
        anchored
            .world_mut()
            .entity_mut(door)
            .insert((DoorState::Open { animated: false }, anchor));
        spawn_test_camera(&mut anchored, eye_from_feet(at(200.0)));
        walk_camera_path(&mut anchored, walk.into_iter().map(at));
        assert_eq!(
            anchored.world().resource::<Crossed>().0,
            vec![door],
            "the crossing fires on the doorway's own plane"
        );

        let mut plain = walk_through_app();
        let model_planed = spawn_test_door(&mut plain, base, false);
        plain
            .world_mut()
            .entity_mut(model_planed)
            .insert(DoorState::Open { animated: false });
        spawn_test_camera(&mut plain, eye_from_feet(at(200.0)));
        walk_camera_path(&mut plain, walk.into_iter().map(at));
        assert!(
            plain.world().resource::<Crossed>().0.is_empty(),
            "without an anchor the same walk is along the door's own plane and crosses nothing"
        );
    }

    /// The doorway trigger fires only where it should: not through a closed door, not the wrong
    /// way, and not across the same plane beside the doorway.
    #[test]
    fn only_an_open_door_walked_through_its_doorway_crosses() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |x: f32, z: f32| base + Vec3::new(x, 0.0, z);
        let walk = [(-200.0_f32), -20.0, 20.0, 200.0];

        // A closed door: its leaf is solid geometry the walk probe stops at, so walking into its
        // plane is nothing at all.
        let mut app = walk_through_app();
        let closed = spawn_test_door(&mut app, base, false);
        spawn_test_camera(&mut app, eye_from_feet(at(0.0, -400.0)));
        walk_camera_path(&mut app, walk.into_iter().map(|z| at(0.0, z)));
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "a closed door is not a way through"
        );

        // Opened, and walked through the other way round: from behind its plane to in front of it.
        // That is not walking in, and the walk back in that follows is.
        app.world_mut()
            .entity_mut(closed)
            .insert(DoorState::Open { animated: false });
        walk_camera_path(
            &mut app,
            [0.0_f32, -200.0, -400.0, -200.0, -20.0, 20.0]
                .into_iter()
                .map(|z| at(0.0, z)),
        );
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![closed],
            "walking in is one crossing, whatever the walk out did"
        );

        // The plane is infinite, the doorway is not: crossing it 400 units to the side of a
        // 160-wide marker's doorway is not walking through the door.
        let mut beside = walk_through_app();
        let missed = spawn_test_door(&mut beside, base, false);
        beside
            .world_mut()
            .entity_mut(missed)
            .insert(DoorState::Open { animated: false });
        spawn_test_camera(&mut beside, eye_from_feet(at(400.0, -400.0)));
        walk_camera_path(&mut beside, walk.into_iter().map(|z| at(400.0, z)));
        assert!(
            beside.world().resource::<Crossed>().0.is_empty(),
            "the wall beside the door is not the door"
        );
    }

    /// The crossing fires on the doorway's own plane, not on the door's reference plane: a door
    /// whose model measures its doorway off its reference (the demo's dwemer doors hang theirs eight
    /// units off) swaps the player where the doorway is, not a step later inside its thickness -
    /// which is the stretch of the walk where the portal's window has already ended.
    #[test]
    fn the_crossing_fires_on_the_doorways_own_plane() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |z: f32| base + Vec3::new(0.0, 0.0, z);
        let mut app = walk_through_app();
        // The model's own box: 200 wide, 300 tall, and its doorway 40 units in front of the
        // reference (which the door faces, so the model's `-Z`, hence bounds that reach from -80 to
        // 0). The doorway volume the trigger builds is centred on that.
        let bounds = ExpectedModelBounds {
            min: Vec3::new(-100.0, -10.0, -80.0),
            max: Vec3::new(100.0, 290.0, 0.0),
        };
        let door = app
            .world_mut()
            .spawn((
                Transform::from_translation(base),
                GlobalTransform::from_translation(base),
                test_door(0x92809, "Alftand02"),
                DoorState::Open { animated: false },
                bounds,
            ))
            .id();
        spawn_test_camera(&mut app, eye_from_feet(at(-400.0)));

        // The walk up to the doorway's plane: past the reference plane's standoff would still be in
        // front of the doorway, and nothing crosses.
        walk_camera_path(
            &mut app,
            [-300.0_f32, -200.0, -120.0, -80.0, -60.0, -50.0]
                .into_iter()
                .map(at),
        );
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "walking to the reference plane's doorstep is not through the doorway"
        );

        // The next step crosses the doorway's own plane - 40 units in front of the reference.
        walk_camera_path(&mut app, [-30.0_f32].into_iter().map(at));
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![door],
            "the crossing fires on the doorway's own plane"
        );
    }

    /// The crossing is made a step *before* the feet reach the doorway's plane, and not after it.
    /// The window the portal draws through the doorway ends the frame the eye gets to the plane the
    /// quad stands in - and, for a door whose doorway hangs behind its reference, the frame the
    /// portal drops the door `MIN_PORTAL_DOOR_DISTANCE` in front of that. A crossing made a step
    /// late leaves exactly one frame with neither the window nor the destination in the doorway:
    /// the black frame a walk-through shows.
    #[test]
    fn the_crossing_is_made_where_the_window_ends_and_not_a_step_later() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |z: f32| base + Vec3::new(0.0, 0.0, z);

        // A door whose doorway is on its reference (no measurable bounds: the marker volume).
        let mut app = walk_through_app();
        let door = spawn_test_door(&mut app, base, false);
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: false });
        spawn_test_camera(&mut app, eye_from_feet(at(-400.0)));
        walk_camera_path(
            &mut app,
            [-300.0_f32, -100.0, -12.0, -9.0].into_iter().map(at),
        );
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "nine units in front of the doorway is more than the window's last step, and is not \
             through it"
        );
        walk_camera_path(&mut app, [-7.5_f32].into_iter().map(at));
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![door],
            "the last step the window can still carry the destination in is where the crossing is \
             made, rather than at the plane the pose it renders from is already inside"
        );

        // A door whose doorway hangs 30 units *behind* its reference: the portal drops the door one
        // unit in front of the reference plane, 30 units before the doorway's own plane, and the
        // crossing is made there - not 30 units of doorway with no window in it later.
        let mut behind = walk_through_app();
        let bounds = ExpectedModelBounds {
            min: Vec3::new(-100.0, -10.0, 0.0),
            max: Vec3::new(100.0, 290.0, 60.0),
        };
        let door = behind
            .world_mut()
            .spawn((
                Transform::from_translation(base),
                GlobalTransform::from_translation(base),
                test_door(0x92809, "Alftand02"),
                DoorState::Open { animated: false },
                bounds,
            ))
            .id();
        spawn_test_camera(&mut behind, eye_from_feet(at(-400.0)));
        walk_camera_path(
            &mut behind,
            [-300.0_f32, -200.0, -100.0, -40.0, -2.0]
                .into_iter()
                .map(at),
        );
        assert!(
            behind.world().resource::<Crossed>().0.is_empty(),
            "the last two units in front of the reference plane are still in front of the window"
        );
        walk_camera_path(&mut behind, [-0.5_f32].into_iter().map(at));
        assert_eq!(
            behind.world().resource::<Crossed>().0,
            vec![door],
            "the crossing is made where the window ends, not thirty units later at the doorway"
        );
    }

    /// The crossing is made in the frame the walk reaches the doorway, not the frame after it.
    ///
    /// The walk and the crossing detection run in one `Update`: `player_walk` moves the camera's
    /// `Transform`, and `GlobalTransform` is only propagated in `PostUpdate`. A trigger that reads
    /// the camera's `GlobalTransform` therefore decides on the pose the camera had *last* frame -
    /// one frame behind the pose the portal draws the window from, which reads the `Transform`
    /// (`portal::MainCameraQuery`, the same bug the doorway image had). The camera is a root entity,
    /// so its `Transform` is its world pose, and this is the walk of the test above with the camera
    /// moved the way the controller moves it.
    #[test]
    fn a_crossing_reads_this_frames_camera_not_the_last_frames() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |z: f32| base + Vec3::new(0.0, 0.0, z);
        let mut app = propagated_walk_through_app();
        let door = spawn_test_door(&mut app, base, false);
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: false });
        spawn_test_camera(&mut app, eye_from_feet(at(-400.0)));

        // The walk of `the_crossing_is_made_where_the_window_ends_and_not_a_step_later` on a door
        // whose doorway is on its reference: the crossing is made at 7.5 units in front of the
        // doorway, and the frame it is made in is the frame the camera is put there.
        //
        // The walk goes on past that step on purpose. This test walks the path itself and never
        // makes the crossing it asks for, so the feet are left walking the same approach and the
        // doorway fires again on the steps after it - exactly as the same test above would if it
        // walked on; in a real frame the crossing carries the camera into the destination, and the
        // next step is a `TELEPORT_STEP` away from the old space, which seats the door without
        // firing it. Only the frame of the *first* crossing is asserted here, and one frame is all
        // that separates the two readings: the pose this frame's walk reached, or the one the last
        // propagation left behind.
        let walk = [-300.0_f32, -100.0, -12.0, -9.0, -7.5, -6.0];
        let reached = walk.iter().position(|z| *z == -7.5).expect("the step");
        let mut fired_on = None;
        for (frame, z) in walk.into_iter().enumerate() {
            app.world_mut()
                .resource_mut::<CameraPath>()
                .0
                .push_back(eye_from_feet(at(z)));
            app.update();
            if fired_on.is_none() && app.world().resource::<Crossed>().0.contains(&door) {
                fired_on = Some(frame);
            }
        }
        assert_eq!(
            fired_on,
            Some(reached),
            "the crossing is made in the frame the camera reaches the doorway (frame {reached} of \
             the walk), not a frame later on the pose the last propagation left behind"
        );
    }

    /// Two doorways walked into in one frame: one crossing, and it is the door the eye is nearest -
    /// the same door every frame, whatever order the doors come in, rather than whichever request
    /// was written last.
    #[test]
    fn two_doorways_in_one_frame_cross_the_nearest_one() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |x: f32, z: f32| base + Vec3::new(x, 0.0, z);
        let mut app = walk_through_app();
        // Two doorways whose volumes overlap where the player walks between them: one step over the
        // plane is inside both, and both fire.
        let near = spawn_test_door(&mut app, base, false);
        let far = spawn_test_door(&mut app, base + Vec3::new(200.0, 0.0, 0.0), false);
        for door in [near, far] {
            app.world_mut()
                .entity_mut(door)
                .insert(DoorState::Open { animated: false });
        }
        // The eye is 60 units from the near doorway and 140 from the far one when both fire.
        spawn_test_camera(&mut app, eye_from_feet(at(-60.0, -100.0)));
        walk_camera_path(&mut app, [at(-60.0, -100.0), at(60.0, 50.0)]);
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![near],
            "one crossing, for the door the eye is nearest"
        );
    }

    /// A frame that moved the player further than a step can - the demo tour placing the camera, a
    /// `--start-position`, the crossing itself - was not a walk, so it crosses nothing: the feet
    /// were put down somewhere, and walking on from there is not an entry either.
    #[test]
    fn a_scripted_move_across_a_doorway_is_not_a_walk_through_it() {
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        let at = |z: f32| base + Vec3::new(0.0, 0.0, z);
        let mut app = walk_through_app();
        let door = spawn_test_door(&mut app, base, false);
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: false });
        spawn_test_camera(&mut app, eye_from_feet(at(-2000.0)));

        // One frame takes the player from 2000 units in front of the door to 200 behind it.
        walk_camera_path(&mut app, [200.0].into_iter().map(at));
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "being put down behind the door is not walking through it"
        );
        walk_camera_path(&mut app, [400.0, 800.0].into_iter().map(at));
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "and walking on from behind it is not either"
        );
    }

    /// The walking layer rests on Bevy's mesh ray casting actually seeing a spawned mesh. This
    /// builds the smallest app that has mesh assets and the visibility systems, spawns a floor, and
    /// casts through the same [`MeshProbe`] the game uses.
    #[test]
    fn mesh_ray_cast_sees_a_spawned_floor() {
        #[derive(Resource, Default)]
        struct Probed(Option<Vec3>);

        fn probe(
            mut ray_cast: MeshRayCast,
            water: Query<(), With<WaterSurface>>,
            mut out: ResMut<Probed>,
        ) {
            let skip = |entity: Entity| water.get(entity).is_ok();
            let mut probe = MeshProbe {
                ray_cast: &mut ray_cast,
                skip: &skip,
            };
            out.0 = probe
                .ray_hit(Vec3::new(0.0, 500.0, 0.0), Vec3::NEG_Y, 1000.0)
                .map(|hit| hit.point);
        }

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::mesh::MeshPlugin,
            bevy::transform::TransformPlugin,
            bevy::camera::visibility::VisibilityPlugin,
        ))
        .init_resource::<Probed>()
        .add_systems(Update, probe);
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Plane3d::default().mesh().size(2000.0, 2000.0));
        app.world_mut().spawn((Mesh3d(mesh), Transform::default()));

        app.update();
        app.update();

        let hit = app
            .world()
            .resource::<Probed>()
            .0
            .expect("the floor was not hit");
        assert!(hit.y.abs() < 1.0e-3, "hit {hit:?}");
        assert!(hit.x.abs() < 1.0e-3 && hit.z.abs() < 1.0e-3, "hit {hit:?}");
    }

    // ---------------------------------------------------------------------------------------------
    // `E`: opening, closing, and waiting for the space behind the door
    // ---------------------------------------------------------------------------------------------

    /// Where the test player stands, and where the test door's doorway is: far enough from the
    /// render origin that transform propagation has certainly placed the door
    /// ([`door_is_placed`]), and a plain point so the geometry is the arithmetic in the tests.
    const DOOR_FEET: Vec3 = Vec3::new(1000.0, 0.0, 1000.0);

    /// The doors `E` was pressed on, in order.
    #[derive(Resource, Default)]
    struct Opened(Vec<Entity>);

    fn collect_opened(mut opened: ResMut<Opened>, mut requests: MessageReader<OpenDoor>) {
        for request in requests.read() {
            opened.0.push(request.door);
        }
    }

    /// The app `E` is pressed in: the real [`player_door`] over hand-built doors, with the requests
    /// it writes collected and the prompt it fills read straight off the entity. It is
    /// [`PlayerPlugin`]'s door half and nothing else of the game.
    fn door_key_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(ButtonInput::<KeyCode>::default())
            .add_message::<OpenDoor>()
            .init_resource::<ProfilingState>()
            .init_resource::<Opened>()
            .add_systems(Update, (player_door, collect_opened).chain());
        let prompt = app
            .world_mut()
            .spawn((
                DoorPrompt,
                Text::new(""),
                Node {
                    display: Display::None,
                    ..default()
                },
            ))
            .id();
        (app, prompt)
    }

    /// The prompt line the HUD entity carries, and whether it is shown at all.
    fn prompt_line(app: &App, prompt: Entity) -> (String, bool) {
        let text = app
            .world()
            .entity(prompt)
            .get::<Text>()
            .expect("the prompt's text");
        let node = app
            .world()
            .entity(prompt)
            .get::<Node>()
            .expect("the prompt's node");
        (text.as_str().to_owned(), node.display != Display::None)
    }

    /// A load door reference standing one eye height above the ground `standoff` units down the
    /// player's view, with the player's feet on the ground in front of it.
    ///
    /// The reference is put at eye height so that the doorway a model-less door measures
    /// ([`AUTO_DOOR_MARKER_SIZE`], centred on the reference) reaches from the floor to above the
    /// player's head, and so that `target_door`'s cone is decided by the horizontal distance alone
    /// - the eye looks straight at the door.
    fn door_scene(app: &mut App, state: DoorState, standoff: f32) -> (Entity, Entity) {
        let door = app
            .world_mut()
            .spawn((
                Transform::from_translation(DOOR_FEET + Vec3::Y * EYE_HEIGHT),
                GlobalTransform::from_translation(DOOR_FEET + Vec3::Y * EYE_HEIGHT),
                test_door(0x15D48, "Alftand01"),
                state,
            ))
            .id();
        let camera = app
            .world_mut()
            .spawn((
                StreamingCamera,
                Player::default(),
                Transform::from_translation(eye_from_feet(DOOR_FEET + Vec3::Z * standoff)),
                GlobalTransform::from_translation(eye_from_feet(DOOR_FEET + Vec3::Z * standoff)),
            ))
            .id();
        (door, camera)
    }

    /// Walks the test camera's feet to `standoff` units in front of the door.
    fn stand_in_front_of_the_door(app: &mut App, camera: Entity, standoff: f32) {
        let eye = eye_from_feet(DOOR_FEET + Vec3::Z * standoff);
        app.world_mut()
            .entity_mut(camera)
            .insert(Transform::from_translation(eye));
    }

    /// One frame with `E` pressed: the input is reset first because there is no input plugin in
    /// this app to clear a press between frames, and `ButtonInput::press` only registers a fresh
    /// press on a key that is not already down.
    fn press_e(app: &mut App) {
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.reset_all();
            keys.press(KeyCode::KeyE);
        }
        app.update();
    }

    /// One frame with no key held, and nothing left of the last press.
    fn no_keys(app: &mut App) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        app.update();
    }

    /// The feature: `E` at a closed door opens it, and the same key at the open door closes it
    /// again - the door's own state decides which, and the prompt says which one is coming.
    #[test]
    fn e_opens_a_closed_door_and_closes_the_open_one() {
        let (mut app, prompt) = door_key_app();
        let (door, camera) = door_scene(&mut app, DoorState::Closed, 150.0);
        stand_in_front_of_the_door(&mut app, camera, 150.0);
        no_keys(&mut app);

        assert_eq!(
            prompt_line(&app, prompt),
            ("E  Open  Alftand01".to_owned(), true),
            "a closed door is offered, and `E` would open it"
        );
        press_e(&mut app);
        assert_eq!(
            app.world().resource::<Opened>().0,
            vec![door],
            "`E` asks for the door to open"
        );

        // The state machine answered (`crate::door_animation`): the door is open now, and the same
        // press at it is a close.
        app.world_mut()
            .entity_mut(door)
            .insert(DoorState::Open { animated: true });
        no_keys(&mut app);
        assert_eq!(
            prompt_line(&app, prompt),
            ("E  Close  Alftand01".to_owned(), true),
            "an open door is offered as a close"
        );
        press_e(&mut app);
        assert_eq!(
            app.world().resource::<Opened>().0,
            vec![door, door],
            "and `E` asks for it again: one press, both directions, the state deciding which"
        );
    }

    /// A door whose far side is not streamed in yet is still something to press `E` at - the press
    /// is taken and the door holds it (`crate::door_animation`) - and the prompt says why nothing
    /// has happened yet.
    #[test]
    fn a_door_whose_far_side_is_not_loaded_says_loading_and_takes_the_press() {
        let (mut app, prompt) = door_key_app();
        // The streamer is there and no cell of this app is resident: the door's link leads to an
        // interior the fixture app has never loaded.
        app.init_resource::<StreamingWorld>();
        let (door, camera) = door_scene(&mut app, DoorState::Closed, 150.0);
        stand_in_front_of_the_door(&mut app, camera, 150.0);
        no_keys(&mut app);

        assert_eq!(
            prompt_line(&app, prompt),
            ("E  Open  Alftand01 (loading)".to_owned(), true),
            "the prompt says the door is waiting for the space behind it"
        );
        press_e(&mut app);
        assert_eq!(
            app.world().resource::<Opened>().0,
            vec![door],
            "and the press is taken: the door opens once its far side is loaded"
        );
    }

    /// A close is refused while the player stands in the doorway: the leaf turns solid the moment
    /// the state leaves `Open` ([`mesh_is_out_of_the_way`]), so pulling it through them would shut
    /// them into it. The same door from a step further out closes, which is what makes the refusal
    /// about where the player stands and not about the door.
    #[test]
    fn no_close_while_the_player_stands_in_the_doorway() {
        let (mut app, prompt) = door_key_app();
        // 50 units from the plane: inside the doorway volume a model-less door measures (60 deep,
        // grown by a body radius each way).
        let (door, camera) = door_scene(&mut app, DoorState::Open { animated: true }, 50.0);
        no_keys(&mut app);

        assert_eq!(
            prompt_line(&app, prompt),
            (String::new(), false),
            "no prompt while the player stands in the doorway"
        );
        press_e(&mut app);
        assert!(
            app.world().resource::<Opened>().0.is_empty(),
            "and `E` does nothing: the door would close on the player"
        );

        // A step back out of the doorway, and the same door is closed by the same press.
        stand_in_front_of_the_door(&mut app, camera, 150.0);
        no_keys(&mut app);
        assert_eq!(
            prompt_line(&app, prompt),
            ("E  Close  Alftand01".to_owned(), true),
            "the doorway is behind the player now"
        );
        press_e(&mut app);
        assert_eq!(app.world().resource::<Opened>().0, vec![door]);
    }

    /// `E` at a door that is opening or closing does nothing at all: the door is on its way
    /// somewhere, and the rest pose is the only pose a `Close` clip may run from.
    #[test]
    fn e_at_a_door_mid_swing_does_nothing() {
        for state in [DoorState::Opening, DoorState::Closing] {
            let (mut app, prompt) = door_key_app();
            door_scene(&mut app, state, 150.0);
            no_keys(&mut app);

            assert_eq!(
                prompt_line(&app, prompt),
                (String::new(), false),
                "{state:?} is not offered"
            );
            press_e(&mut app);
            assert!(
                app.world().resource::<Opened>().0.is_empty(),
                "and `E` does nothing at a door that is {state:?}"
            );
        }
    }

    /// The rule `E` follows, in one place: what the door's state, its model's ability to close,
    /// its destination and where the player stands add up to. An open door whose model has nothing
    /// to close it with is the one the brief calls out - activating it replays its opening
    /// (`crate::door_animation`, design section 5), so nothing offers it as a close.
    #[test]
    fn the_door_action_is_what_the_state_and_the_player_make_of_it() {
        let closed = DoorState::Closed;
        let open = DoorState::Open { animated: true };
        assert_eq!(
            door_action(Some(&closed), true, true, false),
            Some(DoorAction::Open)
        );
        assert_eq!(
            door_action(Some(&closed), true, false, false),
            Some(DoorAction::OpenWhenLoaded),
            "a closed door with nothing behind it yet is a press that waits"
        );
        assert_eq!(
            door_action(Some(&open), true, true, false),
            Some(DoorAction::Close)
        );
        assert_eq!(
            door_action(Some(&open), false, true, false),
            None,
            "an open door with no `Close` clip has nothing to close it with"
        );
        assert_eq!(
            door_action(Some(&open), true, true, true),
            None,
            "and an open door the player stands inside of is never pulled shut"
        );
        assert_eq!(
            door_action(Some(&DoorState::Opening), true, true, false),
            None
        );
        assert_eq!(
            door_action(Some(&DoorState::Closing), true, true, false),
            None
        );
        assert_eq!(
            door_action(None, true, true, false),
            Some(DoorAction::Open),
            "a door with no state counts as closed, which is what `door_is_open` says of it too"
        );
    }

    /// The prompt says which way `E` will take the door, and that a door waiting for its far side
    /// is waiting.
    #[test]
    fn the_prompt_says_what_e_would_do() {
        assert_eq!(
            DoorAction::Open.prompt("Sven's House"),
            "E  Open  Sven's House"
        );
        assert_eq!(
            DoorAction::OpenWhenLoaded.prompt("Sven's House"),
            "E  Open  Sven's House (loading)"
        );
        assert_eq!(
            DoorAction::Close.prompt("Sven's House"),
            "E  Close  Sven's House"
        );
    }
}
