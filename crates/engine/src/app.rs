use crate::{
    config::EngineConfig,
    lod::LodPlugin,
    metrics::AcceptanceMetricsPlugin,
    profiling::{ProfilingPlugin, ProfilingState},
    render::{
        RendererMetrics, TerrainExtension, TerrainMaterial, VercidiumRendererPlugin,
        WaterExtension, WaterMaterial, WaterReflectionTexture,
    },
    streaming::{
        AssetFailure, RenderOrigin, StreamingMetrics, StreamingPlugin, build_terrain_quadrant_mesh,
        validate_standard_material,
    },
    world::{
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{CELL_SIZE, ExpectedModelBounds, InstanceBounds, StreamingCamera},
        database::{AssetCatalog, LodBlockTable, WorldDatabase},
    },
};
use bevy::{
    asset::{AssetPlugin, RenderAssetUsages},
    camera::primitives::MeshAabb,
    camera::visibility::RenderLayers,
    core_pipeline::prepass::DepthPrepass,
    diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin},
    prelude::*,
    render::diagnostic::RenderDiagnosticsPlugin,
    render::occlusion_culling::OcclusionCulling,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    render::view::screenshot::{Screenshot, save_to_disk},
    tasks::{IoTaskPool, TaskPoolBuilder},
    window::{PresentMode, WindowPlugin},
    winit::WinitSettings,
};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Resource)]
struct InitialCameraGroundHeight(f32);

/// Registers the streaming tier and, only when asked, the distant-LOD tier.
///
/// The flag is the whole decision. `LodPlugin` owns `finish_commit_budget`, the
/// system that closes the shared commit window, so installing it beside an
/// LOD-disabled `StreamingPlugin` would re-time every LOD-off frame against a
/// window that overlaps the cell plan and the LOD plan — the numbers would stop
/// being comparable with acceptance runs made before distant LOD existed. With
/// the flag off the tier is absent from the schedule entirely, so no LOD system
/// runs and the cell path keeps upstream `main`'s window.
fn add_streaming_tiers(app: &mut App, lod_enabled: bool) {
    app.add_plugins(StreamingPlugin);
    if lod_enabled {
        app.add_plugins(LodPlugin);
    }
}

pub fn run(mut config: EngineConfig) -> Result<()> {
    configure_io_task_pool();
    let fixture_dir = if config.streaming_fixture || config.lod_fixture {
        let fixture = StreamingFixtureDirectory::create(
            config.worldspace_id,
            config.start_grid,
            config.lod_fixture,
        )?;
        config.assets_dir = fixture.path.clone();
        Some(fixture)
    } else {
        None
    };
    let runtime_data = if config.streaming_fixture || config.lod_fixture {
        let database_path = config.assets_dir.join("skyrim_world.db");
        Some((
            WorldDatabase::open(&database_path)?,
            AssetCatalog::open(&database_path)?,
            CellCache::open(&config.assets_dir.join("cell_cache.rkyv"))?,
            InitialCameraGroundHeight(0.0),
        ))
    } else if config.benchmark_only
        || config.material_fixture
        || config.terrain_water_fixture
        || config.transform_bounds_fixture
        || config.renderer_fixture
    {
        None
    } else {
        validate_runtime_assets(&config)?;
        let database_path = config.assets_dir.join("skyrim_world.db");
        let cache = CellCache::open(&config.assets_dir.join("cell_cache.rkyv"))?;
        let ground_height = initial_camera_ground_height(&config, &database_path, &cache)?;
        Some((
            WorldDatabase::open(&database_path)?,
            AssetCatalog::open(&database_path)?,
            cache,
            InitialCameraGroundHeight(ground_height),
        ))
    };
    let asset_path = config.assets_dir.to_string_lossy().into_owned();
    let benchmark_active =
        config.benchmark_frames.is_some() || config.benchmark_duration_secs.is_some();
    configure_benchmark_priority(benchmark_active)?;
    let window = (!config.headless).then(|| Window {
        title: "OpenSkyrim".into(),
        resolution: (1600, 900).into(),
        present_mode: if benchmark_active {
            PresentMode::AutoNoVsync
        } else {
            PresentMode::AutoVsync
        },
        ..default()
    });
    let origin = RenderOrigin(IVec2::new(config.start_grid.0, config.start_grid.1));
    let mut app = App::new();
    if benchmark_active {
        // Acceptance runs are commonly left unfocused while the campaign driver
        // advances through its scenarios. Bevy's game default throttles an
        // unfocused window to 60 Hz, which makes a 16.67 ms P95 gate measure the
        // event-loop sleep instead of renderer performance.
        app.insert_resource(WinitSettings::continuous());
    }
    app.insert_resource(config)
        .insert_resource(origin)
        .init_resource::<StreamingMetrics>()
        .add_plugins(
            DefaultPlugins
                .set(AssetPlugin {
                    file_path: asset_path,
                    ..default()
                })
                .set(WindowPlugin {
                    primary_window: window,
                    ..default()
                }),
        )
        .add_plugins((
            FrameTimeDiagnosticsPlugin::default(),
            LogDiagnosticsPlugin::default(),
            AcceptanceMetricsPlugin,
            ProfilingPlugin,
            RenderDiagnosticsPlugin,
        ))
        .add_plugins(VercidiumRendererPlugin)
        .add_systems(Update, (fly_camera, capture_acceptance_screenshot));
    if let Some((database, catalog, cache, ground_height)) = runtime_data {
        // The table is the planner's availability input, so it is read once
        // here rather than per frame; an empty table is what a LOD-off run has.
        let lod_table = {
            let config = app.world().resource::<EngineConfig>();
            if config.lod_enabled {
                LodBlockTable::open(
                    &config.assets_dir.join("skyrim_world.db"),
                    config.worldspace_id,
                )?
            } else {
                LodBlockTable::default()
            }
        };
        let lod_enabled = app.world().resource::<EngineConfig>().lod_enabled;
        app.insert_resource(database)
            .insert_resource(catalog)
            .insert_resource(cache)
            .insert_resource(ground_height)
            .insert_resource(lod_table);
        add_streaming_tiers(&mut app, lod_enabled);
        app.add_systems(Startup, setup_world);
        if app.world().resource::<EngineConfig>().streaming_fixture {
            app.init_resource::<StreamingFixtureState>()
                .add_systems(Startup, setup_streaming_fixture_visual)
                .add_systems(PreUpdate, drive_streaming_fixture)
                .add_systems(PostUpdate, validate_streaming_fixture);
        }
        if app.world().resource::<EngineConfig>().lod_fixture {
            app.init_resource::<LodFixtureState>()
                .add_systems(PreUpdate, drive_lod_fixture)
                .add_systems(PostUpdate, validate_lod_fixture);
        }
    } else if app.world().resource::<EngineConfig>().material_fixture {
        app.add_systems(Startup, setup_material_fixture)
            .add_systems(Update, validate_material_fixture);
    } else if app.world().resource::<EngineConfig>().terrain_water_fixture {
        app.add_systems(PostStartup, setup_terrain_water_fixture)
            .add_systems(Update, validate_terrain_water_fixture);
    } else if app
        .world()
        .resource::<EngineConfig>()
        .transform_bounds_fixture
    {
        app.add_systems(Startup, setup_transform_bounds_fixture)
            .add_systems(Update, validate_transform_bounds_fixture);
    } else if app.world().resource::<EngineConfig>().renderer_fixture {
        app.add_systems(Startup, setup_renderer_fixture)
            .add_systems(Update, validate_renderer_fixture);
    } else {
        app.add_systems(Startup, setup_world);
        app.add_systems(Startup, setup_synthetic_benchmark);
    }
    app.run();
    drop(app);
    drop(fixture_dir);
    Ok(())
}

