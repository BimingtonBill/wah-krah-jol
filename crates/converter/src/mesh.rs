use crate::material::{
    NifMaterialDisposition, NifShapeMaterial, build_nif_material_contract, is_editor_marker_shape,
    publish_gltf_materials,
};
use crate::texture::TextureSemantic;
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use project_wormhole_esm::structs::strings::{SizedString8, SizedString32, StringN};
use project_wormhole_nif::{
    nif_block::NifBlock,
    nif_file::{NifFile, nif_to_model, nif_to_static_model},
    nif_header::{Endianess, NifFileVersion, NifHeader},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub struct MeshConverter;

#[derive(Debug, Clone, Default, Serialize)]
pub struct NifParseDiagnostics {
    pub block_count: usize,
    pub parsed_block_count: usize,
    pub geometry_block_count: usize,
    pub scene_node_count: usize,
    pub max_scene_depth: usize,
    pub block_types: BTreeMap<String, usize>,
    pub fallback_blocks: BTreeMap<String, usize>,
    pub fallback_offsets: BTreeMap<String, Vec<usize>>,
    pub material_shape_count: usize,
    pub validated_material_shape_count: usize,
    pub excluded_material_shape_count: usize,
    pub material_exclusions: BTreeMap<String, usize>,
}

impl MeshConverter {
    pub fn dependency_paths(nif_path: &Path) -> Vec<PathBuf> {
        find_skeleton(nif_path).into_iter().collect()
    }

    pub fn convert_nif_to_glb<P: AsRef<Path>>(nif_path: P, glb_output_path: P) -> Result<()> {
        let nif_path = nif_path.as_ref();
        let (nif, diagnostics, material_contract) = open_nif_resilient(nif_path)?;
        let skeleton = if nif.has_skeleton() {
            let skeleton_path = find_skeleton(nif_path).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "skinned NIF requires a skeleton, but none was found near {}",
                    nif_path.display()
                )
            })?;
            Some(open_nif_resilient(&skeleton_path)?.0)
        } else {
            None
        };
        let primary = catch_unwind(AssertUnwindSafe(|| nif_to_model(&nif, skeleton.as_ref())));
        let (mut model, used_static_fallback) = match primary {
            Ok(Ok(model)) => (model, false),
            primary => {
                let primary_error = match primary {
                    Ok(Err(error)) => format!("NIF model conversion failed: {error}"),
                    Err(_) => format!("NIF model conversion panicked for {}", nif_path.display()),
                    Ok(Ok(_)) => unreachable!(),
                };
                let static_model = catch_unwind(AssertUnwindSafe(|| nif_to_static_model(&nif)))
                    .map_err(|_| color_eyre::eyre::eyre!("static NIF fallback panicked"))?
                    .map_err(|error| color_eyre::eyre::eyre!("static NIF fallback failed: {error}"))
                    .wrap_err(primary_error)?;
                (static_model, true)
            }
        };
        model.scene_root_rotation = Some(shared::coordinates::CREATION_TO_RUNTIME_ROTATION);
        model
            .validate()
            .map_err(|error| color_eyre::eyre::eyre!("invalid converted NIF model: {error}"))?;
        let name = nif_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let output = glb_output_path.as_ref();
        let dropped_marker_meshes = drop_editor_marker_geometry(&mut model);
        if model.static_meshes.is_empty() && model.skeletal_meshes.is_empty() {
            ensure!(
                dropped_marker_meshes > 0
                    || is_deferred_dynamic_mesh(nif_path)
                    || !diagnostics
                        .block_types
                        .keys()
                        .any(|block_type| is_declared_geometry_block(block_type)),
                "NIF declares mesh geometry, but no supported geometry was converted"
            );
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            return write_glb_atomic(output, &empty_scene_glb(&name));
        }
        let mut glb = catch_unwind(AssertUnwindSafe(|| model.to_glb(name.clone())))
            .map_err(|_| color_eyre::eyre::eyre!("NIF GLB export panicked"))?;
        if glb_bounds_from_bytes(&glb).is_err() && !used_static_fallback {
            let mut static_model = nif_to_static_model(&nif)
                .map_err(|error| color_eyre::eyre::eyre!("static NIF fallback failed: {error}"))?;
            static_model.scene_root_rotation =
                Some(shared::coordinates::CREATION_TO_RUNTIME_ROTATION);
            let dropped_static_marker_meshes = drop_editor_marker_geometry(&mut static_model);
            if static_model.static_meshes.is_empty() && static_model.skeletal_meshes.is_empty() {
                ensure!(
                    dropped_static_marker_meshes > 0,
                    "NIF contains no supported mesh geometry"
                );
                if let Some(parent) = output.parent() {
                    fs::create_dir_all(parent)?;
                }
                return write_glb_atomic(output, &empty_scene_glb(&name));
            }
            glb = catch_unwind(AssertUnwindSafe(|| static_model.to_glb(name)))
                .map_err(|_| color_eyre::eyre::eyre!("static NIF GLB export panicked"))?;
            model = static_model;
        }
        let shape_blocks = exported_shape_blocks(&nif, &model, &material_contract)?;
        let exported_material_contract = shape_blocks
            .iter()
            .map(|block| {
                material_contract
                    .iter()
                    .find(|shape| shape.shape_block == *block)
                    .cloned()
                    .ok_or_else(|| {
                        color_eyre::eyre::eyre!(
                            "exported mesh references shape block {block} without a material contract"
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let glb = rewrite_materials_and_texture_uris(
            glb,
            &exported_material_contract,
            &shape_blocks,
            output,
        )?;
        // Only a model with a controller manager can carry a clip, so the extra read and header
        // parse below is paid by the ~1% of NIFs that have one (every door that animates).
        let glb = if diagnostics.block_types.contains_key("NiControllerManager") {
            append_nif_animations(glb, nif_path)
        } else {
            glb
        };
        ensure!(
            glb.len() >= 12 && &glb[..4] == b"glTF",
            "NIF exporter produced an invalid GLB header"
        );
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        write_glb_atomic(output, &glb)
    }

    pub fn inspect_nif(path: &Path) -> Result<NifParseDiagnostics> {
        open_nif_resilient(path).map(|(_, diagnostics, _)| diagnostics)
    }

    /// Extracts the validated per-shape NIF material contract without
    /// publishing glTF/PBR decisions that belong to the next pipeline stage.
    pub fn inspect_nif_materials(path: &Path) -> Result<Vec<NifShapeMaterial>> {
        open_nif_resilient(path).map(|(_, _, contract)| contract)
    }

    /// Reads the accessor bounds written to a GLB and applies the complete glTF
    /// node hierarchy. This avoids loading vertex buffers merely to build the
    /// runtime spatial index.
    pub fn glb_bounds(path: &Path) -> Result<shared::Bounds3> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        glb_bounds_from_bytes(&bytes)
            .wrap_err_with(|| format!("failed to extract bounds from {}", path.display()))
    }

    /// Returns every external image URI referenced by a GLB. Embedded images
    /// have no URI and are intentionally omitted.
    pub fn glb_texture_uris(path: &Path) -> Result<Vec<String>> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let document = glb_json_from_bytes(&bytes)
            .wrap_err_with(|| format!("failed to inspect textures in {}", path.display()))?;
        let mut uris = document
            .get("images")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|image| image.get("uri").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        uris.sort_by_key(|uri| uri.to_ascii_lowercase());
        uris.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        Ok(uris)
    }

    /// Returns material-aware external texture dependencies. Base-color
    /// textures are mandatory; auxiliary maps remain explicitly optional until
    /// the shader contract requires them.
    pub fn glb_texture_dependencies(path: &Path) -> Result<Vec<TextureDependency>> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let document = glb_json_from_bytes(&bytes)
            .wrap_err_with(|| format!("failed to inspect textures in {}", path.display()))?;
        Ok(texture_dependencies(&document))
    }
}

fn is_declared_geometry_block(block_type: &str) -> bool {
    matches!(
        block_type,
        "BSTriShape"
            | "BSDynamicTriShape"
            | "BSSubIndexTriShape"
            | "BSMeshLODTriShape"
            | "BSLODTriShape"
            | "NiTriShape"
            | "NiTriStrips"
    )
}

fn is_deferred_dynamic_mesh(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    ["/meshes/actors/", "/meshes/magic/", "/meshes/effects/"]
        .iter()
        .any(|category| normalized.contains(category))
}

fn empty_scene_glb(name: &str) -> Vec<u8> {
    let mut json = serde_json::to_vec(&serde_json::json!({
        "asset": { "version": "2.0", "generator": "OpenSkyrim converter" },
        "scene": 0,
        "scenes": [{ "name": name, "nodes": [] }]
    }))
    .expect("static empty-scene glTF JSON is serializable");
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total_length = 20 + json.len();
    let mut glb = Vec::with_capacity(total_length);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total_length as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb
}

