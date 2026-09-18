//! Round-trip tests that feed generated `dummy-content` fixtures to the real
//! converter parsers.

use converter::{
    archive::ArchiveExtractor,
    script::ScriptConverter,
    texture::{TextureConverter, TextureEncoding, inspect_ktx2},
};
use dummy_content::{Entry, ba2, bsa, dds, pex, rng::Rng};
use std::{fs, path::Path};

fn write(directory: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = directory.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn generated_bsa_archives_extract_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let entries = [
        Entry::new("scripts/one.pex", b"PEX"),
        Entry::new("textures/two.dds", b"DDS "),
        Entry::new("scripts/three.pex", b"PEX3"),
    ];
    let cases = [
        (
            "v105-plain",
            bsa::v105(&entries, bsa::Compression::None).unwrap(),
        ),
        (
            "v105-zlib",
            bsa::v105(&entries, bsa::Compression::Zlib).unwrap(),
        ),
        (
            "v105-lz4",
            bsa::v105(&entries, bsa::Compression::Lz4).unwrap(),
        ),
        (
            "v104-plain",
            bsa::v104(&entries, bsa::Compression::None).unwrap(),
        ),
        (
            "v104-zlib",
            bsa::v104(&entries, bsa::Compression::Zlib).unwrap(),
        ),
    ];

    for (label, bytes) in cases {
        let archive = write(directory.path(), &format!("{label}.bsa"), &bytes);
        let output = directory.path().join(label);
        let extracted = ArchiveExtractor::extract(&archive, &output).unwrap();
        assert_eq!(extracted.len(), 3, "{label}");
        assert_eq!(
            fs::read(output.join("scripts/one.pex")).unwrap(),
            b"PEX",
            "{label}"
        );
        assert_eq!(
            fs::read(output.join("textures/two.dds")).unwrap(),
            b"DDS ",
            "{label}"
        );
        assert_eq!(
            fs::read(output.join("scripts/three.pex")).unwrap(),
            b"PEX3",
            "{label}"
        );
    }
}

#[test]
fn generated_ba2_archives_extract_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let entries = [
        Entry::new("textures/one.dds", b"DDS "),
        Entry::new("meshes/two.nif", b"NIF"),
    ];
    let cases = [
        (
            "gnrl-plain",
            ba2::general(&entries, ba2::Compression::None).unwrap(),
        ),
        (
            "gnrl-zlib",
            ba2::general(&entries, ba2::Compression::Zlib).unwrap(),
        ),
    ];

    for (label, bytes) in cases {
        let archive = write(directory.path(), &format!("{label}.ba2"), &bytes);
        let output = directory.path().join(label);
        let extracted = ArchiveExtractor::extract(&archive, &output).unwrap();
        assert_eq!(extracted.len(), 2, "{label}");
        assert_eq!(
            fs::read(output.join("textures/one.dds")).unwrap(),
            b"DDS ",
            "{label}"
        );
        assert_eq!(
            fs::read(output.join("meshes/two.nif")).unwrap(),
            b"NIF",
            "{label}"
        );
    }

    let pixels = [0xAB; 8];
    let bytes = ba2::dx10(&[ba2::Dx10Texture::new(
        "textures/dx10.dds",
        4,
        4,
        71,
        &pixels,
    )])
    .unwrap();
    let archive = write(directory.path(), "dx10.ba2", &bytes);
    let output = directory.path().join("dx10");
    ArchiveExtractor::extract(&archive, &output).unwrap();
    let dds_bytes = fs::read(output.join("textures/dx10.dds")).unwrap();
    assert_eq!(&dds_bytes[..4], b"DDS ");
    let ktx2 = TextureConverter::convert(&dds_bytes, TextureEncoding::ColorSrgb).unwrap();
    inspect_ktx2(&ktx2, TextureEncoding::ColorSrgb).unwrap();
}