#[cfg(windows)]
fn configure_benchmark_priority(benchmark_active: bool) -> Result<()> {
    if benchmark_active {
        use windows_sys::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
        };
        // SAFETY: GetCurrentProcess returns the current process pseudo-handle,
        // which is valid for SetPriorityClass and must not be closed.
        let configured =
            unsafe { SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) };
        if configured == 0 {
            return Err(std::io::Error::last_os_error())
                .wrap_err("failed to set benchmark process priority");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn configure_benchmark_priority(_benchmark_active: bool) -> Result<()> {
    Ok(())
}

fn configure_io_task_pool() {
    let threads = std::thread::available_parallelism()
        .map(|count| count.get().div_ceil(4).clamp(1, 4))
        .unwrap_or(1);
    IoTaskPool::get_or_init(|| {
        TaskPoolBuilder::new()
            .num_threads(threads)
            .thread_name("IO Task Pool".to_owned())
            .stack_size(8 * 1024 * 1024)
            .build()
    });
}

struct StreamingFixtureDirectory {
    path: PathBuf,
}

impl StreamingFixtureDirectory {
    fn create(worldspace_id: u32, start_grid: (i32, i32), lod: bool) -> Result<Self> {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "openskyrim-streaming-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&path).wrap_err_with(|| format!("failed to create {}", path.display()))?;
        let fixture = Self { path };
        fixture.populate(worldspace_id, start_grid, lod)?;
        Ok(fixture)
    }

    fn populate(&self, worldspace_id: u32, start_grid: (i32, i32), lod: bool) -> Result<()> {
        let database_path = self.path.join("skyrim_world.db");
        let mut connection = Connection::open(&database_path)?;
        connection.execute_batch(&format!(
            r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
            INSERT INTO schema_info VALUES({});
            CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER);
            CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
            CREATE TABLE statics(id INTEGER PRIMARY KEY,model_path TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL);
            CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER,base_form_id INTEGER,pos_x REAL,pos_y REAL,pos_z REAL,rot_x REAL,rot_y REAL,rot_z REAL,scale REAL);
            CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
            CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT);
            CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);
            CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);
            CREATE TABLE lod_grid(worldspace_id INTEGER PRIMARY KEY,origin_x INTEGER NOT NULL,origin_y INTEGER NOT NULL,levels TEXT NOT NULL);
            CREATE TABLE lod_block(worldspace_id INTEGER NOT NULL,kind TEXT NOT NULL,level INTEGER NOT NULL,block_x INTEGER NOT NULL,block_y INTEGER NOT NULL,mesh_path TEXT NOT NULL,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,PRIMARY KEY (worldspace_id,kind,level,block_x,block_y));"#,
            shared::WORLD_DATABASE_SCHEMA_VERSION,
        ))?;
        // One transaction for the grid: 9409 auto-committed inserts cost a disk
        // flush each and dominated every fixture-based test.
        let transaction = connection.transaction()?;
        let mut insert = transaction
            .prepare("INSERT INTO cells(id,worldspace_id,grid_x,grid_y) VALUES(?1,?2,?3,?4)")?;
        let mut cell_id = 1u32;
        for grid_y in start_grid.1.saturating_sub(48)..=start_grid.1.saturating_add(48) {
            for grid_x in start_grid.0.saturating_sub(48)..=start_grid.0.saturating_add(48) {
                insert.execute(params![cell_id, worldspace_id, grid_x, grid_y])?;
                cell_id += 1;
            }
        }
        drop(insert);
        transaction.commit()?;
        if lod {
            populate_lod_fixture(&connection, &self.path, worldspace_id)?;
        }
        drop(connection);
        let cache = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: Vec::new(),
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&cache)
            .wrap_err("failed to archive streaming fixture cache")?;
        fs::write(self.path.join("cell_cache.rkyv"), bytes)?;
        Ok(())
    }
}

/// The worldspace directory `meshes/terrain/<name>` the fixture writes.
///
/// The engine reads the mesh path from `lod_block`, so the name only has to be
/// consistent between the row and the file; it matches the retail Tamriel
/// directory so a fixture dump reads like a converted set.
const LOD_FIXTURE_DIRECTORY: &str = "tamriel";

/// The cell offset of every fixture block, by level.
const LOD_FIXTURE_BLOCKS: [(u8, &[i32]); 3] =
    [(4, &[-8, -4, 0, 4]), (8, &[-8, 0]), (16, &[-16, 0])];

/// The model-space bounds of the fixture quad, which every `lod_block` row
/// repeats so readiness validation has something to compare against.
const LOD_FIXTURE_BOUNDS: ([f32; 3], [f32; 3]) = ([0.0, 0.0, 0.0], [1.0, 0.0, 1.0]);

/// Writes a small patch of `lod_block` rows and one GLB per block.
///
/// The GLBs are written here rather than by the converter: the engine must not
/// depend on the converter crate, and the fixture only needs a loadable scene
/// with a texture dependency.
fn populate_lod_fixture(
    connection: &Connection,
    root: &std::path::Path,
    worldspace_id: u32,
) -> Result<()> {
    connection.execute(
        "INSERT INTO lod_grid(worldspace_id,origin_x,origin_y,levels) VALUES(?1,0,0,'4,8,16')",
        params![worldspace_id],
    )?;
    let directory = root
        .join("meshes")
        .join("terrain")
        .join(LOD_FIXTURE_DIRECTORY);
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("lod-fixture.png"), fixture_png())
        .wrap_err("failed to write the LOD fixture texture")?;
    let mut insert = connection.prepare(
        "INSERT OR REPLACE INTO lod_block(worldspace_id,kind,level,block_x,block_y,mesh_path,\
         bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z) \
         VALUES(?1,'terrain',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )?;
    for (level, offsets) in LOD_FIXTURE_BLOCKS {
        for block_x in offsets {
            for block_y in offsets {
                let name = format!("{LOD_FIXTURE_DIRECTORY}.{level}.{block_x}.{block_y}.glb");
                write_fixture_glb(&directory.join(&name))?;
                insert.execute(params![
                    worldspace_id,
                    level,
                    block_x,
                    block_y,
                    format!("meshes/terrain/{LOD_FIXTURE_DIRECTORY}/{name}"),
                    LOD_FIXTURE_BOUNDS.0[0],
                    LOD_FIXTURE_BOUNDS.0[1],
                    LOD_FIXTURE_BOUNDS.0[2],
                    LOD_FIXTURE_BOUNDS.1[0],
                    LOD_FIXTURE_BOUNDS.1[1],
                    LOD_FIXTURE_BOUNDS.1[2],
                ])?;
            }
        }
    }
    Ok(())
}

/// Writes a minimal GLB: one node, one quad with POSITION, NORMAL and
/// TEXCOORD_0, and a material whose base colour texture is the fixture PNG.
///
/// The texture is the point: it makes the block's load state depend on a
/// recursive dependency, which is the path the real converted GLBs take.
fn write_fixture_glb(path: &std::path::Path) -> Result<()> {
    let positions: [[f32; 3]; 4] = [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0],
    ];
    let normals: [[f32; 3]; 4] = [[0.0, 1.0, 0.0]; 4];
    let uvs: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
    let mut bin = Vec::new();
    for value in positions.iter().flatten() {
        bin.extend_from_slice(&value.to_le_bytes());
    }
    let normal_offset = bin.len();
    for value in normals.iter().flatten() {
        bin.extend_from_slice(&value.to_le_bytes());
    }
    let uv_offset = bin.len();
    for value in uvs.iter().flatten() {
        bin.extend_from_slice(&value.to_le_bytes());
    }
    let index_offset = bin.len();
    for value in indices {
        bin.extend_from_slice(&value.to_le_bytes());
    }
    let document = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{
            "primitives": [{
                "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 },
                "indices": 3,
                "material": 0,
            }],
        }],
        "materials": [{
            "pbrMetallicRoughness": {
                "baseColorTexture": { "index": 0 },
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0,
            },
        }],
        "textures": [{ "source": 0 }],
        "images": [{ "uri": "lod-fixture.png" }],
        "accessors": [
            {
                "bufferView": 0,
                "componentType": 5126,
                "count": 4,
                "type": "VEC3",
                "min": LOD_FIXTURE_BOUNDS.0,
                "max": LOD_FIXTURE_BOUNDS.1,
            },
            { "bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3" },
            { "bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC2" },
            { "bufferView": 3, "componentType": 5123, "count": 6, "type": "SCALAR" },
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 48, "target": 34962 },
            { "buffer": 0, "byteOffset": normal_offset, "byteLength": 48, "target": 34962 },
            { "buffer": 0, "byteOffset": uv_offset, "byteLength": 32, "target": 34962 },
            { "buffer": 0, "byteOffset": index_offset, "byteLength": 12, "target": 34963 },
        ],
        "buffers": [{ "byteLength": bin.len() }],
    });
    let mut json = serde_json::to_vec(&document).wrap_err("failed to encode the fixture glTF")?;
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut glb = Vec::with_capacity(total);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(&0x004E_4942u32.to_le_bytes());
    glb.extend_from_slice(&bin);
    fs::write(path, glb).wrap_err_with(|| format!("failed to write {}", path.display()))
}

/// A one-pixel RGBA PNG, encoded by hand so the fixture needs no image crate.
fn fixture_png() -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&1u32.to_be_bytes());
    header.extend_from_slice(&1u32.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_png_chunk(&mut png, b"IHDR", &header);
    // One scanline: a filter byte and one RGBA pixel, in a stored deflate block.
    let raw = [0u8, 0x40, 0x80, 0x40, 0xff];
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend_from_slice(&(raw.len() as u16).to_le_bytes());
    zlib.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    push_png_chunk(&mut png, b"IDAT", &zlib);
    push_png_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_png_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    png.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut low = 1u32;
    let mut high = 0u32;
    for byte in bytes {
        low = (low + u32::from(*byte)) % 65521;
        high = (high + low) % 65521;
    }
    (high << 16) | low
}

impl Drop for StreamingFixtureDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            warn!(%error, path = %self.path.display(), "failed to remove streaming fixture directory");
        }
    }
}

#[derive(Resource, Default)]
struct StreamingFixtureState {
    frames: u32,
    total_x: i32,
    total_y: i32,
    finished: bool,
}

fn setup_streaming_fixture_visual(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
) {
    let mesh = Mesh3d(meshes.add(Cuboid::new(180.0, 480.0, 180.0)));
    let material = MeshMaterial3d(materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::srgb(0.22, 0.48, 0.18),
            perceptual_roughness: 0.88,
            ..default()
        },
        extension: TerrainExtension::default(),
    }));
    commands.spawn_batch((0..64).map(move |index| {
        let x = index % 8;
        let z = index / 8;
        (
            mesh.clone(),
            material.clone(),
            Transform::from_xyz(700.0 + x as f32 * 360.0, 240.0, -700.0 - z as f32 * 360.0),
        )
    }));
}

fn drive_streaming_fixture(
    mut state: ResMut<StreamingFixtureState>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    let Some((x, y, label)) = (match state.frames {
        4 => Some((6, 0, "rapid_traversal")),
        5 => Some((0, -7, "rapid_traversal")),
        6 => Some((18, 12, "teleport")),
        14 => Some((-30, -9, "teleport")),
        22 => Some((9, 5, "rapid_traversal")),
        30 => Some((-state.total_x, -state.total_y, "return_to_origin")),
        _ => None,
    }) else {
        return;
    };
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    camera.translation.x += x as f32 * crate::world::components::CELL_SIZE;
    camera.translation.z -= y as f32 * crate::world::components::CELL_SIZE;
    state.total_x += x;
    state.total_y += y;
    profiler.event("streaming-fixture", label, None);
}

