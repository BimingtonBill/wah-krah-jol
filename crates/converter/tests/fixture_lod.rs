//! Fixture tests for the distant-LOD mesh containers (`.btr`/`.bto`) and the
//! distant-LOD database contract (`.lod`, `.lst`, `.btt` and the generated
//! billboards).
//!
//! Skyrim ships its distant terrain and object LOD as NIFs in a different
//! container: a `BSMultiBoundNode` (with a `BSMultiBound` → `BSMultiBoundAABB`
//! pair) holding shape blocks that may carry per-vertex tint. These tests drive
//! the generated fixtures through the real conversion path, so no game data is
//! required.

use converter::{AssetPipeline, PipelineConfig, PipelineReport, mesh::MeshConverter};
use dummy_content::{
    dds,
    nif::{LodShape, StaticShape, object_lod, static_shape, terrain_lod},
    rng::Rng,
};
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use std::{fs, path::Path};

const DIFFUSE: &str = "textures/terrain/generated/generated.4.0.0.dds";
const NORMAL: &str = "textures/terrain/generated/generated.4.0.0_n.dds";
/// The worldspace atlas every tree billboard of the fixture samples.
const ATLAS: &str = "textures/terrain/generated/trees/generatedtreelod.dds";

/// A one-cell quad with distinct per-vertex colours, shaped like a LOD block:
/// horizontal in Creation space (Z is up), so it stays horizontal once the
/// exporter applies the Creation-to-runtime basis rotation.
fn lod_quad<'a>(diffuse: &'a str, normal: &'a str) -> LodShape<'a> {
    LodShape {
        name: "GeneratedLodQuad",
        positions: &[
            [0.0, 0.0, 0.0],
            [4096.0, 0.0, 0.0],
            [4096.0, 4096.0, 0.0],
            [0.0, 4096.0, 0.0],
        ],
        normals: &[[0.0, 0.0, 1.0]; 4],
        uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        indices: &[[0, 1, 2], [0, 2, 3]],
        colors: &[
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 128],
        ],
        diffuse,
        normal_texture: normal,
    }
}

fn glb_json(bytes: &[u8]) -> Value {
    assert_eq!(&bytes[..4], b"glTF", "invalid GLB signature");
    assert_eq!(&bytes[16..20], b"JSON", "missing GLB JSON chunk");
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    serde_json::from_slice(&bytes[20..20 + json_length]).expect("invalid glTF JSON")
}

/// The `f32` values of a glTF accessor, read from the GLB binary chunk.
fn accessor_floats(bytes: &[u8], document: &Value, accessor: &Value) -> Vec<f32> {
    let accessor = &document["accessors"][accessor.as_u64().expect("accessor index") as usize];
    assert_eq!(accessor["componentType"], 5126, "expected a f32 accessor");
    let view = &document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
    let components = match accessor["type"].as_str().unwrap() {
        "VEC2" => 2,
        "VEC3" => 3,
        "VEC4" => 4,
        other => panic!("unsupported accessor type {other}"),
    };
    let count = accessor["count"].as_u64().unwrap() as usize * components;
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let binary = 20 + json_length.next_multiple_of(4) + 8;
    let start = binary
        + view["byteOffset"].as_u64().unwrap_or(0) as usize
        + accessor["byteOffset"].as_u64().unwrap_or(0) as usize;
    let mut values = Vec::with_capacity(count);
    for chunk in bytes[start..start + count * 4].as_chunks::<4>().0 {
        values.push(f32::from_le_bytes(*chunk));
    }
    values
}

/// Asserts one vertex's RGB channels. Alpha is deliberately unchecked: how it
/// is interpreted is decided elsewhere.
fn assert_vertex_rgb(colors: &[f32], vertex: usize, expected: [f32; 3]) {
    let actual = &colors[vertex * 4..vertex * 4 + 3];
    for (channel, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
        assert!(
            (actual - expected).abs() < 1.0e-6,
            "vertex {vertex} channel {channel}: {actual} != {expected} ({colors:?})"
        );
    }
}

/// Asserts the exported `COLOR_0` stream still carries the fixture's tint:
/// red, green, blue and white, in vertex order.
fn assert_lod_vertex_colours(bytes: &[u8], document: &Value, primitive: &Value) {
    let attribute = primitive["attributes"]
        .get("COLOR_0")
        .unwrap_or_else(|| panic!("LOD vertex colours were dropped: {primitive}"));
    let colors = accessor_floats(bytes, document, attribute);
    assert_eq!(colors.len(), 16, "unexpected COLOR_0 stream: {colors:?}");
    assert_vertex_rgb(&colors, 0, [1.0, 0.0, 0.0]);
    assert_vertex_rgb(&colors, 1, [0.0, 1.0, 0.0]);
    assert_vertex_rgb(&colors, 2, [0.0, 0.0, 1.0]);
    assert_vertex_rgb(&colors, 3, [1.0, 1.0, 1.0]);
}

