//! Going through a load door: look at one, press E, fade to black, arrive at the other side.
//!
//! One state machine per crossing, with a single crossing at a time:
//!
//! 1. **Fade out** (0.25 s) under a full-screen black UI node.
//! 2. At full black the space switches in one step: every cell of the space left behind unloads at
//!    once (unpaced), the [`ActiveSpace`] and `CameraSpace` change, an exterior landing sets the
//!    render origin to the arrival's grid square (an interior landing leaves it alone), and the
//!    player is teleported to the door's `XTEL` arrival point.
//! 3. **Hold black** until the landing cell is resident and nothing is pending (models, surfaces),
//!    for at most 10 s. A landing cell that fails to load puts the player back where they were,
//!    with a warning.
//! 4. **Fade in** (0.25 s).
//!
//! There is no preload: the fade simply holds until the destination is resident.

use crate::{
    doors::LoadDoor,
    physics::{CursorCapture, MovementTuning, TeleportPlayer, body_and_camera_for_feet},
    sky::CameraSpace,
    streaming::{
        ActiveSpace, RenderOrigin, StreamingMetrics, StreamingWorld, creation_rotation_to_bevy,
        creation_to_bevy, streaming_center, unload_all_cells_now,
    },
    world::{
        components::{CELL_SIZE, ExpectedModelBounds, StreamingCamera, WorldPosition},
        database::CellKey,
    },
};
use bevy::prelude::*;

/// How far from the camera a door can be and still be used, in Creation units.
pub const DOOR_REACH: f32 = 200.0;
/// The box a load door is activated through when its reference carries no model bounds, in the
/// door's local space: a door-sized box around its origin (the origin is at the hinge, on the
/// floor, so the box starts at the floor and reaches a person's height and a little more).
pub const FALLBACK_DOOR_MIN: Vec3 = Vec3::new(-80.0, 0.0, -80.0);
pub const FALLBACK_DOOR_MAX: Vec3 = Vec3::new(80.0, 200.0, 80.0);
/// Seconds to fade out, and to fade in.
pub const FADE_SECONDS: f32 = 0.25;
/// Longest the screen stays black waiting for the destination.
pub const LANDING_TIMEOUT_SECONDS: f32 = 10.0;

pub struct DoorCrossingPlugin;

impl Plugin for DoorCrossingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DoorCrossing>()
            .init_resource::<CameraSpace>()
            .add_message::<TeleportPlayer>()
            .add_systems(Startup, spawn_fade_overlay)
            // After the streaming chain in `Update`, so the pending counts are this frame's.
            .add_systems(PostUpdate, drive_door_crossing);
    }
}

/// The full-screen black node the crossing fades.
#[derive(Component)]
pub struct FadeOverlay;

/// Where a crossing puts the player.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Landing {
    space: ActiveSpace,
    /// The cell whose residency ends the black hold.
    key: CellKey,
    /// `Some(grid)` for an exterior landing; an interior landing leaves the origin alone.
    origin: Option<IVec2>,
    camera_space: CameraSpace,
    /// FEET position in render space (relative to `origin`, or to the unchanged origin), as
    /// [`TeleportPlayer`] takes it.
    position: Vec3,
    yaw: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    FadeOut {
        elapsed: f32,
    },
    /// Black; the switch is done and the landing cell is loading.
    Landing {
        waited: f32,
        restoring: bool,
    },
    FadeIn {
        elapsed: f32,
    },
}

/// The player's own place before a crossing.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Restore {
    space: ActiveSpace,
    camera_space: CameraSpace,
    configured_worldspace: u32,
    /// Feet position in absolute render-space coordinates: the render position plus the render
    /// origin's offset, so independent of whichever origin is in force. For an interior this is
    /// just the position, interiors being placed absolutely.
    feet: Vec3,
    yaw: f32,
}

/// The absolute feet position for render-space `feet` under `origin`.
fn absolute_feet(feet: Vec3, origin: IVec2) -> Vec3 {
    feet + Vec3::new(
        origin.x as f32 * CELL_SIZE,
        0.0,
        -(origin.y as f32) * CELL_SIZE,
    )
}

