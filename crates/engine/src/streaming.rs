use crate::{
    config::EngineConfig,
    doors::{DoorDestination, LoadDoor},
    profiling::ProfilingState,
    render::{
        TerrainExtension, TerrainMaterial, WaterExtension, WaterMaterial, WaterReflectionTexture,
    },
    world::{
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{
            CELL_SIZE, CellRef, ExpectedModelBounds, ExteriorCellGrid, FormId, InstanceBounds,
            MeshHandle, StreamedCellRoot, StreamingCamera, TerrainPatch, WaterSurface,
            WorldPosition, WorldTransform,
        },
        database::{
            AssetCatalog, CellKey, CellPayload, DatabaseRequest, ReferenceRow, WorldDatabase,
        },
    },
};
use bevy::{
    asset::{LoadState, RecursiveDependencyLoadState, RenderAssetUsages},
    camera::primitives::MeshAabb,
    gltf::GltfExtras,
    image::{ImageFilterMode, ImageLoaderSettings, ImageSampler},
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::time::Instant;

// Wall-clock spans can include a short OS scheduler preemption. Keep the raw maximum in metrics,
// but require a material overrun before classifying the frame as a commit-budget violation.
const COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS: u64 = 1_000;

fn commit_budget_exceeded(elapsed_micros: u64, budget_micros: u64) -> bool {
    elapsed_micros > budget_micros.saturating_add(COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS)
}

#[cfg(test)]
use bevy::mesh::VertexAttributeValues;

pub struct StreamingPlugin;

impl Plugin for StreamingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StreamingWorld>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<DiagnosticFallbackAssets>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<ActiveCell>()
            .init_resource::<PrestreamCells>()
            .add_observer(mark_world_instance_ready)
            .add_plugins(crate::transition::TransitionPlugin)
            .add_systems(
                Update,
                (
                    plan_cells,
                    collect_cells,
                    track_asset_readiness,
                    track_surface_readiness,
                    update_render_origin,
                    validate_streaming_lifecycle,
                )
                    .chain()
                    // The transition systems move the camera into its new cell and decide what a
                    // nearby door pre-streams, both of which the plan below has to see this frame.
                    .after(crate::transition::DoorTransition),
            );
    }
}

#[derive(Resource, Default)]
pub struct StreamingWorld {
    generation: u64,
    cells: HashMap<CellKey, CellStatus>,
}

impl StreamingWorld {
    /// Whether `key` is streamed in and its root entity spawned.
    pub fn is_resident(&self, key: &CellKey) -> bool {
        matches!(self.cells.get(key), Some(CellStatus::Resident { .. }))
    }
}

/// The cell the camera streams from: the exterior worldspace around it, or the interior it is
/// inside. While `interior` is `Some`, `worldspace_id` still names the worldspace the camera came
/// from and no exterior is streamed.
///
/// Started from [`EngineConfig::worldspace_id`], so a run that never crosses a load door behaves
/// exactly as it did before doors existed.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveCell {
    pub worldspace_id: u32,
    pub interior: Option<u32>,
}

impl FromWorld for ActiveCell {
    fn from_world(world: &mut World) -> Self {
        Self {
            worldspace_id: world.get_resource::<EngineConfig>().map_or_else(
                || EngineConfig::default().worldspace_id,
                |config| config.worldspace_id,
            ),
            interior: None,
        }
    }
}

/// Cells the transition layer wants streamed before the camera gets there: the destination behind
/// every load door the camera is close to. Rebuilt every frame and merged into the planner's
/// wanted set, so a crossing finds its destination already resident.
#[derive(Resource, Default, Debug, Clone)]
pub struct PrestreamCells {
    interiors: HashSet<u32>,
    exteriors: HashSet<(u32, i32, i32)>,
}

impl PrestreamCells {
    pub fn clear(&mut self) {
        self.interiors.clear();
        self.exteriors.clear();
    }

    pub fn request_interior(&mut self, cell_id: u32) {
        self.interiors.insert(cell_id);
    }

    pub fn request_exterior(&mut self, worldspace_id: u32, grid: IVec2) {
        self.exteriors.insert((worldspace_id, grid.x, grid.y));
    }

    pub fn contains(&self, key: &CellKey) -> bool {
        match *key {
            CellKey::Interior(cell_id) => self.interiors.contains(&cell_id),
            CellKey::Exterior {
                worldspace_id,
                grid_x,
                grid_y,
            } => self.exteriors.contains(&(worldspace_id, grid_x, grid_y)),
        }
    }

    pub fn extend_wanted(&self, wanted: &mut HashSet<CellKey>) {
        wanted.extend(
            self.interiors
                .iter()
                .map(|cell_id| CellKey::Interior(*cell_id)),
        );
        wanted.extend(
            self.exteriors
                .iter()
                .map(|&(worldspace_id, grid_x, grid_y)| CellKey::Exterior {
                    worldspace_id,
                    grid_x,
                    grid_y,
                }),
        );
    }
}

#[derive(Resource, Debug, Clone, Default, Serialize)]
pub struct StreamingMetrics {
    pub requests_submitted: u64,
    pub responses_received: u64,
    pub stale_responses: u64,
    pub failed_cells: u64,
    pub unloaded_cells: u64,
    pub resident_cells: usize,
    pub loading_cells: usize,
    pub peak_resident_cells: usize,
    pub peak_loading_cells: usize,
    pub total_query_micros: u64,
    pub max_query_micros: u64,
    pub max_commit_micros: u64,
    pub total_frame_commit_micros: u64,
    pub max_frame_commit_micros: u64,
    pub commit_frames: u64,
    pub commit_budget_micros: u64,
    pub commit_budget_violations: u64,
    pub total_queue_wait_micros: u64,
    pub max_queue_wait_micros: u64,
    pub total_request_micros: u64,
    pub max_request_micros: u64,
    pub total_rows_loaded: u64,
    pub assets_ready: u64,
    pub asset_load_failures: u64,
    pub max_asset_ready_micros: u64,
    pub pending_asset_instances: usize,
    pub pending_surface_instances: usize,
    pub meshes_validated: u64,
    pub materials_validated: u64,
    pub images_validated: u64,
    pub material_validation_failures: u64,
    pub diagnostic_fallbacks: u64,
    pub canonical_fixture_validated: bool,
    pub terrain_patches_validated: u64,
    pub terrain_seams_validated: u64,
    /// Edge points of an arriving cell whose height had to move onto a resident neighbour's by more
    /// than the edge tolerance, so the two meshes meet exactly instead of the cell being rejected
    /// (see [`validate_and_register_terrain_edges`]).
    pub terrain_seam_points_welded: u64,
    pub terrain_validation_failures: u64,
    pub water_surfaces_validated: u64,
    pub water_validation_failures: u64,
    pub terrain_water_fixture_validated: bool,
    pub transform_instances_validated: u64,
    pub transform_nodes_validated: u64,
    pub bounds_validated: u64,
    pub transform_bounds_validation_failures: u64,
    pub transform_bounds_fixture_validated: bool,
    pub active_requests: usize,
    pub peak_active_requests: usize,
    pub resident_roots: usize,
    pub duplicate_cell_roots: u64,
    pub orphaned_cell_roots: u64,
    pub missing_cell_roots: u64,
    pub out_of_range_cell_roots: u64,
    pub streaming_invariant_failures: u64,
    pub origin_rebases: u64,
    pub streaming_fixture_validated: bool,
    pub streaming_fixture_failures: u64,
    pub asset_failures: Vec<AssetFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetFailure {
    pub model_path: String,
    pub reference_form_id: u32,
    pub base_form_id: u32,
    pub cell_id: u32,
    pub dependency_chain: Vec<String>,
}

#[derive(Resource, Default)]
struct DiagnosticFallbackAssets {
    mesh: Option<Handle<Mesh>>,
    material: Option<Handle<StandardMaterial>>,
}

#[derive(Resource, Default)]
struct TerrainContinuity {
    edges: HashMap<CellKey, TerrainEdges>,
}

#[derive(Clone)]
struct TerrainEdges {
    west: Vec<f32>,
    east: Vec<f32>,
    south: Vec<f32>,
    north: Vec<f32>,
}

impl TerrainEdges {
    /// The four edges of the heights `terrain` currently holds. Registering them after welding
    /// records what was actually drawn, so the next cell welds onto the same surface.
    fn of(terrain: &TerrainSnapshot) -> Self {
        let samples = |side: TerrainEdgeSide| {
            side.points(terrain)
                .into_iter()
                .map(|index| terrain.heights[index])
                .collect()
        };
        Self {
            west: samples(TerrainEdgeSide::West),
            east: samples(TerrainEdgeSide::East),
            south: samples(TerrainEdgeSide::South),
            north: samples(TerrainEdgeSide::North),
        }
    }

    fn side(&self, side: TerrainEdgeSide) -> &[f32] {
        match side {
            TerrainEdgeSide::West => &self.west,
            TerrainEdgeSide::East => &self.east,
            TerrainEdgeSide::South => &self.south,
            TerrainEdgeSide::North => &self.north,
        }
    }
}

/// Which side of an exterior cell an edge belongs to, and which neighbour shares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerrainEdgeSide {
    West,
    East,
    South,
    North,
}

impl TerrainEdgeSide {
    /// Every side, in the order [`validate_and_register_terrain_edges`] welds them. A corner
    /// point belongs to two sides and to the neighbour of each, so the later side wins there.
    const ALL: [Self; 4] = [Self::West, Self::East, Self::South, Self::North];

    const fn opposite(self) -> Self {
        match self {
            Self::West => Self::East,
            Self::East => Self::West,
            Self::South => Self::North,
            Self::North => Self::South,
        }
    }

    /// The neighbour sharing this side in the same worldspace.
    fn neighbor_key(self, worldspace_id: u32, grid: IVec2) -> CellKey {
        let (grid_x, grid_y) = match self {
            Self::West => (grid.x - 1, grid.y),
            Self::East => (grid.x + 1, grid.y),
            Self::South => (grid.x, grid.y - 1),
            Self::North => (grid.x, grid.y + 1),
        };
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        }
    }

    /// The height-field points along this side. Both cells of a shared edge number their points
    /// from the same end, so a cell's side lines up index by index with the neighbour's opposite
    /// side.
    fn points(self, terrain: &TerrainSnapshot) -> Vec<usize> {
        let width = usize::from(terrain.width);
        let height = usize::from(terrain.height);
        match self {
            Self::West => (0..height).map(|row| row * width).collect(),
            Self::East => (0..height).map(|row| row * width + width - 1).collect(),
            Self::South => (0..width).collect(),
            Self::North => ((height - 1) * width..height * width).collect(),
        }
    }
}

enum CellStatus {
    Loading { generation: u64 },
    Resident { root: Entity },
    Failed,
}

#[derive(Resource, Debug, Clone, Copy)]
pub struct RenderOrigin(pub IVec2);

#[allow(clippy::too_many_arguments)]
fn plan_cells(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    active: Res<ActiveCell>,
    prestream: Res<PrestreamCells>,
    origin: Res<RenderOrigin>,
    camera: Query<&Transform, With<StreamingCamera>>,
    mut streaming: ResMut<StreamingWorld>,
    mut continuity: ResMut<TerrainContinuity>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let plan_started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    let global_x = camera.translation.x + origin.0.x as f32 * CELL_SIZE;
    let global_y = -camera.translation.z + origin.0.y as f32 * CELL_SIZE;
    let center = IVec2::new(
        (global_x / CELL_SIZE).floor() as i32,
        (global_y / CELL_SIZE).floor() as i32,
    );
    let wanted = wanted_cells(&active, config.stream_radius, center, &prestream);
    for key in &wanted {
        if !streaming.cells.contains_key(key) {
            streaming.generation = streaming.generation.wrapping_add(1);
            let generation = streaming.generation;
            if database
                .request(DatabaseRequest::Load {
                    generation,
                    key: *key,
                    queued_at: Instant::now(),
                })
                .is_ok()
            {
                metrics.requests_submitted += 1;
                profiler.increment("streaming/requests", 1);
                profiler.event(format!("{key:?}"), "requested", None);
                streaming
                    .cells
                    .insert(*key, CellStatus::Loading { generation });
            }
        }
    }
    streaming.cells.retain(|key, status| {
        let keep =
            cell_within_unload_radius(*key, &active, center, config.unload_radius, &prestream);
        if !keep {
            metrics.unloaded_cells += 1;
            continuity.edges.remove(key);
            profiler.event(format!("{key:?}"), "unloaded", None);
            if let CellStatus::Resident { root } = status {
                commands.entity(*root).despawn();
            }
        }
        keep
    });
    metrics.resident_cells = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Resident { .. }))
        .count();
    metrics.loading_cells = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Loading { .. }))
        .count();
    metrics.peak_resident_cells = metrics.peak_resident_cells.max(metrics.resident_cells);
    metrics.peak_loading_cells = metrics.peak_loading_cells.max(metrics.loading_cells);
    profiler.set_gauge("streaming/resident_cells", metrics.resident_cells as f64);
    profiler.set_gauge("streaming/loading_cells", metrics.loading_cells as f64);
    profiler.record_elapsed("streaming/plan_cells", plan_started);
}

