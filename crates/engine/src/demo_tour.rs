//! A scripted walk through the Alftand -> Blackreach route (`--demo-tour <dir>`).
//!
//! It proves the demo end to end without a person at the keyboard: for each of the four route
//! doors it waits for the world to settle, takes a screenshot, moves the camera in front of the
//! door, waits for the door's destination to pre-stream, sends [`ActivateDoor`], and takes a
//! screenshot on the far side. A `tour.txt` log records every step, and the app exits at the end.
//! Door FormIDs come from `docs/research/worldspace-transition-demo.md`, section 2.2.

use crate::{
    doors::{ActivateDoor, DoorCrossed, LoadDoor},
    world::components::StreamingCamera,
};
use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};
use std::{fmt::Write as _, path::PathBuf};

/// The four load doors from the Alftand entrance down to Blackreach, in order.
pub const ALFTAND_ROUTE: [u32; 4] = [0x0001_5D48, 0x0009_2809, 0x0009_256A, 0x0006_998D];

/// Seconds to let a freshly entered place stream in before its screenshot.
const SETTLE_SECONDS: f32 = 10.0;
/// Seconds to wait in front of a door so its destination pre-streams.
const PRESTREAM_SECONDS: f32 = 6.0;
/// Seconds to look for a door that has not spawned yet before giving up.
const DOOR_SEARCH_SECONDS: f32 = 40.0;
/// Where the camera stands in front of a door: distance and eye height.
const DOOR_STANDOFF: f32 = 320.0;
const EYE_HEIGHT: f32 = 120.0;

pub struct DemoTourPlugin {
    pub output_dir: PathBuf,
}

impl Plugin for DemoTourPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DemoTour::new(self.output_dir.clone()))
            .add_systems(Update, run_demo_tour);
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
    door: Option<Entity>,
    log: String,
    failed: bool,
    walked: bool,
}

impl DemoTour {
    fn new(output_dir: PathBuf) -> Self {
        Self {
            output_dir,
            stage: 0,
            phase: Phase::Settle,
            timer: 0.0,
            door: None,
            log: String::new(),
            failed: false,
            walked: false,
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
}

fn shoot(commands: &mut Commands, tour: &mut DemoTour, name: &str) {
    let path = tour.output_dir.join(format!("{name}.png"));
    tour.note(format!("screenshot {}", path.display()));
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path));
}

#[allow(clippy::too_many_arguments)]
fn run_demo_tour(
    mut commands: Commands,
    time: Res<Time>,
    mut tour: ResMut<DemoTour>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    doors: Query<(Entity, &GlobalTransform, &LoadDoor)>,
    mut crossed: MessageReader<DoorCrossed>,
    mut activate: MessageWriter<ActivateDoor>,
    mut exit: MessageWriter<AppExit>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    player: Query<&crate::player::Player>,
    portal_texture: Option<Res<crate::portal::PortalTexture>>,
) {
    tour.timer += time.delta_secs();
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    match tour.phase {
        Phase::Settle => {
            if tour.timer >= SETTLE_SECONDS {
                let name = format!("{:02}-arrived", tour.stage);
                shoot(&mut commands, &mut tour, &name);
                if tour.stage >= ALFTAND_ROUTE.len() {
                    tour.enter(Phase::LookAround(0));
                } else {
                    tour.enter(Phase::Survey(0));
                }
            }
        }
        Phase::Survey(view) => {
            if tour.timer >= 1.5 {
                if view < 3 {
                    camera.rotate_y(std::f32::consts::FRAC_PI_2);
                    let name = format!("{:02}-survey-{}", tour.stage, view + 1);
                    shoot(&mut commands, &mut tour, &name);
                    tour.enter(Phase::Survey(view + 1));
                } else {
                    camera.rotate_y(std::f32::consts::FRAC_PI_2);
                    tour.enter(Phase::FindDoor);
                }
            }
        }
        Phase::FindDoor => {
            let wanted = ALFTAND_ROUTE[tour.stage];
            if let Some((entity, transform, door)) =
                doors.iter().find(|(_, _, door)| door.ref_id == wanted)
            {
                let door_position = transform.translation();
                // Stand on the door's front (runtime forward, -Z = Creation +Y): the side the player
                // walks in from, where the portal shows the room beyond.
                let mut away = *transform.forward();
                away.y = 0.0;
                let away = away.try_normalize().unwrap_or_else(|| {
                    let mut fallback = camera.translation - door_position;
                    fallback.y = 0.0;
                    fallback.try_normalize().unwrap_or(Vec3::Z)
                });
                let eye = door_position + away * DOOR_STANDOFF + Vec3::Y * EYE_HEIGHT;
                *camera = Transform::from_translation(eye)
                    .looking_at(door_position + Vec3::Y * EYE_HEIGHT, Vec3::Y);
                tour.door = Some(entity);
                let line = format!(
                    "stage {}: door {wanted:08X} -> \"{}\" found at {door_position:?}, camera placed at {eye:?}",
                    tour.stage, door.label
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
                        .map(|(_, _, door)| format!("{:08X}", door.ref_id))
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
                tour.enter(Phase::Activate);
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
        Phase::LookAround(view) => {
            if tour.timer >= 3.0 {
                if view < 3 {
                    camera.rotate_y(std::f32::consts::FRAC_PI_2);
                    let name = format!("{:02}-view-{}", tour.stage, view + 1);
                    shoot(&mut commands, &mut tour, &name);
                    tour.enter(Phase::LookAround(view + 1));
                } else if let Some(player) = player
                    .iter()
                    .next()
                    .filter(|_| !tour.failed && !tour.walked)
                {
                    let line = format!(
                        "walk test: grounded={} mode={:?} at {:?}; holding W for 4 s",
                        player.grounded, player.mode, camera.translation
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
                let grounded = player.iter().next().is_some_and(|player| player.grounded);
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