/// The single-child chain of node names from the scene root down.
fn node_chain(document: &Value) -> Vec<String> {
    let mut index = document["scenes"][0]["nodes"][0]
        .as_u64()
        .expect("the scene has a root node");
    let mut chain = Vec::new();
    loop {
        let node = &document["nodes"][index as usize];
        chain.push(node["name"].as_str().unwrap_or_default().to_owned());
        match node["children"]
            .as_array()
            .and_then(|children| children.first())
            .and_then(Value::as_u64)
        {
            Some(child) => index = child,
            None => return chain,
        }
    }
}

fn write_terrain_lod(directory: &Path) -> std::path::PathBuf {
    let path = directory.join("generated.4.0.0.btr");
    fs::write(&path, terrain_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap()).unwrap();
    path
}

fn write_object_lod(directory: &Path) -> std::path::PathBuf {
    let path = directory.join("generated.4.0.0.bto");
    fs::write(&path, object_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap()).unwrap();
    path
}

/// The same quad as [`lod_quad`], written by the static (`.nif`) writer.
fn static_quad() -> StaticShape<'static> {
    let quad = lod_quad(DIFFUSE, NORMAL);
    StaticShape {
        name: "GeneratedQuad",
        positions: quad.positions,
        normals: quad.normals,
        uvs: quad.uvs,
        indices: quad.indices,
        diffuse: quad.diffuse,
        normal_texture: quad.normal_texture,
    }
}

/// ADR-0005: every generated format is driven through the parser under
/// deterministic truncation (every prefix) and bounded mutation, and a panic
/// fails the suite.
#[test]
fn generated_nifs_never_panic_under_truncation_or_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("generated.nif");
    let mut rng = Rng::new(13);
    let fixtures = [
        static_shape(&static_quad()).unwrap(),
        terrain_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap(),
        object_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap(),
    ];
    for bytes in fixtures {
        for length in 0..bytes.len() {
            fs::write(&input, &bytes[..length]).unwrap();
            let result = std::panic::catch_unwind(|| MeshConverter::inspect_nif(&input));
            assert!(result.is_ok(), "NIF parser panicked at length {length}");
        }
        for _ in 0..128 {
            let mut mutated = bytes.clone();
            let index = rng.next_u64() as usize % mutated.len();
            mutated[index] ^= 0xff;
            fs::write(&input, &mutated).unwrap();
            let result = std::panic::catch_unwind(|| MeshConverter::inspect_nif(&input));
            assert!(result.is_ok(), "NIF parser panicked on mutation at {index}");
        }
    }
}

#[test]
fn fixture_terrain_lod_converts_with_vertex_colours_and_texture_uris() {
    let directory = tempfile::tempdir().unwrap();
    let source = write_terrain_lod(directory.path());
    let output = directory
        .path()
        .join("meshes/terrain/generated/generated.4.0.0.glb");

    let diagnostics = MeshConverter::inspect_nif(&source).unwrap();
    assert_eq!(diagnostics.block_count, 6);
    assert_eq!(diagnostics.parsed_block_count, 6);
    assert!(
        diagnostics.fallback_blocks.is_empty(),
        "unparsed blocks: {:?}",
        diagnostics.fallback_blocks
    );
    assert_eq!(diagnostics.geometry_block_count, 1);
    assert_eq!(diagnostics.validated_material_shape_count, 1);
    // The container node and its shape are both scene nodes, one below the
    // other: previously the container fell through to `Unhandled` and the
    // shape was orphaned.
    assert_eq!(diagnostics.scene_node_count, 2);
    assert_eq!(diagnostics.max_scene_depth, 2);

    MeshConverter::convert_nif_to_glb(&source, &output).unwrap();
    let bytes = fs::read(&output).unwrap();
    let document = glb_json(&bytes);

    assert_eq!(
        node_chain(&document),
        vec![
            "Creation-to-glTF basis".to_owned(),
            "TerrainLodBlock".to_owned(),
            "GeneratedLodQuad".to_owned(),
        ]
    );
    let primitive = &document["meshes"][0]["primitives"][0];
    assert_lod_vertex_colours(&bytes, &document, primitive);
    assert!(primitive["attributes"].get("TEXCOORD_0").is_some());
    assert!(primitive["attributes"].get("NORMAL").is_some());

    let uris = MeshConverter::glb_texture_uris(&output).unwrap();
    assert_eq!(uris.len(), 2, "{uris:?}");
    assert!(
        uris.iter().any(
            |uri| uri.ends_with("textures/terrain/generated/generated.4.0.0.opensky-srgb.ktx2")
        ),
        "{uris:?}"
    );
    assert!(
        uris.iter()
            .any(|uri| uri.ends_with("textures/terrain/generated/generated.4.0.0_n.ktx2")),
        "{uris:?}"
    );

    // The block-local quad spans one cell on the two horizontal runtime axes
    // and stays flat on the vertical one.
    let bounds = MeshConverter::glb_bounds(&output).unwrap();
    let extents = [
        bounds.max[0] - bounds.min[0],
        bounds.max[1] - bounds.min[1],
        bounds.max[2] - bounds.min[2],
    ];
    assert!(extents[1].abs() < 1.0e-3, "{bounds:?}");
    for axis in [0, 2] {
        assert!((extents[axis] - 4096.0).abs() < 1.0e-3, "{bounds:?}");
    }
}