fn validate_streaming_fixture(
    config: Res<EngineConfig>,
    mut state: ResMut<StreamingFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished || state.frames < 90 {
        return;
    }
    let expected_resident = ((config.stream_radius * 2 + 1).max(0) as usize).pow(2);
    let maximum_resident = ((config.unload_radius * 2 + 1).max(0) as usize).pow(2);
    let settled = metrics.active_requests == 0 && metrics.loading_cells == 0;
    let valid = settled
        && metrics.requests_submitted > expected_resident as u64
        && metrics.responses_received > 0
        && metrics.stale_responses > 0
        && metrics.unloaded_cells > 0
        && metrics.origin_rebases >= 6
        && metrics.resident_cells >= expected_resident
        && metrics.resident_cells <= maximum_resident
        && metrics.resident_roots == metrics.resident_cells
        && metrics.out_of_range_cell_roots == 0
        && metrics.streaming_invariant_failures == 0
        && metrics.commit_frames > 0;
    if valid {
        metrics.streaming_fixture_validated = true;
        profiler.event("streaming-fixture", "validated", None);
        state.finished = true;
    } else if state.frames >= 300 {
        metrics.streaming_fixture_failures = metrics.streaming_fixture_failures.saturating_add(1);
        error!(
            ?metrics,
            "streaming fixture did not settle or violated its lifecycle contract"
        );
        profiler.event("streaming-fixture", "failed", None);
        state.finished = true;
    }
}

#[derive(Resource, Default)]
struct LodFixtureState {
    frames: u32,
    total_x: i32,
    total_y: i32,
    finished: bool,
}

/// Walks the fixture camera across band boundaries and out of the patch, so the
/// fixture sees a request, an unload and a return rather than one steady state.
fn drive_lod_fixture(
    mut state: ResMut<LodFixtureState>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    let Some((x, y, label)) = (match state.frames {
        4 => Some((6, 0, "band_crossing")),
        5 => Some((0, -6, "band_crossing")),
        // Far enough to leave every band, close enough to stay inside the
        // fixture's cell grid, which the cell tier still reads.
        12 => Some((40, 0, "outside_the_patch")),
        24 => Some((-state.total_x, -state.total_y, "return_to_origin")),
        _ => None,
    }) else {
        return;
    };
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    camera.translation.x += x as f32 * CELL_SIZE;
    camera.translation.z -= y as f32 * CELL_SIZE;
    state.total_x += x;
    state.total_y += y;
    profiler.event("lod-fixture", label, None);
}

fn validate_lod_fixture(
    mut state: ResMut<LodFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished || state.frames < 90 {
        return;
    }
    // Stale responses are expected here: the driver teleports away from the
    // patch, so blocks that were in flight are cancelled and their answers
    // dropped. Everything else must be clean.
    let settled = metrics.lod_blocks_loading == 0;
    let valid = settled
        && metrics.lod_blocks_resident > 0
        && metrics.lod_blocks_covered_skipped > 0
        && metrics.lod_unloaded_blocks > 0
        && metrics.lod_commits > 0
        && metrics.lod_asset_failures == 0
        && metrics.lod_validation_failures == 0
        && metrics.lod_invariant_failures == 0;
    if valid {
        metrics.lod_fixture_validated = true;
        profiler.event("lod-fixture", "validated", None);
        state.finished = true;
    } else if state.frames >= 400 {
        metrics.lod_fixture_failures = metrics.lod_fixture_failures.saturating_add(1);
        error!(
            ?metrics,
            "LOD fixture did not settle or violated its lifecycle contract"
        );
        profiler.event("lod-fixture", "failed", None);
        state.finished = true;
    }
}

#[derive(Component, Debug, Clone, Copy)]
enum CanonicalMaterialKind {
    Opaque,
    Cutout,
    Blend,
    Emissive,
    DoubleSided,
    NormalMapped,
}

#[derive(Resource, Default)]
struct CanonicalMaterialFixtureState {
    finished: bool,
}

fn fixture_image(data: Vec<u8>, srgb: bool) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        if srgb {
            TextureFormat::Rgba8UnormSrgb
        } else {
            TextureFormat::Rgba8Unorm
        },
        RenderAssetUsages::default(),
    );
    image.sampler = bevy::image::ImageSampler::linear();
    image
}

