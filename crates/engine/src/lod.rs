//! Distant terrain LOD.
//!
//! Skyrim ships precomputed terrain blocks in power-of-two cell sizes
//! (`meshes/terrain/<worldspace>/<worldspace>.<level>.<x>.<y>.btr`, converted to
//! GLB and recorded in `lod_block`). This module streams them as ordinary scene
//! entities in nested distance bands around the camera, so the horizon stays
//! populated beyond the full-detail cells the streamer commits.
//!
//! The tier is off by default: `--lod` turns it on and `--lod-fixture` runs it
//! against a synthetic world. Selection is a pure function of the camera cell,
//! the bands, the `lod_block` table and the residency map ([`plan_blocks`]), and
//! everything else here is the same discipline the cell streamer already uses:
//! one request per missing block, a shared frame commit budget, hysteresis
//! unload, a rebase that follows the floating origin, counters and an invariant
//! validator.
//!
//! Blocks are laid out from the worldspace's LOD grid origin
//! (`lodsettings/<worldspace>.lod`, recorded in `lod_grid`), not from cell 0:
//! Tamriel's grid starts at (-96, -96), which is a multiple of every level, but
//! Blackreach's starts at (-23, -9) and its blocks are named from there. Every
//! cell-to-block lookup therefore goes through [`block_of_cell`] with the
//! worldspace's own origin.

use crate::{
    config::EngineConfig,
    profiling::ProfilingState,
    streaming::{
        AssetFailure, CommitBudget, RenderOrigin, StreamingMetrics, StreamingSet, camera_cell,
        commit_budget_exceeded, converted_model_path, error_chain,
    },
    world::{
        components::{CELL_SIZE, InstanceBounds, LodBlockRoot, StreamingCamera},
        database::{
            DatabaseRequest, LodBlockKey, LodBlockKind, LodBlockPayload, LodBlockTable,
            WorldDatabase,
        },
    },
};
use bevy::{
    asset::{LoadState, RecursiveDependencyLoadState},
    camera::primitives::MeshAabb,
    light::NotShadowCaster,
    pbr::StandardMaterial,
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

/// Bounds a block's spawned meshes may differ from the converted payload by.
const LOD_BOUNDS_TOLERANCE: f32 = 1.0;

/// A tolerance for the block root locality check, which compares render-space
/// translations that are small by construction.
const LOD_LOCALITY_TOLERANCE: f32 = 1.0;

/// One distance band: every block of `level` inside `distance` Creation units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LodBand {
    pub level: u8,
    pub distance: f32,
}

/// The Medium preset, the shipped INI distances for levels 4, 8 and 16.
///
/// Level 32 is deliberately absent: under nested residency a level-32 band
/// inside level 16's reach would add nothing, and it stays available through an
/// explicit `--lod-distances` list for horizon-only experiments.
pub fn medium_bands() -> Vec<LodBand> {
    vec![
        LodBand {
            level: 4,
            distance: 20_000.0,
        },
        LodBand {
            level: 8,
            distance: 32_000.0,
        },
        LodBand {
            level: 16,
            distance: 100_000.0,
        },
    ]
}

/// The High preset from `--lod-distances high`.
pub fn high_bands() -> Vec<LodBand> {
    vec![
        LodBand {
            level: 4,
            distance: 35_000.0,
        },
        LodBand {
            level: 8,
            distance: 70_000.0,
        },
        LodBand {
            level: 16,
            distance: 250_000.0,
        },
    ]
}

/// The Ultra preset from `--lod-distances ultra`.
pub fn ultra_bands() -> Vec<LodBand> {
    vec![
        LodBand {
            level: 4,
            distance: 60_000.0,
        },
        LodBand {
            level: 8,
            distance: 90_000.0,
        },
        LodBand {
            level: 16,
            distance: 250_000.0,
        },
    ]
}

/// Parses `--lod-distances`: a preset name, or an explicit `level:distance` list
/// such as `4:20000,8:32000,16:100000,32:250000`.
///
/// The result is sorted by level, which is the order [`depth_offset_for`]
/// depends on. Returns `None` for anything malformed, and the caller keeps the
/// default bands rather than silently dropping LOD.
pub fn parse_bands(value: &str) -> Option<Vec<LodBand>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "medium" => return Some(medium_bands()),
        "high" => return Some(high_bands()),
        "ultra" => return Some(ultra_bands()),
        _ => {}
    }
    let mut bands = Vec::new();
    for entry in value.split(',') {
        let (level, distance) = entry.trim().split_once(':')?;
        let level = level.trim().parse::<u8>().ok()?;
        let distance = distance.trim().parse::<f32>().ok()?;
        if level == 0 || !distance.is_finite() || distance <= 0.0 {
            return None;
        }
        if bands.iter().any(|band: &LodBand| band.level == level) {
            return None;
        }
        bands.push(LodBand { level, distance });
    }
    bands.sort_by_key(|band| band.level);
    (!bands.is_empty()).then_some(bands)
}

pub struct LodPlugin;

impl Plugin for LodPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LodWorld>()
            .add_observer(mark_lod_instance_ready)
            .add_systems(
                Update,
                (
                    plan_lod_blocks,
                    collect_lod_blocks,
                    finish_commit_budget,
                    track_lod_readiness,
                    validate_lod_lifecycle,
                )
                    .chain()
                    .after(StreamingSet),
            );
    }
}

/// The resident distant-LOD blocks, keyed by block.
#[derive(Resource, Default)]
pub struct LodWorld {
    pub(crate) generation: u64,
    pub(crate) blocks: HashMap<LodBlockKey, LodBlockStatus>,
}

/// The residency state of one distant-LOD block.
#[derive(Debug)]
pub enum LodBlockStatus {
    Loading {
        generation: u64,
    },
    Resident {
        root: Entity,
    },
    /// The block could not be streamed. It stays in the map, unretried, until
    /// its band releases it; retrying every frame would only inflate the
    /// failure counters.
    Failed,
}