#[test]
fn fixture_object_lod_converts_with_its_nested_hierarchy() {
    let directory = tempfile::tempdir().unwrap();
    let source = write_object_lod(directory.path());
    let output = directory
        .path()
        .join("meshes/terrain/generated/objects/generated.4.0.0.glb");

    let diagnostics = MeshConverter::inspect_nif(&source).unwrap();
    assert_eq!(diagnostics.block_count, 7);
    assert_eq!(diagnostics.parsed_block_count, 7);
    assert!(
        diagnostics.fallback_blocks.is_empty(),
        "unparsed blocks: {:?}",
        diagnostics.fallback_blocks
    );
    assert_eq!(diagnostics.geometry_block_count, 1);
    assert_eq!(diagnostics.validated_material_shape_count, 1);
    assert_eq!(diagnostics.scene_node_count, 3);
    assert_eq!(diagnostics.max_scene_depth, 3);

    MeshConverter::convert_nif_to_glb(&source, &output).unwrap();
    let bytes = fs::read(&output).unwrap();
    let document = glb_json(&bytes);

    assert_eq!(
        node_chain(&document),
        vec![
            "Creation-to-glTF basis".to_owned(),
            "ObjectLodRoot".to_owned(),
            "ObjectLodBlock".to_owned(),
            "GeneratedLodQuad".to_owned(),
        ]
    );
    // The atlas UVs and the tint both survive the nested container.
    let primitive = &document["meshes"][0]["primitives"][0];
    assert_lod_vertex_colours(&bytes, &document, primitive);
    assert!(primitive["attributes"].get("TEXCOORD_0").is_some());
}

#[tokio::test]
async fn pipeline_publishes_terrain_and_object_lod_glbs() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    fs::create_dir_all(data.join("meshes/terrain/generated/objects")).unwrap();
    fs::create_dir_all(data.join("textures/terrain/generated")).unwrap();
    fs::write(
        data.join("meshes/terrain/generated/generated.4.0.0.btr"),
        terrain_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap(),
    )
    .unwrap();
    fs::write(
        data.join("meshes/terrain/generated/objects/generated.4.0.0.bto"),
        object_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap(),
    )
    .unwrap();
    let mut rng = Rng::new(7);
    fs::write(
        data.join("textures/terrain/generated/generated.4.0.0.dds"),
        dds::generate(
            &dds::Spec::new(dds::Format::Bc1Unorm, 64, 64).with_mip_levels(7),
            &mut rng,
        )
        .unwrap(),
    )
    .unwrap();
    fs::write(
        data.join("textures/terrain/generated/generated.4.0.0_n.dds"),
        dds::generate(
            &dds::Spec::new(dds::Format::Bc5Unorm, 64, 64).with_mip_levels(7),
            &mut rng,
        )
        .unwrap(),
    )
    .unwrap();

    let output = directory.path().join("modern");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
        .await
        .unwrap();
    drain.await.unwrap();

    assert!(report.complete, "conversion did not complete: {report:?}");
    assert_eq!(report.skipped, 0, "{:?}", report.warnings);
    assert_eq!(report.inputs_by_kind.get("btr"), Some(&1));
    assert_eq!(report.inputs_by_kind.get("bto"), Some(&1));
    for relative in [
        "meshes/terrain/generated/generated.4.0.0.glb",
        "meshes/terrain/generated/objects/generated.4.0.0.glb",
        "textures/terrain/generated/generated.4.0.0.ktx2",
        "textures/terrain/generated/generated.4.0.0_n.ktx2",
    ] {
        assert!(output.join(relative).is_file(), "missing {relative}");
    }
    // The published GLBs still point at the converted block textures.
    let uris = MeshConverter::glb_texture_uris(
        &output.join("meshes/terrain/generated/generated.4.0.0.glb"),
    )
    .unwrap();
    assert!(!uris.is_empty(), "LOD GLB lost its texture references");
}

