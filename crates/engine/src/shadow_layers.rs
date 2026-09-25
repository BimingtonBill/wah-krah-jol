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
//! Delete this module when Bevy's `prepare_lights` gives the shadow views it spawns their light's
//! layers: the insert then writes what the view already has.

use bevy::{
    camera::visibility::RenderLayers,
    pbr::{ExtractedDirectionalLight, LightEntity},
    prelude::*,
    render::{Render, RenderApp, RenderSystems},
};

/// Gives every directional light's shadow views the render layers of that light. See the module
/// documentation for the Bevy lines this stands in for, and for when it can be deleted.
pub struct ShadowViewLayersPlugin;

impl Plugin for ShadowViewLayersPlugin {
    fn build(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app.add_systems(
            Render,
            shadow_views_take_their_lights_layers
                .in_set(RenderSystems::CreateViews)
                .after(bevy::pbr::prepare_lights)
                .before(RenderSystems::QueueMeshes),
        );
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
