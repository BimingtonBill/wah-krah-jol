//! The doorway's sun casts shadows: a directional light's shadow views take that light's render
//! layers.
//!
//! The portal camera draws the space behind a door on `RenderLayers::layer(DESTINATION_LAYER)` and
//! is lit by its own sun on the same layer (`crate::portal`), so that sun's cascades must collect
//! that space's meshes - which are on that layer and no other ([`crate::portal`]'s isolation). Bevy
//! picks a cascade's casters twice:
//!
//! 1. in the main world, by intersecting the **light's** layers with the mesh's
//!    (`bevy_light-0.19.0/src/lib.rs:400`, `:424`, `check_dir_light_mesh_visibility`). The portal
//!    sun's layers are the destination's, so this list is right.
//! 2. in the render world, by intersecting the **shadow view's** layers with the mesh's
//!    (`bevy_pbr-0.19.0/src/render/light.rs:2582-2586`, `queue_shadows`). `prepare_lights` spawns
//!    each cascade's view with `ShadowView`, `ExtractedView`, a frustum and `LightEntity` and with
//!    no `RenderLayers` at all (`:1869-1897`), and an absent `RenderLayers` is layer 0, not "all
//!    layers".
//!
//! So every cascade view of the doorway's sun - a light on layer 2 - acted as a layer-0 view and
//! queued no layer-2 mesh, whatever the main-world list said: the daylit street seen through an
//! open door was drawn with the sun's light and none of its shadows. The main camera's own sun is
//! unaffected, because it is a layer-0 light and layer 0 is what a view defaults to.
//!
//! [`shadow_views_take_their_lights_layers`] copies the light's `render_layers`
//! (`ExtractedDirectionalLight::render_layers`, `bevy_pbr-0.19.0/src/render/light.rs:124`, written
//! from the light entity's own component at `:815`) onto each directional shadow view entity. It
//! runs in the `Render` schedule in `RenderSystems::CreateViews`, `.after(prepare_lights)`
//! (`:981`, which is in that set) and `.before(RenderSystems::QueueMeshes)` (`:2505`, `queue_shadows`
//! is in that set): the ordering is what makes the schedule's auto-inserted `apply_deferred` land
//! between the views being spawned and their layers being read.
//!
//! Point and spot shadow views are left alone. Their view entity is one per light and shared by
//! every camera that sees it (`:1555-1574`), and a converted light's shadows are off in this engine
//! anyway ([`crate::lights`]), so there is nothing here to fix for them.
//!
//! Delete this part when Bevy's `prepare_lights` gives the shadow views it spawns their light's
//! layers: the insert then writes what the view already has.
//!
//! # A mesh that misses its first shadow queue
//!
//! `queue_shadows` walks a mesh for a shadow view only in the frame
//! `DirtySpecializations::iter_to_queue` yields it: the frame it became visible to that view, a
//! frame it was re-specialized, or a frame after one of Bevy's own two retries (no mesh instance
//! yet, `light.rs:2565-2574`; material not prepared, `:2592-2600`). Four other misses are a bare
//! `continue`, and the mesh is then never queued for that view again until it is re-specialized
//! or leaves and re-enters the view:
//! - no specialized pipeline yet (`:2559-2563`; `specialize_shadows` skips a mesh whose
//!   `RenderMesh` is not prepared without a retry, `:2408-2410`);
//! - the mesh's render-world layers do not meet the view's (`:2582-2586`);
//! - no material instance (`:2588-2591`);
//! - no mesh slab yet (`:2611-2613`).
//!
//! The second one is what lost the street's shadows after leaving an interior. The render world's
//! copy of a mesh's `RenderLayers` is written only when `extract_meshes_for_gpu_building`
//! re-extracts the mesh (`bevy_pbr-0.19.0/src/render/mesh.rs:1905-1927`), and `RenderLayers` is
//! not in that system's change filter. The portal moves a cell between layer 2 (seen through the
//! doorway) and layer 0 (walked in) while the cell stays visible to one camera or the other, so
//! its `ViewVisibility` does not change either, and the render world kept the old layer: at the
//! crossing the main sun's cascades saw the street's meshes for the first time, `queue_shadows`
//! compared the view's layer 0 with the stale layer 2, and dropped every one of them for good.
//!
//! Two pieces fix it:
//! - [`reextract_relayered_meshes`] (main world) marks a mesh's `ViewVisibility` changed when its
//!   `RenderLayers` change. That is in the extraction filter, so the render world gets the new
//!   layers in the same frame. Nothing else reads the change: the value itself is untouched.
//! - [`requeue_dropped_shadow_casters`] (render world) is the safety net for all four paths. Right
//!   after `queue_shadows` it walks the same `iter_to_queue` list, repeats `queue_shadows`' checks
//!   on each entry, and puts every one that hit a bare `continue` into the view's
//!   `PendingShadowQueues::current_frame`: the list Bevy's own two retries use, which the next
//!   frame's `specialize_shadows` and `queue_shadows` both walk again. An entity gets
//!   [`REQUEUE_FRAMES`] such frames in a row and is then left alone, so a mesh that can never be
//!   queued (a material whose prepass has no shadow draw function) costs a bounded number of
//!   lookups.