/// A `lodsettings/<worldspace>.lod` header: origin, cells per side and levels.
fn lod_header(origin: [i16; 2], cells: i32, min_level: i32, max_level: i32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&origin[0].to_le_bytes());
    bytes.extend_from_slice(&origin[1].to_le_bytes());
    bytes.extend_from_slice(&cells.to_le_bytes());
    bytes.extend_from_slice(&min_level.to_le_bytes());
    bytes.extend_from_slice(&max_level.to_le_bytes());
    bytes
}

/// One 32-byte `.lst` entry: index, size, atlas rectangle, unused float.
fn lst_entry(index: u32, size: [f32; 2], uv: [f32; 4]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(&index.to_le_bytes());
    for value in size.into_iter().chain(uv) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&0.0f32.to_le_bytes());
    bytes
}

/// A `.lst` table: a count followed by its entries.
fn lst_table(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = (entries.len() as u32).to_le_bytes().to_vec();
    for entry in entries {
        bytes.extend_from_slice(entry);
    }
    bytes
}

/// One 32-byte `.btt` instance.
fn btt_instance(position: [f32; 3], rotation: f32, scale: f32, form_id: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(32);
    for value in position.into_iter().chain([rotation, scale]) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&form_id.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes
}

/// A `.btt` block: `(type index, instances)` groups.
fn btt_block(groups: &[(u32, Vec<Vec<u8>>)]) -> Vec<u8> {
    let mut bytes = (groups.len() as u32).to_le_bytes().to_vec();
    for (type_index, instances) in groups {
        bytes.extend_from_slice(&type_index.to_le_bytes());
        bytes.extend_from_slice(&(instances.len() as u32).to_le_bytes());
        for instance in instances {
            bytes.extend_from_slice(instance);
        }
    }
    bytes
}

/// Writes a texture fixture at `relative`, creating its directory.
fn write_texture(root: &Path, relative: &str, spec: dds::Spec) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut rng = Rng::new(5);
    fs::write(path, dds::generate(&spec, &mut rng).unwrap()).unwrap();
}

/// The archive entry name of each distant-LOD metadata file. The loose path
/// under the Data folder is the same string, which is what lets a loose copy
/// replace the archived one.
const LOD_GRID: &str = "lodsettings/generated.lod";
const LOD_TABLE: &str = "meshes/terrain/generated/trees/generated.lst";
const LOD_BLOCK: &str = "meshes/terrain/generated/trees/generated.4.0.0.btt";

/// The fixture's distant-LOD metadata: the three files the inventory reads,
/// under the names the generated worldspace uses.
#[derive(Clone)]
struct LodMetadata {
    /// `LOD_GRID`.
    grid: Vec<u8>,
    /// `LOD_TABLE`.
    table: Vec<u8>,
    /// `LOD_BLOCK`.
    instances: Vec<u8>,
}

impl LodMetadata {
    /// What the game ships: a 256-cell grid of levels 4..32, two billboard
    /// types and three instances, one of which is stale.
    fn shipped() -> Self {
        Self {
            grid: lod_header([-96, -96], 256, 4, 32),
            table: lst_table(&[
                lst_entry(0, [128.0, 256.0], [0.0, 0.0, 0.25, 0.5]),
                lst_entry(1, [64.0, 512.0], [0.25, 0.5, 0.5, 1.0]),
            ]),
            instances: btt_block(&[(
                1,
                vec![
                    btt_instance([100.0, 200.0, 30.0], 0.5, 1.0, 0x12),
                    btt_instance([400.0, 500.0, 60.0], 1.0, 0.5, 0x0200_0022),
                    btt_instance([700.0, 800.0, 90.0], 1.5, 1.25, 0x99),
                ],
            )]),
        }
    }

