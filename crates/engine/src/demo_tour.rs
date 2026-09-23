//! A scripted walk through a demo's route (`--demo-tour <dir>`).
//!
//! It proves the demo end to end without a person at the keyboard: for each of the route's load
//! doors it waits for the world to settle, takes a screenshot, moves the camera in front of the
//! door, waits for the door's destination to pre-stream, sends [`ActivateDoor`], and takes a
//! screenshot on the far side. A `tour.txt` log records every step, and the app exits at the end.
//!
//! Which doors those are is the demo the run started in ([`route_for_run`]): the Alftand ->
//! Blackreach descent, or Riverwood's four houses, each entered and left again. A run with no
//! scripted route of its own walks the Alftand route, as `--demo-tour` always has. Alftand's Door
//! FormIDs come from `docs/research/worldspace-transition-demo.md`, section 2.2; Riverwood's from
//! the door links measured in `docs/design/riverwood-demo.md`, sections 2 and 3.
//!
//! The tour activates every door itself, exactly as before, so the run stays deterministic however
//! a person would cross it; the log says for each route door whether it is an auto-load marker
//! (which a player crosses by walking into it, see `crate::player`) or one that needs `E`.
//!
//! # The walk-through
//!
//! With a player driving the camera (`--walk --demo-tour <dir>`) the tour does not activate the
//! door at all: it presses `E` while walking up to it, holds `W` through the doorway, and
//! photographs every frame around the crossing, ten frames before the swap and ten after it, into
//! `walk-through/<stage>/` (with `frames.txt`, the eye and the view direction of each of them).
//!
//! That is the check the seamless crossing needs. There is no load screen and no snap, so the
//! frames on either side of the swap have to be the same view of the same room: the window of
//! them tiles into one contact sheet to look at.

use crate::{
    config::EngineConfig,
    doors::{ActivateDoor, DoorCrossed, LoadDoor},
    player::{Player, PlayerInput},
    streaming::creation_to_bevy,
    transition::{DoorOpen, distance_in_front_of_door, door_frame, door_is_open},
    world::components::StreamingCamera,
};
use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};
use std::{fmt::Write as _, path::PathBuf};

/// The four load doors from the Alftand entrance down to Blackreach, in order: a descent, each door
/// leaving the place the one before it arrived in.
pub const ALFTAND_ROUTE: [u32; 4] = [0x0001_5D48, 0x0009_2809, 0x0009_256A, 0x0006_998D];

/// The eight load doors of the Riverwood route, in order: the four houses on the village street,
/// each entered and left again, so the tour is back in Tamriel before it walks on to the next
/// (`docs/design/riverwood-demo.md`, section 3).
///
/// The doors pair up: `2k` is the door into a house and `2k + 1` the door inside it that leads back
/// out - which stands in the cell the crossing into `2k` arrived in, so the tour always finds the
/// next door in the space it is standing in.
const RIVERWOOD_ROUTE: [u32; 8] = [
    0x0001_CBB0, // into Sven's House (interior 0001CB84)
    0x0001_CBAF, // and back out to Tamriel
    0x0001_341F, // into the Riverwood Trader (000133C9)
    0x0001_33E9, // and back out
    0x0001_3420, // into Alvor and Sigrid's House (000133C8)
    0x0001_33FB, // and back out
    0x0001_3424, // into the Sleeping Giant Inn (000133C6)
    0x0001_3419, // and back out
];

/// A scripted route: the load doors to cross, in order, and the objective line shown on screen.
pub struct DemoRoute {
    /// The route's own name, for the log: the demo that walks it is not always the same word
    /// (`blackreach` starts at the far end of the `alftand` route).
    pub name: &'static str,
    pub doors: &'static [u32],
    pub objective: &'static str,
    /// What the countdown between crossings counts toward, named in the route's own words.
    pub destination: &'static str,
    /// The line shown once every door of the route has been crossed.
    pub finale: &'static str,
}

/// The Alftand -> Blackreach descent: four one-way doors, no place visited twice.
static ALFTAND: DemoRoute = DemoRoute {
    name: "alftand",
    doors: &ALFTAND_ROUTE,
    objective: "Objective: find the Alftand entrance nearby - look for the E prompt. Four doors \
                lead down to Blackreach.",
    destination: "Blackreach",
    finale: "You made it: Blackreach. No loading screens. Explore on foot (F to fly).",
};

/// Riverwood: four houses on the village street, each entered and left again.
static RIVERWOOD: DemoRoute = DemoRoute {
    name: "riverwood",
    doors: &RIVERWOOD_ROUTE,
    objective: "Objective: Riverwood's four houses - Sven's, the Trader, Alvor's, the Sleeping \
                Giant Inn. Eight doorways, in and out: press E at each.",
    destination: "the end of the tour",
    finale: "All four houses, and no loading screen between them. Riverwood is yours (F to fly).",
};

