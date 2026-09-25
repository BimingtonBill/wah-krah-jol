use crate::{
    lights::EMISSIVE_EXPOSURE,
    profiling::ProfilingState,
    world::{
        cache::{TerrainLayerSnapshot, TerrainSnapshot},
        database::AssetCatalog,
    },
};
use bevy::{
    asset::{AssetEvent, AssetEventSystems, LoadContext, embedded_asset},
    camera::{RenderTarget, visibility::RenderLayers},
    core_pipeline::{mip_generation::experimental::depth::ViewDepthPyramid, prepass::DepthPrepass},
    gltf::{
        GltfMaterial,
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
    },
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
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
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub type TerrainMaterial = ExtendedMaterial<StandardMaterial, TerrainExtension>;
pub type WaterMaterial = ExtendedMaterial<StandardMaterial, WaterExtension>;
/// The snow material of a `MATO`-covered static. Its type, its shader and its formula live in
/// [`crate::snow`], which is also where the tables behind it are read; this alias keeps the three
/// materials of the renderer side by side.
pub type SnowMaterial = crate::snow::SnowMaterial;
use crate::effect_palette::{EffectPalette, EffectPaletteMaterial, EffectPaletteRegistry};

/// How far the procedural waves tilt the water's normal. Skyrim's water is close to flat at a
/// distance; a stronger tilt striped lakes with bright and dark bands.
const WAVE_STRENGTH: f32 = 0.05;

/// Skyrim's DefaultWater after Update.esm: Fresnel Amount 0.10 and a Reflectivity Amount of 0.8
/// (WATR `DNAM`). Used when a water has no decoded colours yet, e.g. a database converted before
/// `crates/converter` started reading them.
pub const DEFAULT_WATER_FRESNEL: f32 = 0.10;
pub const DEFAULT_WATER_REFLECTIVITY: f32 = 0.8;

pub struct VercidiumRendererPlugin;

impl Plugin for VercidiumRendererPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/terrain.wgsl");
        embedded_asset!(app, "shaders/water.wgsl");
        embedded_asset!(app, "shaders/snow.wgsl");
        embedded_asset!(app, "shaders/effect_palette.wgsl");
        app.add_plugins((
            crate::light_falloff::SkyrimLightFalloffPlugin,
            crate::material_animation::MaterialAnimationPlugin,
            crate::billboard::BillboardPlugin,
            crate::shadow_layers::ShadowViewLayersPlugin,
            MaterialPlugin::<TerrainMaterial>::default(),
            MaterialPlugin::<WaterMaterial>::default(),
            MaterialPlugin::<SnowMaterial>::default(),
            MaterialPlugin::<EffectPaletteMaterial>::default(),
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
        register_skyrim_material_handler(app);
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

/// The landscape texture the base layer of `quadrant` samples.
///
/// Some `LAND` quadrants carry no base texture: the converter publishes them with texture form id
/// `0` (`crates/converter/src/esm/cell_cache.rs`), and Bevy binds its 1x1 white fallback to the
/// empty slot, which paints the quadrant's whole base weight as a white square. Such a quadrant
/// borrows a texture that is there instead: the base of the first neighbouring quadrant of the
/// same cell that has one, in quadrant order, else the texture of its own strongest overlay.
/// `0` still means "no texture", for a quadrant with nothing to borrow.
pub(crate) fn quadrant_base_texture_form_id(
    terrain: &TerrainSnapshot,
    quadrant: u8,
) -> Result<u32, String> {
    let layers = crate::streaming::quadrant_layers(terrain, quadrant)?;
    let own_base = layers
        .iter()
        .find(|layer| layer.is_base)
        .filter(|base| base.texture_form_id != 0);
    if let Some(base) = own_base {
        return Ok(base.texture_form_id);
    }
    for neighbour in 0..4u8 {
        if neighbour == quadrant {
            continue;
        }
        let base = crate::streaming::quadrant_layers(terrain, neighbour)?
            .into_iter()
            .find(|layer| layer.is_base)
            .filter(|base| base.texture_form_id != 0);
        if let Some(base) = base {
            return Ok(base.texture_form_id);
        }
    }
    Ok(strongest_overlay_texture(&layers).unwrap_or(0))
}

/// The texture of the overlay that covers most of the quadrant: the one whose `VTXT` opacities sum
/// highest, the earlier layer winning a tie so the choice is deterministic. Layers with no texture
/// of their own cannot stand in for the base and are skipped.
fn strongest_overlay_texture(layers: &[&TerrainLayerSnapshot]) -> Option<u32> {
    let mut strongest: Option<(f32, u32)> = None;
    for layer in layers.iter().filter(|layer| !layer.is_base) {
        if layer.texture_form_id == 0 {
            continue;
        }
        let covered: f32 = layer.weights.iter().map(|(_, opacity)| opacity).sum();
        let replace = match strongest {
            Some((best, _)) => covered > best,
            None => true,
        };
        if replace {
            strongest = Some((covered, layer.texture_form_id));
        }
    }
    strongest.map(|(_, texture_form_id)| texture_form_id)
}

impl TerrainExtension {
    pub fn from_quadrant(
        terrain: &TerrainSnapshot,
        quadrant: u8,
        catalog: &AssetCatalog,
        asset_server: &AssetServer,
    ) -> Result<(Self, Vec<Handle<Image>>), String> {
        let layers = crate::streaming::quadrant_layers(terrain, quadrant)?;
        // The layer slots in the order the shader samples them: slot 0 is the base, which a
        // quadrant without a `BTXT` of its own samples from the texture
        // [`quadrant_base_texture_form_id`] borrows for it.
        let mut slots = [0u32; 6];
        slots[0] = quadrant_base_texture_form_id(terrain, quadrant)?;
        for (slot, layer) in layers.iter().enumerate().skip(1) {
            slots[slot] = layer.texture_form_id;
        }
        let mut textures: [Option<Handle<Image>>; 6] = std::array::from_fn(|_| None);
        let mut handles = Vec::new();
        for (target, texture_form_id) in textures.iter_mut().zip(slots) {
            if texture_form_id == 0 {
                continue;
            }
            let path = catalog.landscape_diffuse(texture_form_id).ok_or_else(|| {
                format!(
                    "LAND {:08X} quadrant {quadrant} texture {:08X} has no diffuse image",
                    terrain.cell_id, texture_form_id
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
    /// x = Fresnel Amount (Schlick F0), y = Reflectivity Amount. z/w unused.
    fresnel_reflectivity: Vec4,
}

impl Default for WaterExtension {
    fn default() -> Self {
        Self {
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, WAVE_STRENGTH, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, 0.0),
                fresnel_reflectivity: Vec4::new(
                    DEFAULT_WATER_FRESNEL,
                    DEFAULT_WATER_REFLECTIVITY,
                    0.0,
                    0.0,
                ),
            },
            reflection: None,
            flow_normal: None,
        }
    }
}

impl WaterExtension {
    /// Builds a water material with Skyrim's DefaultWater fresnel and reflectivity. Callers that
    /// know a water's own factors (from [`crate::world::database::AssetCatalog::water_colors`])
    /// should use [`Self::with_reflection_and_factors`] instead.
    pub fn with_reflection(reflection: Handle<Image>, flow_normal: Option<Handle<Image>>) -> Self {
        Self::with_reflection_and_factors(
            reflection,
            flow_normal,
            DEFAULT_WATER_FRESNEL,
            DEFAULT_WATER_REFLECTIVITY,
        )
    }

    pub fn with_reflection_and_factors(
        reflection: Handle<Image>,
        flow_normal: Option<Handle<Image>>,
        fresnel: f32,
        reflectivity: f32,
    ) -> Self {
        let has_flow_normal = flow_normal.is_some() as u8 as f32;
        Self {
            reflection: Some(reflection),
            flow_normal,
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, WAVE_STRENGTH, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, has_flow_normal),
                fresnel_reflectivity: Vec4::new(fresnel, reflectivity, 0.0, 0.0),
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
            // The camera is placed below the water looking up, not mirrored, so its triangles keep
            // their winding: inverting the culling drew the back faces.
            invert_culling: false,
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

/// The material extension the converter writes for a Skyrim material whose blend glTF's own
/// `BLEND` cannot express: the two `NiAlphaProperty` factors under `blendSource` and
/// `blendDestination`, spelled as nif.xml's `AlphaFunction` (`crates/converter/src/material.rs`).
const OPEN_SKYRIM_MATERIAL_EXTENSION: &str = "OPEN_SKYRIM_material";
/// A material's animated shader values (`crate::material_animation`).
const MATERIAL_ANIMATION_EXTENSION: &str = "OPEN_SKYRIM_material_animation";

/// A source or destination blend factor of `NiAlphaProperty` (nif.xml's `AlphaFunction`), which is
/// what the extension publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlendFactor {
    One,
    Zero,
    SourceColor,
    InverseSourceColor,
    DestinationColor,
    InverseDestinationColor,
    SourceAlpha,
    InverseSourceAlpha,
    DestinationAlpha,
    InverseDestinationAlpha,
    SourceAlphaSaturate,
}

impl BlendFactor {
    /// The factor a published name stands for, or `None` for a name this engine does not know.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "ONE" => Self::One,
            "ZERO" => Self::Zero,
            "SRC_COLOR" => Self::SourceColor,
            "INV_SRC_COLOR" => Self::InverseSourceColor,
            "DEST_COLOR" => Self::DestinationColor,
            "INV_DEST_COLOR" => Self::InverseDestinationColor,
            "SRC_ALPHA" => Self::SourceAlpha,
            "INV_SRC_ALPHA" => Self::InverseSourceAlpha,
            "DEST_ALPHA" => Self::DestinationAlpha,
            "INV_DEST_ALPHA" => Self::InverseDestinationAlpha,
            "SRC_ALPHA_SATURATE" => Self::SourceAlphaSaturate,
            _ => return None,
        })
    }
}

/// What a pair of blend factors renders as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlendInterpretation {
    /// `SRC_ALPHA`/`ONE` and `ONE`/`ONE`: the surface's colour adds to the frame - the glow cards
    /// of torches, braziers and Dwemer lanterns.
    Additive,
    /// `DEST_COLOR`/`ZERO` and `ZERO`/`SRC_COLOR`: the surface multiplies the frame behind it.
    Multiplicative,
    /// `SRC_ALPHA`/`INV_SRC_ALPHA`, which is exactly what glTF `BLEND` already renders.
    StraightAlpha,
    /// A pair neither glTF nor Bevy's material has a mode for: the material stays alpha-over, and
    /// the pair is reported once.
    Unsupported,
}

impl BlendInterpretation {
    fn of(source: BlendFactor, destination: BlendFactor) -> Self {
        match (source, destination) {
            (BlendFactor::SourceAlpha, BlendFactor::One) | (BlendFactor::One, BlendFactor::One) => {
                Self::Additive
            }
            (BlendFactor::DestinationColor, BlendFactor::Zero)
            | (BlendFactor::Zero, BlendFactor::SourceColor) => Self::Multiplicative,
            (BlendFactor::SourceAlpha, BlendFactor::InverseSourceAlpha) => Self::StraightAlpha,
            _ => Self::Unsupported,
        }
    }

    fn alpha_mode(self) -> AlphaMode {
        match self {
            Self::Additive => AlphaMode::Add,
            Self::Multiplicative => AlphaMode::Multiply,
            Self::StraightAlpha | Self::Unsupported => AlphaMode::Blend,
        }
    }
}

/// The blend pairs already reported, so a model whose shapes share one unmatched pair reports it
/// once instead of once per material.
static UNMATCHED_BLEND_PAIRS: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

fn report_unmatched_blend_pair(source: &str, destination: &str) {
    let mut reported = UNMATCHED_BLEND_PAIRS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if reported.insert((source.to_owned(), destination.to_owned())) {
        warn!(
            blend_source = %source,
            blend_destination = %destination,
            "the published `NiAlphaProperty` blend factors are not one this engine knows; the material keeps straight alpha-over"
        );
    }
}

/// The two blend factors a material's `OPEN_SKYRIM_material` extension publishes, as the converter
/// spelled them. The converter writes both together, and only for a material it published as glTF
/// `BLEND`, so a pair here always means a surface glTF's `BLEND` would draw wrong.
fn published_blend_pair(extension: Option<&serde_json::Value>) -> Option<(&str, &str)> {
    let extension = extension?;
    let source = extension.get("blendSource")?.as_str()?;
    let destination = extension.get("blendDestination")?.as_str()?;
    Some((source, destination))
}

/// The alpha mode a loaded material's blend pair asks for, or `None` for a material that publishes
/// no pair the engine acts on - no pair at all, a pair with a name it does not know, or a pair it
/// cannot express, both of which are reported once and left as alpha-over.
fn skyrim_alpha_mode(material: &bevy::gltf::gltf::Material) -> Option<AlphaMode> {
    let (source, destination) =
        published_blend_pair(material.extension_value(OPEN_SKYRIM_MATERIAL_EXTENSION))?;
    let factors = BlendFactor::from_name(source).zip(BlendFactor::from_name(destination));
    let Some((source_factor, destination_factor)) = factors else {
        report_unmatched_blend_pair(source, destination);
        return None;
    };
    match BlendInterpretation::of(source_factor, destination_factor) {
        BlendInterpretation::Unsupported => {
            report_unmatched_blend_pair(source, destination);
            None
        }
        interpretation => Some(interpretation.alpha_mode()),
    }
}

/// The emissive a streamed material is published with: its colour channels at the engine's scale,
/// its alpha kept.
///
/// The alpha is not the glow's to scale. A `StandardMaterial`'s emissive carries an alpha that a
/// `Blend` surface uses as coverage, and `LinearRgba * f32` would scale it with the rest: an alpha
/// of `1.0` would come out at `EMISSIVE_EXPOSURE`, and one of `0.0` would stay black however bright
/// the scale is.
///
/// `app.rs` builds the canonical emissive fixture with this function too, so the fixture is what a
/// converted glow looks like rather than a magnitude of its own.
pub(crate) fn exposed_emissive(emissive: LinearRgba) -> LinearRgba {
    LinearRgba::new(
        emissive.red * EMISSIVE_EXPOSURE,
        emissive.green * EMISSIVE_EXPOSURE,
        emissive.blue * EMISSIVE_EXPOSURE,
        emissive.alpha,
    )
}

/// Whether a streamed material carries a *deliberate glow* - one this engine has to bring up to its
/// own lighting scale - or an emissive that is a surface's own brightness and must stay where the
/// converter put it.
///
/// The line is `KHR_materials_emissive_strength`, which the converter publishes only for a NIF whose
/// emissive multiple is above 1 (`crates/converter/src/material.rs`, `publish_emissive`), and the two
/// classes it separates behave completely differently in this engine:
///
/// * **Above 1: an emitter.** `blackreachgiantmushroom01`'s caps (2.0 to 3.6), the `BlackreachSun01`
///   orb (3.0), a torch's flame card (3.0). These read as small lights, and against an ambient of
///   650 to 800 - and light pools 10 times *that* (`crate::lights::LIGHT_EXPOSURE`) - a published emissive of 2 to
///   3.6 is invisible: nothing renders it (`docs/research/visual-gaps-spec.md`, gap 1).
/// * **Exactly 1: a self-lit surface.** `emissiveFactor [1, 1, 1]` with the glow slot holding the
///   model's own diffuse texture - Skyrim's `SLSF1_Own_Emit`, which the snow-covered trees, the ice
///   of the Alftand ravine and the landscape ice all carry. Skyrim uses it to keep a surface from
///   going dark where the light leaves it, it is already the brightness the game gives it, and it is
///   not this handler's to change.
///
/// Scaling the second class as well is not a smaller or larger version of the same fix: the first
/// attempt at this scaled both, and at 1000 the ice of the Alftand ravine rendered white and a
/// daylight reference pose went from 0.01 % to 45 % of its pixels clipped, while the value that
/// holds that guard leaves the emitters of Tamriel untouched.
/// The brightest a deliberate glow's channel is drawn at, after [`EMISSIVE_EXPOSURE`].
///
/// Past about 93 the tonemapper's lookup table has one cell for everything, so a glow whose channels
/// all exceed it is drawn the same white whatever its colour: Blackreach's cyan mushroom caps
/// (published `[0.42, 1.98, 2.0]`, x100) and hanging strands (`[2.26, 3.59, 3.6]`) burnt out
/// (research-172, look-gaps item 8). 45 keeps the brightest channel a cell below, the others in
/// proportion. Measured 2026-09-24: Blackreach's three shots 1.33 -> 1.14 (flat white 11.5 % ->
/// 6.4 % on the worst), the exterior holdout 0.740 -> 0.724, interiors unchanged (0.658 -> 0.662).
pub const GLOW_PEAK_CEILING: f32 = 45.0;

/// A glow scaled down, colour kept, so its brightest channel is at most [`GLOW_PEAK_CEILING`].
pub(crate) fn capped_glow(emissive: LinearRgba) -> LinearRgba {
    let peak = emissive.red.max(emissive.green).max(emissive.blue);
    if peak <= GLOW_PEAK_CEILING {
        return emissive;
    }
    let scale = GLOW_PEAK_CEILING / peak;
    LinearRgba::new(
        emissive.red * scale,
        emissive.green * scale,
        emissive.blue * scale,
        emissive.alpha,
    )
}

fn is_deliberate_glow(gltf_material: &bevy::gltf::gltf::Material) -> bool {
    gltf_material
        .emissive_strength()
        .is_some_and(|strength| strength > 1.0)
}

/// The material a streamed Skyrim material is published as: its blend pair's `AlphaMode` (when it
/// publishes one the engine can act on) and, for the deliberate glows, its emissive at the engine's
/// scale. `None` when the material is already what it should be, so nothing is republished for
/// nothing.
fn skyrim_material(
    gltf_material: &bevy::gltf::gltf::Material,
    material: &StandardMaterial,
) -> Option<StandardMaterial> {
    // A material with no pair this engine acts on - no extension at all, or one it cannot express -
    // keeps the `AlphaMode` glTF's own `alphaMode` gave it: an additive glow card is the point of
    // the pair, and the wrong guess in the other direction would veil the world.
    let alpha_mode = skyrim_alpha_mode(gltf_material).unwrap_or(material.alpha_mode);
    let emissive = if is_deliberate_glow(gltf_material) {
        capped_glow(exposed_emissive(material.emissive))
    } else {
        material.emissive
    };
    if alpha_mode == material.alpha_mode && emissive == material.emissive {
        return None;
    }
    Some(StandardMaterial {
        alpha_mode,
        emissive,
        ..material.clone()
    })
}

/// Gives a streamed Skyrim material the `AlphaMode` its blend factors ask for and, for the
/// deliberate glows, the emissive scale the engine lights its world in.
///
/// Bevy's own PBR material handler publishes the loaded material at `"{material_label}/std"`, the
/// label the scene's meshes are then handed, so this handler - registered after that one - replaces
/// the value under that label and changes nothing else about it. Without the blend half, an
/// additive glow card (`SRC_ALPHA`/`ONE`: torch and Dwemer lantern glows) or a multiplicative
/// surface (`ZERO`/`SRC_COLOR`) draws as ordinary alpha-over, a grey veil over the world instead of
/// light.
///
/// It is registered for *every* streamed material, not only the ones with a blend pair: the
/// materials that carry most of the game's glow publish no pair at all (the Blackreach mushroom
/// caps are `OPAQUE` and `MASK`), so an early return on a missing pair would leave exactly those
/// invisible - which is what it did before this handler covered every material.
/// [`is_deliberate_glow`] is what decides which emitted values are that glow and which are a
/// surface's own brightness.
///
/// It also remembers the file's textures by glTF index, because an effect material's palette is a
/// texture no glTF material slot names: only the `OPEN_SKYRIM_material` extension points at it
/// ([`crate::effect_palette`]).
#[derive(Default, Clone)]
struct SkyrimMaterialHandler {
    textures: Vec<Option<Handle<Image>>>,
    palettes: EffectPaletteRegistry,
    animations: crate::material_animation::MaterialAnimationRegistry,
}

impl GltfExtensionHandler for SkyrimMaterialHandler {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::new(self.clone())
    }

    fn on_root(
        &mut self,
        _load_context: &mut LoadContext<'_>,
        _gltf: &bevy::gltf::gltf::Gltf,
        _settings: &bevy::gltf::GltfLoaderSettings,
    ) {
        // A new file: the texture indices of the last one mean nothing here.
        self.textures.clear();
    }

    fn on_texture(&mut self, gltf_texture: &bevy::gltf::gltf::Texture, texture: Handle<Image>) {
        let index = gltf_texture.index();
        if self.textures.len() <= index {
            self.textures.resize(index + 1, None);
        }
        self.textures[index] = Some(texture);
    }

    fn on_material(
        &mut self,
        load_context: &mut LoadContext<'_>,
        gltf_material: &bevy::gltf::gltf::Material,
        _material: Handle<GltfMaterial>,
        _material_asset: &GltfMaterial,
        material_label: &str,
    ) {
        let label = format!("{material_label}/std");
        // Reading the material Bevy built back out of the load context keeps every field it
        // filled in (textures, culling, alpha cutoff, unlit flag) and lets the mode and the
        // emissive be the only differences, which is what makes this safe to run over every
        // streamed material.
        let Some(material) = load_context
            .get_labeled(&label)
            .and_then(|asset| asset.get::<StandardMaterial>())
            .cloned()
        else {
            // Once per process: this only happens if Bevy's own material handler stops publishing
            // at this label, and then it happens for every streamed material.
            warn_once!(
                label = %label,
                "a streamed material has no loaded `StandardMaterial` published for it"
            );
            return;
        };
        if let Some(palette) = effect_palette_of(gltf_material, &material, &self.textures) {
            self.palettes
                .insert(format!("{}#{label}", load_context.path()), palette);
        }
        if let Some(animation) = gltf_material
            .extension_value(MATERIAL_ANIMATION_EXTENSION)
            .and_then(|animation| {
                crate::material_animation::MaterialAnimation::from_extensions(
                    animation,
                    gltf_material.extension_value(OPEN_SKYRIM_MATERIAL_EXTENSION),
                )
            })
            .filter(crate::material_animation::MaterialAnimation::plays_anything)
        {
            self.animations
                .insert(format!("{}#{label}", load_context.path()), animation);
        }
        let Some(material) = skyrim_material(gltf_material, &material) else {
            return;
        };
        load_context.add_labeled_asset(label, material);
    }
}

/// The [`EffectPalette`] of a greyscale-to-palette effect material, or `None` for every other
/// material, and for one whose palette or source texture the file does not carry.
fn effect_palette_of(
    gltf_material: &bevy::gltf::gltf::Material,
    material: &StandardMaterial,
    textures: &[Option<Handle<Image>>],
) -> Option<EffectPalette> {
    let extension = gltf_material.extension_value(OPEN_SKYRIM_MATERIAL_EXTENSION)?;
    let (palette_index, settings) = crate::effect_palette::palette_settings(
        extension,
        gltf_material.emissive_factor(),
        gltf_material.emissive_strength().unwrap_or(1.0),
        gltf_material.pbr_metallic_roughness().base_color_factor()[3],
    )?;
    Some(EffectPalette {
        palette: textures.get(palette_index)?.clone()?,
        // The emissive texture is the source; Bevy leaves it out of a material whose emissive it
        // does not draw, and the converter publishes the same texture as the base colour's.
        source: material
            .emissive_texture
            .clone()
            .or_else(|| material.base_color_texture.clone())?,
        settings,
    })
}

/// Registers [`SkyrimMaterialHandler`] with the glTF loader. It has to be appended after Bevy's own
/// material handler (which `PbrPlugin` registers first) because it replaces what that handler
/// publishes; the handler list is read again on every load, so registering once here is enough.
fn register_skyrim_material_handler(app: &mut App) {
    let palettes = EffectPaletteRegistry::default();
    app.insert_resource(palettes.clone());
    let animations = crate::material_animation::MaterialAnimationRegistry::default();
    app.insert_resource(animations.clone());
    let Some(handlers) = app.world().get_resource::<GltfExtensionHandlers>() else {
        warn!(
            "the glTF extension handlers are unavailable; additive and multiplicative Skyrim materials will render as alpha-over, and no streamed emissive will reach the engine's scale"
        );
        return;
    };
    handlers
        .0
        .write_blocking()
        .push(Box::new(SkyrimMaterialHandler {
            textures: Vec::new(),
            palettes,
            animations,
        }));
}

/// Rewrites the alpha of every loaded mesh's vertex colours to full opacity, leaving the RGB of
/// every vertex alone.
///
/// Skyrim's `COLOR_0` alpha is a shader parameter, not opacity. `SLSF1_VERTEX_ALPHA` is the flag
/// that asks a shader to *read* per-vertex alpha, and the converted tree, plant and architecture
/// shapes carry a channel that flag reads: `TreePineForest03.nif`'s leaf shape has 176 of its 296
/// vertices below the 112/255 cutoff its own material tests, 110 of them at 0.0
/// (`docs/research/foliage-alpha-test.md` section 3). Bevy's PBR fragment shader builds the alpha
/// its `MASK` test compares as `vertex_color.a * texture.a`
/// (`bevy_pbr-0.19.0/src/render/pbr_fragment.wgsl:54-55`, `:194`, tested at
/// `bevy_pbr-0.19.0/src/render/pbr_functions.wgsl:119-127`), so the raw channel dissolves every
/// canopy into the spray of dots beside a solid trunk that the Riverwood frames show. What the test
/// compares is what is wrong, not the cutoff.
///
/// The converter needs no change and gets none: it reads `NiAlphaProperty` correctly, publishes the
/// threshold as `alphaCutoff = threshold/255` correctly, and `alpha_contract` already refuses to
/// read `SLSF1_VERTEX_ALPHA` as a declaration of transparency - the ruling that stopped 2,632 ice,
/// rock and floor materials from rendering see-through (`crates/converter/src/material.rs`). That
/// ruling's stated premise, that the converter exports no vertex colours for the flag to read, is
/// what changed: the vendored exporter has written `COLOR_0` since `33682e6` and 1,337 converted
/// models carry one. Rewriting at load closes the gap on the engine side, which keeps the parameter
/// channel on disk for the wind and snow projections to read later and keeps the whole converted
/// set out of a reconversion.
///
/// Nothing but the alpha is touched - Skyrim's baked per-vertex shading is real and every RGB
/// triple comes out bit-identical. The system is registered in [`VercidiumRendererPlugin::build`]
/// next to the material handler above, on the two events a mesh is published with,
/// [`AssetEvent::Added`] and [`AssetEvent::Modified`].
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
        // The mesh is taken without marking the asset modified until something is really written:
        // a mesh whose alphas are already opaque is left alone, and being left alone is what keeps
        // the `AssetEvent::Modified` this system queues from bringing it back to the same mesh for
        // ever.
        match force_opaque_vertex_alpha(mesh.bypass_change_detection()) {
            // Every alpha is already opaque, or the colour has no alpha component to rewrite.
            Ok(0) => {}
            Ok(vertices) => {
                // Marking the asset modified is what makes the render world re-extract the
                // rewritten vertices (`bevy_render-0.19.0/src/render_asset.rs:305-331` reads
                // `AssetEvent`s), and it is reached only for a mesh whose alpha really changed.
                mesh.into_inner();
                debug!(
                    vertices = vertices,
                    "vertex colour alpha forced to 1.0: Skyrim's `COLOR_0` alpha is a shader parameter, not opacity"
                );
            }
            Err(reason) => {
                debug!(
                    reason = reason,
                    "a loaded mesh's vertex colours were not rewritten"
                );
            }
        }
    }
}