fn glb_json_from_bytes(bytes: &[u8]) -> Result<serde_json::Value> {
    ensure!(
        bytes.len() >= 20 && &bytes[..4] == b"glTF",
        "invalid GLB container"
    );
    ensure!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) == 2,
        "unsupported GLB version"
    );
    let declared_length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    ensure!(declared_length == bytes.len(), "GLB length is inconsistent");
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    ensure!(&bytes[16..20] == b"JSON", "GLB JSON chunk is missing");
    let json_end = 20usize
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON range overflow"))?;
    let json = bytes
        .get(20..json_end)
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?;
    serde_json::from_slice(json).wrap_err("invalid glTF JSON")
}

/// Removes editor-only marker geometry (`EditorMarker` shapes) from a converted
/// model, returning how many meshes were dropped.
///
/// Bethesda's editor writes these shapes into models that are otherwise
/// legitimate (a Dwemer lever, a partition door, a spike trap, an effect), so the
/// engine cannot skip the file the way it skips whole `markers/` and `effects/`
/// models. No shipping renderer draws them: exporting one paints a flat
/// untextured shape over the world. A marker node keeps its place in the
/// hierarchy, because its children carry the real shapes' transforms, but it no
/// longer references a mesh.
///
/// Skinned models are left alone: their meshes are matched to source shapes
/// positionally, so removing one would mis-associate every material after it.
/// The material contract still excludes marker shapes, which the engine treats as
/// non-rendering.
fn drop_editor_marker_geometry(model: &mut project_wormhole_nif::model::all::Model) -> usize {
    let mut remap = Vec::with_capacity(model.static_meshes.len());
    let mut kept = 0usize;
    for mesh in &model.static_meshes {
        if is_editor_marker_shape(mesh.name.as_deref()) {
            remap.push(None);
        } else {
            remap.push(Some(kept));
            kept += 1;
        }
    }
    let dropped = model.static_meshes.len() - kept;
    if dropped == 0 {
        return 0;
    }
    let mut index = 0usize;
    model.static_meshes.retain(|_| {
        let keep = remap[index].is_some();
        index += 1;
        keep
    });
    for node in &mut model.static_nodes {
        node.mesh = node
            .mesh
            .and_then(|mesh| remap.get(mesh).copied().flatten());
    }
    dropped
}

fn exported_shape_blocks(
    nif: &NifFile,
    model: &project_wormhole_nif::model::all::Model,
    contract: &[NifShapeMaterial],
) -> Result<Vec<u32>> {
    let mut blocks = Vec::with_capacity(model.static_meshes.len() + model.skeletal_meshes.len());
    for mesh_index in 0..model.static_meshes.len() {
        let mut matches = model
            .static_nodes
            .iter()
            .filter(|node| node.mesh == Some(mesh_index));
        let node = matches.next().ok_or_else(|| {
            color_eyre::eyre::eyre!("static mesh {mesh_index} has no source shape block")
        })?;
        ensure!(
            matches.next().is_none(),
            "static mesh {mesh_index} is associated with multiple source shape blocks"
        );
        blocks.push(node.block_index);
    }
    if !model.skeletal_meshes.is_empty() {
        let mut skeletal_blocks = Vec::new();
        let predicates: [fn(&NifBlock) -> bool; 3] = [
            |block: &NifBlock| matches!(block, NifBlock::BSTriShape(_)),
            |block: &NifBlock| matches!(block, NifBlock::BSDynamicTriShape(_)),
            |block: &NifBlock| matches!(block, NifBlock::BSSubIndexTriShape(_)),
        ];
        for predicate in predicates {
            skeletal_blocks.extend(
                nif.blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, block)| predicate(block))
                    .map(|(index, _)| u32::try_from(index))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
        ensure!(
            skeletal_blocks.len() == model.skeletal_meshes.len(),
            "skeletal mesh/material association is incomplete: {} meshes, {} source shapes",
            model.skeletal_meshes.len(),
            skeletal_blocks.len()
        );
        blocks.extend(skeletal_blocks);
    }
    ensure!(
        blocks.len() <= contract.len(),
        "mesh/material contract is incomplete: {} exported meshes, {} source shapes",
        blocks.len(),
        contract.len()
    );
    Ok(blocks)
}

fn open_nif_resilient(
    path: &Path,
) -> Result<(NifFile, NifParseDiagnostics, Vec<NifShapeMaterial>)> {
    let bytes = fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let (mut data, header) = parse_skyrim_header(&bytes, path)?;
    let block_count = usize::try_from(header.block_count)
        .wrap_err_with(|| format!("NIF block count is out of range in {}", path.display()))?;
    ensure!(
        header.block_type_index.len() == block_count
            && header.block_size_index.len() == block_count,
        "NIF header block tables are inconsistent in {}",
        path.display()
    );
    let mut diagnostics = NifParseDiagnostics {
        block_count,
        ..Default::default()
    };
    let mut blocks = Vec::with_capacity(block_count);
    for index in 0..block_count {
        let size = usize::try_from(header.block_size_index[index])
            .wrap_err("NIF block size is out of range")?;
        ensure!(
            data.len() >= size,
            "NIF block {index} is truncated in {}",
            path.display()
        );
        let (raw, remaining) = data.split_at(size);
        data = remaining;
        let block_type = header
            .get_block_type(index)
            .map_err(|_| {
                color_eyre::eyre::eyre!(
                    "NIF block {index} has an invalid type index in {}",
                    path.display()
                )
            })?
            .to_owned();
        *diagnostics
            .block_types
            .entry(block_type.clone())
            .or_default() += 1;
        let parsed = catch_unwind(AssertUnwindSafe(|| {
            NifBlock::parse(raw, block_type.clone())
        }));
        let block = match parsed {
            Ok(Ok((_, NifBlock::Unhandled))) => {
                diagnostics
                    .fallback_offsets
                    .entry(block_type.clone())
                    .or_default()
                    .push(0);
                *diagnostics.fallback_blocks.entry(block_type).or_default() += 1;
                NifBlock::Unhandled
            }
            Ok(Ok((_, block))) => {
                diagnostics.parsed_block_count += 1;
                if matches!(
                    &block,
                    NifBlock::BSTriShape(_)
                        | NifBlock::BSDynamicTriShape(_)
                        | NifBlock::BSSubIndexTriShape(_)
                        | NifBlock::BSLODTriShape(_)
                        | NifBlock::NiTriShape(_)
                ) {
                    diagnostics.geometry_block_count += 1;
                }
                block
            }
            Ok(Err(error)) => {
                let offset = match error {
                    nom_derive::nom::Err::Error(error) | nom_derive::nom::Err::Failure(error) => {
                        raw.len().saturating_sub(error.input.len())
                    }
                    nom_derive::nom::Err::Incomplete(_) => raw.len(),
                };
                diagnostics
                    .fallback_offsets
                    .entry(block_type.clone())
                    .or_default()
                    .push(offset);
                *diagnostics.fallback_blocks.entry(block_type).or_default() += 1;
                NifBlock::Unhandled
            }
            Err(_) => {
                diagnostics
                    .fallback_offsets
                    .entry(block_type.clone())
                    .or_default()
                    .push(usize::MAX);
                *diagnostics.fallback_blocks.entry(block_type).or_default() += 1;
                NifBlock::Unhandled
            }
        };
        blocks.push(block);
    }
    diagnostics.scene_node_count = blocks
        .iter()
        .filter(|block| {
            matches!(
                block,
                NifBlock::NiNode(_)
                    | NifBlock::BSFadeNode(_)
                    | NifBlock::BSTriShape(_)
                    | NifBlock::BSDynamicTriShape(_)
                    | NifBlock::BSSubIndexTriShape(_)
                    | NifBlock::BSLODTriShape(_)
                    | NifBlock::NiTriShape(_)
            )
        })
        .count();
    diagnostics.max_scene_depth = nif_scene_depth(&blocks);
    let nif = NifFile { header, blocks };
    let material_contract = build_nif_material_contract(&nif, path)?;
    diagnostics.material_shape_count = material_contract.len();
    for shape in &material_contract {
        match &shape.disposition {
            NifMaterialDisposition::Validated { .. } => {
                diagnostics.validated_material_shape_count += 1;
            }
            NifMaterialDisposition::Excluded { reason } => {
                diagnostics.excluded_material_shape_count += 1;
                *diagnostics
                    .material_exclusions
                    .entry(reason.clone())
                    .or_default() += 1;
            }
        }
    }
    Ok((nif, diagnostics, material_contract))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextureDependency {
    pub uri: String,
    pub semantic: TextureSemantic,
    pub required: bool,
}

/// Appends a NIF's controller sequences (`Open`, `Close`, ...) to the converted GLB.
///
/// Animated models - load doors, gates, secret doors - carry their clips in `NiControllerManager`
/// blocks the vendored exporter ignores. Nothing about them is fatal: a model whose animation
/// cannot be read still exports, with a warning naming what was skipped.
fn append_nif_animations(glb: Vec<u8>, nif_path: &Path) -> Vec<u8> {
    match crate::nif_animation::append_nif_animations(&glb, nif_path) {
        Ok((glb, warnings)) => {
            for warning in warnings {
                eprintln!("{}: {warning}", nif_path.display());
            }
            glb
        }
        Err(error) => {
            eprintln!("{}: skipping animations: {error:#}", nif_path.display());
            glb
        }
    }
}

fn write_glb_atomic(output: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let extension = output
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("glb");
    let temporary = output.with_extension(format!("{extension}.{}.partial", std::process::id()));
    let backup = output.with_extension(format!("{extension}.{}.backup", std::process::id()));
    ensure!(
        !temporary.exists() && !backup.exists(),
        "stale temporary GLB exists for {}",
        output.display()
    );
    let mut file = fs::File::create(&temporary)
        .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if output.exists() {
        fs::rename(output, &backup)
            .wrap_err_with(|| format!("failed to preserve {}", output.display()))?;
    }
    if let Err(error) = fs::rename(&temporary, output) {
        if backup.exists() {
            let _ = fs::rename(&backup, output);
        }
        return Err(error).wrap_err_with(|| format!("failed to publish {}", output.display()));
    }
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

fn nif_scene_depth(blocks: &[NifBlock]) -> usize {
    fn depth(index: usize, blocks: &[NifBlock], visiting: &mut Vec<usize>) -> usize {
        if visiting.contains(&index) {
            return 0;
        }
        let children = match blocks.get(index) {
            Some(NifBlock::NiNode(node) | NifBlock::BSFadeNode(node)) => &node.children,
            _ => return usize::from(index < blocks.len()),
        };
        visiting.push(index);
        let child_depth = children
            .iter()
            .filter_map(|child| usize::try_from(*child).ok())
            .map(|child| depth(child, blocks, visiting))
            .max()
            .unwrap_or(0);
        visiting.pop();
        1 + child_depth
    }

    (0..blocks.len())
        .map(|index| depth(index, blocks, &mut Vec::new()))
        .max()
        .unwrap_or(0)
}

pub(crate) fn parse_skyrim_header<'a>(
    bytes: &'a [u8],
    path: &Path,
) -> Result<(&'a [u8], NifHeader)> {
    let mut cursor = NifCursor::new(bytes, path);
    let file_desc = cursor.line()?;
    ensure!(
        file_desc.starts_with("Gamebryo File Format"),
        "unsupported NIF signature in {}",
        path.display()
    );
    let nif_version = cursor.u32()?;
    let endian_type = cursor.u8()?;
    ensure!(
        endian_type == 1,
        "big-endian NIF is not supported in {}",
        path.display()
    );
    let user_version = cursor.u32()?;
    let block_count = cursor.u32()?;
    ensure!(
        block_count <= 1_000_000,
        "NIF block count exceeds the safety limit in {}",
        path.display()
    );
    let bethesda_version = cursor.u32()?;
    let author = cursor.sized_string8_optional()?;
    let process_script = cursor.sized_string8_optional()?;
    let export_script = cursor.sized_string8_optional()?;
    let block_type_count = usize::from(cursor.u16()?);
    let mut block_types = Vec::with_capacity(block_type_count);
    for _ in 0..block_type_count {
        block_types.push(SizedString32(cursor.sized_string32()?));
    }
    let block_count_usize = usize::try_from(block_count).wrap_err("NIF block count overflow")?;
    let mut block_type_index = Vec::with_capacity(block_count_usize);
    for _ in 0..block_count_usize {
        block_type_index.push(cursor.u16()?);
    }
    let mut block_size_index = Vec::with_capacity(block_count_usize);
    for _ in 0..block_count_usize {
        block_size_index.push(cursor.u32()?);
    }
    let string_count = cursor.u32()?;
    ensure!(
        string_count <= 1_000_000,
        "NIF string count exceeds the safety limit in {}",
        path.display()
    );
    let string_max_size = cursor.u32()?;
    let mut strings = Vec::with_capacity(usize::try_from(string_count)?);
    for _ in 0..string_count {
        strings.push(SizedString32(cursor.sized_string32()?));
    }
    let group_count = cursor.u32()?;
    ensure!(
        group_count <= 1_000_000,
        "NIF group count exceeds the safety limit in {}",
        path.display()
    );
    let mut groups = Vec::with_capacity(usize::try_from(group_count)?);
    for _ in 0..group_count {
        groups.push(cursor.u32()?);
    }
    let remaining = &bytes[cursor.position..];
    Ok((
        remaining,
        NifHeader {
            file_desc: StringN { value: file_desc },
            nif_version: NifFileVersion(nif_version),
            endian_type: Endianess::Little,
            user_version,
            block_count,
            bethesda_version,
            author,
            process_script,
            export_script,
            max_filepath: None,
            block_types,
            block_type_index,
            block_size_index,
            string_count,
            string_max_size,
            strings,
            groups,
        },
    ))
}

struct NifCursor<'a> {
    bytes: &'a [u8],
    position: usize,
    path: &'a Path,
}