use bevy::{
    camera::visibility::RenderLayers,
    pbr::{
        ExtractedDirectionalLight, LightEntity, PendingShadowQueues, PreparedMaterial,
        RenderMaterialInstances, RenderMeshInstanceFlags, RenderMeshInstances,
        SpecializedShadowMaterialPipelineCache,
    },
    platform::collections::HashMap,
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        camera::DirtySpecializations,
        erased_render_asset::ErasedRenderAssets,
        mesh::allocator::MeshAllocator,
        sync_world::MainEntity,
        view::{ExtractedView, RenderShadowMapVisibleEntities, RetainedViewEntity},
    },
};
use std::hash::Hash;

/// How many frames in a row [`requeue_dropped_shadow_casters`] puts the same entity back into a
/// shadow view's pending queue before it gives up on it. Bevy's own retries have no limit; this
/// one has, because some of the paths it covers never resolve (a material with no shadow draw
/// function has no pipeline, ever). Four seconds at 60 fps is far longer than a mesh upload or a
/// pipeline takes after a crossing's burst of new meshes.
const REQUEUE_FRAMES: u32 = 240;

/// Gives every directional light's shadow views the render layers of that light. See the module
/// documentation for the Bevy lines this stands in for, and for when it can be deleted.
pub struct ShadowViewLayersPlugin;

impl Plugin for ShadowViewLayersPlugin {
    fn build(&self, app: &mut App) {
        // After the whole visibility pass, so the touch is not overwritten and lands in the same
        // frame's extraction. `crate::portal::isolate_cells` writes the layers in `Update`.
        app.add_systems(
            PostUpdate,
            reextract_relayered_meshes.after(
                bevy::camera::visibility::VisibilitySystems::MarkNewlyHiddenEntitiesInvisible,
            ),
        );
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app.init_resource::<ShadowRequeue>().add_systems(
            Render,
            (
                shadow_views_take_their_lights_layers
                    .in_set(RenderSystems::CreateViews)
                    .after(bevy::pbr::prepare_lights)
                    .before(RenderSystems::QueueMeshes),
                requeue_dropped_shadow_casters
                    .in_set(RenderSystems::QueueMeshes)
                    .after(bevy::pbr::queue_shadows),
            ),
        );
    }
}

/// Marks the `ViewVisibility` of every mesh whose `RenderLayers` changed this frame as changed, so
/// `extract_meshes_for_gpu_building` re-extracts it and the render world's copy of its layers - the
/// one `queue_shadows` compares with each shadow view's - is the new one. See the module
/// documentation for why the layers alone do not trigger that.
///
/// `ViewVisibility` rather than `Mesh3d`, which Bevy itself touches to force a re-extraction
/// (`bevy_pbr-0.19.0/src/material.rs:658-669`): a changed `Mesh3d` also re-computes the mesh's
/// `Aabb` from its vertices and re-specializes it for every view, and neither is needed here.
fn reextract_relayered_meshes(
    mut meshes: Query<&mut ViewVisibility, (With<Mesh3d>, Changed<RenderLayers>)>,
) {
    for mut visibility in &mut meshes {
        visibility.set_changed();
    }
}

/// The render-world state of [`requeue_dropped_shadow_casters`]: which entities it is retrying,
/// and what it has done since its last log line.
#[derive(Resource, Default)]
struct ShadowRequeue {
    book: RetryBook<(RetainedViewEntity, MainEntity)>,
    burst: RequeueBurst,
    /// This frame's drops; a scratch list kept to avoid allocating each frame.
    dropped: Vec<((RetainedViewEntity, MainEntity), Entity, DropPath)>,
}