/// Sets the alpha of every vertex colour in `mesh` to full opacity in the attribute's own encoding,
/// and reports how many vertices changed - or why the mesh was left alone. Nothing but the alpha
/// component is ever written.
///
/// `Ok(0)` is the ordinary case with nothing to do: every alpha is already opaque, or the colour has
/// no alpha component at all (one, two or three components), which is left as it is rather than
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
        // Every mesh this engine streams keeps it: the scenes are loaded with no settings of their
        // own, so the glTF loader's `load_meshes` default applies, and that is
        // `RenderAssetUsages::MAIN_WORLD | RENDER_WORLD`
        // (`bevy_gltf-0.19.0/src/loader/mod.rs:223`, as `crates/engine/src/player.rs` records).
        Err(_) => return Err("its vertex data is in the render world"),
    };
    let changed = match colours {
        // The two encodings this pipeline actually produces: the glTF loader widens every
        // `COLOR_0` to `Float32x4` (`bevy_gltf-0.19.0/src/vertex_attributes.rs:196-215`), and
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
        // once and skipped.
        VertexAttributeValues::Uint8x4(_)
        | VertexAttributeValues::Sint8x4(_)
        | VertexAttributeValues::Uint16x4(_)
        | VertexAttributeValues::Sint16x4(_)
        | VertexAttributeValues::Uint32x4(_)
        | VertexAttributeValues::Sint32x4(_) => {
            return Err("an unnormalised integer colour encoding");
        }
        // A half-float colour would need the `half` crate to write its 1.0 - `f16` has no
        // `From<f32>` and this crate cannot name the type, since `half` is `bevy_mesh`'s dependency
        // rather than one of this crate's - so it is reported and skipped instead.
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
    use crate::world::cache::TerrainLayerSnapshot;
    use bevy::asset::RenderAssetUsages;
    use bevy::image::ImageFilterMode;
    use bevy::mesh::{MeshVertexAttribute, PrimitiveTopology, VertexAttributeValues};
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

    /// Each blend pair the engine knows renders the way `NiAlphaProperty` asks; the pair glTF's
    /// `BLEND` already expresses keeps it, and a pair neither can express does not silently become
    /// an additive or multiplicative surface.
    #[test]
    fn blend_pairs_map_to_the_alpha_mode_that_renders_them() {
        let mode = |source, destination| BlendInterpretation::of(source, destination).alpha_mode();
        assert_eq!(
            mode(BlendFactor::SourceAlpha, BlendFactor::One),
            AlphaMode::Add,
            "the additive glow card of a torch"
        );
        assert_eq!(mode(BlendFactor::One, BlendFactor::One), AlphaMode::Add);
        assert_eq!(
            mode(BlendFactor::DestinationColor, BlendFactor::Zero),
            AlphaMode::Multiply
        );
        assert_eq!(
            mode(BlendFactor::Zero, BlendFactor::SourceColor),
            AlphaMode::Multiply
        );
        assert_eq!(
            mode(BlendFactor::SourceAlpha, BlendFactor::InverseSourceAlpha),
            AlphaMode::Blend,
            "straight alpha-over is what glTF `BLEND` already renders"
        );

        // A pair with no mode of its own stays alpha-over instead of being rendered as something
        // stronger: the direction is wrong for additive and multiplicative alike.
        for (source, destination) in [
            (BlendFactor::InverseSourceAlpha, BlendFactor::One),
            (BlendFactor::One, BlendFactor::SourceAlpha),
            (BlendFactor::SourceColor, BlendFactor::InverseSourceColor),
            (BlendFactor::DestinationAlpha, BlendFactor::Zero),
            (BlendFactor::SourceAlphaSaturate, BlendFactor::One),
        ] {
            assert_eq!(
                BlendInterpretation::of(source, destination),
                BlendInterpretation::Unsupported,
                "{source:?}/{destination:?} is not expressible"
            );
            assert_eq!(
                BlendInterpretation::of(source, destination).alpha_mode(),
                AlphaMode::Blend
            );
        }
    }

    /// The pair the converter publishes for an additive glow card, read exactly as the loader hands
    /// it to the material handler: both factors are named, and the pair is additive.
    #[test]
    fn a_published_additive_pair_is_an_additive_material() {
        let extension: serde_json::Value = serde_json::from_str(
            r#"{"shaderFamily":"Glow","shaderFlags1":0,"shaderFlags2":0,"premultipliedAlpha":false,
                "screenDoorAlphaFade":false,"textureSlots":[],
                "blendSource":"SRC_ALPHA","blendDestination":"ONE"}"#,
        )
        .unwrap();
        let (source, destination) =
            published_blend_pair(Some(&extension)).expect("the converter publishes both factors");
        assert_eq!((source, destination), ("SRC_ALPHA", "ONE"));
        let pair = BlendFactor::from_name(source)
            .zip(BlendFactor::from_name(destination))
            .expect("both names are ones this engine knows");
        assert_eq!(
            BlendInterpretation::of(pair.0, pair.1).alpha_mode(),
            AlphaMode::Add
        );

        // `ONE`/`ONE` is the other additive spelling, and a zero/second factor is multiplicative.
        let one_one: serde_json::Value =
            serde_json::from_str(r#"{"blendSource":"ONE","blendDestination":"ONE"}"#).unwrap();
        let multiply: serde_json::Value =
            serde_json::from_str(r#"{"blendSource":"DEST_COLOR","blendDestination":"ZERO"}"#)
                .unwrap();
        let mode = |value: &serde_json::Value| {
            let (source, destination) = published_blend_pair(Some(value)).unwrap();
            let pair = BlendFactor::from_name(source)
                .zip(BlendFactor::from_name(destination))
                .unwrap();
            BlendInterpretation::of(pair.0, pair.1).alpha_mode()
        };
        assert_eq!(mode(&one_one), AlphaMode::Add);
        assert_eq!(mode(&multiply), AlphaMode::Multiply);

        // A material with no extension, one with a single field, and one whose factor name this
        // engine does not know all publish no pair this engine acts on.
        assert_eq!(published_blend_pair(None), None);
        assert_eq!(
            published_blend_pair(Some(
                &serde_json::from_str(r#"{"blendSource":"SRC_ALPHA"}"#).unwrap()
            )),
            None
        );
        let unknown =
            serde_json::from_str(r#"{"blendSource":"SRC_ALPHA","blendDestination":"ONE_HALF"}"#)
                .unwrap();
        let (source, destination) = published_blend_pair(Some(&unknown)).unwrap();
        assert_eq!(destination, "ONE_HALF");
        assert_eq!(
            BlendFactor::from_name(destination),
            None,
            "an unknown name is reported, not guessed at"
        );
        assert!(BlendFactor::from_name(source).is_some());
    }

    /// A minimal binary glTF - a header and one JSON chunk, no binary payload - that the parser the
    /// glTF loader itself uses can read: the material JSON goes in the document's `materials`.
    fn glb(materials: &str) -> Vec<u8> {
        let json = format!(r#"{{"asset":{{"version":"2.0"}},"materials":[{materials}]}}"#);
        let mut chunk = json.into_bytes();
        while chunk.len() % 4 != 0 {
            chunk.push(b' ');
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"glTF");
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&(20 + chunk.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"JSON");
        bytes.extend_from_slice(&chunk);
        bytes
    }

    /// The whole reading path, not a hand-built `Value`: the extension as it sits in a `.glb` the
    /// converter wrote, parsed by the same glTF parser the loader uses. This is what proves the
    /// unknown extension survives parsing at all - `gltf::Material::extension_value` reads it out of
    /// the parser's flattened `others` map - and that a glow card comes out additive.
    #[test]
    fn a_glb_material_publishing_an_additive_pair_reads_as_additive() {
        let additive = glb(r#"{"name":"GlowAddMesh","alphaMode":"BLEND",
                "extensions":{"OPEN_SKYRIM_material":{"shaderFamily":"Glow","shaderFlags1":0,
                "shaderFlags2":0,"premultipliedAlpha":false,"screenDoorAlphaFade":false,
                "textureSlots":[],"blendSource":"SRC_ALPHA","blendDestination":"ONE"}}}"#);
        let document = bevy::gltf::gltf::Gltf::from_slice(&additive).expect("a valid .glb");
        let material = document.document.materials().next().expect("one material");
        assert_eq!(
            format!("{:?}", material.alpha_mode()),
            "Blend",
            "the converter publishes these materials as glTF `BLEND`, which Bevy loads as `Blend`"
        );
        assert_eq!(skyrim_alpha_mode(&material), Some(AlphaMode::Add));

        // The pair glTF already renders stays `Blend`, and a material with no extension at all is
        // left alone.
        let straight = glb(r#"{"name":"Glass","alphaMode":"BLEND",
                "extensions":{"OPEN_SKYRIM_material":{"blendSource":"SRC_ALPHA",
                "blendDestination":"INV_SRC_ALPHA"}}}"#);
        let document = bevy::gltf::gltf::Gltf::from_slice(&straight).unwrap();
        let material = document.document.materials().next().unwrap();
        assert_eq!(skyrim_alpha_mode(&material), Some(AlphaMode::Blend));

        let plain = glb(r#"{"name":"Wall","alphaMode":"OPAQUE"}"#);
        let document = bevy::gltf::gltf::Gltf::from_slice(&plain).unwrap();
        let material = document.document.materials().next().unwrap();
        assert_eq!(skyrim_alpha_mode(&material), None);
    }

    /// A converted glow has to leave this handler at the engine's scale. The caps of Blackreach's
    /// glowing mushrooms are the case the fix is for: `emissiveFactor` `[0.212, 0.992, 1.0]` with an
    /// emissive strength of `2.0` and a `MASK` mode, so they publish no blend pair at all - and the
    /// handler used to return early on exactly those materials, leaving the glow at a magnitude of
    /// 2.0 against an ambient of 650 (`docs/research/visual-gaps-spec.md`, gap 1).
    #[test]
    fn a_masked_glow_is_scaled_even_without_a_blend_pair() {
        let cap = glb(r#"{"name":"BlackreachGiantMushroom01:3","alphaMode":"MASK",
                "emissiveFactor":[0.212,0.992,1.0],
                "extensions":{"KHR_materials_emissive_strength":{"emissiveStrength":2.0}}}"#);
        let document = bevy::gltf::gltf::Gltf::from_slice(&cap).unwrap();
        let gltf_material = document.document.materials().next().unwrap();
        assert_eq!(
            skyrim_alpha_mode(&gltf_material),
            None,
            "the cap publishes no pair, which is why it used to be skipped"
        );
        // What Bevy's own handler published for it: `emissiveFactor` times the emissive strength.
        let loaded = StandardMaterial {
            alpha_mode: AlphaMode::Mask(0.5),
            emissive: LinearRgba::new(0.424, 1.984, 2.0, 1.0),
            ..default()
        };
        let published = skyrim_material(&gltf_material, &loaded).expect("the emissive is rescaled");
        assert_eq!(
            published.alpha_mode,
            AlphaMode::Mask(0.5),
            "and its mode is left exactly as glTF's `alphaMode` made it"
        );
        // At the engine's scale, then capped so its brightest channel stays below the tonemapper's
        // white: the cap's cyan keeps its proportions instead of burning out.
        let expected = capped_glow(exposed_emissive(loaded.emissive));
        assert_eq!(
            published.emissive, expected,
            "the glow is published at the engine's scale, capped"
        );
        assert!((published.emissive.blue - GLOW_PEAK_CEILING).abs() < 1e-3);
        assert!(
            (published.emissive.red / published.emissive.blue - 0.424 / 2.0).abs() < 1e-4,
            "and the colour is kept"
        );
        assert_eq!(
            published.emissive.alpha, loaded.emissive.alpha,
            "and the alpha is the surface's coverage, not the glow's brightness: `LinearRgba * f32` \
             would have scaled it too"
        );
        assert!(
            crate::app::emissive_is_visible(published.emissive),
            "which is the point: a converted glow has to come out of this handler at the magnitude \
             the engine's lighting works in, got {:?}",
            published.emissive
        );

        // A torch's additive glow card keeps its pair *and* takes the scale: both changes come out
        // of the one handler.
        let card = glb(
            r#"{"name":"GlowAddMesh","alphaMode":"BLEND","emissiveFactor":[1.0,1.0,1.0],
                "extensions":{"OPEN_SKYRIM_material":{"blendSource":"SRC_ALPHA",
                "blendDestination":"ONE"},
                "KHR_materials_emissive_strength":{"emissiveStrength":3.0}}}"#,
        );
        let document = bevy::gltf::gltf::Gltf::from_slice(&card).unwrap();
        let gltf_material = document.document.materials().next().unwrap();
        let loaded = StandardMaterial {
            alpha_mode: AlphaMode::Blend,
            emissive: LinearRgba::new(3.0, 3.0, 3.0, 1.0),
            ..default()
        };
        let published = skyrim_material(&gltf_material, &loaded).unwrap();
        assert_eq!(published.alpha_mode, AlphaMode::Add);
        assert_eq!(
            published.emissive,
            capped_glow(exposed_emissive(loaded.emissive))
        );
    }

    /// The other class of emissive, which this handler must leave alone: Skyrim's own-emit *surface*
    /// materials. `landscape/ice/icepilel03.glb`'s `IcePileL03` and every snow-covered tree publish
    /// `emissiveFactor [1, 1, 1]` with the glow slot holding the model's own diffuse texture and
    /// **no emissive strength**; scaling those by [`EMISSIVE_EXPOSURE`] is what turned the Alftand
    /// ravine's ice white and clipped 45 % of a daylight frame, which is why the gate exists. The
    /// values here are the real ones, read out of the material JSON of the converted
    /// `landscape/ice/icepilel03.glb`.
    #[test]
    fn an_own_emit_surface_material_keeps_the_emissive_skyrim_gave_it() {
        let own_emit = glb(r#"{"name":"IcePileL03:0","alphaMode":"OPAQUE",
                "emissiveFactor":[1.0,1.0,1.0],
                "extensions":{"OPEN_SKYRIM_material":{"shaderFamily":"lighting",
                "shaderFlags1":2185233153,"shaderFlags2":50331681,"textureSlots":[]}}}"#);
        let document = bevy::gltf::gltf::Gltf::from_slice(&own_emit).unwrap();
        let gltf_material = document.document.materials().next().unwrap();
        assert!(
            gltf_material.emissive_strength().is_none(),
            "the ice publishes no emissive strength: its multiple is 1"
        );
        // What Bevy published for it: `emissiveFactor` alone.
        let loaded = StandardMaterial {
            emissive: LinearRgba::new(1.0, 1.0, 1.0, 1.0),
            ..default()
        };
        assert!(
            skyrim_material(&gltf_material, &loaded).is_none(),
            "an own-emit surface material is not this handler's to change"
        );

        // The strength is the whole difference: the same emissive with a multiple of 2 is a glow.
        let glow = glb(
            r#"{"name":"IcePileL03:0","alphaMode":"OPAQUE","emissiveFactor":[1.0,1.0,1.0],
                "extensions":{"KHR_materials_emissive_strength":{"emissiveStrength":2.0}}}"#,
        );
        let document = bevy::gltf::gltf::Gltf::from_slice(&glow).unwrap();
        let gltf_material = document.document.materials().next().unwrap();
        assert!(is_deliberate_glow(&gltf_material));
        // Bevy's own handler has already folded the strength into the emissive it published.
        let loaded = StandardMaterial {
            emissive: LinearRgba::new(2.0, 2.0, 2.0, 1.0),
            ..default()
        };
        let published = skyrim_material(&gltf_material, &loaded).unwrap();
        assert_eq!(
            published.emissive,
            capped_glow(exposed_emissive(loaded.emissive))
        );
        assert!(published.emissive.green > loaded.emissive.green);
    }

    /// A strength of exactly 1 is not a glow either: the converter writes the extension only above
    /// 1 (`crates/converter/src/material.rs`, `publish_emissive`), and a record whose multiple is 1
    /// emits its own texture at 1:1 like the own-emit class does.
    #[test]
    fn an_emissive_strength_of_one_is_not_a_deliberate_glow() {
        for strength in ["1.0", "0.5"] {
            let material_json = format!(
                r#"{{"name":"Faint","alphaMode":"OPAQUE","emissiveFactor":[1.0,1.0,1.0],
                    "extensions":{{"KHR_materials_emissive_strength":{{"emissiveStrength":{strength}}}}}}}"#
            );
            let document = bevy::gltf::gltf::Gltf::from_slice(&glb(&material_json)).unwrap();
            let gltf_material = document.document.materials().next().unwrap();
            assert!(
                !is_deliberate_glow(&gltf_material),
                "a strength of {strength} does not emit more than the texture it comes from"
            );
        }
    }

    /// The handler runs for every streamed material, so it must not republish the ones it has
    /// nothing to say about: an ordinary surface with no pair and no emissive, and a material whose
    /// pair asks for the alpha mode glTF already gave it.
    #[test]
    fn a_material_with_nothing_to_change_is_not_republished() {
        let plain = glb(r#"{"name":"StoneWall","alphaMode":"OPAQUE"}"#);
        let document = bevy::gltf::gltf::Gltf::from_slice(&plain).unwrap();
        let gltf_material = document.document.materials().next().unwrap();
        assert!(
            skyrim_material(&gltf_material, &StandardMaterial::default()).is_none(),
            "an ordinary surface is left alone"
        );

        let straight = glb(r#"{"name":"Glass","alphaMode":"BLEND",
                "extensions":{"OPEN_SKYRIM_material":{"blendSource":"SRC_ALPHA",
                "blendDestination":"INV_SRC_ALPHA"}}}"#);
        let document = bevy::gltf::gltf::Gltf::from_slice(&straight).unwrap();
        let gltf_material = document.document.materials().next().unwrap();
        assert!(
            skyrim_material(
                &gltf_material,
                &StandardMaterial {
                    alpha_mode: AlphaMode::Blend,
                    ..default()
                }
            )
            .is_none(),
            "straight alpha-over is what glTF's `BLEND` already renders"
        );
    }

    /// A cell whose four quadrants carry exactly the layers given, in quadrant order.
    fn layer_fixture(cell_id: u32, quadrants: [Vec<TerrainLayerSnapshot>; 4]) -> TerrainSnapshot {
        TerrainSnapshot {
            cell_id,
            width: 33,
            height: 33,
            heights: vec![0.0; 33 * 33],
            normals: [0, 0, 127].repeat(33 * 33),
            vertex_colors: vec![255; 33 * 33 * 3],
            layers: quadrants.into_iter().flatten().collect(),
            water_height: None,
            water_type_form_id: None,
        }
    }

    fn base_layer(quadrant: u8, texture_form_id: u32) -> TerrainLayerSnapshot {
        TerrainLayerSnapshot {
            texture_form_id,
            quadrant,
            layer: 0,
            is_base: true,
            weights: Vec::new(),
        }
    }

    fn overlay_layer(
        quadrant: u8,
        layer: u16,
        texture_form_id: u32,
        weights: Vec<(u16, f32)>,
    ) -> TerrainLayerSnapshot {
        TerrainLayerSnapshot {
            texture_form_id,
            quadrant,
            layer,
            is_base: false,
            weights,
        }
    }

    /// The white squares in the visual review: a quadrant whose `LAND` carries no `BTXT` samples
    /// Bevy's 1x1 white fallback. It takes the base of a neighbouring quadrant of the same cell
    /// instead - the first that has one, in quadrant order - while a quadrant with its own base
    /// keeps it.
    #[test]
    fn quadrant_without_a_base_texture_borrows_a_neighbouring_quadrants_base() {
        let terrain = layer_fixture(
            0x0001_2345,
            [
                vec![base_layer(0, 3)],
                vec![base_layer(1, 0), overlay_layer(1, 1, 9, vec![(0, 1.0)])],
                vec![base_layer(2, 5)],
                vec![base_layer(3, 7)],
            ],
        );
        assert_eq!(quadrant_base_texture_form_id(&terrain, 0).unwrap(), 3);
        assert_eq!(
            quadrant_base_texture_form_id(&terrain, 1).unwrap(),
            3,
            "quadrant 0's base, quadrant 1's first neighbour in quadrant order, not its own overlay"
        );
        assert_eq!(quadrant_base_texture_form_id(&terrain, 2).unwrap(), 5);
        assert_eq!(quadrant_base_texture_form_id(&terrain, 3).unwrap(), 7);

        // Quadrant order, not proximity: quadrant 0 takes quadrant 1's base before quadrant 2's.
        let ordered = layer_fixture(
            0x0001_2345,
            [
                vec![base_layer(0, 0)],
                vec![base_layer(1, 11)],
                vec![base_layer(2, 13)],
                vec![base_layer(3, 17)],
            ],
        );
        assert_eq!(quadrant_base_texture_form_id(&ordered, 0).unwrap(), 11);

        // A neighbouring quadrant whose own base is missing is skipped rather than borrowed from
        // empty: quadrant 2's base is the first textured one after it.
        let skipped = layer_fixture(
            0x0001_2345,
            [
                vec![base_layer(0, 0)],
                vec![base_layer(1, 0)],
                vec![base_layer(2, 0)],
                vec![base_layer(3, 17)],
            ],
        );
        assert_eq!(quadrant_base_texture_form_id(&skipped, 0).unwrap(), 17);
    }

    /// With no textured neighbour the quadrant still avoids the white fallback: it samples the
    /// overlay it paints most of itself with, and a quadrant with nothing to borrow at all keeps
    /// the fallback rather than inventing a texture.
    #[test]
    fn quadrant_without_a_base_texture_or_textured_neighbour_uses_its_strongest_overlay() {
        let terrain = layer_fixture(
            0x0001_2345,
            [
                vec![
                    base_layer(0, 0),
                    overlay_layer(0, 1, 9, vec![(0, 0.25)]),
                    overlay_layer(0, 2, 11, vec![(0, 0.5), (1, 0.5)]),
                    overlay_layer(0, 3, 13, Vec::new()),
                ],
                vec![base_layer(1, 0)],
                vec![base_layer(2, 0)],
                vec![base_layer(3, 0)],
            ],
        );
        assert_eq!(
            quadrant_base_texture_form_id(&terrain, 0).unwrap(),
            11,
            "the overlay covering the most of the quadrant"
        );
        assert_eq!(
            quadrant_base_texture_form_id(&terrain, 1).unwrap(),
            0,
            "an untextured neighbour base is not borrowed, and quadrant 1 has no overlay"
        );

        // An overlay with no `VTXT` opacities covers nothing, so a stronger one wins even when its
        // own texture sits in a later slot.
        let weights_only = layer_fixture(
            0x0001_2345,
            [
                vec![
                    base_layer(0, 0),
                    overlay_layer(0, 1, 9, Vec::new()),
                    overlay_layer(0, 2, 11, vec![(0, 0.1)]),
                ],
                vec![base_layer(1, 0)],
                vec![base_layer(2, 0)],
                vec![base_layer(3, 0)],
            ],
        );
        assert_eq!(quadrant_base_texture_form_id(&weights_only, 0).unwrap(), 11);
    }

    /// A catalogue answering every landscape texture form id the fixtures use, one texture set per
    /// id so each id is its own image path.
    fn texture_catalogue(path: &std::path::Path, form_ids: &[u32]) -> AssetCatalog {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT);
                 CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);
                 CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);",
            )
            .unwrap();
        for form_id in form_ids {
            connection
                .execute(
                    "INSERT INTO landscape_textures VALUES(?1,?1)",
                    rusqlite::params![form_id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO texture_sets VALUES(?1,?2)",
                    rusqlite::params![form_id, format!("textures/land/{form_id}.dds")],
                )
                .unwrap();
        }
        drop(connection);
        AssetCatalog::open(path).unwrap()
    }

    /// The material path, not just the choice: the quadrant the reviewer saw as a white square
    /// binds the borrowed texture in its base slot, and a quadrant with its own base still binds
    /// that instead.
    #[test]
    fn borrowed_base_texture_reaches_the_quadrant_material() {
        let terrain = layer_fixture(
            0x0001_2345,
            [
                vec![base_layer(0, 0)],
                vec![base_layer(1, 0)],
                vec![base_layer(2, 5)],
                vec![base_layer(3, 7)],
            ],
        );
        let directory = tempfile::tempdir().unwrap();
        let catalog = texture_catalogue(&directory.path().join("catalogue.db"), &[5, 7]);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<Image>();
        let asset_server = app.world().resource::<AssetServer>().clone();

        let borrowed = TerrainExtension::from_quadrant(&terrain, 1, &catalog, &asset_server)
            .unwrap()
            .0;
        let sibling = TerrainExtension::from_quadrant(&terrain, 2, &catalog, &asset_server)
            .unwrap()
            .0;
        let own = TerrainExtension::from_quadrant(&terrain, 3, &catalog, &asset_server)
            .unwrap()
            .0;
        assert_eq!(
            borrowed.layer_0, sibling.layer_0,
            "quadrant 1 samples quadrant 2's base texture"
        );
        assert!(
            borrowed.layer_0.is_some(),
            "the base slot must not fall back to Bevy's 1x1 white image"
        );
        assert_ne!(
            borrowed.layer_0, own.layer_0,
            "a quadrant with a base of its own keeps it"
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

    /// The same, for a colour in an encoding `Mesh::ATTRIBUTE_COLOR` cannot carry: Bevy refuses
    /// values whose format differs from the attribute's own declared one
    /// (`bevy_mesh-0.19.0/src/mesh.rs:396-403`, "Invalid attribute format for Vertex_Color"), so a
    /// colour in another encoding reaches a mesh under an attribute that declares that encoding and
    /// carries the id the engine looks the attribute up by.
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

    /// A spread of alphas around the leaf shape's own cutoff - 0.0, 64/255, 112/255 and a fully
    /// opaque vertex - through the rewrite: every alpha exactly 1.0, every RGB triple bit-identical
    /// to what went in.
    #[test]
    fn float_colour_keeps_its_rgb_and_loses_its_alpha() {
        let colours = vec![
            [0.0f32, 0.0, 0.0, 0.0],
            [0.929_411_77, 0.4, 0.2, 64.0 / 255.0],
            [0.2, 0.6, 0.9, 112.0 / 255.0],
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

    /// The bug in one assertion. `TreePineForest03_1:1`'s leaf shape has 296 vertices whose
    /// `COLOR_0` alpha is the histogram below (`docs/research/foliage-alpha-test.md` section 3), and
    /// the `MASK` test Bevy runs compares `vertex_alpha * texture_alpha` against the 112/255 cutoff
    /// the material already publishes. A texture alpha of 1.0 is the densest needle texel - the one
    /// place a fragment survives on a vertex whose own alpha is low - so 176 of the 296 can never
    /// pass the test before the rewrite, and all of them do after it.
    #[test]
    fn the_leaf_shape_recovers_the_vertices_its_cutoff_was_discarding() {
        const CUTOFF: f32 = 112.0 / 255.0;
        const LEAF_ALPHA_HISTOGRAM: [(f32, usize); 5] = [
            (0.0, 110),
            (64.0 / 255.0, 66),
            (160.0 / 255.0, 38),
            (192.0 / 255.0, 20),
            (1.0, 62),
        ];
        let survives = |alpha: f32| alpha * 1.0 >= CUTOFF;
        let colours: Vec<[f32; 4]> = LEAF_ALPHA_HISTOGRAM
            .iter()
            .flat_map(|(alpha, count)| {
                std::iter::repeat_n([0.929_411_77, 0.929_411_77, 0.929_411_77, *alpha], *count)
            })
            .collect();
        assert_eq!(colours.len(), 296, "the shape's vertex count");
        assert_eq!(
            colours.iter().filter(|colour| !survives(colour[3])).count(),
            176,
            "the vertices the cutoff discards before the rewrite"
        );

        let mut mesh = colour_mesh(VertexAttributeValues::Float32x4(colours));
        // Every vertex whose alpha is not already 1.0 is rewritten: the 176 below the cutoff and the
        // 58 between it and full opacity.
        assert_eq!(force_opaque_vertex_alpha(&mut mesh), Ok(234));
        assert!(
            float_colours(&mesh)
                .iter()
                .all(|colour| survives(colour[3])),
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

        // Frame 2 publishes the one `Modified` the rewrite queued, and that pass has nothing left to
        // rewrite.
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