fn setup_material_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.init_resource::<CanonicalMaterialFixtureState>();
    let checker = images.add(fixture_image(
        (0..16)
            .flat_map(|index| {
                let alpha = if (index + index / 4) % 2 == 0 { 255 } else { 0 };
                [78, 166, 88, alpha]
            })
            .collect(),
        true,
    ));
    let normal = images.add(fixture_image(
        (0..16).flat_map(|_| [128, 128, 255, 255]).collect(),
        false,
    ));
    let definitions = [
        (
            CanonicalMaterialKind::Opaque,
            StandardMaterial {
                base_color: Color::srgb(0.55, 0.42, 0.25),
                perceptual_roughness: 0.75,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Cutout,
            StandardMaterial {
                base_color_texture: Some(checker),
                alpha_mode: AlphaMode::Mask(0.5),
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Blend,
            StandardMaterial {
                base_color: Color::srgba(0.15, 0.45, 0.9, 0.45),
                alpha_mode: AlphaMode::Blend,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Emissive,
            StandardMaterial {
                base_color: Color::srgb(0.08, 0.08, 0.08),
                emissive: LinearRgba::new(6.0, 1.2, 0.15, 1.0),
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::DoubleSided,
            StandardMaterial {
                base_color: Color::srgb(0.75, 0.2, 0.18),
                double_sided: true,
                cull_mode: None,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::NormalMapped,
            StandardMaterial {
                base_color: Color::srgb(0.45, 0.48, 0.52),
                normal_map_texture: Some(normal),
                ..default()
            },
        ),
    ];
    let mesh = meshes.add(Cuboid::new(2.2, 2.2, 2.2));
    for (index, (kind, material)) in definitions.into_iter().enumerate() {
        commands.spawn((
            Name::new(format!("Canonical {kind:?}")),
            kind,
            Mesh3d(mesh.clone()),
            MeshMaterial3d(materials.add(material)),
            Transform::from_xyz((index as f32 - 2.5) * 2.8, 0.0, 0.0),
        ));
    }
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 5.0, 18.0).looking_at(Vec3::ZERO, Vec3::Y),
        StreamingCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.7, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 120.0,
        ..default()
    });
}

fn validate_material_fixture(
    query: Query<(&CanonicalMaterialKind, &MeshMaterial3d<StandardMaterial>)>,
    materials: Res<Assets<StandardMaterial>>,
    images: Res<Assets<Image>>,
    mut state: ResMut<CanonicalMaterialFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished || query.iter().count() != 6 {
        return;
    }
    let mut validated_images = 0usize;
    for (kind, handle) in &query {
        let result = materials
            .get(handle)
            .ok_or_else(|| "material is not loaded".to_owned())
            .and_then(|material| {
                match kind {
                    CanonicalMaterialKind::Opaque if material.alpha_mode != AlphaMode::Opaque => {
                        Err("opaque mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Cutout
                        if !matches!(material.alpha_mode, AlphaMode::Mask(_)) =>
                    {
                        Err("mask mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Blend if material.alpha_mode != AlphaMode::Blend => {
                        Err("blend mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Emissive if material.emissive.red <= 0.0 => {
                        Err("emissive intensity was lost".to_owned())
                    }
                    CanonicalMaterialKind::DoubleSided
                        if !material.double_sided || material.cull_mode.is_some() =>
                    {
                        Err("double-sided culling was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::NormalMapped
                        if material.normal_map_texture.is_none() =>
                    {
                        Err("normal map was not preserved".to_owned())
                    }
                    _ => Ok(()),
                }?;
                validate_standard_material(material, &images)
            });
        match result {
            Ok(count) => validated_images += count,
            Err(reason) => {
                metrics.asset_load_failures += 1;
                metrics.material_validation_failures += 1;
                metrics.asset_failures.push(AssetFailure {
                    model_path: format!("canonical-material-fixture/{kind:?}"),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: 0,
                    dependency_chain: vec![reason],
                });
                profiler.increment("assets/load_failures", 1);
            }
        }
    }
    metrics.materials_validated += 6;
    metrics.images_validated += validated_images as u64;
    metrics.canonical_fixture_validated = metrics.material_validation_failures == 0;
    state.finished = true;
}

#[derive(Component)]
struct TerrainWaterFixtureTerrain;

#[derive(Component)]
struct TerrainWaterFixtureWater;

#[derive(Resource, Default)]
struct TerrainWaterFixtureState {
    finished: bool,
}

fn setup_terrain_water_fixture(
    mut commands: Commands,
    reflection: Res<WaterReflectionTexture>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.init_resource::<TerrainWaterFixtureState>();
    let palette = [
        [82, 116, 58, 255],
        [122, 101, 70, 255],
        [83, 92, 102, 255],
        [146, 138, 103, 255],
        [60, 91, 54, 255],
        [113, 82, 62, 255],
    ];
    let texture_handles: [Handle<Image>; 6] =
        palette.map(|pixel| images.add(fixture_image((0..16).flat_map(|_| pixel).collect(), true)));
    let flow_normal = images.add(fixture_image(
        (0..16)
            .flat_map(|index| {
                if index % 2 == 0 {
                    [150, 110, 255, 255]
                } else {
                    [110, 150, 255, 255]
                }
            })
            .collect(),
        false,
    ));
    let mut layers = Vec::new();
    for quadrant in 0..4 {
        layers.push(TerrainLayerSnapshot {
            texture_form_id: 1,
            quadrant,
            layer: 0,
            is_base: true,
            weights: Vec::new(),
        });
        for layer in 1..=5u16 {
            let weights = (0usize..17 * 17)
                .filter_map(|vertex| {
                    let x = vertex % 17;
                    let y = vertex / 17;
                    let center = (layer as usize * 3).min(16);
                    let distance = x.abs_diff(center).min(y.abs_diff(center));
                    (distance < 3).then(|| (vertex as u16, (3 - distance) as f32 * 0.12))
                })
                .collect();
            layers.push(TerrainLayerSnapshot {
                texture_form_id: u32::from(layer) + 1,
                quadrant,
                layer,
                is_base: false,
                weights,
            });
        }
    }
    let terrain = TerrainSnapshot {
        cell_id: 0xF170_0001,
        width: 33,
        height: 33,
        heights: (0..33 * 33)
            .map(|index| {
                let x = (index % 33) as f32 - 16.0;
                let y = (index / 33) as f32 - 16.0;
                45.0 * (x * 0.22).sin() + 35.0 * (y * 0.18).cos()
            })
            .collect(),
        normals: (0..33 * 33).flat_map(|_| [0, 0, 127]).collect(),
        vertex_colors: (0..33 * 33)
            .flat_map(|index| {
                let shade = 190 + (index % 33) as u8;
                [shade, shade, shade]
            })
            .collect(),
        layers,
        water_height: Some(12.0),
        water_type_form_id: Some(1),
    };
    for quadrant in 0..4 {
        commands.spawn((
            Name::new(format!("Terrain/water fixture quadrant {quadrant}")),
            Mesh3d(
                meshes.add(
                    build_terrain_quadrant_mesh(&terrain, quadrant)
                        .expect("canonical terrain fixture must build"),
                ),
            ),
            MeshMaterial3d(terrain_materials.add(TerrainMaterial {
                base: StandardMaterial {
                    base_color: Color::WHITE,
                    perceptual_roughness: 0.92,
                    cull_mode: None,
                    double_sided: true,
                    ..default()
                },
                extension: TerrainExtension::fixture(texture_handles.clone()),
            })),
            TerrainWaterFixtureTerrain,
        ));
    }
    commands.spawn((
        Name::new("Terrain/water fixture water"),
        Mesh3d(meshes.add(Plane3d::default().mesh().size(2200.0, 2200.0))),
        MeshMaterial3d(water_materials.add(WaterMaterial {
            base: StandardMaterial {
                base_color: Color::srgba(0.04, 0.2, 0.32, 0.7),
                metallic: 0.15,
                perceptual_roughness: 0.06,
                reflectance: 0.9,
                alpha_mode: AlphaMode::Blend,
                ..default()
            },
            extension: WaterExtension::with_reflection(reflection.0.clone(), Some(flow_normal)),
        })),
        Transform::from_xyz(CELL_SIZE_HALF, 12.0, -CELL_SIZE_HALF),
        crate::world::components::WaterSurface,
        TerrainWaterFixtureWater,
        RenderLayers::layer(1),
    ));
    let target = Vec3::new(CELL_SIZE_HALF, 0.0, -CELL_SIZE_HALF);
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(CELL_SIZE_HALF, 1800.0, 2600.0).looking_at(target, Vec3::Y),
        StreamingCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::from_layers(&[0, 1]),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.48, 0.55, 0.7),
        brightness: 160.0,
        ..default()
    });
}

fn validate_terrain_water_fixture(
    terrain: Query<(&Mesh3d, &MeshMaterial3d<TerrainMaterial>), With<TerrainWaterFixtureTerrain>>,
    water: Query<&MeshMaterial3d<WaterMaterial>, With<TerrainWaterFixtureWater>>,
    meshes: Res<Assets<Mesh>>,
    terrain_materials: Res<Assets<TerrainMaterial>>,
    water_materials: Res<Assets<WaterMaterial>>,
    mut state: ResMut<TerrainWaterFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
) {
    if state.finished || terrain.iter().count() != 4 || water.iter().count() != 1 {
        return;
    }
    let valid_terrain = terrain.iter().all(|(mesh, material)| {
        meshes.get(mesh).is_some() && terrain_materials.get(material).is_some()
    });
    let valid_water = water
        .single()
        .ok()
        .and_then(|material| water_materials.get(material))
        .is_some();
    if valid_terrain && valid_water {
        metrics.terrain_patches_validated += 4;
        metrics.water_surfaces_validated += 1;
        metrics.materials_validated += 5;
        metrics.images_validated += 7;
        metrics.terrain_water_fixture_validated = true;
    } else {
        metrics.terrain_validation_failures += (!valid_terrain) as u64;
        metrics.water_validation_failures += (!valid_water) as u64;
    }
    state.finished = true;
}

#[derive(Component)]
struct TransformBoundsFixtureRoot;

#[derive(Resource, Default)]
struct TransformBoundsFixtureState {
    frames: u8,
    finished: bool,
}

fn setup_transform_bounds_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.init_resource::<TransformBoundsFixtureState>();
    let beam_mesh = meshes.add(Cuboid::new(2.0, 4.0, 1.5));
    let cube_mesh = meshes.add(Cuboid::new(2.0, 2.0, 2.0));
    let cap_mesh = meshes.add(Cuboid::new(6.5, 0.7, 1.2));
    let stone = materials.add(StandardMaterial {
        base_color: Color::srgb(0.38, 0.46, 0.58),
        perceptual_roughness: 0.72,
        ..default()
    });
    let bronze = materials.add(StandardMaterial {
        base_color: Color::srgb(0.72, 0.39, 0.12),
        metallic: 0.45,
        perceptual_roughness: 0.42,
        ..default()
    });
    let moss = materials.add(StandardMaterial {
        base_color: Color::srgb(0.22, 0.48, 0.24),
        perceptual_roughness: 0.86,
        ..default()
    });

    let left = Transform::from_xyz(-2.5, 0.0, 0.0)
        .with_rotation(Quat::from_rotation_z(0.28))
        .with_scale(Vec3::new(1.0, 1.35, 0.75));
    let group = Transform::from_xyz(2.0, 0.5, 0.0)
        .with_rotation(Quat::from_rotation_y(-0.42))
        .with_scale(Vec3::new(0.8, 1.3, 0.65));
    let nested = Transform::from_xyz(1.0, 1.0, 0.0)
        .with_rotation(Quat::from_rotation_x(0.31))
        .with_scale(Vec3::new(1.2, 0.5, 1.7));
    let cap = Transform::from_xyz(0.0, 3.8, 0.0)
        .with_rotation(Quat::from_euler(EulerRot::YXZ, 0.18, -0.12, 0.08))
        .with_scale(Vec3::new(1.05, 0.8, 1.25));

    let mut expected_min = Vec3::splat(f32::INFINITY);
    let mut expected_max = Vec3::splat(f32::NEG_INFINITY);
    for bounds in [
        InstanceBounds::transformed(
            Vec3::new(-1.0, -2.0, -0.75),
            Vec3::new(1.0, 2.0, 0.75),
            left.to_matrix(),
        ),
        InstanceBounds::transformed(
            Vec3::splat(-1.0),
            Vec3::splat(1.0),
            group.to_matrix() * nested.to_matrix(),
        ),
        InstanceBounds::transformed(
            Vec3::new(-3.25, -0.35, -0.6),
            Vec3::new(3.25, 0.35, 0.6),
            cap.to_matrix(),
        ),
    ] {
        expected_min = expected_min.min(bounds.min);
        expected_max = expected_max.max(bounds.max);
    }

    commands
        .spawn((
            Name::new("Canonical transform/bounds assembly"),
            TransformBoundsFixtureRoot,
            ExpectedModelBounds {
                min: expected_min,
                max: expected_max,
            },
            Transform::from_xyz(0.0, -1.0, 0.0)
                .with_rotation(Quat::from_rotation_y(0.48))
                .with_scale(Vec3::new(1.1, 0.9, 1.2)),
            Visibility::default(),
        ))
        .with_children(|parent| {
            parent.spawn((
                Name::new("Rotated left support"),
                Mesh3d(beam_mesh),
                MeshMaterial3d(stone),
                left,
            ));
            parent
                .spawn((
                    Name::new("Non-uniform hierarchy pivot"),
                    group,
                    Visibility::default(),
                ))
                .with_child((
                    Name::new("Nested rotated support"),
                    Mesh3d(cube_mesh),
                    MeshMaterial3d(bronze),
                    nested,
                ));
            parent.spawn((
                Name::new("Rotated top cap"),
                Mesh3d(cap_mesh),
                MeshMaterial3d(moss),
                cap,
            ));
        });
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(2.0, 5.5, 16.0).looking_at(Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
        StreamingCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.7, -0.55, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 140.0,
        ..default()
    });
}

#[allow(clippy::too_many_arguments)]
fn validate_transform_bounds_fixture(
    roots: Query<
        (Entity, &ExpectedModelBounds, &GlobalTransform),
        With<TransformBoundsFixtureRoot>,
    >,
    children: Query<&Children>,
    nodes: Query<(&Transform, &GlobalTransform, Option<&Mesh3d>)>,
    meshes: Res<Assets<Mesh>>,
    mut state: ResMut<TransformBoundsFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    if state.frames < 3 {
        return;
    }
    let result = (|| -> Result<(usize, usize), String> {
        let (root, expected, root_global) = roots
            .single()
            .map_err(|_| "canonical transform fixture root is missing".to_owned())?;
        let root_inverse = root_global.affine().inverse();
        let mut actual_min = Vec3::splat(f32::INFINITY);
        let mut actual_max = Vec3::splat(f32::NEG_INFINITY);
        let mut node_count = 0usize;
        let mut mesh_count = 0usize;
        for descendant in children.iter_descendants(root) {
            let (local, global, mesh) = nodes
                .get(descendant)
                .map_err(|_| format!("fixture node {descendant:?} has no transform"))?;
            if !local.to_matrix().is_finite()
                || !global.to_matrix().is_finite()
                || local.scale.abs().min_element() <= 1.0e-6
            {
                return Err(format!(
                    "fixture node {descendant:?} has an invalid transform"
                ));
            }
            node_count += 1;
            let Some(mesh) = mesh else { continue };
            let aabb = meshes
                .get(mesh)
                .and_then(MeshAabb::compute_aabb)
                .ok_or_else(|| format!("fixture mesh {:?} has no bounds", mesh.id()))?;
            let center = Vec3::from(aabb.center);
            let half = Vec3::from(aabb.half_extents);
            let bounds = InstanceBounds::transformed(
                center - half,
                center + half,
                Mat4::from(root_inverse * global.affine()),
            );
            actual_min = actual_min.min(bounds.min);
            actual_max = actual_max.max(bounds.max);
            mesh_count += 1;
        }
        let error = (actual_min - expected.min)
            .abs()
            .max((actual_max - expected.max).abs())
            .max_element();
        (mesh_count == 3 && error <= 1.0e-4)
            .then_some((node_count, mesh_count))
            .ok_or_else(|| {
                format!(
                    "hierarchy bounds mismatch: expected {:?}..{:?}, actual {:?}..{:?}",
                    expected.min, expected.max, actual_min, actual_max
                )
            })
    })();
    match result {
        Ok((nodes, meshes)) => {
            metrics.transform_instances_validated += 1;
            metrics.transform_nodes_validated += nodes as u64;
            metrics.bounds_validated += meshes as u64;
            metrics.transform_bounds_fixture_validated = true;
            profiler.increment("transforms/fixture_validated", 1);
        }
        Err(reason) => {
            metrics.asset_load_failures += 1;
            metrics.transform_bounds_validation_failures += 1;
            metrics.asset_failures.push(AssetFailure {
                model_path: "fixtures/transform-bounds-assembly".to_owned(),
                reference_form_id: 0,
                base_form_id: 0,
                cell_id: 0,
                dependency_chain: vec![reason],
            });
            profiler.increment("transforms/validation_failures", 1);
        }
    }
    state.finished = true;
}

#[derive(Component)]
struct RendererFixtureCenterVisible;

#[derive(Component)]
struct RendererFixtureRightVisible;

#[derive(Component)]
struct RendererFixtureLeftVisible;

#[derive(Resource, Default)]
struct RendererFixtureState {
    frames: u16,
    phase_started: u16,
    phase: u8,
    center_seen: bool,
    right_seen: bool,
    left_seen: bool,
    finished: bool,
}

fn setup_renderer_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.init_resource::<RendererFixtureState>();
    let cube = meshes.add(Cuboid::new(2.0, 2.0, 2.0));
    let wall = meshes.add(Cuboid::new(12.0, 10.0, 1.0));
    let opaque = materials.add(StandardMaterial {
        base_color: Color::srgb(0.28, 0.3, 0.34),
        perceptual_roughness: 0.9,
        ..default()
    });
    let green = materials.add(StandardMaterial {
        base_color: Color::srgb(0.12, 0.8, 0.2),
        ..default()
    });
    let red = materials.add(StandardMaterial {
        base_color: Color::srgb(0.85, 0.08, 0.05),
        ..default()
    });
    let blue = materials.add(StandardMaterial {
        base_color: Color::srgb(0.08, 0.35, 0.9),
        ..default()
    });
    let gold = materials.add(StandardMaterial {
        base_color: Color::srgb(0.9, 0.55, 0.08),
        metallic: 0.25,
        ..default()
    });
    commands.spawn((
        Name::new("Renderer fixture occluder"),
        Mesh3d(wall),
        MeshMaterial3d(opaque),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture front visible"),
        RendererFixtureCenterVisible,
        Mesh3d(cube.clone()),
        MeshMaterial3d(green),
        Transform::from_xyz(0.0, 0.0, 5.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture fully occluded"),
        Mesh3d(cube.clone()),
        MeshMaterial3d(red),
        Transform::from_xyz(0.0, 0.0, -4.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture visible after right turn"),
        RendererFixtureRightVisible,
        Mesh3d(cube.clone()),
        MeshMaterial3d(blue),
        Transform::from_xyz(10.0, 0.0, -2.0)
            .with_rotation(Quat::from_rotation_y(0.45))
            .with_scale(Vec3::new(1.8, 0.7, 1.2)),
    ));
    commands.spawn((
        Name::new("Renderer fixture visible after left turn"),
        RendererFixtureLeftVisible,
        Mesh3d(cube),
        MeshMaterial3d(gold),
        Transform::from_xyz(-10.0, 0.0, -2.0)
            .with_rotation(Quat::from_euler(EulerRot::XYZ, 0.25, -0.5, 0.18))
            .with_scale(Vec3::new(0.65, 2.1, 1.4)),
    ));
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 1.5, 16.0).looking_at(Vec3::ZERO, Vec3::Y),
        StreamingCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.65, -0.45, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 130.0,
        ..default()
    });
}