#[allow(clippy::too_many_arguments)]
fn collect_cells(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    cache: Res<CellCache>,
    origin: Res<RenderOrigin>,
    asset_server: Res<AssetServer>,
    catalog: Res<AssetCatalog>,
    reflection: Res<WaterReflectionTexture>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
    mut streaming: ResMut<StreamingWorld>,
    mut continuity: ResMut<TerrainContinuity>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let frame_commit_started = Instant::now();
    let mut commits_this_frame = 0u64;
    for _ in 0..config.max_cell_commits_per_frame {
        let Some(response) = database.try_response() else {
            break;
        };
        metrics.responses_received += 1;
        metrics.total_query_micros = metrics
            .total_query_micros
            .saturating_add(response.query_micros);
        metrics.max_query_micros = metrics.max_query_micros.max(response.query_micros);
        metrics.total_queue_wait_micros = metrics
            .total_queue_wait_micros
            .saturating_add(response.queue_wait_micros);
        metrics.max_queue_wait_micros = metrics
            .max_queue_wait_micros
            .max(response.queue_wait_micros);
        metrics.total_request_micros = metrics
            .total_request_micros
            .saturating_add(response.total_request_micros);
        metrics.max_request_micros = metrics
            .max_request_micros
            .max(response.total_request_micros);
        metrics.total_rows_loaded = metrics
            .total_rows_loaded
            .saturating_add(response.row_count as u64);
        profiler.record_micros("streaming/db_queue_wait", response.queue_wait_micros);
        profiler.record_micros("streaming/db_query", response.query_micros);
        profiler.record_micros("streaming/db_request_total", response.total_request_micros);
        let Some(CellStatus::Loading { generation }) = streaming.cells.get(&response.key) else {
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_discarded", None);
            continue;
        };
        if *generation != response.generation {
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_generation", None);
            continue;
        }
        let commit_started = std::time::Instant::now();
        match response.result {
            Ok(payload) => {
                let mut terrain = cache.terrain(payload.cell_id);
                if let Some(terrain) = terrain.as_mut() {
                    let validation = validate_terrain_snapshot(terrain, &catalog).and_then(|()| {
                        validate_and_register_terrain_edges(
                            payload.key,
                            terrain,
                            &mut continuity,
                            &mut metrics,
                        )
                    });
                    if let Err(reason) = validation {
                        error!(cell = format_args!("{:08X}", payload.cell_id), %reason, "LAND failed strict validation");
                        metrics.failed_cells = metrics.failed_cells.saturating_add(1);
                        metrics.terrain_validation_failures =
                            metrics.terrain_validation_failures.saturating_add(1);
                        metrics.asset_failures.push(AssetFailure {
                            model_path: format!("terrain/{:08X}", payload.cell_id),
                            reference_form_id: 0,
                            base_form_id: 0,
                            cell_id: payload.cell_id,
                            dependency_chain: vec![reason],
                        });
                        profiler.increment("terrain/validation_failures", 1);
                        streaming.cells.insert(response.key, CellStatus::Failed);
                        continue;
                    }
                }
                let root = spawn_cell(
                    &mut commands,
                    &asset_server,
                    &catalog,
                    &reflection,
                    &mut meshes,
                    &mut terrain_materials,
                    &mut water_materials,
                    origin.0,
                    payload,
                    terrain,
                    &mut profiler,
                );
                streaming
                    .cells
                    .insert(response.key, CellStatus::Resident { root });
            }
            Err(error) => {
                debug!(?response.key, %error, "cell could not be streamed");
                streaming.cells.insert(response.key, CellStatus::Failed);
                metrics.failed_cells += 1;
                profiler.increment("streaming/failed_cells", 1);
            }
        }
        let commit_micros = commit_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        metrics.max_commit_micros = metrics.max_commit_micros.max(commit_micros);
        commits_this_frame = commits_this_frame.saturating_add(1);
        profiler.record_micros("streaming/cell_commit", commit_micros);
        profiler.event(
            format!("{:?}", response.key),
            "committed",
            Some(commit_micros as f64 / 1000.0),
        );
    }
    if commits_this_frame > 0 {
        let frame_micros = frame_commit_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        metrics.commit_frames = metrics.commit_frames.saturating_add(1);
        metrics.total_frame_commit_micros = metrics
            .total_frame_commit_micros
            .saturating_add(frame_micros);
        metrics.max_frame_commit_micros = metrics.max_frame_commit_micros.max(frame_micros);
        metrics.commit_budget_micros = config.max_commit_micros_per_frame;
        if commit_budget_exceeded(frame_micros, config.max_commit_micros_per_frame) {
            metrics.commit_budget_violations = metrics.commit_budget_violations.saturating_add(1);
            profiler.event(
                "streaming",
                "commit_budget_exceeded",
                Some(frame_micros as f64 / 1_000.0),
            );
        }
        profiler.set_gauge("streaming/commits_this_frame", commits_this_frame as f64);
        profiler.record_micros("streaming/frame_commit", frame_micros);
    }
}

/// The cells the active cell wants resident: the interior the camera is in, or the exterior grid
/// of `stream_radius` around it — plus everything the transition layer pre-streamed for a nearby
/// load door, which is what makes a crossing seamless.
fn wanted_cells(
    active: &ActiveCell,
    stream_radius: i32,
    center: IVec2,
    prestream: &PrestreamCells,
) -> HashSet<CellKey> {
    let mut wanted = HashSet::new();
    match active.interior {
        Some(cell_id) => {
            wanted.insert(CellKey::Interior(cell_id));
        }
        None => {
            for y in -stream_radius..=stream_radius {
                for x in -stream_radius..=stream_radius {
                    wanted.insert(CellKey::Exterior {
                        worldspace_id: active.worldspace_id,
                        grid_x: center.x + x,
                        grid_y: center.y + y,
                    });
                }
            }
        }
    }
    prestream.extend_wanted(&mut wanted);
    wanted
}

/// Whether a streamed cell survives this frame: a cell the transition layer pre-streamed, the
/// active interior, or an exterior of the active worldspace within `radius` of the camera.
///
/// So an interior unloads as soon as it is neither active nor pre-streamed, no exterior survives
/// while an interior is active, and a crossing into another worldspace drops the one just left
/// instead of holding its cells at whatever grid the destination happens to share.
fn cell_within_unload_radius(
    key: CellKey,
    active: &ActiveCell,
    center: IVec2,
    radius: i32,
    prestream: &PrestreamCells,
) -> bool {
    if prestream.contains(&key) {
        return true;
    }
    match (key, active.interior) {
        (CellKey::Interior(cell_id), Some(active_interior)) => cell_id == active_interior,
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
            worldspace_id == active.worldspace_id
                && (grid_x - center.x).abs() <= radius
                && (grid_y - center.y).abs() <= radius
        }
    }
}

/// Whether a water plane at `water_height` would cover any of this cell's ground, which is what
/// decides whether the cell gets one. A cell whose own height samples all stay at or above the
/// water level is dry: the plane would sit exactly on top of the terrain where they are equal and
/// within a few units of it where they are close, which is inside the depth precision of a
/// distant camera, so the two surfaces z-fight and the plane's cell-aligned edges read as a grid.
/// A cell with one sample below the level still gets its plane - clipping it to the shoreline is
/// a separate change.
fn terrain_reaches_water(terrain: &TerrainSnapshot, water_height: f32) -> bool {
    terrain.heights.iter().any(|height| *height < water_height)
}

#[allow(clippy::too_many_arguments)]
fn spawn_cell(
    commands: &mut Commands,
    asset_server: &AssetServer,
    catalog: &AssetCatalog,
    reflection: &WaterReflectionTexture,
    meshes: &mut Assets<Mesh>,
    terrain_materials: &mut Assets<TerrainMaterial>,
    water_materials: &mut Assets<WaterMaterial>,
    origin: IVec2,
    payload: CellPayload,
    terrain: Option<TerrainSnapshot>,
    profiler: &mut ProfilingState,
) -> Entity {
    let spawn_started = Instant::now();
    let reference_count = payload.references.len();
    let root_translation = cell_translation(payload.key, origin);
    let mut root_commands = commands.spawn((
        Name::new(format!("Cell {:08X}", payload.cell_id)),
        CellRef(payload.cell_id),
        StreamedCellRoot,
        Transform::from_translation(root_translation),
        Visibility::default(),
    ));
    if let CellKey::Exterior { grid_x, grid_y, .. } = payload.key {
        root_commands.insert(ExteriorCellGrid(IVec2::new(grid_x, grid_y)));
    }
    let root = root_commands.id();
    commands.entity(root).with_children(|parent| {
        if let Some(terrain) = terrain {
            for quadrant in 0..4 {
                let started = Instant::now();
                let mesh = build_terrain_quadrant_mesh(&terrain, quadrant)
                    .expect("validated terrain must build");
                profiler.record_elapsed("streaming/terrain_mesh", started);
                let (extension, images) =
                    TerrainExtension::from_quadrant(&terrain, quadrant, catalog, asset_server)
                        .expect("validated terrain material must build");
                let material = terrain_materials.add(TerrainMaterial {
                    base: StandardMaterial {
                        base_color: Color::WHITE,
                        perceptual_roughness: 0.92,
                        cull_mode: None,
                        double_sided: true,
                        ..default()
                    },
                    extension,
                });
                parent.spawn((
                    Name::new(format!("Terrain quadrant {quadrant}")),
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(material),
                    Transform::default(),
                    TerrainPatch,
                    Visibility::Hidden,
                    PendingTerrainProfile {
                        cell_id: terrain.cell_id,
                        quadrant,
                        images,
                    },
                ));
            }
            if let Some(height) = terrain
                .water_height
                .filter(|height| height.is_finite() && height.abs() < 1.0e7)
                // A cell whose own samples stay at or above the water level has no ground for the
                // plane to cover: drawing the full 4096x4096 quad there puts a second surface on
                // top of the terrain, whose cell-aligned edges then show through it as grid lines.
                .filter(|height| terrain_reaches_water(&terrain, *height))
            {
                let water_mesh = meshes.add(Plane3d::default().mesh().size(CELL_SIZE, CELL_SIZE));
                let flow_normal = terrain
                    .water_type_form_id
                    .and_then(|form_id| catalog.water_flow(form_id))
                    .map(|path| {
                        asset_server
                            .load_builder()
                            .with_settings(|settings: &mut ImageLoaderSettings| {
                                settings.is_srgb = false;
                            })
                            .load(path.to_owned())
                    });
                let water_material = water_materials.add(WaterMaterial {
                    base: StandardMaterial {
                        base_color: Color::srgba(0.05, 0.2, 0.32, 0.68),
                        metallic: 0.15,
                        perceptual_roughness: 0.06,
                        reflectance: 0.9,
                        alpha_mode: AlphaMode::Blend,
                        ..default()
                    },
                    extension: WaterExtension::with_reflection(
                        reflection.0.clone(),
                        flow_normal.clone(),
                    ),
                });
                parent.spawn((
                    Name::new("Water"),
                    Mesh3d(water_mesh),
                    MeshMaterial3d(water_material),
                    Transform::from_translation(Vec3::new(
                        CELL_SIZE * 0.5,
                        height,
                        -CELL_SIZE * 0.5,
                    )),
                    WaterSurface,
                    Visibility::Hidden,
                    PendingWaterProfile {
                        cell_id: terrain.cell_id,
                        flow_normal,
                    },
                    bevy::camera::visibility::RenderLayers::layer(1),
                ));
            }
        }
        for reference in payload.references {
            let creation_position = Vec3::from_array(reference.position);
            let world_position = WorldPosition::from_creation_units(creation_position);
            let translation = match payload.key {
                CellKey::Exterior { grid_x, grid_y, .. } => {
                    let cell_origin = IVec2::new(grid_x, grid_y);
                    creation_to_bevy(world_position.relative_to(cell_origin))
                }
                CellKey::Interior(_) => creation_to_bevy(creation_position),
            };
            let rotation = creation_rotation_to_bevy(reference.rotation);
            let transform = Transform::from_translation(translation)
                .with_rotation(rotation)
                .with_scale(Vec3::splat(reference.scale));
            let model_bounds = reference.bounds_valid.then(|| {
                ExpectedModelBounds::new(
                    Vec3::from_array(reference.bounds_min),
                    Vec3::from_array(reference.bounds_max),
                )
            });
            let model_bounds = model_bounds.flatten();
            let bounds = model_bounds.map(|bounds| {
                InstanceBounds::transformed(bounds.min, bounds.max, transform.to_matrix())
            });
            let mut entity = parent.spawn((
                Name::new(format!("Reference {:08X}", reference.form_id)),
                FormId(reference.form_id),
                CellRef(reference.cell_id),
                world_position,
                WorldTransform(transform.to_matrix()),
                transform,
            ));
            if let Some(bounds) = bounds.zip(model_bounds) {
                entity.insert(bounds);
            }
            if let Some(door) = load_door(&reference) {
                entity.insert(door);
            }
            if let Some(path) = reference.model_path.and_then(converted_model_path) {
                entity.insert((
                    MeshHandle(path.clone()),
                    WorldAssetRoot(
                        asset_server.load(GltfAssetLabel::Scene(0).from_asset(path.clone())),
                    ),
                    PendingAssetProfile {
                        started: Instant::now(),
                        scene_spawned: false,
                        path,
                        form_id: reference.form_id,
                        base_form_id: reference.base_form_id,
                        cell_id: reference.cell_id,
                    },
                ));
            }
        }
    });
    profiler.increment("streaming/references_spawned", reference_count as u64);
    profiler.record_elapsed("streaming/spawn_cell", spawn_started);
    root
}

/// The [`LoadDoor`] a reference spawns with, or `None` for an ordinary reference and for a door
/// link the converter could not resolve to a cell.
///
/// The destination's cell decides which kind of destination this is: a `worldspace_id` names an
/// exterior, and its absence makes the cell an interior.
fn load_door(reference: &ReferenceRow) -> Option<LoadDoor> {
    let door = reference.door.as_ref()?;
    let interior_cell_id = match (door.destination_worldspace_id, door.destination_cell_id) {
        (None, Some(cell_id)) if cell_id != 0 => Some(cell_id),
        _ => None,
    };
    let destination = DoorDestination {
        destination_ref_id: door.destination_ref_id,
        interior_cell_id,
        worldspace_id: door.destination_worldspace_id,
        arrival_position: door.arrival_position,
        arrival_rotation: door.arrival_rotation,
    };
    if destination.interior_cell_id.is_none() && destination.worldspace_id.is_none() {
        return None;
    }
    Some(LoadDoor {
        ref_id: reference.form_id,
        destination,
        label: door.label.clone(),
    })
}

