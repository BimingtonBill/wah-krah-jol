use crate::{
    profiling::ProfilingState,
    world::{cache::TerrainSnapshot, database::AssetCatalog},
};
use bevy::{
    asset::{AssetEvent, AssetEventSystems, embedded_asset},
    camera::{RenderTarget, visibility::RenderLayers},
    core_pipeline::{mip_generation::experimental::depth::ViewDepthPyramid, prepass::DepthPrepass},
    mesh::VertexAttributeValues,
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        batching::gpu_preprocessing::{
            GpuPreprocessingMode, GpuPreprocessingSupport, IndirectParametersBuffers,
        },
        occlusion_culling::OcclusionCulling,
        render_resource::{AsBindGroup, ShaderType},
    },
    shader::ShaderRef,
};
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

pub type TerrainMaterial = ExtendedMaterial<StandardMaterial, TerrainExtension>;
pub type WaterMaterial = ExtendedMaterial<StandardMaterial, WaterExtension>;

pub struct VercidiumRendererPlugin;

impl Plugin for VercidiumRendererPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/terrain.wgsl");
        embedded_asset!(app, "shaders/water.wgsl");
        app.add_plugins((
            MaterialPlugin::<TerrainMaterial>::default(),
            MaterialPlugin::<WaterMaterial>::default(),
        ))
        .init_resource::<RendererMetrics>()
        .add_systems(Startup, setup_water_reflection)
        .add_systems(
            Update,
            (
                animate_water_materials,
                update_water_reflection_camera,
                sync_renderer_metrics,
            ),
        )
        // `AssetEventSystems` is where a loaded asset's `Added` message is published, and the
        // frame's render extraction runs right after the main schedule ends: rewriting a mesh's
        // vertex colours in the same `PostUpdate` is what puts the rewritten vertices in front of
        // that extraction. [`force_opaque_vertex_colours`] has the why.
        .add_systems(
            PostUpdate,
            force_opaque_vertex_colours.after(AssetEventSystems),
        );

        let bridge = RendererProofBridge::default();
        app.insert_resource(bridge.clone());
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.insert_resource(bridge).add_systems(
                Render,
                sample_renderer_path.after(RenderSystems::PrepareResourcesCollectPhaseBuffers),
            );
        }
    }
}

#[derive(Resource, Debug, Clone, Default, Serialize)]
pub struct RendererMetrics {
    pub gpu_preprocessing_active: bool,
    pub gpu_culling_active: bool,
    pub indirect_drawing_active: bool,
    pub occlusion_culling_views: u64,
    pub hzb_views: u64,
    pub indirect_phase_buffers: u64,
    pub indirect_batch_sets: u64,
    pub proof_frames: u64,
    pub renderer_fixture_validated: bool,
    pub renderer_validation_failures: u64,
}

impl RendererMetrics {
    pub fn final_path_active(&self) -> bool {
        self.gpu_preprocessing_active
            && self.gpu_culling_active
            && self.indirect_drawing_active
            && self.occlusion_culling_views > 0
            && self.hzb_views > 0
            && self.indirect_phase_buffers > 0
            && self.indirect_batch_sets > 0
            && self.proof_frames > 0
            && self.renderer_validation_failures == 0
    }
}

#[derive(Default)]
struct RendererProofState {
    gpu_preprocessing: AtomicBool,
    gpu_culling: AtomicBool,
    indirect_drawing: AtomicBool,
    occlusion_views: AtomicU64,
    hzb_views: AtomicU64,
    indirect_phases: AtomicU64,
    indirect_batch_sets: AtomicU64,
    frames: AtomicU64,
}

#[derive(Resource, Clone, Default)]
struct RendererProofBridge(Arc<RendererProofState>);