fn validate_renderer_fixture(
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    center: Query<&ViewVisibility, With<RendererFixtureCenterVisible>>,
    right: Query<&ViewVisibility, With<RendererFixtureRightVisible>>,
    left: Query<&ViewVisibility, With<RendererFixtureLeftVisible>>,
    mut state: ResMut<RendererFixtureState>,
    mut renderer: ResMut<RendererMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    let phase_frames = state.frames.saturating_sub(state.phase_started);
    let center_visible = center.single().is_ok_and(|visibility| visibility.get());
    let right_visible = right.single().is_ok_and(|visibility| visibility.get());
    let left_visible = left.single().is_ok_and(|visibility| visibility.get());
    match state.phase {
        0 if phase_frames >= 10 && renderer.final_path_active() && center_visible => {
            state.center_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0)
                    .looking_at(Vec3::new(10.0, 0.0, -2.0), Vec3::Y);
            }
            state.phase = 1;
            state.phase_started = state.frames;
        }
        1 if phase_frames >= 8 && right_visible => {
            state.right_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0)
                    .looking_at(Vec3::new(-10.0, 0.0, -2.0), Vec3::Y);
            }
            state.phase = 2;
            state.phase_started = state.frames;
        }
        2 if phase_frames >= 8 && left_visible => {
            state.left_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0).looking_at(Vec3::ZERO, Vec3::Y);
            }
            state.phase = 3;
            state.phase_started = state.frames;
        }
        3 if phase_frames >= 8 && center_visible && renderer.final_path_active() => {
            renderer.renderer_fixture_validated =
                state.center_seen && state.right_seen && state.left_seen;
            renderer.renderer_validation_failures += (!renderer.renderer_fixture_validated) as u64;
            profiler.increment("renderer/fixture_validated", 1);
            state.finished = true;
        }
        _ if state.frames >= 180 => {
            renderer.renderer_validation_failures =
                renderer.renderer_validation_failures.saturating_add(1);
            profiler.increment("renderer/validation_failures", 1);
            state.finished = true;
        }
        _ => {}
    }
}

#[derive(Deserialize)]
struct RuntimeManifest {
    schema_version: u32,
    complete: bool,
}

#[derive(Deserialize)]
struct RuntimeIntegrationReport {
    schema_version: u32,
    passed: bool,
}

fn validate_runtime_assets(config: &EngineConfig) -> Result<()> {
    for required in ["skyrim_world.db", "cell_cache.rkyv"] {
        color_eyre::eyre::ensure!(
            config.assets_dir.join(required).is_file(),
            "converted asset set is missing {required}: {}",
            config.assets_dir.display()
        );
    }
    if config.allow_incomplete_assets {
        return Ok(());
    }
    let manifest_path = config.assets_dir.join("conversion-manifest.json");
    let manifest: RuntimeManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .wrap_err("invalid conversion manifest")?;
    color_eyre::eyre::ensure!(
        manifest.schema_version == converter_schema_version() && manifest.complete,
        "asset conversion is incomplete or stale; reconvert assets with converter schema {}",
        converter_schema_version()
    );
    let report_path = config.assets_dir.join("integration-report.json");
    let report: RuntimeIntegrationReport = serde_json::from_slice(
        &std::fs::read(&report_path)
            .wrap_err_with(|| format!("failed to read {}", report_path.display()))?,
    )
    .wrap_err("invalid integration report")?;
    color_eyre::eyre::ensure!(
        report.schema_version == shared::WORLD_DATABASE_SCHEMA_VERSION && report.passed,
        "asset integration report did not pass; inspect {}",
        report_path.display()
    );
    Ok(())
}

const fn converter_schema_version() -> u32 {
    // Kept in sync with converter::cache::CONVERTER_SCHEMA_VERSION without
    // linking the heavy converter crate into the runtime binary.
    14
}