/// Which of `queue_shadows`' bare `continue`s an entity hit. Bevy does not retry any of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropPath {
    /// No specialized pipeline for the view (`light.rs:2559-2563`).
    NoPipeline,
    /// The mesh's render-world layers do not meet the view's (`:2582-2586`).
    Layers,
    /// No material instance (`:2588-2591`).
    NoMaterialInstance,
    /// No mesh slab (`:2611-2613`).
    NoSlab,
}

impl DropPath {
    const ALL: [DropPath; 4] = [
        DropPath::NoPipeline,
        DropPath::Layers,
        DropPath::NoMaterialInstance,
        DropPath::NoSlab,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// What `queue_shadows` did with one entity it was handed, re-derived from the same resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueOutcome {
    /// Added to the view's shadow phase.
    Queued,
    /// Put into the pending queue by Bevy itself (mesh instance or material not ready).
    BevyRetries,
    /// Not a shadow caster (`NotShadowCaster`, or a material with shadows off): leaving it out is
    /// the right answer, not a miss.
    NeverCasts,
    /// Dropped by a bare `continue`, and not looked at again unless someone re-queues it.
    Dropped(DropPath),
}

/// What `queue_shadows` looks at for one entity, in the order it looks: the inputs of
/// [`queue_outcome`], split out so the decision can be tested without a render world.
#[derive(Clone, Copy, Debug)]
struct QueueFacts {
    has_pipeline: bool,
    /// `None`: no mesh instance in the render world.
    mesh: Option<MeshFacts>,
    has_material_instance: bool,
    /// `None`: the material asset is not prepared. `Some(shadows_enabled)` otherwise.
    material_shadows_enabled: Option<bool>,
    has_slabs: bool,
}

#[derive(Clone, Copy, Debug)]
struct MeshFacts {
    shadow_caster: bool,
    layers_meet_the_view: bool,
}

/// Replays `queue_shadows`' checks (`bevy_pbr-0.19.0/src/render/light.rs:2559-2613`) in its order.
///
/// A missing pipeline is the first check, so it is a drop even when a later check would also
/// fail - unless the reason there is no pipeline is that the mesh never casts (`specialize_shadows`
/// skips non-casters and materials with shadows off without a retry, `:2398-2407`).
fn queue_outcome(facts: &QueueFacts) -> QueueOutcome {
    let never_casts = facts.mesh.is_some_and(|mesh| !mesh.shadow_caster)
        || facts.material_shadows_enabled == Some(false);
    if !facts.has_pipeline {
        return if never_casts {
            QueueOutcome::NeverCasts
        } else {
            QueueOutcome::Dropped(DropPath::NoPipeline)
        };
    }
    let Some(mesh) = facts.mesh else {
        return QueueOutcome::BevyRetries;
    };
    if !mesh.shadow_caster {
        return QueueOutcome::NeverCasts;
    }
    if !mesh.layers_meet_the_view {
        return QueueOutcome::Dropped(DropPath::Layers);
    }
    if !facts.has_material_instance {
        return QueueOutcome::Dropped(DropPath::NoMaterialInstance);
    }
    if facts.material_shadows_enabled.is_none() {
        return QueueOutcome::BevyRetries;
    }
    if !facts.has_slabs {
        return QueueOutcome::Dropped(DropPath::NoSlab);
    }
    QueueOutcome::Queued
}

/// Counts retries per key across consecutive frames. A key that is not dropped in a frame - it was
/// queued, or it left the view - is forgotten, so a later drop starts a fresh budget.
#[derive(Debug)]
struct RetryBook<K> {
    /// Attempts per key as of the end of the previous frame.
    last: HashMap<K, u32>,
    /// Attempts per key dropped in the current frame.
    this: HashMap<K, u32>,
}

impl<K> Default for RetryBook<K> {
    fn default() -> Self {
        Self {
            last: HashMap::default(),
            this: HashMap::default(),
        }
    }
}

/// What [`RetryBook::admit`] decided for one dropped key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// Re-queue it; `first` is true the first frame of a run of drops.
    Retry { first: bool },
    /// Its budget ran out this frame: not re-queued, and reported once.
    GiveUp,
    /// Its budget ran out earlier (only reachable if Bevy itself keeps yielding it).
    Exhausted,
}