fn sample_renderer_path(
    bridge: Res<RendererProofBridge>,
    support: Option<Res<GpuPreprocessingSupport>>,
    indirect: Option<Res<IndirectParametersBuffers>>,
    views: Query<Option<&ViewDepthPyramid>, With<OcclusionCulling>>,
) {
    let Some(support) = support else { return };
    let preprocessing = support.is_available();
    let culling = support.max_supported_mode == GpuPreprocessingMode::Culling;
    let (phase_count, batch_sets, indirect_active) =
        indirect.as_deref().map_or((0, 0, false), |buffers| {
            let phases = buffers.len() as u64;
            let batch_sets = buffers
                .values()
                .map(|phase| {
                    phase.batch_set_count(true) as u64 + phase.batch_set_count(false) as u64
                })
                .sum::<u64>();
            let active = buffers.values().any(|phase| {
                phase.indexed.data_buffer().is_some() || phase.non_indexed.data_buffer().is_some()
            });
            (phases, batch_sets, active)
        });
    let occlusion_views = views.iter().count() as u64;
    let hzb_views = views.iter().filter(|pyramid| pyramid.is_some()).count() as u64;
    bridge
        .0
        .gpu_preprocessing
        .fetch_or(preprocessing, Ordering::Relaxed);
    bridge.0.gpu_culling.fetch_or(culling, Ordering::Relaxed);
    bridge
        .0
        .indirect_drawing
        .fetch_or(indirect_active, Ordering::Relaxed);
    bridge
        .0
        .occlusion_views
        .fetch_max(occlusion_views, Ordering::Relaxed);
    bridge.0.hzb_views.fetch_max(hzb_views, Ordering::Relaxed);
    bridge
        .0
        .indirect_phases
        .fetch_max(phase_count, Ordering::Relaxed);
    bridge
        .0
        .indirect_batch_sets
        .fetch_max(batch_sets, Ordering::Relaxed);
    bridge.0.frames.fetch_add(1, Ordering::Relaxed);
}

fn sync_renderer_metrics(bridge: Res<RendererProofBridge>, mut metrics: ResMut<RendererMetrics>) {
    metrics.gpu_preprocessing_active = bridge.0.gpu_preprocessing.load(Ordering::Relaxed);
    metrics.gpu_culling_active = bridge.0.gpu_culling.load(Ordering::Relaxed);
    metrics.indirect_drawing_active = bridge.0.indirect_drawing.load(Ordering::Relaxed);
    metrics.occlusion_culling_views = bridge.0.occlusion_views.load(Ordering::Relaxed);
    metrics.hzb_views = bridge.0.hzb_views.load(Ordering::Relaxed);
    metrics.indirect_phase_buffers = bridge.0.indirect_phases.load(Ordering::Relaxed);
    metrics.indirect_batch_sets = bridge.0.indirect_batch_sets.load(Ordering::Relaxed);
    metrics.proof_frames = bridge.0.frames.load(Ordering::Relaxed);
}

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct TerrainExtension {
    #[texture(100)]
    #[sampler(101)]
    layer_0: Option<Handle<Image>>,
    #[texture(102)]
    #[sampler(103)]
    layer_1: Option<Handle<Image>>,
    #[texture(104)]
    #[sampler(105)]
    layer_2: Option<Handle<Image>>,
    #[texture(106)]
    #[sampler(107)]
    layer_3: Option<Handle<Image>>,
    #[texture(108)]
    #[sampler(109)]
    layer_4: Option<Handle<Image>>,
    #[texture(110)]
    #[sampler(111)]
    layer_5: Option<Handle<Image>>,
    #[uniform(112)]
    settings: TerrainSettings,
}

#[derive(ShaderType, Reflect, Debug, Clone)]
struct TerrainSettings {
    tiling_and_layer_count: Vec4,
    fallback_weights_0: Vec4,
    fallback_weights_1: Vec4,
}

impl TerrainExtension {
    pub fn from_quadrant(
        terrain: &TerrainSnapshot,
        quadrant: u8,
        catalog: &AssetCatalog,
        asset_server: &AssetServer,
    ) -> Result<(Self, Vec<Handle<Image>>), String> {
        let mut textures: [Option<Handle<Image>>; 6] = std::array::from_fn(|_| None);
        let layers = crate::streaming::quadrant_layers(terrain, quadrant)?;
        let mut handles = Vec::with_capacity(layers.len());
        for (target, layer) in textures.iter_mut().zip(&layers) {
            if layer.is_base && layer.texture_form_id == 0 {
                continue;
            }
            let path = catalog
                .landscape_diffuse(layer.texture_form_id)
                .ok_or_else(|| {
                    format!(
                        "LAND {:08X} quadrant {quadrant} texture {:08X} has no diffuse image",
                        terrain.cell_id, layer.texture_form_id
                    )
                })?;
            let handle = asset_server.load(path.to_owned());
            *target = Some(handle.clone());
            handles.push(handle);
        }
        Ok((
            Self {
                layer_0: textures[0].clone(),
                layer_1: textures[1].clone(),
                layer_2: textures[2].clone(),
                layer_3: textures[3].clone(),
                layer_4: textures[4].clone(),
                layer_5: textures[5].clone(),
                settings: TerrainSettings {
                    tiling_and_layer_count: Vec4::new(8.0, 8.0, layers.len() as f32, 0.0),
                    fallback_weights_0: Vec4::new(1.0, 0.0, 0.0, 0.0),
                    fallback_weights_1: Vec4::ZERO,
                },
            },
            handles,
        ))
    }