fn setup_synthetic_benchmark(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = std::time::Instant::now();
    let mesh = Mesh3d(meshes.add(Cuboid::new(18.0, 60.0, 18.0)));
    let material = MeshMaterial3d(terrain_materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::srgb(0.16, 0.36, 0.12),
            perceptual_roughness: 0.9,
            ..default()
        },
        extension: TerrainExtension::default(),
    }));
    let side = (config.synthetic_instances as f64).sqrt().ceil() as usize;
    commands.spawn_batch((0..config.synthetic_instances).map(move |index| {
        let x = index % side;
        let z = index / side;
        (
            mesh.clone(),
            material.clone(),
            Transform::from_xyz(x as f32 * 32.0, 30.0, -(z as f32 * 32.0)),
        )
    }));
    info!(
        instances = config.synthetic_instances,
        "synthetic indirect-render benchmark initialized"
    );
    profiler.increment("synthetic/instances", config.synthetic_instances as u64);
    profiler.record_elapsed("startup/synthetic_scene", started);
}

/// The camera's far plane.
///
/// With LOD the plane must reach past the coarsest band *and* across the widest
/// admitted block: admission measures to the block rectangle, so a level-16
/// block admitted at 100 000 units measures roughly 92 681 units across its
/// diagonal. Without LOD the plane stays exactly as it was.
fn camera_far_plane(config: &EngineConfig) -> f32 {
    let full_detail = CELL_SIZE * (config.stream_radius.max(1) + 2) as f32 * 2.0;
    if !config.lod_enabled {
        return full_detail;
    }
    let coarsest = config
        .lod_bands
        .iter()
        .map(|band| band.distance)
        .fold(0.0f32, f32::max);
    let widest = config
        .lod_bands
        .iter()
        .map(|band| f32::from(band.level))
        .fold(0.0f32, f32::max);
    coarsest
        + CELL_SIZE * (widest * std::f32::consts::SQRT_2 + config.stream_radius.max(0) as f32 + 2.0)
}

fn setup_world(
    mut commands: Commands,
    config: Res<EngineConfig>,
    clear_color: Option<Res<ClearColor>>,
    ground_height: Option<Res<InitialCameraGroundHeight>>,
) {
    let ground_height = ground_height.as_deref().map_or(0.0, |height| height.0);
    let target = Vec3::new(CELL_SIZE_HALF, ground_height, -CELL_SIZE_HALF);
    let camera_offset = if config.acceptance_screenshot.is_some() {
        Vec3::new(0.0, 20_000.0, 1000.0)
    } else {
        Vec3::new(0.0, 1200.0, 2500.0)
    };
    let camera_position = target + camera_offset;
    let far = camera_far_plane(&config);
    let mut camera = commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection { far, ..default() }),
        Transform::from_translation(camera_position).looking_at(target, Vec3::Y),
        StreamingCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::from_layers(&[0, 1]),
    ));
    if config.lod_enabled {
        // Without fog the LOD horizon would end in a hard silhouette against
        // the sky, because the engine draws no atmosphere of its own. Gated on
        // the flag, so no existing scenario's pixels move.
        let coarsest = config
            .lod_bands
            .iter()
            .map(|band| band.distance)
            .fold(0.0f32, f32::max);
        camera.insert(DistanceFog {
            color: clear_color.as_deref().map_or(Color::BLACK, |clear| clear.0),
            falloff: FogFalloff::Linear {
                start: coarsest * 0.5,
                end: camera_far_plane(&config),
            },
            ..default()
        });
    }
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.48, 0.55, 0.7),
        brightness: 160.0,
        ..default()
    });
    info!(
        assets = %config.assets_dir.display(),
        worldspace = format_args!("{:08X}", config.worldspace_id),
        ground_height,
        camera = ?camera_position,
        target = ?target,
        "OpenSkyrim runtime initialized"
    );
}

fn initial_camera_ground_height(
    config: &EngineConfig,
    database_path: &std::path::Path,
    cache: &CellCache,
) -> Result<f32> {
    let connection = Connection::open(database_path)
        .wrap_err_with(|| format!("failed to open {}", database_path.display()))?;
    let cell_id = connection
        .query_row(
            crate::world::database::EXTERIOR_CELL_ID_SQL,
            params![
                config.worldspace_id,
                config.start_grid.0,
                config.start_grid.1
            ],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    let Some(terrain) = cell_id.and_then(|cell_id| cache.terrain(cell_id)) else {
        return Ok(0.0);
    };
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    let center = (height / 2)
        .checked_mul(width)
        .and_then(|row| row.checked_add(width / 2));
    Ok(center
        .and_then(|index| terrain.heights.get(index))
        .copied()
        .unwrap_or(0.0))
}

const CELL_SIZE_HALF: f32 = crate::world::components::CELL_SIZE * 0.5;
const AUTO_FLIGHT_HALF_SPAN: f32 = crate::world::components::CELL_SIZE * 4.0;

#[derive(Default)]
struct AutoFlightState {
    initialized: bool,
    axis: Vec3,
    sign: f32,
    offset: f32,
}

fn bounded_auto_flight_direction(
    forward: Vec3,
    step_distance: f32,
    state: &mut AutoFlightState,
) -> Vec3 {
    if !state.initialized {
        state.initialized = true;
        state.axis = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z);
        state.sign = 1.0;
    }
    let next_offset = state.offset + state.sign * step_distance.max(0.0);
    if next_offset >= AUTO_FLIGHT_HALF_SPAN {
        state.sign = -1.0;
    } else if next_offset <= -AUTO_FLIGHT_HALF_SPAN {
        state.sign = 1.0;
    }
    state.offset += state.sign * step_distance.max(0.0);
    state.axis * state.sign
}

fn fly_camera(
    time: Res<Time>,
    config: Res<EngineConfig>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
    mut auto_flight: Local<AutoFlightState>,
) {
    let started = std::time::Instant::now();
    let Ok(mut transform) = camera.single_mut() else {
        return;
    };
    let mut direction = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyW) {
        direction += *transform.forward();
    }
    if keyboard.pressed(KeyCode::KeyS) {
        direction += *transform.back();
    }
    if keyboard.pressed(KeyCode::KeyA) {
        direction += *transform.left();
    }
    if keyboard.pressed(KeyCode::KeyD) {
        direction += *transform.right();
    }
    if keyboard.pressed(KeyCode::Space) {
        direction += Vec3::Y;
    }
    if keyboard.pressed(KeyCode::ShiftLeft) {
        direction -= Vec3::Y;
    }
    let acceptance_capture_pending = config
        .acceptance_screenshot
        .as_ref()
        .is_some_and(|path| !path.is_file());
    let speed = if config.auto_fly_speed > 0.0 {
        config.auto_fly_speed
    } else if keyboard.pressed(KeyCode::ControlLeft) {
        4000.0
    } else {
        900.0
    };
    if config.auto_fly_speed > 0.0 && !acceptance_capture_pending {
        direction += bounded_auto_flight_direction(
            *transform.forward(),
            speed * time.delta_secs(),
            &mut auto_flight,
        );
    }
    transform.translation += direction.normalize_or_zero() * speed * time.delta_secs();
    profiler.record_elapsed("world/fly_camera", started);
}

fn capture_acceptance_screenshot(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut state: Local<ScreenshotCaptureState>,
    streaming: Option<Res<StreamingMetrics>>,
    renderer: Res<RendererMetrics>,
    windows: Query<(), With<Window>>,
) {
    let Some(path) = &config.acceptance_screenshot else {
        return;
    };
    state.frames = state.frames.saturating_add(1);
    let gpu_warmed_up = state
        .started
        .get_or_insert_with(std::time::Instant::now)
        .elapsed()
        >= std::time::Duration::from_secs(2);
    if state.captured
        || state.frames < config.benchmark_warmup_frames.saturating_add(10)
        || !gpu_warmed_up
        || windows.is_empty()
    {
        return;
    }
    let assets_ready = streaming.as_deref().is_none_or(|metrics| {
        metrics.pending_asset_instances == 0
            && metrics.pending_surface_instances == 0
            && metrics.asset_load_failures == 0
            && metrics.material_validation_failures == 0
            && metrics.transform_bounds_validation_failures == 0
            && metrics.diagnostic_fallbacks == 0
            && metrics.streaming_invariant_failures == 0
            && metrics.streaming_fixture_failures == 0
            && metrics.lod_asset_failures == 0
            && metrics.lod_validation_failures == 0
            && metrics.lod_invariant_failures == 0
            && metrics.lod_fixture_failures == 0
            && (!config.lod_fixture || metrics.lod_fixture_validated)
            && (!config.material_fixture || metrics.canonical_fixture_validated)
            && (!config.terrain_water_fixture || metrics.terrain_water_fixture_validated)
            && (!config.transform_bounds_fixture || metrics.transform_bounds_fixture_validated)
            && (!config.streaming_fixture || metrics.streaming_fixture_validated)
    });
    let renderer_ready = renderer.final_path_active()
        && (!config.renderer_fixture || renderer.renderer_fixture_validated);
    if !assets_ready || !renderer_ready {
        return;
    }
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        error!(%error, path = %path.display(), "failed to create screenshot directory");
        return;
    }
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path.clone()));
    state.captured = true;
}

