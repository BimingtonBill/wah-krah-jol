//! First-person player controller for interactive runs (`--walk`). See
//! `docs/design/blackreach-demo.md`.
//!
//! [`PlayerPlugin`] turns the engine's [`StreamingCamera`] into a walking player: mouse look while
//! the cursor is grabbed, WASD movement relative to the current yaw, gravity and the 120-unit eye
//! height, step-up over small ledges, walls that stop motion, and `E` to open the load door in
//! front. One engine unit is one Creation-engine unit (Skyrim's player eye sits about 120 units up,
//! walking is about 150 units/s and running about 350), and Y is up.
//!
//! Controls: left click grabs the cursor, `Escape` releases it, `W`/`A`/`S`/`D` move, `Shift` runs,
//! `Space` jumps, `E` opens the targeted load door, `F` toggles a free-flight mode with the old
//! `fly_camera` feel (mouse to look, `Space` up, `Shift` down, `Ctrl` fast).
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
    doors::{ActivateDoor, DoorCrossed, LoadDoor},
    profiling::ProfilingState,
    world::components::{CELL_SIZE, StreamingCamera, WaterSurface},
};
use bevy::{
    input::mouse::AccumulatedMouseMotion,
    picking::mesh_picking::ray_cast::RayMeshHit,
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};
use std::time::Instant;

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
/// How far off the centre of the view a load door may be and still be targeted.
pub const DOOR_CONE_DEGREES: f32 = 45.0;

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
/// The help line is a one-time hint; it disappears after this many seconds.
const HELP_LINE_SECONDS: f32 = 25.0;
/// The one-time help line, exactly as the brief writes it.
const HELP_TEXT: &str = "WASD move · Shift run · Space jump · F fly · E open · Esc cursor";

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
/// take the arrival yaw, so the player faces the way the door's `XTEL` recorded. The pitch is left
/// alone - the player keeps looking where they were looking.
pub fn apply_crossing(player: &mut Player, rotation: Quat) {
    player.velocity = Vec3::ZERO;
    player.grounded = false;
    let (yaw, _, _) = rotation.to_euler(EulerRot::YXZ);
    player.yaw = yaw;
}

/// The nearest [`LoadDoor`] the player can use: inside [`DOOR_RANGE`] and within
/// [`DOOR_CONE_DEGREES`] of where they look.
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
    let mut best: Option<(Entity, &LoadDoor, f32)> = None;
    for (entity, position, door) in doors {
        let offset = position - eye;
        let distance = offset.length();
        if !distance.is_finite() || distance > DOOR_RANGE {
            continue;
        }
        let Some(direction) = offset.try_normalize() else {
            continue;
        };
        if direction.dot(forward) < cone {
            continue;
        }
        if best.is_none_or(|(_, _, closest)| distance < closest) {
            best = Some((entity, door, distance));
        }
    }
    best.map(|(entity, door, _)| (entity, door))
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
/// (Reported by `impl-006`, whose streaming side spawns the doors.)
fn door_is_placed(transform: &GlobalTransform) -> bool {
    transform.translation() != Vec3::ZERO
        || transform.rotation() != Quat::IDENTITY
        || transform.scale() != Vec3::ONE
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

/// The load door prompt at the bottom of the screen.
#[derive(Component)]
struct DoorPrompt;

/// The one-time control hint.
#[derive(Component)]
struct HelpLine {
    /// Seconds left before the hint is hidden for good.
    remaining: f32,
}

/// Turns the engine's camera into a first-person player. The lead adds this only for `--walk`; in
/// that mode the old `fly_camera` system is not registered.
pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ActivateDoor>()
            .add_message::<DoorCrossed>()
            .add_systems(Startup, setup_player_hud)
            .add_systems(
                Update,
                (
                    attach_player,
                    (player_look, player_walk, player_door).chain(),
                    player_door_crossed,
                    player_help_line,
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
        commands.entity(entity).insert((
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
fn player_walk(
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut camera: Query<(&mut Transform, &mut Player), With<StreamingCamera>>,
    mut ray_cast: MeshRayCast,
    water: Query<(), With<WaterSurface>>,
    mut held: Local<bool>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok((mut transform, mut player)) = camera.single_mut() else {
        return;
    };
    let input = MoveInput::from_keys(&keyboard);
    let outcome = {
        let skip = |entity: Entity| water.get(entity).is_ok();
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

/// Targets the load door in front, opens it on `E`, and shows the prompt while one is targeted.
fn player_door(
    keyboard: Res<ButtonInput<KeyCode>>,
    camera: Query<(&GlobalTransform, &Player), With<StreamingCamera>>,
    doors: Query<(Entity, &GlobalTransform, &LoadDoor)>,
    mut activate: MessageWriter<ActivateDoor>,
    mut prompt: Query<(&mut Text, &mut Node), With<DoorPrompt>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok((camera_transform, player)) = camera.single() else {
        return;
    };
    let target = target_door(
        camera_transform.translation(),
        player.forward(),
        doors
            .iter()
            .filter(|(_, transform, _)| door_is_placed(transform))
            .map(|(entity, transform, door)| (entity, transform.translation(), door)),
    );
    if let Some((entity, _)) = target
        && keyboard.just_pressed(KeyCode::KeyE)
    {
        activate.write(ActivateDoor { door: entity });
    }
    if let Ok((mut text, mut node)) = prompt.single_mut() {
        match target {
            Some((_, door)) => {
                let label = format!("E  Open  {}", door.label);
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

/// Hides the one-time help line once it has been up long enough.
fn player_help_line(time: Res<Time>, mut help: Query<(&mut HelpLine, &mut Node)>) {
    for (mut line, mut node) in &mut help {
        if line.remaining <= 0.0 {
            continue;
        }
        line.remaining -= time.delta_secs();
        if line.remaining <= 0.0 {
            node.display = Display::None;
        }
    }
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

/// Builds the door prompt and the one-time help line.
fn setup_player_hud(mut commands: Commands) {
    commands.spawn((
        DoorPrompt,
        Text::new(""),
        TextFont::from_font_size(24.0),
        TextColor(Color::WHITE),
        TextShadow::default(),
        centered_bar(56.0, Display::None),
    ));
    commands.spawn((
        HelpLine {
            remaining: HELP_LINE_SECONDS,
        },
        Text::new(HELP_TEXT),
        TextFont::from_font_size(14.0),
        TextColor(Color::srgb(0.86, 0.86, 0.86)),
        TextShadow::default(),
        centered_bar(24.0, Display::Flex),
    ));
}

/// A full-width bar sitting `bottom` pixels above the bottom of the window, with its text centred.
fn centered_bar(bottom: f32, display: Display) -> Node {
    Node {
        position_type: PositionType::Absolute,
        bottom: Val::Px(bottom),
        left: Val::Px(0.0),
        right: Val::Px(0.0),
        justify_content: JustifyContent::Center,
        display,
        ..default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doors::DoorDestination;
    use std::f32::consts::FRAC_PI_2;

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

    #[test]
    fn the_nearest_door_in_the_view_cone_is_targeted() {
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
        }
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
}