/// The `--demo` names that have a scripted route, and the route each one walks.
static DEMO_ROUTES: [(&str, &DemoRoute); 3] = [
    ("alftand", &ALFTAND),
    // The Blackreach demo starts inside Blackreach, at the arrival point of the Alftand route's
    // last door, and walks the same four back up: one route, two starting points.
    ("blackreach", &ALFTAND),
    ("riverwood", &RIVERWOOD),
];

/// The route for a `--demo <name>` run; `None` when the name has no scripted route.
pub fn route_for_demo(name: Option<&str>) -> Option<&'static DemoRoute> {
    let name = name?;
    DEMO_ROUTES
        .iter()
        .find(|(demo, _)| *demo == name)
        .map(|(_, route)| *route)
}

/// The route a run follows: the scripted route of the demo it started in, or the Alftand route when
/// it started in none - a `--start-position` run, a benchmark, or an unknown `--demo` name, which
/// `crate::config` leaves as no demo at all. That fallback is the route `--demo-tour` has always
/// walked and the objective has always described, so a run without a route of its own is unchanged.
pub fn route_for_run(demo: Option<&str>) -> &'static DemoRoute {
    route_for_demo(demo).unwrap_or(&ALFTAND)
}

/// Seconds to let a freshly entered place stream in before its screenshot.
const SETTLE_SECONDS: f32 = 10.0;
/// Seconds to wait in front of a door so its destination pre-streams.
const PRESTREAM_SECONDS: f32 = 6.0;
/// Seconds to look for a door that has not spawned yet before giving up.
const DOOR_SEARCH_SECONDS: f32 = 40.0;
/// Where the camera stands in front of a door: distance and eye height.
const DOOR_STANDOFF: f32 = 320.0;
const EYE_HEIGHT: f32 = 120.0;
/// The standoffs the walk-through tries, in front of the door, until the player is standing on
/// floor. Where that is depends on the door: the route's last door opens onto the Blackreach shaft,
/// and 320 units in front of it - a good look at the door for the photograph - is mid-air, while
/// the Alftand entrance's marker is set in an open passage where 60 units is inside the rock.
///
/// Sixty units first is about where the game puts a player who walks back out of an interior door
/// (the route's return links arrive 62-71 units out), which is floor by construction, and it puts
/// the doorway filling most of the view for the frames around the crossing.
///
/// Riverwood added the third. Its houses are single rooms 700-1500 units across, and inside
/// Alvor's the far wall is closer than 320 units: the tour failed at door 000133FB with neither
/// standoff walkable (`local/demo/riverwood/tour.txt`, first run). 160 is tried last so that the
/// Alftand route, which passes on 320, walks in from exactly where it always did.
const WALK_STANDOFFS: [f32; 3] = [60.0, 320.0, 160.0];
/// How long the player may be off the ground before the next standoff is tried.
const WALK_FALL_SECONDS: f32 = 0.7;
/// How much closer to the doorway a walk has to get, within [`WALK_STUCK_SECONDS`], for the
/// standoff it started from to count as one the player can walk in from.
const WALK_PROGRESS: f32 = 5.0;
/// How long the walk may make no progress toward the door - held keys, no closing of the distance -
/// before the next standoff is tried. A standoff inside rock leaves the player standing on
/// something and sliding along a wall, which walking cannot fix.
const WALK_STUCK_SECONDS: f32 = 2.5;
/// Seconds the walk-through may take to cross a door before it counts as a failure.
const WALK_THROUGH_SECONDS: f32 = 25.0;
/// How close to the doorway the walk-through starts photographing: close enough that the doorway
/// fills the view, and far enough out that walking to the plane takes longer than [`WALK_WINDOW`]
/// frames of capture even at the frame rate writing the sheet holds the run to.
///
/// The tour does not make the player run (`crate::player::WALK_SPEED` is 150 units per second, and
/// the frames come about four units apart at the rate this was run at), so 200 units of approach is
/// about twenty frames of capture before the swap.
const CAPTURE_DISTANCE: f32 = 200.0;
/// The frames kept on either side of the swap: ten before, ten after.
const WALK_WINDOW: u32 = 10;
/// Where the walk-through's frames go, under the tour's output directory.
const WALK_DIRECTORY: &str = "walk-through";

/// The demo's scripted tour and its objective line.
///
/// Added by [`portal::PortalPlugin`](crate::portal::PortalPlugin) for the runs that are looked at
/// rather than measured. Each half is added only when the run has it: the tour when `--demo-tour`
/// named an output folder, and the objective line when a walked demo has a route.
pub struct DemoTourPlugin {
    /// Where the tour writes its log and its screenshots; `None` for a run that only walks a demo,
    /// which has the objective line and no script.
    pub output_dir: Option<PathBuf>,
}

