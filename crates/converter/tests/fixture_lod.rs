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
    nif::{LodShape, StaticShape, object_lod, object_lod_segmented, static_shape, terrain_lod},
    rng::Rng,
};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path};

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

/// Six unshared triangles laid out edge to edge along X, horizontal in
/// Creation space like [`lod_quad`]. Used with an explicit segment table
/// (`SEGMENTED_COUNTS`) whose three non-empty cells hold 1, 2 and 3 triangles
/// respectively, so the split primitives' index counts and coverage can be
/// checked exactly: no two triangles share a vertex, so each vertex index
/// identifies one triangle.
fn segmented_lod_shape<'a>(diffuse: &'a str, normal: &'a str) -> LodShape<'a> {
    LodShape {
        name: "GeneratedLodSegments",
        positions: &[
            [0.0, 0.0, 0.0],
            [4.0, 0.0, 0.0],
            [4.0, 4.0, 0.0],
            [10.0, 0.0, 0.0],
            [14.0, 0.0, 0.0],
            [14.0, 4.0, 0.0],
            [20.0, 0.0, 0.0],
            [24.0, 0.0, 0.0],
            [24.0, 4.0, 0.0],
            [30.0, 0.0, 0.0],
            [34.0, 0.0, 0.0],
            [34.0, 4.0, 0.0],
            [40.0, 0.0, 0.0],
            [44.0, 0.0, 0.0],
            [44.0, 4.0, 0.0],
            [50.0, 0.0, 0.0],
            [54.0, 0.0, 0.0],
            [54.0, 4.0, 0.0],
        ],
        normals: &[[0.0, 0.0, 1.0]; 18],
        uvs: &[[0.0, 0.0]; 18],
        indices: &[
            [0, 1, 2],
            [3, 4, 5],
            [6, 7, 8],
            [9, 10, 11],
            [12, 13, 14],
            [15, 16, 17],
        ],
        colors: &[[128, 128, 128, 255]; 18],
        diffuse,
        normal_texture: normal,
    }
}

/// Cell `i`'s triangle count for [`segmented_lod_shape`]: cell 0 gets the
/// shape's first triangle, cell 5 the next two, cell 15 the last three; every
/// other cell (dropped after 15, since it is the last) is empty.
const SEGMENTED_COUNTS: [u32; 16] = [1, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3];

/// The `u16` index values of a glTF accessor, read from the GLB binary chunk.
fn accessor_indices(bytes: &[u8], document: &Value, accessor_index: u64) -> Vec<u16> {
    let accessor = &document["accessors"][accessor_index as usize];
    assert_eq!(
        accessor["componentType"], 5123,
        "expected a u16 index accessor"
    );
    let view = &document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
    let count = accessor["count"].as_u64().unwrap() as usize;
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let binary = 20 + json_length.next_multiple_of(4) + 8;
    let start = binary
        + view["byteOffset"].as_u64().unwrap_or(0) as usize
        + accessor["byteOffset"].as_u64().unwrap_or(0) as usize;
    let mut values = Vec::with_capacity(count);
    for chunk in bytes[start..start + count * 2].as_chunks::<2>().0 {
        values.push(u16::from_le_bytes(*chunk));
    }
    values
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
    let primitives = document["meshes"][0]["primitives"].as_array().unwrap();
    assert_eq!(
        primitives.len(),
        1,
        "a shape with a single segment must still export one primitive: {primitives:#?}"
    );
    let primitive = &primitives[0];
    assert_lod_vertex_colours(&bytes, &document, primitive);
    assert!(primitive["attributes"].get("TEXCOORD_0").is_some());
    assert!(
        primitive.get("extras").is_none(),
        "a single-segment shape must not carry lodSegment extras: {primitive}"
    );
}

#[test]
fn fixture_object_lod_segments_split_into_one_primitive_per_cell() {
    let directory = tempfile::tempdir().unwrap();
    let shape = segmented_lod_shape(DIFFUSE, NORMAL);

    let segmented_source = directory.path().join("segmented.4.0.0.bto");
    fs::write(
        &segmented_source,
        object_lod_segmented(&shape, &SEGMENTED_COUNTS).unwrap(),
    )
    .unwrap();
    let segmented_output = directory.path().join("segmented.glb");
    MeshConverter::convert_nif_to_glb(&segmented_source, &segmented_output).unwrap();
    let bytes = fs::read(&segmented_output).unwrap();
    let document = glb_json(&bytes);

    let primitives = document["meshes"][0]["primitives"].as_array().unwrap();
    assert_eq!(
        primitives.len(),
        3,
        "3 non-empty segments must split into 3 primitives: {primitives:#?}"
    );

    // Segment 0 is the shape's triangle 0 (vertices 0..3), segment 5 the next
    // two triangles (vertices 3..9), segment 15 the last three (9..18): every
    // triangle in `segmented_lod_shape` has its own, unshared vertices, so the
    // vertex index ranges pin down exactly which triangles each primitive
    // carries.
    let expected = [(0u64, 3u32, 0u16..3u16), (5, 6, 3..9), (15, 9, 9..18)];
    let mut covered = BTreeSet::new();
    for (primitive, (expected_segment, expected_index_count, expected_vertices)) in
        primitives.iter().zip(expected)
    {
        let lod_segment = primitive
            .pointer("/extras/openSkyrim/lodSegment")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("primitive has no lodSegment extra: {primitive}"));
        assert_eq!(lod_segment, expected_segment);
        // TRIANGLES is glTF's default primitive mode, so the exporter never
        // serializes a "mode" key for it; a split primitive must not add one
        // that would override that default with something else.
        assert!(
            primitive.get("mode").is_none(),
            "split primitive got an unexpected explicit mode: {primitive}"
        );
        assert!(
            primitive["attributes"]["POSITION"]
                == document["meshes"][0]["primitives"][0]["attributes"]["POSITION"],
            "split primitives must share the shape's vertex accessors: {primitive}"
        );

        let accessor_index = primitive["indices"].as_u64().unwrap();
        let mut indices = accessor_indices(&bytes, &document, accessor_index);
        assert_eq!(
            indices.len() as u32,
            expected_index_count,
            "segment {expected_segment}'s index count is not 3x its triangle count"
        );
        indices.sort_unstable();
        assert_eq!(
            indices,
            expected_vertices.collect::<Vec<u16>>(),
            "segment {expected_segment} covers the wrong triangles"
        );
        covered.extend(indices);
    }
    assert_eq!(
        covered,
        (0u16..18u16).collect::<BTreeSet<_>>(),
        "the split primitives must together cover every triangle exactly once"
    );

    // The split must not change the block's recorded bounds.
    let unsplit_source = directory.path().join("unsplit.4.0.0.bto");
    fs::write(&unsplit_source, object_lod(&shape).unwrap()).unwrap();
    let unsplit_output = directory.path().join("unsplit.glb");
    MeshConverter::convert_nif_to_glb(&unsplit_source, &unsplit_output).unwrap();

    let segmented_bounds = MeshConverter::glb_bounds(&segmented_output).unwrap();
    let unsplit_bounds = MeshConverter::glb_bounds(&unsplit_output).unwrap();
    assert_eq!(segmented_bounds.min, unsplit_bounds.min);
    assert_eq!(segmented_bounds.max, unsplit_bounds.max);
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