    pub(crate) fn fixture(textures: [Handle<Image>; 6]) -> Self {
        Self {
            layer_0: Some(textures[0].clone()),
            layer_1: Some(textures[1].clone()),
            layer_2: Some(textures[2].clone()),
            layer_3: Some(textures[3].clone()),
            layer_4: Some(textures[4].clone()),
            layer_5: Some(textures[5].clone()),
            settings: TerrainSettings {
                tiling_and_layer_count: Vec4::new(8.0, 8.0, 6.0, 0.0),
                fallback_weights_0: Vec4::X,
                fallback_weights_1: Vec4::ZERO,
            },
        }
    }
}

impl Default for TerrainExtension {
    fn default() -> Self {
        Self {
            layer_0: None,
            layer_1: None,
            layer_2: None,
            layer_3: None,
            layer_4: None,
            layer_5: None,
            settings: TerrainSettings {
                tiling_and_layer_count: Vec4::new(8.0, 8.0, 0.0, 0.0),
                fallback_weights_0: Vec4::X,
                fallback_weights_1: Vec4::ZERO,
            },
        }
    }
}

impl MaterialExtension for TerrainExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/terrain.wgsl".into()
    }
}

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct WaterExtension {
    #[uniform(100)]
    settings: WaterSettings,
    #[texture(101)]
    #[sampler(102)]
    reflection: Option<Handle<Image>>,
    #[texture(103)]
    #[sampler(104)]
    flow_normal: Option<Handle<Image>>,
}

#[derive(ShaderType, Reflect, Debug, Clone)]
struct WaterSettings {
    wave_scale_speed_strength: Vec4,
    flow_direction: Vec4,
}

impl Default for WaterExtension {
    fn default() -> Self {
        Self {
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, 0.32, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, 0.0),
            },
            reflection: None,
            flow_normal: None,
        }
    }
}

impl WaterExtension {
    pub fn with_reflection(reflection: Handle<Image>, flow_normal: Option<Handle<Image>>) -> Self {
        let has_flow_normal = flow_normal.is_some() as u8 as f32;
        Self {
            reflection: Some(reflection),
            flow_normal,
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, 0.32, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, has_flow_normal),
            },
        }
    }
}

impl MaterialExtension for WaterExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/water.wgsl".into()
    }
}

fn animate_water_materials(
    time: Res<Time>,
    config: Option<Res<crate::config::EngineConfig>>,
    mut materials: ResMut<Assets<WaterMaterial>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = std::time::Instant::now();
    let elapsed = if config.is_some_and(|config| config.terrain_water_fixture) {
        1.0
    } else {
        time.elapsed_secs()
    };
    for (_, material) in materials.iter_mut() {
        material.extension.settings.wave_scale_speed_strength.w = elapsed;
    }
    profiler.record_elapsed("render/water_animation", started);
}

#[derive(Resource, Clone)]
pub struct WaterReflectionTexture(pub Handle<Image>);

#[derive(Component)]
struct WaterReflectionCamera;

fn setup_water_reflection(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let image = images.add(Image::new_target_texture(
        1024,
        576,
        bevy::render::render_resource::TextureFormat::Rgba8Unorm,
        Some(bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb),
    ));
    commands.insert_resource(WaterReflectionTexture(image.clone()));
    commands.spawn((
        Camera3d::default(),
        Camera {
            order: -1,
            invert_culling: true,
            is_active: false,
            ..default()
        },
        RenderTarget::Image(image.into()),
        Transform::default(),
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::layer(0),
        WaterReflectionCamera,
    ));
}

fn update_water_reflection_camera(
    main_camera: Query<
        &GlobalTransform,
        (
            With<crate::world::components::StreamingCamera>,
            Without<WaterReflectionCamera>,
        ),
    >,
    water: Query<&GlobalTransform, With<crate::world::components::WaterSurface>>,
    mut reflection_camera: Query<(&mut Transform, &mut Camera), With<WaterReflectionCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = std::time::Instant::now();
    let (Ok(main), Ok((mut reflection, mut camera))) =
        (main_camera.single(), reflection_camera.single_mut())
    else {
        return;
    };
    let Some(surface) = water.iter().min_by(|left, right| {
        let left_distance = (left.translation().y - main.translation().y).abs();
        let right_distance = (right.translation().y - main.translation().y).abs();
        left_distance.total_cmp(&right_distance)
    }) else {
        camera.is_active = false;
        return;
    };
    let water_y = surface.translation().y;
    *reflection = reflected_camera_transform(main, water_y);
    camera.is_active = true;
    profiler.record_elapsed("render/water_reflection_camera", started);
}

fn reflected_camera_transform(main: &GlobalTransform, water_y: f32) -> Transform {
    let mut position = main.translation();
    position.y = water_y * 2.0 - position.y;
    let mut forward = main.forward().as_vec3();
    forward.y = -forward.y;
    Transform::from_translation(position).looking_to(forward, Vec3::Y)
}