#[test]
fn generated_dds_textures_convert_to_ktx2() {
    let mut rng = Rng::new(42);
    let cases = [
        (
            dds::Spec::new(dds::Format::X8R8G8B8, 8, 8).with_mip_levels(3),
            TextureEncoding::ColorSrgb,
        ),
        (
            dds::Spec::new(dds::Format::Bc1Unorm, 8, 8).with_mip_levels(3),
            TextureEncoding::ColorSrgb,
        ),
        (
            dds::Spec::new(dds::Format::Bc5Unorm, 8, 8).with_mip_levels(3),
            TextureEncoding::NormalLinear,
        ),
        (
            dds::Spec::new(dds::Format::Bc7Unorm, 8, 8).with_mip_levels(3),
            TextureEncoding::ColorSrgb,
        ),
    ];

    for (spec, encoding) in cases {
        let bytes = dds::generate(&spec, &mut rng).unwrap();
        let ktx2 = TextureConverter::convert(&bytes, encoding)
            .unwrap_or_else(|error| panic!("{spec:?}: {error:#}"));
        let metadata = inspect_ktx2(&ktx2, encoding).unwrap();
        assert_eq!(metadata.width, spec.width, "{spec:?}");
        assert_eq!(metadata.height, spec.height, "{spec:?}");
        assert_eq!(metadata.levels, spec.mip_levels, "{spec:?}");
    }

    let cube = dds::generate(
        &dds::Spec::new(dds::Format::Bc1Unorm, 4, 4).as_cubemap(),
        &mut rng,
    )
    .unwrap();
    let metadata = inspect_ktx2(
        &TextureConverter::convert(&cube, TextureEncoding::ColorSrgb).unwrap(),
        TextureEncoding::ColorSrgb,
    )
    .unwrap();
    assert_eq!(metadata.faces, 6);

    let volume = dds::generate(
        &dds::Spec::new(dds::Format::Bc1Unorm, 4, 4).with_depth(4),
        &mut rng,
    )
    .unwrap();
    let metadata = inspect_ktx2(
        &TextureConverter::convert(&volume, TextureEncoding::DataLinear).unwrap(),
        TextureEncoding::DataLinear,
    )
    .unwrap();
    assert_eq!(metadata.depth, 4);
}

#[test]
fn generated_pex_scripts_convert_to_luau() {
    let directory = tempfile::tempdir().unwrap();
    let input = write(
        directory.path(),
        "script.pex",
        &pex::minimal("Generated").unwrap(),
    );
    let output = directory.path().join("scripts/script.luau");
    ScriptConverter::convert_pex_to_luau(&input, &output).unwrap();
    let generated = fs::read_to_string(output).unwrap();
    assert!(generated.contains("Generated"));
    assert!(generated.ends_with("return Script\n"));
}

#[test]
fn generated_nif_static_shape_converts_to_glb() {
    use converter::mesh::MeshConverter;

    let directory = tempfile::tempdir().unwrap();
    let nif_path = directory.path().join("generated.nif");
    let shape = dummy_content::nif::StaticShape {
        name: "GeneratedQuad",
        positions: &[
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ],
        normals: &[[0.0, 0.0, 1.0]; 4],
        uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        indices: &[[0, 1, 2], [0, 2, 3]],
        diffuse: "textures/generated_color.dds",
        normal_texture: "textures/generated_normal.dds",
    };
    fs::write(&nif_path, dummy_content::nif::static_shape(&shape).unwrap()).unwrap();

    let diagnostics = MeshConverter::inspect_nif(&nif_path).unwrap();
    assert_eq!(diagnostics.geometry_block_count, 1);
    assert_eq!(diagnostics.validated_material_shape_count, 1);

    let output = directory.path().join("generated.glb");
    MeshConverter::convert_nif_to_glb(&nif_path, &output).unwrap();
    assert!(output.is_file());
    let bounds = MeshConverter::glb_bounds(&output).unwrap();
    // The exporter bakes the Z-up to Y-up runtime rotation into the mesh, so
    // the quad spans -1..1 on X/Z with a flat Y axis.
    for (axis, value) in bounds.min.iter().enumerate() {
        let expected = if axis == 1 { 0.0 } else { -1.0 };
        assert!(
            (value - expected).abs() < 1.0e-5,
            "min axis {axis}: {value} != {expected}"
        );
    }
    for (axis, value) in bounds.max.iter().enumerate() {
        let expected = if axis == 1 { 0.0 } else { 1.0 };
        assert!(
            (value - expected).abs() < 1.0e-5,
            "max axis {axis}: {value} != {expected}"
        );
    }
}

#[tokio::test]
async fn generated_data_directory_converts_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    dummy_content::layout::prepare_directory(&data, false).unwrap();
    dummy_content::layout::generate(
        &data,
        dummy_content::layout::DEFAULT_SEED,
        dummy_content::layout::Formats::all(),
    )
    .unwrap();

    let output = directory.path().join("modern");
    let config = converter::PipelineConfig::new(&data, &output);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();

    assert!(report.complete);
    assert_eq!(report.skipped, 0);
    assert!(output.join("conversion-manifest.json").is_file());
    for relative in [
        "scripts/generated.luau",
        "scripts/second.luau",
        "textures/generated_color.ktx2",
        "textures/generated_normal.ktx2",
        "textures/generated_color_x8.ktx2",
        "textures/generated_cube.ktx2",
        "textures/generated_volume.ktx2",
    ] {
        assert!(output.join(relative).is_file(), "missing {relative}");
    }

    let config = converter::PipelineConfig::new(&data, &output);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();
    assert!(report.complete);
    assert_eq!(report.converted, 0);
    assert!(report.cache_hits > 0);
}