/// A block root whose scene has not finished loading.
#[derive(Component)]
struct PendingLodBlock {
    key: LodBlockKey,
    path: String,
    /// Model-space bounds from the `lod_block` row, when the converter measured
    /// them.
    bounds: Option<(Vec3, Vec3)>,
    scene_spawned: bool,
}

/// The block coordinates of the block that contains `cell` at `level`, on the
/// LOD grid whose south-west corner is `origin`.
///
/// The origin is the worldspace's, from `lod_grid`, because a block is named by
/// its south-west cell measured from that corner: Tamriel's grid starts at
/// (-96, -96), a multiple of every level, but Blackreach's starts at (-23, -9),
/// where cell (-20, -5) is in block (-23, -5) and not in the (-20, -8) that a
/// cell-0 grid would name. Euclidean division is what puts a cell west or south
/// of the origin into the block before it, which is the convention the shipped
/// file names use (`tamriel.4.-12.-12.btr`, `blackreach.4.-23.-1.btr`).
pub fn block_of_cell(cell: IVec2, level: u8, origin: IVec2) -> IVec2 {
    let level = i32::from(level.max(1));
    let offset = cell - origin;
    let block = IVec2::new(
        offset.x.div_euclid(level) * level,
        offset.y.div_euclid(level) * level,
    );
    origin + block
}

/// The inclusive cell rectangle a block covers.
///
/// A block is named by its south-west cell, so the rectangle is the same on
/// every grid and needs no origin: the LOD grid origin only decides which of
/// these rectangles a cell falls into ([`block_of_cell`]).
pub fn block_cells(block: IVec2, level: u8) -> (IVec2, IVec2) {
    let span = i32::from(level.max(1)) - 1;
    (block, block + IVec2::splat(span))
}

/// Euclidean distance in Creation units between two cell rectangles, zero when
/// they overlap.
///
/// Measuring to the rectangle rather than to the anchor keeps a block that
/// contains the camera at distance zero and gives every level the same
/// quantity.
pub fn rect_distance_units(left: (IVec2, IVec2), right: (IVec2, IVec2)) -> f32 {
    let dx = (left.0.x - right.1.x).max(right.0.x - left.1.x).max(0) as f32;
    let dy = (left.0.y - right.1.y).max(right.0.y - left.1.y).max(0) as f32;
    (dx * dx + dy * dy).sqrt() * CELL_SIZE
}

/// Whether the full-detail rectangle of `stream_radius` cells around `center`
/// covers the whole block.
///
/// Both rectangles are measured in cells, so coverage is the same on every
/// grid; which block the camera stands in comes from [`block_of_cell`] with the
/// worldspace's own origin.
pub fn block_is_covered(block: IVec2, level: u8, center: IVec2, stream_radius: i32) -> bool {
    let (min, max) = block_cells(block, level);
    let radius = stream_radius.max(0);
    min.x >= center.x - radius
        && max.x <= center.x + radius
        && min.y >= center.y - radius
        && max.y <= center.y + radius
}

/// How far a level is lowered below true height.
///
/// The step is one `lod_depth_offset` per band index, so the finest resident
/// surface is always in front: with nested residency two levels cover the same
/// ground, and a single constant would leave them coplanar and z-fighting.
/// A level no band declares is lowered as deep as the coarsest one.
pub fn depth_offset_for(level: u8, bands: &[LodBand], step: f32) -> f32 {
    let index = bands
        .iter()
        .position(|band| band.level == level)
        .unwrap_or(bands.len());
    (index + 1) as f32 * step
}

/// The render-space translation of a block root.
///
/// The converted block meshes are block-local with the south-west corner at the
/// origin, so the anchor is placed exactly like a cell root and the depth
/// lowering goes on `Y`. `render_origin` is the floating origin, not the LOD
/// grid origin: a block's anchor is an absolute cell, so the grid origin does
/// not move it.
pub fn block_translation(block: IVec2, render_origin: IVec2, depth_offset: f32) -> Vec3 {
    Vec3::new(
        (block.x - render_origin.x) as f32 * CELL_SIZE,
        -depth_offset,
        -(block.y - render_origin.y) as f32 * CELL_SIZE,
    )
}

/// Everything one planning step reads.
#[derive(Debug, Clone, Copy)]
pub struct LodPlanning<'a> {
    pub worldspace_id: u32,
    pub kind: LodBlockKind,
    /// The worldspace's LOD grid origin, from `lod_grid`; cell 0 for a
    /// worldspace the converter recorded no grid for.
    ///
    /// Every block coordinate the plan derives from a cell goes through it, so
    /// an offset worldspace (Blackreach, the Soul Cairn, Apocrypha) is looked up
    /// on the grid its blocks are actually laid out on.
    pub grid_origin: IVec2,
    pub camera_cell: IVec2,
    pub stream_radius: i32,
    pub unload_scale: f32,
    pub bands: &'a [LodBand],
    /// The blocks the worldspace has converted.
    pub available: &'a LodBlockTable,
    /// The blocks already in the residency map, in any state.
    pub resident: &'a HashSet<LodBlockKey>,
}

/// The planner's decision for one frame.
#[derive(Debug, Default, PartialEq)]
pub struct LodPlan {
    /// Blocks to request, nearest first.
    pub wanted: Vec<LodBlockKey>,
    /// Blocks in the map that left their band, or that the full-detail grid now
    /// covers, and must be unloaded.
    pub unloaded: Vec<LodBlockKey>,
    /// Blocks skipped because the full-detail rectangle covers them.
    pub covered_skipped: u64,
}