/// Rewrites the alpha of every loaded mesh's vertex colours to full opacity, leaving every RGB
/// triple bit-identical.
///
/// Skyrim's `COLOR_0` alpha is a per-vertex shader parameter, not opacity. The vendored exporter
/// writes it as a `Float32x4` colour (`vendor/project-wormhole-nif/src/model/model.rs:175-184`) and
/// the converter publishes the threshold its material tests as `alphaCutoff = threshold / 255`
/// (`crates/converter/src/material.rs:449-452`). Bevy's PBR shader builds the alpha its `MASK` test
/// compares as `vertex_color.a * texture.a`: `pbr_input.material.base_color = in.color`
/// (`bevy_pbr-0.19.0/src/render/pbr_fragment.wgsl:54-56`) is then multiplied by the sampled base
/// colour texture (`bevy_pbr-0.19.0/src/render/pbr_fragment.wgsl:193-194`) before `alpha_discard`
/// compares it with the cutoff (`bevy_pbr-0.19.0/src/render/pbr_functions.wgsl:119-128`). A foliage
/// fragment whose vertex alpha is low is therefore discarded however dense the texel under it is,
/// so an alpha-tested canopy loses everything below its own cutoff and the trees render as a spray
/// of dots beside a solid trunk.
///
/// What the test compares is what is wrong, not the cutoff, so the fix belongs at load rather than
/// in the converter: the converted set keeps the `COLOR_0` channel it already carries and needs no
/// reconversion, and only the alpha is written, so Skyrim's baked per-vertex shading comes through
/// untouched.
///
/// The system reads the two messages a mesh is published with, [`AssetEvent::Added`] and
/// [`AssetEvent::Modified`], and marks the asset modified only when a rewrite really happened, so
/// the `AssetEvent::Modified` it queues cannot bring it back to the same mesh for ever.
fn force_opaque_vertex_colours(
    mut meshes: ResMut<Assets<Mesh>>,
    mut events: MessageReader<AssetEvent<Mesh>>,
) {
    for event in events.read() {
        let id = match event {
            AssetEvent::Added { id } | AssetEvent::Modified { id } => *id,
            _ => continue,
        };
        let Some(mut mesh) = meshes.get_mut(id) else {
            continue;
        };
        // The mesh is taken with change detection bypassed, and marked modified only when
        // something is really written: a mesh whose alphas are already opaque is left alone, and
        // being left alone is what stops the `AssetEvent::Modified` this system queues from
        // bringing it back to the same mesh for ever.
        match force_opaque_vertex_alpha(mesh.bypass_change_detection()) {
            // Every alpha is already opaque, or the colour has no alpha component to rewrite.
            Ok(0) => {}
            Ok(vertices) => {
                // Marking the asset modified is what makes the render world re-extract the
                // rewritten vertices, and it is reached only for a mesh whose alpha really
                // changed: everything above wrote through `bypass_change_detection`.
                mesh.into_inner();
                debug!(
                    vertices = vertices,
                    "vertex colour alpha forced to 1.0: Skyrim's `COLOR_0` alpha is a shader parameter, not opacity"
                );
            }
            Err(reason) => debug!(
                reason = reason,
                "a loaded mesh's vertex colours were not rewritten"
            ),
        }
    }
}