impl<'a> NifCursor<'a> {
    fn new(bytes: &'a [u8], path: &'a Path) -> Self {
        Self {
            bytes,
            position: 0,
            path,
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| color_eyre::eyre::eyre!("NIF offset overflow"))?;
        ensure!(
            end <= self.bytes.len(),
            "truncated NIF header at byte {} in {}",
            self.position,
            self.path.display()
        );
        let result = &self.bytes[self.position..end];
        self.position = end;
        Ok(result)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn line(&mut self) -> Result<String> {
        let Some(length) = self.bytes[self.position..]
            .iter()
            .position(|byte| *byte == b'\n')
        else {
            color_eyre::eyre::bail!(
                "NIF header line is not terminated in {}",
                self.path.display()
            );
        };
        let value = String::from_utf8_lossy(self.take(length)?).into_owned();
        self.take(1)?;
        Ok(value)
    }

    fn sized_string8_optional(&mut self) -> Result<Option<SizedString8>> {
        let length = usize::from(self.u8()?);
        let value = String::from_utf8_lossy(self.take(length)?)
            .trim_end_matches('\0')
            .to_owned();
        Ok((!value.is_empty()).then_some(SizedString8(value)))
    }

    fn sized_string32(&mut self) -> Result<String> {
        let length = usize::try_from(self.u32()?).wrap_err("NIF string length overflow")?;
        ensure!(
            length <= 16 * 1024 * 1024,
            "NIF string exceeds the safety limit in {}",
            self.path.display()
        );
        Ok(String::from_utf8_lossy(self.take(length)?)
            .trim_end_matches('\0')
            .to_owned())
    }
}

fn glb_bounds_from_bytes(glb: &[u8]) -> Result<shared::Bounds3> {
    ensure!(
        glb.len() >= 20 && &glb[..4] == b"glTF",
        "invalid GLB container"
    );
    let json_length = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
    ensure!(&glb[16..20] == b"JSON", "GLB JSON chunk is missing");
    let document: serde_json::Value = serde_json::from_slice(
        glb.get(20..20 + json_length)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?,
    )?;
    let nodes = document
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let roots = scene_roots(&document, nodes);
    let mut bounds = BoundsAccumulator::default();
    for root in roots {
        visit_node(&document, nodes, root, Mat4::IDENTITY, 0, &mut bounds)?;
    }
    bounds.finish()
}

fn scene_roots(document: &serde_json::Value, nodes: &[serde_json::Value]) -> Vec<usize> {
    if let Some(scenes) = document.get("scenes").and_then(serde_json::Value::as_array) {
        let scene_index = document
            .get("scene")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        if let Some(scene_nodes) = scenes
            .get(scene_index)
            .and_then(|scene| scene.get("nodes"))
            .and_then(serde_json::Value::as_array)
        {
            return scene_nodes
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|index| index as usize)
                .collect();
        }
    }
    let mut children = vec![false; nodes.len()];
    for node in nodes {
        if let Some(indices) = node.get("children").and_then(serde_json::Value::as_array) {
            for index in indices.iter().filter_map(serde_json::Value::as_u64) {
                if let Some(child) = children.get_mut(index as usize) {
                    *child = true;
                }
            }
        }
    }
    children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| (!child).then_some(index))
        .collect()
}

fn visit_node(
    document: &serde_json::Value,
    nodes: &[serde_json::Value],
    index: usize,
    parent: Mat4,
    depth: usize,
    bounds: &mut BoundsAccumulator,
) -> Result<()> {
    ensure!(depth <= nodes.len(), "cyclic glTF node hierarchy");
    let node = nodes
        .get(index)
        .ok_or_else(|| color_eyre::eyre::eyre!("glTF node {index} is out of range"))?;
    let transform = parent.mul(Mat4::from_node(node));
    if let Some(mesh_index) = node.get("mesh").and_then(serde_json::Value::as_u64) {
        accumulate_mesh(document, mesh_index as usize, transform, bounds)?;
    }
    if let Some(children) = node.get("children").and_then(serde_json::Value::as_array) {
        for child in children.iter().filter_map(serde_json::Value::as_u64) {
            visit_node(
                document,
                nodes,
                child as usize,
                transform,
                depth + 1,
                bounds,
            )?;
        }
    }
    Ok(())
}