#[derive(Default)]
struct ScreenshotCaptureState {
    frames: u32,
    captured: bool,
    started: Option<std::time::Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lod::{LodBlockStatus, LodWorld, block_translation, validate_lod_lifecycle},
        streaming::{CommitBudget, update_render_origin},
        world::{
            components::LodBlockRoot,
            database::{LodBlockKey, LodBlockKind},
        },
    };
    use std::time::{Duration, Instant};

    /// Builds a headless app that runs the distant-LOD tier against the fixture.
    ///
    /// The renderer is deliberately absent: the tier's contract is what the
    /// engine commits, hides, rebases and unloads, and the asset pipeline
    /// (glTF scene, meshes, textures) all runs in the main world. Adding
    /// `RenderPlugin` here would make the test need a GPU.
    fn lod_fixture_app(config: EngineConfig) -> App {
        let database = config.assets_dir.join("skyrim_world.db");
        let worldspace_id = config.worldspace_id;
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            AssetPlugin {
                file_path: config.assets_dir.to_string_lossy().into_owned(),
                ..default()
            },
            bevy::image::ImagePlugin::default(),
            bevy::transform::TransformPlugin,
            bevy::gltf::GltfPlugin::default(),
            bevy::world_serialization::WorldSerializationPlugin,
        ));
        app.init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .init_resource::<CommitBudget>()
            .init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(LodBlockTable::open(&database, worldspace_id).unwrap())
            .insert_resource(WorldDatabase::open(&database).unwrap())
            .add_plugins(LodPlugin);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        app
    }

    /// Builds a headless app that runs the cell tier against the fixture.
    ///
    /// The tier is registered through [`add_streaming_tiers`], the same call
    /// `run` makes, so the test sees the schedule the engine actually builds
    /// for a given flag. The renderer is absent for the reason given on
    /// [`lod_fixture_app`]; the fixture's cells carry no references, so a commit
    /// only has to spawn a root.
    fn streaming_fixture_app(config: EngineConfig) -> App {
        let lod_enabled = config.lod_enabled;
        let database = config.assets_dir.join("skyrim_world.db");
        let cache = config.assets_dir.join("cell_cache.rkyv");
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            AssetPlugin {
                file_path: config.assets_dir.to_string_lossy().into_owned(),
                ..default()
            },
            bevy::image::ImagePlugin::default(),
            bevy::transform::TransformPlugin,
            bevy::gltf::GltfPlugin::default(),
            bevy::world_serialization::WorldSerializationPlugin,
        ));
        app.init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .init_resource::<CommitBudget>()
            .init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(WorldDatabase::open(&database).unwrap())
            .insert_resource(CellCache::open(&cache).unwrap())
            .insert_resource(AssetCatalog::open(&database).unwrap())
            .insert_resource(config);
        let reflection =
            app.world_mut()
                .resource_mut::<Assets<Image>>()
                .add(Image::new_target_texture(
                    64,
                    64,
                    TextureFormat::Rgba8Unorm,
                    Some(TextureFormat::Rgba8UnormSrgb),
                ));
        app.insert_resource(WaterReflectionTexture(reflection));
        add_streaming_tiers(&mut app, lod_enabled);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        app
    }

    /// Runs frames until the resident block count stops moving.
    fn settle_lod_tier(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stable = 0;
        let mut last = usize::MAX;
        loop {
            app.update();
            let resident = app
                .world()
                .resource::<StreamingMetrics>()
                .lod_blocks_resident;
            if resident == last && resident > 0 {
                stable += 1;
            } else {
                stable = 0;
                last = resident;
            }
            if stable >= 6 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the distant-LOD tier did not settle"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn lod_fixture_config(
        directory: &StreamingFixtureDirectory,
        stream_radius: i32,
    ) -> EngineConfig {
        EngineConfig {
            assets_dir: directory.path.clone(),
            stream_radius,
            // `--lod-fixture` implies both of these on the command line.
            lod_fixture: true,
            lod_enabled: true,
            ..default()
        }
    }

    fn move_camera_by(app: &mut App, x: i32, y: i32) {
        let mut camera = app
            .world_mut()
            .query_filtered::<&mut Transform, With<StreamingCamera>>()
            .single_mut(app.world_mut())
            .unwrap();
        camera.translation.x += x as f32 * CELL_SIZE;
        camera.translation.z -= y as f32 * CELL_SIZE;
    }

    /// With distant LOD off the engine must measure what upstream `main` did.
    ///
    /// The tier is not installed at all, so no LOD system can run, and the
    /// frame the commit budget describes is `collect_cells`' own window: the
    /// shared resource that a `--lod` run opens is never touched here. If the
    /// cell path ever stops accounting for itself the LOD-off numbers — and the
    /// `commit_frames > 0` streaming-fixture contract — would read as an engine
    /// that never commits.
    #[test]
    fn lod_off_keeps_upstreams_commit_window() {
        let directory = StreamingFixtureDirectory::create(0x3c, (0, 0), false).unwrap();
        let config = EngineConfig {
            assets_dir: directory.path.clone(),
            ..default()
        };
        assert!(!config.lod_enabled);
        let mut app = streaming_fixture_app(config);

        assert!(!app.is_plugin_added::<LodPlugin>());
        assert!(
            app.world().get_resource::<LodWorld>().is_none(),
            "an LOD world in the app means an LOD system can run"
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        while app.world().resource::<StreamingMetrics>().commit_frames == 0 {
            app.update();
            assert!(Instant::now() < deadline, "the cell tier never committed");
            std::thread::sleep(Duration::from_millis(2));
        }
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.commit_budget_micros, 16_670);
        assert!(metrics.max_frame_commit_micros > 0);
        assert_eq!(
            metrics.commit_budget_violations, 0,
            "a fixture cell commit took {} us",
            metrics.max_frame_commit_micros
        );
        assert_eq!(
            app.world().resource::<CommitBudget>().commits,
            0,
            "the cell tier must time itself, not the shared LOD window"
        );
    }

    #[test]
    fn lod_fixture_streams_blocks_across_band_boundaries() {
        let directory = StreamingFixtureDirectory::create(0x3c, (0, 0), true).unwrap();
        let mut app = lod_fixture_app(lod_fixture_config(&directory, 4));
        app.init_resource::<LodFixtureState>()
            .add_systems(PreUpdate, drive_lod_fixture)
            .add_systems(PostUpdate, validate_lod_fixture);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !app.world().resource::<LodFixtureState>().finished {
            app.update();
            assert!(Instant::now() < deadline, "the LOD fixture did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        let metrics = app.world().resource::<StreamingMetrics>();
        assert!(
            metrics.lod_fixture_validated,
            "fixture failures {}: {metrics:?}",
            metrics.lod_fixture_failures
        );
        assert!(metrics.lod_blocks_resident > 0);
        assert_eq!(metrics.lod_invariant_failures, 0);
        assert_eq!(metrics.lod_asset_failures, 0);
        // The tier registers the window's opening as well as its close, so a
        // committed block must still book the shared frame.
        assert!(metrics.commit_frames > 0);
    }

    #[test]
    fn lod_fixture_unloads_a_block_when_the_camera_leaves_its_band() {
        let directory = StreamingFixtureDirectory::create(0x3c, (0, 0), true).unwrap();
        let mut app = lod_fixture_app(lod_fixture_config(&directory, 4));
        settle_lod_tier(&mut app);
        let resident = app
            .world()
            .resource::<StreamingMetrics>()
            .lod_blocks_resident;
        assert!(resident > 0, "nothing streamed, so nothing could unload");
        move_camera_by(&mut app, 64, 0);
        for _ in 0..8 {
            app.update();
        }
        let metrics = app.world().resource::<StreamingMetrics>();
        assert!(metrics.lod_unloaded_blocks >= resident as u64);
        assert_eq!(metrics.lod_blocks_resident, 0);
        assert_eq!(metrics.lod_invariant_failures, 0);
    }

    #[test]
    fn lod_fixture_skips_the_blocks_the_full_detail_grid_covers() {
        let directory = StreamingFixtureDirectory::create(0x3c, (0, 0), true).unwrap();
        // A radius of four covers the four level-4 blocks around the origin and
        // nothing coarser.
        let mut app = lod_fixture_app(lod_fixture_config(&directory, 4));
        settle_lod_tier(&mut app);
        let metrics = app.world().resource::<StreamingMetrics>();
        assert!(metrics.lod_blocks_covered_skipped > 0);
        assert_eq!(metrics.lod_blocks_covered_skipped % 4, 0);
        assert_eq!(metrics.lod_invariant_failures, 0);
    }

    /// A block the tier cannot obtain is counted once and then left alone.
    ///
    /// A converted mesh that will not load and a `lod_block` row that has gone
    /// missing share this path and this counter; the row is what a test can
    /// drive deterministically, because a headless app never finishes an asset
    /// load (the loader's tasks stay `Loading`), so a deleted `.glb` is not
    /// observable here. The `--lod-fixture` app run covers the mesh case.
    #[test]
    fn a_lod_block_that_cannot_be_read_increments_lod_asset_failures_once() {
        let directory = StreamingFixtureDirectory::create(0x3c, (0, 0), true).unwrap();
        let mut app = lod_fixture_app(lod_fixture_config(&directory, 4));
        // The startup table has already listed the row; remove it underneath
        // the engine, so the request the planner is about to issue misses.
        let database = directory.path.join("skyrim_world.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "DELETE FROM lod_block WHERE level=?1 AND block_x=?2 AND block_y=?3",
                params![4, 4, 0],
            )
            .unwrap();
        drop(connection);
        for _ in 0..60 {
            app.update();
        }
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.lod_asset_failures, 1);
        assert_eq!(metrics.lod_blocks_failed, 1);
        assert_eq!(metrics.lod_blocks_loading, 0);
        assert_eq!(metrics.lod_invariant_failures, 0);
    }

    #[test]
    fn rebase_rewrites_a_lod_anchor_and_keeps_its_depth_offset() {
        let mut app = App::new();
        app.insert_resource(RenderOrigin(IVec2::ZERO))
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, update_render_origin);
        let camera = app
            .world_mut()
            .spawn((Transform::default(), StreamingCamera))
            .id();
        let root = app
            .world_mut()
            .spawn((
                LodBlockRoot {
                    kind: LodBlockKind::Terrain,
                    level: 8,
                    anchor: IVec2::new(12, -3),
                },
                Transform::from_translation(Vec3::new(0.0, -64.0, 0.0)),
            ))
            .id();
        {
            let mut entity = app.world_mut().entity_mut(camera);
            let mut transform = entity.get_mut::<Transform>().unwrap();
            transform.translation.x = 2.0 * CELL_SIZE + 12.0;
        }
        app.update();
        let origin = app.world().resource::<RenderOrigin>().0;
        let transform = app.world().entity(root).get::<Transform>().unwrap();
        assert_eq!(
            transform.translation,
            Vec3::new(
                (12 - origin.x) as f32 * CELL_SIZE,
                -64.0,
                -(-3 - origin.y) as f32 * CELL_SIZE,
            )
        );
        // The lowering must survive every rebase.
        assert_eq!(transform.translation.y, -64.0);
    }

    #[test]
    fn lod_lifecycle_validator_detects_duplicate_orphaned_and_misplaced_blocks() {
        let mut app = App::new();
        app.insert_resource(EngineConfig {
            lod_enabled: true,
            ..default()
        })
        .insert_resource(RenderOrigin(IVec2::ZERO))
        .init_resource::<LodWorld>()
        .init_resource::<StreamingMetrics>()
        .init_resource::<ProfilingState>()
        .add_systems(Update, validate_lod_lifecycle);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        let anchor = |level: u8, block_x: i32, block_y: i32, depth: f32| {
            (
                LodBlockRoot {
                    kind: LodBlockKind::Terrain,
                    level,
                    anchor: IVec2::new(block_x, block_y),
                },
                Transform::from_translation(block_translation(
                    IVec2::new(block_x, block_y),
                    IVec2::ZERO,
                    depth,
                )),
            )
        };
        let resident = app.world_mut().spawn(anchor(4, 0, 0, 32.0)).id();
        // The same block twice.
        app.world_mut().spawn(anchor(4, 0, 0, 32.0));
        // A root the residency map does not know.
        app.world_mut().spawn(anchor(8, 0, 0, 64.0));
        // A resident root placed somewhere else entirely.
        let misplaced = app
            .world_mut()
            .spawn((
                LodBlockRoot {
                    kind: LodBlockKind::Terrain,
                    level: 16,
                    anchor: IVec2::ZERO,
                },
                Transform::from_translation(Vec3::new(999.0, -96.0, 0.0)),
            ))
            .id();
        let world = &mut app.world_mut().resource_mut::<LodWorld>();
        world.blocks.insert(
            LodBlockKey {
                worldspace_id: 0x3c,
                kind: LodBlockKind::Terrain,
                level: 4,
                block_x: 0,
                block_y: 0,
            },
            LodBlockStatus::Resident { root: resident },
        );
        world.blocks.insert(
            LodBlockKey {
                worldspace_id: 0x3c,
                kind: LodBlockKind::Terrain,
                level: 16,
                block_x: 0,
                block_y: 0,
            },
            LodBlockStatus::Resident { root: misplaced },
        );
        app.update();
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.lod_duplicate_roots, 1);
        // The second root of the duplicated block, and the root the map does
        // not know, are both orphaned.
        assert_eq!(metrics.lod_orphaned_roots, 2);
        assert_eq!(metrics.lod_missing_roots, 0);
        assert_eq!(metrics.lod_misplaced_roots, 1);
        assert_eq!(metrics.lod_out_of_range_roots, 0);
        assert_eq!(metrics.lod_invariant_failures, 4);
    }

    #[test]
    fn the_camera_far_plane_covers_the_coarsest_band() {
        let without_lod = camera_far_plane(&EngineConfig::default());
        assert_eq!(without_lod, CELL_SIZE * (2 + 2) as f32 * 2.0);
        let config = EngineConfig {
            lod_enabled: true,
            ..default()
        };
        let coarsest = config
            .lod_bands
            .iter()
            .map(|band| band.distance)
            .fold(0.0f32, f32::max);
        let far = camera_far_plane(&config);
        // The plane has to clear the coarsest band *and* the diagonal of the
        // wide blocks admitted at its edge.
        assert!(far > coarsest + CELL_SIZE * 16.0 * std::f32::consts::SQRT_2);
    }

    /// The fixture GLB and PNG are written by hand, so their structure is
    /// checked directly: a headless app cannot complete an asset load, and a
    /// malformed fixture would otherwise only surface in a full app run.
    #[test]
    fn the_lod_fixture_assets_decode() {
        let directory = StreamingFixtureDirectory::create(0x3c, (0, 0), true).unwrap();
        let png = fs::read(
            directory
                .path
                .join("meshes/terrain/tamriel/lod-fixture.png"),
        )
        .unwrap();
        let image = Image::from_buffer(
            &png,
            bevy::image::ImageType::Extension("png"),
            bevy::image::CompressedImageFormats::NONE,
            true,
            bevy::image::ImageSampler::Default,
            RenderAssetUsages::default(),
        )
        .unwrap();
        assert_eq!((image.width(), image.height()), (1, 1));

        let glb = fs::read(
            directory
                .path
                .join("meshes/terrain/tamriel/tamriel.4.0.0.glb"),
        )
        .unwrap();
        assert_eq!(&glb[..4], b"glTF");
        let json_length = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
        assert_eq!(&glb[16..20], b"JSON");
        let document: serde_json::Value =
            serde_json::from_slice(&glb[20..20 + json_length]).unwrap();
        let bin_length = u32::from_le_bytes([
            glb[20 + json_length],
            glb[21 + json_length],
            glb[22 + json_length],
            glb[23 + json_length],
        ]) as usize;
        assert_eq!(&glb[24 + json_length..28 + json_length], b"BIN\0");
        assert_eq!(glb.len(), 28 + json_length + bin_length);
        assert_eq!(
            document["buffers"][0]["byteLength"].as_u64().unwrap() as usize,
            bin_length
        );
        for view in document["bufferViews"].as_array().unwrap() {
            let offset = view["byteOffset"].as_u64().unwrap() as usize;
            let length = view["byteLength"].as_u64().unwrap() as usize;
            assert!(
                offset + length <= bin_length,
                "bufferView escapes the BIN chunk"
            );
        }
        let attributes = &document["meshes"][0]["primitives"][0]["attributes"];
        assert_eq!(attributes["POSITION"], 0);
        assert_eq!(attributes["NORMAL"], 1);
        assert_eq!(attributes["TEXCOORD_0"], 2);
        assert_eq!(document["images"][0]["uri"], "lod-fixture.png");
        // The scene must hold bounds, or the loader produces an empty AABB.
        assert_eq!(
            document["accessors"][0]["min"],
            serde_json::json!([0.0, 0.0, 0.0])
        );
        assert_eq!(
            document["accessors"][0]["max"],
            serde_json::json!([1.0, 0.0, 1.0])
        );
    }

    #[test]
    fn automatic_flight_reverses_before_leaving_the_representative_world_area() {
        let mut state = AutoFlightState::default();
        let direction = bounded_auto_flight_direction(Vec3::new(0.0, -1.0, -1.0), 1.0, &mut state);
        assert_eq!(direction, Vec3::NEG_Z);

        assert_eq!(
            bounded_auto_flight_direction(Vec3::NEG_Z, AUTO_FLIGHT_HALF_SPAN, &mut state),
            Vec3::Z
        );
        assert_eq!(
            bounded_auto_flight_direction(Vec3::NEG_Z, 2.0, &mut state),
            Vec3::NEG_Z
        );
        assert!(state.offset.abs() <= AUTO_FLIGHT_HALF_SPAN);
    }

    #[test]
    fn rejects_stale_or_incomplete_runtime_assets() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            br#"{"schema_version":3,"complete":true}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                shared::WORLD_DATABASE_SCHEMA_VERSION
            ),
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        assert!(validate_runtime_assets(&config).is_err());
    }

    #[test]
    fn accepts_current_complete_runtime_assets() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            format!(
                r#"{{"schema_version":{},"complete":true}}"#,
                converter_schema_version()
            ),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                shared::WORLD_DATABASE_SCHEMA_VERSION
            ),
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        validate_runtime_assets(&config).unwrap();
    }

    #[test]
    fn rejects_truncated_manifest_and_integration_report() {
        for truncated_file in ["conversion-manifest.json", "integration-report.json"] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
            std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
            std::fs::write(
                directory.path().join("conversion-manifest.json"),
                format!(
                    r#"{{"schema_version":{},"complete":true}}"#,
                    converter_schema_version()
                ),
            )
            .unwrap();
            std::fs::write(
                directory.path().join("integration-report.json"),
                format!(
                    r#"{{"schema_version":{},"passed":true}}"#,
                    shared::WORLD_DATABASE_SCHEMA_VERSION
                ),
            )
            .unwrap();
            std::fs::write(directory.path().join(truncated_file), b"{").unwrap();
            let config = EngineConfig {
                assets_dir: directory.path().to_owned(),
                ..default()
            };
            assert!(
                validate_runtime_assets(&config).is_err(),
                "{truncated_file}"
            );
        }
    }
}
