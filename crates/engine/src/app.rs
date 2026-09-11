use crate::{
    config::EngineConfig,
    metrics::AcceptanceMetricsPlugin,
    profiling::{ProfilingPlugin, ProfilingState},
    render::{
        TerrainExtension, TerrainMaterial, VercidiumRendererPlugin, WaterExtension, WaterMaterial,
        WaterReflectionTexture,
    },
    streaming::{
        AssetFailure, RenderOrigin, StreamingMetrics, StreamingPlugin, build_terrain_quadrant_mesh,
        validate_standard_material,
    },
    world::{
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{ExpectedModelBounds, InstanceBounds, StreamingCamera},
        database::{AssetCatalog, WorldDatabase},
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
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    render::view::screenshot::{Screenshot, save_to_disk},
    window::{PresentMode, WindowPlugin},
};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

#[derive(Resource)]
struct InitialCameraGroundHeight(f32);

pub fn run(config: EngineConfig) -> Result<()> {
    let runtime_data = if config.benchmark_only
        || config.material_fixture
        || config.terrain_water_fixture
        || config.transform_bounds_fixture
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
    let window = (!config.headless).then(|| Window {
        title: "OpenSkyrim".into(),
        resolution: (1600, 900).into(),
        present_mode: if config.benchmark_frames.is_some()
            || config.benchmark_duration_secs.is_some()
        {
            PresentMode::AutoNoVsync
        } else {
            PresentMode::AutoVsync
        },
        ..default()
    });
    let origin = RenderOrigin(IVec2::new(config.start_grid.0, config.start_grid.1));
    let mut app = App::new();
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
        app.insert_resource(database)
            .insert_resource(catalog)
            .insert_resource(cache)
            .insert_resource(ground_height)
            .add_plugins(StreamingPlugin);
        app.add_systems(Startup, setup_world);
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
    } else {
        app.add_systems(Startup, setup_world);
        app.add_systems(Startup, setup_synthetic_benchmark);
    }
    app.run();
    Ok(())
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
    12
}

fn setup_synthetic_benchmark(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
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
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(1024.0, 1024.0))),
        MeshMaterial3d(water_materials.add(WaterMaterial {
            base: StandardMaterial {
                base_color: Color::srgba(0.04, 0.18, 0.3, 0.7),
                metallic: 0.1,
                perceptual_roughness: 0.08,
                alpha_mode: AlphaMode::Blend,
                ..default()
            },
            extension: WaterExtension::default(),
        })),
        Transform::from_xyz(CELL_SIZE_HALF, 8.0, -CELL_SIZE_HALF),
        crate::world::components::WaterSurface,
        RenderLayers::layer(1),
    ));
    info!(
        instances = config.synthetic_instances,
        "synthetic indirect-render benchmark initialized"
    );
    profiler.increment("synthetic/instances", config.synthetic_instances as u64);
    profiler.record_elapsed("startup/synthetic_scene", started);
}

fn setup_world(
    mut commands: Commands,
    config: Res<EngineConfig>,
    ground_height: Option<Res<InitialCameraGroundHeight>>,
) {
    let ground_height = ground_height.as_deref().map_or(0.0, |height| height.0);
    let target = Vec3::new(CELL_SIZE_HALF, ground_height, -CELL_SIZE_HALF);
    let camera_position = target + Vec3::new(0.0, 1200.0, 2500.0);
    let far = crate::world::components::CELL_SIZE * (config.stream_radius.max(1) + 2) as f32 * 2.0;
    commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection { far, ..default() }),
        Transform::from_translation(camera_position).looking_at(target, Vec3::Y),
        StreamingCamera,
        Msaa::Off,
        DepthPrepass,
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
            "SELECT id FROM cells WHERE worldspace_id=?1 AND grid_x=?2 AND grid_y=?3",
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

fn fly_camera(
    time: Res<Time>,
    config: Res<EngineConfig>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
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
    if config.auto_fly_speed > 0.0 {
        direction += *transform.forward();
    }
    let speed = if config.auto_fly_speed > 0.0 {
        config.auto_fly_speed
    } else if keyboard.pressed(KeyCode::ControlLeft) {
        4000.0
    } else {
        900.0
    };
    transform.translation += direction.normalize_or_zero() * speed * time.delta_secs();
    profiler.record_elapsed("world/fly_camera", started);
}

fn capture_acceptance_screenshot(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut state: Local<ScreenshotCaptureState>,
    streaming: Option<Res<StreamingMetrics>>,
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
            && (!config.material_fixture || metrics.canonical_fixture_validated)
            && (!config.terrain_water_fixture || metrics.terrain_water_fixture_validated)
            && (!config.transform_bounds_fixture || metrics.transform_bounds_fixture_validated)
    });
    if !assets_ready {
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
            br#"{"schema_version":3,"passed":true}"#,
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
            br#"{"schema_version":3,"passed":true}"#,
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        validate_runtime_assets(&config).unwrap();
    }
}