fn accumulate_mesh(
    document: &serde_json::Value,
    mesh_index: usize,
    transform: Mat4,
    output: &mut BoundsAccumulator,
) -> Result<()> {
    let meshes = document
        .get("meshes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB has no meshes"))?;
    let accessors = document
        .get("accessors")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB has no accessors"))?;
    let primitives = meshes
        .get(mesh_index)
        .and_then(|mesh| mesh.get("primitives"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("mesh {mesh_index} has no primitives"))?;
    for primitive in primitives {
        let Some(accessor_index) = primitive
            .pointer("/attributes/POSITION")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        let accessor = accessors
            .get(accessor_index as usize)
            .ok_or_else(|| color_eyre::eyre::eyre!("POSITION accessor is out of range"))?;
        let min = json_vec3(accessor.get("min"))?;
        let max = json_vec3(accessor.get("max"))?;
        for x in [min[0], max[0]] {
            for y in [min[1], max[1]] {
                for z in [min[2], max[2]] {
                    output.include(transform.transform([x, y, z]));
                }
            }
        }
    }
    Ok(())
}

fn json_vec3(value: Option<&serde_json::Value>) -> Result<[f64; 3]> {
    let values = value
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("POSITION accessor has no min/max"))?;
    ensure!(values.len() >= 3, "POSITION accessor min/max is not a vec3");
    Ok([
        values[0]
            .as_f64()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid bound"))?,
        values[1]
            .as_f64()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid bound"))?,
        values[2]
            .as_f64()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid bound"))?,
    ])
}

#[derive(Clone, Copy)]
struct Mat4([f64; 16]);

impl Mat4 {
    const IDENTITY: Self = Self([
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]);

    fn from_node(node: &serde_json::Value) -> Self {
        if let Some(matrix) = node.get("matrix").and_then(serde_json::Value::as_array)
            && matrix.len() == 16
        {
            let mut output = [0.0; 16];
            for (target, source) in output.iter_mut().zip(matrix) {
                *target = source.as_f64().unwrap_or(0.0);
            }
            return Self(output);
        }
        let t = array_or(node.get("translation"), [0.0, 0.0, 0.0]);
        let r = array_or(node.get("rotation"), [0.0, 0.0, 0.0, 1.0]);
        let s = array_or(node.get("scale"), [1.0, 1.0, 1.0]);
        let [x, y, z, w] = r;
        let mut matrix = [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + z * w),
            2.0 * (x * z - y * w),
            0.0,
            2.0 * (x * y - z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + x * w),
            0.0,
            2.0 * (x * z + y * w),
            2.0 * (y * z - x * w),
            1.0 - 2.0 * (x * x + y * y),
            0.0,
            t[0],
            t[1],
            t[2],
            1.0,
        ];
        for row in 0..4 {
            matrix[row] *= s[0];
            matrix[4 + row] *= s[1];
            matrix[8 + row] *= s[2];
        }
        Self(matrix)
    }

    fn mul(self, rhs: Self) -> Self {
        let mut output = [0.0; 16];
        for column in 0..4 {
            for row in 0..4 {
                output[column * 4 + row] = (0..4)
                    .map(|axis| self.0[axis * 4 + row] * rhs.0[column * 4 + axis])
                    .sum();
            }
        }
        Self(output)
    }

    fn transform(self, point: [f64; 3]) -> [f64; 3] {
        [
            self.0[0] * point[0] + self.0[4] * point[1] + self.0[8] * point[2] + self.0[12],
            self.0[1] * point[0] + self.0[5] * point[1] + self.0[9] * point[2] + self.0[13],
            self.0[2] * point[0] + self.0[6] * point[1] + self.0[10] * point[2] + self.0[14],
        ]
    }
}

fn array_or<const N: usize>(value: Option<&serde_json::Value>, fallback: [f64; N]) -> [f64; N] {
    let Some(values) = value.and_then(serde_json::Value::as_array) else {
        return fallback;
    };
    std::array::from_fn(|index| {
        values
            .get(index)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(fallback[index])
    })
}

#[derive(Default)]
struct BoundsAccumulator {
    min: [f64; 3],
    max: [f64; 3],
    populated: bool,
}

impl BoundsAccumulator {
    fn include(&mut self, point: [f64; 3]) {
        if !self.populated {
            self.min = point;
            self.max = point;
            self.populated = true;
        } else {
            for (axis, value) in point.into_iter().enumerate() {
                self.min[axis] = self.min[axis].min(value);
                self.max[axis] = self.max[axis].max(value);
            }
        }
    }

    fn finish(self) -> Result<shared::Bounds3> {
        ensure!(self.populated, "GLB contains no bounded POSITION accessor");
        let bounds = shared::Bounds3 {
            min: self.min.map(|value| value as f32),
            max: self.max.map(|value| value as f32),
        };
        ensure!(
            bounds.is_finite_and_ordered(),
            "GLB contains invalid bounds"
        );
        Ok(bounds)
    }
}

fn rewrite_materials_and_texture_uris(
    glb: Vec<u8>,
    material_contract: &[NifShapeMaterial],
    shape_blocks: &[u32],
    glb_output_path: &Path,
) -> Result<Vec<u8>> {
    ensure!(
        glb.len() >= 20 && &glb[..4] == b"glTF",
        "invalid GLB container"
    );
    let json_length = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
    ensure!(&glb[16..20] == b"JSON", "GLB JSON chunk is missing");
    let json_end = 20usize
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON range overflow"))?;
    let json_bytes = glb
        .get(20..json_end)
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?;
    let mut document: serde_json::Value =
        serde_json::from_slice(json_bytes).wrap_err("NIF exporter produced invalid glTF JSON")?;
    publish_gltf_materials(
        &mut document,
        material_contract,
        shape_blocks,
        glb_output_path,
    )?;
    let mut json = serde_json::to_vec(&document)?;
    while json.len() % 4 != 0 {
        json.push(b' ');
    }
    let suffix = glb
        .get(json_end..)
        .ok_or_else(|| color_eyre::eyre::eyre!("invalid GLB suffix"))?;
    let total_length = 20usize
        .checked_add(json.len())
        .and_then(|length| length.checked_add(suffix.len()))
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB size overflow"))?;
    let mut output = Vec::with_capacity(total_length);
    output.extend_from_slice(&glb[..8]);
    let total_length = u32::try_from(total_length).wrap_err("GLB exceeds 4 GiB")?;
    let json_length = u32::try_from(json.len()).wrap_err("GLB JSON exceeds 4 GiB")?;
    output.extend_from_slice(&total_length.to_le_bytes());
    output.extend_from_slice(&json_length.to_le_bytes());
    output.extend_from_slice(b"JSON");
    output.extend_from_slice(&json);
    output.extend_from_slice(suffix);
    Ok(output)
}

fn texture_dependencies(document: &serde_json::Value) -> Vec<TextureDependency> {
    let mut dependencies = BTreeMap::<(String, TextureSemantic), TextureDependency>::new();
    if let Some(materials) = document
        .get("materials")
        .and_then(serde_json::Value::as_array)
    {
        for material in materials {
            for (pointer, semantic, required) in [
                (
                    "/pbrMetallicRoughness/baseColorTexture/index",
                    TextureSemantic::BaseColor,
                    true,
                ),
                ("/normalTexture/index", TextureSemantic::Normal, false),
                ("/emissiveTexture/index", TextureSemantic::Emissive, false),
                (
                    "/pbrMetallicRoughness/metallicRoughnessTexture/index",
                    TextureSemantic::MetallicRoughness,
                    false,
                ),
                ("/occlusionTexture/index", TextureSemantic::Occlusion, false),
                (
                    "/extensions/KHR_materials_pbrSpecularGlossiness/diffuseTexture/index",
                    TextureSemantic::BaseColor,
                    true,
                ),
                (
                    "/extensions/KHR_materials_pbrSpecularGlossiness/specularGlossinessTexture/index",
                    TextureSemantic::SpecularGlossiness,
                    false,
                ),
                (
                    "/extensions/KHR_materials_specular/specularColorTexture/index",
                    TextureSemantic::SpecularGlossiness,
                    false,
                ),
            ] {
                if let Some(index) = material
                    .pointer(pointer)
                    .and_then(serde_json::Value::as_u64)
                    && let Some(uri) = texture_uri(document, index as usize)
                {
                    dependencies.insert(
                        (uri.to_ascii_lowercase(), semantic),
                        TextureDependency {
                            uri: uri.to_owned(),
                            semantic,
                            required,
                        },
                    );
                }
            }
            if let Some(slots) = material
                .pointer("/extensions/OPEN_SKYRIM_material/textureSlots")
                .and_then(serde_json::Value::as_array)
            {
                for slot in slots {
                    let Some(index) = slot.get("texture").and_then(serde_json::Value::as_u64)
                    else {
                        continue;
                    };
                    let Some(uri) = texture_uri(document, index as usize) else {
                        continue;
                    };
                    let semantic = match slot.get("semantic").and_then(serde_json::Value::as_str) {
                        Some("height") => TextureSemantic::Height,
                        Some("detail") => TextureSemantic::Detail,
                        Some("environment_cube") => TextureSemantic::EnvironmentCube,
                        Some("environment_mask") => TextureSemantic::EnvironmentMask,
                        Some("inner_layer") => TextureSemantic::InnerLayer,
                        Some("greyscale") => TextureSemantic::Greyscale,
                        _ => TextureSemantic::Unclassified,
                    };
                    let required = slot
                        .get("required")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    dependencies.insert(
                        (uri.to_ascii_lowercase(), semantic),
                        TextureDependency {
                            uri: uri.to_owned(),
                            semantic,
                            required,
                        },
                    );
                }
            }
        }
    }
    if let Some(images) = document.get("images").and_then(serde_json::Value::as_array) {
        for uri in images
            .iter()
            .filter_map(|image| image.get("uri").and_then(serde_json::Value::as_str))
        {
            if !dependencies
                .keys()
                .any(|(known, _)| known.eq_ignore_ascii_case(uri))
            {
                dependencies.insert(
                    (uri.to_ascii_lowercase(), TextureSemantic::Unclassified),
                    TextureDependency {
                        uri: uri.to_owned(),
                        semantic: TextureSemantic::Unclassified,
                        required: false,
                    },
                );
            }
        }
    }
    dependencies.into_values().collect()
}