/// Sets the alpha of every vertex colour in `mesh` to full opacity in the attribute's own encoding,
/// and reports how many vertices changed - or why the mesh was left alone. Nothing but the alpha
/// component is ever written.
///
/// `Ok(0)` is the ordinary case with nothing to do: every alpha is already opaque, or the colour
/// has no alpha component at all (one, two or three components), which is left as it is rather than
/// widened with an invented one.
fn force_opaque_vertex_alpha(mesh: &mut Mesh) -> Result<usize, &'static str> {
    /// One pass over a four-component colour: every vertex's alpha to `opaque`, the RGB untouched,
    /// counting the vertices whose alpha was not already there.
    fn set_opaque<T: Copy + PartialEq>(colours: &mut [[T; 4]], opaque: T) -> usize {
        let mut changed = 0;
        for colour in colours {
            if colour[3] != opaque {
                colour[3] = opaque;
                changed += 1;
            }
        }
        changed
    }

    let colours = match mesh.try_attribute_mut_option(Mesh::ATTRIBUTE_COLOR) {
        Ok(Some(colours)) => colours,
        // No colour attribute: nothing to rewrite, and nothing worth saying about it.
        Ok(None) => return Ok(0),
        // The only error `try_attribute_mut_option` reports is the data having been extracted - a
        // missing attribute arrives as `Ok(None)` - because Bevy hands a mesh's vertex data to the
        // render world and drops the main-world copy unless `RenderAssetUsages::MAIN_WORLD` is set.
        // Every mesh this engine streams keeps it: the scenes are loaded with the glTF loader's
        // defaults, and `GltfLoaderSettings::default` is `RenderAssetUsages::default()`
        // (`bevy_gltf-0.19.0/src/loader/mod.rs:220-224`), which keeps both copies
        // (`bevy_asset-0.19.0/src/render_asset.rs:40-46`).
        Err(_) => return Err("its vertex data is in the render world"),
    };
    let changed = match colours {
        // The two encodings this pipeline actually produces: the glTF loader widens every
        // `COLOR_0` to `Float32x4` (`bevy_gltf-0.19.0/src/vertex_attributes.rs`), and
        // `build_terrain_quadrant_mesh` inserts `Float32x4` colours of its own, already opaque.
        VertexAttributeValues::Float32x4(colours) => set_opaque(colours, 1.0),
        // The rest of the float family: full opacity is the same number in all of them.
        VertexAttributeValues::Float64x4(colours) => set_opaque(colours, 1.0),
        // Normalised integer encodings: the encoding's own full-scale value.
        VertexAttributeValues::Unorm8x4(colours) => set_opaque(colours, u8::MAX),
        VertexAttributeValues::Unorm16x4(colours) => set_opaque(colours, u16::MAX),
        VertexAttributeValues::Snorm8x4(colours) => set_opaque(colours, i8::MAX),
        VertexAttributeValues::Snorm16x4(colours) => set_opaque(colours, i16::MAX),
        // BGRA is the same four bytes in another order; alpha is the fourth of them in both.
        VertexAttributeValues::Unorm8x4Bgra(colours) => set_opaque(colours, u8::MAX),
        // One word of three 10-bit channels and two alpha bits, where the top two bits are the
        // alpha and 3 is that field's 1.0.
        VertexAttributeValues::Unorm10_10_10_2(colours) => {
            const OPAQUE: u32 = 3;
            let mut changed = 0;
            for colour in colours.iter_mut() {
                if *colour >> 30 != OPAQUE {
                    *colour = (*colour & 0x3fff_ffff) | (OPAQUE << 30);
                    changed += 1;
                }
            }
            changed
        }
        // An unnormalised integer channel is not a colour encoding - the shader reads `in.color` as
        // a `vec4<f32>`, so a `u32`/`i32` colour is never read as one - and what "opaque" means in
        // such a channel is not defined. Guessing would be a silent wrong write, so it is reported
        // and skipped.
        VertexAttributeValues::Uint8x4(_)
        | VertexAttributeValues::Sint8x4(_)
        | VertexAttributeValues::Uint16x4(_)
        | VertexAttributeValues::Sint16x4(_)
        | VertexAttributeValues::Uint32x4(_)
        | VertexAttributeValues::Sint32x4(_) => {
            return Err("an unnormalised integer colour encoding");
        }
        // A half-float colour would need the `half` crate to write its 1.0 - `f16` has no
        // `From<f32>`, and this crate cannot name the type, since `half` is `bevy_mesh`'s
        // dependency rather than one of this crate's - so it is reported and skipped instead.
        VertexAttributeValues::Float16x4(_) => return Err("a half-float colour encoding"),
        // One, two or three components: there is no alpha channel to rewrite, and widening the
        // colour would change a format the mesh is entitled to have.
        VertexAttributeValues::Uint8(_)
        | VertexAttributeValues::Uint8x2(_)
        | VertexAttributeValues::Sint8(_)
        | VertexAttributeValues::Sint8x2(_)
        | VertexAttributeValues::Unorm8(_)
        | VertexAttributeValues::Unorm8x2(_)
        | VertexAttributeValues::Snorm8(_)
        | VertexAttributeValues::Snorm8x2(_)
        | VertexAttributeValues::Uint16(_)
        | VertexAttributeValues::Uint16x2(_)
        | VertexAttributeValues::Sint16(_)
        | VertexAttributeValues::Sint16x2(_)
        | VertexAttributeValues::Unorm16(_)
        | VertexAttributeValues::Unorm16x2(_)
        | VertexAttributeValues::Snorm16(_)
        | VertexAttributeValues::Snorm16x2(_)
        | VertexAttributeValues::Float16(_)
        | VertexAttributeValues::Float16x2(_)
        | VertexAttributeValues::Float32(_)
        | VertexAttributeValues::Float32x2(_)
        | VertexAttributeValues::Float32x3(_)
        | VertexAttributeValues::Uint32(_)
        | VertexAttributeValues::Uint32x2(_)
        | VertexAttributeValues::Uint32x3(_)
        | VertexAttributeValues::Sint32(_)
        | VertexAttributeValues::Sint32x2(_)
        | VertexAttributeValues::Sint32x3(_)
        | VertexAttributeValues::Float64(_)
        | VertexAttributeValues::Float64x2(_)
        | VertexAttributeValues::Float64x3(_) => 0,
    };
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{MeshVertexAttribute, PrimitiveTopology};
    use bevy::render::render_resource::VertexFormat;

    #[test]
    fn reflects_camera_above_and_below_the_water_plane() {
        let above = GlobalTransform::from(
            Transform::from_xyz(2.0, 10.0, 4.0).looking_to(Vec3::new(0.0, -0.5, -1.0), Vec3::Y),
        );
        let reflected = reflected_camera_transform(&above, 3.0);
        assert!((reflected.translation.y + 4.0).abs() < 1.0e-5);
        assert!(reflected.forward().y > 0.0);

        let below = GlobalTransform::from(
            Transform::from_xyz(2.0, -4.0, 4.0).looking_to(Vec3::new(0.0, 0.5, -1.0), Vec3::Y),
        );
        let reflected = reflected_camera_transform(&below, 3.0);
        assert!((reflected.translation.y - 10.0).abs() < 1.0e-5);
        assert!(reflected.forward().y < 0.0);
    }

    #[test]
    fn animated_water_advances_the_shader_phase() {
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<Assets<WaterMaterial>>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, animate_water_materials);
        let handle = app
            .world_mut()
            .resource_mut::<Assets<WaterMaterial>>()
            .add(WaterMaterial {
                base: StandardMaterial::default(),
                extension: WaterExtension::default(),
            });
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_secs(2));
        app.update();
        let material = app
            .world()
            .resource::<Assets<WaterMaterial>>()
            .get(&handle)
            .unwrap();
        assert_eq!(material.extension.settings.wave_scale_speed_strength.w, 2.0);
    }

    #[test]
    fn final_renderer_requires_every_gpu_path_signal() {
        let complete = RendererMetrics {
            gpu_preprocessing_active: true,
            gpu_culling_active: true,
            indirect_drawing_active: true,
            occlusion_culling_views: 1,
            hzb_views: 1,
            indirect_phase_buffers: 1,
            indirect_batch_sets: 1,
            proof_frames: 1,
            ..default()
        };
        assert!(complete.final_path_active());
        assert!(
            !RendererMetrics {
                hzb_views: 0,
                ..complete.clone()
            }
            .final_path_active()
        );
        assert!(
            !RendererMetrics {
                renderer_validation_failures: 1,
                ..complete
            }
            .final_path_active()
        );
    }

    /// A mesh whose only attribute is the colour one: the rewrite reads nothing else.
    fn colour_mesh(colours: VertexAttributeValues) -> Mesh {
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colours);
        mesh
    }

    /// The same, for a colour in an encoding `Mesh::ATTRIBUTE_COLOR` cannot carry: Bevy refuses a
    /// value whose format differs from the attribute's declared one
    /// (`bevy_mesh-0.19.0/src/mesh.rs:396-403`, "Invalid attribute format for Vertex_Color"), so a
    /// colour in another encoding reaches a mesh under an attribute that declares that encoding
    /// and carries the id the rewrite looks the attribute up by.
    fn colour_mesh_in(colours: VertexAttributeValues, format: VertexFormat) -> Mesh {
        let attribute = MeshVertexAttribute::new("Vertex_Color", 5, format);
        assert_eq!(
            attribute.id,
            Mesh::ATTRIBUTE_COLOR.id,
            "the fixture's colour attribute must carry the id the rewrite looks up"
        );
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        mesh.insert_attribute(attribute, colours);
        mesh
    }

    /// Every vertex colour of a float mesh, which also asserts the attribute kept its encoding.
    fn float_colours(mesh: &Mesh) -> Vec<[f32; 4]> {
        match mesh
            .attribute(Mesh::ATTRIBUTE_COLOR)
            .expect("the mesh has colours")
        {
            VertexAttributeValues::Float32x4(colours) => colours.clone(),
            other => panic!("the colour attribute changed encoding: {other:?}"),
        }
    }

    /// The same, as bits, so "bit-identical" is what is compared and not "close enough".
    fn float_colour_bits(mesh: &Mesh) -> Vec<[u32; 4]> {
        float_colours(mesh)
            .iter()
            .map(|colour| colour.map(f32::to_bits))
            .collect()
    }

    /// The rewrite on the encoding Skyrim's converted models actually carry: every alpha becomes
    /// exactly 1.0, every RGB triple stays bit-identical, and the count is the vertices whose alpha
    /// was not already there.
    #[test]
    fn float_colour_keeps_its_rgb_and_loses_its_alpha() {
        let colours = vec![
            [0.0f32, 0.0, 0.0, 0.0],
            [0.929_411_77, 0.4, 0.2, 0.25],
            [0.2, 0.6, 0.9, 0.0],
            [1.0, 1.0, 1.0, 1.0],
        ];
        let mut mesh = colour_mesh(VertexAttributeValues::Float32x4(colours.clone()));
        let mut expected = colours.clone();
        for colour in &mut expected {
            colour[3] = 1.0;
        }

        assert_eq!(
            force_opaque_vertex_alpha(&mut mesh),
            Ok(3),
            "the three below full opacity are rewritten and the opaque one is not"
        );
        assert_eq!(
            float_colour_bits(&mesh),
            expected
                .iter()
                .map(|colour| colour.map(f32::to_bits))
                .collect::<Vec<_>>()
        );
    }

    /// A byte-per-channel colour is rewritten in its own encoding: 255 is that encoding's opaque
    /// alpha, and the RGB bytes are untouched.
    #[test]
    fn byte_colour_alpha_is_forced_opaque_in_its_own_encoding() {
        let colours = vec![[12u8, 34, 56, 0], [200, 100, 50, 64], [7, 8, 9, 255]];
        let mut mesh = colour_mesh_in(
            VertexAttributeValues::Unorm8x4(colours.clone()),
            VertexFormat::Unorm8x4,
        );
        assert_eq!(force_opaque_vertex_alpha(&mut mesh), Ok(2));

        let VertexAttributeValues::Unorm8x4(rewritten) =
            mesh.attribute(Mesh::ATTRIBUTE_COLOR).unwrap()
        else {
            panic!("the rewrite must not change the attribute's encoding");
        };
        for (before, after) in colours.iter().zip(rewritten) {
            assert_eq!(after[3], u8::MAX, "a normalised byte's opaque alpha is 255");
            assert_eq!(&after[..3], &before[..3], "the RGB bytes are untouched");
        }
    }

    /// A colour with no alpha component is left exactly as it is: widening it would invent an
    /// alpha, and a three-component colour is a format a mesh is entitled to have.
    #[test]
    fn three_component_colour_is_left_untouched() {
        let colours = vec![
            [0.929_411_77f32, 0.929_411_77, 0.929_411_77],
            [0.0, 0.0, 0.0],
        ];
        let mut mesh = colour_mesh_in(
            VertexAttributeValues::Float32x3(colours.clone()),
            VertexFormat::Float32x3,
        );
        assert_eq!(force_opaque_vertex_alpha(&mut mesh), Ok(0));
        assert_eq!(
            mesh.attribute(Mesh::ATTRIBUTE_COLOR).unwrap(),
            &VertexAttributeValues::Float32x3(colours),
            "not rewritten and not widened"
        );
    }

    /// A mesh with no colour attribute is untouched and not counted.
    #[test]
    fn mesh_without_vertex_colours_is_not_touched() {
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32, 0.0, 0.0]; 3]);
        let attributes = mesh.attributes().count();

        assert_eq!(force_opaque_vertex_alpha(&mut mesh), Ok(0));
        assert_eq!(
            mesh.attributes().count(),
            attributes,
            "no attribute is added"
        );
        assert!(!mesh.contains_attribute(Mesh::ATTRIBUTE_COLOR));
    }

    /// An encoding this engine does not read as a colour is skipped, not guessed at: writing 255
    /// into an unnormalised `u32` channel would be a silent wrong write, and no converted mesh has
    /// been seen with one.
    #[test]
    fn unnormalised_integer_colour_is_skipped_not_guessed() {
        let colours = vec![[0u8, 12, 240, 0], [64, 128, 255, 64]];
        let mut mesh = colour_mesh_in(
            VertexAttributeValues::Uint8x4(colours.clone()),
            VertexFormat::Uint8x4,
        );
        assert!(force_opaque_vertex_alpha(&mut mesh).is_err());
        assert_eq!(
            mesh.attribute(Mesh::ATTRIBUTE_COLOR).unwrap(),
            &VertexAttributeValues::Uint8x4(colours),
            "the colour is left exactly as it arrived"
        );
    }

    /// Rewriting twice equals rewriting once, bit for bit, and the second pass reports that it has
    /// nothing left to do - which is what stops the system's own `AssetEvent::Modified` from
    /// bringing it back to the same mesh for ever.
    #[test]
    fn forcing_opaque_is_idempotent() {
        let colours = vec![
            [0.1f32, 0.2, 0.3, 0.0],
            [0.4, 0.5, 0.6, 0.251],
            [0.7, 0.8, 0.9, 1.0],
        ];
        let mut once = colour_mesh(VertexAttributeValues::Float32x4(colours.clone()));
        let mut twice = colour_mesh(VertexAttributeValues::Float32x4(colours));

        assert_eq!(force_opaque_vertex_alpha(&mut once), Ok(2));
        assert_eq!(force_opaque_vertex_alpha(&mut twice), Ok(2));
        assert_eq!(
            force_opaque_vertex_alpha(&mut twice),
            Ok(0),
            "the second pass has nothing left to rewrite"
        );
        assert_eq!(float_colour_bits(&twice), float_colour_bits(&once));
    }

    /// The bug in one assertion: a `MASK` material has a cutoff, Bevy discards a fragment whose
    /// `vertex_alpha * texture_alpha` is below it, and a vertex whose own alpha is low can never
    /// reach it whatever the texture says, so the canopy renders as the fragments whose texels are
    /// opaque and nothing else.
    #[test]
    fn every_vertex_survives_the_cutoff_after_the_rewrite() {
        const CUTOFF: f32 = 0.5;
        let survives = |colour: [f32; 4], texture_alpha: f32| colour[3] * texture_alpha >= CUTOFF;
        let colours: Vec<[f32; 4]> = [0.0, 0.25, 0.4, 0.9, 1.0]
            .into_iter()
            .map(|alpha| [0.5, 0.5, 0.5, alpha])
            .collect();
        let mut mesh = colour_mesh(VertexAttributeValues::Float32x4(colours));
        assert_eq!(
            float_colours(&mesh)
                .iter()
                .filter(|colour| !survives(**colour, 1.0))
                .count(),
            3,
            "the densest texel cannot save the vertices the cutoff discards"
        );

        assert_eq!(force_opaque_vertex_alpha(&mut mesh), Ok(4));
        assert!(
            float_colours(&mesh)
                .iter()
                .all(|colour| survives(*colour, 1.0)),
            "every vertex passes the test the shader runs, once its alpha is 1.0"
        );
    }

    #[derive(Resource, Default)]
    struct CollectedMeshEvents(Vec<AssetEvent<Mesh>>);

    /// The rewrite runs on the events a loaded mesh publishes, and the `AssetEvent::Modified` it
    /// queues for a mesh it rewrote does not come back to rewrite anything: if it did, every
    /// rewritten mesh would publish one more `Modified` for ever.
    #[test]
    fn the_rewrite_runs_on_an_added_mesh_and_does_not_retrigger_itself() {
        fn collect(
            mut events: MessageReader<AssetEvent<Mesh>>,
            mut collected: ResMut<CollectedMeshEvents>,
        ) {
            collected.0.extend(events.read().cloned());
        }

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_resource::<CollectedMeshEvents>()
            .add_systems(
                PostUpdate,
                (
                    force_opaque_vertex_colours.after(AssetEventSystems),
                    collect.after(AssetEventSystems),
                ),
            );
        let handle = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(colour_mesh(VertexAttributeValues::Float32x4(vec![
                [0.5, 0.5, 0.5, 0.0],
                [0.4, 0.4, 0.4, 1.0],
            ])));
        let colours = |app: &App| {
            float_colours(
                app.world()
                    .resource::<Assets<Mesh>>()
                    .get(&handle)
                    .expect("the fixture mesh is still there"),
            )
        };

        // Frame 1: the `Added` message is published and the rewrite happens in the same frame, in
        // front of the render world's extraction.
        app.update();
        assert_eq!(
            colours(&app),
            vec![[0.5, 0.5, 0.5, 1.0], [0.4, 0.4, 0.4, 1.0]]
        );

        // Frame 2 publishes the one `Modified` the rewrite queued, and that pass has nothing left
        // to rewrite.
        app.world_mut()
            .resource_mut::<CollectedMeshEvents>()
            .0
            .clear();
        app.update();
        assert_eq!(
            app.world()
                .resource::<CollectedMeshEvents>()
                .0
                .iter()
                .filter(|event| matches!(event, AssetEvent::Modified { .. }))
                .count(),
            1,
            "a rewritten mesh is marked modified once"
        );

        // Frame 3: nothing follows, so the system is not feeding itself.
        app.world_mut()
            .resource_mut::<CollectedMeshEvents>()
            .0
            .clear();
        app.update();
        assert!(
            app.world().resource::<CollectedMeshEvents>().0.is_empty(),
            "a pass with nothing to rewrite publishes nothing"
        );
        assert_eq!(
            colours(&app),
            vec![[0.5, 0.5, 0.5, 1.0], [0.4, 0.4, 0.4, 1.0]]
        );
    }
}
