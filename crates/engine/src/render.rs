use crate::{
    profiling::ProfilingState,
    world::{cache::TerrainSnapshot, database::AssetCatalog},
};
use bevy::{
    asset::embedded_asset,
    camera::{RenderTarget, visibility::RenderLayers},
    core_pipeline::{mip_generation::experimental::depth::ViewDepthPyramid, prepass::DepthPrepass},
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
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
    quadrant_origin: Vec4,
    fallback_weights_0: Vec4,
    fallback_weights_1: Vec4,
    weight_source: Vec4,
    weights: [Vec4; WEIGHT_FIELD_WORDS],
}

/// The sample grid of one terrain quadrant: 17x17 opacities on the same 128-unit spacing as the
/// quadrant's mesh columns, which is what LAND's `VTXT` entries index.
pub(crate) const QUADRANT_WEIGHT_SAMPLES: usize = 17;

/// Samples per overlay that a quadrant can carry: the base layer plus five overlays, the runtime's
/// own cap (`quadrant_layers` rejects more).
pub(crate) const OVERLAY_WEIGHT_SLOTS: usize = 5;

/// Vec4s per overlay in the weight field: `289` samples four to a vec4, padded to whole vec4s so
/// every overlay starts on a 16-byte boundary. The shader declares the same numbers
/// (`WEIGHT_GRID_WORDS` in `terrain.wgsl`).
pub(crate) const OVERLAY_WEIGHT_WORDS: usize =
    (QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES).div_ceil(4);

/// The whole field the material's uniform carries: five overlays of [`OVERLAY_WEIGHT_WORDS`].
pub(crate) const WEIGHT_FIELD_WORDS: usize = OVERLAY_WEIGHT_SLOTS * OVERLAY_WEIGHT_WORDS;

/// The descriptor Bevy's asset loader uses for a `.ktx2` that requests no sampler of its own:
/// `ImagePlugin::default`'s `ImageSamplerDescriptor::linear()`, whose address modes are
/// `ImageAddressMode`'s default.
fn default_linear_sampler() -> ImageSamplerDescriptor {
    ImageSamplerDescriptor::linear()
}

/// The sampler a terrain layer texture needs: the shader tiles each layer `tiling` times across
/// the cell (`uv * 8`), so the address mode must repeat. Bevy's default sampler clamps to the
/// edge, which stretches the texture's last texel column, row and corner across most of a cell -
/// long streaks where the edge column is stretched, and one flat colour where the corner texel
/// covers everything past the first tile.
fn terrain_layer_sampler() -> ImageSamplerDescriptor {
    ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        address_mode_w: ImageAddressMode::Repeat,
        ..default_linear_sampler()
    }
}

/// Packs a quadrant's overlay weight grids into the material uniform's field. Sample `s` of
/// overlay `o` lands in component `s % 4` of word `o * OVERLAY_WEIGHT_WORDS + s / 4`, the layout
/// `grid_weight` in `terrain.wgsl` reads.
fn weight_field(overlay_weights: &[Vec<f32>]) -> [Vec4; WEIGHT_FIELD_WORDS] {
    let mut field = [Vec4::ZERO; WEIGHT_FIELD_WORDS];
    for (overlay, grid) in overlay_weights
        .iter()
        .take(OVERLAY_WEIGHT_SLOTS)
        .enumerate()
    {
        for (sample, weight) in grid
            .iter()
            .enumerate()
            .take(QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES)
        {
            field[overlay * OVERLAY_WEIGHT_WORDS + sample / 4][sample % 4] = *weight;
        }
    }
    field
}

impl TerrainSettings {
    /// The settings of one streamed quadrant: the tiling the shader repeats every layer by, the
    /// quadrant's origin inside the cell, and its overlay weight field - which the shader
    /// interpolates itself, so the weights reach the fragment stage exactly as LAND records them.
    fn for_quadrant(quadrant: u8, layers: usize, overlay_weights: &[Vec<f32>]) -> Self {
        Self {
            tiling_and_layer_count: Vec4::new(8.0, 8.0, layers as f32, 0.0),
            quadrant_origin: Vec4::new(f32::from(quadrant % 2), f32::from(quadrant / 2), 0.0, 0.0),
            fallback_weights_0: Vec4::X,
            fallback_weights_1: Vec4::ZERO,
            weight_source: Vec4::X,
            weights: weight_field(overlay_weights),
        }
    }