fn texture_uri(document: &serde_json::Value, texture_index: usize) -> Option<&str> {
    let image_index = document
        .get("textures")?
        .get(texture_index)?
        .get("source")?
        .as_u64()? as usize;
    document
        .get("images")?
        .get(image_index)?
        .get("uri")?
        .as_str()
}

fn find_skeleton(nif_path: &Path) -> Option<PathBuf> {
    let parent = nif_path.parent()?;
    let mut candidates = vec![
        parent.join("skeleton.nif"),
        parent.join("skeleton_female.nif"),
        parent.join("character assets").join("skeleton.nif"),
    ];
    if let Some(grandparent) = parent.parent() {
        candidates.extend([
            grandparent.join("skeleton.nif"),
            grandparent.join("character assets").join("skeleton.nif"),
        ]);
    }
    if let Some((actors_root, actor_name)) = actor_root(nif_path) {
        let actor_dir = actors_root.join(actor_name);
        candidates.extend([
            actor_dir.join("character assets").join("skeleton.nif"),
            actor_dir
                .join("character assets female")
                .join("skeleton_female.nif"),
        ]);
        if let Some(found) = WalkDir::new(&actor_dir)
            .max_depth(4)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.into_path())
            .find(|path| {
                path.is_file()
                    && path.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .to_ascii_lowercase()
                            .starts_with("skeleton")
                            && path.extension().is_some_and(|ext| {
                                ext.to_string_lossy().eq_ignore_ascii_case("nif")
                            })
                    })
            })
        {
            candidates.push(found);
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn actor_root(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let components: Vec<_> = path.components().collect();
    let index = components.iter().position(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("actors")
    })?;
    let actor = components.get(index + 1)?.as_os_str();
    let mut root = PathBuf::new();
    for component in &components[..=index] {
        root.push(component.as_os_str());
    }
    Some((root, PathBuf::from(actor)))
}

/// Unit tests for the NIF-to-glTF path.
///
/// A few of them read real files rather than a generated fixture. Those are
/// `#[ignore]`d and opt-in, and they skip - printing why - when the environment
/// does not name the data, so CI never needs proprietary data (ADR-0002):
///
/// - `OPENSKYRIM_NIF_FIXTURE`, `OPENSKYRIM_STATIC_NIF_FIXTURE`: one NIF file
///   from a local Skyrim installation.
/// - `OPENSKYRIM_CONVERTED_DIR`: a converted asset tree - its `skyrim_world.db`
///   and the NIFs it extracted under `vfs/meshes`. The `real_*` tests below read
///   the models the converted set was built from.
#[cfg(test)]
mod tests {
    use super::*;
    use project_wormhole_nif::model::all::{Model, StaticMesh, StaticSceneNode};