impl Plugin for DemoTourPlugin {
    fn build(&self, app: &mut App) {
        if let Some(output_dir) = self.output_dir.clone() {
            app.insert_resource(DemoTour::new(output_dir)).add_systems(
                Update,
                // Ahead of the player's own systems: the walk-through presses their keys, and a
                // press has to be in the frame the controller reads it.
                run_demo_tour.before(PlayerInput),
            );
        }
        // The objective line belongs to a walked demo, whether or not a script is walking it: the
        // rule is the one `app::run` has always applied, and it is read from the configuration
        // because the run mode is a property of the run (`EngineConfig::walks`).
        let (walks, demo) = {
            let config = app.world().resource::<EngineConfig>();
            (config.walks(), config.portal.demo.clone())
        };
        if walks && route_for_demo(demo.as_deref()).is_some() {
            app.add_systems(Startup, spawn_demo_objective)
                .add_systems(Update, update_demo_objective);
        }
    }
}

/// The one-line goal shown in the top-left corner of a demo, and how far through its route the run
/// is.
#[derive(Component)]
struct DemoObjective {
    doors_crossed: usize,
    /// The route the run is walking: what the countdown counts down from, what it counts toward,
    /// and the line shown when it reaches zero.
    route: &'static DemoRoute,
}

fn spawn_demo_objective(mut commands: Commands, config: Res<EngineConfig>) {
    // Both the line and the count are the route of the demo the run started in; a run with no
    // scripted route of its own keeps the Alftand line and its four doors, which is what the
    // objective has always shown (`route_for_run`).
    let route = route_for_run(config.portal.demo.as_deref());
    commands.spawn((
        DemoObjective {
            doors_crossed: 0,
            route,
        },
        Text::new(route.objective),
        TextFont {
            font_size: bevy::text::FontSize::Px(18.0),
            ..default()
        },
        TextColor(Color::srgb(0.95, 0.9, 0.75)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(12.0),
            left: Val::Px(14.0),
            ..default()
        },
    ));
}

fn update_demo_objective(
    mut crossed: MessageReader<DoorCrossed>,
    mut objective: Query<(&mut DemoObjective, &mut Text)>,
) {
    let Ok((mut state, mut text)) = objective.single_mut() else {
        return;
    };
    for event in crossed.read() {
        state.doors_crossed += 1;
        let place = event.label.trim();
        let route = state.route;
        let left = route.doors.len().saturating_sub(state.doors_crossed);
        // The route is done when its last door has been crossed. Riverwood's route ends back in
        // Tamriel, where it began, so there is no arrival place to recognise by name.
        text.0 = if left == 0 {
            route.finale.to_owned()
        } else {
            let destination = route.destination;
            format!(
                "Now in {place}. Find the next load door (E) - {left} more to {destination}. F flies if you get stuck."
            )
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Wait for the current place to stream, then photograph it.
    Settle,
    /// Turn on the spot and photograph each place four ways, for visual audits.
    Survey(u8),
    /// Find the next route door and stand in front of it.
    FindDoor,
    /// Wait in front of the door so its destination pre-streams, then photograph the door.
    Prestream,
    /// Door photographed; activate it once the screenshot has been taken.
    Activate,
    /// Door activated; waiting for `DoorCrossed`.
    Crossing,
    /// With a player: open the door with `E` and walk through it, photographing every frame.
    WalkThrough,
    /// Walked through; keep photographing until ten frames past the swap, then move on.
    WalkThroughAfter {
        swap: u32,
    },
    /// Extra views after the last crossing.
    LookAround(u8),
    /// With the player controller active: hold W and check the player walks on the ground.
    Walk {
        start: Vec3,
    },
    Done,
}

#[derive(Resource)]
pub struct DemoTour {
    output_dir: PathBuf,
    stage: usize,
    phase: Phase,
    timer: f32,
    /// Frames since the tour started: what a walk-through capture is named after, so the window
    /// around the swap can be picked out of it.
    frame: u32,
    door: Option<Entity>,
    log: String,
    failed: bool,
    walked: bool,
    /// The walk-through's captures this stage: the frame each was taken in and where it went.
    walk_frames: Vec<(u32, PathBuf)>,
    /// The pose of each of those frames, one line each, written next to them when the window ends.
    walk_log: String,
    /// Which of [`WALK_STANDOFFS`] the walk-through is standing off by, how long the player has
    /// been off the ground there, and how long they have gone without getting closer to the
    /// doorway: a standoff the walk gets nowhere from is left for the next one.
    walk_standoff: usize,
    walk_fell: f32,
    walk_stuck: f32,
    walk_furthest: f32,
}

impl DemoTour {
    fn new(output_dir: PathBuf) -> Self {
        Self {
            output_dir,
            stage: 0,
            phase: Phase::Settle,
            timer: 0.0,
            frame: 0,
            door: None,
            log: String::new(),
            failed: false,
            walked: false,
            walk_frames: Vec::new(),
            walk_log: String::new(),
            walk_standoff: 0,
            walk_fell: 0.0,
            walk_stuck: 0.0,
            walk_furthest: f32::INFINITY,
        }
    }

    fn note(&mut self, line: impl AsRef<str>) {
        info!(target: "demo_tour", "{}", line.as_ref());
        let _ = writeln!(self.log, "{}", line.as_ref());
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.timer = 0.0;
    }

    /// Where this stage's walk-through frames go.
    fn walk_directory(&self) -> PathBuf {
        self.output_dir
            .join(WALK_DIRECTORY)
            .join(format!("{:02}", self.stage))
    }
}

fn shoot_path(commands: &mut Commands, tour: &mut DemoTour, path: PathBuf) {
    tour.note(format!("screenshot {}", path.display()));
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path));
}

