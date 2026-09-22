use crate::{
    config::EngineConfig,
    doors::{DoorDestination, LoadDoor},
    profiling::ProfilingState,
    render::{
        TerrainExtension, TerrainMaterial, WaterExtension, WaterMaterial, WaterReflectionTexture,
    },
    snow::{DirectionalSnow, DirectionalSnowCatalog, SnowMaterial, SnowMaterialCache},
    world::{
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{
            CELL_SIZE, CellRef, DistantTerrainRoot, ExpectedModelBounds, ExteriorCellGrid, FormId,
            InstanceBounds, MeshHandle, StreamedCellRoot, StreamingCamera, TerrainPatch,
            WaterSurface, WorldPosition, WorldTransform,
        },
        database::{
            AssetCatalog, CellDetail, CellKey, CellPayload, DatabaseRequest, DatabaseResponse,
            ReferenceRow, WorldDatabase,
        },
    },
};
use bevy::{
    asset::{LoadState, RecursiveDependencyLoadState, RenderAssetUsages},
    camera::primitives::MeshAabb,
    ecs::system::SystemParam,
    gltf::GltfExtras,
    image::{ImageFilterMode, ImageLoaderSettings, ImageSampler},
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error as StdError;
use std::time::Instant;

// Wall-clock spans can include a short OS scheduler preemption. Keep the raw maximum in metrics,
// but require a material overrun before classifying the frame as a commit-budget violation.
const COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS: u64 = 1_000;

/// How many terrain-only cells one frame may commit, on top of the full cells
/// [`EngineConfig::max_cell_commits_per_frame`] allows.
///
/// A terrain-only commit builds four small landscape meshes and their materials and spawns no
/// references, so it is a fraction of a full commit's cost. The default ring is 264 such cells: at
/// one commit per frame they would take 264 frames to appear, four seconds at sixty frames a
/// second, and a `--shots` pose that waits for streaming to settle would wait that out for every
/// pose - measured at 1.9 s for the ring to settle with this budget against 1.3 s without a ring
/// at all (`local/impl-021/after/shots.log`). The frame's own
/// [`EngineConfig::max_commit_micros_per_frame`] budget still applies, and a response that does
/// not fit is held for the next frame rather than dropped.
const MAX_TERRAIN_CELL_COMMITS_PER_FRAME: usize = 16;

fn commit_budget_exceeded(elapsed_micros: u64, budget_micros: u64) -> bool {
    elapsed_micros > budget_micros.saturating_add(COMMIT_BUDGET_SCHEDULER_TOLERANCE_MICROS)
}

/// The time since `started` in whole microseconds, saturating at `u64::MAX`.
fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
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
            // Empty until the startup system reads the database, and empty for every run whose
            // database has no snow tables: with the catalogue empty `spawn_cell` gives no
            // reference a `DirectionalSnow` and nothing about the spawned world changes.
            .init_resource::<DirectionalSnowCatalog>()
            .init_resource::<SnowMaterialCache>()
            .add_observer(mark_world_instance_ready)
            .add_systems(Startup, load_directional_snow_catalog)
            .add_plugins(crate::transition::TransitionPlugin)
            .add_systems(
                Update,
                (
                    plan_cells,
                    collect_cells,
                    track_asset_readiness,
                    apply_directional_snow,
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

/// Reads the snow tables - `matos` and the two snow columns of `statics` - before the first cell is
/// planned, so a reference already knows at spawn time whether its static is snow-covered.
///
/// [`StreamingPlugin`] is added with the engine's own configuration in the world, which is where the
/// assets directory comes from. A test app that adds the plugin without one keeps the empty
/// catalogue, and so does every run whose database predates the tables ([`DirectionalSnowCatalog`]).
fn load_directional_snow_catalog(
    config: Option<Res<EngineConfig>>,
    mut catalog: ResMut<DirectionalSnowCatalog>,
) {
    let Some(config) = config else {
        return;
    };
    *catalog = DirectionalSnowCatalog::open(&config.assets_dir.join("skyrim_world.db"));
}

#[derive(Resource, Default)]
pub struct StreamingWorld {
    generation: u64,
    cells: HashMap<CellKey, CellStatus>,
}

impl StreamingWorld {
    /// Whether `key` is streamed in as a whole cell and its root entity spawned.
    ///
    /// A terrain-only cell is not resident by this test: it holds no references, so it cannot be
    /// crossed into (a load door's destination), looked through (the portal's) or shot (a `--shots`
    /// pose). Every caller wants a cell it can stand in, and a destination a door pre-streamed is
    /// always full, so a terrain-only cell is never the answer they are waiting for.
    pub fn is_resident(&self, key: &CellKey) -> bool {
        matches!(
            self.cells.get(key),
            Some(CellStatus::Resident {
                detail: CellDetail::Full,
                ..
            })
        )
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

    /// Every pre-streamed cell, interiors and exteriors. A pre-streamed cell is always streamed in
    /// full: it is the space behind a door, which the camera may cross into at any moment.
    pub fn keys(&self) -> impl Iterator<Item = CellKey> + '_ {
        let interiors = self
            .interiors
            .iter()
            .map(|cell_id| CellKey::Interior(*cell_id));
        let exteriors = self
            .exteriors
            .iter()
            .map(|&(worldspace_id, grid_x, grid_y)| CellKey::Exterior {
                worldspace_id,
                grid_x,
                grid_y,
            });
        interiors.chain(exteriors)
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
    /// Resident cells that hold their landscape only, the distant ring ([`CellDetail::Terrain`]).
    pub terrain_cells: usize,
    pub peak_terrain_cells: usize,
    /// Cells reloaded at the other detail as the camera moved: a terrain-only cell that entered the
    /// full-detail grid, and a full cell that left it for the ring.
    pub cells_upgraded: u64,
    pub cells_downgraded: u64,
    /// Ring grids that held nothing: the ones a worldspace does not reach, and cells whose
    /// landscape the cell cache does not carry. Deliberately not part of [`Self::failed_cells`]:
    /// see the commit in [`collect_cells`](fn@collect_cells).
    pub ring_cells_empty: u64,
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
    /// References whose converted model is an empty glTF scene: an editor marker the converter
    /// dropped every shape of, so the model holds no render primitive and there is nothing to
    /// place, draw or bound. Counted here rather than in
    /// [`Self::transform_bounds_validation_failures`], which is a hard gate and must count only
    /// real conversion defects.
    pub empty_model_references: u64,
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

#[derive(Debug)]
enum CellStatus {
    /// A request is outstanding. The generation is what makes a late answer from a superseded
    /// request detectable, and it changes when a cell has to be loaded again at another detail.
    ///
    /// `replaced` is the root the cell is drawn by *now*, while it is loading at another detail:
    /// a cell that changes tier keeps drawing what it had until its replacement is committed, so a
    /// tier change is never a hole in the world. The old root is despawned in the same frame the
    /// new one is spawned, so the two are never drawn together and a cell still never has two
    /// terrains.
    Loading {
        generation: u64,
        detail: CellDetail,
        replaced: Option<Entity>,
    },
    Resident {
        root: Entity,
        detail: CellDetail,
    },
    /// The request failed and is not repeated at this detail. The detail it failed at is kept: a
    /// cell that held nothing as terrain-only is asked for again in full when the camera reaches
    /// it, because the full request asks a different question (see [`plan_cells`]).
    Failed {
        detail: CellDetail,
    },
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
    let wanted = wanted_cells(
        &active,
        config.stream_radius,
        config.terrain_radius,
        center,
        &prestream,
    );
    for (key, detail) in wanted {
        // What the cell is streamed as now, and the root that draws it.
        let (held, root) = match streaming.cells.get(&key) {
            None => (None, None),
            // A failed cell is asked for again only at a *different* tier than the one it failed
            // at, so a grid the worldspace does not cover - hundreds of them, in the ring - stays
            // failed instead of being re-requested every frame. A tier change asks a different
            // question: a cell that held nothing as terrain-only may still hold the references,
            // lights and doors the full request asks for, and a cell the camera is standing in must
            // be complete; a full request that failed may have failed on its references, and the
            // ring can still draw that cell's landscape. Each tier is tried once.
            Some(CellStatus::Failed { detail: failed }) => {
                if *failed == detail {
                    continue;
                }
                (None, None)
            }
            Some(CellStatus::Loading {
                detail, replaced, ..
            }) => (Some(*detail), *replaced),
            Some(CellStatus::Resident { root, detail }) => (Some(*detail), Some(*root)),
        };
        if held == Some(detail) {
            continue;
        }
        // The request goes first: a cell only gives up the root that draws it once its replacement
        // is on the worker's queue. A full queue (or a stopped worker) leaves the cell exactly as
        // it was - still drawn, still resident - rather than despawned with a status that says it
        // is there, which the lifecycle check reads as a missing root.
        streaming.generation = streaming.generation.wrapping_add(1);
        let generation = streaming.generation;
        if !database.try_request(DatabaseRequest::Load {
            generation,
            key,
            detail,
            queued_at: Instant::now(),
        }) {
            // No room for this request this frame. The wanted list is ordered nearest first, so
            // stopping keeps the cells closest to the camera at the head of the queue; the rest are
            // asked for again next frame.
            break;
        }
        match (held, detail) {
            (Some(CellDetail::Terrain), CellDetail::Full) => {
                metrics.cells_upgraded = metrics.cells_upgraded.saturating_add(1);
                profiler.event(format!("{key:?}"), "upgraded_to_full", None);
            }
            (Some(CellDetail::Full), CellDetail::Terrain) => {
                metrics.cells_downgraded = metrics.cells_downgraded.saturating_add(1);
                profiler.event(format!("{key:?}"), "downgraded_to_terrain", None);
            }
            _ => {}
        }
        metrics.requests_submitted += 1;
        profiler.increment("streaming/requests", 1);
        profiler.event(format!("{key:?}"), "requested", None);
        // A cell being loaded at another detail keeps drawing its old root until the commit swaps
        // it; a fresh cell has none.
        streaming.cells.insert(
            key,
            CellStatus::Loading {
                generation,
                detail,
                replaced: root,
            },
        );
    }
    streaming.cells.retain(|key, status| {
        let keep = cell_within_unload_radius(
            *key,
            &active,
            center,
            config.unload_radius,
            config.terrain_radius,
            &prestream,
        );
        if !keep {
            metrics.unloaded_cells += 1;
            continuity.edges.remove(key);
            profiler.event(format!("{key:?}"), "unloaded", None);
            // A loading cell that still draws the root it is replacing loses that root too: it is
            // the only root the cell has.
            match status {
                CellStatus::Resident { root, .. } => commands.entity(*root).despawn(),
                CellStatus::Loading { replaced, .. } => {
                    if let Some(root) = replaced {
                        commands.entity(*root).despawn();
                    }
                }
                CellStatus::Failed { .. } => {}
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
    metrics.terrain_cells = streaming
        .cells
        .values()
        .filter(|status| {
            matches!(
                status,
                CellStatus::Resident {
                    detail: CellDetail::Terrain,
                    ..
                }
            )
        })
        .count();
    metrics.peak_resident_cells = metrics.peak_resident_cells.max(metrics.resident_cells);
    metrics.peak_loading_cells = metrics.peak_loading_cells.max(metrics.loading_cells);
    metrics.peak_terrain_cells = metrics.peak_terrain_cells.max(metrics.terrain_cells);
    profiler.set_gauge("streaming/resident_cells", metrics.resident_cells as f64);
    profiler.set_gauge("streaming/loading_cells", metrics.loading_cells as f64);
    profiler.set_gauge("streaming/terrain_cells", metrics.terrain_cells as f64);
    profiler.record_elapsed("streaming/plan_cells", plan_started);
}

/// The two read-only catalogues a cell commit reads: the landscape and water texture paths
/// ([`AssetCatalog`]) and the projected snow materials ([`DirectionalSnowCatalog`]).
///
/// They travel as one because a system function takes at most sixteen parameters, and
/// [`collect_cells`] - the commit path - is at that limit.
#[derive(SystemParam)]
struct CommitCatalogs<'w> {
    assets: Res<'w, AssetCatalog>,
    snow: Res<'w, DirectionalSnowCatalog>,
}

#[allow(clippy::too_many_arguments)]
fn collect_cells(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    cache: Res<CellCache>,
    origin: Res<RenderOrigin>,
    asset_server: Res<AssetServer>,
    catalogs: CommitCatalogs,
    reflection: Res<WaterReflectionTexture>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
    mut streaming: ResMut<StreamingWorld>,
    mut continuity: ResMut<TerrainContinuity>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
    mut deferred: Local<VecDeque<DatabaseResponse>>,
) {
    let frame_commit_started = Instant::now();
    // Everything the worker has answered this frame, behind whatever the last frame had no room
    // for. Answers are taken out of the channel here rather than inside the commit loop, so a
    // response held over is counted - and timed - exactly once.
    let mut pending = std::mem::take(&mut *deferred);
    while let Some(response) = database.try_response() {
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
        pending.push_back(response);
    }
    let mut commits_this_frame = 0u64;
    let mut full_commits = 0usize;
    let mut terrain_commits = 0usize;
    while let Some(response) = pending.pop_front() {
        let Some(CellStatus::Loading {
            generation,
            detail,
            replaced,
        }) = streaming.cells.get(&response.key)
        else {
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_discarded", None);
            continue;
        };
        if *generation != response.generation || *detail != response.detail {
            metrics.stale_responses += 1;
            profiler.event(format!("{:?}", response.key), "stale_generation", None);
            continue;
        }
        let (detail, replaced) = (*detail, *replaced);
        // The cell was drawn by an older root while this answer was in flight. Whatever happens to
        // the answer, what that root registered describes terrain that stops being drawn in this
        // frame: the new terrain registers its own edges in the validation below, and a failed
        // answer removes the root.
        if replaced.is_some() {
            continuity.edges.remove(&response.key);
        }
        // A full cell commits at the one-per-frame cap it always had: the references, their assets
        // and their lights are what that cap protects. A terrain-only cell is a fraction of that
        // work, so it commits under its own count and the frame's commit budget. What does not fit
        // is held - with everything behind it, keeping the order - for the next frame, rather than
        // re-requested or dropped.
        let over_budget = match detail {
            CellDetail::Full => full_commits >= config.max_cell_commits_per_frame,
            CellDetail::Terrain => {
                terrain_commits >= MAX_TERRAIN_CELL_COMMITS_PER_FRAME
                    || elapsed_micros(frame_commit_started) >= config.max_commit_micros_per_frame
            }
        };
        if over_budget {
            deferred.push_back(response);
            deferred.extend(pending.drain(..));
            break;
        }
        let commit_started = std::time::Instant::now();
        match response.result {
            Ok(payload) => {
                let mut terrain = cache.terrain(payload.cell_id);
                if let Some(terrain) = terrain.as_mut() {
                    let validation =
                        validate_terrain_snapshot(terrain, &catalogs.assets).and_then(|()| {
                            validate_and_register_terrain_edges(
                                payload.key,
                                terrain,
                                &mut continuity,
                                &mut metrics,
                            )
                        });
                    if let Err(reason) = validation {
                        if detail == CellDetail::Terrain {
                            // A ring cell whose landscape does not validate either is empty space
                            // for the ring, and counted like one: a few hundred ring cells are
                            // requested at a time, and a bad one must not read as a cell failure
                            // the camera can stand in. If the data really is broken, the full
                            // request that follows the camera into the cell validates the same
                            // landscape again and reports it then.
                            debug!(
                                cell = format_args!("{:08X}", payload.cell_id),
                                %reason,
                                "terrain-only cell's landscape failed validation"
                            );
                            metrics.ring_cells_empty = metrics.ring_cells_empty.saturating_add(1);
                            profiler.increment("streaming/ring_cells_invalid_terrain", 1);
                        } else {
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
                        }
                        fail_cell(
                            &mut commands,
                            &mut streaming,
                            response.key,
                            detail,
                            replaced,
                        );
                        continue;
                    }
                }
                if detail == CellDetail::Terrain && terrain.is_none() {
                    // A terrain-only cell with no landscape: the grid has a cell but the cell cache
                    // holds no terrain for it (a persistent cell, or terrain outside the cache).
                    // There is nothing to draw in it and nothing to enter, so it is not held: an
                    // empty root would count against the ring's budget and draw nothing. This is
                    // not a terrain validation failure - the cell has no terrain to be wrong.
                    debug!(
                        cell = format_args!("{:08X}", payload.cell_id),
                        "terrain-only cell has no landscape in the cell cache"
                    );
                    metrics.ring_cells_empty = metrics.ring_cells_empty.saturating_add(1);
                    profiler.increment("streaming/ring_cells_without_terrain", 1);
                    fail_cell(
                        &mut commands,
                        &mut streaming,
                        response.key,
                        detail,
                        replaced,
                    );
                    continue;
                }
                // The replacement is committed: the root the cell kept drawing goes now, in the
                // same frame its successor is spawned, so the two are never both drawn and the
                // cell never has two terrains.
                if let Some(replaced) = replaced {
                    commands.entity(replaced).despawn();
                }
                let root = spawn_cell(
                    &mut commands,
                    &asset_server,
                    &catalogs.assets,
                    &catalogs.snow,
                    &reflection,
                    &mut meshes,
                    &mut terrain_materials,
                    &mut water_materials,
                    origin.0,
                    payload,
                    terrain,
                    detail,
                    &mut profiler,
                );
                streaming
                    .cells
                    .insert(response.key, CellStatus::Resident { root, detail });
            }
            Err(error) => {
                // A terrain-only request that fails is empty space, not a failure: the ring covers
                // hundreds of grids around the camera, and the corners of a worldspace beyond its
                // cells, or a grid whose cell the cache has no landscape for, are what most of the
                // answers are (the transition layer's own fixture has 624 of them). Counting them
                // as cell failures would drown the number that matters - a full cell the camera
                // stands in failing to load - so they are counted on their own.
                if detail == CellDetail::Terrain {
                    debug!(?response.key, %error, "the ring holds nothing at this grid");
                    metrics.ring_cells_empty = metrics.ring_cells_empty.saturating_add(1);
                    profiler.increment("streaming/ring_cells_without_a_cell", 1);
                } else {
                    debug!(?response.key, %error, "cell could not be streamed");
                    metrics.failed_cells += 1;
                    profiler.increment("streaming/failed_cells", 1);
                }
                fail_cell(
                    &mut commands,
                    &mut streaming,
                    response.key,
                    detail,
                    replaced,
                );
            }
        }
        let commit_micros = commit_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        metrics.max_commit_micros = metrics.max_commit_micros.max(commit_micros);
        commits_this_frame = commits_this_frame.saturating_add(1);
        match detail {
            CellDetail::Full => full_commits += 1,
            CellDetail::Terrain => terrain_commits += 1,
        }
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

/// Marks a cell failed, dropping the root it kept drawn while its replacement was loading.
///
/// A failed replacement tears the cell's old root down with it: what is drawn would no longer be
/// what the cell claims to be - a full cell's references and lights were the reason for the
/// request, and a cell that has left the ring must not keep them - and a root nothing tracks is
/// exactly what the lifecycle check reports as orphaned.
fn fail_cell(
    commands: &mut Commands,
    streaming: &mut StreamingWorld,
    key: CellKey,
    detail: CellDetail,
    replaced: Option<Entity>,
) {
    if let Some(replaced) = replaced {
        commands.entity(replaced).despawn();
    }
    streaming.cells.insert(key, CellStatus::Failed { detail });
}

/// The cells the active cell wants resident, and what it wants of each: the interior the camera is
/// in, or the exterior grid of `stream_radius` around it in full and the terrain-only ring out to
/// `terrain_radius` beyond — plus everything the transition layer pre-streamed for a nearby load
/// door, which is what makes a crossing seamless.
///
/// Ordered for the worker's queue, which takes them in the order they are asked for:
///
/// 1. the cells a nearby door pre-streamed, wherever they are - the space behind a door the camera
///    may cross into at any moment, which is worth more than any view of the landscape;
/// 2. the interior the camera is inside, if it is in one - the cell it stands in;
/// 3. the exterior grid, nearest first and full before terrain-only, so the world appears from the
///    camera outwards.
fn wanted_cells(
    active: &ActiveCell,
    stream_radius: i32,
    terrain_radius: i32,
    center: IVec2,
    prestream: &PrestreamCells,
) -> Vec<(CellKey, CellDetail)> {
    let mut wanted = Vec::new();
    // Everything a nearby door pre-streamed, wherever it is: a destination in another worldspace,
    // an interior, or a cell the grid below reaches only as terrain. Always in full: it is the
    // space behind a door, which the camera may cross into at any moment.
    for key in prestream.keys() {
        wanted.push((key, CellDetail::Full));
    }
    if let Some(cell_id) = active.interior {
        // An interior is the whole plan: it is one cell, and no exterior is streamed while the
        // camera is inside one.
        wanted.push((CellKey::Interior(cell_id), CellDetail::Full));
    } else {
        let radius = terrain_radius.max(stream_radius);
        let mut grid = Vec::new();
        for y in -radius..=radius {
            for x in -radius..=radius {
                let key = CellKey::Exterior {
                    worldspace_id: active.worldspace_id,
                    grid_x: center.x + x,
                    grid_y: center.y + y,
                };
                // A cell a door pre-streamed is already in the list, in full: it is not the grid's
                // to decide the tier of.
                if prestream.contains(&key) {
                    continue;
                }
                if let Some(detail) = wanted_detail(
                    active,
                    stream_radius,
                    terrain_radius,
                    center,
                    key,
                    prestream,
                ) {
                    grid.push((x.abs().max(y.abs()), key, detail));
                }
            }
        }
        grid.sort_by_key(|(distance, _, detail)| {
            (matches!(detail, CellDetail::Terrain), *distance)
        });
        wanted.extend(grid.into_iter().map(|(_, key, detail)| (key, detail)));
    }
    wanted
}

/// What the plan wants done with one cell: nothing, its terrain, or the whole cell.
///
/// A cell is full inside the inner grid, terrain-only in the ring around it, and not wanted beyond
/// the ring — except that a pre-streamed cell is always full, however far away it is: it is the
/// space behind a door the camera is about to cross into, and an interior is never terrain-only.
fn wanted_detail(
    active: &ActiveCell,
    stream_radius: i32,
    terrain_radius: i32,
    center: IVec2,
    key: CellKey,
    prestream: &PrestreamCells,
) -> Option<CellDetail> {
    if prestream.contains(&key) {
        return Some(CellDetail::Full);
    }
    match (key, active.interior) {
        (CellKey::Interior(cell_id), Some(interior)) if cell_id == interior => {
            Some(CellDetail::Full)
        }
        (CellKey::Interior(_), _) => None,
        (CellKey::Exterior { .. }, Some(_)) => None,
        (
            CellKey::Exterior {
                worldspace_id,
                grid_x,
                grid_y,
            },
            None,
        ) => {
            if worldspace_id != active.worldspace_id {
                return None;
            }
            let distance = (grid_x - center.x).abs().max((grid_y - center.y).abs());
            if distance <= stream_radius {
                Some(CellDetail::Full)
            } else if distance <= terrain_radius {
                Some(CellDetail::Terrain)
            } else {
                None
            }
        }
    }
}

/// Whether a streamed cell survives this frame: a cell the transition layer pre-streamed, the
/// active interior, or an exterior of the active worldspace within the outer of the two radii.
///
/// `radius` is the full-detail band's, which is what it was before the ring existed;
/// `terrain_radius + 1` is the ring's, one cell further out than the ring itself so that its edge
/// does not pop in and out as the camera crosses a cell boundary. The wider of the two decides:
/// beyond `radius` inside the ring only a terrain-only cell can exist, because the plan demotes a
/// full cell the moment it leaves the inner grid, so the extra cell this keeps is a ring cell.
///
/// So an interior unloads as soon as it is neither active nor pre-streamed, no exterior survives
/// while an interior is active, and a crossing into another worldspace drops the one just left
/// instead of holding its cells at whatever grid the destination happens to share.
fn cell_within_unload_radius(
    key: CellKey,
    active: &ActiveCell,
    center: IVec2,
    radius: i32,
    terrain_radius: i32,
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
            let limit = radius.max(terrain_radius.saturating_add(1));
            worldspace_id == active.worldspace_id
                && (grid_x - center.x).abs() <= limit
                && (grid_y - center.y).abs() <= limit
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
    snow: &DirectionalSnowCatalog,
    reflection: &WaterReflectionTexture,
    meshes: &mut Assets<Mesh>,
    terrain_materials: &mut Assets<TerrainMaterial>,
    water_materials: &mut Assets<WaterMaterial>,
    origin: IVec2,
    payload: CellPayload,
    terrain: Option<TerrainSnapshot>,
    detail: CellDetail,
    profiler: &mut ProfilingState,
) -> Entity {
    let spawn_started = Instant::now();
    let reference_count = payload.references.len();
    let root_translation = cell_translation(payload.key, origin);
    let mut root_commands = commands.spawn((
        Name::new(format!("Cell {:08X}", payload.cell_id)),
        CellRef(payload.cell_id),
        Transform::from_translation(root_translation),
        Visibility::default(),
    ));
    match detail {
        CellDetail::Full => {
            root_commands.insert(StreamedCellRoot);
        }
        CellDetail::Terrain => {
            root_commands.insert(DistantTerrainRoot);
        }
    }
    if let CellKey::Exterior { grid_x, grid_y, .. } = payload.key {
        root_commands.insert(ExteriorCellGrid(IVec2::new(grid_x, grid_y)));
    }
    // The exact key, so portal isolation can tell two worldspaces apart at the same grid.
    root_commands.insert(crate::portal::StreamedCellKey(payload.key));
    let root = root_commands.id();
    commands.entity(root).with_children(|parent| {
        if let Some(terrain) = terrain {
            for quadrant in 0..4 {
                let started = Instant::now();
                let mesh = build_terrain_quadrant_mesh(&terrain, quadrant)
                    .expect("validated terrain must build");
                profiler.record_elapsed("streaming/terrain_mesh", started);
                let (extension, layer_images) =
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
                let mut patch = parent.spawn((
                    Name::new(format!("Terrain quadrant {quadrant}")),
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(material),
                    Transform::default(),
                    TerrainPatch,
                    Visibility::Hidden,
                    PendingTerrainProfile {
                        cell_id: terrain.cell_id,
                        quadrant,
                        images: layer_images,
                    },
                ));
                if detail == CellDetail::Terrain {
                    // A ring cell casts no shadow: its own shadows fall on the ring, tens of
                    // thousands of units away and past the last shadow cascade, so nobody can see
                    // them - and every caster is another draw per frame. Keeping the ring out of
                    // the shadow pass measured 30 -> 59 frames a second at a twelve-cell ring with
                    // the whole Pale in view, which is what the default's cell count is chosen
                    // against (`crate::config::DEFAULT_TERRAIN_RADIUS`). Receiving light is
                    // untouched: the ring is shaded by the sun like everything else.
                    patch.insert(bevy::light::NotShadowCaster);
                }
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
        // A terrain-only cell is landscape seen from a distance and never entered. Its references
        // are not spawned - the database did not even read them - so nothing in it can be drawn,
        // lit, walked through or opened. Its terrain and water plane above are built by the same
        // code the full path uses, so the two tiers meet with the same mesh and the same materials.
        if detail == CellDetail::Full {
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
                // A static whose `DNAM` names a `MATO` draws its model with a projected snow
                // material. The reference carries the coverage from here; the swap itself waits for
                // the scene to be validated (`apply_directional_snow`). An ordinary static - the
                // overwhelming majority, and every one in a database without the tables - gets
                // nothing, and its meshes stay exactly as the model published them.
                if let Some(coverage) = snow.coverage_for(reference.base_form_id) {
                    entity.insert(DirectionalSnow {
                        static_form_id: reference.base_form_id,
                        coverage,
                    });
                }
                if let Some(door) = load_door(&reference) {
                    entity.insert(door);
                }
                // A child of the reference, so the light sits where the reference is and follows it
                // through a render-origin rebase - and, because it is a descendant of the cell root,
                // through the portal isolation that walks a cell's hierarchy.
                if let Some(light) = reference.light.as_ref().and_then(|row| {
                    crate::lights::point_light(row, reference.light_radius_override)
                }) {
                    // A reference without a model has no visibility components, so its light child
                    // could never become visible: Bevy warned (B0004) and extract_lights dropped every
                    // such light. The reference needs Visibility for the hierarchy to propagate.
                    entity.insert(Visibility::default());
                    entity.with_child((
                        Name::new(format!("Light {:08X}", reference.form_id)),
                        light,
                        crate::lights::SkyrimLight {
                            form_id: reference.form_id,
                            cell_id: reference.cell_id,
                        },
                    ));
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
/// exterior, and its absence makes the cell an interior. `auto_load` comes from the base record
/// ([`ReferenceRow::auto_load`]), which is how an invisible `AutoLoadDoor01` marker is told from a
/// door the player has to open. `outward` comes from the link that leads back into this door
/// ([`DoorLinkRow::return_arrival`]), which is what says which side of the door the player arrives
/// on, whatever the door model's own axes are.
pub(crate) fn load_door(reference: &ReferenceRow) -> Option<LoadDoor> {
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
    let outward = door.return_arrival.and_then(|(position, rotation)| {
        crate::doors::outward_from_return_link(reference.position, position, rotation)
    });
    Some(LoadDoor {
        ref_id: reference.form_id,
        destination,
        label: door.label.clone(),
        auto_load: reference.auto_load,
        outward,
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

/// Every streamed cell root, in either tier: a full cell ([`StreamedCellRoot`]) or one of the
/// ring's terrain-only cells ([`DistantTerrainRoot`]). Both carry the cell they draw, so the
/// lifecycle check counts them together against the plan.
type CellRootsQuery<'world, 'state> = Query<
    'world,
    'state,
    (Entity, &'static CellRef, Option<&'static ExteriorCellGrid>),
    Or<(With<StreamedCellRoot>, With<DistantTerrainRoot>)>,
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
            // An empty model - the converter's scene for an editor marker - spawns no render
            // primitive at all, so there is nothing to place, draw or validate: skip it and count
            // it separately instead of failing the run's bounds gate. Everything else about the
            // reference stays: it keeps its transform, its load door and its light, and its scene
            // is left alone (it is empty; there is nothing in it to hide).
            if transform_summary.empty_model {
                metrics.empty_model_references = metrics.empty_model_references.saturating_add(1);
                profiler.increment("assets/empty_model_references", 1);
                profiler.event(&pending.path, "empty_model", None);
                commands.entity(entity).remove::<PendingAssetProfile>();
                completed_this_scan += 1;
                continue;
            }
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

/// Gives every mesh of a reference whose static carries a `MATO` the projected snow material of
/// [`crate::snow`].
///
/// It runs directly after [`track_asset_readiness`], and only for a reference whose
/// `PendingAssetProfile` has gone, because that pass is what validates a loaded material - its alpha
/// mode, its culling, its emissive, the colour space of each of its textures - through
/// `MeshMaterial3d<StandardMaterial>`. A mesh already moved to the snow material would reach the
/// validation as one with no material at all, and a scene swapped before it is validated would fail
/// its own gate. Swapping after it also means the snow base is the material the glTF handler in
/// `render.rs` published - blend pair and engine-scale emissive included - rather than the raw glTF
/// one, which is the same thing every other mesh in the cell is drawn with.
///
/// The reference carries its coverage from spawn time ([`DirectionalSnow`]); this system only moves
/// meshes onto it. A reference whose scene holds no mesh - an editor marker's empty one, or a model
/// that failed to load - is left with what it has and simply stops being asked about.
#[allow(clippy::too_many_arguments)]
fn apply_directional_snow(
    mut commands: Commands,
    materials: Res<Assets<StandardMaterial>>,
    // `None` in an app that never registered the snow material's asset collection - the streaming
    // and transition tests build their worlds without the renderer's plugins. There is nothing to
    // swap a mesh onto there, and the reference keeps the material its scene was validated with.
    snow_materials: Option<ResMut<Assets<SnowMaterial>>>,
    mut cache: ResMut<SnowMaterialCache>,
    references: Query<(Entity, &DirectionalSnow), Without<PendingAssetProfile>>,
    children: Query<&Children>,
    primitives: Query<&MeshMaterial3d<StandardMaterial>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let Some(mut snow_materials) = snow_materials else {
        return;
    };
    if references.is_empty() {
        return;
    }
    let started = Instant::now();
    let mut swapped = 0u64;
    for (entity, snow) in &references {
        for descendant in children.iter_descendants(entity) {
            let Ok(material) = primitives.get(descendant) else {
                continue;
            };
            // `None` when the standard material asset is gone, which is a scene that failed its
            // load: there is nothing to extend, and the mesh keeps what it has.
            let Some(handle) =
                cache.material_for(material.0.id(), snow, &materials, &mut snow_materials)
            else {
                continue;
            };
            commands
                .entity(descendant)
                .remove::<MeshMaterial3d<StandardMaterial>>()
                .insert(MeshMaterial3d(handle));
            swapped += 1;
        }
        commands.entity(entity).remove::<DirectionalSnow>();
    }
    profiler.increment("streaming/snow_meshes", swapped);
    profiler.record_elapsed("streaming/directional_snow", started);
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
    /// The model is an empty glTF scene - an editor marker the converter dropped every shape of -
    /// so it carries no converted bounds and the spawned hierarchy holds no render primitive.
    /// Nothing was drawn, so the reference is counted in
    /// [`StreamingMetrics::empty_model_references`] rather than as a validated instance.
    empty_model: bool,
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
    let Some(expected) = expected else {
        // A model the converter wrote no aggregate bounds for is an empty scene: an editor marker
        // whose every shape it dropped (`converter/src/mesh.rs`). The asset itself decides - an
        // invisible marker has nothing to place, draw or bound, so it is not a failure - and it
        // only counts as empty when the spawned scene really holds no render primitive. A scene
        // that does hold one and still arrived without bounds is a conversion defect, and stays
        // as fatal as any other.
        let primitives = spawned_primitive_count(root, children, primitives);
        return if primitives == 0 {
            Ok(TransformValidationSummary {
                nodes: 0,
                empty_model: true,
            })
        } else {
            Err(format!(
                "converted model has no validated aggregate bounds; reconvert the asset (its spawned scene holds {primitives} render primitives)"
            ))
        };
    };
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
        empty_model: false,
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

/// How many render primitives (`Mesh3d` entities) the spawned hierarchy under `root` holds, at any
/// depth. Zero is an empty scene - what the converter writes for an editor marker - and is read
/// from the asset, never from the model's name or path.
fn spawned_primitive_count(
    root: Entity,
    children: &Query<&Children>,
    primitives: &RenderPrimitiveQuery,
) -> usize {
    children
        .iter_descendants(root)
        .filter(|descendant| primitives.contains(*descendant))
        .count()
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
        // `Add` and `Multiply` are what a shape whose `NiAlphaProperty` blends additively
        // (`SRC_ALPHA`/`ONE`) or multiplicatively (`ZERO`/`SRC_COLOR`) is given by the glTF
        // material handler in `render.rs`, so they are as expected here as `Blend`.
        AlphaMode::Opaque | AlphaMode::Blend | AlphaMode::Add | AlphaMode::Multiply => {}
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

/// The dense weight field of one quadrant: one `17x17` grid per overlay layer, indexed by the raw
/// `VTXT` vertex value, in the same order as [`quadrant_layers`] (base first, so slot 0 is the
/// first overlay). Unlisted grid points are opacity 0. Both the mesh's packed vertex weights and
/// the material's weight images are built from this, so they cannot drift apart.
pub(crate) fn quadrant_overlay_weights(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<Vec<Vec<f32>>, String> {
    let layers = quadrant_layers(terrain, quadrant)?;
    let mut overlay_weights = vec![vec![0.0f32; 17 * 17]; layers.len().saturating_sub(1)];
    for (slot, layer) in layers.iter().skip(1).enumerate() {
        for &(vertex, opacity) in &layer.weights {
            overlay_weights[slot][usize::from(vertex)] = opacity;
        }
    }
    Ok(overlay_weights)
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
    let overlay_weights = quadrant_overlay_weights(terrain, quadrant)?;
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    if width != 33 || height != 33 || terrain.heights.len() != width * height {
        return Err("terrain must contain a complete 33x33 height field".to_owned());
    }
    let step_x = CELL_SIZE / (width - 1) as f32;
    let step_z = CELL_SIZE / (height - 1) as f32;
    let origin_x = usize::from(quadrant % 2) * 16;
    let origin_y = usize::from(quadrant / 2) * 16;
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
            // The packed vertex weights are the fallback for materials with no weight field (the
            // synthetic fixtures, which are built without a LAND snapshot): weights 1-3 as a unit
            // direction plus its magnitude in `w`, weights 4-5 in the second UV set. Bevy
            // re-normalizes `world_tangent.xyz` in the vertex shader, so this carrier sharpens
            // every transition (`0.25` where the true interpolated weight is `0.5`); the streamed
            // path reads `TerrainExtension`'s weight field instead.
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
    roots: CellRootsQuery,
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
            CellStatus::Resident { root, .. } => Some(*root),
            // A cell loading at another detail still draws the root it is replacing: it is a root
            // the plan accounts for, not an orphan.
            CellStatus::Loading { replaced, .. } => *replaced,
            CellStatus::Failed { .. } => None,
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
    //
    // The radius is the wider of the two the plan keeps cells by, for a cell of either tier: a full
    // cell outside the inner grid is on its way to being terrain-only, and that is a *request* the
    // worker's queue may not have room for in the frame the camera moved (`plan_cells` asks again
    // next frame), so a full cell a few cells out is a cell the plan wants and has asked for, not
    // one it has stopped wanting. What this catches is a cell of another worldspace, a cell beyond
    // every band, and one the plan's own retain should have dropped.
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
            let limit = config
                .unload_radius
                .max(config.terrain_radius.saturating_add(1));
            streaming
                .cells
                .iter()
                .filter(|(key, status)| {
                    let key = **key;
                    matches!(status, CellStatus::Resident { .. })
                        && !prestream.contains(&key)
                        && matches!(key, CellKey::Exterior { worldspace_id, grid_x, grid_y }
                            if worldspace_id == active.worldspace_id
                                && ((grid_x - center.x).abs() > limit
                                    || (grid_y - center.y).abs() > limit))
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
    use crate::{
        snow::{SnowCoverage, snow_material_object_1p},
        world::database::DoorLinkRow,
    };
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
                &DirectionalSnowCatalog::default(),
                &reflection,
                &mut meshes,
                &mut terrain_materials,
                &mut water_materials,
                IVec2::ZERO,
                payload,
                Some(terrain.clone()),
                CellDetail::Full,
                &mut profiler,
            );
            streaming.cells.insert(
                *key,
                CellStatus::Resident {
                    root,
                    detail: CellDetail::Full,
                },
            );
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
            .init_resource::<DirectionalSnowCatalog>()
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
            0,
            &none
        ));
        assert!(!cell_within_unload_radius(
            exterior(60, 8, -2),
            &outside,
            center,
            3,
            0,
            &none
        ));
        assert!(
            !cell_within_unload_radius(exterior(614, 4, -2), &outside, center, 3, 0, &none),
            "the same grid in another worldspace belongs to the worldspace just left"
        );
        assert!(
            !cell_within_unload_radius(exterior(60, 4, -2), &inside, center, 3, 0, &none),
            "no exterior is streamed while an interior is active"
        );
        assert!(cell_within_unload_radius(
            CellKey::Interior(99),
            &inside,
            center,
            0,
            0,
            &none
        ));
        assert!(
            !cell_within_unload_radius(CellKey::Interior(98), &inside, center, 0, 0, &none),
            "an interior that is neither active nor pre-streamed unloads"
        );
        assert!(!cell_within_unload_radius(
            CellKey::Interior(99),
            &outside,
            center,
            0,
            0,
            &none
        ));

        // The ring's own band: a cell one past the ring's edge survives, the next one does not, so
        // the edge of the ring does not flicker as the camera crosses a cell boundary.
        let terrain_radius = 12;
        assert!(cell_within_unload_radius(
            exterior(60, 4 + terrain_radius + 1, -2),
            &outside,
            center,
            3,
            terrain_radius,
            &none
        ));
        assert!(!cell_within_unload_radius(
            exterior(60, 4 + terrain_radius + 2, -2),
            &outside,
            center,
            3,
            terrain_radius,
            &none
        ));
        assert!(
            cell_within_unload_radius(
                exterior(60, 4 + 7, -2),
                &outside,
                center,
                3,
                terrain_radius,
                &none
            ),
            "a ring cell seven cells out is nobody's unload radius but the ring's"
        );

        // A pre-streamed destination survives anywhere until the request stops.
        let mut prestream = PrestreamCells::default();
        prestream.request_interior(98);
        prestream.request_exterior(614, IVec2::new(5, 4));
        assert!(cell_within_unload_radius(
            CellKey::Interior(98),
            &inside,
            center,
            0,
            0,
            &prestream
        ));
        assert!(cell_within_unload_radius(
            exterior(614, 5, 4),
            &inside,
            center,
            3,
            0,
            &prestream
        ));
        assert!(!cell_within_unload_radius(
            CellKey::Interior(97),
            &inside,
            center,
            0,
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
            wanted_cells(&inside, 1, 1, center, &none),
            vec![(CellKey::Interior(99), CellDetail::Full)],
            "an interior is the whole plan, and there is no exterior grid while it is active"
        );

        let blackreach = ActiveCell {
            worldspace_id: 614,
            interior: None,
        };
        // With no ring, the plan is exactly the grid `stream_radius` always asked for.
        let wanted = wanted_cells(&blackreach, 1, 1, center, &none);
        assert_eq!(wanted.len(), 9);
        assert!(wanted.iter().all(|(key, detail)| matches!(
            key,
            CellKey::Exterior {
                worldspace_id: 614,
                ..
            }
        ) && *detail == CellDetail::Full));
        assert!(wanted.contains(&(
            CellKey::Exterior {
                worldspace_id: 614,
                grid_x: 5,
                grid_y: -3,
            },
            CellDetail::Full
        )));

        // Pre-streamed destinations are wanted on top of the active cell's own plan, and an
        // exterior destination too far from the camera for the grid is still wanted in full.
        let mut prestream = PrestreamCells::default();
        prestream.request_interior(98);
        prestream.request_exterior(60, IVec2::new(19, 18));
        let wanted = wanted_cells(&inside, 1, 1, center, &prestream);
        assert!(wanted.contains(&(CellKey::Interior(99), CellDetail::Full)));
        assert!(wanted.contains(&(CellKey::Interior(98), CellDetail::Full)));
        assert!(wanted.contains(&(
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 19,
                grid_y: 18,
            },
            CellDetail::Full
        )));
    }

    /// The ring classification, as a pure function of the camera cell and the two radii: the inner
    /// grid in full, the ring around it terrain-only, and nothing beyond. Its own cell is the
    /// nearest full one, the cell diagonally inside the grid corner counts as inside it, and a
    /// pre-streamed cell outside the inner grid is still wanted in full - it is the space behind a
    /// door, which has to be complete before the camera crosses.
    #[test]
    fn classifies_cells_into_the_inner_grid_the_ring_and_nothing() {
        let center = IVec2::new(4, -2);
        let outside = ActiveCell {
            worldspace_id: 60,
            interior: None,
        };
        let exterior = |grid_x, grid_y| CellKey::Exterior {
            worldspace_id: 60,
            grid_x,
            grid_y,
        };
        let none = PrestreamCells::default();
        let detail = |key| wanted_detail(&outside, 2, 5, center, key, &none);

        assert_eq!(detail(exterior(4, -2)), Some(CellDetail::Full));
        assert_eq!(detail(exterior(2, -4)), Some(CellDetail::Full));
        assert_eq!(
            detail(exterior(6, 0)),
            Some(CellDetail::Full),
            "the corner of the 5x5 grid is inside it"
        );
        assert_eq!(detail(exterior(7, -2)), Some(CellDetail::Terrain));
        assert_eq!(detail(exterior(4, 3)), Some(CellDetail::Terrain));
        assert_eq!(
            detail(exterior(9, -7)),
            Some(CellDetail::Terrain),
            "the ring's own corner is in the ring"
        );
        assert_eq!(detail(exterior(10, -2)), None);
        assert_eq!(detail(exterior(4, 4)), None);
        assert_eq!(detail(exterior(4, 8)), None);
        assert_eq!(
            detail(CellKey::Interior(99)),
            None,
            "an interior is not streamed while an exterior is active"
        );
        assert_eq!(
            wanted_detail(
                &outside,
                2,
                5,
                center,
                CellKey::Exterior {
                    worldspace_id: 614,
                    grid_x: 4,
                    grid_y: -2,
                },
                &none
            ),
            None,
            "an exterior of the worldspace the camera came from is not wanted at any detail"
        );

        // A cell a nearby door pre-streamed is full even outside the inner grid: an interior with
        // no grid position at all, and an exterior at the far edge of the ring.
        let mut prestream = PrestreamCells::default();
        prestream.request_interior(99);
        prestream.request_exterior(60, IVec2::new(4, 3));
        let with_prestream = |key| wanted_detail(&outside, 2, 5, center, key, &prestream);
        assert_eq!(
            with_prestream(CellKey::Interior(99)),
            Some(CellDetail::Full),
            "a pre-streamed interior is always full"
        );
        assert_eq!(
            with_prestream(exterior(4, 3)),
            Some(CellDetail::Full),
            "a pre-streamed cell outside the inner grid stays full"
        );
        assert_eq!(with_prestream(exterior(4, 2)), Some(CellDetail::Terrain));

        // A ring at or inside the inner grid is no ring at all: the same cells, one tier.
        assert_eq!(
            wanted_detail(&outside, 3, 3, center, exterior(7, -2), &none),
            Some(CellDetail::Full)
        );
        assert_eq!(
            wanted_detail(&outside, 3, 3, center, exterior(8, -2), &none),
            None
        );
        assert_eq!(
            wanted_detail(&outside, 0, 0, center, exterior(1, -2), &none),
            None,
            "the plan is the one cell the camera is in"
        );
    }

    /// The order the plan asks for its cells in is the order the worker's queue gets them in, and
    /// it decides what appears on screen first: the space behind a nearby door, then the cell the
    /// camera is in, then the world around it, nearest first and full before terrain-only. A
    /// pre-streamed cell is asked for once, at the head of the list.
    #[test]
    fn pre_streamed_destinations_come_before_the_ring() {
        let center = IVec2::new(4, -2);
        let outside = ActiveCell {
            worldspace_id: 60,
            interior: None,
        };
        let mut prestream = PrestreamCells::default();
        // An interior destination, and an exterior one at the far edge of the ring.
        prestream.request_interior(99);
        prestream.request_exterior(60, IVec2::new(4, 3));
        let wanted = wanted_cells(&outside, 2, 5, center, &prestream);

        let keys: Vec<CellKey> = wanted.iter().map(|(key, _)| *key).collect();
        let unique: HashSet<CellKey> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len(), "no cell is asked for twice");
        assert_eq!(
            &keys[..2],
            [
                CellKey::Interior(99),
                CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 4,
                    grid_y: 3,
                }
            ],
            "the cells a nearby door pre-streamed lead the queue"
        );
        assert!(
            wanted[..2]
                .iter()
                .all(|(_, detail)| *detail == CellDetail::Full),
            "a pre-streamed cell is always asked for in full"
        );
        assert_eq!(
            keys[2],
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 4,
                grid_y: -2,
            },
            "then the cell the camera is in"
        );
        // The ring's own cells follow, and the pre-streamed exterior appears once, at the head.
        assert!(keys[3..].iter().all(|key| key != &CellKey::Interior(99)));
        assert_eq!(
            keys.iter()
                .filter(|key| **key
                    == CellKey::Exterior {
                        worldspace_id: 60,
                        grid_x: 4,
                        grid_y: 3,
                    })
                .count(),
            1
        );
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

    /// A database in which worldspace 60's grid (2,-3) is cell 1 and that cell has landscape. It
    /// carries the reference tables too - empty - so a full request for the same cell resolves to a
    /// cell with no references rather than failing on a missing table.
    fn write_terrain_database(path: &std::path::Path) {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch(&format!(
                r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
                INSERT INTO schema_info VALUES({version});
                CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER);
                CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                CREATE TABLE statics(id INTEGER PRIMARY KEY,model_path TEXT,
                    bounds_min_x REAL NOT NULL DEFAULT -64,bounds_min_y REAL NOT NULL DEFAULT -64,bounds_min_z REAL NOT NULL DEFAULT -64,
                    bounds_max_x REAL NOT NULL DEFAULT 64,bounds_max_y REAL NOT NULL DEFAULT 64,bounds_max_z REAL NOT NULL DEFAULT 64,
                    bounds_valid INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER NOT NULL,worldspace_id INTEGER,base_form_id INTEGER NOT NULL,
                    is_exterior INTEGER NOT NULL,pos_x REAL NOT NULL,pos_y REAL NOT NULL,pos_z REAL NOT NULL,local_x REAL,local_y REAL,
                    rot_x REAL NOT NULL,rot_y REAL NOT NULL,rot_z REAL NOT NULL,scale REAL NOT NULL DEFAULT 1.0);
                CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
                INSERT INTO cells VALUES(1,60,2,-3);
                INSERT INTO land VALUES(1);
                -- The cell one grid west, with no landscape in the cache: a full request for it
                -- still resolves to a cell, so a run that starts there has no failed cell in it.
                INSERT INTO cells VALUES(2,60,1,-3);"#,
                version = shared::WORLD_DATABASE_SCHEMA_VERSION
            ))
            .unwrap();
    }

    /// A cell cache holding one flat landscape without layers, encoded as the converter writes it.
    /// Without layers no layer texture is ever looked up, which is what lets the test use an empty
    /// asset catalogue.
    fn write_terrain_cache(path: &std::path::Path, cell_id: u32) {
        let cache = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![shared::CachedLand {
                cell_id,
                width: 33,
                height: 33,
                heights: vec![0.0; 33 * 33],
                normals: (0..33 * 33).flat_map(|_| [0, 0, 127]).collect(),
                vertex_colors: Vec::new(),
                layers: Vec::new(),
                water_height: None,
                water_type_form_id: None,
            }],
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// The same cache with a landscape that cannot pass validation: every packed normal is zero,
    /// which `validate_terrain_snapshot` rejects. A ring cell that gets this must not read as a
    /// cell failure.
    fn write_invalid_terrain_cache(path: &std::path::Path, cell_id: u32) {
        let cache = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![shared::CachedLand {
                cell_id,
                width: 33,
                height: 33,
                heights: vec![0.0; 33 * 33],
                normals: vec![0; 33 * 33 * 3],
                vertex_colors: Vec::new(),
                layers: Vec::new(),
                water_height: None,
                water_type_form_id: None,
            }],
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// An app that streams through the real plan and the real commit, with the assets and the
    /// database a terrain-only and a full cell both need, and a camera that does not move itself.
    fn terrain_streaming_app(database_path: &std::path::Path, cache_path: &std::path::Path) -> App {
        let catalogue_path = database_path.with_extension("catalogue");
        write_empty_catalogue(&catalogue_path);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_asset::<StandardMaterial>()
            .init_asset::<TerrainMaterial>()
            .init_asset::<WaterMaterial>()
            .insert_resource(EngineConfig {
                // The camera's own cell is the inner grid; one cell out from it is the ring, so the
                // same cell can be streamed either way by moving the camera one cell.
                stream_radius: 0,
                unload_radius: 1,
                terrain_radius: 2,
                ..EngineConfig::default()
            })
            .insert_resource(RenderOrigin(IVec2::new(2, -3)))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .insert_resource(CellCache::open(cache_path).unwrap())
            .insert_resource(AssetCatalog::open(&catalogue_path).unwrap())
            .init_resource::<DirectionalSnowCatalog>()
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .insert_resource(WorldDatabase::open(database_path).unwrap())
            .init_resource::<StreamingWorld>()
            .init_resource::<PrestreamCells>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, (plan_cells, collect_cells).chain());
        app
    }

    /// Runs frames until `done`, or panics: the worker answers on its own thread, so a fixed number
    /// of updates would be a race.
    fn run_until<F: Fn(&App) -> bool>(app: &mut App, what: &str, done: F) {
        for _ in 0..600 {
            app.update();
            if done(app) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    /// The cell roots that draw one cell, counted by the cell id they carry. References carry a
    /// `CellRef` too, so only roots are counted.
    fn roots_of(app: &mut App, cell_id: u32) -> usize {
        let mut roots = app
            .world_mut()
            .query_filtered::<&CellRef, Or<(With<StreamedCellRoot>, With<DistantTerrainRoot>)>>();
        roots
            .iter(app.world())
            .filter(|cell| cell.0 == cell_id)
            .count()
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
            terrain_radius: 1,
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

    /// A cell that moves from the ring into the full-detail grid and back, as the camera crosses a
    /// cell boundary: the plan asks for it again at the new detail, and the tier change is never a
    /// hole - the root the cell has keeps drawing it until its replacement is committed, and the
    /// cell still has exactly one root at every point.
    #[test]
    fn moving_the_camera_upgrades_and_downgrades_a_ring_cell_without_duplicating_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        write_plan_database(&path);
        let config = EngineConfig {
            stream_radius: 1,
            terrain_radius: 3,
            ..EngineConfig::default()
        };
        let mut app = App::new();
        app.insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .insert_resource(WorldDatabase::open(&path).unwrap())
            .init_resource::<StreamingWorld>()
            .init_resource::<PrestreamCells>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, plan_cells);
        // The camera cell is (0, 0): the plan wants (0..3) in full and the ring out to 3.
        let camera = app
            .world_mut()
            .spawn((Transform::default(), StreamingCamera))
            .id();
        app.update();

        let ring_cell = exterior_cell(2, 0);
        let detail_of = |app: &App, key: CellKey| match app
            .world()
            .resource::<StreamingWorld>()
            .cells
            .get(&key)
        {
            Some(CellStatus::Loading { detail, .. })
            | Some(CellStatus::Resident { detail, .. }) => Some(*detail),
            _ => None,
        };
        let cell_roots = |app: &mut App| {
            let mut roots = app.world_mut().query_filtered::<Entity, (
                With<CellRef>,
                Or<(With<StreamedCellRoot>, With<DistantTerrainRoot>)>,
            )>();
            roots.iter(app.world()).count()
        };
        assert_eq!(
            detail_of(&app, ring_cell),
            Some(CellDetail::Terrain),
            "two cells out is the ring"
        );
        assert_eq!(detail_of(&app, exterior_cell(1, 0)), Some(CellDetail::Full));

        // A committed terrain-only cell for the ring cell: a root of its own, and a reference under
        // it, both of which belong to the terrain tier.
        let root = app
            .world_mut()
            .spawn((DistantTerrainRoot, CellRef(10), Transform::default()))
            .id();
        app.world_mut().spawn((ChildOf(root), FormId(0x30)));
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                ring_cell,
                CellStatus::Resident {
                    root,
                    detail: CellDetail::Terrain,
                },
            );

        // The camera steps one cell towards it. Two cells out is now the inner grid, which is a
        // full cell: the cell is asked for again in full, and keeps drawing the terrain it has
        // until that answer is committed - the camera never sees a hole where the cell is.
        //
        // Every other cell the plan wanted is forgotten first, so the tier-change counters below
        // count this cell's change and not the other cells the camera's step moved across the
        // inner grid's edge in the same frame.
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .retain(|key, _| *key == ring_cell);
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = Vec3::new(CELL_SIZE, 0.0, 0.0);
        app.update();

        assert_eq!(
            detail_of(&app, ring_cell),
            Some(CellDetail::Full),
            "the cell the camera is about to stand in is loaded in full"
        );
        assert_eq!(app.world().resource::<StreamingMetrics>().cells_upgraded, 1);
        assert!(
            app.world().get_entity(root).is_ok(),
            "the terrain-only root keeps drawing the cell while the full cell loads"
        );
        assert!(
            matches!(
                app.world()
                    .resource::<StreamingWorld>()
                    .cells
                    .get(&ring_cell),
                Some(CellStatus::Loading {
                    replaced: Some(replaced),
                    ..
                }) if *replaced == root
            ),
            "the root that draws the cell is the one the loading cell is replacing"
        );
        assert_eq!(cell_roots(&mut app), 1, "one root for the cell, never two");

        // The full cell is committed, standing in for the commit the way the real one does it: the
        // root that was drawing the cell goes in the same frame the new one arrives, so the cell is
        // never drawn twice and never dropped.
        app.world_mut().entity_mut(root).despawn();
        let upgraded = app
            .world_mut()
            .spawn((StreamedCellRoot, CellRef(10), Transform::default()))
            .id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .retain(|key, _| *key == ring_cell);
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = Vec3::ZERO;
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                ring_cell,
                CellStatus::Resident {
                    root: upgraded,
                    detail: CellDetail::Full,
                },
            );
        app.update();

        // Stepping back downgrades it the other way round, with the same guarantee.
        assert_eq!(
            detail_of(&app, ring_cell),
            Some(CellDetail::Terrain),
            "the cell the camera has left is terrain only"
        );
        assert_eq!(
            app.world().resource::<StreamingMetrics>().cells_downgraded,
            1
        );
        assert!(
            app.world().get_entity(upgraded).is_ok(),
            "the full root keeps drawing the cell while the terrain-only cell loads"
        );
        assert_eq!(
            cell_roots(&mut app),
            1,
            "the cell is still drawn by exactly one root"
        );
    }

    /// A cell that failed at one tier is asked for again at the other, and never at the same one:
    /// a ring grid that held nothing is loaded in full when the camera reaches it, while the
    /// hundreds of empty grids around the camera are not asked about again every frame.
    #[test]
    fn a_failed_cell_is_asked_for_again_at_the_other_tier_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        write_plan_database(&path);
        let config = EngineConfig {
            stream_radius: 1,
            terrain_radius: 2,
            ..EngineConfig::default()
        };
        let mut app = App::new();
        app.insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .insert_resource(WorldDatabase::open(&path).unwrap())
            .init_resource::<StreamingWorld>()
            .init_resource::<PrestreamCells>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, plan_cells);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        // The camera is in cell (0, 0): its own cell and the eight around it are wanted in full,
        // everything out to two cells is the ring. These four failed as the plan last saw them.
        for (key, detail) in [
            // The camera's own cell, which the ring answered with nothing.
            (exterior_cell(0, 0), CellDetail::Terrain),
            // Inside the inner grid, failed in full.
            (exterior_cell(1, 0), CellDetail::Full),
            // In the ring, failed as terrain-only: the case of the hundreds of empty grids.
            (exterior_cell(0, 2), CellDetail::Terrain),
            // In the ring, failed in full: the landscape may still be drawable.
            (exterior_cell(-2, 0), CellDetail::Full),
        ] {
            app.world_mut()
                .resource_mut::<StreamingWorld>()
                .cells
                .insert(key, CellStatus::Failed { detail });
        }
        app.update();

        let status = |app: &App, key: CellKey| match app
            .world()
            .resource::<StreamingWorld>()
            .cells
            .get(&key)
        {
            Some(CellStatus::Loading { detail, .. }) => format!("loading {detail:?}"),
            Some(CellStatus::Resident { detail, .. }) => format!("resident {detail:?}"),
            Some(CellStatus::Failed { detail }) => format!("failed {detail:?}"),
            None => "gone".to_owned(),
        };
        assert_eq!(
            status(&app, exterior_cell(0, 0)),
            "loading Full",
            "the cell the camera stands in is asked for in full even though the ring found nothing"
        );
        assert_eq!(
            status(&app, exterior_cell(1, 0)),
            "failed Full",
            "a full cell that failed in full is not asked for again every frame"
        );
        assert_eq!(
            status(&app, exterior_cell(0, 2)),
            "failed Terrain",
            "a ring grid with nothing in it is not asked for again every frame"
        );
        assert_eq!(
            status(&app, exterior_cell(-2, 0)),
            "loading Terrain",
            "a full cell that failed can still be drawn as terrain by the ring, once"
        );
    }

    /// The plan asks for a saturated worker's cells one frame later, and a cell that is waiting for
    /// its replacement keeps the root it has: nothing is despawned before the request is accepted,
    /// so a full queue cannot leave the world with a cell it claims to be drawing and is not.
    #[test]
    fn a_saturated_request_queue_leaves_every_cell_drawn() {
        let config = EngineConfig {
            stream_radius: 1,
            terrain_radius: 2,
            ..EngineConfig::default()
        };
        let database = WorldDatabase::saturated_queue();
        // One slot, filled: the plan's first request is refused.
        assert!(database.try_request(DatabaseRequest::Load {
            generation: 0,
            key: exterior_cell(0, 0),
            detail: CellDetail::Full,
            queued_at: Instant::now(),
        }));
        assert!(
            !database.try_request(DatabaseRequest::Load {
                generation: 0,
                key: exterior_cell(0, 0),
                detail: CellDetail::Full,
                queued_at: Instant::now(),
            }),
            "the queue is full, which is the state the plan has to survive"
        );
        let mut app = App::new();
        app.insert_resource(config)
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(ActiveCell {
                worldspace_id: 60,
                interior: None,
            })
            .insert_resource(database)
            .init_resource::<StreamingWorld>()
            .init_resource::<PrestreamCells>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, (plan_cells, validate_streaming_lifecycle).chain());
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        // The camera's own cell is resident as terrain-only, and the plan wants it in full.
        let root = app
            .world_mut()
            .spawn((DistantTerrainRoot, CellRef(1), Transform::default()))
            .id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                exterior_cell(0, 0),
                CellStatus::Resident {
                    root,
                    detail: CellDetail::Terrain,
                },
            );
        app.update();

        assert!(
            app.world().get_entity(root).is_ok(),
            "the root that draws the cell is still there: the request was never accepted"
        );
        assert!(matches!(
            app.world()
                .resource::<StreamingWorld>()
                .cells
                .get(&exterior_cell(0, 0)),
            Some(CellStatus::Resident {
                detail: CellDetail::Terrain,
                ..
            })
        ));
        assert_eq!(
            app.world()
                .resource::<StreamingMetrics>()
                .requests_submitted,
            0
        );
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.orphaned_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);
    }

    /// A tier change seen end to end, through the real plan and the real commit: the cell is drawn
    /// in every frame of it - no frame has the camera's cell missing - and the terrain-only root
    /// and the full root are never both in the world.
    #[test]
    fn a_tier_change_is_never_a_hole_in_the_world() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("world.db");
        write_terrain_database(&database_path);
        let cache_path = directory.path().join("cell_cache.rkyv");
        write_terrain_cache(&cache_path, 1);
        let mut app = terrain_streaming_app(&database_path, &cache_path);
        // The camera starts in cell (1,-3), one cell west of cell 1 in (2,-3): that cell is the
        // ring's, so it streams as terrain only.
        let camera = app
            .world_mut()
            .spawn((Transform::from_xyz(-2048.0, 0.0, -2048.0), StreamingCamera))
            .id();
        let key = exterior_cell(2, -3);
        let detail_of = |app: &App| match app.world().resource::<StreamingWorld>().cells.get(&key) {
            Some(CellStatus::Loading { detail, .. })
            | Some(CellStatus::Resident { detail, .. }) => Some(*detail),
            Some(CellStatus::Failed { .. }) => None,
            None => None,
        };
        run_until(&mut app, "the ring cell to be committed", |app| {
            matches!(
                app.world()
                    .resource::<StreamingWorld>()
                    .cells
                    .get(&exterior_cell(2, -3)),
                Some(CellStatus::Resident {
                    detail: CellDetail::Terrain,
                    ..
                })
            )
        });
        let terrain_root = match app.world().resource::<StreamingWorld>().cells.get(&key) {
            Some(CellStatus::Resident { root, detail }) if *detail == CellDetail::Terrain => *root,
            other => panic!("the ring cell is not resident as terrain: {other:?}"),
        };
        assert!(
            app.world()
                .get::<DistantTerrainRoot>(terrain_root)
                .is_some(),
            "the ring cell is drawn by a terrain-only root"
        );
        assert_eq!(roots_of(&mut app, 1), 1);

        // The camera steps into it. From that frame on the cell is the camera's own, which the plan
        // wants in full; the terrain the cell already has must keep being drawn until the full cell
        // is committed.
        app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .unwrap()
            .translation = Vec3::new(2048.0, 0.0, -2048.0);
        let mut frames = 0;
        loop {
            app.update();
            frames += 1;
            assert_eq!(
                roots_of(&mut app, 1),
                1,
                "frame {frames}: the cell is drawn by exactly one root from the frame the upgrade \
                 was asked for"
            );
            if detail_of(&app) == Some(CellDetail::Full)
                && matches!(
                    app.world().resource::<StreamingWorld>().cells.get(&key),
                    Some(CellStatus::Resident { .. })
                )
            {
                break;
            }
            assert!(frames < 600, "the full cell never committed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let root = match app.world().resource::<StreamingWorld>().cells.get(&key) {
            Some(CellStatus::Resident { root, .. }) => *root,
            other => panic!("the cell is not resident in full: {other:?}"),
        };
        assert!(
            app.world().get::<StreamedCellRoot>(root).is_some(),
            "the cell is now a full cell, which the portal's isolation walks"
        );
        assert!(app.world().get::<DistantTerrainRoot>(root).is_none());
        assert_eq!(app.world().resource::<StreamingMetrics>().cells_upgraded, 1);
        assert_eq!(roots_of(&mut app, 1), 1);
    }

    /// A ring cell whose landscape does not validate is empty space for the ring, not a cell
    /// failure: a few hundred ring cells are requested at once, and one bad landscape among them
    /// must not read as a cell the camera can stand in failing to load. The same landscape, asked
    /// for by the full request that follows the camera into the cell, is a failure like any other -
    /// that is where real broken data is reported.
    #[test]
    fn a_ring_cell_with_an_invalid_landscape_is_empty_space_not_a_failure() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("world.db");
        write_terrain_database(&database_path);
        let cache_path = directory.path().join("cell_cache.rkyv");
        write_invalid_terrain_cache(&cache_path, 1);
        let mut app = terrain_streaming_app(&database_path, &cache_path);
        let key = exterior_cell(2, -3);
        let commit = |app: &mut App, generation: u64, detail: CellDetail, what: &str| {
            app.world_mut()
                .resource_mut::<StreamingWorld>()
                .cells
                .insert(
                    key,
                    CellStatus::Loading {
                        generation,
                        detail,
                        replaced: None,
                    },
                );
            app.world_mut()
                .resource::<WorldDatabase>()
                .request(DatabaseRequest::Load {
                    generation,
                    key,
                    detail,
                    queued_at: Instant::now(),
                })
                .unwrap();
            run_until(app, what, |app| {
                !matches!(
                    app.world().resource::<StreamingWorld>().cells.get(&key),
                    Some(CellStatus::Loading { .. })
                )
            });
        };

        // The ring's request, which is what a cell at this distance gets.
        commit(&mut app, 1, CellDetail::Terrain, "the ring's answer");
        assert!(matches!(
            app.world().resource::<StreamingWorld>().cells.get(&key),
            Some(CellStatus::Failed {
                detail: CellDetail::Terrain
            })
        ));
        {
            let metrics = app.world().resource::<StreamingMetrics>();
            assert_eq!(
                metrics.failed_cells, 0,
                "a ring cell with a bad landscape is not a cell failure"
            );
            assert_eq!(
                metrics.terrain_validation_failures, 0,
                "and it is not a terrain validation failure either"
            );
            assert_eq!(metrics.ring_cells_empty, 1);
            assert!(
                metrics.asset_failures.is_empty(),
                "the ring's bad landscape is not reported as a broken asset"
            );
        }
        assert_eq!(roots_of(&mut app, 1), 0, "nothing is drawn for it");

        // The same cell, asked for in full because the camera has arrived: now it is a failure.
        commit(&mut app, 2, CellDetail::Full, "the full request's answer");
        assert!(matches!(
            app.world().resource::<StreamingWorld>().cells.get(&key),
            Some(CellStatus::Failed {
                detail: CellDetail::Full
            })
        ));
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.failed_cells, 1);
        assert_eq!(metrics.terrain_validation_failures, 1);
        assert_eq!(
            metrics.ring_cells_empty, 1,
            "the full request's failure is not counted as ring noise"
        );
        assert_eq!(metrics.asset_failures.len(), 1);
    }

    /// The terrain-only request end to end: the planner asks for it, the worker answers it from the
    /// cell cache, and the commit spawns the landscape and nothing else - four quadrant patches and
    /// no reference, light or door - under a root the portal's isolation does not walk.
    #[test]
    fn commits_a_terrain_only_cell_with_its_landscape_and_nothing_else() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("world.db");
        write_terrain_database(&database_path);
        let cache_path = directory.path().join("cell_cache.rkyv");
        write_terrain_cache(&cache_path, 1);
        let catalogue_path = directory.path().join("catalogue.db");
        write_empty_catalogue(&catalogue_path);

        let key = CellKey::Exterior {
            worldspace_id: 60,
            grid_x: 2,
            grid_y: -3,
        };
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
            .insert_resource(CellCache::open(&cache_path).unwrap())
            .insert_resource(AssetCatalog::open(&catalogue_path).unwrap())
            .init_resource::<DirectionalSnowCatalog>()
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .insert_resource(WorldDatabase::open(&database_path).unwrap())
            .init_resource::<StreamingWorld>()
            .init_resource::<TerrainContinuity>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, collect_cells);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                key,
                CellStatus::Loading {
                    generation: 1,
                    detail: CellDetail::Terrain,
                    replaced: None,
                },
            );
        app.world_mut()
            .resource::<WorldDatabase>()
            .request(DatabaseRequest::Load {
                generation: 1,
                key,
                detail: CellDetail::Terrain,
                queued_at: Instant::now(),
            })
            .unwrap();

        // The answer is a query away on the worker's own thread.
        let committed = |app: &App| {
            matches!(
                app.world().resource::<StreamingWorld>().cells.get(&key),
                Some(CellStatus::Resident { .. })
            )
        };
        for _ in 0..400 {
            app.update();
            if committed(&app) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(committed(&app), "the terrain-only cell never committed");

        let streaming = app.world().resource::<StreamingWorld>();
        let Some(CellStatus::Resident { root, detail }) = streaming.cells.get(&key) else {
            panic!("the terrain-only cell did not commit");
        };
        assert_eq!(*detail, CellDetail::Terrain);
        let root = *root;
        assert!(
            app.world().get::<DistantTerrainRoot>(root).is_some(),
            "a terrain-only root is not a cell root the portal's isolation walks"
        );
        assert!(app.world().get::<StreamedCellRoot>(root).is_none());
        assert!(app.world().get::<CellRef>(root).is_some());

        let mut patches = 0;
        let mut references = 0;
        let mut waters = 0;
        let mut descendants = app.world_mut().query::<(
            Option<&TerrainPatch>,
            Option<&FormId>,
            Option<&WaterSurface>,
        )>();
        for (patch, form_id, water) in descendants.iter(app.world()) {
            match (patch, form_id, water) {
                (Some(_), _, _) => patches += 1,
                (_, Some(_), _) => references += 1,
                (_, _, Some(_)) => waters += 1,
                _ => {}
            }
        }
        assert_eq!(patches, 4, "the landscape of four quadrants");
        assert_eq!(references, 0, "no reference is spawned in the ring");
        assert_eq!(waters, 0, "the fixture's cell is dry");
        assert_eq!(
            app.world()
                .resource::<StreamingMetrics>()
                .terrain_validation_failures,
            0
        );
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
        let reference = |door, auto_load| ReferenceRow {
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
            light: None,
            light_radius_override: None,
            auto_load,
        };
        let link = |cell, worldspace| DoorLinkRow {
            destination_ref_id: 0x31,
            destination_cell_id: cell,
            destination_worldspace_id: worldspace,
            arrival_position: [1.0, 2.0, 3.0],
            arrival_rotation: [0.0, 0.0, 0.5],
            label: "Alftand01".into(),
            return_arrival: None,
        };

        let interior = load_door(&reference(Some(link(Some(99), None)), false)).unwrap();
        assert_eq!(interior.ref_id, 0x30);
        assert_eq!(interior.destination.interior_cell_id, Some(99));
        assert_eq!(interior.destination.worldspace_id, None);
        assert_eq!(interior.destination.arrival_position, [1.0, 2.0, 3.0]);
        assert_eq!(interior.label, "Alftand01");

        let exterior = load_door(&reference(Some(link(Some(120), Some(614))), false)).unwrap();
        assert_eq!(exterior.destination.interior_cell_id, None);
        assert_eq!(exterior.destination.worldspace_id, Some(614));

        assert!(
            load_door(&reference(None, false)).is_none(),
            "not a door at all"
        );
        assert!(
            load_door(&reference(Some(link(None, None)), false)).is_none(),
            "a link the converter could not resolve leads nowhere"
        );
        assert!(
            load_door(&reference(Some(link(Some(0), None)), false)).is_none(),
            "cell 0 is not a cell the converter resolved"
        );
    }

    /// The spawned door's outward direction, from the link that leads back into it: the Alftand
    /// ruined tower's door at 73550, 78431 with a link arriving 32 units east of it.
    #[test]
    fn a_door_faces_the_way_the_link_that_leads_back_into_it_arrives() {
        let door_of = |position: [f32; 3], return_arrival| {
            load_door(&ReferenceRow {
                form_id: 0x5BDF1,
                cell_id: 10,
                base_form_id: 0x5BDF0,
                model_path: Some("dungeons\\dwemer\\doors\\dwemerloaddoor.nif".into()),
                position,
                rotation: [0.0, 0.0, 0.0],
                scale: 1.0,
                bounds_min: [0.0; 3],
                bounds_max: [0.0; 3],
                bounds_valid: false,
                door: Some(DoorLinkRow {
                    destination_ref_id: 0x5BDC3,
                    destination_cell_id: Some(0x699B3),
                    destination_worldspace_id: None,
                    arrival_position: [75998.0, 78971.0, -8106.0],
                    arrival_rotation: [0.0, 0.0, -1.87080],
                    label: "AlftandZCell".into(),
                    return_arrival,
                }),
                light: None,
                light_radius_override: None,
                auto_load: false,
            })
            .unwrap()
        };
        let tower = [73550.0, 78431.0, -5609.0];
        let east =
            |outward: [f32; 3]| outward[0] > 0.99 && outward[1].abs() < 0.05 && outward[2] == 0.0;

        // From the arrival point of the return link, 32 units east, and from its heading alone when
        // the point is inside the doorway: both say the door faces east.
        let from_point = door_of(
            tower,
            Some(([73582.0, 78430.0, -5609.0], [0.0, 0.0, 1.60570])),
        );
        assert!(
            east(from_point.outward.expect("a return link is a direction")),
            "{:?}",
            from_point.outward
        );
        let from_heading = door_of(tower, Some((tower, [0.0, 0.0, 1.60570])));
        assert!(
            east(from_heading.outward.expect("a return link is a direction")),
            "{:?}",
            from_heading.outward
        );

        // No link leads back: nothing in the database says which side the door faces, and the
        // portal and the trigger fall back to the door model's own axes.
        assert_eq!(door_of(tower, None).outward, None);
        assert_eq!(
            door_of(tower, Some(([f32::NAN; 3], [f32::NAN; 3]))).outward,
            None,
            "a return link that is not a direction either"
        );
    }

    /// What the base record said about auto-loading reaches the spawned door unchanged: the
    /// database marks an `AutoLoadDoor01` base ([`AUTO_LOAD_COLUMN`]) and nothing about the
    /// reference or the link may drop that.
    #[test]
    fn a_door_keeps_the_auto_load_flag_of_its_base() {
        let door = |auto_load| {
            load_door(&ReferenceRow {
                form_id: 0x15D48,
                cell_id: 10,
                base_form_id: 0x1A4A2,
                model_path: Some("architecture\\doors\\AutoLoadMarker01.nif".into()),
                position: [0.0; 3],
                rotation: [0.0; 3],
                scale: 1.0,
                bounds_min: [0.0; 3],
                bounds_max: [0.0; 3],
                bounds_valid: false,
                door: Some(DoorLinkRow {
                    destination_ref_id: 0x152CF,
                    destination_cell_id: Some(0x152C3),
                    destination_worldspace_id: None,
                    arrival_position: [-947.038, 3958.835, 591.917],
                    arrival_rotation: [0.0, 0.0, 2.96989],
                    label: "Alftand01".into(),
                    return_arrival: None,
                }),
                light: None,
                light_radius_override: None,
                auto_load,
            })
            .unwrap()
        };
        assert!(
            door(true).auto_load,
            "an AutoLoadDoor01 base crosses on contact"
        );
        assert!(
            !door(false).auto_load,
            "a DweDoorLarge01Load keeps the E key"
        );
    }

    use crate::world::database::LightRow;

    fn light_row(radius: f32, color: [u8; 3], flags: u32) -> LightRow {
        LightRow {
            radius,
            color,
            flags,
            falloff: 1.0,
            fade: None,
        }
    }

    /// A reference in interior cell 99 with a light row and, optionally, an `XRDS` radius of its
    /// own, as the database hands one to `spawn_cell`.
    fn lit_reference(
        form_id: u32,
        light: Option<LightRow>,
        radius_override: Option<f32>,
    ) -> ReferenceRow {
        ReferenceRow {
            form_id,
            cell_id: 99,
            base_form_id: 0x200 + form_id,
            model_path: None,
            position: [100.0, 50.0, -200.0],
            rotation: [0.0; 3],
            scale: 1.0,
            bounds_min: [0.0; 3],
            bounds_max: [0.0; 3],
            bounds_valid: false,
            door: None,
            light,
            light_radius_override: radius_override,
            auto_load: false,
        }
    }

    #[derive(Resource, Default)]
    struct QueuedReferences(Vec<ReferenceRow>);

    /// The cell root the last [`spawn_queued_references`] produced.
    #[derive(Resource, Default)]
    struct SpawnedCellRoot(Option<Entity>);

    #[allow(clippy::too_many_arguments)]
    fn spawn_queued_references(
        mut commands: Commands,
        queued: Res<QueuedReferences>,
        mut root: ResMut<SpawnedCellRoot>,
        asset_server: Res<AssetServer>,
        catalog: Res<AssetCatalog>,
        snow: Res<DirectionalSnowCatalog>,
        reflection: Res<WaterReflectionTexture>,
        mut meshes: ResMut<Assets<Mesh>>,
        mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
        mut water_materials: ResMut<Assets<WaterMaterial>>,
        mut profiler: ResMut<ProfilingState>,
    ) {
        root.0 = Some(spawn_cell(
            &mut commands,
            &asset_server,
            &catalog,
            &snow,
            &reflection,
            &mut meshes,
            &mut terrain_materials,
            &mut water_materials,
            IVec2::ZERO,
            CellPayload {
                generation: 1,
                key: CellKey::Interior(99),
                cell_id: 99,
                references: queued.0.clone(),
            },
            None,
            CellDetail::Full,
            &mut profiler,
        ));
    }

    /// An app that spawns one interior cell holding `references` through the real [`spawn_cell`].
    fn spawn_reference_cell_app(references: Vec<ReferenceRow>) -> App {
        spawn_reference_cell_app_with(references, DirectionalSnowCatalog::default())
    }

    /// The same, with the snow catalogue the references are spawned against.
    fn spawn_reference_cell_app_with(
        references: Vec<ReferenceRow>,
        snow: DirectionalSnowCatalog,
    ) -> App {
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
                interior: Some(99),
            })
            .insert_resource(AssetCatalog::open(&path).unwrap())
            .insert_resource(snow)
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .insert_resource(QueuedReferences(references))
            .init_resource::<SpawnedCellRoot>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, spawn_queued_references);
        app.update();
        app
    }

    /// One lit reference, one negative light, one flagged off by default and one plain reference:
    /// exactly one `PointLight` comes out, carrying the reference's own radius and colour.
    #[test]
    fn spawns_one_point_light_for_a_lit_reference() {
        let mut app = spawn_reference_cell_app(vec![
            lit_reference(
                0x100,
                Some(light_row(256.0, [255, 200, 120], 0)),
                Some(850.8),
            ),
            lit_reference(0x101, Some(light_row(512.0, [80, 80, 90], 0x4)), None),
            lit_reference(0x102, Some(light_row(512.0, [80, 80, 90], 0x20)), None),
            lit_reference(0x103, None, None),
        ]);

        let lights: Vec<(Entity, PointLight, crate::lights::SkyrimLight)> = {
            let mut query = app
                .world_mut()
                .query::<(Entity, &PointLight, &crate::lights::SkyrimLight)>();
            query
                .iter(app.world())
                .map(|(entity, light, marker)| (entity, *light, *marker))
                .collect()
        };
        assert_eq!(
            lights.len(),
            1,
            "the negative and off-by-default lights spawn none"
        );
        let (light_entity, light, marker) = lights[0];
        assert_eq!(marker.form_id, 0x100);
        assert_eq!(marker.cell_id, 99);
        assert_eq!(
            light.range, 850.8,
            "the reference's XRDS radius wins over the record's 256"
        );
        assert_eq!(light.color, Color::srgb_u8(255, 200, 120));
        assert!(
            (light.intensity - crate::lights::intensity_for_radius(850.8)).abs() < 1.0e3,
            "{}",
            light.intensity
        );
        assert!(!light.shadow_maps_enabled);

        // The light has to be inside the cell root: that hierarchy is what the portal isolation
        // walks to hide a pre-streamed cell, and what a render-origin rebase moves.
        let root = app
            .world()
            .resource::<SpawnedCellRoot>()
            .0
            .expect("the cell spawned");
        assert!(app.world().entity(root).get::<StreamedCellRoot>().is_some());
        let reference = app
            .world()
            .entity(light_entity)
            .get::<ChildOf>()
            .expect("the light is a child of its reference")
            .parent();
        assert_eq!(
            app.world().entity(reference).get::<FormId>(),
            Some(&FormId(0x100))
        );
        assert_eq!(
            app.world()
                .entity(reference)
                .get::<ChildOf>()
                .expect("the reference is a child of the cell root")
                .parent(),
            root
        );
    }

    /// The static the snow tests place: `DweFacadeTowerRoof01SnowHeavy`, 120 degrees.
    const SNOW_STATIC: u32 = 0x000D_C850;
    /// An ordinary static: a `STAT` with no `DNAM`, which must stay exactly as the model published
    /// it.
    const PLAIN_STATIC: u32 = 0x0001_0000;

    /// The heavy roof's catalogue: the real `MATO` (0x25129) at the roof's 120 degrees, and the
    /// same material at the arch's 90.
    fn snow_catalogue() -> DirectionalSnowCatalog {
        DirectionalSnowCatalog::from_parts(
            &[
                (SNOW_STATIC, 0x0002_5129, 120.0),
                (0x0006_DD66, 0x0002_5129, 90.0),
            ],
            &[(0x0002_5129, snow_material_object_1p())],
        )
    }

    /// A reference like [`lit_reference`] but placed from a static of our choosing. It carries no
    /// model: the snow marker is attached at spawn, before any scene exists, and a model path here
    /// would only try to load a real glb through an asset server this test app does not have.
    fn snow_reference(form_id: u32, base_form_id: u32) -> ReferenceRow {
        ReferenceRow {
            base_form_id,
            ..lit_reference(form_id, None, None)
        }
    }

    /// Every reference the last [`spawn_cell`] produced, by the form id it was placed with.
    fn reference_entity(app: &App, form_id: u32) -> Entity {
        let root = app
            .world()
            .resource::<SpawnedCellRoot>()
            .0
            .expect("the cell spawned");
        app.world()
            .entity(root)
            .get::<Children>()
            .expect("the cell root has the references as children")
            .iter()
            .find(|child| {
                app.world()
                    .entity(*child)
                    .get::<FormId>()
                    .is_some_and(|id| id.0 == form_id)
            })
            .expect("the reference spawned")
    }

    /// A reference whose static carries a `MATO` is spawned with its coverage; one whose static has
    /// none - and every reference at all, in a database without the tables - is spawned with
    /// nothing, which is the behaviour this feature has to leave alone.
    #[test]
    fn a_snow_static_spawns_with_its_coverage_and_a_plain_static_does_not() {
        let app = spawn_reference_cell_app_with(
            vec![
                snow_reference(0x100, SNOW_STATIC),
                snow_reference(0x101, PLAIN_STATIC),
            ],
            snow_catalogue(),
        );

        let snowed = app.world().entity(reference_entity(&app, 0x100));
        let snow = snowed
            .get::<DirectionalSnow>()
            .expect("the heavy roof's reference carries its coverage");
        assert_eq!(snow.static_form_id, SNOW_STATIC);
        // The record's `dir_proj` is (0, 0, -1) in Creation space, so the axis snow falls along is
        // Bevy's +Y and the cone is the roof's 120 degrees.
        assert_eq!(snow.coverage.up, Vec3::Y);
        assert!(
            (snow.coverage.cos_max_angle - (-0.5)).abs() < 1.0e-6,
            "{}",
            snow.coverage.cos_max_angle
        );
        assert_eq!(snow.coverage.color, [107, 116, 126]);
        // The material is the same one for both snow statics, so a catalogue keyed on the material
        // alone would lose the angle.
        assert!(
            snow_catalogue()
                .coverage_for(0x0006_DD66)
                .unwrap()
                .cos_max_angle
                .abs()
                < 1.0e-6
        );

        assert!(
            app.world()
                .entity(reference_entity(&app, 0x101))
                .get::<DirectionalSnow>()
                .is_none(),
            "a static with no `DNAM` is left alone"
        );

        // And with no catalogue at all - the schema-15 database this ships against today - nothing
        // in the cell carries the component.
        let app = spawn_reference_cell_app(vec![snow_reference(0x100, SNOW_STATIC)]);
        assert!(
            app.world()
                .entity(reference_entity(&app, 0x100))
                .get::<DirectionalSnow>()
                .is_none()
        );
    }

    /// The coverage the roof's material draws with, off the real catalogue.
    fn roof_coverage() -> SnowCoverage {
        snow_catalogue().coverage_for(SNOW_STATIC).unwrap()
    }

    /// The swap happens once the readiness pass has had its say, and it moves the mesh onto a snow
    /// material that keeps the base the handler published - alpha mode and all - instead of
    /// replacing the material with one of its own.
    #[test]
    fn a_validated_scene_is_swapped_onto_the_snow_material() {
        let mut app = App::new();
        // The entities need to exist before the app's systems are added, so build them first.
        let root = app
            .world_mut()
            .spawn((Name::new("snow reference"), Transform::default()))
            .id();
        let mesh = app.world_mut().spawn(Transform::default()).id();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<StandardMaterial>()
            .init_asset::<SnowMaterial>()
            .init_resource::<SnowMaterialCache>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, apply_directional_snow);
        let base = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial {
                base_color: Color::srgb(0.4, 0.3, 0.2),
                // The blend pair the glTF handler in `render.rs` publishes: the snow material has
                // to keep it, or every additive snow card turns into a grey veil.
                alpha_mode: AlphaMode::Add,
                emissive: LinearRgba::new(500.0, 400.0, 300.0, 1.0),
                ..default()
            });
        app.world_mut().entity_mut(root).insert(DirectionalSnow {
            static_form_id: SNOW_STATIC,
            coverage: roof_coverage(),
        });
        app.world_mut()
            .entity_mut(mesh)
            .insert((ChildOf(root), MeshMaterial3d(base)));
        app.update();

        let material = app
            .world()
            .entity(mesh)
            .get::<MeshMaterial3d<SnowMaterial>>()
            .expect("the mesh is drawn with the snow material");
        assert!(
            app.world()
                .entity(mesh)
                .get::<MeshMaterial3d<StandardMaterial>>()
                .is_none(),
            "the standard material is replaced, not added beside"
        );
        let snow = app
            .world()
            .resource::<Assets<SnowMaterial>>()
            .get(&material.0)
            .expect("the material is in the asset collection");
        assert_eq!(snow.base.alpha_mode, AlphaMode::Add);
        assert_eq!(snow.base.base_color, Color::srgb(0.4, 0.3, 0.2));
        assert_eq!(
            snow.base.emissive,
            LinearRgba::new(500.0, 400.0, 300.0, 1.0)
        );
        assert!(
            (snow.extension.axis_and_cos_max().w - (-0.5)).abs() < 1.0e-6,
            "the roof's 120-degree cone reaches the uniform: {}",
            snow.extension.axis_and_cos_max().w
        );
        // Applied once: the reference stops being asked about, so the swap cannot run again on a
        // material of its own making.
        assert!(
            app.world().entity(root).get::<DirectionalSnow>().is_none(),
            "the marker goes with the swap"
        );
        app.update();
        assert_eq!(
            app.world().resource::<Assets<SnowMaterial>>().len(),
            1,
            "a second frame must not add a second material"
        );
    }

    /// The swap must not reach a reference the readiness pass is still holding: that pass validates
    /// `MeshMaterial3d<StandardMaterial>`, so a mesh moved early would reach it as a mesh with no
    /// material at all.
    #[test]
    fn a_reference_still_loading_is_left_alone() {
        let coverage = roof_coverage();
        let mut app = App::new();
        let root = app.world_mut().spawn(Transform::default()).id();
        let mesh = app.world_mut().spawn(Transform::default()).id();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<StandardMaterial>()
            .init_asset::<SnowMaterial>()
            .init_resource::<SnowMaterialCache>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, apply_directional_snow);
        let base = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        app.world_mut().entity_mut(root).insert((
            DirectionalSnow {
                static_form_id: SNOW_STATIC,
                coverage,
            },
            PendingAssetProfile {
                started: Instant::now(),
                scene_spawned: false,
                path: "meshes/dwefacadetowerroof01.glb".into(),
                form_id: 0x100,
                base_form_id: SNOW_STATIC,
                cell_id: 99,
            },
        ));
        app.world_mut()
            .entity_mut(mesh)
            .insert((ChildOf(root), MeshMaterial3d(base)));
        app.update();

        assert!(
            app.world()
                .entity(mesh)
                .get::<MeshMaterial3d<StandardMaterial>>()
                .is_some(),
            "the validating pass has to see the loaded material"
        );
        assert!(app.world().entity(root).get::<DirectionalSnow>().is_some());

        // Once the profile is gone the swap runs on the next frame.
        app.world_mut()
            .entity_mut(root)
            .remove::<PendingAssetProfile>();
        app.update();
        assert!(
            app.world()
                .entity(mesh)
                .get::<MeshMaterial3d<SnowMaterial>>()
                .is_some()
        );
    }

    /// Two references of one snow static share the model's materials, and the two of them must end
    /// up on one material asset, not one each.
    #[test]
    fn references_sharing_a_model_and_a_static_share_one_material() {
        let coverage = roof_coverage();
        let mut app = App::new();
        let mut roots = Vec::new();
        for _ in 0..2 {
            roots.push(app.world_mut().spawn(Transform::default()).id());
        }
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<StandardMaterial>()
            .init_asset::<SnowMaterial>()
            .init_resource::<SnowMaterialCache>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, apply_directional_snow);
        let base = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        for root in &roots {
            let mesh = app.world_mut().spawn(Transform::default()).id();
            app.world_mut()
                .entity_mut(mesh)
                .insert((ChildOf(*root), MeshMaterial3d(base.clone())));
            app.world_mut().entity_mut(*root).insert(DirectionalSnow {
                static_form_id: SNOW_STATIC,
                coverage,
            });
        }
        app.update();
        assert_eq!(
            app.world().resource::<SnowMaterialCache>().len(),
            1,
            "the cache is keyed on the material and the static, not the reference"
        );
        assert_eq!(app.world().resource::<Assets<SnowMaterial>>().len(), 1);

        // A second snow static that happens to share the model's material is a different material:
        // its coverage is the only thing telling the two apart, and 120 degrees is not 90.
        let other = app
            .world_mut()
            .spawn((
                Transform::default(),
                DirectionalSnow {
                    static_form_id: 0x0006_DD66,
                    coverage: snow_catalogue().coverage_for(0x0006_DD66).unwrap(),
                },
            ))
            .id();
        let mesh = app.world_mut().spawn(Transform::default()).id();
        app.world_mut()
            .entity_mut(mesh)
            .insert((ChildOf(other), MeshMaterial3d(base)));
        app.update();
        assert_eq!(app.world().resource::<SnowMaterialCache>().len(), 2);
        assert_eq!(app.world().resource::<Assets<SnowMaterial>>().len(), 2);
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
                CellStatus::Resident {
                    root: resident,
                    detail: CellDetail::Full,
                },
            );
        app.update();
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.duplicate_cell_roots, 1);
        assert_eq!(metrics.orphaned_cell_roots, 1);
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 2);
    }

    /// A cell that is loading at another tier keeps drawing the root it is replacing, and a full
    /// cell on its way to being terrain-only sits inside the band the plan keeps cells by: neither
    /// is an orphan and neither is out of range. The streaming fixture caught this first, with the
    /// ring filling the worker's queue while the camera crossed the inner grid's edge: three cells
    /// were still full a few cells out in the frame the camera moved, and the check called them
    /// violations.
    #[test]
    fn lifecycle_validator_accepts_a_cell_that_is_changing_tier() {
        let mut app = App::new();
        app.insert_resource(EngineConfig {
            stream_radius: 1,
            unload_radius: 2,
            terrain_radius: 4,
            ..EngineConfig::default()
        })
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
        // Three cells out: beyond the inner grid, so the plan wants it terrain-only and is loading
        // that now, and its full root is still the one drawing it.
        let replacing = app
            .world_mut()
            .spawn((StreamedCellRoot, CellRef(7), Transform::default()))
            .id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                exterior_cell(3, 0),
                CellStatus::Loading {
                    generation: 1,
                    detail: CellDetail::Terrain,
                    replaced: Some(replacing),
                },
            );
        // Four cells out, which is the far edge of the ring: resident as terrain-only.
        let edge = app
            .world_mut()
            .spawn((DistantTerrainRoot, CellRef(8), Transform::default()))
            .id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                exterior_cell(4, 0),
                CellStatus::Resident {
                    root: edge,
                    detail: CellDetail::Terrain,
                },
            );

        app.update();
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.orphaned_cell_roots, 0);
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.out_of_range_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);

        // A cell beyond every band is still a cell the plan should have unloaded.
        let far = app
            .world_mut()
            .spawn((DistantTerrainRoot, CellRef(9), Transform::default()))
            .id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .cells
            .insert(
                exterior_cell(6, 0),
                CellStatus::Resident {
                    root: far,
                    detail: CellDetail::Terrain,
                },
            );
        app.update();
        let metrics = app.world().resource::<StreamingMetrics>();
        assert_eq!(metrics.out_of_range_cell_roots, 1);
        assert_eq!(metrics.orphaned_cell_roots, 0);
    }

    #[test]
    fn maps_creation_position_and_rotation_through_the_same_basis() {
        assert_eq!(creation_to_bevy(Vec3::Y), Vec3::NEG_Z);
        assert_eq!(creation_to_bevy(Vec3::Z), Vec3::Y);

        // Creation yaw turns clockwise seen from above: a quarter turn takes east (+X) to south
        // (Creation -Y, runtime +Z).
        let rotation = creation_rotation_to_bevy([0.0, 0.0, std::f32::consts::FRAC_PI_2]);
        let rotated = rotation * Vec3::X;
        assert!(rotated.abs_diff_eq(Vec3::Z, 1.0e-5));
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

    /// The additive and multiplicative modes the glTF material handler gives a Skyrim glow are
    /// valid streamed materials, not the "unsupported alpha mode" the validator used to report.
    #[test]
    fn accepts_the_additive_and_multiplicative_alpha_modes() {
        let images = Assets::<Image>::default();
        for alpha_mode in [AlphaMode::Add, AlphaMode::Multiply] {
            assert_eq!(
                validate_standard_material(
                    &StandardMaterial {
                        base_color: Color::srgba(1.0, 0.8, 0.4, 0.6),
                        alpha_mode,
                        ..default()
                    },
                    &images
                ),
                Ok(0)
            );
        }
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

    use bevy::world_serialization::WorldSerializationPlugin;

    /// The app the empty-model tests run in: the real readiness scan
    /// ([`track_asset_readiness`]) over an asset server and the world serialization spawner the
    /// engine uses, so a model reference is spawned and becomes ready by the same route a
    /// converted glb takes.
    ///
    /// The model is a [`WorldAsset`] really added to the asset server, so
    /// `is_loaded_with_dependencies` is true for it exactly as it is for a loaded glb. An empty
    /// `World` is what the converter's empty scene produces: no entity and no render primitive.
    fn empty_model_app() -> (App, Handle<WorldAsset>) {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            WorldSerializationPlugin,
        ))
        .init_asset::<Mesh>()
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .insert_resource(EngineConfig::default())
        .init_resource::<StreamingMetrics>()
        .init_resource::<ProfilingState>()
        .init_resource::<DiagnosticFallbackAssets>()
        .add_observer(mark_world_instance_ready)
        .add_systems(Update, track_asset_readiness);
        let handle = app
            .world()
            .resource::<AssetServer>()
            .add(WorldAsset::new(World::new()));
        // The `Loaded` event is applied in the asset schedule, before the spawner reads it.
        app.update();
        (app, handle)
    }

    /// A reference as [`spawn_cell`] spawns one for a model: the root components, the asset root
    /// pointing at the loaded scene, and the pending profile the readiness scan waits on. No
    /// `ExpectedModelBounds` is inserted, which is what `statics.bounds_valid = 0` produces -
    /// the component's absence is the whole signal.
    fn spawn_model_reference(
        app: &mut App,
        handle: Handle<WorldAsset>,
        expected_bounds: Option<ExpectedModelBounds>,
    ) -> Entity {
        let transform = Transform::from_translation(Vec3::new(3.0, -4.0, 5.0));
        let mut entity = app.world_mut().spawn((
            Name::new("Reference 000F9907"),
            FormId(0x00F9907),
            CellRef(0x02D4E0),
            transform,
            GlobalTransform::from(transform),
            WorldTransform(transform.to_matrix()),
            WorldAssetRoot(handle),
            PendingAssetProfile {
                started: Instant::now(),
                scene_spawned: false,
                path: "meshes/furniture/creatureexit/wispambush.glb".to_owned(),
                form_id: 0x00F9907,
                base_form_id: 0x00EF957,
                cell_id: 0x02D4E0,
            },
        ));
        if let Some(bounds) = expected_bounds {
            entity.insert(bounds);
        }
        entity.id()
    }

    /// Runs the readiness scan to completion and reads the metrics back.
    fn settle_readiness(app: &mut App) -> StreamingMetrics {
        for _ in 0..8 {
            app.update();
        }
        app.world().resource::<StreamingMetrics>().clone()
    }

    /// The converter exports an editor marker as an **empty scene** on purpose - every shape is
    /// dropped - so the model has no converted bounds and no render primitive. Two of them sit in
    /// Blackreach cell `0002D4E0` (`WispAmbush` REFR 000F9907, `FrostSpiderAmbush01` REFR
    /// 0007C068), and a run there used to fail the bounds gate on a model with nothing to draw.
    /// Such a reference is skipped and counted on its own, and nothing fails.
    #[test]
    fn an_empty_scene_model_is_skipped_and_counted_instead_of_failing() {
        let (mut app, handle) = empty_model_app();
        let reference = spawn_model_reference(&mut app, handle, None);

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.transform_bounds_validation_failures, 0,
            "an invisible marker is not a conversion failure: {:?}",
            metrics.asset_failures
        );
        assert_eq!(
            metrics.empty_model_references, 1,
            "the empty model is counted on its own"
        );
        assert!(
            metrics.asset_failures.is_empty(),
            "nothing is recorded as a failed asset: {:?}",
            metrics.asset_failures
        );
        assert_eq!(
            metrics.pending_asset_instances, 0,
            "the instance stops waiting for its asset"
        );
        assert_eq!(
            metrics.bounds_validated, 0,
            "there were no converted bounds to validate"
        );
        assert_eq!(
            metrics.assets_ready, 0,
            "nothing was spawned, so the instance is not a ready asset either"
        );
        assert!(
            !app.world()
                .entity(reference)
                .contains::<PendingAssetProfile>(),
            "the reference is no longer pending"
        );
        assert!(
            app.world().entity(reference).contains::<Transform>(),
            "the empty reference keeps its own transform"
        );
    }

    /// The empty-scene rule is not a way to accept a model that does have geometry: a scene with a
    /// render primitive but no converted bounds is still a conversion defect, and still fatal.
    #[test]
    fn a_model_with_primitives_but_no_converted_bounds_still_fails() {
        let (mut app, handle) = empty_model_app();
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(2.0, 4.0, 6.0));
        let reference = spawn_model_reference(&mut app, handle, None);
        app.world_mut().spawn((
            Mesh3d(mesh),
            Transform::default(),
            GlobalTransform::default(),
            ChildOf(reference),
        ));

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.empty_model_references, 0,
            "a model with geometry is never counted as empty"
        );
        assert_eq!(
            metrics.transform_bounds_validation_failures, 1,
            "a model with geometry and no converted bounds must still fail: {:?}",
            metrics.asset_failures
        );
        let failure = metrics
            .asset_failures
            .first()
            .expect("the failure is recorded as an asset failure");
        assert!(
            failure
                .dependency_chain
                .iter()
                .any(|reason| reason.contains("no validated aggregate bounds")),
            "unexpected failure reason: {:?}",
            failure.dependency_chain
        );
    }

    /// The other direction of the same rule: a model the converter *did* bound, whose spawned
    /// scene turns out to hold no render primitive, is not an empty marker to wave through - the
    /// two disagree and the disagreement is fatal.
    #[test]
    fn a_bounded_model_whose_scene_is_empty_still_fails() {
        let (mut app, handle) = empty_model_app();
        spawn_model_reference(
            &mut app,
            handle,
            ExpectedModelBounds::new(Vec3::splat(-1.0), Vec3::splat(1.0)),
        );

        let metrics = settle_readiness(&mut app);

        assert_eq!(
            metrics.empty_model_references, 0,
            "only a model without converted bounds may be an empty marker"
        );
        assert_eq!(
            metrics.transform_bounds_validation_failures, 1,
            "a bound model whose hierarchy holds no mesh must still fail: {:?}",
            metrics.asset_failures
        );
        let failure = metrics
            .asset_failures
            .first()
            .expect("the failure is recorded as an asset failure");
        assert!(
            failure
                .dependency_chain
                .iter()
                .any(|reason| reason.contains("no bounded mesh")),
            "unexpected failure reason: {:?}",
            failure.dependency_chain
        );
    }
}
