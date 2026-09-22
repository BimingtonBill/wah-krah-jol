//! Load-door crossings between cells and worldspaces. See docs/design/blackreach-demo.md.
//!
//! Two jobs, both of them cheap enough to run every frame:
//!
//! * **Pre-stream.** Every load door within [`DOOR_PRESTREAM_RADIUS`] of the camera puts its
//!   destination into [`PrestreamCells`], which the streaming plan merges into its wanted set, so
//!   the destination is already resident before the camera gets there. That is the whole of "no
//!   loading screen": the crossing itself is a camera move.
//! * **Cross.** On [`ActivateDoor`], set [`ActiveCell`] from the door's link, place the camera at
//!   the arrival point the converter stored in `XTEL`, and write [`DoorCrossed`].
//!
//! Both run in [`DoorTransition`], which the streaming plan is ordered after, so the plan sees
//! the camera's new cell and the door's pre-stream in the frame they change.

use crate::{
    doors::{ActivateDoor, DoorCrossed, LoadDoor},
    profiling::ProfilingState,
    streaming::{
        ActiveCell, PrestreamCells, RenderOrigin, creation_rotation_to_bevy, creation_to_bevy,
        render_position, reposition_cell_roots,
    },
    world::components::{CELL_SIZE, ExteriorCellGrid, StreamingCamera},
};
use bevy::prelude::*;

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
            .add_message::<DoorCrossed>()
            .init_resource::<PrestreamCells>()
            .add_systems(
                Update,
                // Cross first, then plan from where the camera ended up, so the door just left
                // does not pre-stream the cell just entered.
                (apply_door_crossings, plan_door_prestream)
                    .chain()
                    .in_set(DoorTransition),
            );
    }
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
/// `creation_rotation_to_bevy` places *objects*: it turns a reference's forward, Creation `+Y`,
/// into runtime space. Applied to the camera as-is, it left the arriving player looking back at the
/// door they came through: every crossing on the Alftand route arrived facing the return door, which
/// the portal then immediately picked as the door in view (engine log, 2026-09-22). A camera looks
/// down its `-Z`, the opposite way from the object convention, so the arrival view needs a half
/// turn about the up axis.
pub(crate) fn arrival_camera_rotation(rotation: [f32; 3]) -> Quat {
    creation_rotation_to_bevy(rotation) * Quat::from_rotation_y(std::f32::consts::PI)
}

/// Moves the camera through a load door.
///
/// The camera lands on the `XTEL` arrival point, which is *not* the destination door's position,
/// and faces the `XTEL` arrival rotation: for a door that is a yaw, and the converted rotation
/// points the camera's forward (runtime `-Z`, i.e. Creation `+Y`) the way the arriving player
/// faces.
#[allow(clippy::too_many_arguments)]
fn apply_door_crossings(
    mut requests: MessageReader<ActivateDoor>,
    doors: Query<&LoadDoor>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut active: ResMut<ActiveCell>,
    mut origin: ResMut<RenderOrigin>,
    mut roots: Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    mut crossed: MessageWriter<DoorCrossed>,
    mut profiler: ResMut<ProfilingState>,
) {
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    for request in requests.read() {
        let Ok(door) = doors.get(request.door) else {
            // The door was unloaded between the request and this frame.
            continue;
        };
        let arrival = Vec3::from_array(door.destination.arrival_position);
        let translation = if let Some(cell_id) = door.destination.interior_cell_id {
            // An interior root sits at the render origin and its references carry the interior's
            // absolute creation coordinates, so the camera does too and the origin must not move.
            active.interior = Some(cell_id);
            creation_to_bevy(arrival)
        } else if let Some(worldspace_id) = door.destination.worldspace_id {
            active.worldspace_id = worldspace_id;
            active.interior = None;
            // The destination was pre-streamed, so its roots were placed for the origin the
            // camera is leaving. Take the origin to the arrival cell and re-place them, exactly
            // as a rebase does, which leaves the camera near the origin of the new worldspace.
            let arrival_in_world = creation_to_bevy(arrival);
            origin.0 = IVec2::new(
                (arrival_in_world.x / CELL_SIZE).floor() as i32,
                (-arrival_in_world.z / CELL_SIZE).floor() as i32,
            );
            reposition_cell_roots(origin.0, &mut roots);
            render_position(arrival, origin.0)
        } else {
            continue;
        };
        camera.translation = translation;
        camera.rotation = arrival_camera_rotation(door.destination.arrival_rotation);
        profiler.increment("doors/crossed", 1);
        profiler.event(format!("{:08X}", door.ref_id), "door_crossed", None);
        crossed.write(DoorCrossed {
            from_ref_id: door.ref_id,
            label: door.label.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::EngineConfig,
        doors::DoorDestination,
        render::{TerrainMaterial, WaterMaterial, WaterReflectionTexture},
        streaming::{StreamingMetrics, StreamingPlugin, StreamingWorld},
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
        let expected = creation_to_bevy(Vec3::from_array([-947.038, 3958.835, 591.917]));
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
        // The arrival point of (21088.559, 18512.045, 2434) with grid (5, 4) as the origin.
        assert!(
            camera_transform
                .translation
                .abs_diff_eq(Vec3::new(608.559, 2434.0, -2128.045), 1.0e-2),
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
        // Creation-engine actors face +Y; a door's arrival yaw turns that direction, and it has
        // to be the camera's forward (runtime -Z) after the conversion.
        for yaw in [0.0_f32, 2.96989, -1.8708, 1.2] {
            let rotation = creation_rotation_to_bevy([0.0, 0.0, yaw]);
            let expected = creation_to_bevy(Vec3::new(-yaw.sin(), yaw.cos(), 0.0));
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

    #[test]
    fn a_crossing_streams_its_destination_before_the_camera_arrives() {
        let directory = tempfile::tempdir().unwrap();
        let (database_path, cache_path) = write_fixture(directory.path());
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
        // The reference at 8200, -12200, 50 of cell (2, -3) renders at (8, 50, -88) while the
        // origin is that cell: the camera stands 200 units from the door, inside the pre-stream
        // radius and still inside the cell.
        let camera = spawn_camera(&mut app, Vec3::new(8.0, 50.0, -288.0));

        run_until(&mut app, "the door of the exterior cell", |app| {
            !app.world().resource::<DoorWatch>().0.is_empty()
        });
        let (door, load_door) = {
            let watch = app.world().resource::<DoorWatch>();
            assert_eq!(watch.0.len(), 1, "one reference of one cell is a door");
            watch.0[0].clone()
        };
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
        let arrival = creation_to_bevy(Vec3::from_array([-947.038, 3958.835, 591.917]));
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
}
