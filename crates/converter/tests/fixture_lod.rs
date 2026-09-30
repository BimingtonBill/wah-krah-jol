//! Fixture tests for the distant-LOD mesh containers (`.btr`/`.bto`).
//!
//! Skyrim ships its distant terrain and object LOD as NIFs in a different
//! container: a `BSMultiBoundNode` (with a `BSMultiBound` → `BSMultiBoundAABB`
//! pair) holding shape blocks that may carry per-vertex tint. These tests drive
//! the generated fixtures through the real conversion path, so no game data is
//! required.

use converter::{AssetPipeline, PipelineConfig, mesh::MeshConverter};
use dummy_content::{
    dds,
    nif::{LodShape, StaticShape, object_lod, static_shape, terrain_lod},
    rng::Rng,
};
use serde_json::Value;
use std::{fs, path::Path};

const DIFFUSE: &str = "textures/terrain/generated/generated.4.0.0.dds";
const NORMAL: &str = "textures/terrain/generated/generated.4.0.0_n.dds";

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