impl<K: Copy + Eq + Hash> RetryBook<K> {
    /// Starts a frame: what was dropped in the frame before becomes the history, and anything not
    /// dropped again in this frame is forgotten when the next one starts.
    fn begin_frame(&mut self) {
        std::mem::swap(&mut self.last, &mut self.this);
        self.this.clear();
    }

    /// Records that `key` was dropped in this frame and says whether to re-queue it: yes for the
    /// first `limit` frames in a row, then no.
    fn admit(&mut self, key: K, limit: u32) -> Admission {
        let attempts = self.last.get(&key).copied().unwrap_or(0);
        self.this.insert(key, attempts.saturating_add(1));
        if attempts < limit {
            Admission::Retry {
                first: attempts == 0,
            }
        } else if attempts == limit {
            Admission::GiveUp
        } else {
            Admission::Exhausted
        }
    }
}

/// What the safety net did over one run of consecutive frames with drops - in practice, one
/// crossing's burst of new and re-layered meshes. Logged once when the run ends.
#[derive(Debug, Default, PartialEq)]
struct RequeueBurst {
    frames: u32,
    /// Distinct entity-view pairs re-queued, by the path that dropped them first.
    first_drops: [usize; 4],
    /// Re-queues in total, counting each frame an entity was put back.
    requeues: usize,
    given_up: usize,
}

impl RequeueBurst {
    fn is_empty(&self) -> bool {
        self.frames == 0
    }

    fn summary(&self) -> String {
        let paths = DropPath::ALL
            .iter()
            .map(|path| format!("{path:?} {}", self.first_drops[path.index()]))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "shadow re-queue: {} entity-view pairs re-queued ({paths}), {} re-queues over {} frames, {} given up",
            self.first_drops.iter().sum::<usize>(),
            self.requeues,
            self.frames,
            self.given_up,
        )
    }
}

/// Puts back into each directional shadow view's pending queue the entities `queue_shadows` just
/// dropped with a bare `continue`, so next frame's `specialize_shadows` and `queue_shadows` look at
/// them again. See the module documentation.
///
/// Costs nothing when nothing changed: `iter_to_queue` is empty for a view with no newly visible,
/// re-specialized or pending entities. It never adds a duplicate bin entry: the entities it
/// re-queues are ones `queue_shadows` did not add this frame, and an entity leaves the retry list
/// the first frame it is queued.
#[allow(clippy::too_many_arguments)]
fn requeue_dropped_shadow_casters(
    views: Query<(&LightEntity, &ExtractedView, Option<&RenderLayers>)>,
    visible: Query<&RenderShadowMapVisibleEntities>,
    dirty_specializations: Res<DirtySpecializations>,
    pipelines: Res<SpecializedShadowMaterialPipelineCache>,
    render_mesh_instances: Res<RenderMeshInstances>,
    render_material_instances: Res<RenderMaterialInstances>,
    render_materials: Res<ErasedRenderAssets<PreparedMaterial>>,
    mesh_allocator: Res<MeshAllocator>,
    mut pending: ResMut<PendingShadowQueues>,
    mut state: ResMut<ShadowRequeue>,
) {
    let state = &mut *state;
    state.dropped.clear();

    for (light_entity, view, view_layers) in &views {
        // Point and spot shadows are off in this engine (`crate::lights`); only cascades are
        // looked at, the way `get_shadow_map_visible_entities` finds a cascade's list.
        let LightEntity::Directional { light_entity, .. } = light_entity else {
            continue;
        };
        let retained = view.retained_view_entity;
        let Some(view_pipelines) = pipelines.get(&retained) else {
            continue;
        };
        let Some(view_pending) = pending.get(&retained) else {
            continue;
        };
        let Some(visible_meshes) = visible
            .get(*light_entity)
            .ok()
            .and_then(|visible| visible.subviews.get(&retained))
            .and_then(|visible| visible.get::<Mesh3d>())
        else {
            continue;
        };
        let view_layers = view_layers.unwrap_or_default();

        for (render_entity, main_entity) in
            dirty_specializations.iter_to_queue(retained, visible_meshes, &view_pending.prev_frame)
        {
            // Bevy put it back itself this frame.
            if view_pending
                .current_frame
                .contains(&(*render_entity, *main_entity))
            {
                continue;
            }
            let mesh_instance = render_mesh_instances.render_mesh_queue_data(*main_entity);
            let material_instance = render_material_instances.instances.get(main_entity);
            let facts = QueueFacts {
                has_pipeline: view_pipelines.contains_key(main_entity),
                mesh: mesh_instance.as_ref().map(|mesh| MeshFacts {
                    shadow_caster: mesh
                        .flags()
                        .contains(RenderMeshInstanceFlags::SHADOW_CASTER),
                    layers_meet_the_view: view_layers
                        .intersects(mesh.render_layers.as_ref().unwrap_or_default()),
                }),
                has_material_instance: material_instance.is_some(),
                material_shadows_enabled: material_instance
                    .and_then(|instance| render_materials.get(instance.asset_id))
                    .map(|material| material.properties.shadows_enabled),
                has_slabs: mesh_instance
                    .as_ref()
                    .is_some_and(|mesh| mesh_allocator.mesh_slabs(&mesh.mesh_asset_id()).is_some()),
            };
            if let QueueOutcome::Dropped(path) = queue_outcome(&facts) {
                state
                    .dropped
                    .push(((retained, *main_entity), *render_entity, path));
            }
        }
    }

    state.book.begin_frame();
    if state.dropped.is_empty() {
        if !state.burst.is_empty() {
            info!("{}", state.burst.summary());
            state.burst = RequeueBurst::default();
        }
        return;
    }

    state.burst.frames += 1;
    for ((retained, main_entity), render_entity, path) in state.dropped.drain(..) {
        match state.book.admit((retained, main_entity), REQUEUE_FRAMES) {
            Admission::Retry { first } => {
                if first {
                    state.burst.first_drops[path.index()] += 1;
                }
                state.burst.requeues += 1;
                if let Some(view_pending) = pending.get_mut(&retained) {
                    view_pending
                        .current_frame
                        .insert((render_entity, main_entity));
                }
            }
            Admission::GiveUp => state.burst.given_up += 1,
            Admission::Exhausted => {}
        }
    }
}