fn shoot(commands: &mut Commands, tour: &mut DemoTour, name: &str) {
    let path = tour.output_dir.join(format!("{name}.png"));
    shoot_path(commands, tour, path);
}

/// Photographs the frame being drawn now and records the frame it belongs to.
///
/// The image is written a frame or two later, so the file is named after the frame it was asked
/// for: that is what puts it on the right side of the swap.
fn capture_walk_frame(commands: &mut Commands, tour: &mut DemoTour, camera: &Transform) {
    let directory = tour.walk_directory();
    if let Err(error) = std::fs::create_dir_all(&directory) {
        warn!("could not create {}: {error}", directory.display());
        return;
    }
    let path = directory.join(format!("f{:05}.png", tour.frame));
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path.clone()));
    tour.walk_frames.push((tour.frame, path));
    let forward = camera.rotation * Vec3::NEG_Z;
    let _ = writeln!(
        tour.walk_log,
        "frame {} eye {:.1} {:.1} {:.1} forward {:.4} {:.4} {:.4}",
        tour.frame,
        camera.translation.x,
        camera.translation.y,
        camera.translation.z,
        forward.x,
        forward.y,
        forward.z
    );
}

/// Points the tour's view at `position` with `rotation`: the camera always, and the player too when
/// one drives it.
///
/// `crate::player::player_look` writes the camera's rotation from the player's own yaw and pitch
/// every frame, so an aim written only to the camera is undone before the next frame is drawn. The
/// pitch is clamped exactly as the player's own look clamps it, and the player is dropped from the
/// air, so the pose the tour asks for is the pose the next frame renders.
fn point_the_view(
    camera: &mut Transform,
    player: Option<&mut Player>,
    position: Vec3,
    rotation: Quat,
) {
    camera.translation = position;
    camera.rotation = rotation;
    if let Some(player) = player {
        let (yaw, pitch, _) = rotation.to_euler(EulerRot::YXZ);
        player.yaw = yaw;
        player.pitch = crate::player::clamp_pitch(pitch);
        player.velocity = Vec3::ZERO;
        player.grounded = false;
        camera.rotation = player.look_rotation();
    }
}

/// Points the view at a door from `standoff` units in front of it, on the side the door faces.
///
/// The side comes from the door's own link data ([`LoadDoor::outward`]) rather than from the model,
/// for the reason [`crate::transition::door_frame`] gives: door models disagree about which of
/// their own axes is their front.
fn stand_in_front_of_door(
    camera: &mut Transform,
    player: Option<&mut Player>,
    transform: &GlobalTransform,
    door: &LoadDoor,
    standoff: f32,
) {
    let position = transform.translation();
    let mut away = match door.outward {
        Some(outward) => creation_to_bevy(Vec3::from_array(outward)),
        None => *transform.forward(),
    };
    away.y = 0.0;
    let away = away.try_normalize().unwrap_or_else(|| {
        let mut fallback = camera.translation - position;
        fallback.y = 0.0;
        fallback.try_normalize().unwrap_or(Vec3::Z)
    });
    let eye = position + away * standoff + Vec3::Y * EYE_HEIGHT;
    let looking =
        Transform::from_translation(eye).looking_at(position + Vec3::Y * EYE_HEIGHT, Vec3::Y);
    point_the_view(camera, player, looking.translation, looking.rotation);
}