impl Restore {
    fn landing(&self) -> Landing {
        match self.space.interior {
            Some(cell_id) => Landing {
                space: self.space,
                key: CellKey::Interior(cell_id),
                origin: None,
                camera_space: self.camera_space,
                position: self.feet,
                yaw: self.yaw,
            },
            None => {
                let grid = streaming_center(self.feet, IVec2::ZERO);
                Landing {
                    space: self.space,
                    key: CellKey::Exterior {
                        worldspace_id: self.space.exterior_worldspace(self.configured_worldspace),
                        grid_x: grid.x,
                        grid_y: grid.y,
                    },
                    origin: Some(grid),
                    camera_space: self.camera_space,
                    position: absolute_feet(self.feet, -grid),
                    yaw: self.yaw,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Crossing {
    door: LoadDoor,
    target: Landing,
    /// Where the player was, to put them back if the target cannot load. Kept in world terms (the
    /// absolute feet position, see [`absolute_feet`]) so a render-origin rebase before the switch
    /// cannot move it; it becomes a [`Landing`] only when it is used.
    restore: Restore,
    /// Seconds since E was pressed.
    since_press: f32,
    stage: Stage,
}

/// The crossing in progress, if any.
#[derive(Resource, Default, Debug)]
pub struct DoorCrossing {
    active: Option<Crossing>,
}

impl DoorCrossing {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

fn spawn_fade_overlay(mut commands: Commands) {
    commands.spawn((
        Name::new("Door fade"),
        FadeOverlay,
        Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.0)),
        GlobalZIndex(i32::MAX - 1),
    ));
}

fn fallback_bounds() -> ExpectedModelBounds {
    ExpectedModelBounds {
        min: FALLBACK_DOOR_MIN,
        max: FALLBACK_DOOR_MAX,
    }
}

/// The destination as the crossing's INFO line prints it: the interior cell id, or the
/// worldspace and grid square.
fn describe_destination(key: &CellKey) -> String {
    match key {
        CellKey::Interior(cell_id) => format!("interior cell {cell_id}"),
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        } => format!("worldspace {worldspace_id} grid ({grid_x}, {grid_y})"),
    }
}

/// The distance along a ray (unit `direction`) to a door's model bounds, if within `reach`.
///
/// The bounds are the converter's model-space box; the ray is taken into the door's local space
/// so a rotated or scaled door is tested as the oriented box it is.
fn ray_hits_door(
    origin: Vec3,
    direction: Vec3,
    door: &GlobalTransform,
    bounds: &ExpectedModelBounds,
    reach: f32,
) -> Option<f32> {
    let inverse = door.affine().inverse();
    let local_origin = inverse.transform_point3(origin);
    // Not renormalised, so the ray parameter stays a world distance.
    let local_direction = inverse.transform_vector3(direction);
    let mut near = 0.0f32;
    let mut far = reach;
    for axis in 0..3 {
        let (start, step) = (local_origin[axis], local_direction[axis]);
        if step.abs() < 1e-9 {
            if start < bounds.min[axis] || start > bounds.max[axis] {
                return None;
            }
            continue;
        }
        let (a, b) = (
            (bounds.min[axis] - start) / step,
            (bounds.max[axis] - start) / step,
        );
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return None;
        }
    }
    Some(near)
}

/// The landing a door's destination describes, from the space the player is in now.
fn landing_for(door: &LoadDoor, current: &ActiveSpace, configured_worldspace: u32) -> Landing {
    let destination = &door.destination;
    let creation = Vec3::from_array(destination.arrival_position);
    let yaw = creation_rotation_to_bevy(destination.arrival_rotation)
        .to_euler(EulerRot::YXZ)
        .0;
    if let Some(cell_id) = destination.interior_cell_id {
        // Interiors are placed in absolute coordinates and the origin stays as it is.
        return Landing {
            space: ActiveSpace {
                worldspace_id: Some(current.exterior_worldspace(configured_worldspace)),
                interior: Some(cell_id),
            },
            key: CellKey::Interior(cell_id),
            origin: None,
            camera_space: CameraSpace::Interior,
            position: creation_to_bevy(creation),
            yaw,
        };
    }
    let worldspace_id = destination
        .worldspace_id
        .unwrap_or_else(|| current.exterior_worldspace(configured_worldspace));
    let at = WorldPosition::from_creation_units(creation);
    Landing {
        space: ActiveSpace {
            worldspace_id: Some(worldspace_id),
            interior: None,
        },
        key: CellKey::Exterior {
            worldspace_id,
            grid_x: at.grid.x,
            grid_y: at.grid.y,
        },
        origin: Some(at.grid),
        camera_space: CameraSpace::Exterior,
        position: creation_to_bevy(at.relative_to(at.grid)),
        yaw,
    }
}

/// Switches the world to `landing` in one step. Runs as a command, so it applies before the next
/// system that reads the cells and the old space is gone the same frame.
fn switch_space(world: &mut World, landing: Landing) {
    unload_all_cells_now(world);
    *world.resource_mut::<ActiveSpace>() = landing.space;
    if let Some(grid) = landing.origin {
        world.resource_mut::<RenderOrigin>().0 = grid;
    }
    *world.resource_mut::<CameraSpace>() = landing.camera_space;
    let (_, eye) = body_and_camera_for_feet(world.resource::<MovementTuning>(), landing.position);
    // Move the camera now as well as through the message: the planner runs before the teleport is
    // applied and must already see the player in the new space.
    let mut cameras = world.query_filtered::<&mut Transform, With<StreamingCamera>>();
    for mut camera in cameras.iter_mut(world) {
        camera.translation = eye;
    }
    world.write_message(TeleportPlayer {
        position: landing.position,
        yaw: landing.yaw,
    });
}

fn set_alpha(overlay: &mut Query<&mut BackgroundColor, With<FadeOverlay>>, alpha: f32) {
    for mut colour in overlay.iter_mut() {
        colour.0 = Color::srgba(0.0, 0.0, 0.0, alpha.clamp(0.0, 1.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_door_crossing(
    keyboard: Res<ButtonInput<KeyCode>>,
    capture: Res<CursorCapture>,
    time: Res<Time>,
    config: Res<crate::config::EngineConfig>,
    tuning: Res<MovementTuning>,
    space: Res<ActiveSpace>,
    origin: Res<RenderOrigin>,
    camera_space: Res<CameraSpace>,
    streaming: Res<StreamingWorld>,
    metrics: Res<StreamingMetrics>,
    camera: Query<&Transform, With<StreamingCamera>>,
    doors: Query<(&LoadDoor, &GlobalTransform, Option<&ExpectedModelBounds>)>,
    mut crossing: ResMut<DoorCrossing>,
    mut overlay: Query<&mut BackgroundColor, With<FadeOverlay>>,
    mut commands: Commands,
) {
    let delta = time.delta_secs();
    let Some(active) = crossing.active.as_mut() else {
        // Idle: E at a door starts a crossing.
        if !keyboard.just_pressed(KeyCode::KeyE) || *capture != CursorCapture::Captured {
            return;
        }
        let Ok(view) = camera.single() else {
            return;
        };
        let direction = view.forward().as_vec3();
        let Some((door, _)) = doors
            .iter()
            .filter_map(|(door, transform, bounds)| {
                let bounds = bounds.copied().unwrap_or_else(fallback_bounds);
                ray_hits_door(view.translation, direction, transform, &bounds, DOOR_REACH)
                    .map(|distance| (door, distance))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
        else {
            return;
        };
        let (yaw, _, _) = view.rotation.to_euler(EulerRot::YXZ);
        // The camera sits an eye height above the body, and the body a capsule half-extent above
        // the feet.
        let (body_offset, _) = body_and_camera_for_feet(&tuning, Vec3::ZERO);
        let restore = Restore {
            space: *space,
            camera_space: *camera_space,
            configured_worldspace: config.worldspace_id,
            feet: absolute_feet(
                view.translation - Vec3::Y * tuning.eye_height - body_offset,
                origin.0,
            ),
            yaw,
        };
        let target = landing_for(door, &space, config.worldspace_id);
        debug!(
            door = format_args!("{:08X}", door.ref_id),
            "door crossing started"
        );
        crossing.active = Some(Crossing {
            door: door.clone(),
            target,
            restore,
            since_press: 0.0,
            stage: Stage::FadeOut { elapsed: 0.0 },
        });
        set_alpha(&mut overlay, 0.0);
        return;
    };
    active.since_press += delta;
    match active.stage {
        Stage::FadeOut { elapsed } => {
            let elapsed = elapsed + delta;
            set_alpha(&mut overlay, elapsed / FADE_SECONDS);
            if elapsed >= FADE_SECONDS {
                let target = active.target;
                commands.queue(move |world: &mut World| switch_space(world, target));
                active.stage = Stage::Landing {
                    waited: 0.0,
                    restoring: false,
                };
            } else {
                active.stage = Stage::FadeOut { elapsed };
            }
        }
        Stage::Landing { waited, restoring } => {
            set_alpha(&mut overlay, 1.0);
            let waited = waited + delta;
            let landing = if restoring {
                active.restore.landing()
            } else {
                active.target
            };
            if streaming.is_failed(landing.key) {
                if restoring {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        "door crossing: the player's own space could not be reloaded; fading in anyway"
                    );
                    active.stage = Stage::FadeIn { elapsed: 0.0 };
                } else {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        destination = ?active.target.key,
                        "door crossing: the destination could not be loaded; putting the player back"
                    );
                    let restore = active.restore.landing();
                    commands.queue(move |world: &mut World| switch_space(world, restore));
                    active.stage = Stage::Landing {
                        waited: 0.0,
                        restoring: true,
                    };
                }
                return;
            }
            let ready = streaming.is_resident(landing.key)
                && metrics.pending_asset_instances == 0
                && metrics.pending_surface_instances == 0;
            if ready || waited >= LANDING_TIMEOUT_SECONDS {
                if !ready {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        waited_seconds = waited,
                        "door crossing: the destination was not ready in time; fading in anyway"
                    );
                }
                info!(
                    door = format_args!("{:08X}", active.door.ref_id),
                    destination = %describe_destination(&active.target.key),
                    milliseconds = (active.since_press * 1000.0) as u32,
                    "door crossing: fade-in starts"
                );
                active.stage = Stage::FadeIn { elapsed: 0.0 };
            } else {
                active.stage = Stage::Landing { waited, restoring };
            }
        }
        Stage::FadeIn { elapsed } => {
            let elapsed = elapsed + delta;
            set_alpha(&mut overlay, 1.0 - elapsed / FADE_SECONDS);
            if elapsed >= FADE_SECONDS {
                crossing.active = None;
            } else {
                active.stage = Stage::FadeIn { elapsed };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        doors::DoorDestination, physics::MoveMode, profiling::ProfilingState,
        streaming::StreamingWorld,
    };
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    const DT: f32 = 0.05;

    #[derive(Resource, Default)]
    struct Teleports(Vec<TeleportPlayer>);

    fn collect(mut reader: MessageReader<TeleportPlayer>, mut out: ResMut<Teleports>) {
        out.0.extend(reader.read().copied());
    }

    fn door(destination: DoorDestination) -> LoadDoor {
        LoadDoor {
            ref_id: 0x10,
            destination,
        }
    }

    fn interior_door() -> LoadDoor {
        door(DoorDestination {
            destination_ref_id: 0x20,
            interior_cell_id: Some(77),
            worldspace_id: None,
            arrival_position: [100.0, 200.0, 300.0],
            arrival_rotation: [0.0, 0.0, 0.0],
        })
    }

    fn exterior_door() -> LoadDoor {
        door(DoorDestination {
            destination_ref_id: 0x21,
            interior_cell_id: None,
            worldspace_id: Some(60),
            // Grid (3, -2) in Creation units, 100 into the cell on each axis.
            arrival_position: [3.0 * 4096.0 + 100.0, -2.0 * 4096.0 + 100.0, 50.0],
            arrival_rotation: [0.0, 0.0, 0.0],
        })
    }

    /// A headless world with the crossing registered, the camera 100 units in front of a door
    /// whose bounds are a 100-unit cube, and one resident old-space cell.
    fn app_with(door: LoadDoor) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin));
        app.init_resource::<ProfilingState>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<StreamingWorld>()
            .init_resource::<crate::streaming::TerrainContinuity>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ActiveSpace>()
            .init_resource::<MovementTuning>()
            .init_resource::<Teleports>()
            .insert_resource(MoveMode::Noclip)
            .insert_resource(CursorCapture::Captured)
            .insert_resource(RenderOrigin(IVec2::new(1, 1)))
            .insert_resource(crate::config::EngineConfig::default())
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
                DT,
            )))
            .add_plugins(DoorCrossingPlugin)
            .add_systems(Last, collect);
        app.world_mut().spawn((
            StreamingCamera,
            Transform::from_xyz(0.0, 0.0, 0.0).looking_to(Vec3::NEG_Z, Vec3::Y),
            GlobalTransform::default(),
        ));
        app.world_mut().spawn((
            door,
            ExpectedModelBounds::new(Vec3::splat(-50.0), Vec3::splat(50.0)).unwrap(),
            Transform::from_xyz(0.0, 0.0, -150.0),
            GlobalTransform::default(),
        ));
        let old_root = app.world_mut().spawn_empty().id();
        let old = CellKey::Exterior {
            worldspace_id: 1,
            grid_x: 1,
            grid_y: 1,
        };
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(old, old_root);
        app.finish();
        app.update();
        (app, old_root)
    }

    fn press_e(app: &mut App) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyE);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(KeyCode::KeyE);
    }

    fn alpha(app: &mut App) -> f32 {
        let mut query = app
            .world_mut()
            .query_filtered::<&BackgroundColor, With<FadeOverlay>>();
        query.single(app.world()).unwrap().0.to_srgba().alpha
    }

    fn run_until_black(app: &mut App) {
        for _ in 0..20 {
            if app.world().resource::<ActiveSpace>() != &ActiveSpace::default() {
                return;
            }
            app.update();
        }
        panic!("the space never switched");
    }

    #[test]
    fn e_at_a_door_starts_a_crossing_and_e_elsewhere_does_not() {
        let (mut app, _) = app_with(interior_door());
        // Turn away from the door: nothing starts.
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            let mut camera = query.single_mut(app.world_mut()).unwrap();
            camera.look_to(Vec3::Z, Vec3::Y);
        }
        press_e(&mut app);
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        // Face it, but from beyond reach.
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            let mut camera = query.single_mut(app.world_mut()).unwrap();
            camera.translation = Vec3::new(0.0, 0.0, 400.0);
            camera.look_to(Vec3::NEG_Z, Vec3::Y);
        }
        press_e(&mut app);
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            query.single_mut(app.world_mut()).unwrap().translation = Vec3::ZERO;
        }
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
        // E while it runs does not start another.
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
    }

    #[test]
    fn the_space_switches_at_full_black_and_the_old_cells_go_the_same_frame() {
        let (mut app, old_root) = app_with(interior_door());
        press_e(&mut app);
        assert!(alpha(&mut app) < 1.0);
        run_until_black(&mut app);
        assert!(alpha(&mut app) >= 0.99, "the switch happens at full black");
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.interior, Some(77));
        assert!(app.world().get_entity(old_root).is_err());
        assert_eq!(
            app.world()
                .resource::<StreamingWorld>()
                .cell_count_for_test(),
            0
        );
        assert_eq!(
            *app.world().resource::<CameraSpace>(),
            CameraSpace::Interior
        );
        // An interior landing leaves the render origin alone.
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(1, 1));
        let teleports = &app.world().resource::<Teleports>().0;
        assert_eq!(teleports.len(), 1);
        assert_eq!(
            teleports[0].position,
            creation_to_bevy(Vec3::new(100.0, 200.0, 300.0))
        );
    }

    #[test]
    fn an_exterior_landing_sets_the_render_origin_to_the_arrival_grid() {
        let (mut app, _) = app_with(exterior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(3, -2));
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.worldspace_id, Some(60));
        assert_eq!(space.interior, None);
        let teleports = &app.world().resource::<Teleports>().0;
        // 100 units into the cell on each axis, relative to the new origin: no cell offset.
        assert_eq!(
            teleports[0].position,
            creation_to_bevy(Vec3::new(100.0, 100.0, 50.0))
        );
    }

    #[test]
    fn the_fade_in_waits_for_the_landing_cell_and_starts_within_a_frame_of_it() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        for _ in 0..30 {
            app.update();
        }
        assert!(
            alpha(&mut app) >= 0.99,
            "still black while nothing is resident"
        );
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        // Something still pending keeps it black.
        app.world_mut()
            .resource_mut::<StreamingMetrics>()
            .pending_asset_instances = 1;
        app.update();
        assert!(alpha(&mut app) >= 0.99);
        app.world_mut()
            .resource_mut::<StreamingMetrics>()
            .pending_asset_instances = 0;
        app.update();
        app.update();
        assert!(alpha(&mut app) < 1.0, "the fade-in starts once it is ready");
        for _ in 0..10 {
            app.update();
        }
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        assert_eq!(alpha(&mut app), 0.0);
    }

    #[test]
    fn the_timeout_fades_in_anyway() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let frames = (LANDING_TIMEOUT_SECONDS / DT) as usize;
        for _ in 0..frames - 5 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99);
        for _ in 0..20 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0, "the timeout started the fade-in");
    }

    #[test]
    fn a_destination_that_fails_puts_the_player_back_where_they_were() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space, ActiveSpace::default());
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(1, 1));
        let teleports = &app.world().resource::<Teleports>().0;
        assert_eq!(teleports.len(), 2);
        let back = teleports[1].position;
        assert!(back.x.abs() < 0.01 && back.z.abs() < 0.01);
    }

    #[test]
    fn the_restore_pose_survives_a_render_origin_rebase_during_the_fade_out() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        // A rebase while fading out: the origin moves, the stored pose must not.
        app.world_mut().resource_mut::<RenderOrigin>().0 = IVec2::new(5, 5);
        run_until_black(&mut app);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        let origin = app.world().resource::<RenderOrigin>().0;
        let teleports = &app.world().resource::<Teleports>().0;
        let back = teleports.last().unwrap().position;
        // The player stood at render (0, _, 0) under origin (1, 1): the same world place now.
        let world_x = back.x + origin.x as f32 * CELL_SIZE;
        let world_z = back.z - origin.y as f32 * CELL_SIZE;
        assert!((world_x - CELL_SIZE).abs() < 0.01, "x {world_x}");
        assert!((world_z + CELL_SIZE).abs() < 0.01, "z {world_z}");
        let tuning = MovementTuning::default();
        let feet_y = -tuning.eye_height - (tuning.capsule_half_height() + tuning.capsule_radius);
        assert!((back.y - feet_y).abs() < 0.01);
    }

    #[test]
    fn a_door_without_model_bounds_can_still_be_activated() {
        let (mut app, _) = app_with(interior_door());
        let mut doors = app.world_mut().query_filtered::<Entity, With<LoadDoor>>();
        let door = doors.single(app.world()).unwrap();
        app.world_mut()
            .entity_mut(door)
            .remove::<ExpectedModelBounds>();
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
    }

    #[test]
    fn the_teleport_target_is_the_arrival_feet_position() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let tuning = MovementTuning::default();
        let (_, eye) =
            body_and_camera_for_feet(&tuning, creation_to_bevy(Vec3::new(100.0, 200.0, 300.0)));
        let mut view = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>();
        assert!((view.single(app.world()).unwrap().translation - eye).length() < 1e-3);
    }

    #[test]
    fn a_ray_tests_the_oriented_bounds() {
        let bounds =
            ExpectedModelBounds::new(Vec3::new(-10.0, 0.0, -2.0), Vec3::new(10.0, 40.0, 2.0))
                .unwrap();
        let door = GlobalTransform::from(
            Transform::from_xyz(0.0, 0.0, -100.0).with_rotation(Quat::from_rotation_y(0.5)),
        );
        let hit = ray_hits_door(
            Vec3::new(0.0, 20.0, 0.0),
            Vec3::NEG_Z,
            &door,
            &bounds,
            200.0,
        );
        assert!(hit.is_some_and(|distance| (distance - 98.0).abs() < 5.0));
        assert!(ray_hits_door(Vec3::new(0.0, 20.0, 0.0), Vec3::Z, &door, &bounds, 200.0).is_none());
        assert!(
            ray_hits_door(Vec3::new(0.0, 20.0, 0.0), Vec3::NEG_Z, &door, &bounds, 50.0).is_none()
        );
    }
}