    /// The same three file names with different contents: another grid origin
    /// and level range, one billboard type and two instances. Nothing here
    /// overlaps [`LodMetadata::shipped`] by accident, so a conversion that reads
    /// this copy is unmistakable.
    fn replacement() -> Self {
        Self {
            grid: lod_header([-64, -64], 128, 4, 16),
            table: lst_table(&[lst_entry(0, [200.0, 400.0], [0.0, 0.0, 0.5, 0.5])]),
            instances: btt_block(&[(
                0,
                vec![
                    btt_instance([1000.0, 2000.0, 3000.0], 2.0, 1.5, 0x12),
                    btt_instance([-5.0, -6.0, -7.0], 0.25, 1.0, 0x0200_0022),
                ],
            )]),
        }
    }

    /// The three files with the path each ships under, archive entry and loose
    /// file alike.
    fn files(&self) -> [(&'static str, &[u8]); 3] {
        [
            (LOD_GRID, &self.grid),
            (LOD_TABLE, &self.table),
            (LOD_BLOCK, &self.instances),
        ]
    }
}

/// Builds the fixture's Data folder: one plugin with two cells, a terrain and an
/// object block, the tree atlas and textures, and the LOD metadata.
///
/// `archived` metadata is packed into `Generated - Meshes.bsa` the way the game
/// ships it, `loose` metadata is written under the Data folder the way LOD mods
/// ship it. Either may be absent; giving both is what a mod on top of the game's
/// archives looks like.
fn lod_fixture(data: &Path, archived: Option<&LodMetadata>, loose: Option<&LodMetadata>) {
    fs::create_dir_all(data).unwrap();
    let cells = [
        dummy_content::esm::Cell {
            grid_x: 0,
            grid_y: 0,
        },
        dummy_content::esm::Cell {
            grid_x: 1,
            grid_y: 0,
        },
    ];
    fs::write(
        data.join("generated.esm"),
        dummy_content::esm::plugin(&dummy_content::esm::Plugin {
            author: "OpenSkyrim",
            worldspace: "Generated",
            cells: &cells,
            model_path: "meshes/generated/quad.nif",
            diffuse: DIFFUSE,
            normal_texture: NORMAL,
        })
        .unwrap(),
    )
    .unwrap();
    if let Some(metadata) = archived {
        let entries = metadata
            .files()
            .map(|(path, bytes)| dummy_content::Entry::new(path, bytes));
        fs::write(
            data.join("Generated - Meshes.bsa"),
            dummy_content::bsa::v105(&entries, dummy_content::bsa::Compression::None).unwrap(),
        )
        .unwrap();
    }
    if let Some(metadata) = loose {
        for (relative, bytes) in metadata.files() {
            let path = data.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }
    fs::create_dir_all(data.join("meshes/terrain/generated/objects")).unwrap();
    fs::write(
        data.join("meshes/terrain/generated/generated.4.0.0.btr"),
        terrain_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap(),
    )
    .unwrap();
    fs::write(
        data.join("meshes/terrain/generated/objects/generated.4.0.0.bto"),
        object_lod(&lod_quad(DIFFUSE, NORMAL)).unwrap(),
    )
    .unwrap();
    write_texture(
        data,
        DIFFUSE,
        dds::Spec::new(dds::Format::Bc1Unorm, 64, 64).with_mip_levels(7),
    );
    write_texture(
        data,
        NORMAL,
        dds::Spec::new(dds::Format::Bc5Unorm, 64, 64).with_mip_levels(7),
    );
    write_texture(
        data,
        ATLAS,
        dds::Spec::new(dds::Format::Bc1Unorm, 256, 256).with_mip_levels(9),
    );
}

/// Converts one fixture asset set and returns the temporary directory that
/// holds it (the caller must keep it alive), its assets root and the report.
async fn convert_lod_fixture(
    archived: Option<LodMetadata>,
    loose: Option<LodMetadata>,
) -> (tempfile::TempDir, std::path::PathBuf, PipelineReport) {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    lod_fixture(&data, archived.as_ref(), loose.as_ref());
    let output = directory.path().join("modern");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
        .await
        .unwrap();
    drain.await.unwrap();
    (directory, output, report)
}

/// The rows of a query as JSON, values in column order, so two conversions can
/// be compared field by field.
fn query_rows(connection: &rusqlite::Connection, sql: &str) -> Value {
    let mut statement = connection.prepare(sql).unwrap();
    let columns = statement.column_count();
    let rows = statement
        .query_map([], |row| {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                values.push(match row.get_ref(column)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(value) => json!(value),
                    ValueRef::Real(value) => json!(value),
                    ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
                    ValueRef::Blob(value) => json!(format!("<{} bytes>", value.len())),
                });
            }
            Ok(Value::Array(values))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    Value::Array(rows)
}

/// Everything one conversion wrote for the distant LOD: the world database rows
/// and each published billboard's glTF document.
fn lod_snapshot(output: &Path) -> Value {
    let trees = output.join("meshes/terrain/generated/trees");
    let mut billboards: Vec<(String, Value)> = fs::read_dir(&trees)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "glb"))
        .map(|path| {
            let relative = path
                .strip_prefix(output)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (relative, glb_json(&fs::read(&path).unwrap()))
        })
        .collect();
    billboards.sort_by(|left, right| left.0.cmp(&right.0));
    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    json!({
        "grid": query_rows(&connection, "SELECT origin_x, origin_y, levels FROM lod_grid"),
        "blocks": query_rows(
            &connection,
            "SELECT kind, level, block_x, block_y, mesh_path FROM lod_block ORDER BY kind",
        ),
        "tree_types": query_rows(
            &connection,
            "SELECT tree_index, mesh_path, size_x, size_y, u0, v0, u1, v1 FROM lod_tree_type \
             ORDER BY tree_index",
        ),
        "tree_instances": query_rows(
            &connection,
            "SELECT block_x, block_y, tree_index, pos_x, pos_y, pos_z, rotation, scale \
             FROM lod_tree_instance ORDER BY pos_x, pos_z",
        ),
        "billboards": billboards,
    })
}