    #[test]
    fn rejects_invalid_nif_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("bad.nif");
        let output = dir.path().join("bad.glb");
        fs::write(&input, b"not a nif").unwrap();
        assert!(MeshConverter::convert_nif_to_glb(&input, &output).is_err());
        assert!(!output.exists());
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_NIF_FIXTURE with a locally installed Skyrim NIF"]
    fn converts_installed_non_renderable_nif_to_empty_scene() {
        let path = std::env::var_os("OPENSKYRIM_NIF_FIXTURE")
            .map(PathBuf::from)
            .expect("set OPENSKYRIM_NIF_FIXTURE to a Skyrim NIF");
        let diagnostics = MeshConverter::inspect_nif(&path).unwrap();
        assert_eq!(diagnostics.geometry_block_count, 0);
        assert!(
            !diagnostics
                .block_types
                .keys()
                .any(|block_type| is_declared_geometry_block(block_type))
        );
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("non-renderable.glb");
        MeshConverter::convert_nif_to_glb(&path, &output).unwrap();
        let document = glb_json_from_bytes(&fs::read(output).unwrap()).unwrap();
        assert_eq!(document["scenes"][0]["nodes"], serde_json::json!([]));
        assert!(document.get("meshes").is_none());
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_STATIC_NIF_FIXTURE with a locally installed Skyrim NIF"]
    fn static_fallback_converts_installed_nif_fixture() {
        let path = std::env::var_os("OPENSKYRIM_STATIC_NIF_FIXTURE")
            .map(PathBuf::from)
            .expect("set OPENSKYRIM_STATIC_NIF_FIXTURE to a Skyrim NIF");
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("static-fallback.glb");
        MeshConverter::convert_nif_to_glb(&path, &output).unwrap();
        MeshConverter::glb_bounds(&output).unwrap();
    }

    #[test]
    fn derives_actor_root_case_insensitively() {
        let (root, actor) = actor_root(Path::new(
            "vfs/Meshes/Actors/Dragon/character assets/dragon.nif",
        ))
        .unwrap();
        assert_eq!(root, PathBuf::from("vfs/Meshes/Actors"));
        assert_eq!(actor, PathBuf::from("Dragon"));
    }

    #[test]
    fn defers_only_dynamic_runtime_geometry() {
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/Meshes/Actors/Character/FaceGenData/FaceGeom/Skyrim.esm/00045CB1.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/actors/character/character assets/hair/elf/female/hair03.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/magic/lightningbolt01.nif"
        )));
        assert!(is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/effects/fxemptycontroller.nif"
        )));
        assert!(!is_deferred_dynamic_mesh(Path::new(
            "vfs/meshes/architecture/whiterun/wrwall.nif"
        )));
    }

    #[test]
    fn rewrites_glb_material_chunk_without_corrupting_the_container() {
        let mut json = br#"{"asset":{"version":"2.0"},"meshes":[{"primitives":[{}]}]}"#.to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        let contract = [NifShapeMaterial {
            shape_block: 7,
            shape_name: Some("excluded".to_owned()),
            shader_property_block: None,
            alpha_property_block: None,
            disposition: NifMaterialDisposition::Excluded {
                reason: "fixture".to_owned(),
            },
        }];
        let rewritten =
            rewrite_materials_and_texture_uris(glb, &contract, &[7], Path::new("meshes/a.glb"))
                .unwrap();
        assert_eq!(
            u32::from_le_bytes(rewritten[8..12].try_into().unwrap()) as usize,
            rewritten.len()
        );
        let length = u32::from_le_bytes(rewritten[12..16].try_into().unwrap()) as usize;
        let document: serde_json::Value =
            serde_json::from_slice(&rewritten[20..20 + length]).unwrap();
        assert_eq!(document["materials"][0]["alphaMode"], "MASK");
        assert_eq!(document["meshes"][0]["primitives"][0]["material"], 0);
        assert_eq!(
            document["meshes"][0]["primitives"][0]["extras"]["openSkyrim"]["shapeBlock"],
            7
        );
    }

    #[test]
    fn extracts_bounds_with_node_transform() {
        let mut json = br#"{
            "asset":{"version":"2.0"},
            "scene":0,
            "scenes":[{"nodes":[0]}],
            "nodes":[{"mesh":0,"translation":[10,20,30],"scale":[2,3,4]}],
            "meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}],
            "accessors":[{"min":[-1,-2,-3],"max":[1,2,3]}]
        }"#
        .to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        let bounds = glb_bounds_from_bytes(&glb).unwrap();
        assert_eq!(bounds.min, [8.0, 14.0, 18.0]);
        assert_eq!(bounds.max, [12.0, 26.0, 42.0]);
    }

    #[test]
    fn extracts_bounds_through_rotated_non_uniform_hierarchy() {
        let half_sqrt = std::f64::consts::FRAC_1_SQRT_2;
        let mut json = format!(
            r#"{{
                "asset":{{"version":"2.0"}},
                "scene":0,
                "scenes":[{{"nodes":[0]}}],
                "nodes":[
                    {{"children":[1],"translation":[10,0,0],"rotation":[0,0,{half_sqrt},{half_sqrt}],"scale":[2,1,1]}},
                    {{"mesh":0,"translation":[1,2,0],"rotation":[{half_sqrt},0,0,{half_sqrt}],"scale":[1,3,2]}}
                ],
                "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}}],
                "accessors":[{{"min":[-1,-1,-1],"max":[1,1,1]}}]
            }}"#
        )
        .into_bytes();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);

        let bounds = glb_bounds_from_bytes(&glb).unwrap();
        for (actual, expected) in bounds.min.into_iter().zip([6.0, 0.0, -3.0]) {
            assert!((actual - expected).abs() < 1.0e-5);
        }
        for (actual, expected) in bounds.max.into_iter().zip([10.0, 4.0, 3.0]) {
            assert!((actual - expected).abs() < 1.0e-5);
        }
    }

    #[test]
    fn lists_external_glb_textures_deterministically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mesh.glb");
        let mut json = br#"{
            "asset":{"version":"2.0"},
            "images":[
                {"uri":"../textures/B.ktx2"},
                {"bufferView":0,"mimeType":"image/png"},
                {"uri":"../textures/a.ktx2"},
                {"uri":"../textures/A.ktx2"}
            ]
        }"#
        .to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let total = 20 + json.len();
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        fs::write(&path, glb).unwrap();

        assert_eq!(
            MeshConverter::glb_texture_uris(&path).unwrap(),
            vec!["../textures/a.ktx2", "../textures/B.ktx2"]
        );
    }

    #[test]
    fn classifies_required_and_optional_texture_dependencies() {
        let document = serde_json::json!({
            "images": [
                {"uri": "../textures/diffuse.ktx2"},
                {"uri": "../textures/normal.ktx2"}
            ],
            "textures": [{"source": 0}, {"source": 1}],
            "materials": [{
                "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}},
                "normalTexture": {"index": 1}
            }]
        });
        let dependencies = texture_dependencies(&document);
        assert_eq!(dependencies.len(), 2);
        assert!(dependencies.iter().any(|dependency| {
            dependency.semantic == TextureSemantic::BaseColor && dependency.required
        }));
        assert!(dependencies.iter().any(|dependency| {
            dependency.semantic == TextureSemantic::Normal && !dependency.required
        }));
    }

    fn static_mesh(name: &str) -> StaticMesh {
        StaticMesh {
            name: Some(name.to_owned()),
            positions: Vec::new(),
            normals: Vec::new(),
            uvs: Vec::new(),
            triangles: Vec::new(),
            colors: Vec::new(),
        }
    }

    fn fixture_model(meshes: &[&str], nodes: &[(u32, Option<usize>)]) -> Model {
        Model {
            name: Some("fixture".to_owned()),
            static_meshes: meshes.iter().map(|name| static_mesh(name)).collect(),
            static_nodes: nodes
                .iter()
                .map(|(block, mesh)| StaticSceneNode {
                    block_index: *block,
                    name: Some(format!("shape-{block}")),
                    translation: Default::default(),
                    rotation: Default::default(),
                    scale: 1.0,
                    children: Vec::new(),
                    mesh: *mesh,
                })
                .collect(),
            skeletal_meshes: Vec::new(),
            materials: Vec::new(),
            material_indices: Vec::new(),
            scene_root_rotation: None,
        }
    }

    #[test]
    fn drops_editor_marker_geometry_and_renumbers_the_survivors() {
        // `DwePtnDoor01`'s shape list, reduced: two door leaves plus the marker
        // the editor writes into the file.
        let mut model = fixture_model(
            &["DoorLeft:12", "EditorMarker", "DoorRight:12"],
            &[(23, Some(0)), (40, Some(1)), (35, Some(2))],
        );

        assert_eq!(drop_editor_marker_geometry(&mut model), 1);
        assert_eq!(
            model
                .static_meshes
                .iter()
                .map(|mesh| mesh.name.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("DoorLeft:12".to_owned()),
                Some("DoorRight:12".to_owned())
            ]
        );
        assert_eq!(model.static_nodes[0].mesh, Some(0));
        assert_eq!(
            model.static_nodes[1].mesh, None,
            "the marker node keeps its transform for its children but exports no mesh"
        );
        assert_eq!(
            model.static_nodes[2].mesh,
            Some(1),
            "surviving meshes are renumbered so glTF node.mesh stays valid"
        );
    }

    #[test]
    fn drops_a_model_whose_only_geometry_was_an_editor_marker() {
        // `clutter/dummyitems/*.nif` and `cameras/*.nif` are editor placeholders
        // whose only shape is the marker, so the converter's existing empty scene
        // rule applies to them once the marker is gone.
        let mut model = fixture_model(&["EditorMarker"], &[(7, Some(0))]);
        assert_eq!(drop_editor_marker_geometry(&mut model), 1);
        assert!(model.static_meshes.is_empty());
        assert_eq!(model.static_nodes[0].mesh, None);
    }

    #[test]
    fn keeps_geometry_whose_name_merely_contains_marker() {
        // `MarkerTeleport` and `WayShrinePourMarker` are real models that the
        // engine filters by path; a substring rule would delete real geometry.
        let mut model = fixture_model(
            &["MarkerTeleport:0", "WayShrinePourMarker"],
            &[(3, Some(0)), (4, Some(1))],
        );
        assert_eq!(drop_editor_marker_geometry(&mut model), 0);
        assert_eq!(model.static_meshes.len(), 2);
        assert_eq!(model.static_nodes[0].mesh, Some(0));
        assert_eq!(model.static_nodes[1].mesh, Some(1));
    }

    /// The converted asset tree named by `OPENSKYRIM_CONVERTED_DIR`, or `None`
    /// with the reason printed.
    ///
    /// Game data is never committed, so the tests that read a converted set are
    /// `#[ignore]`d and skipped, not failed, when the environment does not name
    /// one (ADR-0002).
    fn converted_assets() -> Option<PathBuf> {
        let Some(root) = std::env::var_os("OPENSKYRIM_CONVERTED_DIR").map(PathBuf::from) else {
            eprintln!("skipping: set OPENSKYRIM_CONVERTED_DIR to a converted asset tree");
            return None;
        };
        if !root.is_dir() {
            eprintln!("skipping: {} is not a directory", root.display());
            return None;
        }
        Some(root)
    }

    /// The extracted Skyrim NIFs the converted set was built from: the tree's
    /// `vfs/meshes`. `None` when there is no converted set to read.
    fn converted_nif(relative: &str) -> Option<PathBuf> {
        let root = converted_assets()?.join("vfs").join("meshes");
        if !root.is_dir() {
            eprintln!("skipping: {} is not a directory", root.display());
            return None;
        }
        Some(root.join(relative))
    }

    fn convert_fixture_to_document(relative: &str) -> Option<serde_json::Value> {
        let path = converted_nif(relative)?;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("converted.glb");
        MeshConverter::convert_nif_to_glb(path.as_path(), output.as_path()).unwrap();
        Some(glb_json_from_bytes(&fs::read(&output).unwrap()).unwrap())
    }

    fn material_named<'a>(document: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        document["materials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|material| material["name"] == name)
            .unwrap_or_else(|| panic!("no material named {name}"))
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_ice_pile_publishes_opaque_materials() {
        // The vertex-alpha class (`docs/research/transparent-and-misplaced-meshes.md`):
        // `IcePileM02:1` carries `SLSF1_VERTEX_ALPHA` (shader flags 0x82400309) and
        // no `NiAlphaProperty`, so the base texture's alpha channel (snow01.dds,
        // mean 165/255 - a shader mask, not opacity) was published as opacity and
        // the pile rendered ghostly.
        let Some(document) = convert_fixture_to_document("landscape/ice/icepilem02.nif") else {
            return;
        };
        assert_eq!(
            material_named(&document, "IcePileM02:1")["alphaMode"],
            "OPAQUE"
        );
        // The control: a shape whose property enables the alpha test stays a
        // cutout at its own threshold (26/255), which the reader already got right.
        let cutout = material_named(&document, "IcePileM02:6");
        assert_eq!(cutout["alphaMode"], "MASK");
        let cutoff = cutout["alphaCutoff"].as_f64().unwrap();
        assert!((cutoff - 26.0 / 255.0).abs() < 1e-6, "cutoff {cutoff}");
        assert!(
            document["materials"]
                .as_array()
                .unwrap()
                .iter()
                .all(|material| material["alphaMode"] != "BLEND"),
            "no shape of this model has a blend-enabled NiAlphaProperty"
        );
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_glow_card_keeps_its_additive_blend_factors() {
        // The additive-blend class (`docs/research/transparent-and-misplaced-meshes.md`):
        // the torch's `GlowAddMesh` is additive (`NiAlphaProperty` flags 0x100D:
        // SRC_ALPHA / ONE), which glTF `BLEND` alone renders as an ordinary grey
        // veil. Its sibling `HeatRefraction:0` is the vertex-alpha case in the
        // same file: SLSF1_VERTEX_ALPHA and no property, so it is opaque rather
        // than a blend.
        let Some(document) = convert_fixture_to_document("weapons/torch/torch.nif") else {
            return;
        };
        let glow = material_named(&document, "GlowAddMesh");
        assert_eq!(glow["alphaMode"], "BLEND");
        let extension = &glow["extensions"]["OPEN_SKYRIM_material"];
        assert_eq!(extension["blendSource"], "SRC_ALPHA");
        assert_eq!(extension["blendDestination"], "ONE");
        assert_eq!(
            material_named(&document, "HeatRefraction:0")["alphaMode"],
            "OPAQUE"
        );
        // The control: this shape's property tests, so it is a cutout with no
        // blend factors to publish.
        let torch = material_named(&document, "Torch:0");
        assert_eq!(torch["alphaMode"], "MASK");
        assert!(
            torch["extensions"]["OPEN_SKYRIM_material"]
                .get("blendSource")
                .is_none()
        );
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_partition_door_exports_without_its_editor_marker() {
        // The editor-marker class (`docs/research/transparent-and-misplaced-meshes.md`):
        // `DwePtnDoor01` is a real object whose fifth shape is the editor marker,
        // exported as a flat untextured door-sized shape.
        let Some(document) =
            convert_fixture_to_document("dungeons/dwemer/partitions/dweptndoor01.nif")
        else {
            return;
        };
        let names = document["meshes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|mesh| mesh["name"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 4, "exported meshes: {names:?}");
        for expected in ["DoorLeft:12", "DoorLeft:13", "DoorRight:12", "DoorRight:13"] {
            assert!(names.iter().any(|name| name == expected), "{names:?}");
        }
        assert!(
            !names.iter().any(|name| name.contains("EditorMarker")),
            "{names:?}"
        );
        assert!(
            !document["materials"]
                .as_array()
                .unwrap()
                .iter()
                .any(|material| material["name"] == "EditorMarker"),
            "the marker shape must not keep a material either"
        );
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_marker_only_model_converts_to_an_empty_scene() {
        // `dummybook01` is a display placeholder whose only shape is the marker:
        // the converter's existing empty-scene rule covers it, exactly as it does
        // for NIFs that carry no renderable geometry at all.
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("dummybook01.glb");
        let Some(path) = converted_nif("clutter/dummyitems/dummybook01.nif") else {
            return;
        };
        MeshConverter::convert_nif_to_glb(path.as_path(), output.as_path()).unwrap();
        let document = glb_json_from_bytes(&fs::read(&output).unwrap()).unwrap();
        assert!(document.get("meshes").is_none());
        assert_eq!(document["scenes"][0]["nodes"], serde_json::json!([]));
    }

    // ------------------------------------------------------------------------------------
    // Animated doors (`crates/converter/src/nif_animation.rs`)
    // ------------------------------------------------------------------------------------

    /// Converts one of the extracted NIFs, returning its GLB JSON and the container's bytes.
    /// `None` when there is no converted set to read.
    fn convert_fixture_glb(relative: &str) -> Option<(serde_json::Value, Vec<u8>)> {
        let path = converted_nif(relative)?;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("converted.glb");
        MeshConverter::convert_nif_to_glb(path.as_path(), output.as_path()).unwrap();
        let bytes = fs::read(&output).unwrap();
        Some((glb_json_from_bytes(&bytes).unwrap(), bytes))
    }

    fn animation_names(document: &serde_json::Value) -> Vec<String> {
        document
            .get("animations")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .map(|animation| animation["name"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn animation_named<'a>(document: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        document["animations"]
            .as_array()
            .unwrap_or_else(|| panic!("the model has no animations at all"))
            .iter()
            .find(|animation| animation["name"] == name)
            .unwrap_or_else(|| {
                panic!(
                    "no animation named {name} in {:?}",
                    animation_names(document)
                )
            })
    }

    /// The GLB's binary chunk, where the animation accessors live.
    fn glb_binary_chunk(glb: &[u8]) -> Vec<u8> {
        let json_length = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
        let start = 20 + json_length;
        assert!(glb.len() >= start + 8, "the GLB has no binary chunk");
        let length = u32::from_le_bytes(glb[start..start + 4].try_into().unwrap()) as usize;
        glb[start + 8..start + 8 + length].to_vec()
    }

    /// One element per accessor entry, each with its components.
    fn accessor_values(document: &serde_json::Value, bin: &[u8], accessor: usize) -> Vec<Vec<f32>> {
        let accessor = &document["accessors"][accessor];
        let view = &document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
        let offset = view["byteOffset"].as_u64().unwrap() as usize;
        let count = accessor["count"].as_u64().unwrap() as usize;
        let components = match accessor["type"].as_str().unwrap() {
            "SCALAR" => 1,
            "VEC3" => 3,
            "VEC4" => 4,
            other => panic!("unexpected accessor type {other}"),
        };
        (0..count)
            .map(|element| {
                (0..components)
                    .map(|component| {
                        let start = offset + (element * components + component) * 4;
                        f32::from_le_bytes(bin[start..start + 4].try_into().unwrap())
                    })
                    .collect()
            })
            .collect()
    }

    fn channel_samples(
        document: &serde_json::Value,
        bin: &[u8],
        animation: &serde_json::Value,
        channel: &serde_json::Value,
    ) -> (Vec<f32>, Vec<Vec<f32>>) {
        let sampler = &animation["samplers"][channel["sampler"].as_u64().unwrap() as usize];
        let times = accessor_values(document, bin, sampler["input"].as_u64().unwrap() as usize)
            .into_iter()
            .map(|sample| sample[0])
            .collect();
        let values = accessor_values(document, bin, sampler["output"].as_u64().unwrap() as usize);
        (times, values)
    }

    /// The angle, in radians, between the first sample of a rotation channel and its furthest
    /// sample.
    fn rotation_swing(values: &[Vec<f32>]) -> f32 {
        let first = &values[0];
        values
            .iter()
            .map(|sample| {
                let dot: f32 = first
                    .iter()
                    .zip(sample)
                    .map(|(left, right)| left * right)
                    .sum();
                // q and -q are the same rotation, so the angle is 2*acos(|dot|).
                2.0 * dot.abs().min(1.0).acos()
            })
            .fold(0.0f32, f32::max)
    }

    /// Every channel of a clip meets its target node's exported rest transform at one end.
    ///
    /// The transform curves of a door model are absolute - the interpolator's own rest fields are
    /// the `-FLT_MAX` sentinel - and the node's rest pose *is* the door's closed pose. `Open`
    /// therefore begins there and `Close` ends there; a clip that missed the rest pose at its
    /// closed end would pop the door when it is played.
    fn assert_clip_meets_rest_pose(
        document: &serde_json::Value,
        bin: &[u8],
        animation: &serde_json::Value,
        at_start: bool,
    ) {
        let channels = animation["channels"].as_array().unwrap();
        assert!(
            !channels.is_empty(),
            "a clip with no channels is not written"
        );
        let name = animation["name"].as_str().unwrap_or_default();
        for channel in channels {
            let node = &document["nodes"][channel["target"]["node"].as_u64().unwrap() as usize];
            let path = channel["target"]["path"].as_str().unwrap();
            let (times, values) = channel_samples(document, bin, animation, channel);
            let index = if at_start { 0 } else { values.len() - 1 };
            if at_start {
                assert!(
                    times[index].abs() < 1.0e-5,
                    "{path} of '{name}' starts at {} instead of 0",
                    times[index]
                );
            } else {
                assert!(
                    times[index] > 1.0e-3,
                    "{path} of '{name}' ends at {}",
                    times[index]
                );
            }
            let sample = &values[index];
            let rest = |property: &str, identity: Vec<f32>| -> Vec<f32> {
                node.get(property)
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .map(|value| value.as_f64().unwrap() as f32)
                            .collect()
                    })
                    .unwrap_or(identity)
            };
            let rest: Vec<f32> = match path {
                "rotation" => rest("rotation", vec![0.0, 0.0, 0.0, 1.0]),
                "translation" => rest("translation", vec![0.0; sample.len()]),
                _ => vec![1.0; sample.len()],
            };
            if path == "rotation" {
                // `q` and `-q` are the same rotation.
                let dot: f32 = sample.iter().zip(&rest).map(|(a, b)| a * b).sum();
                assert!(
                    dot.abs() > 0.9999,
                    "the rest rotation {rest:?} of node '{}' is not the {} pose {sample:?} of \
                     '{name}'",
                    node["name"],
                    if at_start { "t = 0" } else { "final" }
                );
            } else {
                for (actual, expected) in sample.iter().zip(&rest) {
                    assert!(
                        (actual - expected).abs() <= 1.0e-3 * (1.0 + expected.abs()),
                        "the rest {path} {rest:?} of node '{}' is not the {} pose {sample:?} of \
                         '{name}'",
                        node["name"],
                        if at_start { "t = 0" } else { "final" }
                    );
                }
            }
        }
    }

    /// The model a static record points at, read from the converted database
    /// (read-only). `None` when there is no converted set to read.
    fn static_model(editor_id: &str) -> Option<String> {
        let root = converted_assets()?;
        let database = root.join("skyrim_world.db");
        if !database.is_file() {
            eprintln!("skipping: no {} to read", database.display());
            return None;
        }
        let connection = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap_or_else(|error| panic!("failed to open {}: {error}", database.display()));
        let model: String = connection
            .query_row(
                "SELECT model_path FROM statics WHERE editor_id = ?1 AND model_path IS NOT NULL \
                 LIMIT 1",
                [editor_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("no statics row named {editor_id}: {error}"));
        Some(
            model
                .replace('\\', "/")
                .trim_start_matches("meshes/")
                .to_ascii_lowercase(),
        )
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_dwemer_small_door_exports_open_and_close_animations() {
        let Some((document, glb)) =
            convert_fixture_glb("dungeons/dwemer/door/dwemersmalldoorload01.nif")
        else {
            return;
        };
        let bin = glb_binary_chunk(&glb);
        // The Z-up to Y-up basis change is a rotation on the scene's root node, not baked into
        // the vertices or into the animated nodes - which is why the curves below are written to
        // the glTF exactly as the NIF stores them, with no coordinate conversion.
        let root = document["scenes"][0]["nodes"][0].as_u64().unwrap() as usize;
        let rotation = document["nodes"][root]["rotation"]
            .as_array()
            .unwrap_or_else(|| {
                panic!(
                    "the scene root carries no rotation: {:?}",
                    document["nodes"][root]
                )
            });
        for (actual, expected) in rotation
            .iter()
            .zip(shared::coordinates::CREATION_TO_RUNTIME_ROTATION)
        {
            assert!(
                (actual.as_f64().unwrap() as f32 - expected).abs() < 1.0e-6,
                "the scene root carries {rotation:?}, not the Creation-to-runtime basis"
            );
        }
        let names = animation_names(&document);
        for expected in ["Open", "Close"] {
            assert!(names.iter().any(|name| name == expected), "{names:?}");
        }
        for expected in ["Open", "Close"] {
            let animation = animation_named(&document, expected);
            let channels = animation["channels"].as_array().unwrap();
            assert!(
                channels.len() >= 2,
                "'{expected}' animates {} nodes, expected the door's leaves",
                channels.len()
            );
            let mut swings = Vec::new();
            for channel in channels {
                let node = channel["target"]["node"].as_u64().unwrap() as usize;
                assert!(
                    document["nodes"][node]["name"].is_string(),
                    "a channel targets node {node}, which is not an exported node"
                );
                let path = channel["target"]["path"].as_str().unwrap();
                let (times, values) = channel_samples(&document, &bin, animation, channel);
                assert!(times.len() >= 2, "a channel needs at least two samples");
                assert_eq!(
                    path, "rotation",
                    "the small door's translation and scale groups are empty, so '{expected}' \
                     animates nothing but rotation"
                );
                swings.push(rotation_swing(&values).to_degrees());
            }
            swings.sort_by(|left, right| left.partial_cmp(right).unwrap());
            // Read off the model's own curves: `Open` separates the leaves by 8.59 and 9.45
            // degrees, and `Close` brings them back from 8.52 and 9.16.
            assert!(
                (8.0..=10.0).contains(&swings[0]) && (8.0..=10.0).contains(&swings[1]),
                "'{expected}' swings {swings:?} degrees"
            );
            if expected == "Open" {
                assert!((swings[0] - 8.59).abs() < 0.2, "{swings:?}");
                assert!((swings[1] - 9.45).abs() < 0.2, "{swings:?}");
            }
            assert_clip_meets_rest_pose(&document, &bin, animation, expected == "Open");
        }
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_route_door_swings_a_few_degrees_and_never_translates() {
        // The Alftand -> Blackreach route's three real doors are all `DweDoorLarge01Load`. It
        // looked as though their visible motion lived in translation keys rather than the 5-9
        // degree rotation; the model's own blocks say otherwise: `Open` swings one leaf 5.36
        // degrees and the other 8.74 degrees, and there is no translation key anywhere in the file.
        let Some(model) = static_model("DweDoorLarge01Load") else {
            return;
        };
        let Some((document, glb)) = convert_fixture_glb(&model) else {
            return;
        };
        let bin = glb_binary_chunk(&glb);
        let animation = animation_named(&document, "Open");
        let mut swings = Vec::new();
        for channel in animation["channels"].as_array().unwrap() {
            let path = channel["target"]["path"].as_str().unwrap();
            let (_, values) = channel_samples(&document, &bin, animation, channel);
            if path == "rotation" {
                swings.push(rotation_swing(&values));
            } else {
                panic!("the route door's 'Open' has a {path} channel");
            }
        }
        swings.sort_by(|left, right| left.partial_cmp(right).unwrap());
        assert_eq!(swings.len(), 2, "two leaves swing: {swings:?}");
        let degrees = |radians: f32| radians.to_degrees();
        assert!(
            (degrees(swings[0]) - 5.36).abs() < 0.2,
            "the smaller leaf swings {:.2} degrees, expected 5.36",
            degrees(swings[0])
        );
        assert!(
            (degrees(swings[1]) - 8.74).abs() < 0.2,
            "the larger leaf swings {:.2} degrees, expected 8.74",
            degrees(swings[1])
        );
        assert!(animation_names(&document).contains(&"Close".to_owned()));
        assert_clip_meets_rest_pose(&document, &bin, animation, true);
        assert_clip_meets_rest_pose(&document, &bin, animation_named(&document, "Close"), false);
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_sliding_door_exports_a_translation_channel() {
        // The counter-example to the dwemer doors: `riftenrwthievesguilddoor01.nif` has *no*
        // rotation keys at all - its `Open` is a 95 key translation curve that slides the leaf
        // roughly 243 units along its local X and 84 along Y over 3.13 seconds. A decoder that
        // read the rotation type unconditionally would misread this block and drop the clip.
        let Some((document, glb)) =
            convert_fixture_glb("dungeons/riften/thievesguild/riftenrwthievesguilddoor01.nif")
        else {
            return;
        };
        let bin = glb_binary_chunk(&glb);
        let animation = animation_named(&document, "Open");
        let channels = animation["channels"].as_array().unwrap();
        assert_eq!(
            channels.len(),
            2,
            "one leaf slides and keeps its scale: {channels:?}"
        );
        let paths = channels
            .iter()
            .map(|channel| channel["target"]["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(
            !paths.contains(&"rotation"),
            "the model has no rotation keys, so it needs no rotation channel: {paths:?}"
        );
        let translation = channels
            .iter()
            .find(|channel| channel["target"]["path"] == "translation")
            .expect("the door slides");
        for channel in channels {
            assert_eq!(channel["target"]["node"], translation["target"]["node"]);
        }
        let (times, values) = channel_samples(&document, &bin, animation, translation);
        assert_eq!(times.len(), 95, "one sample per authored key");
        assert!(
            (times[times.len() - 1] - 3.1333).abs() < 1.0e-3,
            "the clip slides for {:.4} s",
            times[times.len() - 1]
        );
        let first = &values[0];
        let travel = (0..3)
            .map(|axis| {
                values
                    .iter()
                    .map(|value| (value[axis] - first[axis]).abs())
                    .fold(0.0f32, f32::max)
            })
            .collect::<Vec<_>>();
        assert!(travel[0] > 200.0, "travel {travel:?}");
        assert!(travel[1] > 50.0, "travel {travel:?}");
        assert_clip_meets_rest_pose(&document, &bin, animation, true);
        // `Close` slides the leaf back: it *ends* at the rest pose, not at its start.
        assert_clip_meets_rest_pose(&document, &bin, animation_named(&document, "Close"), false);
    }

    #[test]
    #[ignore = "requires the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_nordic_door_swings_ninety_degrees() {
        // `FarmhouseAnimDoor01` is the calibration model of the layout: its `Door01` swings
        // -1.65806 rad (-95 degrees) over a second, which only a decoded euler curve reproduces.
        let Some((document, glb)) =
            convert_fixture_glb("architecture/farmhouse/farmhouseanimdoor01.nif")
        else {
            return;
        };
        let bin = glb_binary_chunk(&glb);
        let animation = animation_named(&document, "Open");
        let channels = animation["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 1, "one leaf: {channels:?}");
        let (_, values) = channel_samples(&document, &bin, animation, &channels[0]);
        let degrees = rotation_swing(&values).to_degrees();
        assert!(
            (90.0..=135.0).contains(&degrees),
            "the nordic door swings {degrees:.2} degrees, expected about 95"
        );
        assert_clip_meets_rest_pose(&document, &bin, animation, true);
    }
}