    /// The settings of a material that carries no weight field: only the tiling and the fallback
    /// weight, with the shader reading the packed vertex attributes instead. The synthetic
    /// fixtures are built without an `Assets<Image>` or a LAND snapshot, so this is how they are
    /// rendered; no streamed quadrant uses it.
    fn vertex_weights_only(layers: f32) -> Self {
        Self {
            tiling_and_layer_count: Vec4::new(8.0, 8.0, layers, 0.0),
            quadrant_origin: Vec4::ZERO,
            fallback_weights_0: Vec4::X,
            fallback_weights_1: Vec4::ZERO,
            weight_source: Vec4::ZERO,
            weights: [Vec4::ZERO; WEIGHT_FIELD_WORDS],
        }
    }
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
            let handle = asset_server
                .load_builder()
                .with_settings(|settings: &mut ImageLoaderSettings| {
                    settings.sampler = ImageSampler::Descriptor(terrain_layer_sampler());
                })
                .load(path.to_owned());
            *target = Some(handle.clone());
            handles.push(handle);
        }
        let overlay_weights = crate::streaming::quadrant_overlay_weights(terrain, quadrant)?;
        Ok((
            Self {
                layer_0: textures[0].clone(),
                layer_1: textures[1].clone(),
                layer_2: textures[2].clone(),
                layer_3: textures[3].clone(),
                layer_4: textures[4].clone(),
                layer_5: textures[5].clone(),
                settings: TerrainSettings::for_quadrant(quadrant, layers.len(), &overlay_weights),
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
            settings: TerrainSettings::vertex_weights_only(6.0),
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
            settings: TerrainSettings::vertex_weights_only(0.0),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::cache::TerrainLayerSnapshot;
    use bevy::image::ImageFilterMode;
    use bevy::mesh::VertexAttributeValues;

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

    /// A cell whose four quadrants each carry a base layer and one overlay with the same `VTXT`
    /// list, so a weight measured in one quadrant can be compared with its neighbour's.
    fn overlay_fixture(cell_id: u32, overlay: Vec<(u16, f32)>) -> TerrainSnapshot {
        let layers = (0..4)
            .flat_map(|quadrant| {
                [
                    TerrainLayerSnapshot {
                        texture_form_id: 1,
                        quadrant,
                        layer: 0,
                        is_base: true,
                        weights: Vec::new(),
                    },
                    TerrainLayerSnapshot {
                        texture_form_id: 7,
                        quadrant,
                        layer: 1,
                        is_base: false,
                        weights: overlay.clone(),
                    },
                ]
            })
            .collect();
        TerrainSnapshot {
            cell_id,
            width: 33,
            height: 33,
            heights: vec![0.0; 33 * 33],
            normals: [0, 0, 127].repeat(33 * 33),
            vertex_colors: vec![255; 33 * 33 * 3],
            layers,
            water_height: None,
            water_type_form_id: None,
        }
    }

    /// The VTXT list of an overlay that has full weight on both of a quadrant's x edge columns and
    /// none in between, so the samples one column inside an edge differ from the edge itself.
    fn edge_columns_overlay() -> Vec<(u16, f32)> {
        (0..QUADRANT_WEIGHT_SAMPLES as u16)
            .flat_map(|y| [0u16, 16].map(move |x| (y * 17 + x, 1.0)))
            .collect()
    }

    /// `terrain.wgsl`'s cell uv to quadrant-local uv, for the quadrant at `origin`.
    fn quadrant_uv(cell_uv: [f32; 2], origin: [f32; 2]) -> [f32; 2] {
        [cell_uv[0] * 2.0 - origin[0], cell_uv[1] * 2.0 - origin[1]]
    }

    /// The weight a fragment receives, mirroring `grid_weight` in `terrain.wgsl`: the
    /// quadrant-local coordinate scaled to the 17x17 sample grid, read bilinearly, clamped at the
    /// quadrant's edge.
    fn shader_weight(settings: &TerrainSettings, overlay: usize, local_uv: [f32; 2]) -> f32 {
        let last = (QUADRANT_WEIGHT_SAMPLES - 1) as f32;
        let sample = |x: usize, y: usize| -> f32 {
            let index = y * QUADRANT_WEIGHT_SAMPLES + x;
            settings.weights[overlay * OVERLAY_WEIGHT_WORDS + index / 4][index % 4]
        };
        let corner = [local_uv[0] * last, local_uv[1] * last];
        let blend = [corner[0] - corner[0].floor(), corner[1] - corner[1].floor()];
        let axis = |value: f32| -> (usize, usize) {
            let base = value.clamp(0.0, last) as usize;
            (base, (base + 1).min(last as usize))
        };
        let (west, east) = axis(corner[0]);
        let (north, south) = axis(corner[1]);
        let top = sample(west, north) * (1.0 - blend[0]) + sample(east, north) * blend[0];
        let bottom = sample(west, south) * (1.0 - blend[0]) + sample(east, south) * blend[0];
        top * (1.0 - blend[1]) + bottom * blend[1]
    }

    /// Defect A. The overlay weight field is the quadrant's own 17x17 sample grid. A layer that is
    /// full strength on one sample and absent on the next must reach the fragment as the linear
    /// interpolation of the two - `0.5` halfway between them, `0.25`/`0.75` at the quarter points -
    /// not as the sharpened `t^2` ramp the packed vertex attributes produce.
    #[test]
    fn weight_field_interpolates_linearly_between_samples() {
        let terrain = overlay_fixture(0x0001_2345, vec![(0, 1.0)]);
        let overlay_weights = crate::streaming::quadrant_overlay_weights(&terrain, 0).unwrap();
        assert_eq!(overlay_weights.len(), 1, "one overlay, one weight grid");
        let settings = TerrainSettings::for_quadrant(0, 2, &overlay_weights);
        assert_eq!(settings.weight_source, Vec4::X, "the field is in charge");

        let sample = |x: usize, y: usize| -> f32 {
            let index = y * QUADRANT_WEIGHT_SAMPLES + x;
            settings.weights[index / 4][index % 4]
        };
        assert_eq!(sample(0, 0), 1.0, "the sample itself");
        assert_eq!(sample(1, 0), 0.0, "and the empty sample beside it");
        assert_eq!(shader_weight(&settings, 0, [0.0, 0.0]), 1.0);

        // Vertex 0 is the quadrant's (x=0, y=0) sample; its x neighbour is empty, so travelling
        // one sample step in x crosses the 1.0 -> 0.0 edge.
        let step = 1.0 / 16.0;
        let halfway = shader_weight(&settings, 0, [0.5 * step, 0.0]);
        assert!(
            (halfway - 0.5).abs() < 0.01,
            "the midpoint of a 1.0 -> 0.0 edge must read 0.5, not the packed tangent's 0.25: {halfway}"
        );
        let quarter = shader_weight(&settings, 0, [0.25 * step, 0.0]);
        let three_quarters = shader_weight(&settings, 0, [0.75 * step, 0.0]);
        assert!(
            (quarter - 0.75).abs() < 0.01,
            "a quarter of the way in: {quarter}"
        );
        assert!(
            (three_quarters - 0.25).abs() < 0.01,
            "three quarters of the way in: {three_quarters}"
        );

        // Both axes: the field is bilinear, so the centre of the quad whose only full sample is
        // the corner reads 0.25 - the value Skyrim's interpolation gives, and not the one the
        // triangle split would if the weights stayed per vertex.
        let centre = shader_weight(&settings, 0, [0.5 * step, 0.5 * step]);
        assert!((centre - 0.25).abs() < 0.01, "the quad centre: {centre}");

        // The overlay slots the quadrant does not use stay empty, so an unused slot cannot leak
        // weight into the blend.
        for overlay in 1..OVERLAY_WEIGHT_SLOTS {
            assert_eq!(shader_weight(&settings, overlay, [0.0, 0.0]), 0.0);
        }
    }

    /// Defect A, the mechanism this replaced. `build_terrain_quadrant_mesh` still packs overlays
    /// 1-3 into `ATTRIBUTE_TANGENT` for materials without a weight field, and Bevy re-normalizes
    /// `world_tangent.xyz` in the vertex shader. Interpolating that carrier halfway across the
    /// same 1.0 -> 0.0 edge yields 0.25: the sharpened ramp the user sees as hard, stair-stepped
    /// transitions. Streamed quadrants carry the weight field above instead.
    #[test]
    fn packed_vertex_weights_sharpen_the_midpoint_to_a_quarter() {
        let terrain = overlay_fixture(0x0001_2345, vec![(0, 1.0)]);
        let mesh = crate::streaming::build_terrain_quadrant_mesh(&terrain, 0).unwrap();
        let VertexAttributeValues::Float32x4(tangents) =
            mesh.attribute(Mesh::ATTRIBUTE_TANGENT).unwrap()
        else {
            panic!("terrain tangents must be Float32x4");
        };
        let full = tangents[0];
        let empty = tangents[1];
        // The vertex shader normalizes xyz per vertex and interpolates the result; the fragment
        // shader only multiplies the interpolated xyz by the interpolated magnitude.
        let midpoint: Vec<f32> = (0..4)
            .map(|axis| (full[axis] + empty[axis]) * 0.5)
            .collect();
        let reconstructed = midpoint[0] * midpoint[3].abs();
        assert!(
            (reconstructed - 0.25).abs() < 0.01,
            "the packed path reads {reconstructed} where the linear weight is 0.5"
        );
    }

    /// Defect A, at a quadrant and at a cell boundary. Each quadrant's weight field holds that
    /// quadrant's own samples clamped at its edge, so where neighbouring data agrees on the shared
    /// column the two sides read the same weight - and they read the *edge* sample, not a blend
    /// with the column behind it.
    #[test]
    fn adjacent_quadrants_and_cells_agree_on_their_shared_edge() {
        let west_cell = overlay_fixture(0x0001_2345, edge_columns_overlay());
        let east_cell = overlay_fixture(0x0001_2346, edge_columns_overlay());
        let settings_of = |terrain: &TerrainSnapshot, quadrant: u8| {
            let weights = crate::streaming::quadrant_overlay_weights(terrain, quadrant).unwrap();
            TerrainSettings::for_quadrant(quadrant, weights.len() + 1, &weights)
        };
        let south_west = settings_of(&west_cell, 0);
        let south_east = settings_of(&west_cell, 1);
        let next_cell_south_west = settings_of(&east_cell, 0);

        // The quadrant boundary inside one cell: quadrant 0's east column and quadrant 1's west
        // column are the same column of ground, at cell uv x = 0.5. Both sides must read it.
        for cell_y in [0.0_f32, 0.25, 0.5] {
            let left = shader_weight(&south_west, 0, quadrant_uv([0.5, cell_y], [0.0, 0.0]));
            let right = shader_weight(&south_east, 0, quadrant_uv([0.5, cell_y], [1.0, 0.0]));
            assert!(
                (left - 1.0).abs() < 0.01,
                "quadrant 0's east column at cell y {cell_y}: {left}"
            );
            assert!(
                (right - 1.0).abs() < 0.01,
                "quadrant 1's west column at cell y {cell_y}: {right}"
            );
        }

        // The cell boundary: this cell's east edge against the next cell's west edge.
        for cell_y in [0.0_f32, 0.25, 0.5] {
            let left = shader_weight(&south_east, 0, quadrant_uv([1.0, cell_y], [1.0, 0.0]));
            let right = shader_weight(
                &next_cell_south_west,
                0,
                quadrant_uv([0.0, cell_y], [0.0, 0.0]),
            );
            assert!(
                (left - right).abs() < 0.01,
                "adjacent cells disagree at cell y {cell_y}: {left} against {right}"
            );
            assert!((left - 1.0).abs() < 0.01, "the shared cell edge: {left}");
        }

        // Half a sample inside that edge the field is halfway between its two samples, which is
        // what makes the measurements above evidence that the edge sample itself is read. Half a
        // sample is `0.5 / 16` of a quadrant, and half that again in cell units.
        let inside = shader_weight(
            &south_east,
            0,
            quadrant_uv([1.0 - 0.5 / 32.0, 0.0], [1.0, 0.0]),
        );
        assert!(
            (inside - 0.5).abs() < 0.01,
            "half a sample inside the edge the weight ramps to 0.5: {inside}"
        );
    }

    /// Defect B. The shader tiles every layer `tiling` times across a cell (`uv * 8`), so the layer
    /// textures must be sampled with a repeating address mode. Bevy's own default sampler clamps
    /// to the edge, which is what stretched the textures in the user's frames.
    #[test]
    fn tiled_terrain_layers_are_sampled_with_a_repeating_sampler() {
        let sampler = terrain_layer_sampler();
        assert_eq!(sampler.address_mode_u, ImageAddressMode::Repeat);
        assert_eq!(sampler.address_mode_v, ImageAddressMode::Repeat);
        assert_eq!(sampler.address_mode_w, ImageAddressMode::Repeat);
        assert_eq!(sampler.mag_filter, ImageFilterMode::Linear);
        assert_eq!(sampler.min_filter, ImageFilterMode::Linear);
        assert_eq!(sampler.mipmap_filter, ImageFilterMode::Linear);
        assert_eq!(sampler.lod_min_clamp, 0.0);
        assert_eq!(sampler.lod_max_clamp, 32.0);
        assert_eq!(
            default_linear_sampler().address_mode_u,
            ImageAddressMode::ClampToEdge
        );
        assert!(
            TerrainExtension::default()
                .settings
                .tiling_and_layer_count
                .x
                > 1.0,
            "layers only need a repeating sampler because the shader tiles them"
        );
    }

    /// Defect B's mechanism on the CPU, with the tiling the material uses. A four-texel layer
    /// sampled at `uv = cell_uv * tiling`: clamping reads the last texel column (and, past the
    /// first tile in both axes, the one corner texel) wherever the tiling has moved past the
    /// texture, which is the flat colour and the stretched streaks of the frames; repeating reads
    /// the texel the tiling actually points at.
    #[test]
    fn a_clamping_sampler_smears_a_tiled_layer_across_the_cell() {
        let tiling = TerrainExtension::default()
            .settings
            .tiling_and_layer_count
            .x;
        let texels = 4i32;
        // A layer whose column index is its red channel, so a sample says which column it read.
        let layer = |x: i32, _y: i32| [x as u8 * 60, 0, 0, 255];
        let read = |cell_uv: f32, repeat: bool| -> i32 {
            let uv = cell_uv * tiling;
            let texel = uv * texels as f32 - 0.5;
            let column = if repeat {
                texel.floor().rem_euclid(texels as f32)
            } else {
                texel.floor().clamp(0.0, texels as f32 - 1.0)
            };
            i32::from(layer(column as i32, 0)[0]) / 60
        };

        // Every position past the first tile reads the edge column when clamping...
        for cell_uv in [0.2_f32, 0.5, 0.9] {
            assert_eq!(
                read(cell_uv, false),
                texels - 1,
                "cell uv {cell_uv} clamps to the layer's edge column"
            );
        }
        // ...while repeating keeps sampling the layer the tiling points at, so the texel read
        // changes with the position instead of sticking at the edge.
        assert_ne!(read(0.2, true), read(0.2, false));
        assert_ne!(read(0.5, true), read(0.9, true));
    }
}