/// The conversion of a worldspace with one terrain block, one object block, two
/// tree types and three tree instances, one of which is stale. The ESM defines
/// two cells, so references `0x12` and `0x22` exist and instance FormIDs of
/// `0x12` and `0x02000022` (authored at the plugin's own index, which is zero
/// masters long) resolve to them.
#[tokio::test]
async fn pipeline_records_the_distant_lod_inventory_and_publishes_billboards() {
    let (_fixture, output, report) = convert_lod_fixture(Some(LodMetadata::shipped()), None).await;

    assert!(report.complete, "{:?}", report.warnings);
    let lod = report
        .lod
        .as_ref()
        .expect("the fixture has a LOD inventory");
    assert!(lod.errors.is_empty(), "{:?}", lod.errors);
    assert!(lod.issues.is_empty(), "{:?}", lod.issues);
    assert_eq!(
        (
            lod.worldspaces,
            lod.grids,
            lod.terrain_blocks,
            lod.object_blocks,
            lod.tree_types,
            lod.tree_instances,
        ),
        (1, 1, 1, 1, 2, 3)
    );
    assert_eq!(
        lod.tree_instances_unresolved, 1,
        "0x12 and 0x02000022 resolve against the fixture references, 0x99 does not"
    );
    assert_eq!(lod.billboards.len(), 2);

    // The billboards and both forms of the atlas are published.
    for relative in [
        "meshes/terrain/generated/trees/generated.tree.0.glb",
        "meshes/terrain/generated/trees/generated.tree.1.glb",
        "textures/terrain/generated/trees/generatedtreelod.ktx2",
        "textures/terrain/generated/trees/generatedtreelod.opensky-srgb.ktx2",
    ] {
        assert!(output.join(relative).is_file(), "missing {relative}");
    }
    let billboard = output.join("meshes/terrain/generated/trees/generated.tree.0.glb");
    let uris = MeshConverter::glb_texture_uris(&billboard).unwrap();
    assert_eq!(uris.len(), 1, "{uris:?}");
    assert!(
        uris[0].ends_with("trees/generatedtreelod.opensky-srgb.ktx2"),
        "{uris:?}"
    );
    assert!(uris[0].starts_with("../../../../textures/"), "{uris:?}");

    let integration = report.integration.as_ref().expect("integration report");
    assert!(integration.passed, "{integration:?}");
    assert_eq!(
        (
            integration.lod_grids,
            integration.lod_terrain_blocks,
            integration.lod_object_blocks,
            integration.lod_tree_types,
            integration.lod_tree_instances,
            integration.missing_lod_mesh_count,
        ),
        (1, 1, 1, 2, 3, 0)
    );

    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    let (levels, worldspace): (String, i64) = connection
        .query_row("SELECT levels, worldspace_id FROM lod_grid", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(levels, "4,8,16,32");
    assert_eq!(worldspace, 1, "the fixture worldspace form id");
    let objects: String = connection
        .query_row(
            "SELECT mesh_path FROM lod_block WHERE kind = 'objects'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        objects,
        "meshes/terrain/generated/objects/generated.4.0.0.glb"
    );
    let trees: i64 = connection
        .query_row(
            "SELECT count(*) FROM lod_tree_instance WHERE tree_index = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trees, 3);
}

/// Loose `.lod`, `.lst` and `.btt` files — what a LOD mod ships — must be read
/// exactly like the archived copies they stand in for: same grids, tree types,
/// tree instances and billboards.
#[tokio::test]
async fn pipeline_reads_loose_lod_metadata_like_the_archived_copy() {
    let (_archived_fixture, archived_output, archived_report) =
        convert_lod_fixture(Some(LodMetadata::shipped()), None).await;
    let (_loose_fixture, loose_output, loose_report) =
        convert_lod_fixture(None, Some(LodMetadata::shipped())).await;

    let loose = loose_report
        .lod
        .as_ref()
        .expect("the fixture has a LOD inventory");
    assert!(loose_report.complete, "{:?}", loose_report.warnings);
    assert_eq!(
        (
            loose.worldspaces,
            loose.grids,
            loose.tree_types,
            loose.tree_instances,
        ),
        (1, 1, 2, 3),
        "the loose metadata was not read: {loose:?}"
    );
    assert_eq!(
        serde_json::to_value(&loose_report.lod).unwrap(),
        serde_json::to_value(&archived_report.lod).unwrap(),
        "the loose metadata produced a different inventory report"
    );
    assert_eq!(lod_snapshot(&loose_output), lod_snapshot(&archived_output));
}

/// A loose copy of a LOD metadata file overrides the archived copy, the way the
/// game resolves the Data folder against the archives beside it.
#[tokio::test]
async fn pipeline_prefers_loose_lod_metadata_over_the_archived_copy() {
    let (_fixture, output, report) = convert_lod_fixture(
        Some(LodMetadata::shipped()),
        Some(LodMetadata::replacement()),
    )
    .await;
    let lod = report
        .lod
        .as_ref()
        .expect("the fixture has a LOD inventory");
    assert!(lod.errors.is_empty(), "{:?}", lod.errors);
    assert_eq!(
        (
            lod.grids,
            lod.tree_types,
            lod.tree_instances,
            lod.tree_instances_unresolved,
        ),
        (1, 1, 2, 0),
        "the archived metadata won: {lod:?}"
    );

    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    let grid: (i16, i16, String) = connection
        .query_row(
            "SELECT origin_x, origin_y, levels FROM lod_grid",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(grid, (-64, -64, "4,8,16".to_owned()));
    let billboard: (f32, f32) = connection
        .query_row("SELECT size_x, size_y FROM lod_tree_type", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(billboard, (200.0, 400.0));
    let instances: Vec<(f32, f32, f32)> = {
        let mut statement = connection
            .prepare("SELECT pos_x, pos_y, pos_z FROM lod_tree_instance ORDER BY pos_x")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        instances,
        vec![(-5.0, -6.0, -7.0), (1000.0, 2000.0, 3000.0)]
    );

    // The archived table has two billboard types and the loose one has a
    // single type, so the second published billboard must be gone.
    let trees = output.join("meshes/terrain/generated/trees");
    assert!(trees.join("generated.tree.0.glb").is_file());
    assert!(!trees.join("generated.tree.1.glb").is_file());
}

/// Parses every real `.lod`, `.lst` and `.btt` under a directory. Requires game
/// data, so it is ignored by default:
///
/// ```text
/// OPENSKYRIM_LOD_FIXTURE="<converted assets>/vfs" \
///     cargo test -p converter --test fixture_lod -- --ignored real_tree_lod
/// ```
///
/// Every `.lst` and `.btt` the game ships must decode, including the six
/// `dlc2solstheimworld` blocks that carry bytes after their declared groups;
/// point it at `vfs/meshes/terrain` for the tree tables alone, or at `vfs` to
/// cover `lodsettings` as well. The ignored mesh test above reads the same
/// variable but expects a single `.btr`/`.bto` file, so run them separately.
#[test]
#[ignore = "requires OPENSKYRIM_LOD_FIXTURE pointing at a directory of real .lod/.lst/.btt files"]
fn real_tree_lod_decodes() {
    let sample = std::env::var_os("OPENSKYRIM_LOD_FIXTURE").map(std::path::PathBuf::from);
    let Some(root) = sample else {
        eprintln!("skipping: set OPENSKYRIM_LOD_FIXTURE to a directory of real LOD files");
        return;
    };
    if root.is_file() && !is_lod_file(&root) {
        eprintln!(
            "skipping: {root:?} is not a .lod/.lst/.btt file; point the variable at a tree LOD directory"
        );
        return;
    }
    let mut tables = 0u64;
    let mut blocks = 0u64;
    let mut instances = 0u64;
    let mut trailing_blocks = 0u64;
    let mut trailing_bytes = 0u64;
    let mut grids = 0u64;
    for entry in walkdir::WalkDir::new(&root).follow_links(false) {
        let path = entry.unwrap().into_path();
        if !path.is_file() {
            continue;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("lst") => {
                let types = converter::lod::parse_lst(&fs::read(&path).unwrap()).unwrap();
                tables += 1;
                println!("{}: {} billboard types", path.display(), types.len());
            }
            Some("btt") => {
                let stem = path.file_stem().unwrap().to_str().unwrap();
                let block = converter::lod::parse_block_stem(stem).expect("a LOD block file name");
                let bytes = fs::read(&path).unwrap();
                let parsed = converter::lod::parse_btt(&bytes, block.x, block.y).unwrap();
                blocks += 1;
                instances += parsed
                    .groups
                    .iter()
                    .map(|group| group.instances.len() as u64)
                    .sum::<u64>();
                if !parsed.trailing.is_empty() {
                    trailing_blocks += 1;
                    trailing_bytes += parsed.trailing.len() as u64;
                    println!(
                        "{}: {} trailing bytes after {} groups",
                        path.display(),
                        parsed.trailing.len(),
                        parsed.groups.len()
                    );
                }
            }
            Some("lod") => {
                let grid = converter::lod::parse_lod_grid(&fs::read(&path).unwrap()).unwrap();
                grids += 1;
                println!(
                    "{}: origin {},{} levels {:?}",
                    path.display(),
                    grid.origin_x,
                    grid.origin_y,
                    grid.levels().unwrap()
                );
            }
            _ => {}
        }
    }
    println!(
        "{root:?}: {grids} grids, {tables} tables, {blocks} blocks, {instances} instances, \
         {trailing_blocks} blocks with trailing bytes ({trailing_bytes} bytes)"
    );
    assert!(
        tables > 0 || blocks > 0 || grids > 0,
        "no LOD files under {root:?}"
    );
}

/// Whether a path is one of the distant-LOD file kinds this test can decode.
fn is_lod_file(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("lod")
                || extension.eq_ignore_ascii_case("lst")
                || extension.eq_ignore_ascii_case("btt")
        })
}

/// Runs one real `.btr`/`.bto` through the parser. Requires game data, so it is
/// ignored by default:
///
/// ```text
/// OPENSKYRIM_LOD_FIXTURE="/path/to/meshes/terrain/tamriel/tamriel.32.32.-96.btr" \
///     cargo test -p converter --test fixture_lod -- --ignored real_lod
/// ```
///
/// Point it at each level (`tamriel.4.*`, `tamriel.8.*`, `tamriel.16.*`,
/// `tamriel.32.*`) and at `objects/tamriel.*.bto`, which exercise a different
/// shape block. `BSDistantObjectLargeRefExtraData` is not a geometry block, so
/// it may still report as unparsed; every LOD container and geometry block must
/// not.
#[test]
#[ignore = "requires OPENSKYRIM_LOD_FIXTURE pointing at a real .btr or .bto"]
fn real_lod_mesh_parses_and_converts() {
    let sample = std::env::var_os("OPENSKYRIM_LOD_FIXTURE").map(std::path::PathBuf::from);
    let Some(path) = sample else {
        eprintln!("skipping: set OPENSKYRIM_LOD_FIXTURE to a real .btr or .bto to run this test");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("sample.glb");

    let diagnostics = MeshConverter::inspect_nif(&path).unwrap();
    for block_type in [
        "BSMultiBoundNode",
        "BSMultiBound",
        "BSMultiBoundAABB",
        "BSMultiBoundOBB",
        "BSTriShape",
        "BSSubIndexTriShape",
    ] {
        assert!(
            !diagnostics.fallback_blocks.contains_key(block_type),
            "{block_type} did not parse: {:?}",
            diagnostics.fallback_blocks
        );
    }
    assert!(diagnostics.geometry_block_count > 0);
    assert!(diagnostics.scene_node_count > 0);
    assert!(diagnostics.max_scene_depth >= 2);
    MeshConverter::convert_nif_to_glb(&path, &output).unwrap();
    let document = glb_json(&fs::read(&output).unwrap());
    assert!(!document["meshes"].as_array().unwrap().is_empty());
    println!("{path:?}: {diagnostics:?}");
}