#[derive(Component)]
struct PendingAssetProfile {
    started: Instant,
    scene_spawned: bool,
    path: String,
    form_id: u32,
    base_form_id: u32,
    cell_id: u32,
}

#[derive(Component)]
struct PendingTerrainProfile {
    cell_id: u32,
    quadrant: u8,
    images: Vec<Handle<Image>>,
}

#[derive(Component)]
struct PendingWaterProfile {
    cell_id: u32,
    flow_normal: Option<Handle<Image>>,
}

type RenderPrimitiveQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        &'static Mesh3d,
        Option<&'static MeshMaterial3d<StandardMaterial>>,
        Option<&'static GltfExtras>,
    ),
>;

type PendingAssetQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static WorldAssetRoot,
        &'static PendingAssetProfile,
        &'static Transform,
        &'static GlobalTransform,
        &'static WorldTransform,
        Option<&'static ExpectedModelBounds>,
    ),
>;

fn mark_world_instance_ready(
    ready: On<WorldInstanceReady>,
    mut pending: Query<&mut PendingAssetProfile>,
) {
    if let Ok(mut pending) = pending.get_mut(ready.entity) {
        pending.scene_spawned = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn track_asset_readiness(
    mut commands: Commands,
    config: Res<EngineConfig>,
    asset_server: Res<AssetServer>,
    pending: PendingAssetQuery,
    children: Query<&Children>,
    primitives: RenderPrimitiveQuery,
    transforms: Query<(&Transform, &GlobalTransform)>,
    images: Res<Assets<Image>>,
    mut fallback_assets: ResMut<DiagnosticFallbackAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    metrics.pending_asset_instances = pending.iter().count();
    let mut completed_this_scan = 0usize;
    for (entity, root, pending, local, global, world_transform, expected_bounds) in &pending {
        let load_failure =
            asset_server
                .get_load_states(root.0.id())
                .and_then(|(load, _, recursive)| match (load, recursive) {
                    (LoadState::Failed(error), _) => Some(error),
                    (_, RecursiveDependencyLoadState::Failed(error)) => Some(error),
                    _ => None,
                });
        if let Some(error) = load_failure {
            let chain = error_chain(error.as_ref());
            record_asset_failure(&mut metrics, &mut profiler, pending, chain, false);
            hide_partial_scene(&mut commands, entity, &children);
            if config.diagnostic_asset_fallbacks {
                spawn_diagnostic_fallback(
                    &mut commands,
                    entity,
                    &mut fallback_assets,
                    &mut meshes,
                    &mut materials,
                );
                metrics.diagnostic_fallbacks = metrics.diagnostic_fallbacks.saturating_add(1);
            }
            commands.entity(entity).remove::<PendingAssetProfile>();
            completed_this_scan += 1;
        } else if pending.scene_spawned && asset_server.is_loaded_with_dependencies(root.0.id()) {
            let transform_validation = validate_spawned_transforms_and_bounds(
                entity,
                local,
                global,
                world_transform,
                expected_bounds,
                &children,
                &transforms,
                &primitives,
                &meshes,
            );
            let transform_summary = match transform_validation {
                Ok(summary) => summary,
                Err(reason) => {
                    metrics.transform_bounds_validation_failures = metrics
                        .transform_bounds_validation_failures
                        .saturating_add(1);
                    profiler.increment("transforms/validation_failures", 1);
                    record_asset_failure(&mut metrics, &mut profiler, pending, vec![reason], false);
                    hide_partial_scene(&mut commands, entity, &children);
                    if config.diagnostic_asset_fallbacks {
                        spawn_diagnostic_fallback(
                            &mut commands,
                            entity,
                            &mut fallback_assets,
                            &mut meshes,
                            &mut materials,
                        );
                        metrics.diagnostic_fallbacks =
                            metrics.diagnostic_fallbacks.saturating_add(1);
                    }
                    commands.entity(entity).remove::<PendingAssetProfile>();
                    completed_this_scan += 1;
                    continue;
                }
            };
            let validation = validate_spawned_asset(
                entity,
                &children,
                &primitives,
                &meshes,
                &materials,
                &images,
            );
            let summary = match validation {
                Ok(summary) => summary,
                Err(reason) => {
                    record_asset_failure(&mut metrics, &mut profiler, pending, vec![reason], true);
                    hide_partial_scene(&mut commands, entity, &children);
                    if config.diagnostic_asset_fallbacks {
                        spawn_diagnostic_fallback(
                            &mut commands,
                            entity,
                            &mut fallback_assets,
                            &mut meshes,
                            &mut materials,
                        );
                        metrics.diagnostic_fallbacks =
                            metrics.diagnostic_fallbacks.saturating_add(1);
                    }
                    commands.entity(entity).remove::<PendingAssetProfile>();
                    completed_this_scan += 1;
                    continue;
                }
            };
            let micros = pending
                .started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            metrics.assets_ready = metrics.assets_ready.saturating_add(1);
            metrics.meshes_validated = metrics
                .meshes_validated
                .saturating_add(summary.meshes as u64);
            metrics.materials_validated = metrics
                .materials_validated
                .saturating_add(summary.materials as u64);
            metrics.images_validated = metrics
                .images_validated
                .saturating_add(summary.images as u64);
            metrics.transform_instances_validated =
                metrics.transform_instances_validated.saturating_add(1);
            metrics.transform_nodes_validated = metrics
                .transform_nodes_validated
                .saturating_add(transform_summary.nodes as u64);
            metrics.bounds_validated = metrics.bounds_validated.saturating_add(1);
            metrics.max_asset_ready_micros = metrics.max_asset_ready_micros.max(micros);
            profiler.record_micros("assets/model_ready", micros);
            profiler.event(&pending.path, "asset_ready", Some(micros as f64 / 1000.0));
            commands.entity(entity).remove::<PendingAssetProfile>();
            completed_this_scan += 1;
        }
    }
    metrics.pending_asset_instances = metrics
        .pending_asset_instances
        .saturating_sub(completed_this_scan);
    profiler.set_gauge(
        "assets/pending_instances",
        metrics.pending_asset_instances as f64,
    );
    profiler.record_elapsed("assets/readiness_scan", started);
}

fn track_surface_readiness(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    images: Res<Assets<Image>>,
    terrain: Query<(Entity, &PendingTerrainProfile)>,
    water: Query<(Entity, &PendingWaterProfile)>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    metrics.pending_surface_instances = terrain.iter().count() + water.iter().count();
    let mut completed = 0usize;
    for (entity, pending) in &terrain {
        match validate_surface_dependencies(&asset_server, &images, &pending.images, true) {
            SurfaceDependencyState::Pending => {}
            SurfaceDependencyState::Ready => {
                metrics.terrain_patches_validated =
                    metrics.terrain_patches_validated.saturating_add(1);
                metrics.materials_validated = metrics.materials_validated.saturating_add(1);
                metrics.images_validated = metrics
                    .images_validated
                    .saturating_add(pending.images.len() as u64);
                profiler.increment("terrain/patches_validated", 1);
                commands.entity(entity).insert(Visibility::Inherited);
                commands.entity(entity).remove::<PendingTerrainProfile>();
                completed += 1;
            }
            SurfaceDependencyState::Failed(reason) => {
                metrics.asset_load_failures = metrics.asset_load_failures.saturating_add(1);
                metrics.terrain_validation_failures =
                    metrics.terrain_validation_failures.saturating_add(1);
                metrics.asset_failures.push(AssetFailure {
                    model_path: format!(
                        "terrain/{:08X}/quadrant-{}",
                        pending.cell_id, pending.quadrant
                    ),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: pending.cell_id,
                    dependency_chain: vec![reason],
                });
                profiler.increment("terrain/validation_failures", 1);
                commands.entity(entity).insert(Visibility::Hidden);
                commands.entity(entity).remove::<PendingTerrainProfile>();
                completed += 1;
            }
        }
    }
    for (entity, pending) in &water {
        let handles: Vec<_> = pending.flow_normal.iter().cloned().collect();
        match validate_surface_dependencies(&asset_server, &images, &handles, false) {
            SurfaceDependencyState::Pending => {}
            SurfaceDependencyState::Ready => {
                metrics.water_surfaces_validated =
                    metrics.water_surfaces_validated.saturating_add(1);
                metrics.materials_validated = metrics.materials_validated.saturating_add(1);
                metrics.images_validated = metrics
                    .images_validated
                    .saturating_add(handles.len() as u64);
                profiler.increment("water/surfaces_validated", 1);
                commands.entity(entity).insert(Visibility::Inherited);
                commands.entity(entity).remove::<PendingWaterProfile>();
                completed += 1;
            }
            SurfaceDependencyState::Failed(reason) => {
                metrics.asset_load_failures = metrics.asset_load_failures.saturating_add(1);
                metrics.water_validation_failures =
                    metrics.water_validation_failures.saturating_add(1);
                metrics.asset_failures.push(AssetFailure {
                    model_path: format!("water/{:08X}", pending.cell_id),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: pending.cell_id,
                    dependency_chain: vec![reason],
                });
                profiler.increment("water/validation_failures", 1);
                commands.entity(entity).insert(Visibility::Hidden);
                commands.entity(entity).remove::<PendingWaterProfile>();
                completed += 1;
            }
        }
    }
    metrics.pending_surface_instances = metrics.pending_surface_instances.saturating_sub(completed);
    profiler.set_gauge(
        "assets/pending_surface_instances",
        metrics.pending_surface_instances as f64,
    );
}

enum SurfaceDependencyState {
    Pending,
    Ready,
    Failed(String),
}

fn validate_surface_dependencies(
    asset_server: &AssetServer,
    images: &Assets<Image>,
    handles: &[Handle<Image>],
    expects_srgb: bool,
) -> SurfaceDependencyState {
    for handle in handles {
        if let Some((load, _, recursive)) = asset_server.get_load_states(handle.id()) {
            let failed = match (load, recursive) {
                (LoadState::Failed(error), _) => Some(error),
                (_, RecursiveDependencyLoadState::Failed(error)) => Some(error),
                _ => None,
            };
            if let Some(error) = failed {
                return SurfaceDependencyState::Failed(error_chain(error.as_ref()).join(" -> "));
            }
        }
        if !asset_server.is_loaded_with_dependencies(handle.id()) {
            return SurfaceDependencyState::Pending;
        }
        let Some(image) = images.get(handle) else {
            return SurfaceDependencyState::Pending;
        };
        if image.texture_descriptor.format.is_srgb() != expects_srgb {
            return SurfaceDependencyState::Failed(format!(
                "image {:?} has wrong color space {:?}",
                handle.id(),
                image.texture_descriptor.format
            ));
        }
        if let Err(reason) = validate_image_sampler("surface", &image.sampler) {
            return SurfaceDependencyState::Failed(reason);
        }
    }
    SurfaceDependencyState::Ready
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AssetValidationSummary {
    meshes: usize,
    materials: usize,
    images: usize,
    excluded_materials: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TransformValidationSummary {
    nodes: usize,
}

#[allow(clippy::too_many_arguments)]
fn validate_spawned_transforms_and_bounds(
    root: Entity,
    root_local: &Transform,
    root_global: &GlobalTransform,
    world_transform: &WorldTransform,
    expected: Option<&ExpectedModelBounds>,
    children: &Query<&Children>,
    transforms: &Query<(&Transform, &GlobalTransform)>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
) -> Result<TransformValidationSummary, String> {
    validate_transform("reference", root_local, root_global)?;
    let local_matrix = root_local.to_matrix();
    if matrix_max_difference(local_matrix, world_transform.0) > 1.0e-4 {
        return Err("WorldTransform differs from the spawned reference Transform".to_owned());
    }
    let expected = expected.ok_or_else(|| {
        "converted model has no validated aggregate bounds; reconvert the asset".to_owned()
    })?;
    ExpectedModelBounds::new(expected.min, expected.max)
        .ok_or_else(|| "converted model bounds are non-finite, empty, or inverted".to_owned())?;

    let spawned = spawned_relative_bounds(root, children, transforms, primitives, meshes)?;
    let extent = (expected.max - expected.min).abs().max_element().max(1.0);
    let tolerance = (extent * 1.0e-4).max(1.0e-3);
    let error = (spawned.min - expected.min)
        .abs()
        .max((spawned.max - expected.max).abs())
        .max_element();
    if !error.is_finite() || error > tolerance {
        return Err(format!(
            "spawned hierarchy bounds diverge from conversion: expected {:?}..{:?}, actual {:?}..{:?}, tolerance {tolerance}",
            expected.min, expected.max, spawned.min, spawned.max
        ));
    }
    Ok(TransformValidationSummary {
        nodes: spawned.nodes,
    })
}

/// Aggregate bounds of the spawned hierarchy's meshes, measured in model space: every
/// node's transform relative to `root` is composed from the local `Transform`s along its
/// parent chain, so the result does not depend on where the reference sits in the world.
///
/// `root_global.affine().inverse() * global.affine()` is the same matrix in exact
/// arithmetic, but evaluating it in world space subtracts two translations the size of the
/// reference's distance from the render origin in f32. That cancels catastrophically - the
/// error grows with the distance and can exceed the caller's tolerance for a model the
/// converter produced correctly - whereas the composed local product stays at model-space
/// magnitudes.
fn spawned_relative_bounds(
    root: Entity,
    children: &Query<&Children>,
    transforms: &Query<(&Transform, &GlobalTransform)>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
) -> Result<SpawnedBounds, String> {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    let mut nodes = 0usize;
    let mut bounded_meshes = 0usize;
    let mut stack: Vec<(Entity, Mat4)> = Vec::new();
    if let Ok(root_children) = children.get(root) {
        stack.extend(root_children.iter().map(|child| (child, Mat4::IDENTITY)));
    }
    while let Some((descendant, relative)) = stack.pop() {
        let (local, global) = transforms
            .get(descendant)
            .map_err(|_| format!("hierarchy node {descendant:?} has no local/global transform"))?;
        validate_transform(&format!("hierarchy node {descendant:?}"), local, global)?;
        nodes += 1;
        let relative = relative * local.to_matrix();
        if let Ok(grandchildren) = children.get(descendant) {
            stack.extend(grandchildren.iter().map(|child| (child, relative)));
        }
        let Ok((mesh_handle, _, _)) = primitives.get(descendant) else {
            continue;
        };
        let mesh = meshes.get(mesh_handle).ok_or_else(|| {
            format!(
                "mesh {:?} is absent while validating bounds",
                mesh_handle.id()
            )
        })?;
        let aabb = mesh
            .compute_aabb()
            .ok_or_else(|| format!("mesh {:?} has no finite POSITION bounds", mesh_handle.id()))?;
        let center = Vec3::from(aabb.center);
        let half_extents = Vec3::from(aabb.half_extents);
        let transformed =
            InstanceBounds::transformed(center - half_extents, center + half_extents, relative);
        min = min.min(transformed.min);
        max = max.max(transformed.max);
        bounded_meshes += 1;
    }
    if bounded_meshes == 0 {
        return Err("spawned hierarchy contains no bounded mesh".to_owned());
    }
    Ok(SpawnedBounds { min, max, nodes })
}

#[derive(Debug, Clone, Copy)]
struct SpawnedBounds {
    min: Vec3,
    max: Vec3,
    /// Hierarchy nodes visited, which the validation summary reports to the metrics.
    nodes: usize,
}

fn validate_transform(
    label: &str,
    local: &Transform,
    global: &GlobalTransform,
) -> Result<(), String> {
    let local_matrix = local.to_matrix();
    let global_matrix = global.to_matrix();
    if !local_matrix.is_finite() || !global_matrix.is_finite() {
        return Err(format!("{label} contains a non-finite transform"));
    }
    if local.scale.abs().min_element() <= 1.0e-6
        || local_matrix.determinant().abs() <= 1.0e-8
        || global_matrix.determinant().abs() <= 1.0e-8
    {
        return Err(format!(
            "{label} contains a singular scale or hierarchy transform"
        ));
    }
    let rotation_length = local.rotation.length();
    if !rotation_length.is_finite() || (rotation_length - 1.0).abs() > 1.0e-3 {
        return Err(format!("{label} contains a non-normalized rotation"));
    }
    Ok(())
}

fn matrix_max_difference(left: Mat4, right: Mat4) -> f32 {
    left.to_cols_array()
        .into_iter()
        .zip(right.to_cols_array())
        .map(|(left, right)| (left - right).abs())
        .fold(0.0, f32::max)
}

fn validate_spawned_asset(
    root: Entity,
    children: &Query<&Children>,
    primitives: &RenderPrimitiveQuery,
    meshes: &Assets<Mesh>,
    materials: &Assets<StandardMaterial>,
    images: &Assets<Image>,
) -> Result<AssetValidationSummary, String> {
    let mut summary = AssetValidationSummary::default();
    for descendant in children.iter_descendants(root) {
        let Ok((mesh, material_handle, extras)) = primitives.get(descendant) else {
            continue;
        };
        if meshes.get(mesh).is_none() {
            return Err(format!(
                "mesh {:?} is absent after scene readiness",
                mesh.id()
            ));
        }
        summary.meshes += 1;
        let Some(material_handle) = material_handle else {
            if extras.is_some_and(has_explicit_material_exclusion) {
                summary.excluded_materials += 1;
                continue;
            }
            return Err(format!(
                "mesh entity {descendant:?} has no loaded material or explicit exclusion"
            ));
        };
        let material = materials.get(material_handle).ok_or_else(|| {
            format!(
                "material {:?} is absent after scene readiness",
                material_handle.id()
            )
        })?;
        summary.images += validate_standard_material(material, images)?;
        summary.materials += 1;
    }
    Ok(summary)
}

fn has_explicit_material_exclusion(extras: &GltfExtras) -> bool {
    serde_json::from_str::<serde_json::Value>(&extras.value)
        .ok()
        .and_then(|value| {
            value
                .pointer("/openSkyrim/materialExclusion")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some()
}

pub(crate) fn validate_standard_material(
    material: &StandardMaterial,
    images: &Assets<Image>,
) -> Result<usize, String> {
    match material.alpha_mode {
        AlphaMode::Opaque | AlphaMode::Blend => {}
        AlphaMode::Mask(cutoff) if cutoff.is_finite() && (0.0..=1.0).contains(&cutoff) => {}
        AlphaMode::Mask(cutoff) => return Err(format!("invalid alpha cutoff {cutoff}")),
        mode => return Err(format!("unsupported Skyrim material alpha mode {mode:?}")),
    }
    if material.double_sided != material.cull_mode.is_none() {
        return Err(format!(
            "inconsistent culling: double_sided={} cull_mode={:?}",
            material.double_sided, material.cull_mode
        ));
    }
    let emissive = material.emissive;
    if ![emissive.red, emissive.green, emissive.blue, emissive.alpha]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err("emissive contains a non-finite or negative channel".to_owned());
    }

    let slots = [
        ("base_color", material.base_color_texture.as_ref(), true),
        ("emissive", material.emissive_texture.as_ref(), true),
        (
            "metallic_roughness",
            material.metallic_roughness_texture.as_ref(),
            false,
        ),
        ("normal", material.normal_map_texture.as_ref(), false),
        ("occlusion", material.occlusion_texture.as_ref(), false),
        ("specular", material.specular_texture.as_ref(), false),
        (
            "specular_tint",
            material.specular_tint_texture.as_ref(),
            true,
        ),
    ];
    let mut validated = 0usize;
    for (slot, handle, expects_srgb) in slots {
        let Some(handle) = handle else {
            continue;
        };
        let image = images
            .get(handle)
            .ok_or_else(|| format!("{slot} image {:?} is not loaded", handle.id()))?;
        let descriptor = &image.texture_descriptor;
        if descriptor.size.width == 0
            || descriptor.size.height == 0
            || descriptor.mip_level_count == 0
        {
            return Err(format!("{slot} image has invalid dimensions or mip levels"));
        }
        if descriptor.format.is_srgb() != expects_srgb {
            return Err(format!(
                "{slot} image color space mismatch: {:?}",
                descriptor.format
            ));
        }
        validate_image_sampler(slot, &image.sampler)?;
        validated += 1;
    }
    Ok(validated)
}

fn validate_image_sampler(slot: &str, sampler: &ImageSampler) -> Result<(), String> {
    let ImageSampler::Descriptor(descriptor) = sampler else {
        return Ok(());
    };
    if descriptor.anisotropy_clamp == 0
        || !descriptor.lod_min_clamp.is_finite()
        || !descriptor.lod_max_clamp.is_finite()
        || descriptor.lod_min_clamp > descriptor.lod_max_clamp
    {
        return Err(format!("{slot} image has an invalid sampler descriptor"));
    }
    if descriptor.anisotropy_clamp > 1
        && (descriptor.mag_filter != ImageFilterMode::Linear
            || descriptor.min_filter != ImageFilterMode::Linear
            || descriptor.mipmap_filter != ImageFilterMode::Linear)
    {
        return Err(format!(
            "{slot} image requests anisotropy without linear filtering"
        ));
    }
    Ok(())
}

fn error_chain(error: &(dyn StdError + 'static)) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = Some(error);
    while let Some(error) = current {
        chain.push(error.to_string());
        current = error.source();
    }
    chain
}

fn record_asset_failure(
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
    pending: &PendingAssetProfile,
    dependency_chain: Vec<String>,
    material_validation: bool,
) {
    metrics.asset_load_failures = metrics.asset_load_failures.saturating_add(1);
    if material_validation {
        metrics.material_validation_failures =
            metrics.material_validation_failures.saturating_add(1);
    }
    profiler.increment("assets/load_failures", 1);
    profiler.event(&pending.path, "asset_failed", None);
    let mut full_chain = vec![
        format!("REFR {:08X}", pending.form_id),
        format!("base record {:08X}", pending.base_form_id),
        pending.path.clone(),
    ];
    full_chain.extend(dependency_chain);
    error!(
        reference = format_args!("{:08X}", pending.form_id),
        base = format_args!("{:08X}", pending.base_form_id),
        cell = format_args!("{:08X}", pending.cell_id),
        path = %pending.path,
        chain = ?full_chain,
        "model, material, or image dependency failed strict validation"
    );
    metrics.asset_failures.push(AssetFailure {
        model_path: pending.path.clone(),
        reference_form_id: pending.form_id,
        base_form_id: pending.base_form_id,
        cell_id: pending.cell_id,
        dependency_chain: full_chain,
    });
}

fn hide_partial_scene(commands: &mut Commands, root: Entity, children: &Query<&Children>) {
    for descendant in children.iter_descendants(root) {
        commands.entity(descendant).insert(Visibility::Hidden);
    }
}

fn spawn_diagnostic_fallback(
    commands: &mut Commands,
    root: Entity,
    fallback: &mut DiagnosticFallbackAssets,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    let mesh = fallback
        .mesh
        .get_or_insert_with(|| meshes.add(Cuboid::new(96.0, 96.0, 96.0)))
        .clone();
    let material = fallback
        .material
        .get_or_insert_with(|| {
            materials.add(StandardMaterial {
                base_color: Color::srgb(1.0, 0.0, 0.8),
                emissive: LinearRgba::new(8.0, 0.0, 5.0, 1.0),
                unlit: true,
                ..default()
            })
        })
        .clone();
    commands.entity(root).with_child((
        Name::new("DIAGNOSTIC ASSET FAILURE"),
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::default(),
    ));
}

fn cell_translation(key: CellKey, origin: IVec2) -> Vec3 {
    match key {
        CellKey::Exterior { grid_x, grid_y, .. } => Vec3::new(
            (grid_x - origin.x) as f32 * CELL_SIZE,
            0.0,
            -(grid_y - origin.y) as f32 * CELL_SIZE,
        ),
        CellKey::Interior(_) => Vec3::ZERO,
    }
}

pub(crate) fn creation_to_bevy(position: Vec3) -> Vec3 {
    Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
        position.to_array(),
    ))
}

/// Where a Creation-space point lands in render coordinates while `origin` is the render origin:
/// the position [`spawn_cell`] gives an exterior reference of that point, and the local position
/// a camera has to take to stand on it.
///
/// An interior cell root carries no grid offset, so an interior reference at the same point
/// renders at [`creation_to_bevy`] of it instead.
pub(crate) fn render_position(creation_position: Vec3, origin: IVec2) -> Vec3 {
    let position = creation_to_bevy(creation_position);
    Vec3::new(
        position.x - origin.x as f32 * CELL_SIZE,
        position.y,
        position.z + origin.y as f32 * CELL_SIZE,
    )
}

pub(crate) fn creation_rotation_to_bevy(rotation: [f32; 3]) -> Quat {
    Quat::from_array(shared::coordinates::creation_euler_to_runtime_quaternion(
        rotation,
    ))
}

fn converted_model_path(path: String) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let lowercase = normalized.to_ascii_lowercase();
    let filename = lowercase.rsplit('/').next().unwrap_or_default();
    if lowercase.starts_with("meshes/sky/")
        || lowercase.starts_with("sky/")
        || lowercase.starts_with("meshes/markers/")
        || lowercase.starts_with("markers/")
        || lowercase.starts_with("meshes/effects/")
        || lowercase.starts_with("effects/")
        || filename.contains("marker")
    {
        return None;
    }
    let without_prefix = normalized
        .strip_prefix("meshes/")
        .or_else(|| normalized.strip_prefix("Meshes/"))
        .unwrap_or(&normalized);
    if without_prefix.is_empty() {
        return None;
    }
    let mut converted = std::path::PathBuf::from("meshes").join(without_prefix);
    converted.set_extension("glb");
    Some(converted.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn quadrant_layers(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<Vec<&TerrainLayerSnapshot>, String> {
    let mut layers: Vec<_> = terrain
        .layers
        .iter()
        .filter(|layer| layer.quadrant == quadrant)
        .collect();
    layers.sort_by_key(|layer| (!layer.is_base, layer.layer, layer.texture_form_id));
    if layers.is_empty() {
        return Ok(layers);
    }
    let base_count = layers.iter().filter(|layer| layer.is_base).count();
    if base_count != 1 {
        return Err(format!(
            "LAND {:08X} quadrant {quadrant} has {base_count} base layers; expected one",
            terrain.cell_id
        ));
    }
    if layers.len() > 6 {
        return Err(format!(
            "LAND {:08X} quadrant {quadrant} has {} layers; runtime supports six",
            terrain.cell_id,
            layers.len()
        ));
    }
    let mut layer_ids = HashSet::new();
    for layer in layers.iter().filter(|layer| !layer.is_base) {
        if !layer_ids.insert(layer.layer) {
            return Err(format!(
                "LAND {:08X} quadrant {quadrant} repeats ATXT layer {}",
                terrain.cell_id, layer.layer
            ));
        }
        let mut vertices = HashSet::new();
        for &(vertex, opacity) in &layer.weights {
            if usize::from(vertex) >= 17 * 17
                || !opacity.is_finite()
                || !(0.0..=1.0).contains(&opacity)
                || !vertices.insert(vertex)
            {
                return Err(format!(
                    "LAND {:08X} quadrant {quadrant} has invalid or duplicate VTXT data",
                    terrain.cell_id
                ));
            }
        }
    }
    Ok(layers)
}

fn validate_terrain_snapshot(
    terrain: &TerrainSnapshot,
    catalog: &AssetCatalog,
) -> Result<(), String> {
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width != 33 || height != 33 || terrain.heights.len() != width * height {
        return Err(format!(
            "terrain dimensions/data mismatch: {width}x{height} with {} heights",
            terrain.heights.len()
        ));
    }
    if terrain.heights.iter().any(|height| !height.is_finite()) {
        return Err("terrain contains a non-finite height".to_owned());
    }
    if terrain.normals.len() != width * height * 3 {
        return Err(format!(
            "terrain has {} packed normal bytes",
            terrain.normals.len()
        ));
    }
    if terrain
        .normals
        .as_chunks::<3>()
        .0
        .iter()
        .any(|normal| normal == &[0, 0, 0])
    {
        return Err("terrain contains a zero-length packed normal".to_owned());
    }
    if !terrain.vertex_colors.is_empty() && terrain.vertex_colors.len() != width * height * 3 {
        return Err(format!(
            "terrain has {} packed vertex-color bytes",
            terrain.vertex_colors.len()
        ));
    }
    for quadrant in 0..4 {
        for layer in quadrant_layers(terrain, quadrant)? {
            if layer.is_base && layer.texture_form_id == 0 {
                continue;
            }
            if catalog.landscape_diffuse(layer.texture_form_id).is_none() {
                return Err(format!(
                    "quadrant {quadrant} texture {:08X} has no converted diffuse image",
                    layer.texture_form_id
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn build_terrain_quadrant_mesh(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<Mesh, String> {
    let layers = quadrant_layers(terrain, quadrant)?;
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width != 33 || height != 33 || terrain.heights.len() != width * height {
        return Err("terrain must contain a complete 33x33 height field".to_owned());
    }
    let step_x = CELL_SIZE / (width - 1) as f32;
    let step_z = CELL_SIZE / (height - 1) as f32;
    let origin_x = usize::from(quadrant % 2) * 16;
    let origin_y = usize::from(quadrant / 2) * 16;
    let mut overlay_weights = vec![vec![0.0f32; 17 * 17]; layers.len().saturating_sub(1)];
    for (slot, layer) in layers.iter().skip(1).enumerate() {
        for &(vertex, opacity) in &layer.weights {
            overlay_weights[slot][usize::from(vertex)] = opacity;
        }
    }
    let mut positions = Vec::with_capacity(17 * 17);
    let mut normals = Vec::with_capacity(17 * 17);
    let mut uvs = Vec::with_capacity(17 * 17);
    let mut extra_weights = Vec::with_capacity(17 * 17);
    let mut packed_weights = Vec::with_capacity(17 * 17);
    let mut colors = Vec::with_capacity(17 * 17);
    for local_y in 0..17 {
        for local_x in 0..17 {
            let x = origin_x + local_x;
            let y = origin_y + local_y;
            let index = y * width + x;
            let local = local_y * 17 + local_x;
            positions.push([
                x as f32 * step_x,
                terrain.heights[index],
                -(y as f32 * step_z),
            ]);
            normals.push(
                Vec3::new(
                    terrain.normals[index * 3] as f32,
                    terrain.normals[index * 3 + 2] as f32,
                    -(terrain.normals[index * 3 + 1] as f32),
                )
                .normalize_or(Vec3::Y)
                .to_array(),
            );
            uvs.push([
                x as f32 / (width - 1) as f32,
                y as f32 / (height - 1) as f32,
            ]);
            let weight = |slot: usize| {
                overlay_weights
                    .get(slot)
                    .map_or(0.0, |values| values[local])
            };
            let first = Vec3::new(weight(0), weight(1), weight(2));
            let length = first.length();
            packed_weights.push(if length > 0.0 {
                let normalized = first / length;
                [normalized.x, normalized.y, normalized.z, length]
            } else {
                [0.0; 4]
            });
            extra_weights.push([weight(3), weight(4)]);
            colors.push(if terrain.vertex_colors.is_empty() {
                [1.0; 4]
            } else {
                [
                    terrain.vertex_colors[index * 3] as f32 / 255.0,
                    terrain.vertex_colors[index * 3 + 1] as f32 / 255.0,
                    terrain.vertex_colors[index * 3 + 2] as f32 / 255.0,
                    1.0,
                ]
            });
        }
    }
    let mut indices = Vec::with_capacity(16 * 16 * 6);
    for y in 0..16 {
        for x in 0..16 {
            let a = (y * 17 + x) as u32;
            let b = a + 1;
            let c = a + 17;
            let d = c + 1;
            indices.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, extra_weights);
    mesh.insert_attribute(Mesh::ATTRIBUTE_TANGENT, packed_weights);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    Ok(mesh)
}

/// The largest difference between an arriving cell's edge and a resident neighbour's that is
/// still treated as a seam to weld.
///
/// Real seams are small: the worst measured on `Skyrim.esm` is 24 units on one of 33 points of
/// Tamriel's (18,18) north edge against (18,19), and 16 units on one of (18,20)'s east edge -
/// across a sample step of 128 units. Anything larger is corrupt or mismatched terrain rather
/// than a seam, and the cell is rejected as before.
const MAX_WELDABLE_EDGE_DELTA: f32 = 512.0;

/// Edge heights closer than this are already the same point: the tolerance the strict comparison
/// used before edges were welded.
const EDGE_MATCH_TOLERANCE: f32 = 0.01;

/// Registers a cell's edge heights, welding the arriving cell onto the neighbours already drawn.
///
/// The resident neighbour is authoritative: its mesh is in the world, so where the two disagree
/// the arriving cell moves. This replaces the strict comparison that rejected the whole cell -
/// and with it the terrain and its references - over a single point of one edge, which is what
/// leaves a hole in the ground on real `Skyrim.esm` data. What is not a seam is still rejected:
/// an edge of a different length, a non-finite height on either side, and a difference above
/// [`MAX_WELDABLE_EDGE_DELTA`].
///
/// The welded heights - not the loaded ones - are what gets registered, so a cell arriving later
/// welds onto the surface that is actually drawn and the block stays watertight.
fn validate_and_register_terrain_edges(
    key: CellKey,
    terrain: &mut TerrainSnapshot,
    continuity: &mut TerrainContinuity,
    metrics: &mut StreamingMetrics,
) -> Result<(), String> {
    let CellKey::Exterior {
        worldspace_id,
        grid_x,
        grid_y,
    } = key
    else {
        return Ok(());
    };
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width == 0 || height == 0 || terrain.heights.len() != width * height {
        return Err(format!(
            "terrain dimensions/data mismatch: {width}x{height} with {} heights",
            terrain.heights.len()
        ));
    }
    if terrain.heights.iter().any(|height| !height.is_finite()) {
        return Err("terrain contains a non-finite height".to_owned());
    }
    let grid = IVec2::new(grid_x, grid_y);
    // Everything is checked before anything moves, so a rejected cell is left exactly as it was
    // loaded even when an earlier edge was weldable.
    let mut welded_edges = Vec::new();
    for side in TerrainEdgeSide::ALL {
        let neighbor_key = side.neighbor_key(worldspace_id, grid);
        let Some(neighbor) = continuity.edges.get(&neighbor_key) else {
            continue;
        };
        let points = side.points(terrain);
        let other = neighbor.side(side.opposite());
        if points.len() != other.len() {
            return Err(format!(
                "terrain edge {side:?} has {} points; neighbor {neighbor_key:?} has {}",
                points.len(),
                other.len()
            ));
        }
        if other.iter().any(|height| !height.is_finite()) {
            return Err(format!(
                "terrain edge {side:?} of neighbor {neighbor_key:?} is not finite"
            ));
        }
        let max_delta = points
            .iter()
            .zip(other)
            .map(|(index, height)| (terrain.heights[*index] - height).abs())
            .fold(0.0_f32, f32::max);
        if max_delta > MAX_WELDABLE_EDGE_DELTA {
            return Err(format!(
                "terrain edge {side:?} differs from neighbor {neighbor_key:?} by {max_delta} units"
            ));
        }
        welded_edges.push((side, neighbor_key, other.to_vec(), max_delta));
    }
    let mut welded_points = Vec::new();
    for (side, neighbor_key, other, max_delta) in welded_edges {
        let mut moved = 0u64;
        for (index, height) in side.points(terrain).into_iter().zip(&other) {
            if (terrain.heights[index] - height).abs() > EDGE_MATCH_TOLERANCE {
                welded_points.push(index);
                moved += 1;
            }
            terrain.heights[index] = *height;
        }
        if moved > 0 {
            warn!(
                cell = format_args!("{:08X}", terrain.cell_id),
                neighbor = ?neighbor_key,
                max_delta,
                moved,
                "LAND edge welded onto the resident neighbor"
            );
        }
        metrics.terrain_seams_validated = metrics.terrain_seams_validated.saturating_add(1);
    }
    if !welded_points.is_empty() {
        // A moved point no longer lies where its stored normal was computed, and the drawn
        // triangle is what gets shaded, so recompute it from the welded field. Real seams have the
        // two sides' normals already matching (at the 24-unit seam above, 0 of 99 `VNML` bytes
        // differ), so keeping the loaded ones instead would shade the boundary exactly like the
        // neighbour at the cost of a normal that disagrees with our own geometry.
        recompute_packed_normals(terrain, &welded_points);
        metrics.terrain_seam_points_welded = metrics
            .terrain_seam_points_welded
            .saturating_add(welded_points.len() as u64);
    }
    continuity.edges.insert(key, TerrainEdges::of(terrain));
    Ok(())
}

/// Recomputes the packed `VNML` bytes of the listed height-field points from the heights around
/// them, in the converter's own encoding (`crates/converter/src/esm/cell_cache.rs`,
/// `decode_normals`): `(h(left) - h(right), h(down) - h(up), 2 * step)`, normalized and scaled by
/// 127. `points` index a complete `width * height` field.
fn recompute_packed_normals(terrain: &mut TerrainSnapshot, points: &[usize]) {
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width < 2 || height < 2 || terrain.normals.len() != width * height * 3 {
        return;
    }
    let step = CELL_SIZE / (width - 1) as f32;
    for &index in points {
        let (x, y) = (index % width, index / width);
        let left = terrain.heights[y * width + x.saturating_sub(1)];
        let right = terrain.heights[y * width + (x + 1).min(width - 1)];
        let down = terrain.heights[y.saturating_sub(1) * width + x];
        let up = terrain.heights[(y + 1).min(height - 1) * width + x];
        let normal = Vec3::new(left - right, down - up, 2.0 * step).normalize_or(Vec3::Z);
        // A height field's own normal always has a positive up component, so the packed bytes can
        // never come out all zero - which is what the validation rejects.
        let byte = |component: f32| (component * 127.0).round().clamp(-127.0, 127.0) as i8;
        terrain.normals[index * 3..index * 3 + 3].copy_from_slice(&[
            byte(normal.x),
            byte(normal.y),
            byte(normal.z),
        ]);
    }
}

/// Places every spawned exterior cell root where `origin` says it belongs. Interiors carry no
/// [`ExteriorCellGrid`] and sit at the origin already, so they are not touched.
pub(crate) fn reposition_cell_roots(
    origin: IVec2,
    roots: &mut Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
) {
    for (grid, mut transform) in roots.iter_mut() {
        transform.translation = Vec3::new(
            (grid.0.x - origin.x) as f32 * CELL_SIZE,
            0.0,
            -(grid.0.y - origin.y) as f32 * CELL_SIZE,
        );
    }
}

fn update_render_origin(
    active: Res<ActiveCell>,
    mut origin: ResMut<RenderOrigin>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut roots: Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    // An interior is placed at its absolute creation coordinates and carries no cell grid, so
    // rebasing would slide the interior sideways relative to the camera. A crossing out of one
    // sets the origin for the destination itself.
    if active.interior.is_some() {
        return;
    }
    let started = Instant::now();
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    let shift = IVec2::new(
        (camera.translation.x / CELL_SIZE).trunc() as i32,
        (-camera.translation.z / CELL_SIZE).trunc() as i32,
    );
    if shift == IVec2::ZERO {
        return;
    }
    origin.0 += shift;
    camera.translation.x -= shift.x as f32 * CELL_SIZE;
    camera.translation.z += shift.y as f32 * CELL_SIZE;
    reposition_cell_roots(origin.0, &mut roots);
    profiler.increment("streaming/origin_rebases", 1);
    metrics.origin_rebases = metrics.origin_rebases.saturating_add(1);
    profiler.event(
        format!("{},{}", origin.0.x, origin.0.y),
        "origin_rebased",
        None,
    );
    profiler.record_elapsed("streaming/render_origin_rebase", started);
}

#[allow(clippy::too_many_arguments)]
fn validate_streaming_lifecycle(
    config: Res<EngineConfig>,
    active: Res<ActiveCell>,
    prestream: Res<PrestreamCells>,
    origin: Res<RenderOrigin>,
    streaming: Res<StreamingWorld>,
    camera: Query<&Transform, With<StreamingCamera>>,
    roots: Query<(Entity, &CellRef, Option<&ExteriorCellGrid>), With<StreamedCellRoot>>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let active_requests = streaming
        .cells
        .values()
        .filter(|status| matches!(status, CellStatus::Loading { .. }))
        .count();
    let resident_entities: HashSet<_> = streaming
        .cells
        .values()
        .filter_map(|status| match status {
            CellStatus::Resident { root } => Some(*root),
            _ => None,
        })
        .collect();
    let root_entries: Vec<_> = roots.iter().collect();
    let root_entities: HashSet<_> = root_entries.iter().map(|(entity, _, _)| *entity).collect();
    let mut roots_by_cell = HashMap::<u32, usize>::new();
    for (_, cell, _) in &root_entries {
        *roots_by_cell.entry(cell.0).or_default() += 1;
    }
    let duplicate_roots = roots_by_cell.values().filter(|count| **count > 1).count() as u64;
    let orphaned_roots = root_entities.difference(&resident_entities).count() as u64;
    let missing_roots = resident_entities.difference(&root_entities).count() as u64;
    // Out of range means "resident although the plan does not want it any more": an exterior of
    // the active worldspace the camera has left behind. Cells of the worldspace the camera came
    // from, and the destination pre-streamed behind a door, are legitimately outside that radius
    // -- while an interior is active there is no exterior plan at all.
    let out_of_range_roots = if active.interior.is_some() {
        0
    } else {
        camera.single().map_or(0, |camera| {
            let global_x = camera.translation.x + origin.0.x as f32 * CELL_SIZE;
            let global_y = -camera.translation.z + origin.0.y as f32 * CELL_SIZE;
            let center = IVec2::new(
                (global_x / CELL_SIZE).floor() as i32,
                (global_y / CELL_SIZE).floor() as i32,
            );
            streaming
                .cells
                .iter()
                .filter(|(key, status)| {
                    let key = **key;
                    matches!(status, CellStatus::Resident { .. })
                        && !prestream.contains(&key)
                        && matches!(key, CellKey::Exterior { worldspace_id, grid_x, grid_y }
                            if worldspace_id == active.worldspace_id
                                && ((grid_x - center.x).abs() > config.unload_radius
                                    || (grid_y - center.y).abs() > config.unload_radius))
                })
                .count() as u64
        })
    };
    let violations = duplicate_roots + orphaned_roots + missing_roots + out_of_range_roots;

    metrics.active_requests = active_requests;
    metrics.peak_active_requests = metrics.peak_active_requests.max(active_requests);
    metrics.resident_roots = root_entries.len();
    metrics.duplicate_cell_roots = metrics.duplicate_cell_roots.max(duplicate_roots);
    metrics.orphaned_cell_roots = metrics.orphaned_cell_roots.max(orphaned_roots);
    metrics.missing_cell_roots = metrics.missing_cell_roots.max(missing_roots);
    metrics.out_of_range_cell_roots = metrics.out_of_range_cell_roots.max(out_of_range_roots);
    if violations > metrics.streaming_invariant_failures {
        error!(
            duplicate_roots,
            orphaned_roots,
            missing_roots,
            out_of_range_roots,
            "streaming lifecycle invariant failed"
        );
        profiler.event("streaming", "invariant_failed", None);
    }
    metrics.streaming_invariant_failures = metrics.streaming_invariant_failures.max(violations);
    profiler.set_gauge("streaming/active_requests", active_requests as f64);
    profiler.set_gauge("streaming/resident_roots", root_entries.len() as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::database::DoorLinkRow;
    use bevy::asset::{AssetApp, AssetPlugin};

    #[test]
    fn commit_budget_ignores_only_the_documented_scheduler_tolerance() {
        assert!(!commit_budget_exceeded(16_670, 16_670));
        assert!(!commit_budget_exceeded(17_670, 16_670));
        assert!(commit_budget_exceeded(17_671, 16_670));
    }

    fn terrain_fixture(cell_id: u32, height: f32) -> TerrainSnapshot {
        TerrainSnapshot {
            cell_id,
            width: 33,
            height: 33,
            heights: vec![height; 33 * 33],
            normals: (0..33 * 33).flat_map(|_| [0, 0, 127]).collect(),
            vertex_colors: vec![255; 33 * 33 * 3],
            layers: (0..4)
                .map(|quadrant| TerrainLayerSnapshot {
                    texture_form_id: u32::from(quadrant) + 1,
                    quadrant,
                    layer: 0,
                    is_base: true,
                    weights: Vec::new(),
                })
                .collect(),
            water_height: None,
            water_type_form_id: None,
        }
    }

    /// The same field without layers: `spawn_cell` looks every layer's diffuse image up in the
    /// asset catalogue, and the catalogue of a test app has none in it.
    fn untextured_fixture(cell_id: u32, height: f32) -> TerrainSnapshot {
        let mut terrain = terrain_fixture(cell_id, height);
        terrain.layers.clear();
        terrain
    }

    fn exterior_cell(grid_x: i32, grid_y: i32) -> CellKey {
        CellKey::Exterior {
            worldspace_id: 60,
            grid_x,
            grid_y,
        }
    }

    /// The tables `AssetCatalog::open` reads, empty: the cells spawned by these tests carry no
    /// layers, so nothing is ever looked up in it. The catalogue is read eagerly, so the file may
    /// go away with its temporary directory.
    fn write_empty_catalogue(path: &std::path::Path) {
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);
                 CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT);
                 CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);",
            )
            .unwrap();
    }

    #[derive(Resource, Default)]
    struct QueuedCells(Vec<(CellKey, TerrainSnapshot)>);

    /// Spawns each queued cell through the real [`spawn_cell`] and records it as resident, so the
    /// lifecycle validator next in the chain sees entities and bookkeeping that agree.
    #[allow(clippy::too_many_arguments)]
    fn spawn_queued_cells(
        mut commands: Commands,
        queued: Res<QueuedCells>,
        asset_server: Res<AssetServer>,
        catalog: Res<AssetCatalog>,
        reflection: Res<WaterReflectionTexture>,
        mut meshes: ResMut<Assets<Mesh>>,
        mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
        mut water_materials: ResMut<Assets<WaterMaterial>>,
        mut streaming: ResMut<StreamingWorld>,
        mut profiler: ResMut<ProfilingState>,
    ) {
        for (key, terrain) in &queued.0 {
            let payload = CellPayload {
                generation: 1,
                key: *key,
                cell_id: terrain.cell_id,
                references: Vec::new(),
            };
            let root = spawn_cell(
                &mut commands,
                &asset_server,
                &catalog,
                &reflection,
                &mut meshes,
                &mut terrain_materials,
                &mut water_materials,
                IVec2::ZERO,
                payload,
                Some(terrain.clone()),
                &mut profiler,
            );
            streaming.cells.insert(*key, CellStatus::Resident { root });
        }
    }

    /// An app that streams the given cells the way a commit does - spawn first, then the lifecycle
    /// validator, in one frame - with the assets `spawn_cell` writes into.
    fn spawn_cells_app(cells: Vec<(CellKey, TerrainSnapshot)>, metrics: StreamingMetrics) -> App {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalogue.db");
        write_empty_catalogue(&path);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_asset::<StandardMaterial>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .insert_resource(AssetCatalog::open(&path).unwrap())
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .insert_resource(QueuedCells(cells))
            .insert_resource(metrics)
            .init_resource::<StreamingWorld>()
            .init_resource::<PrestreamCells>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<ProfilingState>()
            .add_systems(
                Update,
                (spawn_queued_cells, validate_streaming_lifecycle).chain(),
            );
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        app.update();
        app
    }

    #[test]
    fn maps_nif_paths_to_converted_glb_paths() {
        assert_eq!(
            converted_model_path("meshes\\architecture\\wall.nif".into()).as_deref(),
            Some("meshes/architecture/wall.glb")
        );
        assert_eq!(
            converted_model_path("meshes/Sky/CloudShape01.nif".into()),
            None
        );
        assert_eq!(converted_model_path("meshes/Marker_Map.nif".into()), None);
        assert_eq!(
            converted_model_path("Markers/CivilWarMarkers/CWAttSpawn02.nif".into()),
            None
        );
        assert_eq!(converted_model_path("Effects/FXRapids.nif".into()), None);
        assert_eq!(
            converted_model_path("meshes/Furniture/SitLedgeMarker.nif".into()),
            None
        );
    }

    #[test]
    fn unload_radius_drops_distant_exteriors_and_inactive_interiors() {
        let center = IVec2::new(4, -2);
        let outside = ActiveCell {
            worldspace_id: 60,
            interior: None,
        };
        let inside = ActiveCell {
            worldspace_id: 60,
            interior: Some(99),
        };
        let none = PrestreamCells::default();
        let exterior = |worldspace_id, grid_x, grid_y| CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        };

        assert!(cell_within_unload_radius(
            exterior(60, 7, -5),
            &outside,
            center,
            3,
            &none
        ));
        assert!(!cell_within_unload_radius(
            exterior(60, 8, -2),
            &outside,
            center,
            3,
            &none
        ));
        assert!(
            !cell_within_unload_radius(exterior(614, 4, -2), &outside, center, 3, &none),
            "the same grid in another worldspace belongs to the worldspace just left"
        );
        assert!(
            !cell_within_unload_radius(exterior(60, 4, -2), &inside, center, 3, &none),
            "no exterior is streamed while an interior is active"
        );
        assert!(cell_within_unload_radius(
            CellKey::Interior(99),
            &inside,
            center,
            0,
            &none
        ));
        assert!(
            !cell_within_unload_radius(CellKey::Interior(98), &inside, center, 0, &none),
            "an interior that is neither active nor pre-streamed unloads"
        );
        assert!(!cell_within_unload_radius(
            CellKey::Interior(99),
            &outside,
            center,
            0,
            &none
        ));

        // A pre-streamed destination survives anywhere until the request stops.
        let mut prestream = PrestreamCells::default();
        prestream.request_interior(98);
        prestream.request_exterior(614, IVec2::new(5, 4));
        assert!(cell_within_unload_radius(
            CellKey::Interior(98),
            &inside,
            center,
            0,
            &prestream
        ));
        assert!(cell_within_unload_radius(
            exterior(614, 5, 4),
            &inside,
            center,
            3,
            &prestream
        ));
        assert!(!cell_within_unload_radius(
            CellKey::Interior(97),
            &inside,
            center,
            0,
            &prestream
        ));
    }

    #[test]
    fn wanted_cells_are_the_active_interior_or_the_active_worldspace_grid() {
        let center = IVec2::new(4, -2);
        let none = PrestreamCells::default();
        let inside = ActiveCell {
            worldspace_id: 60,
            interior: Some(99),
        };
        assert_eq!(
            wanted_cells(&inside, 1, center, &none),
            HashSet::from([CellKey::Interior(99)]),
            "an interior is the whole plan"
        );

        let blackreach = ActiveCell {
            worldspace_id: 614,
            interior: None,
        };
        let wanted = wanted_cells(&blackreach, 1, center, &none);
        assert_eq!(wanted.len(), 9);
        assert!(wanted.iter().all(|key| matches!(
            key,
            CellKey::Exterior {
                worldspace_id: 614,
                ..
            }
        )));
        assert!(wanted.contains(&CellKey::Exterior {
            worldspace_id: 614,
            grid_x: 5,
            grid_y: -3,
        }));

        // Pre-streamed destinations are wanted on top of the active cell's own plan.
        let mut prestream = PrestreamCells::default();
        prestream.request_interior(98);
        prestream.request_exterior(60, IVec2::new(19, 18));
        let wanted = wanted_cells(&inside, 1, center, &prestream);
        assert!(wanted.contains(&CellKey::Interior(99)));
        assert!(wanted.contains(&CellKey::Interior(98)));
        assert!(wanted.contains(&CellKey::Exterior {
            worldspace_id: 60,
            grid_x: 19,
            grid_y: 18,
        }));
    }

    /// The columns [`plan_cells`] and the worker's queries read, without any cell of the
    /// destination worldspace: this test only checks which keys the planner asks for.
    fn write_plan_database(path: &std::path::Path) {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch(&format!(
                r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
                INSERT INTO schema_info VALUES({version});
                CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER,interior_name TEXT,flags INTEGER NOT NULL);
                CREATE TABLE land(cell_id INTEGER PRIMARY KEY,heightmap BLOB NOT NULL);
                CREATE TABLE statics(id INTEGER PRIMARY KEY,editor_id TEXT,model_path TEXT,flags INTEGER NOT NULL,
                    bounds_min_x REAL NOT NULL DEFAULT -64,bounds_min_y REAL NOT NULL DEFAULT -64,bounds_min_z REAL NOT NULL DEFAULT -64,
                    bounds_max_x REAL NOT NULL DEFAULT 64,bounds_max_y REAL NOT NULL DEFAULT 64,bounds_max_z REAL NOT NULL DEFAULT 64,
                    bounds_valid INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER NOT NULL,worldspace_id INTEGER,base_form_id INTEGER NOT NULL,
                    is_exterior INTEGER NOT NULL,pos_x REAL NOT NULL,pos_y REAL NOT NULL,pos_z REAL NOT NULL,local_x REAL,local_y REAL,
                    rot_x REAL NOT NULL,rot_y REAL NOT NULL,rot_z REAL NOT NULL,scale REAL NOT NULL DEFAULT 1.0);
                CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
                INSERT INTO cells VALUES(99,NULL,NULL,NULL,'Alftand01',0);"#,
                version = shared::WORLD_DATABASE_SCHEMA_VERSION
            ))
            .unwrap();
    }

    fn planned_keys(app: &App) -> HashSet<CellKey> {
        app.world()
            .resource::<StreamingWorld>()
            .cells
            .keys()
            .copied()
            .collect()
    }

    #[test]
    fn plan_cells_follows_the_active_cell_across_a_worldspace_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        write_plan_database(&path);
        let config = EngineConfig {
            stream_radius: 1,
            ..EngineConfig::default()
        };
        let mut app = App::new();
        app.insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: Some(99),
            })
            .insert_resource(WorldDatabase::open(&path).unwrap())
            .init_resource::<StreamingWorld>()
            .init_resource::<PrestreamCells>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, plan_cells);
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(100.0, 40.0, -200.0)),
            StreamingCamera,
        ));
        app.update();

        assert_eq!(
            planned_keys(&app),
            HashSet::from([CellKey::Interior(99)]),
            "an interior plans exactly itself, not a grid of exteriors"
        );
        assert_eq!(
            app.world()
                .resource::<StreamingMetrics>()
                .requests_submitted,
            1
        );

        *app.world_mut().resource_mut::<ActiveCell>() = ActiveCell {
            worldspace_id: 614,
            interior: None,
        };
        app.update();

        let wanted = planned_keys(&app);
        assert_eq!(wanted.len(), 9);
        assert!(
            !wanted.contains(&CellKey::Interior(99)),
            "the interior that stopped being active is unloaded"
        );
        assert!(wanted.iter().all(|key| matches!(
            key,
            CellKey::Exterior {
                worldspace_id: 614,
                ..
            }
        )));
    }

    #[test]
    fn an_interior_reference_keeps_its_camera_relative_position() {
        let mut app = App::new();
        app.insert_resource(RenderOrigin(IVec2::new(3, -2)))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: Some(0x56C1B),
            })
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, update_render_origin);
        // Alftand02's door: -9223.65, -2592.04, -9223.65 in Creation units, i.e. more than two
        // cell widths from the render origin, which is what makes an exterior camera rebase.
        let reference = Vec3::new(-9223.65, -500.0, 2592.04);
        let camera_position = reference + Vec3::new(100.0, 0.0, 0.0);
        let camera = app
            .world_mut()
            .spawn((
                Transform::from_translation(camera_position),
                StreamingCamera,
            ))
            .id();
        let root = app
            .world_mut()
            .spawn((StreamedCellRoot, Transform::default()))
            .id();
        let reference_entity = app
            .world_mut()
            .spawn((Transform::from_translation(reference), ChildOf(root)))
            .id();

        for _ in 0..3 {
            app.update();
        }

        assert_eq!(
            app.world().resource::<StreamingMetrics>().origin_rebases,
            0,
            "an interior never rebases"
        );
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(3, -2));
        let camera_transform = app.world().entity(camera).get::<Transform>().unwrap();
        assert!(
            camera_transform
                .translation
                .abs_diff_eq(camera_position, 1.0e-4)
        );
        let relative = app
            .world()
            .entity(reference_entity)
            .get::<Transform>()
            .unwrap()
            .translation
            - camera_transform.translation;
        assert!(relative.abs_diff_eq(Vec3::new(-100.0, 0.0, 0.0), 1.0e-4));
    }

    #[test]
    fn builds_a_load_door_from_every_shape_of_door_link() {
        let reference = |door| ReferenceRow {
            form_id: 0x30,
            cell_id: 10,
            base_form_id: 20,
            model_path: None,
            position: [0.0; 3],
            rotation: [0.0; 3],
            scale: 1.0,
            bounds_min: [0.0; 3],
            bounds_max: [0.0; 3],
            bounds_valid: false,
            door,
        };
        let link = |cell, worldspace| DoorLinkRow {
            destination_ref_id: 0x31,
            destination_cell_id: cell,
            destination_worldspace_id: worldspace,
            arrival_position: [1.0, 2.0, 3.0],
            arrival_rotation: [0.0, 0.0, 0.5],
            label: "Alftand01".into(),
        };

        let interior = load_door(&reference(Some(link(Some(99), None)))).unwrap();
        assert_eq!(interior.ref_id, 0x30);
        assert_eq!(interior.destination.interior_cell_id, Some(99));
        assert_eq!(interior.destination.worldspace_id, None);
        assert_eq!(interior.destination.arrival_position, [1.0, 2.0, 3.0]);
        assert_eq!(interior.label, "Alftand01");

        let exterior = load_door(&reference(Some(link(Some(120), Some(614))))).unwrap();
        assert_eq!(exterior.destination.interior_cell_id, None);
        assert_eq!(exterior.destination.worldspace_id, Some(614));

        assert!(load_door(&reference(None)).is_none(), "not a door at all");
        assert!(
            load_door(&reference(Some(link(None, None)))).is_none(),
            "a link the converter could not resolve leads nowhere"
        );
        assert!(
            load_door(&reference(Some(link(Some(0), None)))).is_none(),
            "cell 0 is not a cell the converter resolved"
        );
    }

    #[test]
    fn repeated_rebasing_preserves_camera_and_cell_root_locality() {
        let mut app = App::new();
        app.insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, update_render_origin);
        let camera = app
            .world_mut()
            .spawn((Transform::default(), StreamingCamera))
            .id();
        let root = app
            .world_mut()
            .spawn((ExteriorCellGrid(IVec2::new(8, -3)), Transform::default()))
            .id();
        for shift in [IVec2::new(2, 1), IVec2::new(-3, 4), IVec2::new(7, -2)] {
            {
                let mut entity = app.world_mut().entity_mut(camera);
                let mut transform = entity.get_mut::<Transform>().unwrap();
                transform.translation.x = shift.x as f32 * CELL_SIZE + 12.0;
                transform.translation.z = -(shift.y as f32 * CELL_SIZE) - 20.0;
            }
            app.update();
            let camera_transform = app.world().entity(camera).get::<Transform>().unwrap();
            assert!(camera_transform.translation.x.abs() < CELL_SIZE);
            assert!(camera_transform.translation.z.abs() < CELL_SIZE);
        }
        assert_eq!(app.world().resource::<StreamingMetrics>().origin_rebases, 3);
        let origin = app.world().resource::<RenderOrigin>().0;
        let root_transform = app.world().entity(root).get::<Transform>().unwrap();
        assert_eq!(
            root_transform.translation,
            Vec3::new(
                (8 - origin.x) as f32 * CELL_SIZE,
                0.0,
                -(-3 - origin.y) as f32 * CELL_SIZE,
            )
        );
    }

    #[test]
    fn lifecycle_validator_detects_duplicate_and_orphaned_roots() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .init_resource::<PrestreamCells>()
            .init_resource::<StreamingWorld>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, validate_streaming_lifecycle);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        let resident = app.world_mut().spawn((CellRef(7), StreamedCellRoot)).id();
        app.world_mut().spawn((CellRef(7), StreamedCellRoot));
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                CellKey::Interior(7),
                CellStatus::Resident { root: resident },
            );
        app.update();
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.duplicate_cell_roots, 1);
        assert_eq!(metrics.orphaned_cell_roots, 1);
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 2);
    }

    #[test]
    fn maps_creation_position_and_rotation_through_the_same_basis() {
        assert_eq!(creation_to_bevy(Vec3::Y), Vec3::NEG_Z);
        assert_eq!(creation_to_bevy(Vec3::Z), Vec3::Y);

        let rotation = creation_rotation_to_bevy([0.0, 0.0, std::f32::consts::FRAC_PI_2]);
        let rotated = rotation * Vec3::X;
        assert!(rotated.abs_diff_eq(Vec3::NEG_Z, 1.0e-5));
    }

    #[test]
    fn creates_upward_wound_quadrants_with_continuous_uvs() {
        let terrain = terrain_fixture(1, 0.0);
        let mesh = build_terrain_quadrant_mesh(&terrain, 3).unwrap();
        assert_eq!(mesh.count_vertices(), 17 * 17);
        assert_eq!(mesh.indices().unwrap().len(), 16 * 16 * 6);
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap();
        let [a, b, c] = [positions[0], positions[1], positions[17]];
        let normal = (Vec3::from(b) - Vec3::from(a)).cross(Vec3::from(c) - Vec3::from(a));
        assert!(normal.y > 0.0);
        let VertexAttributeValues::Float32x2(uvs) = mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap()
        else {
            panic!("terrain UVs must be Float32x2");
        };
        assert_eq!(uvs[0], [0.5, 0.5]);
        assert_eq!(uvs[16 * 17 + 16], [1.0, 1.0]);
    }

    /// The rule that replaced "reject any edge that differs": matching edges pass untouched, a
    /// crack inside the weldable range is welded onto the resident cell, and a difference past
    /// the sanity bound is still corrupt data and rejected.
    #[test]
    fn accepts_matching_neighbor_edges_and_welds_small_cracks() {
        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        let west = exterior_cell(0, 0);
        let east = exterior_cell(1, 0);
        let mut west_terrain = terrain_fixture(1, 10.0);
        validate_and_register_terrain_edges(west, &mut west_terrain, &mut continuity, &mut metrics)
            .unwrap();
        let mut east_terrain = terrain_fixture(2, 10.0);
        validate_and_register_terrain_edges(east, &mut east_terrain, &mut continuity, &mut metrics)
            .unwrap();
        assert_eq!(metrics.terrain_seams_validated, 1);
        assert_eq!(
            metrics.terrain_seam_points_welded, 0,
            "a matching edge moves nothing"
        );

        // A one-unit crack is a seam, not a failure: the arriving cell takes the resident heights
        // along the shared edge and keeps its own everywhere else.
        let mut farther_east_terrain = terrain_fixture(3, 11.0);
        validate_and_register_terrain_edges(
            exterior_cell(2, 0),
            &mut farther_east_terrain,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();
        assert!(
            farther_east_terrain
                .heights
                .iter()
                .enumerate()
                .all(|(index, height)| *height == if index % 33 == 0 { 10.0 } else { 11.0 })
        );
        assert_eq!(metrics.terrain_seams_validated, 2);
        assert_eq!(metrics.terrain_seam_points_welded, 33);

        // Past the bound it is not a seam. The cell is rejected and stays unregistered, so no
        // later cell can weld onto a neighbour that was never drawn.
        let corrupt = exterior_cell(3, 0);
        let mut corrupt_terrain = terrain_fixture(4, 11.0 + MAX_WELDABLE_EDGE_DELTA + 1.0);
        assert!(
            validate_and_register_terrain_edges(
                corrupt,
                &mut corrupt_terrain,
                &mut continuity,
                &mut metrics
            )
            .is_err()
        );
        assert!(!continuity.edges.contains_key(&corrupt));
        assert_eq!(
            metrics.terrain_seams_validated, 2,
            "a rejected cell does not count as validated"
        );
    }

    /// The seam measured on real data (`tools/research/land_edges.py`: Tamriel (18,18) against
    /// (18,19), one point of the north edge 24 units off) used to reject the whole cell and leave
    /// a hole where the player stands. It must weld, spawn, and mesh exactly like its neighbour.
    #[test]
    fn welds_one_point_of_an_edge_and_spawns_the_arriving_cell() {
        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        let resident_key = exterior_cell(0, 0);
        let arriving_key = exterior_cell(0, 1);
        let mut resident = untextured_fixture(0x0000_8F64, 100.0);
        validate_and_register_terrain_edges(
            resident_key,
            &mut resident,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();

        // The arriving cell's south edge - height-field row 0 - is the edge it shares with the
        // resident cell's north edge, and it differs at one of its 33 points by 24 units.
        let mut arriving = untextured_fixture(0x0000_8F65, 100.0);
        arriving.heights[9] = 124.0;
        validate_and_register_terrain_edges(
            arriving_key,
            &mut arriving,
            &mut continuity,
            &mut metrics,
        )
        .expect("a seam inside the weldable range must not reject the cell");

        for column in 0..33 {
            assert_eq!(
                arriving.heights[column],
                resident.heights[32 * 33 + column],
                "point {column} of the shared edge"
            );
        }
        assert_eq!(arriving.heights[9], 100.0, "the arriving cell moved");
        assert_eq!(arriving.heights[33 + 9], 100.0, "only the edge moved");
        assert_eq!(metrics.terrain_seams_validated, 1);
        assert_eq!(metrics.terrain_seam_points_welded, 1);

        // Both cells spawn, the arriving one with the welded height in its mesh, and the
        // lifecycle validator has nothing to report about either.
        let mut app = spawn_cells_app(
            vec![(resident_key, resident), (arriving_key, arriving)],
            metrics,
        );
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.terrain_seam_points_welded, 1);
        assert_eq!(metrics.streaming_invariant_failures, 0);
        let handles: Vec<Handle<Mesh>> = {
            let mut patches = app
                .world_mut()
                .query_filtered::<&Mesh3d, With<TerrainPatch>>();
            patches
                .iter(app.world())
                .map(|mesh| mesh.0.clone())
                .collect()
        };
        assert_eq!(handles.len(), 8, "two cells of four quadrants each");
        let meshes = app.world().resource::<Assets<Mesh>>();
        let mut vertices = 0usize;
        for handle in &handles {
            let mesh = meshes.get(handle).expect("a spawned quadrant has its mesh");
            let positions = mesh
                .attribute(Mesh::ATTRIBUTE_POSITION)
                .unwrap()
                .as_float3()
                .unwrap();
            assert!(
                positions.iter().all(|position| position[1] == 100.0),
                "the seam must not survive into the geometry"
            );
            vertices += positions.len();
        }
        assert_eq!(vertices, 8 * 17 * 17);
    }

    /// A welded point no longer lies where its stored normal was computed from, so the normal is
    /// recomputed from the welded field in the converter's packing.
    #[test]
    fn recomputes_the_normals_of_welded_points_only() {
        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        let mut resident = untextured_fixture(1, 100.0);
        validate_and_register_terrain_edges(
            exterior_cell(0, 0),
            &mut resident,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();

        // Rows 0 and 1 of the arriving cell stand 24 units above the resident cell, so row 0 is
        // welded down onto it and row 1 stays where it is: the welded point's normal must lean
        // towards the higher row instead of keeping the flat normal it was loaded with.
        let mut arriving = untextured_fixture(2, 100.0);
        for column in 0..33 {
            arriving.heights[column] = 124.0;
            arriving.heights[33 + column] = 124.0;
        }
        validate_and_register_terrain_edges(
            exterior_cell(0, 1),
            &mut arriving,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();
        assert_eq!(metrics.terrain_seam_points_welded, 33);

        // (h(left) - h(right), h(down) - h(up), 2 * 128) at (x, y) = (9, 0) is (0, -24, 256):
        // normalized and scaled by 127 that is (0, -12, 126).
        assert_eq!(arriving.normals[9 * 3..9 * 3 + 3], [0, -12, 126]);
        assert_eq!(
            arriving.normals[(33 + 9) * 3..(33 + 9) * 3 + 3],
            [0, 0, 127],
            "a point that did not move keeps the normal it was loaded with"
        );
    }

    /// The rejections that are not seams: an edge of a length the neighbour cannot share, and a
    /// non-finite height - on either side of the comparison.
    #[test]
    fn still_rejects_wrong_edge_lengths_and_non_finite_heights() {
        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        let mut short_neighbor = untextured_fixture(1, 10.0);
        short_neighbor.width = 17;
        short_neighbor.height = 17;
        short_neighbor.heights = vec![10.0; 17 * 17];
        validate_and_register_terrain_edges(
            exterior_cell(0, 0),
            &mut short_neighbor,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();

        let mut full = untextured_fixture(2, 10.0);
        let error = validate_and_register_terrain_edges(
            exterior_cell(1, 0),
            &mut full,
            &mut continuity,
            &mut metrics,
        )
        .unwrap_err();
        assert!(error.contains("points"), "{error}");

        let mut continuity = TerrainContinuity::default();
        let mut metrics = StreamingMetrics::default();
        let mut resident = untextured_fixture(3, 10.0);
        validate_and_register_terrain_edges(
            exterior_cell(2, 0),
            &mut resident,
            &mut continuity,
            &mut metrics,
        )
        .unwrap();
        let mut broken = untextured_fixture(4, 10.0);
        broken.heights[4 * 33] = f32::NAN;
        let error = validate_and_register_terrain_edges(
            exterior_cell(3, 0),
            &mut broken,
            &mut continuity,
            &mut metrics,
        )
        .unwrap_err();
        assert!(error.contains("non-finite"), "{error}");

        // A value `f32::max` would quietly fold away is a rejection, not a weld.
        continuity.edges.insert(
            exterior_cell(3, 0),
            TerrainEdges {
                west: vec![f32::NAN; 33],
                east: vec![f32::NAN; 33],
                south: vec![10.0; 33],
                north: vec![10.0; 33],
            },
        );
        let mut arriving = untextured_fixture(5, 10.0);
        assert!(
            validate_and_register_terrain_edges(
                exterior_cell(4, 0),
                &mut arriving,
                &mut continuity,
                &mut metrics
            )
            .is_err()
        );
    }

    /// Defect B: a cell whose terrain is dry has nothing for a water plane to cover, and the
    /// plane - a second surface on the ground, aligned to the cell - shows as grid lines.
    #[test]
    fn spawns_a_water_plane_only_where_the_terrain_reaches_the_water() {
        let mut dry = untextured_fixture(1, 200.0);
        dry.water_height = Some(100.0);
        let mut wet = untextured_fixture(2, 200.0);
        wet.water_height = Some(100.0);
        wet.heights[16 * 33 + 16] = 99.0;
        let mut app = spawn_cells_app(
            vec![(exterior_cell(0, 0), dry), (exterior_cell(1, 0), wet)],
            StreamingMetrics::default(),
        );

        let mut roots = app.world_mut().query::<(&ExteriorCellGrid, &Children)>();
        let mut planes_per_cell = Vec::new();
        for (grid, children) in roots.iter(app.world()) {
            let planes = children
                .iter()
                .filter(|child| app.world().get::<WaterSurface>(*child).is_some())
                .count();
            planes_per_cell.push(((grid.0.x, grid.0.y), planes));
        }
        planes_per_cell.sort();
        assert_eq!(
            planes_per_cell,
            vec![((0, 0), 0), ((1, 0), 1)],
            "only the cell with a sample under the water gets a plane"
        );

        let mut surfaces = app.world_mut().query::<(&WaterSurface, &Transform)>();
        let heights: Vec<f32> = surfaces
            .iter(app.world())
            .map(|(_, transform)| transform.translation.y)
            .collect();
        assert_eq!(heights, vec![100.0]);
    }

    #[test]
    fn rejects_more_than_six_layers_per_quadrant() {
        let mut terrain = terrain_fixture(1, 0.0);
        terrain
            .layers
            .extend((1..=6).map(|layer| TerrainLayerSnapshot {
                texture_form_id: u32::from(layer) + 10,
                quadrant: 0,
                layer,
                is_base: false,
                weights: Vec::new(),
            }));
        assert!(quadrant_layers(&terrain, 0).is_err());
    }

    #[test]
    fn accepts_textureless_official_land_quadrant() {
        let mut terrain = terrain_fixture(1, 0.0);
        terrain.layers.clear();
        assert!(quadrant_layers(&terrain, 0).unwrap().is_empty());
    }

    #[test]
    fn validates_loaded_material_images_and_rejects_missing_required_texture() {
        let mut images = Assets::<Image>::default();
        let base_color = images.add(Image::new_fill(
            bevy::render::render_resource::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            bevy::render::render_resource::TextureDimension::D2,
            &[255, 255, 255, 255],
            bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        ));
        let material = StandardMaterial {
            base_color_texture: Some(base_color),
            ..default()
        };
        assert_eq!(validate_standard_material(&material, &images), Ok(1));

        let missing = StandardMaterial {
            normal_map_texture: Some(Handle::default()),
            ..default()
        };
        assert!(
            validate_standard_material(&missing, &images)
                .unwrap_err()
                .contains("normal image")
        );
    }

    #[test]
    fn rejects_invalid_alpha_and_culling_semantics() {
        let images = Assets::<Image>::default();
        assert!(
            validate_standard_material(
                &StandardMaterial {
                    alpha_mode: AlphaMode::Mask(f32::NAN),
                    ..default()
                },
                &images
            )
            .is_err()
        );
        assert!(
            validate_standard_material(
                &StandardMaterial {
                    double_sided: true,
                    ..default()
                },
                &images
            )
            .is_err()
        );
    }

    /// A `Transform` of a reference the acceptance run's stability scenario flew to far
    /// across Tamriel: at this distance one f32 step is ~0.008 units, which is larger than
    /// the bounds tolerance (at least 1e-3) of a typical model. A streamed instance is
    /// placed with a yaw and a position that is not exactly representable at that
    /// magnitude, both of which are what makes the world-space subtraction of the old
    /// measurement lose precision.
    fn far_reference() -> Transform {
        Transform::from_rotation(Quat::from_rotation_z(0.41)).with_translation(
            Vec3::new(120_000.0, -95_000.0, 8_000.0) + Vec3::new(1.37, -2.19, 0.43),
        )
    }

    #[derive(Resource, Default)]
    struct BoundsOutcome {
        expected: Option<ExpectedModelBounds>,
        bounds: Option<Result<SpawnedBounds, String>>,
        check: Option<Result<TransformValidationSummary, String>>,
    }

    /// Runs the real validation entry point with the same queries the streaming system
    /// hands it, capturing both the bounds it measured and its verdict.
    fn run_bounds_check(
        mut outcome: ResMut<BoundsOutcome>,
        roots: Query<(
            Entity,
            &Transform,
            &GlobalTransform,
            &WorldTransform,
            Option<&ExpectedModelBounds>,
        )>,
        transforms: Query<(&Transform, &GlobalTransform)>,
        children: Query<&Children>,
        primitives: RenderPrimitiveQuery,
        meshes: Res<Assets<Mesh>>,
    ) {
        let Ok((root, local, global, world_transform, expected)) = roots.single() else {
            panic!("the fixture must spawn exactly one model root");
        };
        outcome.expected = expected.copied();
        outcome.bounds = Some(spawned_relative_bounds(
            root,
            &children,
            &transforms,
            &primitives,
            &meshes,
        ));
        outcome.check = Some(validate_spawned_transforms_and_bounds(
            root,
            local,
            global,
            world_transform,
            expected,
            &children,
            &transforms,
            &primitives,
            &meshes,
        ));
    }

    /// Spawns what a converted asset produces: a reference root, a rotated and offset
    /// intermediate node, and a rotated, scaled and offset mesh node under it, with every
    /// `GlobalTransform` composed exactly as Bevy's transform propagation composes it. The
    /// returned bounds are the aggregate model-space bounds the converter would have
    /// written for the unshifted mesh; `mesh_shift` displaces the mesh afterwards to model
    /// a genuinely wrong hierarchy.
    fn spawn_bounds_fixture(
        world: &mut World,
        root_local: Transform,
        node_local: Transform,
        mesh_local: Transform,
        mesh_shift: Vec3,
        mesh: Handle<Mesh>,
    ) -> (Entity, ExpectedModelBounds) {
        let (min, max) = {
            let aabb = world
                .resource::<Assets<Mesh>>()
                .get(&mesh)
                .and_then(|mesh| mesh.compute_aabb())
                .expect("the fixture mesh has finite POSITION bounds");
            let center = Vec3::from(aabb.center);
            let half_extents = Vec3::from(aabb.half_extents);
            let converted = InstanceBounds::transformed(
                center - half_extents,
                center + half_extents,
                node_local.to_matrix() * mesh_local.to_matrix(),
            );
            (converted.min, converted.max)
        };
        let root_global = GlobalTransform::from(root_local.to_matrix());
        let root = world
            .spawn((
                root_local,
                root_global,
                WorldTransform(root_local.to_matrix()),
                ExpectedModelBounds { min, max },
            ))
            .id();
        let node_global = root_global.mul_transform(node_local);
        let node = world.spawn((node_local, node_global, ChildOf(root))).id();
        let mut mesh_local = mesh_local;
        mesh_local.translation += mesh_shift;
        world.spawn((
            mesh_local,
            node_global.mul_transform(mesh_local),
            Mesh3d(mesh),
            ChildOf(node),
        ));
        (root, ExpectedModelBounds { min, max })
    }

    fn bounds_case(root_local: Transform, mesh_shift: Vec3) -> BoundsOutcome {
        let mut app = App::new();
        let mut meshes = Assets::<Mesh>::default();
        let mesh = meshes.add(Cuboid::new(2.0, 4.0, 6.0));
        app.insert_resource(meshes);
        spawn_bounds_fixture(
            app.world_mut(),
            root_local,
            scene_node(),
            rotated_mesh_node(),
            mesh_shift,
            mesh,
        );
        app.init_resource::<BoundsOutcome>()
            .add_systems(Update, run_bounds_check);
        app.update();
        app.world_mut()
            .remove_resource::<BoundsOutcome>()
            .expect("the bounds check system ran")
    }

    /// The intermediate node a glTF scene hangs under the instance reference: it moves and
    /// rotates the model without carrying a mesh of its own.
    fn scene_node() -> Transform {
        Transform::from_rotation(Quat::from_rotation_x(-0.55) * Quat::from_rotation_z(0.3))
            .with_translation(Vec3::new(-1.23, 4.07, 0.61))
    }

    fn rotated_mesh_node() -> Transform {
        Transform::from_rotation(
            Quat::from_rotation_y(0.7) * Quat::from_rotation_x(0.35) * Quat::from_rotation_z(-0.2),
        )
        .with_translation(Vec3::new(3.37, 0.51, -2.19))
        .with_scale(Vec3::new(1.5, 0.75, 2.0))
    }

    #[test]
    fn bounds_check_ignores_the_reference_distance_from_the_origin() {
        let mut at_origin = bounds_case(Transform::default(), Vec3::ZERO);
        let mut far = bounds_case(far_reference(), Vec3::ZERO);

        let (at_origin_check, far_check) = (
            at_origin.check.take().expect("the bounds check ran"),
            far.check.take().expect("the bounds check ran"),
        );
        assert!(at_origin_check.is_ok(), "{at_origin_check:?}");
        assert!(far_check.is_ok(), "{far_check:?}");
        let expected = far.expected.expect("the fixture carries converted bounds");
        assert_eq!(expected, at_origin.expected.unwrap());
        let at_origin = at_origin
            .bounds
            .expect("bounds were measured")
            .expect("the hierarchy has bounded meshes");
        let far = far
            .bounds
            .expect("bounds were measured")
            .expect("the hierarchy has bounded meshes");
        assert_eq!(
            at_origin.nodes, 2,
            "the fixture has two descendants under the reference"
        );
        assert_eq!(
            far.nodes, 2,
            "the fixture has two descendants under the reference"
        );

        for (label, bounds) in [("origin", at_origin), ("far from the origin", far)] {
            assert!(
                bounds.min.abs_diff_eq(expected.min, 1.0e-5)
                    && bounds.max.abs_diff_eq(expected.max, 1.0e-5),
                "measured bounds at the {label} ({:?}..{:?}) differ from the converted bounds {:?}..{:?}",
                bounds.min,
                bounds.max,
                expected.min,
                expected.max
            );
        }
        assert!(
            far.min.abs_diff_eq(at_origin.min, 1.0e-5)
                && far.max.abs_diff_eq(at_origin.max, 1.0e-5),
            "measured bounds moved with the reference: {:?}..{:?} at the origin, {:?}..{:?} far from it",
            at_origin.min,
            at_origin.max,
            far.min,
            far.max
        );
    }

    #[test]
    fn bounds_check_rejects_a_moved_mesh_at_any_distance_from_the_origin() {
        for root_local in [Transform::default(), far_reference()] {
            let outcome = bounds_case(root_local, Vec3::new(0.5, 0.0, 0.0));
            let reason = outcome
                .check
                .expect("the bounds check ran")
                .expect_err("a mesh moved 0.5 units from the converted bounds must be rejected");
            assert!(
                reason.contains("spawned hierarchy bounds diverge from conversion"),
                "unexpected rejection reason at {root_local:?}: {reason}"
            );
        }
    }
}