/// Copies each directional shadow view's light's `RenderLayers` onto the view.
///
/// The views are the ones `prepare_lights` has just spawned (or re-used from an earlier frame, in
/// which case the layers are already there and nothing is written), and `queue_shadows` reads them
/// in the same frame: with the layers, a cascade of the doorway's sun queues the destination's
/// meshes instead of none. Point and spot views are skipped - see the module documentation.
fn shadow_views_take_their_lights_layers(
    mut commands: Commands,
    views: Query<(Entity, &LightEntity, Option<&RenderLayers>)>,
    lights: Query<&ExtractedDirectionalLight>,
) {
    for (view, light_entity, view_layers) in &views {
        let Some(light_layers) = directional_view_layers(light_entity, &lights) else {
            continue;
        };
        if view_layers == Some(light_layers) {
            continue;
        }
        // `try_insert`, not `insert`: a view can be despawned in the same frame it was made (its
        // light or camera went away), and that is not a reason to take the frame down.
        commands.entity(view).try_insert(light_layers.clone());
    }
}

/// The layers a shadow view takes from the light it is a view of, or `None` for a view to leave as
/// Bevy made it: only a directional light's cascade views take layers, and only when the light
/// entity is in the render world's query (a light despawned between `prepare_lights` and this
/// system has no layers left to take).
fn directional_view_layers<'a>(
    light_entity: &LightEntity,
    lights: &'a Query<&ExtractedDirectionalLight>,
) -> Option<&'a RenderLayers> {
    match light_entity {
        LightEntity::Directional { light_entity, .. } => lights
            .get(*light_entity)
            .ok()
            .map(|light| &light.render_layers),
        LightEntity::Point { .. } | LightEntity::Spot { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{color::LinearRgba, ecs::entity::EntityHashMap, light::CascadeShadowConfig};

    /// The render-world light as `extract_lights` writes it, with only the layers set to
    /// something: the rest is whatever a light with no atmosphere and no cascades computed yet
    /// looks like.
    fn extracted_directional_light(layers: RenderLayers) -> ExtractedDirectionalLight {
        ExtractedDirectionalLight {
            color: LinearRgba::WHITE,
            illuminance: 1000.0,
            transform: GlobalTransform::default(),
            shadow_maps_enabled: true,
            contact_shadows_enabled: false,
            volumetric: false,
            affects_lightmapped_mesh_diffuse: false,
            shadow_depth_bias: 0.0,
            shadow_normal_bias: 0.0,
            cascade_shadow_config: CascadeShadowConfig::default(),
            cascades: EntityHashMap::default(),
            frusta: EntityHashMap::default(),
            render_layers: layers,
            soft_shadow_size: None,
            occlusion_culling: false,
            sun_disk_angular_size: 0.0,
            sun_disk_intensity: 0.0,
        }
    }

    /// The whole fix, on the pieces a unit test can build: a light on the destination's layer, a
    /// cascade view of it with no layers of its own, and the layers the view carries afterwards.
    /// Bevy's own `prepare_lights` and `queue_shadows` are not run here - the first needs a
    /// render device and the second a render phase - so what this pins is the write the fix makes,
    /// not that the queue then finds the meshes.
    #[test]
    fn a_cascade_view_takes_its_lights_layers() {
        let mut app = App::new();
        app.add_systems(Update, shadow_views_take_their_lights_layers);
        let light = app
            .world_mut()
            .spawn(extracted_directional_light(RenderLayers::layer(2)))
            .id();
        let view = app
            .world_mut()
            .spawn(LightEntity::Directional {
                light_entity: light,
                cascade_index: 0,
            })
            .id();

        app.update();

        assert_eq!(
            app.world().entity(view).get::<RenderLayers>(),
            Some(&RenderLayers::layer(2)),
            "a cascade view of a layer-2 light is a layer-2 view; left as Bevy spawns it, it is layer 0 and queues none of the light's own meshes"
        );
    }

    /// A view that already carries the light's layers is left untouched, so the component is not
    /// marked changed every frame for a view that lives on across frames.
    #[test]
    fn a_view_that_already_has_them_is_not_written_again() {
        let mut app = App::new();
        app.add_systems(Update, shadow_views_take_their_lights_layers);
        let light = app
            .world_mut()
            .spawn(extracted_directional_light(RenderLayers::none()))
            .id();
        let view = app
            .world_mut()
            .spawn((
                LightEntity::Directional {
                    light_entity: light,
                    cascade_index: 0,
                },
                RenderLayers::none(),
            ))
            .id();

        app.update();

        let world = app.world_mut();
        let mut query = world.query::<Ref<RenderLayers>>();
        let layers = query.get(world, view).expect("the view keeps its layers");
        assert!(
            !layers.is_changed(),
            "the layers were written although the view already had them"
        );
    }

    /// Point and spot shadow views are shared between cameras and their lights carry no layers in
    /// this engine: whatever layers a view of one happens to have, this system does not touch it.
    #[test]
    fn point_and_spot_shadow_views_are_left_alone() {
        let mut app = App::new();
        app.add_systems(Update, shadow_views_take_their_lights_layers);
        let light = app
            .world_mut()
            .spawn(extracted_directional_light(RenderLayers::layer(2)))
            .id();
        let point = app
            .world_mut()
            .spawn(LightEntity::Point {
                light_entity: light,
                face_index: 0,
            })
            .id();
        let spot = app
            .world_mut()
            .spawn(LightEntity::Spot {
                light_entity: light,
            })
            .id();

        app.update();

        assert!(
            app.world().entity(point).get::<RenderLayers>().is_none(),
            "a point light's cube view is not given layers"
        );
        assert!(
            app.world().entity(spot).get::<RenderLayers>().is_none(),
            "a spot light's view is not given layers"
        );
    }

    /// Facts for an entity `queue_shadows` adds to the phase; each test below breaks one.
    fn queueable() -> QueueFacts {
        QueueFacts {
            has_pipeline: true,
            mesh: Some(MeshFacts {
                shadow_caster: true,
                layers_meet_the_view: true,
            }),
            has_material_instance: true,
            material_shadows_enabled: Some(true),
            has_slabs: true,
        }
    }

    #[test]
    fn an_entity_with_everything_ready_is_queued_and_not_retried() {
        assert_eq!(queue_outcome(&queueable()), QueueOutcome::Queued);
    }

    /// The three bare `continue`s the brief names, and the material-instance one next to them:
    /// each is a drop the safety net re-queues.
    #[test]
    fn each_bare_continue_is_a_drop() {
        let no_pipeline = QueueFacts {
            has_pipeline: false,
            ..queueable()
        };
        let stale_layers = QueueFacts {
            mesh: Some(MeshFacts {
                shadow_caster: true,
                layers_meet_the_view: false,
            }),
            ..queueable()
        };
        let no_material_instance = QueueFacts {
            has_material_instance: false,
            material_shadows_enabled: None,
            ..queueable()
        };
        let no_slab = QueueFacts {
            has_slabs: false,
            ..queueable()
        };
        assert_eq!(
            queue_outcome(&no_pipeline),
            QueueOutcome::Dropped(DropPath::NoPipeline)
        );
        assert_eq!(
            queue_outcome(&stale_layers),
            QueueOutcome::Dropped(DropPath::Layers),
            "the street's meshes after a crossing: layer 0 view, render-world layer still 2"
        );
        assert_eq!(
            queue_outcome(&no_material_instance),
            QueueOutcome::Dropped(DropPath::NoMaterialInstance)
        );
        assert_eq!(
            queue_outcome(&no_slab),
            QueueOutcome::Dropped(DropPath::NoSlab)
        );
    }

    /// A missing pipeline comes first in `queue_shadows`, so it hides later misses: with no mesh
    /// instance either, Bevy still drops it at the pipeline check rather than retrying it.
    #[test]
    fn a_missing_pipeline_is_a_drop_even_with_no_mesh_instance() {
        let facts = QueueFacts {
            has_pipeline: false,
            mesh: None,
            ..queueable()
        };
        assert_eq!(
            queue_outcome(&facts),
            QueueOutcome::Dropped(DropPath::NoPipeline)
        );
    }

    /// The two misses Bevy re-queues itself are left to Bevy.
    #[test]
    fn bevys_own_retries_are_not_doubled() {
        let no_mesh_instance = QueueFacts {
            mesh: None,
            ..queueable()
        };
        let material_not_prepared = QueueFacts {
            material_shadows_enabled: None,
            ..queueable()
        };
        assert_eq!(queue_outcome(&no_mesh_instance), QueueOutcome::BevyRetries);
        assert_eq!(
            queue_outcome(&material_not_prepared),
            QueueOutcome::BevyRetries
        );
    }

    /// A mesh that never casts has no pipeline because `specialize_shadows` skipped it on purpose;
    /// retrying it would only spend the budget.
    #[test]
    fn a_mesh_that_never_casts_is_not_a_drop() {
        let not_a_caster = QueueFacts {
            has_pipeline: false,
            mesh: Some(MeshFacts {
                shadow_caster: false,
                layers_meet_the_view: true,
            }),
            ..queueable()
        };
        let shadows_off = QueueFacts {
            has_pipeline: false,
            material_shadows_enabled: Some(false),
            ..queueable()
        };
        let caster_flag_off_with_pipeline = QueueFacts {
            mesh: Some(MeshFacts {
                shadow_caster: false,
                layers_meet_the_view: false,
            }),
            ..queueable()
        };
        assert_eq!(queue_outcome(&not_a_caster), QueueOutcome::NeverCasts);
        assert_eq!(queue_outcome(&shadows_off), QueueOutcome::NeverCasts);
        assert_eq!(
            queue_outcome(&caster_flag_off_with_pipeline),
            QueueOutcome::NeverCasts
        );
    }

    /// An entity dropped every frame is re-queued `limit` frames in a row, reported once when it
    /// runs out, and not re-queued after that.
    #[test]
    fn a_dropped_entity_is_retried_for_a_bounded_number_of_frames() {
        let mut book = RetryBook::default();
        let mut decisions = Vec::new();
        for _ in 0..5 {
            book.begin_frame();
            decisions.push(book.admit('a', 3));
        }
        assert_eq!(
            decisions,
            vec![
                Admission::Retry { first: true },
                Admission::Retry { first: false },
                Admission::Retry { first: false },
                Admission::GiveUp,
                Admission::Exhausted,
            ]
        );
    }

    /// A frame without the drop (it was queued, or it left the view) forgets the entity, so a
    /// later crossing gives it a whole new budget - and other entities keep their own counts.
    #[test]
    fn a_frame_without_the_drop_resets_its_budget() {
        let mut book = RetryBook::default();
        book.begin_frame();
        assert_eq!(book.admit('a', 2), Admission::Retry { first: true });
        assert_eq!(book.admit('b', 2), Admission::Retry { first: true });
        book.begin_frame();
        assert_eq!(book.admit('b', 2), Admission::Retry { first: false });
        book.begin_frame();
        assert_eq!(book.admit('a', 2), Admission::Retry { first: true });
        assert_eq!(book.admit('b', 2), Admission::GiveUp);
        book.begin_frame();
        book.begin_frame();
        assert!(
            book.last.is_empty() && book.this.is_empty(),
            "nothing dropped for a frame leaves nothing to carry: the next frame costs nothing"
        );
    }

    /// The log line names every path, with the count of entities first dropped there.
    #[test]
    fn the_burst_summary_counts_every_path() {
        let burst = RequeueBurst {
            frames: 3,
            first_drops: [1, 40, 0, 2],
            requeues: 45,
            given_up: 0,
        };
        assert_eq!(
            burst.summary(),
            "shadow re-queue: 43 entity-view pairs re-queued (NoPipeline 1, Layers 40, \
             NoMaterialInstance 0, NoSlab 2), 45 re-queues over 3 frames, 0 given up"
        );
    }

    /// How many meshes had their `ViewVisibility` marked changed in the frame, as the extraction's
    /// change filter sees it.
    #[derive(Resource, Default)]
    struct TouchedThisFrame(usize);

    fn count_touched(
        meshes: Query<(), Changed<ViewVisibility>>,
        mut touched: ResMut<TouchedThisFrame>,
    ) {
        touched.0 = meshes.iter().count();
    }

    fn relayer_app() -> App {
        let mut app = App::new();
        app.init_resource::<TouchedThisFrame>()
            .add_systems(Update, (reextract_relayered_meshes, count_touched).chain());
        app
    }

    /// The crossing: a mesh moved from the portal's layer 2 to layer 0 is marked for
    /// re-extraction in the frame its layers change, and only in that frame.
    #[test]
    fn a_mesh_whose_layers_change_is_re_extracted() {
        let mut app = relayer_app();
        let mesh = app
            .world_mut()
            .spawn((
                Mesh3d(Handle::default()),
                ViewVisibility::default(),
                RenderLayers::layer(2),
            ))
            .id();
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<TouchedThisFrame>().0,
            0,
            "a mesh whose layers did not change is left alone"
        );

        app.world_mut()
            .entity_mut(mesh)
            .insert(RenderLayers::layer(0));
        app.update();
        assert_eq!(
            app.world().resource::<TouchedThisFrame>().0,
            1,
            "the render world only reads the new layers if the mesh is re-extracted"
        );

        app.update();
        assert_eq!(app.world().resource::<TouchedThisFrame>().0, 0);
    }

    /// A non-mesh entity (a light, a cell root) changing layers is not touched.
    #[test]
    fn an_entity_without_a_mesh_is_not_touched() {
        let mut app = relayer_app();
        let light = app
            .world_mut()
            .spawn((ViewVisibility::default(), RenderLayers::layer(2)))
            .id();
        app.update();
        app.update();
        app.world_mut()
            .entity_mut(light)
            .insert(RenderLayers::layer(0));
        app.update();
        assert_eq!(app.world().resource::<TouchedThisFrame>().0, 0);
    }

    /// The layers are the light's own, not the view's and not a constant: a light on the main
    /// camera's layers leaves its cascade views exactly as they were.
    #[test]
    fn a_light_with_no_layers_of_its_own_leaves_its_views_default() {
        let mut app = App::new();
        app.add_systems(Update, shadow_views_take_their_lights_layers);
        let light = app
            .world_mut()
            .spawn(extracted_directional_light(RenderLayers::default()))
            .id();
        let view = app
            .world_mut()
            .spawn(LightEntity::Directional {
                light_entity: light,
                cascade_index: 0,
            })
            .id();

        app.update();

        assert_eq!(
            app.world().entity(view).get::<RenderLayers>(),
            Some(&RenderLayers::default()),
            "the engine's own sun (spawned with no layers) keeps the layer-0 views it had"
        );
    }
}