/// Selects the distant-LOD blocks one frame wants.
///
/// A block is selected when it is inside its band and the full-detail rectangle
/// does not cover it; a block already in the map is kept until it leaves the
/// relaxed band (`distance * unload_scale`), which is the same hysteresis the
/// cell streamer uses between its stream and unload radii. Coarser levels are
/// never skipped because a finer level covers them: nesting bounds the visual
/// cost of a missing block to one LOD step while the finer tier loads.
pub fn plan_blocks(planning: LodPlanning<'_>) -> LodPlan {
    let LodPlanning {
        worldspace_id,
        kind,
        grid_origin,
        camera_cell,
        stream_radius,
        unload_scale,
        bands,
        available,
        resident,
    } = planning;
    let camera = (camera_cell, camera_cell);
    let mut candidates: Vec<LodBlockKey> = available
        .keys(kind)
        .filter(|key| key.worldspace_id == worldspace_id)
        .collect();
    // A block the table no longer lists must still be evaluated, or a resident
    // block would never leave the map.
    candidates.extend(
        resident
            .iter()
            .copied()
            .filter(|key| key.worldspace_id == worldspace_id && key.kind == kind),
    );
    // The block under the camera is looked up on the worldspace's grid, through
    // its origin: offset worldspaces do not start their blocks at cell 0, so the
    // lookup cannot assume one. The table still decides availability, so a grid
    // cell the converter did not produce stays a hole in the data rather than a
    // request that fails.
    for band in bands {
        let block = block_of_cell(camera_cell, band.level, grid_origin);
        let key = LodBlockKey {
            worldspace_id,
            kind,
            level: band.level,
            block_x: block.x,
            block_y: block.y,
        };
        if available.contains(key) {
            candidates.push(key);
        }
    }

    let mut desired = HashSet::new();
    let mut wanted = Vec::new();
    let mut seen = HashSet::new();
    let mut covered_skipped = 0u64;
    for key in candidates {
        // The table and the residency map overlap; each block is judged once.
        if !seen.insert(key) {
            continue;
        }
        let Some(band) = bands.iter().find(|band| band.level == key.level) else {
            // No band declares this level: it is never wanted, so a resident
            // block of that level falls out of the plan and is unloaded.
            continue;
        };
        let block = IVec2::new(key.block_x, key.block_y);
        if block_is_covered(block, key.level, camera_cell, stream_radius) {
            covered_skipped = covered_skipped.saturating_add(1);
            continue;
        }
        let distance = rect_distance_units(camera, block_cells(block, key.level));
        let already_resident = resident.contains(&key);
        let limit = if already_resident {
            band.distance * unload_scale
        } else {
            band.distance
        };
        if distance > limit {
            continue;
        }
        if !already_resident {
            wanted.push((distance, key));
        }
        desired.insert(key);
    }

    let mut unloaded: Vec<LodBlockKey> = resident
        .iter()
        .copied()
        .filter(|key| {
            key.worldspace_id == worldspace_id && key.kind == kind && !desired.contains(key)
        })
        .collect();
    unloaded.sort_by_key(|key| (key.level, key.block_x, key.block_y));
    wanted.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.level.cmp(&right.1.level))
            .then_with(|| left.1.block_x.cmp(&right.1.block_x))
            .then_with(|| left.1.block_y.cmp(&right.1.block_y))
    });
    LodPlan {
        wanted: wanted.into_iter().map(|(_, key)| key).collect(),
        unloaded,
        covered_skipped,
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_lod_blocks(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    table: Res<LodBlockTable>,
    origin: Res<RenderOrigin>,
    camera: Query<&Transform, With<StreamingCamera>>,
    mut world: ResMut<LodWorld>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if !config.lod_enabled {
        return;
    }
    let Ok(camera) = camera.single() else {
        return;
    };
    let center = camera_cell(camera.translation, origin.0);
    let resident: HashSet<LodBlockKey> = world.blocks.keys().copied().collect();
    let plan = plan_blocks(LodPlanning {
        worldspace_id: config.worldspace_id,
        kind: LodBlockKind::Terrain,
        // The grid the worldspace's blocks are laid out on, read with the table
        // at startup; cell 0 when the worldspace has no `lod_grid` row.
        grid_origin: table.origin(),
        camera_cell: center,
        stream_radius: config.stream_radius,
        unload_scale: config.lod_unload_scale,
        bands: &config.lod_bands,
        available: &table,
        resident: &resident,
    });
    metrics.lod_blocks_covered_skipped = metrics
        .lod_blocks_covered_skipped
        .saturating_add(plan.covered_skipped);
    for key in plan.unloaded {
        let Some(status) = world.blocks.remove(&key) else {
            continue;
        };
        metrics.lod_unloaded_blocks = metrics.lod_unloaded_blocks.saturating_add(1);
        if let LodBlockStatus::Resident { root } = status {
            commands.entity(root).despawn();
        }
        profiler.event(format!("{key:?}"), "lod_unloaded", None);
    }

    let mut loading = world
        .blocks
        .values()
        .filter(|status| matches!(status, LodBlockStatus::Loading { .. }))
        .count();
    let mut requested = 0usize;
    for key in plan.wanted {
        if requested >= config.lod_requests_per_frame || loading >= config.lod_max_in_flight {
            break;
        }
        world.generation = world.generation.wrapping_add(1);
        let generation = world.generation;
        // The request channel is bounded and `request` blocks the main thread
        // when full, so the per-frame and in-flight limits above are what keeps
        // a teleport from stalling the frame.
        let requested_ok = database
            .request(DatabaseRequest::LoadLod {
                generation,
                key,
                queued_at: Instant::now(),
            })
            .is_ok();
        if requested_ok {
            world
                .blocks
                .insert(key, LodBlockStatus::Loading { generation });
            requested += 1;
            loading += 1;
            profiler.increment("lod/requests", 1);
            profiler.event(format!("{key:?}"), "lod_requested", None);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_lod_blocks(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    origin: Res<RenderOrigin>,
    asset_server: Res<AssetServer>,
    mut world: ResMut<LodWorld>,
    mut budget: ResMut<CommitBudget>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if !config.lod_enabled {
        return;
    }
    let mut commits = 0usize;
    while commits < config.lod_commits_per_frame
        && budget.started.elapsed().as_micros() < u128::from(config.max_commit_micros_per_frame)
    {
        let Some(response) = database.try_lod_response() else {
            break;
        };
        profiler.record_micros("lod/db_queue_wait", response.queue_wait_micros);
        profiler.record_micros("lod/db_query", response.query_micros);
        let Some(LodBlockStatus::Loading { generation }) = world.blocks.get(&response.key) else {
            metrics.lod_stale_responses = metrics.lod_stale_responses.saturating_add(1);
            continue;
        };
        if *generation != response.generation {
            metrics.lod_stale_responses = metrics.lod_stale_responses.saturating_add(1);
            continue;
        }
        let commit_started = Instant::now();
        match response.result {
            Ok(payload) => match converted_model_path(payload.mesh_path.clone()) {
                Some(path) => {
                    let root = spawn_lod_block(
                        &mut commands,
                        &asset_server,
                        &config,
                        origin.0,
                        &payload,
                        path,
                    );
                    world
                        .blocks
                        .insert(response.key, LodBlockStatus::Resident { root });
                }
                None => {
                    // A block the converter could not publish under `meshes/**`
                    // is as undrawable as a missing one.
                    metrics.lod_asset_failures = metrics.lod_asset_failures.saturating_add(1);
                    profiler.increment("lod/asset_failures", 1);
                    error!(
                        key = ?response.key,
                        path = %payload.mesh_path,
                        "distant LOD block has no runtime mesh path"
                    );
                    world.blocks.insert(response.key, LodBlockStatus::Failed);
                }
            },
            Err(error) => {
                debug!(key = ?response.key, %error, "distant LOD block could not be read");
                metrics.lod_asset_failures = metrics.lod_asset_failures.saturating_add(1);
                profiler.increment("lod/asset_failures", 1);
                world.blocks.insert(response.key, LodBlockStatus::Failed);
            }
        }
        let commit_micros = commit_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        metrics.lod_commits = metrics.lod_commits.saturating_add(1);
        metrics.lod_max_commit_micros = metrics.lod_max_commit_micros.max(commit_micros);
        budget.commits = budget.commits.saturating_add(1);
        commits += 1;
        profiler.record_micros("lod/block_commit", commit_micros);
    }
}

fn spawn_lod_block(
    commands: &mut Commands,
    asset_server: &AssetServer,
    config: &EngineConfig,
    origin: IVec2,
    payload: &LodBlockPayload,
    path: String,
) -> Entity {
    let key = payload.key;
    let anchor = IVec2::new(key.block_x, key.block_y);
    let depth = depth_offset_for(key.level, &config.lod_bands, config.lod_depth_offset);
    commands
        .spawn((
            Name::new(format!(
                "LOD {} L{} {}.{}",
                key.kind.name(),
                key.level,
                key.block_x,
                key.block_y
            )),
            LodBlockRoot {
                kind: key.kind,
                level: key.level,
                anchor,
            },
            Transform::from_translation(block_translation(anchor, origin, depth)),
            // Hidden until the scene is ready, the same idiom the terrain
            // patches use, so a half-spawned block is never drawn.
            Visibility::Hidden,
            WorldAssetRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(path.clone()))),
            PendingLodBlock {
                key,
                path,
                bounds: payload.bounds_valid.then(|| {
                    (
                        Vec3::from_array(payload.bounds_min),
                        Vec3::from_array(payload.bounds_max),
                    )
                }),
                scene_spawned: false,
            },
        ))
        .id()
}

fn mark_lod_instance_ready(
    ready: On<WorldInstanceReady>,
    mut pending: Query<&mut PendingLodBlock>,
) {
    if let Ok(mut pending) = pending.get_mut(ready.entity) {
        pending.scene_spawned = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn track_lod_readiness(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    pending: Query<(Entity, &WorldAssetRoot, &PendingLodBlock, &GlobalTransform)>,
    children: Query<&Children>,
    primitives: Query<(&Mesh3d, Option<&MeshMaterial3d<StandardMaterial>>)>,
    transforms: Query<&GlobalTransform>,
    meshes: Res<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut world: ResMut<LodWorld>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    for (entity, root, pending, root_global) in &pending {
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
            error!(key = ?pending.key, path = %pending.path, chain = ?chain, "distant LOD block failed to load");
            metrics.lod_asset_failures = metrics.lod_asset_failures.saturating_add(1);
            metrics.asset_failures.push(AssetFailure {
                model_path: pending.path.clone(),
                reference_form_id: 0,
                base_form_id: 0,
                cell_id: 0,
                dependency_chain: chain,
            });
            profiler.increment("lod/asset_failures", 1);
            commands.entity(entity).despawn();
            if let Some(status) = world.blocks.get_mut(&pending.key) {
                *status = LodBlockStatus::Failed;
            }
            continue;
        }
        if !pending.scene_spawned || !asset_server.is_loaded_with_dependencies(root.0.id()) {
            continue;
        }
        if let Some(expected) = pending.bounds {
            let bounds = lod_scene_bounds(
                entity,
                root_global,
                &children,
                &primitives,
                &transforms,
                &meshes,
            );
            let mismatch = match bounds {
                Some((min, max)) => !lod_bounds_match(min, max, expected),
                None => true,
            };
            if mismatch {
                // The block is left in the map so it is not re-requested every
                // frame, but it is never drawn: the counter is the evidence.
                error!(
                    key = ?pending.key,
                    path = %pending.path,
                    ?bounds,
                    ?expected,
                    "distant LOD block bounds differ from the converted payload"
                );
                metrics.lod_validation_failures = metrics.lod_validation_failures.saturating_add(1);
                metrics.asset_failures.push(AssetFailure {
                    model_path: pending.path.clone(),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: 0,
                    dependency_chain: vec![
                        "spawned LOD block bounds differ from lod_block.bounds_*".to_owned(),
                    ],
                });
                profiler.increment("lod/validation_failures", 1);
                commands.entity(entity).insert(Visibility::Hidden);
                commands.entity(entity).remove::<PendingLodBlock>();
                continue;
            }
        }
        for descendant in children.iter_descendants(entity) {
            if let Ok((_, Some(handle))) = primitives.get(descendant)
                && let Some(mut material) = materials.get_mut(&handle.0)
            {
                // The GLB owns the texture wiring; the engine only aligns the
                // terrain look. Each block loads its own GLB material, so this
                // cannot leak into a shared material.
                material.cull_mode = None;
                material.double_sided = true;
                material.perceptual_roughness = 0.92;
            }
            if primitives.contains(descendant) {
                // Shadow cascades cover the full-detail grid only; letting a
                // block cast would double every shadow in the overlap ring.
                // The shadow query tests the mesh entity, not its root.
                commands.entity(descendant).insert(NotShadowCaster);
            }
        }
        commands.entity(entity).insert(Visibility::Inherited);
        commands.entity(entity).remove::<PendingLodBlock>();
        profiler.increment("lod/blocks_ready", 1);
    }
}

/// The model-space bounds of every mesh spawned under `root`, in the root's
/// local space.
fn lod_scene_bounds(
    root: Entity,
    root_global: &GlobalTransform,
    children: &Query<&Children>,
    primitives: &Query<(&Mesh3d, Option<&MeshMaterial3d<StandardMaterial>>)>,
    transforms: &Query<&GlobalTransform>,
    meshes: &Assets<Mesh>,
) -> Option<(Vec3, Vec3)> {
    let inverse = root_global.affine().inverse();
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    let mut found = false;
    for descendant in children.iter_descendants(root) {
        let Ok((mesh_handle, _)) = primitives.get(descendant) else {
            continue;
        };
        let Some(mesh) = meshes.get(&mesh_handle.0) else {
            continue;
        };
        let Some(aabb) = mesh.compute_aabb() else {
            continue;
        };
        let Ok(global) = transforms.get(descendant) else {
            continue;
        };
        let local = Mat4::from(inverse * global.affine());
        let bounds = InstanceBounds::transformed(aabb.min().into(), aabb.max().into(), local);
        min = min.min(bounds.min);
        max = max.max(bounds.max);
        found = true;
    }
    found.then_some((min, max))
}

fn lod_bounds_match(actual_min: Vec3, actual_max: Vec3, expected: (Vec3, Vec3)) -> bool {
    (actual_min - expected.0).abs().max_element() <= LOD_BOUNDS_TOLERANCE
        && (actual_max - expected.1).abs().max_element() <= LOD_BOUNDS_TOLERANCE
}

/// Closes the shared commit window and accounts for both streaming tiers.
///
/// The window opens before the cell plan and closes here, after the LOD commit
/// loop, so `commit_frames`, `max_frame_commit_micros` and the budget violations
/// cover everything that committed inside the frame.
pub(crate) fn finish_commit_budget(
    config: Res<EngineConfig>,
    budget: Res<CommitBudget>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if budget.commits == 0 {
        return;
    }
    let frame_micros = budget
        .started
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
    profiler.set_gauge("streaming/commits_this_frame", budget.commits as f64);
    profiler.record_micros("streaming/frame_commit", frame_micros);
}

pub(crate) fn validate_lod_lifecycle(
    config: Res<EngineConfig>,
    origin: Res<RenderOrigin>,
    world: Res<LodWorld>,
    camera: Query<&Transform, With<StreamingCamera>>,
    roots: Query<(Entity, &LodBlockRoot, &Transform)>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if !config.lod_enabled {
        return;
    }
    let resident_entities: HashSet<Entity> = world
        .blocks
        .values()
        .filter_map(|status| match status {
            LodBlockStatus::Resident { root } => Some(*root),
            _ => None,
        })
        .collect();
    let root_entries: Vec<_> = roots.iter().collect();
    let root_entities: HashSet<Entity> =
        root_entries.iter().map(|(entity, _, _)| *entity).collect();
    let mut roots_by_block = HashMap::<(LodBlockKind, u8, i32, i32), usize>::new();
    for (_, block, _) in &root_entries {
        *roots_by_block
            .entry((block.kind, block.level, block.anchor.x, block.anchor.y))
            .or_default() += 1;
    }
    let duplicate_roots = roots_by_block.values().filter(|count| **count > 1).count() as u64;
    let orphaned_roots = root_entities.difference(&resident_entities).count() as u64;
    let missing_roots = resident_entities.difference(&root_entities).count() as u64;
    let center = camera
        .single()
        .ok()
        .map(|camera| camera_cell(camera.translation, origin.0));
    let mut out_of_range_roots = 0u64;
    let mut misplaced_roots = 0u64;
    for (_, block, transform) in &root_entries {
        if let Some(center) = center {
            // Relaxed on purpose: a block is unloaded in the frame it leaves
            // this radius, so anything resident must be inside it.
            let limit = config
                .lod_bands
                .iter()
                .find(|band| band.level == block.level)
                .map_or(0.0, |band| band.distance * config.lod_unload_scale);
            let distance =
                rect_distance_units((center, center), block_cells(block.anchor, block.level));
            if distance > limit {
                out_of_range_roots += 1;
            }
        }
        // Rebasing is the one piece of LOD state the camera moves underneath,
        // so a stale anchor is a counted failure instead of a visual artefact.
        let expected_x = (block.anchor.x - origin.0.x) as f32 * CELL_SIZE;
        let expected_z = -(block.anchor.y - origin.0.y) as f32 * CELL_SIZE;
        if (transform.translation.x - expected_x).abs() > LOD_LOCALITY_TOLERANCE
            || (transform.translation.z - expected_z).abs() > LOD_LOCALITY_TOLERANCE
        {
            misplaced_roots += 1;
        }
    }
    let violations =
        duplicate_roots + orphaned_roots + missing_roots + out_of_range_roots + misplaced_roots;
    metrics.lod_blocks_resident = world
        .blocks
        .values()
        .filter(|status| matches!(status, LodBlockStatus::Resident { .. }))
        .count();
    metrics.lod_blocks_loading = world
        .blocks
        .values()
        .filter(|status| matches!(status, LodBlockStatus::Loading { .. }))
        .count();
    metrics.lod_blocks_failed = world
        .blocks
        .values()
        .filter(|status| matches!(status, LodBlockStatus::Failed))
        .count();
    metrics.lod_peak_resident_blocks = metrics
        .lod_peak_resident_blocks
        .max(metrics.lod_blocks_resident);
    metrics.lod_duplicate_roots = metrics.lod_duplicate_roots.max(duplicate_roots);
    metrics.lod_orphaned_roots = metrics.lod_orphaned_roots.max(orphaned_roots);
    metrics.lod_missing_roots = metrics.lod_missing_roots.max(missing_roots);
    metrics.lod_out_of_range_roots = metrics.lod_out_of_range_roots.max(out_of_range_roots);
    metrics.lod_misplaced_roots = metrics.lod_misplaced_roots.max(misplaced_roots);
    if violations > metrics.lod_invariant_failures {
        error!(
            duplicate_roots,
            orphaned_roots,
            missing_roots,
            out_of_range_roots,
            misplaced_roots,
            "distant LOD lifecycle invariant failed"
        );
        profiler.event("lod", "invariant_failed", None);
    }
    metrics.lod_invariant_failures = metrics.lod_invariant_failures.max(violations);
    profiler.set_gauge("lod/resident_blocks", metrics.lod_blocks_resident as f64);
    profiler.set_gauge("lod/loading_blocks", metrics.lod_blocks_loading as f64);
    profiler.set_gauge("lod/failed_blocks", metrics.lod_blocks_failed as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORLDSPACE: u32 = 60;

    /// Blackreach's LOD grid origin, from its `lodsettings/blackreach.lod`
    /// header: its blocks are named from (-23, -9) rather than from cell 0. The
    /// worldspace id above is the fixture's; the grid and the block names in
    /// these tests are Blackreach's.
    const BLACKREACH_ORIGIN: IVec2 = IVec2::new(-23, -9);

    fn key(level: u8, block_x: i32, block_y: i32) -> LodBlockKey {
        LodBlockKey {
            worldspace_id: WORLDSPACE,
            kind: LodBlockKind::Terrain,
            level,
            block_x,
            block_y,
        }
    }

    fn table(keys: &[LodBlockKey]) -> LodBlockTable {
        LodBlockTable::from_keys(keys.iter().copied())
    }

    fn planning<'a>(
        camera_cell: IVec2,
        available: &'a LodBlockTable,
        resident: &'a HashSet<LodBlockKey>,
        bands: &'a [LodBand],
    ) -> LodPlanning<'a> {
        LodPlanning {
            worldspace_id: WORLDSPACE,
            kind: LodBlockKind::Terrain,
            grid_origin: IVec2::ZERO,
            camera_cell,
            stream_radius: 0,
            unload_scale: 1.1,
            bands,
            available,
            resident,
        }
    }

    #[test]
    fn block_coordinates_use_euclidean_division_for_negative_cells() {
        assert_eq!(
            block_of_cell(IVec2::new(-1, -5), 4, IVec2::ZERO),
            IVec2::new(-4, -8)
        );
        assert_eq!(
            block_of_cell(IVec2::new(-4, 0), 4, IVec2::ZERO),
            IVec2::new(-4, 0)
        );
        assert_eq!(
            block_of_cell(IVec2::new(4, 7), 4, IVec2::ZERO),
            IVec2::new(4, 4)
        );
        assert_eq!(
            block_of_cell(IVec2::new(-11, -1), 1, IVec2::ZERO),
            IVec2::new(-11, -1)
        );
        assert_eq!(block_cells(IVec2::new(-4, -8), 4).1, IVec2::new(-1, -5));
    }

    #[test]
    fn a_tamriel_style_origin_describes_the_same_grid_as_cell_zero() {
        // Tamriel's grid starts at (-96, -96), a multiple of every level, so
        // the origin-relative lookup agrees with the cell-0 grid there. The
        // regression this pins: the formula must not shift a worldspace whose
        // origin is already aligned.
        let tamriel = IVec2::new(-96, -96);
        for cell in [
            IVec2::new(-1, -5),
            IVec2::new(-96, -96),
            IVec2::new(4, 7),
            IVec2::new(-100, 13),
        ] {
            for level in [1, 4, 8, 16, 32] {
                assert_eq!(
                    block_of_cell(cell, level, tamriel),
                    block_of_cell(cell, level, IVec2::ZERO),
                    "cell {cell:?} at level {level}"
                );
            }
        }
    }

    #[test]
    fn block_lookup_follows_the_worldspaces_grid_origin() {
        // Blackreach's grid starts at (-23, -9): cell (-20, -6) is the
        // south-west cell of the corner block, and cell (-20, -5) is one block
        // north of it. A lookup that assumed cell 0 would call both of them
        // (-20, -8) and (-24, -8), blocks the worldspace does not have.
        assert_eq!(
            block_of_cell(IVec2::new(-20, -6), 4, BLACKREACH_ORIGIN),
            IVec2::new(-23, -9)
        );
        assert_eq!(
            block_of_cell(IVec2::new(-20, -5), 4, BLACKREACH_ORIGIN),
            IVec2::new(-23, -5)
        );
        // One cell west and south of the grid corner is the block before it.
        assert_eq!(
            block_of_cell(IVec2::new(-24, -10), 4, BLACKREACH_ORIGIN),
            IVec2::new(-27, -13)
        );
        // The shipped block `blackreach.4.-23.-1` covers cells
        // (-23..-20, -1..2), and every cell in that rectangle maps back to it.
        let block = IVec2::new(-23, -1);
        assert_eq!(
            block_cells(block, 4),
            (IVec2::new(-23, -1), IVec2::new(-20, 2))
        );
        for cell in [IVec2::new(-23, -1), IVec2::new(-22, 0), IVec2::new(-20, 2)] {
            assert_eq!(block_of_cell(cell, 4, BLACKREACH_ORIGIN), block);
        }
        // The coarser levels of the same grid, as Blackreach ships them:
        // `blackreach.8.-7.-9`, `blackreach.16.-7.7`, `blackreach.32.-23.-9`.
        assert_eq!(
            block_of_cell(IVec2::new(-7, -9), 8, BLACKREACH_ORIGIN),
            IVec2::new(-7, -9)
        );
        assert_eq!(
            block_of_cell(IVec2::new(-1, 7), 16, BLACKREACH_ORIGIN),
            IVec2::new(-7, 7)
        );
        assert_eq!(
            block_of_cell(IVec2::new(3, -5), 32, BLACKREACH_ORIGIN),
            IVec2::new(-23, -9)
        );
    }

    #[test]
    fn an_offset_grid_lookup_finds_the_blocks_the_table_lists() {
        // Four blocks Blackreach ships, at three levels. Their anchors are the
        // converted rows, so a lookup that lands anywhere else names a block
        // the worldspace does not have.
        let listed = [
            key(4, -23, -9),
            key(4, -23, -1),
            key(8, -7, -1),
            key(16, -7, 7),
        ];
        let available = LodBlockTable::from_keys_at(listed.iter().copied(), BLACKREACH_ORIGIN);
        for entry in listed {
            let block = IVec2::new(entry.block_x, entry.block_y);
            let (min, max) = block_cells(block, entry.level);
            for cell in [min, max] {
                let found = block_of_cell(cell, entry.level, BLACKREACH_ORIGIN);
                assert_eq!(found, block, "cell {cell:?} at level {}", entry.level);
                assert!(
                    available.contains(LodBlockKey {
                        block_x: found.x,
                        block_y: found.y,
                        ..entry
                    }),
                    "block {found:?} at level {} is not in the table",
                    entry.level
                );
            }
        }
    }

    #[test]
    fn coverage_is_measured_on_the_offset_grid() {
        // The camera stands in the shipped block `blackreach.4.-23.-1`, which
        // covers cells (-23..-20, -1..2).
        let camera_cell = IVec2::new(-22, 0);
        let camera_block = block_of_cell(camera_cell, 4, BLACKREACH_ORIGIN);
        assert_eq!(camera_block, IVec2::new(-23, -1));
        // A block the camera is inside is at distance zero.
        assert_eq!(
            rect_distance_units((camera_cell, camera_cell), block_cells(camera_block, 4)),
            0.0
        );
        let available = LodBlockTable::from_keys_at([key(4, -23, -1)], BLACKREACH_ORIGIN);
        let resident = HashSet::new();
        // A full-detail rectangle that holds the whole block skips it...
        let plan = plan_blocks(LodPlanning {
            grid_origin: available.origin(),
            camera_cell,
            stream_radius: 3,
            ..planning(camera_cell, &available, &resident, &medium_bands())
        });
        assert!(plan.wanted.is_empty());
        assert_eq!(plan.covered_skipped, 1);
        // ...one cell smaller and the block is wanted, once, at distance zero.
        let plan = plan_blocks(LodPlanning {
            grid_origin: available.origin(),
            camera_cell,
            stream_radius: 1,
            ..planning(camera_cell, &available, &resident, &medium_bands())
        });
        assert_eq!(plan.wanted, vec![key(4, -23, -1)]);
        assert_eq!(plan.covered_skipped, 0);
    }

    #[test]
    fn a_block_fully_inside_the_full_detail_rectangle_is_skipped() {
        let available = table(&[key(4, 0, 0)]);
        let resident = HashSet::new();
        let plan = plan_blocks(LodPlanning {
            stream_radius: 4,
            ..planning(IVec2::ZERO, &available, &resident, &medium_bands())
        });
        assert!(plan.wanted.is_empty());
        assert_eq!(plan.covered_skipped, 1);
    }

    #[test]
    fn a_block_straddling_the_full_detail_rectangle_is_still_selected() {
        let available = table(&[key(4, 0, 0)]);
        let resident = HashSet::new();
        // The block covers cells 0..=3; the full-detail rectangle covers 1..=3.
        let plan = plan_blocks(LodPlanning {
            camera_cell: IVec2::new(2, 2),
            stream_radius: 1,
            ..planning(IVec2::ZERO, &available, &resident, &medium_bands())
        });
        assert_eq!(plan.wanted, vec![key(4, 0, 0)]);
        assert_eq!(plan.covered_skipped, 0);
    }

    #[test]
    fn a_coarser_block_stays_resident_where_a_finer_one_covers_it() {
        let available = table(&[key(4, 0, 0), key(8, 0, 0)]);
        let resident: HashSet<_> = [key(4, 0, 0), key(8, 0, 0)].into_iter().collect();
        let plan = plan_blocks(LodPlanning {
            stream_radius: 5,
            ..planning(IVec2::ZERO, &available, &resident, &medium_bands())
        });
        // Level 4 spans 0..=3 and is covered; level 8 spans 0..=7 and is not.
        assert_eq!(plan.unloaded, vec![key(4, 0, 0)]);
        assert!(plan.wanted.is_empty());
        assert_eq!(plan.covered_skipped, 1);
    }

    #[test]
    fn a_block_beyond_its_band_is_never_requested() {
        // The level-4 band is 20 000 units, which is 4.88 cells. The block four
        // cells east is 16 384 units away and inside the band; the diagonal
        // block four cells in both axes is 23 170, and the one eight cells east
        // is 32 768.
        let available = table(&[key(4, 0, 0), key(4, 4, 4), key(4, 8, 0)]);
        let resident = HashSet::new();
        let plan = plan_blocks(planning(
            IVec2::ZERO,
            &available,
            &resident,
            &medium_bands(),
        ));
        assert_eq!(plan.wanted, vec![key(4, 0, 0)]);
    }

    #[test]
    fn a_block_absent_from_the_table_is_never_requested() {
        // The level-4 block at (4, 0) is inside the band and only the table
        // decides whether the worldspace has it.
        let available = table(&[key(4, 0, 0)]);
        let resident = HashSet::new();
        let plan = plan_blocks(planning(
            IVec2::ZERO,
            &available,
            &resident,
            &medium_bands(),
        ));
        assert_eq!(plan.wanted, vec![key(4, 0, 0)]);
    }

    #[test]
    fn a_resident_block_survives_its_band_up_to_the_unload_scale() {
        let available = table(&[key(4, 0, 0)]);
        let resident: HashSet<_> = [key(4, 0, 0)].into_iter().collect();
        let bands = medium_bands();
        // The camera five cells east of the block is 20 480 units from its
        // rectangle: past the 20 000 band, inside the 22 000 relaxed band.
        let plan = plan_blocks(planning(IVec2::new(8, 0), &available, &resident, &bands));
        assert!(plan.unloaded.is_empty());
        // A block that was never requested is not admitted that far out.
        let empty = HashSet::new();
        let plan = plan_blocks(planning(IVec2::new(8, 0), &available, &empty, &bands));
        assert!(plan.wanted.is_empty());
    }

    #[test]
    fn a_resident_block_beyond_the_unload_scale_is_unloaded() {
        let available = table(&[key(4, 0, 0)]);
        let resident: HashSet<_> = [key(4, 0, 0)].into_iter().collect();
        // Six cells east is 24 576 units, past the relaxed band.
        let plan = plan_blocks(planning(
            IVec2::new(9, 0),
            &available,
            &resident,
            &medium_bands(),
        ));
        assert_eq!(plan.unloaded, vec![key(4, 0, 0)]);
        assert!(plan.wanted.is_empty());
    }

    #[test]
    fn distance_is_measured_to_the_block_rectangle_not_to_its_anchor() {
        let block = IVec2::new(-4, -4);
        let camera = (IVec2::ZERO, IVec2::ZERO);
        let rect = block_cells(block, 4);
        assert_eq!(rect.1, IVec2::new(-1, -1));
        // The camera cell is diagonally adjacent to the block's corner cell.
        assert_eq!(
            rect_distance_units(camera, rect),
            std::f32::consts::SQRT_2 * CELL_SIZE
        );
        // The anchor itself sits four cells away in both axes; measuring to it
        // would put the block several cells further out than it is.
        assert!(rect_distance_units(camera, (block, block)) > CELL_SIZE * 5.0);
        // A block the camera is inside is at distance zero.
        assert_eq!(
            rect_distance_units(camera, block_cells(IVec2::ZERO, 4)),
            0.0
        );
    }

    #[test]
    fn the_depth_offset_steps_down_once_per_level() {
        let bands = medium_bands();
        assert_eq!(depth_offset_for(4, &bands, 32.0), 32.0);
        assert_eq!(depth_offset_for(8, &bands, 32.0), 64.0);
        assert_eq!(depth_offset_for(16, &bands, 32.0), 96.0);
        assert_eq!(
            block_translation(IVec2::new(8, -3), IVec2::new(2, 1), 64.0),
            Vec3::new(6.0 * CELL_SIZE, -64.0, 4.0 * CELL_SIZE)
        );
    }

    #[test]
    fn band_presets_match_the_shipped_ini_distances() {
        assert_eq!(
            medium_bands()
                .iter()
                .map(|band| (band.level, band.distance))
                .collect::<Vec<_>>(),
            vec![(4, 20_000.0), (8, 32_000.0), (16, 100_000.0)]
        );
        assert_eq!(parse_bands("medium"), Some(medium_bands()));
        assert_eq!(parse_bands("High"), Some(high_bands()));
        assert_eq!(parse_bands(" ultra "), Some(ultra_bands()));
        assert_eq!(
            high_bands()
                .iter()
                .map(|band| band.distance)
                .collect::<Vec<_>>(),
            vec![35_000.0, 70_000.0, 250_000.0]
        );
        assert_eq!(
            ultra_bands()
                .iter()
                .map(|band| band.distance)
                .collect::<Vec<_>>(),
            vec![60_000.0, 90_000.0, 250_000.0]
        );
        assert!(parse_bands("nonsense").is_none());
        assert!(parse_bands("4:20000,4:30000").is_none());
        assert!(parse_bands("0:20000").is_none());
    }

    #[test]
    fn an_explicit_distance_list_overrides_the_preset() {
        let bands = parse_bands("4:20000,8:32000,16:100000,32:250000").unwrap();
        assert_eq!(
            bands,
            vec![
                LodBand {
                    level: 4,
                    distance: 20_000.0
                },
                LodBand {
                    level: 8,
                    distance: 32_000.0
                },
                LodBand {
                    level: 16,
                    distance: 100_000.0
                },
                LodBand {
                    level: 32,
                    distance: 250_000.0
                },
            ]
        );
        // The list is sorted by level, which is the order the depth offset reads.
        assert_eq!(depth_offset_for(32, &bands, 32.0), 128.0);
        assert_eq!(
            parse_bands("16:100000,4:20000"),
            Some(vec![
                LodBand {
                    level: 4,
                    distance: 20_000.0
                },
                LodBand {
                    level: 16,
                    distance: 100_000.0
                },
            ])
        );
    }
}