/// Turns the view on the spot, the tour's four-way survey.
fn turn_the_view(camera: &mut Transform, player: Option<&mut Player>, radians: f32) {
    match player {
        Some(player) => {
            player.yaw += radians;
            camera.rotation = player.look_rotation();
        }
        None => camera.rotate_y(radians),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_demo_tour(
    mut commands: Commands,
    time: Res<Time>,
    config: Res<EngineConfig>,
    mut tour: ResMut<DemoTour>,
    mut camera: Query<(&mut Transform, Option<&mut Player>), With<StreamingCamera>>,
    doors: Query<(Entity, &GlobalTransform, &LoadDoor, Option<&DoorOpen>)>,
    mut crossed: MessageReader<DoorCrossed>,
    mut activate: MessageWriter<ActivateDoor>,
    mut exit: MessageWriter<AppExit>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    portal_texture: Option<Res<crate::portal::PortalTexture>>,
) {
    tour.frame += 1;
    tour.timer += time.delta_secs();
    // The route belongs to the demo the run started in, and does not change while it runs.
    let route = route_for_run(config.portal.demo.as_deref());
    if tour.frame == 1 {
        let line = format!(
            "tour route: {} ({} doors), demo {:?}, objective \"{}\"",
            route.name,
            route.doors.len(),
            config.portal.demo.as_deref(),
            route.objective
        );
        tour.note(line);
    }
    let Ok((mut camera, player)) = camera.single_mut() else {
        return;
    };
    let mut player = player;
    match tour.phase {
        Phase::Settle => {
            if tour.timer >= SETTLE_SECONDS {
                let name = format!("{:02}-arrived", tour.stage);
                shoot(&mut commands, &mut tour, &name);
                if tour.stage >= route.doors.len() {
                    tour.enter(Phase::LookAround(0));
                } else {
                    tour.enter(Phase::Survey(0));
                }
            }
        }
        Phase::Survey(view) => {
            if tour.timer >= 1.5 {
                if view < 3 {
                    turn_the_view(
                        &mut camera,
                        player.as_deref_mut(),
                        std::f32::consts::FRAC_PI_2,
                    );
                    let name = format!("{:02}-survey-{}", tour.stage, view + 1);
                    shoot(&mut commands, &mut tour, &name);
                    tour.enter(Phase::Survey(view + 1));
                } else {
                    turn_the_view(
                        &mut camera,
                        player.as_deref_mut(),
                        std::f32::consts::FRAC_PI_2,
                    );
                    tour.enter(Phase::FindDoor);
                }
            }
        }
        Phase::FindDoor => {
            // `Phase::Settle` sends the tour on to `Phase::LookAround` once the stage count reaches
            // the route's length, so every stage that gets here names a door of the route.
            let wanted = route.doors[tour.stage];
            if let Some((entity, transform, door, _)) =
                doors.iter().find(|(_, _, door, _)| door.ref_id == wanted)
            {
                let door_position = transform.translation();
                // Stand on the door's front: the side the player walks in from, where the portal
                // shows the room beyond (`stand_in_front_of_door`).
                stand_in_front_of_door(
                    &mut camera,
                    player.as_deref_mut(),
                    transform,
                    door,
                    DOOR_STANDOFF,
                );
                let eye = camera.translation;
                tour.door = Some(entity);
                let line = format!(
                    "stage {}: door {wanted:08X} -> \"{}\" found at {door_position:?}, {} (auto_load={}), camera placed at {eye:?}",
                    tour.stage,
                    door.label,
                    if door.auto_load {
                        "auto-load marker"
                    } else {
                        "E-key door"
                    },
                    door.auto_load
                );
                tour.note(line);
                tour.enter(Phase::Prestream);
            } else if tour.timer >= DOOR_SEARCH_SECONDS {
                let line = format!(
                    "FAIL stage {}: door {wanted:08X} never spawned ({} load doors resident: {})",
                    tour.stage,
                    doors.iter().count(),
                    doors
                        .iter()
                        .map(|(_, _, door, _)| format!("{:08X}", door.ref_id))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                tour.note(line);
                tour.failed = true;
                tour.enter(Phase::LookAround(0));
            }
        }
        Phase::Prestream => {
            if tour.timer >= PRESTREAM_SECONDS {
                let name = format!("{:02}-door", tour.stage);
                shoot(&mut commands, &mut tour, &name);
                // What the portal camera itself renders, independent of where the main camera
                // stands: the room beyond this door, if the portal is showing it.
                if let Some(portal) = &portal_texture {
                    let path = tour
                        .output_dir
                        .join(format!("{:02}-portal-view.png", tour.stage));
                    tour.note(format!("portal render target {}", path.display()));
                    commands
                        .spawn(Screenshot::image(portal.0.clone()))
                        .observe(save_to_disk(path));
                }
                // A screenshot is taken on a later frame; crossing now would photograph the far side.
                if player.is_some() {
                    // A player drives the camera: open the door and walk through it instead, which
                    // is the crossing this tour is here to check. The walk starts closer in than
                    // the photograph was taken from (`WALK_STANDOFF`).
                    tour.walk_standoff = 0;
                    tour.walk_fell = 0.0;
                    tour.walk_stuck = 0.0;
                    tour.walk_furthest = f32::INFINITY;
                    if let Some((_, transform, door, _)) =
                        tour.door.and_then(|door| doors.get(door).ok())
                    {
                        stand_in_front_of_door(
                            &mut camera,
                            player.as_deref_mut(),
                            transform,
                            door,
                            WALK_STANDOFFS[0],
                        );
                    }
                    let line = format!(
                        "stage {}: walk-through - pressing E and walking through the doorway, photographing the frames around the crossing",
                        tour.stage
                    );
                    tour.note(line);
                    tour.walk_frames.clear();
                    tour.walk_log.clear();
                    tour.enter(Phase::WalkThrough);
                } else {
                    tour.enter(Phase::Activate);
                }
            }
        }
        Phase::Activate => {
            if tour.timer >= 1.0 {
                match tour.door {
                    Some(door) if doors.get(door).is_ok() => {
                        activate.write(ActivateDoor { door });
                        tour.enter(Phase::Crossing);
                    }
                    _ => {
                        let line = format!(
                            "FAIL stage {}: the door unloaded before activation",
                            tour.stage
                        );
                        tour.note(line);
                        tour.failed = true;
                        tour.enter(Phase::LookAround(0));
                    }
                }
            }
        }
        Phase::Crossing => {
            if let Some(event) = crossed.read().last() {
                let line = format!(
                    "stage {}: crossed from {:08X} into \"{}\", camera now at {:?}",
                    tour.stage, event.from_ref_id, event.label, camera.translation
                );
                tour.note(line);
                tour.stage += 1;
                tour.enter(Phase::Settle);
            } else if tour.timer >= 5.0 {
                let line = format!("FAIL stage {}: no DoorCrossed within 5 s", tour.stage);
                tour.note(line);
                tour.failed = true;
                tour.enter(Phase::LookAround(0));
            }
        }
        Phase::WalkThrough => {
            // The crossing comes first: the frame it happens in is the frame the cell just left
            // unloads in, so the door and its transform are usually gone by the time the tour sees
            // `DoorCrossed`, and looking for the door first would call that a failure.
            if let Some(event) = crossed.read().last() {
                stop_walking(&mut keys);
                let line = format!(
                    "stage {}: walked through the doorway into \"{}\", camera at {:?} on frame {}",
                    tour.stage, event.label, camera.translation, tour.frame
                );
                tour.note(line);
                let swap = tour.frame;
                // The frame this is seen in is photographed like every other one: it is the frame
                // the crossing is read in, so it is the first one on the far side of the doorway,
                // and leaving it out would leave a hole in the window either side of the swap -
                // the only frames the walk-through is judged on. Ten before the swap, the swap and
                // ten after it are all kept (`walk_frames`).
                capture_walk_frame(&mut commands, &mut tour, &camera);
                tour.enter(Phase::WalkThroughAfter { swap });
                return;
            }
            let Some((_, door_transform, door, open)) =
                tour.door.and_then(|door| doors.get(door).ok())
            else {
                stop_walking(&mut keys);
                let line = format!(
                    "FAIL stage {}: the door unloaded before the walk-through",
                    tour.stage
                );
                tour.note(line);
                tour.failed = true;
                tour.enter(Phase::LookAround(0));
                return;
            };
            // How far the player still is from the doorway, and whether walking is getting them
            // anywhere: the ladder of standoffs below is chosen from these two.
            let frame = door_frame(door_transform.rotation(), door.outward);
            let in_front =
                distance_in_front_of_door(door_transform.translation(), frame, camera.translation);
            let grounded = player.as_deref().is_some_and(|player| player.grounded);
            tour.walk_fell = if grounded {
                0.0
            } else {
                tour.walk_fell + time.delta_secs()
            };
            if in_front < tour.walk_furthest - WALK_PROGRESS {
                tour.walk_furthest = in_front;
                tour.walk_stuck = 0.0;
            } else {
                tour.walk_stuck += time.delta_secs();
            }

            // The walk starts at a standoff the player can stand on and walk in from, which depends
            // on the door (see [`WALK_STANDOFFS`]). This one did not work if the player is falling
            // or getting no closer to the door, so the next standoff is tried - the frames taken
            // while it did not work are forgotten, since they are not the walk being judged.
            if tour.walk_fell >= WALK_FALL_SECONDS || tour.walk_stuck >= WALK_STUCK_SECONDS {
                tour.walk_fell = 0.0;
                tour.walk_stuck = 0.0;
                tour.walk_standoff += 1;
                let Some(standoff) = WALK_STANDOFFS.get(tour.walk_standoff) else {
                    stop_walking(&mut keys);
                    let line = format!(
                        "FAIL stage {}: the player cannot walk in from any of the standoffs in front of door {:08X} ({:?})",
                        tour.stage, door.ref_id, WALK_STANDOFFS
                    );
                    tour.note(line);
                    tour.failed = true;
                    tour.enter(Phase::LookAround(0));
                    return;
                };
                let was = WALK_STANDOFFS[tour.walk_standoff - 1];
                stand_in_front_of_door(
                    &mut camera,
                    player.as_deref_mut(),
                    door_transform,
                    door,
                    *standoff,
                );
                tour.walk_furthest = f32::INFINITY;
                tour.walk_frames.clear();
                tour.walk_log.clear();
                let line = format!(
                    "stage {}: the walk from {was:.0} units in front of the door is going nowhere (grounded={grounded}); walking in from {standoff:.0} units instead",
                    tour.stage
                );
                tour.note(line);
                return;
            }

            // Walk up to the door and open it: `E` only reaches a door within `DOOR_RANGE`, so the
            // key is pressed on every frame until the door is open. Released and pressed again so
            // that the controller sees a fresh press, whichever order the two systems run in.
            if !door.auto_load && !door_is_open(open) {
                keys.release(KeyCode::KeyE);
                keys.press(KeyCode::KeyE);
            }
            keys.press(KeyCode::KeyW);

            // Photograph the approach once the doorway is close enough to fill the view. Walking,
            // not running: the frames are what the crossing is judged on, and they are dense enough
            // to cover the ten before the swap at this speed.
            if in_front > 0.0 && in_front <= CAPTURE_DISTANCE {
                capture_walk_frame(&mut commands, &mut tour, &camera);
            }

            if tour.timer >= WALK_THROUGH_SECONDS {
                stop_walking(&mut keys);
                let line = format!(
                    "FAIL stage {}: no crossing after {WALK_THROUGH_SECONDS:.0} s of walking at the door ({} frames captured)",
                    tour.stage,
                    tour.walk_frames.len()
                );
                tour.note(line);
                tour.failed = true;
                tour.enter(Phase::LookAround(0));
            }
        }
        Phase::WalkThroughAfter { swap } => {
            if tour.frame <= swap + WALK_WINDOW {
                capture_walk_frame(&mut commands, &mut tour, &camera);
                return;
            }
            // The window the frames are kept for: ten frames before the swap and ten after it.
            let first = swap.saturating_sub(WALK_WINDOW);
            let last = swap + WALK_WINDOW;
            let window = tour
                .walk_frames
                .iter()
                .filter(|(frame, _)| (first..=last).contains(frame))
                .count();
            let directory = tour.walk_directory();
            let frames_path = directory.join("frames.txt");
            let document = format!(
                "swap frame {swap}\nwindow {first}..={last} ({window} frames)\n\n{}",
                tour.walk_log
            );
            if let Err(error) = std::fs::write(&frames_path, document) {
                warn!("could not write {}: {error}", frames_path.display());
            }
            let line = format!(
                "stage {}: walk-through crossed on frame {swap}; {} frames captured, {} of them in the window {first}..={last} ({})",
                tour.stage,
                tour.walk_frames.len(),
                window,
                directory.display()
            );
            tour.note(line);
            tour.walk_frames.clear();
            tour.walk_log.clear();
            tour.stage += 1;
            tour.enter(Phase::Settle);
        }
        Phase::LookAround(view) => {
            if tour.timer >= 3.0 {
                if view < 3 {
                    turn_the_view(
                        &mut camera,
                        player.as_deref_mut(),
                        std::f32::consts::FRAC_PI_2,
                    );
                    let name = format!("{:02}-view-{}", tour.stage, view + 1);
                    shoot(&mut commands, &mut tour, &name);
                    tour.enter(Phase::LookAround(view + 1));
                } else if let Some(grounded) = player.as_deref().map(|player| player.grounded)
                    && !tour.failed
                    && !tour.walked
                {
                    // The last quarter of the survey: back to the heading the player arrived with.
                    // A crossing through an anchored doorway lands in the doorway itself, and the
                    // survey's third view there faces the door frame, not the street.
                    turn_the_view(
                        &mut camera,
                        player.as_deref_mut(),
                        std::f32::consts::FRAC_PI_2,
                    );
                    let line = format!(
                        "walk test: grounded={grounded} at {:?}; holding W for 4 s",
                        camera.translation
                    );
                    tour.note(line);
                    let start = camera.translation;
                    tour.walked = true;
                    tour.enter(Phase::Walk { start });
                } else {
                    let verdict = if tour.failed { "FAILED" } else { "PASSED" };
                    let line = format!("tour {verdict} after {} crossings", tour.stage);
                    tour.note(line);
                    let log_path = tour.output_dir.join("tour.txt");
                    if let Err(error) = std::fs::write(&log_path, &tour.log) {
                        error!("could not write {}: {error}", log_path.display());
                    }
                    tour.enter(Phase::Done);
                }
            }
        }
        Phase::Walk { start } => {
            if tour.timer < 4.0 {
                keys.press(KeyCode::KeyW);
            } else {
                keys.release(KeyCode::KeyW);
                let moved = camera.translation - start;
                let horizontal = Vec2::new(moved.x, moved.z).length();
                let grounded = player.as_deref().is_some_and(|player| player.grounded);
                let line = format!(
                    "walk test: moved {horizontal:.0} units horizontally, {:.0} vertically, grounded={grounded}",
                    moved.y
                );
                tour.note(line);
                if horizontal < 100.0 || !grounded {
                    tour.note("FAIL walk test: the player did not walk on the ground");
                    tour.failed = true;
                }
                shoot(&mut commands, &mut tour, "05-walked");
                tour.enter(Phase::LookAround(3));
            }
        }
        Phase::Done => {
            // Give the last screenshot a moment to reach the disk.
            if tour.timer >= 2.0 {
                exit.write(if tour.failed {
                    AppExit::error()
                } else {
                    AppExit::Success
                });
            }
        }
    }
}

/// Lets go of the keys the walk-through holds, whichever way it ended.
fn stop_walking(keys: &mut ButtonInput<KeyCode>) {
    keys.release(KeyCode::KeyW);
    keys.release(KeyCode::ShiftRight);
    keys.release(KeyCode::KeyE);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Riverwood route's door pairs, as `door_links` has them: `2k` leads into a house and
    /// `2k + 1` is the door inside it that leads back out to Tamriel. Written out here rather than
    /// read from the converted database, so the test runs in CI, which has no game data.
    const RIVERWOOD_PAIRS: [(u32, u32); 4] = [
        (0x0001_CBB0, 0x0001_CBAF), // Sven's House (interior 0001CB84)
        (0x0001_341F, 0x0001_33E9), // the Riverwood Trader (000133C9)
        (0x0001_3420, 0x0001_33FB), // Alvor and Sigrid's House (000133C8)
        (0x0001_3424, 0x0001_3419), // the Sleeping Giant Inn (000133C6)
    ];

    /// The number words the objectives count their crossings in, by count.
    const COUNT_WORDS: [&str; 9] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight",
    ];

    #[test]
    fn a_demo_name_resolves_to_its_route() {
        let route = |demo: Option<&str>| route_for_demo(demo).map(|route| route.name);
        assert_eq!(
            route(Some("riverwood")),
            Some("riverwood"),
            "the Riverwood demo has a route of its own"
        );
        assert_eq!(route(Some("alftand")), Some("alftand"));
        assert_eq!(
            route(Some("blackreach")),
            Some("alftand"),
            "the Blackreach demo starts at the far end of the Alftand route and walks it back"
        );
        assert_eq!(route(None), None, "a run with no --demo has no named route");
        assert_eq!(
            route(Some("nowhere")),
            None,
            "an unknown name has no route either, as `--demo` leaves no demo behind for one"
        );
    }

    #[test]
    fn a_run_without_a_route_walks_the_alftand_one() {
        let fallback = route_for_run(None);
        for demo in [None, Some("nowhere")] {
            assert_eq!(route_for_run(demo).name, fallback.name);
            assert_eq!(route_for_run(demo).doors, fallback.doors);
        }
        assert_eq!(
            fallback.doors,
            &ALFTAND_ROUTE[..],
            "the fallback is the route --demo-tour has always walked"
        );
        assert_eq!(
            fallback.doors.len(),
            4,
            "and the count the objective has always shown"
        );
        assert_eq!(route_for_run(Some("riverwood")).name, "riverwood");
    }

    #[test]
    fn the_riverwood_route_is_four_houses_entered_and_left_again() {
        let doors = route_for_demo(Some("riverwood"))
            .expect("riverwood has a scripted route")
            .doors;
        assert_eq!(doors.len(), 8, "four houses, entered and left again");

        // Every pair is the door into a house and the door inside it that leads back out, in that
        // order: the tour leaves each house by a door standing in the cell it just arrived in.
        for (pair, (there, back)) in RIVERWOOD_PAIRS.iter().enumerate() {
            assert_eq!(
                doors[2 * pair],
                *there,
                "door {} leads into house {}",
                2 * pair + 1,
                pair + 1
            );
            assert_eq!(
                doors[2 * pair + 1],
                *back,
                "door {} is the way back out of house {}",
                2 * pair + 2,
                pair + 1
            );
        }
    }

    #[test]
    fn every_route_crosses_each_door_once() {
        for (demo, route) in DEMO_ROUTES {
            let mut crossed = Vec::new();
            for door in route.doors {
                assert!(
                    !crossed.contains(door),
                    "{demo}: the {} route crosses {door:08X} twice",
                    route.name
                );
                crossed.push(*door);
            }
            assert!(
                !route.doors.is_empty(),
                "{demo}: a route with no doors is not a route"
            );
        }
    }

    #[test]
    fn every_objective_states_how_many_doorways_its_route_has() {
        for (demo, route) in DEMO_ROUTES {
            let objective = route.objective;
            assert!(
                objective.starts_with("Objective: "),
                "{demo}: the objective line is what the HUD shows: {objective:?}"
            );
            assert!(
                objective.ends_with('.'),
                "{demo}: an objective is a sentence: {objective:?}"
            );

            // The count the objective states is the route's own door count, which is also what the
            // objective's countdown starts from in `crate::app`.
            let count = route.doors.len();
            assert!(
                count < COUNT_WORDS.len(),
                "{demo}: the route has {count} doors, more than this test spells out"
            );
            let expected = COUNT_WORDS[count];
            assert!(
                objective
                    .split(|c: char| !c.is_ascii_alphabetic())
                    .any(|word| word.eq_ignore_ascii_case(expected)),
                "{demo}: the objective of a {count}-door route says \"{expected}\": {objective:?}"
            );
        }
    }
}
