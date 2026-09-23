//! Distant-LOD inventory: grid headers, block meshes and tree billboards.
//!
//! Skyrim drives the world beyond the streamed cells from three file-driven
//! systems per worldspace, all of which the converter already extracts:
//!
//! * `lodsettings/<worldspace>.lod` — a 16-byte grid header
//!   (`i16 origin_x, i16 origin_y, i32 cells_per_side, i32 min_level,
//!   i32 max_level`; Tamriel is `-96, -96, 256, 4, 32`).
//! * `meshes/terrain/<worldspace>/<worldspace>.<level>.<x>.<y>.btr` and the
//!   object blocks under `objects/` — the block meshes the mesh pipeline
//!   converts to GLB. `<x>,<y>` are the block's south-west cell coordinates
//!   and are always multiples of `<level>`.
//! * `meshes/terrain/<worldspace>/trees/<worldspace>.lst` (one billboard type
//!   per tree) and `<worldspace>.4.<x>.<y>.btt` (the placed instances).
//!
//! [`record_lod_inventory`] reads all three into the `lod_grid`, `lod_block`,
//! `lod_tree_type` and `lod_tree_instance` tables and publishes one billboard
//! GLB per tree type. Block availability comes from the files that exist,
//! never from the `.lod` extents: Tamriel's header claims 256 cells per side
//! while the shipped blocks cover 192, which is why residency cannot be
//! derived from the header.
//!
//! The layouts are measured rather than quoted (`docs/research/tree-lod-layout.md`
//! and `docs/design/distant-lod.md` §6.1). Every reader is bounds-checked and
//! rejects non-finite or implausible values, because these files arrive from
//! archives a mod may have replaced (ADR-0005).

use crate::{
    asset_path::{AssetKind, canonical_asset_path},
    esm::binary::parse_plugin_metadata,
    mesh::MeshConverter,
};
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure, eyre},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};

/// Fixed size of a `lodsettings/<worldspace>.lod` header.
const LOD_GRID_BYTES: usize = 16;
/// Fixed size of one `.lst` billboard type.
const LST_ENTRY_BYTES: usize = 32;
/// Fixed size of one `.btt` instance record.
const BTT_INSTANCE_BYTES: usize = 32;
/// Level of every shipped tree-LOD block. The `.btt` name carries the level,
/// but only level 4 is generated, and the database keys instances by their
/// level-4 block, which is also the block the engine reads them with.
pub const TREE_LOD_LEVEL: i32 = 4;
/// Largest block level a header may declare before it is implausible.
const MAX_LEVEL: i32 = 1 << 16;
/// Widest billboard accepted from a `.lst`, in Creation units.
const MAX_BILLBOARD_SIZE: f32 = 32768.0;
/// Largest instance scale accepted from a `.btt`.
const MAX_INSTANCE_SCALE: f32 = 1000.0;
/// Atlas rectangles are allowed to overshoot the unit square by this much; the
/// shipped rectangles reach 0.001 past an edge. Sampling is clamped, so a
/// slight overshoot is harmless.
const UV_OVERSHOOT: f32 = 0.01;
/// Cap on collected issue messages, matching the integration report's cap.
const MAX_ISSUES: usize = 100;
/// Vertices in a generated billboard: two quads of four.
const BILLBOARD_VERTICES: usize = 8;
/// Indices in a generated billboard: two triangles per quad.
const BILLBOARD_INDICES: usize = 12;

// `lodsettings/<worldspace>.lod` -------------------------------------------------

/// The block grid a worldspace's LOD was generated on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LodGrid {
    /// South-west corner of the grid, in cells.
    pub origin_x: i16,
    /// South-west corner of the grid, in cells.
    pub origin_y: i16,
    /// Cells along one side of the grid as the generator wrote it. Kept for
    /// the reader's contract only: the shipped value (256) does not describe
    /// the blocks that exist, so nothing derives residency from it.
    pub cells_per_side: i32,
    /// Finest block level present.
    pub min_level: i32,
    /// Coarsest block level present.
    pub max_level: i32,
}

impl LodGrid {
    /// The block levels the header declares, coarsened by powers of two:
    /// Tamriel's `(4, 32)` yields `4, 8, 16, 32`.
    ///
    /// A maximum that is not a power-of-two multiple of the minimum is kept as
    /// its own level rather than silently dropped.
    pub fn levels(&self) -> Result<Vec<i32>> {
        ensure!(
            self.min_level > 0,
            "LOD grid minimum level is not positive: {}",
            self.min_level
        );
        ensure!(
            self.max_level >= self.min_level,
            "LOD grid level range is inverted: {}..{}",
            self.min_level,
            self.max_level
        );
        ensure!(
            self.max_level <= MAX_LEVEL,
            "LOD grid maximum level is implausible: {}",
            self.max_level
        );
        let mut levels = vec![self.min_level];
        let mut level = self.min_level;
        while level < self.max_level {
            let Some(next) = level.checked_mul(2) else {
                break;
            };
            if next >= self.max_level {
                break;
            }
            levels.push(next);
            level = next;
        }
        if levels.last() != Some(&self.max_level) {
            levels.push(self.max_level);
        }
        Ok(levels)
    }
}

/// Decodes a `lodsettings/<worldspace>.lod` header.
///
/// Bytes past the 16-byte header are ignored; none are shipped.
pub fn parse_lod_grid(bytes: &[u8]) -> Result<LodGrid> {
    ensure!(
        bytes.len() >= LOD_GRID_BYTES,
        "truncated LOD grid header: {} bytes",
        bytes.len()
    );
    let origin_x = i16::from_le_bytes(bytes[0..2].try_into().expect("a two-byte LOD grid origin"));
    let origin_y = i16::from_le_bytes(bytes[2..4].try_into().expect("a two-byte LOD grid origin"));
    Ok(LodGrid {
        origin_x,
        origin_y,
        cells_per_side: read_i32(bytes, 4)?,
        min_level: read_i32(bytes, 8)?,
        max_level: read_i32(bytes, 12)?,
    })
}

// `<worldspace>.lst` ------------------------------------------------------------

/// One billboard type from a `<worldspace>.lst` table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeLodType {
    /// The record's own index field, which is its table position in every
    /// shipped file.
    pub index: u32,
    /// Billboard width in Creation units at scale 1.
    pub size_x: f32,
    /// Billboard height in Creation units at scale 1.
    pub size_y: f32,
    /// Atlas rectangle minimum, `(u0, v0)`, with `v` measured from the top of
    /// the atlas image.
    pub uv_min: [f32; 2],
    /// Atlas rectangle maximum, `(u1, v1)`.
    pub uv_max: [f32; 2],
    /// Trailing float of the record; zero in every shipped entry. Kept so the
    /// struct describes the 32-byte record it was read from.
    pub unused: f32,
}

impl TreeLodType {
    /// Rejects entries that cannot describe a billboard.
    ///
    /// These are corruption gates, not format rules: every shipped entry
    /// passes them with room to spare, and a mutated float must not become
    /// geometry or a texture rectangle.
    fn validate(&self) -> Result<()> {
        for (label, value) in [("size_x", self.size_x), ("size_y", self.size_y)] {
            ensure!(
                value.is_finite() && value > 0.0 && value <= MAX_BILLBOARD_SIZE,
                "billboard {label} is implausible: {value}"
            );
        }
        for (label, value) in [
            ("u0", self.uv_min[0]),
            ("v0", self.uv_min[1]),
            ("u1", self.uv_max[0]),
            ("v1", self.uv_max[1]),
        ] {
            ensure!(
                value.is_finite() && (-UV_OVERSHOOT..=1.0 + UV_OVERSHOOT).contains(&value),
                "billboard atlas coordinate {label} is implausible: {value}"
            );
        }
        ensure!(
            self.uv_max[0] > self.uv_min[0] && self.uv_max[1] > self.uv_min[1],
            "billboard has an empty atlas rectangle"
        );
        Ok(())
    }
}

/// Decodes a `<worldspace>.lst` billboard table: a `u32` count followed by one
/// 32-byte entry per type.
///
/// Entries are keyed by their table position, which is what `.btt` groups
/// reference; a record whose `index` field disagrees is treated as corruption
/// and rejected with the rest of the file. Bytes after the declared entries
/// are ignored (none are shipped).
pub fn parse_lst(bytes: &[u8]) -> Result<Vec<TreeLodType>> {
    let count = read_u32(bytes, 0).wrap_err(".lst has no type count")? as usize;
    let declared = count
        .checked_mul(LST_ENTRY_BYTES)
        .and_then(|length| length.checked_add(4))
        .ok_or_else(|| eyre!(".lst declares {count} types, which cannot fit in memory"))?;
    ensure!(
        bytes.len() >= declared,
        ".lst declares {count} types but has only {} bytes",
        bytes.len()
    );
    let mut types = Vec::with_capacity(count);
    for (position, entry) in bytes[4..declared]
        .as_chunks::<LST_ENTRY_BYTES>()
        .0
        .iter()
        .enumerate()
    {
        let tree = TreeLodType {
            index: read_u32(entry, 0)?,
            size_x: read_f32(entry, 4)?,
            size_y: read_f32(entry, 8)?,
            uv_min: [read_f32(entry, 12)?, read_f32(entry, 16)?],
            uv_max: [read_f32(entry, 20)?, read_f32(entry, 24)?],
            unused: read_f32(entry, 28)?,
        };
        ensure!(
            tree.index as usize == position,
            ".lst entry {position} stores index {}",
            tree.index
        );
        tree.validate()
            .wrap_err_with(|| format!(".lst entry {position}"))?;
        types.push(tree);
    }
    Ok(types)
}

// `<worldspace>.<level>.<x>.<y>.btt` ---------------------------------------------

/// One placed billboard: the tree reference's position, yaw and scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeLodInstance {
    /// World position in Creation units; `z` is the tree's base.
    pub position: [f32; 3],
    /// Billboard yaw in radians, independent of the reference's own rotation.
    pub rotation: f32,
    /// The reference's scale, multiplying `size_x` and `size_y`.
    pub scale: f32,
    /// Placed reference FormID, in the authoring plugin's own master index
    /// space (see [`PluginLayout`]).
    pub form_id: u32,
    /// Third unknown word of the record: the group's ordinal in most files.
    pub unknown_a: u32,
    /// Fourth unknown word: zero in 41% of shipped records, otherwise small.
    pub unknown_b: u32,
}

impl TreeLodInstance {
    /// Rejects instances that cannot describe a billboard.
    fn validate(&self) -> Result<()> {
        for (axis, value) in self.position.iter().enumerate() {
            ensure!(
                value.is_finite(),
                "instance position axis {axis} is not finite"
            );
        }
        ensure!(self.rotation.is_finite(), "instance rotation is not finite");
        ensure!(
            self.scale.is_finite() && self.scale > 0.0 && self.scale <= MAX_INSTANCE_SCALE,
            "instance scale is implausible: {}",
            self.scale
        );
        Ok(())
    }
}

/// One type's instances within a `.btt` block.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeLodGroup {
    /// Index into the worldspace's `.lst` table.
    pub type_index: u32,
    /// The billboards of this type placed in the block.
    pub instances: Vec<TreeLodInstance>,
}

/// A decoded `<worldspace>.<level>.<x>.<y>.btt` block.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeLodBlock {
    /// Block south-west cell X, from the file name.
    pub grid_x: i32,
    /// Block south-west cell Y, from the file name.
    pub grid_y: i32,
    /// The groups the file declares.
    pub groups: Vec<TreeLodGroup>,
    /// Bytes after the declared groups.
    ///
    /// Six shipped `dlc2solstheimworld` blocks carry real 32-byte records here
    /// whose framing is not determined by the evidence available, so they are
    /// preserved and reported rather than guessed at.
    pub trailing: Vec<u8>,
}

/// Decodes a `<worldspace>.<level>.<x>.<y>.btt` instance block.
///
/// The block coordinates come from the file name; the file itself does not
/// repeat them.
pub fn parse_btt(bytes: &[u8], grid_x: i32, grid_y: i32) -> Result<TreeLodBlock> {
    let group_count = read_u32(bytes, 0).wrap_err(".btt has no group count")? as usize;
    let mut offset = 4usize;
    let mut groups = Vec::new();
    for group in 0..group_count {
        let header = bytes
            .get(offset..offset.saturating_add(8))
            .ok_or_else(|| eyre!(".btt group {group} header is truncated"))?;
        let type_index = read_u32(header, 0)?;
        let count = read_u32(header, 4)? as usize;
        offset += header.len();
        let length = count
            .checked_mul(BTT_INSTANCE_BYTES)
            .ok_or_else(|| eyre!(".btt group {group} of {count} instances overflows"))?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| eyre!(".btt group {group} of {count} instances overflows"))?;
        let records = bytes.get(offset..end).ok_or_else(|| {
            eyre!(
                ".btt group {group} of {count} instances is truncated at {offset} of {} bytes",
                bytes.len()
            )
        })?;
        let mut instances = Vec::with_capacity(count.min(1024));
        for (index, record) in records
            .as_chunks::<BTT_INSTANCE_BYTES>()
            .0
            .iter()
            .enumerate()
        {
            let instance = TreeLodInstance {
                position: [
                    read_f32(record, 0)?,
                    read_f32(record, 4)?,
                    read_f32(record, 8)?,
                ],
                rotation: read_f32(record, 12)?,
                scale: read_f32(record, 16)?,
                form_id: read_u32(record, 20)?,
                unknown_a: read_u32(record, 24)?,
                unknown_b: read_u32(record, 28)?,
            };
            instance
                .validate()
                .wrap_err_with(|| format!(".btt group {group} instance {index}"))?;
            instances.push(instance);
        }
        groups.push(TreeLodGroup {
            type_index,
            instances,
        });
        offset = end;
    }
    Ok(TreeLodBlock {
        grid_x,
        grid_y,
        groups,
        trailing: bytes.get(offset..).unwrap_or_default().to_vec(),
    })
}

/// `<worldspace>.<level>.<x>.<y>` — the naming convention every LOD block
/// file shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStem {
    /// Worldspace directory name, as written in the file name.
    pub worldspace: String,
    /// Block level; the block's side is this many cells.
    pub level: i32,
    /// South-west cell X of the block.
    pub x: i32,
    /// South-west cell Y of the block.
    pub y: i32,
}

/// Parses a LOD block file stem, rejecting names whose coordinates are not the
/// block's south-west cell (the convention is that both are multiples of the
/// level, which the engine relies on when it looks a block up).
pub fn parse_block_stem(stem: &str) -> Option<BlockStem> {
    let mut parts = stem.split('.');
    let worldspace = parts.next()?;
    let level = parts.next()?.parse::<i32>().ok()?;
    let x = parts.next()?.parse::<i32>().ok()?;
    let y = parts.next()?.parse::<i32>().ok()?;
    if parts.next().is_some()
        || worldspace.is_empty()
        || worldspace.contains(['/', '\\'])
        || !(1..=MAX_LEVEL).contains(&level)
        || x.rem_euclid(level) != 0
        || y.rem_euclid(level) != 0
    {
        return None;
    }
    Some(BlockStem {
        worldspace: worldspace.to_owned(),
        level,
        x,
        y,
    })
}

// FormID resolution ---------------------------------------------------------------

/// One plugin's contribution to a [`PluginLayout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSpec {
    /// Lowercase plugin file name, for example `dragonborn.esm`.
    pub name: String,
    /// The plugin's `MAST` list, in order.
    pub masters: Vec<String>,
    /// Whether the plugin is a light plugin (`.esl`, or the light flag).
    pub light: bool,
}

/// Global FormID slots and `MAST` lists of the plugins in load order.
///
/// A `.btt` record stores the placed reference's FormID with the *authoring*
/// plugin's own master index rather than the load-order slot, so a raw
/// `0x02xxxxxx` from Solstheim (Dragonborn's own index among its two masters)
/// is stored in the database at `0x04xxxxxx` (Dragonborn's slot). Resolving it
/// needs both the owning plugin and that plugin's `MAST` list; Dawnguard's
/// trees only resolve unchanged because its own index coincides with its slot.
///
/// Evidence: `docs/research/dragonborn-formid-slots.md`.
#[derive(Debug, Default)]
pub struct PluginLayout {
    normal: HashMap<String, u32>,
    light: HashMap<String, u32>,
    masters: HashMap<String, Vec<String>>,
}

impl PluginLayout {
    /// Builds a layout without reading plugin files.
    ///
    /// Slots are assigned exactly as `EsmParser::merge_plugins` assigns them:
    /// normal plugins take the next low byte in load order, light plugins are
    /// packed into the `0xFE` space by their own counter. Keep the two in step.
    pub fn from_specs(specs: &[PluginSpec]) -> Result<Self> {
        let mut layout = Self::default();
        let mut next_normal = 0u32;
        let mut next_light = 0u32;
        for spec in specs {
            let name = spec.name.to_ascii_lowercase();
            ensure!(!name.is_empty(), "plugin name is empty");
            if spec.light {
                layout.light.insert(name.clone(), next_light);
                next_light = next_light.checked_add(1).ok_or_else(|| {
                    eyre!("too many light plugins to pack into the 0xFE FormID space")
                })?;
            } else {
                layout.normal.insert(name.clone(), next_normal);
                next_normal = next_normal
                    .checked_add(1)
                    .ok_or_else(|| eyre!("too many plugins to fit in the FormID space"))?;
            }
            layout.masters.insert(
                name,
                spec.masters
                    .iter()
                    .map(|master| master.to_ascii_lowercase())
                    .collect(),
            );
        }
        Ok(layout)
    }

    /// Reads the `MAST` list and light flag of every plugin in load order.
    pub fn from_plugins(plugins: &[PathBuf]) -> Result<Self> {
        let mut specs = Vec::with_capacity(plugins.len());
        for path in plugins {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .ok_or_else(|| eyre!("plugin path has no file name: {}", path.display()))?;
            let metadata = parse_plugin_metadata(path)?;
            let light = path
                .extension()
                .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("esl"))
                || metadata.flags & 0x0000_0200 != 0;
            specs.push(PluginSpec {
                name,
                masters: metadata.masters,
                light,
            });
        }
        Self::from_specs(&specs)
    }

    /// Maps a `form_id` authored by `owner` into the database's FormID space.
    ///
    /// Returns `None` when the owner or the master the FormID names is not in
    /// the load order, so callers can tell "not resolvable here" from a
    /// resolvable id that happens to be absent from the database.
    pub fn remap(&self, owner: &str, form_id: u32) -> Option<u32> {
        if form_id == 0 {
            return None;
        }
        let owner = owner.to_ascii_lowercase();
        let masters = self.masters.get(&owner)?;
        let local_index = (form_id >> 24) as usize;
        let source = match masters.get(local_index) {
            Some(master) => master.clone(),
            None => owner,
        };
        if let Some(index) = self.light.get(&source) {
            return Some(0xFE00_0000 | (index << 12) | (form_id & 0xFFF));
        }
        let index = self.normal.get(&source)?;
        Some((index << 24) | (form_id & 0x00FF_FFFF))
    }
}

// Billboard meshes ----------------------------------------------------------------

/// Builds the GLB for one `.lst` billboard type.
///
/// The mesh is two quads crossed at 90 degrees, `size_x` wide and `size_y`
/// tall in Creation units at scale 1, standing on the instance's base point:
/// the `.btt` record's `z` is the tree's base, so the quads rise from the
/// local origin. Vertices are authored in Creation coordinates and mapped
/// through [`shared::coordinates::creation_to_runtime_vector`], so the result
/// is in the same runtime basis as every converted NIF.
///
/// Normals face out of each quad and the material is double sided with an
/// alpha cut-out, so a billboard is visible from any yaw. The atlas rectangle
/// is baked into `TEXCOORD_0` with `v` measured from the top of the atlas, and
/// the sampler clamps so a neighbouring billboard in the atlas cannot bleed
/// into the rectangle.
pub fn build_tree_billboard_glb(tree: &TreeLodType, texture_uri: &str) -> Result<Vec<u8>> {
    tree.validate()?;
    let half_width = tree.size_x * 0.5;
    let height = tree.size_y;
    let [u0, v0] = tree.uv_min;
    let [u1, v1] = tree.uv_max;
    // Creation space: quad A spans X, quad B spans Y, both rise along Z. The
    // UV rows follow the atlas: `v0` is the rectangle's top row.
    let vertices: [([f32; 3], [f32; 3], [f32; 2]); BILLBOARD_VERTICES] = [
        ([-half_width, 0.0, 0.0], [0.0, 0.0, -1.0], [u0, v1]),
        ([half_width, 0.0, 0.0], [0.0, 0.0, -1.0], [u1, v1]),
        ([half_width, 0.0, height], [0.0, 0.0, -1.0], [u1, v0]),
        ([-half_width, 0.0, height], [0.0, 0.0, -1.0], [u0, v0]),
        ([0.0, half_width, 0.0], [1.0, 0.0, 0.0], [u0, v1]),
        ([0.0, -half_width, 0.0], [1.0, 0.0, 0.0], [u1, v1]),
        ([0.0, -half_width, height], [1.0, 0.0, 0.0], [u1, v0]),
        ([0.0, half_width, height], [1.0, 0.0, 0.0], [u0, v0]),
    ];
    // Two triangles per quad, wound so each quad's front face carries its
    // normal (the quads only differ by which axis they span).
    const INDICES: [u16; BILLBOARD_INDICES] = [0, 2, 1, 0, 3, 2, 4, 6, 5, 4, 7, 6];
    let mut binary = Vec::with_capacity(BILLBOARD_VERTICES * 32 + BILLBOARD_INDICES * 2);
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for (position, normal, uv) in vertices {
        let runtime = shared::coordinates::creation_to_runtime_vector(position);
        for (axis, value) in runtime.iter().enumerate() {
            min[axis] = min[axis].min(*value);
            max[axis] = max[axis].max(*value);
        }
        for value in runtime.iter().chain(normal.iter()).chain(uv.iter()) {
            binary.extend_from_slice(&value.to_le_bytes());
        }
    }
    for index in INDICES {
        binary.extend_from_slice(&index.to_le_bytes());
    }
    let document = json!({
        "asset": { "version": "2.0", "generator": "OpenSkyrim converter" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": "TreeLodBillboard", "mesh": 0 }],
        "meshes": [{
            "name": "TreeLodBillboard",
            "primitives": [{
                "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 },
                "indices": 3,
                "material": 0,
            }],
        }],
        "materials": [{
            "name": "TreeLodAtlas",
            "alphaMode": "MASK",
            "alphaCutoff": 0.5,
            "doubleSided": true,
            "pbrMetallicRoughness": {
                "baseColorTexture": { "index": 0 },
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0,
            },
        }],
        "samplers": [{ "magFilter": 9729, "minFilter": 9987, "wrapS": 33071, "wrapT": 33071 }],
        "textures": [{ "sampler": 0, "source": 0 }],
        "images": [{ "uri": texture_uri }],
        "accessors": [
            {
                "bufferView": 0, "byteOffset": 0, "componentType": 5126,
                "count": BILLBOARD_VERTICES, "type": "VEC3",
                "min": [min[0], min[1], min[2]], "max": [max[0], max[1], max[2]],
            },
            {
                "bufferView": 0, "byteOffset": 12, "componentType": 5126,
                "count": BILLBOARD_VERTICES, "type": "VEC3",
            },
            {
                "bufferView": 0, "byteOffset": 24, "componentType": 5126,
                "count": BILLBOARD_VERTICES, "type": "VEC2",
            },
            {
                "bufferView": 1, "byteOffset": 0, "componentType": 5123,
                "count": BILLBOARD_INDICES, "type": "SCALAR",
            },
        ],
        "bufferViews": [
            {
                "buffer": 0, "byteOffset": 0,
                "byteLength": BILLBOARD_VERTICES * 32, "byteStride": 32, "target": 34962,
            },
            {
                "buffer": 0, "byteOffset": BILLBOARD_VERTICES * 32,
                "byteLength": BILLBOARD_INDICES * 2, "target": 34963,
            },
        ],
        "buffers": [{ "byteLength": binary.len() }],
    });
    encode_glb(&document, &binary)
}

/// Wraps a glTF document and its binary chunk into a GLB container, padding
/// both chunks to a four-byte boundary as the GLB specification requires.
fn encode_glb(document: &serde_json::Value, binary: &[u8]) -> Result<Vec<u8>> {
    let mut json = serde_json::to_vec(document)?;
    while json.len() % 4 != 0 {
        json.push(b' ');
    }
    let mut binary = binary.to_vec();
    while !binary.len().is_multiple_of(4) {
        binary.push(0);
    }
    let total = 12usize
        .checked_add(8)
        .and_then(|length| length.checked_add(json.len()))
        .and_then(|length| length.checked_add(8))
        .and_then(|length| length.checked_add(binary.len()))
        .ok_or_else(|| eyre!("GLB size overflow"))?;
    let mut output = Vec::with_capacity(total);
    output.extend_from_slice(b"glTF");
    output.extend_from_slice(&2u32.to_le_bytes());
    output.extend_from_slice(&u32::try_from(total)?.to_le_bytes());
    output.extend_from_slice(&u32::try_from(json.len())?.to_le_bytes());
    output.extend_from_slice(b"JSON");
    output.extend_from_slice(&json);
    output.extend_from_slice(&u32::try_from(binary.len())?.to_le_bytes());
    output.extend_from_slice(b"BIN\0");
    output.extend_from_slice(&binary);
    Ok(output)
}

/// Rewrites an assets-root-relative texture path into a URI relative to
/// `glb_path`, the way `material::runtime_texture_uri` does for converted
/// NIFs.
fn runtime_texture_uri(glb_path: &Path, texture_path: &str) -> String {
    let depth = glb_path.components().count().saturating_sub(1);
    format!("{}{}", "../".repeat(depth), texture_path)
}

// Inventory -----------------------------------------------------------------------

/// What one [`record_lod_inventory`] pass found, wrote and could not interpret.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LodInventoryReport {
    /// Worldspace directories that matched a worldspace record.
    pub worldspaces: u64,
    /// `lod_grid` rows written.
    pub grids: u64,
    /// `lod_block` rows written for terrain blocks.
    pub terrain_blocks: u64,
    /// `lod_block` rows written for object blocks.
    pub object_blocks: u64,
    /// Blocks skipped because their converted mesh is missing.
    pub missing_blocks: u64,
    /// Blocks recorded without bounds, because their converted mesh carries
    /// none. A block without bounds cannot be validated or occluded, but it is
    /// counted rather than treated as a failure, the way the integration step
    /// counts statics whose model has no bounds.
    pub unbounded_blocks: u64,
    /// `lod_tree_type` rows written, one billboard each.
    pub tree_types: u64,
    /// `lod_tree_instance` rows written.
    pub tree_instances: u64,
    /// Instances whose reference FormID resolves to no `references` row.
    ///
    /// A small count is expected: the shipped `.btt` files predate reference
    /// edits by later plugins and keep stale positions and ids.
    pub tree_instances_unresolved: u64,
    /// `.btt` groups whose type index is outside their `.lst` table.
    pub out_of_range_groups: u64,
    /// `.btt` blocks with bytes after their declared groups.
    pub trailing_blocks: u64,
    /// Trailing bytes across those blocks.
    pub trailing_bytes: u64,
    /// Published billboard meshes, relative to the assets root.
    pub billboards: Vec<PathBuf>,
    /// Defects that make the inventory incomplete: a file that could not be
    /// interpreted, a block whose converted mesh is missing, a tree type
    /// without its atlas. The pipeline turns these into conversion warnings,
    /// which fail the conversion. Capped by [`MAX_ISSUES`].
    pub errors: Vec<String>,
    /// Diagnostics that do not fail the conversion on their own: assets for a
    /// worldspace that is not loaded, block files whose names do not follow the
    /// convention, blocks recorded without bounds, stale instance FormIDs.
    /// Capped by [`MAX_ISSUES`].
    pub issues: Vec<String>,
}

/// Records the distant-LOD inventory of one converted asset set.
///
/// Reads `lodsettings/<worldspace>.lod`, the block meshes under
/// `meshes/terrain/**` and the tree tables under `.../trees/` from
/// `<staging>/vfs`, writes the four `lod_*` tables of
/// `<staging>/skyrim_world.db`, and publishes one billboard GLB per tree type
/// under `meshes/terrain/<worldspace>/trees/<worldspace>.tree.<n>.glb`.
///
/// A set without a world database has no worldspaces to key the inventory on
/// and yields an empty report. A file that cannot be interpreted, a block
/// whose converted mesh is missing and a tree type without its atlas are
/// collected in [`LodInventoryReport::errors`], which the pipeline turns into
/// conversion warnings; everything else the pass skips or suspects is counted
/// in [`LodInventoryReport::issues`] without failing the conversion.
pub fn record_lod_inventory(staging: &Path, plugins: &[PathBuf]) -> Result<LodInventoryReport> {
    let database = staging.join("skyrim_world.db");
    if !database.is_file() {
        return Ok(LodInventoryReport::default());
    }
    let mut connection = Connection::open(&database)
        .wrap_err_with(|| format!("failed to open {}", database.display()))?;
    let layout = PluginLayout::from_plugins(plugins)?;
    let worldspaces = load_worldspaces(&connection)?;
    let vfs = staging.join("vfs");
    let lodsettings = vfs.join("lodsettings");
    let terrain = vfs.join("meshes").join("terrain");
    // The vfs preserves the case the archive used, so the worldspace key is
    // lowered for the database lookup while the on-disk names are kept for
    // reading. A worldspace can appear in either place alone.
    let mut directories = BTreeMap::<String, LodDirectories>::new();
    for file in files_with_extension(&lodsettings, "lod")? {
        if let Some(stem) = file.file_stem().and_then(|value| value.to_str()) {
            directories
                .entry(stem.to_ascii_lowercase())
                .or_default()
                .grid = Some(stem.to_owned());
        }
    }
    for name in subdirectories(&terrain)? {
        let key = name.to_ascii_lowercase();
        directories.entry(key).or_default().terrain = Some(name);
    }
    let mut report = LodInventoryReport::default();
    let transaction = connection.transaction()?;
    for table in [
        "lod_grid",
        "lod_block",
        "lod_tree_type",
        "lod_tree_instance",
    ] {
        transaction
            .execute(&format!("DELETE FROM {table}"), [])
            .wrap_err_with(|| format!("failed to clear {table}"))?;
    }
    for (key, names) in directories {
        let display = names
            .terrain
            .clone()
            .or_else(|| names.grid.clone())
            .unwrap_or_else(|| key.clone());
        let Some(worldspace) = worldspaces.get(&key) else {
            issue(
                &mut report,
                format!("distant LOD assets for {display:?} have no worldspace record"),
            );
            continue;
        };
        report.worldspaces += 1;
        let worldspace_lod = WorldspaceLod {
            id: worldspace.id,
            owner: worldspace.owner.clone().unwrap_or_default(),
            // Generated paths use the lowercase name the rest of the asset
            // tree uses, so the engine's path joins stay canonical.
            key,
            directory: display,
        };
        if let Some(stem) = &names.grid {
            record_grid(
                &transaction,
                &lodsettings.join(format!("{stem}.lod")),
                &mut report,
                worldspace_lod.id,
            )?;
        }
        let Some(terrain_name) = &names.terrain else {
            continue;
        };
        let blocks = terrain.join(terrain_name);
        record_blocks(
            &transaction,
            staging,
            &mut report,
            &worldspace_lod,
            BlockKind::Terrain,
            &blocks,
        )?;
        record_blocks(
            &transaction,
            staging,
            &mut report,
            &worldspace_lod,
            BlockKind::Objects,
            &blocks.join("objects"),
        )?;
        record_trees(
            &transaction,
            staging,
            &layout,
            &mut report,
            &worldspace_lod,
            &blocks.join("trees"),
        )?;
    }
    transaction.commit()?;
    Ok(report)
}

/// Where a worldspace's LOD assets live, as the vfs spells them.
#[derive(Debug, Default)]
struct LodDirectories {
    /// Stem of `lodsettings/<stem>.lod`.
    grid: Option<String>,
    /// Directory under `meshes/terrain/`.
    terrain: Option<String>,
}

/// One worldspace directory being inventoried.
struct WorldspaceLod {
    /// Worldspace FormID, the key every LOD table uses.
    id: u32,
    /// Plugin whose `WRLD` record defines the worldspace, lowercased.
    owner: String,
    /// Lowercase worldspace name, used for generated asset paths.
    key: String,
    /// Worldspace name as the vfs spells it, used in messages.
    directory: String,
}

/// The worldspace's defining plugin, as far as the database records it.
struct WorldspaceRow {
    id: u32,
    owner: Option<String>,
}

/// Reads every worldspace and the plugin whose `WRLD` record defines it.
///
/// `records.load_order` is the load-order index of the plugin that supplied
/// the record the export kept, which for a worldspace is the plugin that owns
/// its LOD directory. `plugins.id` is written as that same index.
fn load_worldspaces(connection: &Connection) -> Result<HashMap<String, WorldspaceRow>> {
    let mut statement = connection.prepare(
        "SELECT w.id, w.editor_id, p.name FROM worldspaces w \
         LEFT JOIN records r ON r.form_id = w.id \
         LEFT JOIN plugins p ON p.id = r.load_order",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, editor_id, owner)| {
            (
                editor_id.to_ascii_lowercase(),
                WorldspaceRow {
                    id,
                    owner: owner.map(|owner| owner.to_ascii_lowercase()),
                },
            )
        })
        .collect())
}

/// Writes the worldspace's `lod_grid` row from its `lodsettings` header.
fn record_grid(
    transaction: &rusqlite::Transaction<'_>,
    path: &Path,
    report: &mut LodInventoryReport,
    worldspace_id: u32,
) -> Result<()> {
    let bytes = fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    match parse_lod_grid(&bytes).and_then(|grid| grid.levels().map(|levels| (grid, levels))) {
        Ok((grid, levels)) => {
            let levels = levels
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            transaction.execute(
                "INSERT OR REPLACE INTO lod_grid(worldspace_id, origin_x, origin_y, levels) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![worldspace_id, grid.origin_x, grid.origin_y, levels],
            )?;
            report.grids += 1;
        }
        Err(failure) => error(
            report,
            format!("unusable LOD grid {}: {failure:#}", path.display()),
        ),
    }
    Ok(())
}

/// The two block kinds the database stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Terrain,
    Objects,
}

impl BlockKind {
    /// The `lod_block.kind` value.
    const fn name(self) -> &'static str {
        match self {
            Self::Terrain => "terrain",
            Self::Objects => "objects",
        }
    }

    /// The source file extension of a block of this kind.
    const fn extension(self) -> &'static str {
        match self {
            Self::Terrain => "btr",
            Self::Objects => "bto",
        }
    }
}

/// Writes one `lod_block` row per converted block of `kind`.
fn record_blocks(
    transaction: &rusqlite::Transaction<'_>,
    staging: &Path,
    report: &mut LodInventoryReport,
    worldspace: &WorldspaceLod,
    kind: BlockKind,
    directory: &Path,
) -> Result<()> {
    let vfs = staging.join("vfs");
    for path in files_with_extension(directory, kind.extension())? {
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(block) = parse_block_stem(stem) else {
            issue(
                report,
                format!("unrecognised distant LOD block name {}", path.display()),
            );
            continue;
        };
        if !block.worldspace.eq_ignore_ascii_case(&worldspace.directory) {
            issue(
                report,
                format!(
                    "distant LOD block {} does not belong to worldspace {}",
                    path.display(),
                    worldspace.directory
                ),
            );
            continue;
        }
        let relative = path
            .strip_prefix(&vfs)
            .wrap_err_with(|| format!("{} is not under the vfs", path.display()))?;
        let mesh_path = canonical_asset_path(&relative.to_string_lossy(), AssetKind::Mesh, "glb")?;
        let mesh = staging.join(&mesh_path);
        if !mesh.is_file() {
            report.missing_blocks += 1;
            error(
                report,
                format!("distant LOD block {mesh_path} has no converted mesh"),
            );
            continue;
        }
        // A mesh without bounds is still a usable block: it is counted the way
        // the integration step counts statics whose model has no bounds.
        let bounds = match MeshConverter::glb_bounds(&mesh) {
            Ok(bounds) => Some(bounds),
            Err(failure) => {
                report.unbounded_blocks += 1;
                issue(
                    report,
                    format!("distant LOD block {mesh_path} has no bounds: {failure:#}"),
                );
                None
            }
        };
        // A block the converter could not measure keeps NULL bounds.
        let bounds = bounds.map(|bounds| (bounds.min, bounds.max));
        transaction.execute(
            "INSERT OR REPLACE INTO lod_block(worldspace_id, kind, level, block_x, block_y, \
             mesh_path, bounds_min_x, bounds_min_y, bounds_min_z, bounds_max_x, bounds_max_y, \
             bounds_max_z) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                worldspace.id,
                kind.name(),
                block.level,
                block.x,
                block.y,
                mesh_path,
                bounds.map(|(min, _)| min[0]),
                bounds.map(|(min, _)| min[1]),
                bounds.map(|(min, _)| min[2]),
                bounds.map(|(_, max)| max[0]),
                bounds.map(|(_, max)| max[1]),
                bounds.map(|(_, max)| max[2]),
            ],
        )?;
        match kind {
            BlockKind::Terrain => report.terrain_blocks += 1,
            BlockKind::Objects => report.object_blocks += 1,
        }
    }
    Ok(())
}

/// Writes the worldspace's tree types, their billboard meshes and every tree
/// instance, and counts how many instances resolve to a reference.
fn record_trees(
    transaction: &rusqlite::Transaction<'_>,
    staging: &Path,
    layout: &PluginLayout,
    report: &mut LodInventoryReport,
    worldspace: &WorldspaceLod,
    directory: &Path,
) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    let vfs = staging.join("vfs");
    // The type table decides whether there is anything to publish at all:
    // `dlc01soulcairn` and `dlc2apocryphaworld` ship a `trees/` directory with
    // an empty `.lst` and no atlas, which is not a defect.
    let Some(table) = files_with_extension(directory, "lst")?.into_iter().next() else {
        return Ok(());
    };
    let bytes = fs::read(&table).wrap_err_with(|| format!("failed to read {}", table.display()))?;
    let types = match parse_lst(&bytes) {
        Ok(types) => types,
        Err(failure) => {
            error(
                report,
                format!("unusable tree LOD table {}: {failure:#}", table.display()),
            );
            return Ok(());
        }
    };
    if types.is_empty() {
        let blocks = files_with_extension(directory, "btt")?;
        if !blocks.is_empty() {
            error(
                report,
                format!(
                    "{} has {} tree LOD block(s) but no billboard types",
                    directory.display(),
                    blocks.len()
                ),
            );
        }
        return Ok(());
    }
    // The atlas sits with the textures, not with the block meshes: each
    // worldspace ships one `textures/terrain/<worldspace>/trees/*.dds`. The
    // tree material samples it as a base colour, so the billboards reference
    // its sRGB alias, which the texture stage publishes from these meshes.
    let textures = vfs.join("textures").join("terrain");
    let Some(atlas_directory) = child_directory(&textures, &worldspace.key)
        .and_then(|worldspace_textures| child_directory(&worldspace_textures, "trees"))
    else {
        error(
            report,
            format!(
                "tree LOD for {} has no atlas under {}",
                directory.display(),
                textures.display()
            ),
        );
        return Ok(());
    };
    let Some(atlas) = files_with_extension(&atlas_directory, "dds")?
        .into_iter()
        .next()
    else {
        error(
            report,
            format!(
                "tree LOD atlas directory {} is empty",
                atlas_directory.display()
            ),
        );
        return Ok(());
    };
    let relative = atlas
        .strip_prefix(&vfs)
        .wrap_err_with(|| format!("{} is not under the vfs", atlas.display()))?;
    let atlas_key = canonical_asset_path(&relative.to_string_lossy(), AssetKind::Texture, "ktx2")?;
    let Some(stem) = atlas_key.strip_suffix(".ktx2") else {
        return Ok(());
    };
    let atlas_uri = format!("{stem}.opensky-srgb.ktx2");
    for (position, tree) in types.iter().enumerate() {
        let mesh_path = format!(
            "meshes/terrain/{}/trees/{}.tree.{position}.glb",
            worldspace.key, worldspace.key
        );
        let uri = runtime_texture_uri(Path::new(&mesh_path), &atlas_uri);
        let glb = build_tree_billboard_glb(tree, &uri)?;
        let destination = staging.join(&mesh_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&destination, &glb)
            .wrap_err_with(|| format!("failed to write {}", destination.display()))?;
        transaction.execute(
            "INSERT OR REPLACE INTO lod_tree_type(worldspace_id, tree_index, mesh_path, size_x, \
             size_y, u0, v0, u1, v1) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                worldspace.id,
                position as u32,
                mesh_path,
                tree.size_x,
                tree.size_y,
                tree.uv_min[0],
                tree.uv_min[1],
                tree.uv_max[0],
                tree.uv_max[1],
            ],
        )?;
        report.tree_types += 1;
        report.billboards.push(PathBuf::from(&mesh_path));
    }
    let mut reference = transaction.prepare("SELECT 1 FROM \"references\" WHERE id = ?1")?;
    for path in files_with_extension(directory, "btt")? {
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(block) = parse_block_stem(stem) else {
            issue(
                report,
                format!("unrecognised tree LOD block name {}", path.display()),
            );
            continue;
        };
        if !block.worldspace.eq_ignore_ascii_case(&worldspace.directory) {
            issue(
                report,
                format!(
                    "tree LOD block {} does not belong to worldspace {}",
                    path.display(),
                    worldspace.directory
                ),
            );
            continue;
        }
        if block.level != TREE_LOD_LEVEL {
            issue(
                report,
                format!(
                    "tree LOD block {} is level {}, not {TREE_LOD_LEVEL}",
                    path.display(),
                    block.level
                ),
            );
            continue;
        }
        let bytes =
            fs::read(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let parsed = match parse_btt(&bytes, block.x, block.y) {
            Ok(parsed) => parsed,
            Err(failure) => {
                error(
                    report,
                    format!("unusable tree LOD block {}: {failure:#}", path.display()),
                );
                continue;
            }
        };
        if !parsed.trailing.is_empty() {
            report.trailing_blocks += 1;
            report.trailing_bytes += parsed.trailing.len() as u64;
            eprintln!(
                "warning: tree LOD block {} has {} bytes after its declared groups; kept opaque",
                path.display(),
                parsed.trailing.len()
            );
        }
        for group in &parsed.groups {
            if group.type_index as usize >= types.len() {
                report.out_of_range_groups += 1;
                continue;
            }
            for instance in &group.instances {
                transaction.execute(
                    "INSERT INTO lod_tree_instance(worldspace_id, block_x, block_y, tree_index, \
                     pos_x, pos_y, pos_z, rotation, scale) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        worldspace.id,
                        parsed.grid_x,
                        parsed.grid_y,
                        group.type_index,
                        instance.position[0],
                        instance.position[1],
                        instance.position[2],
                        instance.rotation,
                        instance.scale,
                    ],
                )?;
                report.tree_instances += 1;
                let resolved = layout
                    .remap(&worldspace.owner, instance.form_id)
                    .is_some_and(|form_id| reference.exists(params![form_id]).unwrap_or(false));
                if !resolved {
                    report.tree_instances_unresolved += 1;
                }
            }
        }
    }
    Ok(())
}

/// Records one blocking defect, capped so a broken asset set cannot flood the
/// report.
fn error(report: &mut LodInventoryReport, message: String) {
    if report.errors.len() < MAX_ISSUES {
        report.errors.push(message);
    }
}

/// Records one diagnostic, capped the same way.
fn issue(report: &mut LodInventoryReport, message: String) {
    if report.issues.len() < MAX_ISSUES {
        report.issues.push(message);
    }
}

/// Files in `directory` whose extension matches, sorted for determinism. A
/// missing directory yields none.
fn files_with_extension(directory: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)
        .wrap_err_with(|| format!("failed to read {}", directory.display()))?
    {
        let path = entry?.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Subdirectory names of `directory`, sorted for determinism. A missing
/// directory yields none.
fn subdirectories(directory: &Path) -> Result<Vec<String>> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(directory)
        .wrap_err_with(|| format!("failed to read {}", directory.display()))?
    {
        let path = entry?.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|value| value.to_str())
        {
            names.insert(name.to_owned());
        }
    }
    Ok(names.into_iter().collect())
}

/// The child directory of `parent` whose name matches `name` ignoring case.
fn child_directory(parent: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(parent).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case(name))
        {
            return Some(path);
        }
    }
    None
}

/// Reads a little-endian `u32`, rejecting a truncated read.
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| eyre!("truncated 32-bit read at byte {offset}"))?;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
}

/// Reads a little-endian `i32`, rejecting a truncated read.
fn read_i32(bytes: &[u8], offset: usize) -> Result<i32> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| eyre!("truncated 32-bit read at byte {offset}"))?;
    Ok(i32::from_le_bytes(value.try_into().expect("four bytes")))
}

/// Reads a little-endian `f32`, rejecting a truncated read. Non-finite values
/// are caught by the caller's validation, not here.
fn read_f32(bytes: &[u8], offset: usize) -> Result<f32> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| eyre!("truncated 32-bit read at byte {offset}"))?;
    Ok(f32::from_le_bytes(value.try_into().expect("four bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::esm::exporter::create_tables;
    use dummy_content::rng::Rng;
    use serde_json::Value;

    /// A `lodsettings` header.
    fn lod_header(origin: [i16; 2], cells: i32, levels: [i32; 2]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LOD_GRID_BYTES);
        bytes.extend_from_slice(&origin[0].to_le_bytes());
        bytes.extend_from_slice(&origin[1].to_le_bytes());
        bytes.extend_from_slice(&cells.to_le_bytes());
        bytes.extend_from_slice(&levels[0].to_le_bytes());
        bytes.extend_from_slice(&levels[1].to_le_bytes());
        bytes
    }

    /// One `.lst` entry: index, size, atlas rectangle.
    fn lst_entry(index: u32, size: [f32; 2], uv: [f32; 4]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LST_ENTRY_BYTES);
        bytes.extend_from_slice(&index.to_le_bytes());
        for value in size.into_iter().chain(uv) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
        bytes
    }

    /// A `.lst` table.
    fn lst_table(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = (entries.len() as u32).to_le_bytes().to_vec();
        for entry in entries {
            bytes.extend_from_slice(entry);
        }
        bytes
    }

    /// One `.btt` instance record.
    fn btt_instance(position: [f32; 3], rotation: f32, scale: f32, form_id: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(BTT_INSTANCE_BYTES);
        for value in position.into_iter().chain([rotation, scale]) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&form_id.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes
    }

    /// A `.btt` block: `(type index, instances)` groups, then trailing bytes.
    fn btt_block(groups: &[(u32, Vec<Vec<u8>>)], trailing: &[u8]) -> Vec<u8> {
        let mut bytes = (groups.len() as u32).to_le_bytes().to_vec();
        for (type_index, instances) in groups {
            bytes.extend_from_slice(&type_index.to_le_bytes());
            bytes.extend_from_slice(&(instances.len() as u32).to_le_bytes());
            for instance in instances {
                bytes.extend_from_slice(instance);
            }
        }
        bytes.extend_from_slice(trailing);
        bytes
    }

    /// A minimal plugin file: a `TES4` record carrying a `MAST` list.
    fn plugin_bytes(masters: &[&str]) -> Vec<u8> {
        let mut payload = Vec::new();
        for master in masters {
            let mut name = master.as_bytes().to_vec();
            name.push(0);
            payload.extend_from_slice(b"MAST");
            payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
            payload.extend_from_slice(&name);
        }
        let mut bytes = b"TES4".to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&44u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    /// One `.lst` entry as a type, spelled out field by field.
    fn tree_type(size: [f32; 2], uv: [f32; 4]) -> TreeLodType {
        TreeLodType {
            index: 0,
            size_x: size[0],
            size_y: size[1],
            uv_min: [uv[0], uv[1]],
            uv_max: [uv[2], uv[3]],
            unused: 0.0,
        }
    }

    /// The glTF JSON chunk of a GLB.
    fn glb_json(bytes: &[u8]) -> Value {
        assert_eq!(&bytes[..4], b"glTF", "invalid GLB signature");
        assert_eq!(&bytes[16..20], b"JSON", "missing GLB JSON chunk");
        let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        serde_json::from_slice(&bytes[20..20 + json_length]).expect("invalid glTF JSON")
    }

    /// The `f32` values of an accessor, read from the GLB binary chunk.
    fn accessor_floats(bytes: &[u8], accessor: usize, components: usize) -> Vec<f32> {
        let document = glb_json(bytes);
        let accessor = &document["accessors"][accessor];
        assert_eq!(accessor["componentType"], 5126, "expected a f32 accessor");
        let view = &document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
        let count = accessor["count"].as_u64().unwrap() as usize * components;
        let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let binary = 20 + json_length.next_multiple_of(4) + 8;
        let start = binary
            + view["byteOffset"].as_u64().unwrap_or(0) as usize
            + accessor["byteOffset"].as_u64().unwrap_or(0) as usize;
        // Honour an interleaved view: each element starts `byteStride` apart.
        let stride = view["byteStride"]
            .as_u64()
            .map_or(components * 4, |stride| stride as usize);
        let elements = count / components;
        (0..elements)
            .flat_map(|element| {
                let offset = start + element * stride;
                (0..components).map(move |component| {
                    let at = offset + component * 4;
                    f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
                })
            })
            .collect()
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn parses_the_tamriel_grid_header() {
        let grid = parse_lod_grid(&lod_header([-96, -96], 256, [4, 32])).unwrap();
        assert_eq!(
            (grid.origin_x, grid.origin_y, grid.cells_per_side),
            (-96, -96, 256)
        );
        assert_eq!(grid.levels().unwrap(), vec![4, 8, 16, 32]);
        assert_eq!(
            parse_lod_grid(&lod_header([-64, -64], 256, [4, 6]))
                .unwrap()
                .levels()
                .unwrap(),
            vec![4, 6],
            "a declared maximum that is not a doubling is kept"
        );
    }

    #[test]
    fn accepts_grid_trailing_bytes_and_rejects_truncation() {
        let mut bytes = lod_header([-96, -96], 256, [4, 32]);
        bytes.extend_from_slice(&[0u8; 9]);
        assert!(parse_lod_grid(&bytes).is_ok());
        for length in 0..LOD_GRID_BYTES {
            assert!(parse_lod_grid(&bytes[..length]).is_err(), "length {length}");
            let mut mutated = bytes[..LOD_GRID_BYTES].to_vec();
            mutated[length] ^= 0xff;
            let _ = parse_lod_grid(&mutated).map(|grid| grid.levels());
        }
    }

    #[test]
    fn rejects_grid_levels_that_cannot_describe_blocks() {
        for levels in [[0, 32], [32, 4], [-4, 32], [4, i32::MAX]] {
            assert!(
                parse_lod_grid(&lod_header([0, 0], 32, levels))
                    .unwrap()
                    .levels()
                    .is_err(),
                "{levels:?}"
            );
        }
    }

    #[test]
    fn parses_lst_entries_and_ignores_trailing_bytes() {
        let table = lst_table(&[
            lst_entry(0, [333.5, 741.7], [0.0, 0.0, 0.125, 0.25]),
            lst_entry(1, [1199.8, 2556.2], [0.5, 0.5, 1.0009, 0.75]),
        ]);
        let types = parse_lst(&table).unwrap();
        assert_eq!(types.len(), 2);
        assert_eq!(types[1].index, 1);
        assert!((types[1].size_x - 1199.8).abs() < 1.0e-3);
        assert!((types[1].uv_max[0] - 1.0009).abs() < 1.0e-4);
        let mut padded = table.clone();
        padded.extend_from_slice(&[1, 2, 3]);
        assert_eq!(parse_lst(&padded).unwrap().len(), 2);
    }

    #[test]
    fn rejects_lst_tables_that_are_truncated_or_absurd() {
        let table = lst_table(&[lst_entry(0, [10.0, 20.0], [0.0, 0.0, 0.5, 0.5])]);
        for length in 0..table.len() {
            assert!(parse_lst(&table[..length]).is_err(), "length {length}");
        }
        for entry in [
            lst_entry(1, [10.0, 20.0], [0.0, 0.0, 0.5, 0.5]),
            lst_entry(0, [0.0, 20.0], [0.0, 0.0, 0.5, 0.5]),
            lst_entry(0, [10.0, f32::NAN], [0.0, 0.0, 0.5, 0.5]),
            lst_entry(0, [10.0, 20.0], [0.5, 0.0, 0.5, 0.5]),
            lst_entry(0, [10.0, 20.0], [0.0, 0.0, 0.5, 4.0]),
        ] {
            assert!(parse_lst(&lst_table(&[entry])).is_err());
        }
        let mut rng = Rng::new(11);
        for _ in 0..256 {
            let mut mutated = table.clone();
            let index = rng.next_u64() as usize % mutated.len();
            mutated[index] ^= 0xff;
            let _ = parse_lst(&mutated);
        }
    }

    #[test]
    fn parses_btt_groups_and_preserves_trailing_bytes() {
        let block = btt_block(
            &[
                (
                    0,
                    vec![btt_instance([-49049.4, -46474.7, 773.1], 0.781, 1.02, 0x12)],
                ),
                (
                    24,
                    vec![
                        btt_instance([100.0, 200.0, 300.0], 0.0, 1.0, 0x0200_0022),
                        btt_instance([1.0, 2.0, 3.0], 6.0, 0.5, 0x0200_0023),
                    ],
                ),
            ],
            &[0xAB; 12],
        );
        let parsed = parse_btt(&block, -12, -12).unwrap();
        assert_eq!((parsed.grid_x, parsed.grid_y), (-12, -12));
        assert_eq!(parsed.groups.len(), 2);
        assert_eq!(parsed.groups[1].type_index, 24);
        assert_eq!(parsed.groups[1].instances.len(), 2);
        assert_eq!(parsed.groups[0].instances[0].form_id, 0x12);
        assert!((parsed.groups[0].instances[0].position[0] + 49049.4).abs() < 0.1);
        assert_eq!(parsed.trailing.len(), 12);
    }

    #[test]
    fn btt_never_panics_and_rejects_truncation_mutation_and_absurd_values() {
        let block = btt_block(
            &[
                (
                    0,
                    vec![
                        btt_instance([1.0, 2.0, 3.0], 0.5, 1.0, 0x12),
                        btt_instance([4.0, 5.0, 6.0], 1.5, 1.0, 0x13),
                    ],
                ),
                (1, vec![btt_instance([7.0, 8.0, 9.0], 2.5, 0.5, 0x14)]),
            ],
            &[],
        );
        for length in 0..block.len() {
            assert!(
                parse_btt(&block[..length], 0, 0).is_err(),
                "length {length}"
            );
        }
        let mut rng = Rng::new(29);
        for _ in 0..512 {
            let mut mutated = block.clone();
            let index = rng.next_u64() as usize % mutated.len();
            mutated[index] ^= 0xff;
            let _ = parse_btt(&mutated, 0, 0);
        }
        let mut absurd = 4_000_000_000u32.to_le_bytes().to_vec();
        absurd.extend_from_slice(&[0u8; 32]);
        assert!(parse_btt(&absurd, 0, 0).is_err());
        for instance in [
            btt_instance([f32::NAN, 0.0, 0.0], 0.0, 1.0, 1),
            btt_instance([0.0; 3], f32::INFINITY, 1.0, 1),
            btt_instance([0.0; 3], 0.0, f32::INFINITY, 1),
            btt_instance([0.0; 3], 0.0, 0.0, 1),
        ] {
            assert!(parse_btt(&btt_block(&[(0, vec![instance])], &[]), 0, 0).is_err());
        }
    }

    #[test]
    fn parses_block_stems_and_rejects_foreign_coordinates() {
        let block = parse_block_stem("tamriel.4.-12.-12").unwrap();
        assert_eq!(
            (block.worldspace.as_str(), block.level, block.x, block.y),
            ("tamriel", 4, -12, -12)
        );
        assert_eq!(
            parse_block_stem("dlc2solstheimworld.4.-64.-64").unwrap().x,
            -64
        );
        for stem in [
            "tamriel.4.3.3",
            "tamriel.4.0",
            "tamriel.4.0.0.0",
            "tamriel.0.0.0",
            "tamriel.4.x.0",
            ".4.0.0",
            "tamriel/4.0.0",
        ] {
            assert!(parse_block_stem(stem).is_none(), "{stem}");
        }
    }

    #[test]
    fn remaps_btt_form_ids_through_the_owning_plugins_master_list() {
        let layout = PluginLayout::from_specs(&[
            PluginSpec {
                name: "Skyrim.esm".to_owned(),
                masters: Vec::new(),
                light: false,
            },
            PluginSpec {
                name: "Update.esm".to_owned(),
                masters: vec!["Skyrim.esm".to_owned()],
                light: false,
            },
            PluginSpec {
                name: "Dawnguard.esm".to_owned(),
                masters: vec!["Skyrim.esm".to_owned(), "Update.esm".to_owned()],
                light: false,
            },
            PluginSpec {
                name: "HearthFires.esm".to_owned(),
                masters: vec!["Skyrim.esm".to_owned(), "Update.esm".to_owned()],
                light: false,
            },
            PluginSpec {
                name: "Dragonborn.esm".to_owned(),
                masters: vec!["Skyrim.esm".to_owned(), "Update.esm".to_owned()],
                light: false,
            },
        ])
        .unwrap();
        // Dragonborn and Dawnguard both author their own records at index 2
        // (their master lists are two long) while their slots are 4 and 2.
        assert_eq!(
            layout.remap("dragonborn.esm", 0x02_01B1BE),
            Some(0x04_01B1BE)
        );
        assert_eq!(
            layout.remap("dawnguard.esm", 0x02_000123),
            Some(0x02_000123)
        );
        // A master's own record keeps the master's slot.
        assert_eq!(
            layout.remap("dragonborn.esm", 0x00_00003C),
            Some(0x00_00003C)
        );
        assert_eq!(
            layout.remap("dragonborn.esm", 0x01_000001),
            Some(0x01_000001)
        );
        // A plugin without masters owns every index, including the identity.
        assert_eq!(layout.remap("skyrim.esm", 0x0200_0123), Some(0x0000_0123));
        // Unknown owners and unloaded masters cannot be resolved.
        assert_eq!(layout.remap("missing.esp", 0x0000_0001), None);
        assert_eq!(layout.remap("skyrim.esm", 0), None);
        let unloaded = PluginLayout::from_specs(&[PluginSpec {
            name: "mod.esp".to_owned(),
            masters: vec!["Skyrim.esm".to_owned(), "missing.esm".to_owned()],
            light: false,
        }])
        .unwrap();
        assert_eq!(unloaded.remap("mod.esp", 0x00_000001), None);
        assert_eq!(unloaded.remap("mod.esp", 0x01_000001), None);
        let light = PluginLayout::from_specs(&[
            PluginSpec {
                name: "Skyrim.esm".to_owned(),
                masters: Vec::new(),
                light: false,
            },
            PluginSpec {
                name: "patch.esl".to_owned(),
                masters: vec!["Skyrim.esm".to_owned()],
                light: true,
            },
        ])
        .unwrap();
        assert_eq!(light.remap("patch.esl", 0x01_0000AB), Some(0xFE00_00AB));
    }

    #[test]
    fn reads_master_lists_from_plugin_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (name, masters) in [
            ("Skyrim.esm", vec![]),
            ("Update.esm", vec!["Skyrim.esm"]),
            ("Dawnguard.esm", vec!["Skyrim.esm", "Update.esm"]),
            ("HearthFires.esm", vec!["Skyrim.esm", "Update.esm"]),
            ("Dragonborn.esm", vec!["Skyrim.esm", "Update.esm"]),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, plugin_bytes(&masters)).unwrap();
            paths.push(path);
        }
        let layout = PluginLayout::from_plugins(&paths).unwrap();
        // The vanilla load order stores Dragonborn's references at 0x04 while
        // its tree LOD was authored at 0x02.
        assert_eq!(
            layout.remap("dragonborn.esm", 0x02_01B1BE),
            Some(0x04_01B1BE)
        );
        assert_eq!(layout.remap("skyrim.esm", 0x02_01B1BE), Some(0x00_01B1BE));
    }

    #[test]
    fn builds_two_crossed_quads_sized_by_the_lst_entry() {
        let tree = tree_type([200.0, 800.0], [0.25, 0.125, 0.5, 0.625]);
        let uri = "../../../../textures/terrain/tamriel/trees/tamrieltreelod.opensky-srgb.ktx2";
        let bytes = build_tree_billboard_glb(&tree, uri).unwrap();
        let document = glb_json(&bytes);
        assert_eq!(
            document["accessors"][0]["count"].as_u64(),
            Some(BILLBOARD_VERTICES as u64)
        );
        assert_eq!(document["accessors"][0]["type"].as_str(), Some("VEC3"));
        assert_eq!(
            document["accessors"][3]["count"].as_u64(),
            Some(BILLBOARD_INDICES as u64)
        );
        assert_eq!(
            document["accessors"][3]["componentType"].as_u64(),
            Some(5123)
        );
        assert_eq!(document["materials"][0]["alphaMode"].as_str(), Some("MASK"));
        assert_eq!(
            document["materials"][0]["doubleSided"].as_bool(),
            Some(true)
        );
        assert_eq!(document["images"][0]["uri"].as_str(), Some(uri));
        // Bounds are required for POSITION and are what the block spawner
        // would read; runtime Y is up, so the quads are 800 tall.
        let bounds = |key: &str| {
            document["accessors"][0][key]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_f64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(bounds("min"), vec![-100.0, 0.0, -100.0]);
        assert_eq!(bounds("max"), vec![100.0, 800.0, 100.0]);

        let positions = accessor_floats(&bytes, 0, 3);
        let uv = accessor_floats(&bytes, 2, 2);
        // Quad A spans runtime X and Y at z = 0; quad B spans runtime Y and Z
        // at x = 0, so the two cross at 90 degrees.
        for vertex in 0..4 {
            assert!(positions[vertex * 3 + 2].abs() < 1.0e-4, "quad A is flat");
            assert!((positions[vertex * 3].abs() - 100.0).abs() < 1.0e-4);
        }
        for vertex in 4..BILLBOARD_VERTICES {
            assert!(positions[vertex * 3].abs() < 1.0e-4, "quad B is flat");
            assert!((positions[vertex * 3 + 2].abs() - 100.0).abs() < 1.0e-4);
        }
        for vertex in 0..BILLBOARD_VERTICES {
            assert!(positions[vertex * 3 + 1] <= 800.0);
        }
        // The atlas rectangle is baked into the UVs, `v` measured from the top.
        let us: Vec<f32> = uv.iter().step_by(2).copied().collect();
        let vs: Vec<f32> = uv.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(us.iter().cloned().fold(f32::INFINITY, f32::min), 0.25);
        assert_eq!(us.iter().cloned().fold(f32::NEG_INFINITY, f32::max), 0.5);
        assert_eq!(vs.iter().cloned().fold(f32::INFINITY, f32::min), 0.125);
        assert_eq!(vs.iter().cloned().fold(f32::NEG_INFINITY, f32::max), 0.625);
    }

    /// A staging tree with one worldspace, one terrain block, one object block,
    /// two tree types and three instances (two resolvable, one stale).
    fn fixture_staging(directory: &Path) -> PathBuf {
        let staging = directory.join("assets");
        let vfs = staging.join("vfs");
        fs::create_dir_all(&staging).unwrap();
        let connection = Connection::open(staging.join("skyrim_world.db")).unwrap();
        create_tables(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO plugins(id, name, priority, checksum) VALUES(0, 'Skyrim.esm', 0, x'00');
                 INSERT INTO worldspaces(id, editor_id, parent_world, flags) VALUES(60, 'Tamriel', NULL, 0);
                 INSERT INTO records(form_id, record_type, cell_id, worldspace_id, load_order, data) VALUES(60, 'WRLD', NULL, NULL, 0, x'00');
                 INSERT INTO \"references\"(id, cell_id, worldspace_id, base_form_id, is_exterior, pos_x, pos_y, pos_z, rot_x, rot_y, rot_z, scale) VALUES(18, 10, 60, 20, 1, 100.0, 200.0, 30.0, 0, 0, 0, 1.0);
                 INSERT INTO \"references\"(id, cell_id, worldspace_id, base_form_id, is_exterior, pos_x, pos_y, pos_z, rot_x, rot_y, rot_z, scale) VALUES(34, 10, 60, 20, 1, 400.0, 500.0, 60.0, 0, 0, 0, 1.0);",
            )
            .unwrap();
        drop(connection);
        write(
            &vfs.join("lodsettings/tamriel.lod"),
            &lod_header([-96, -96], 256, [4, 32]),
        );
        write(&vfs.join("meshes/terrain/tamriel/tamriel.4.0.0.btr"), b"b");
        write(
            &vfs.join("meshes/terrain/tamriel/objects/tamriel.4.0.0.bto"),
            b"o",
        );
        // Any valid GLB stands in for a converted block; its bounds are read
        // back from the file, so its size is this fixture's contract.
        let quad = tree_type([4096.0, 100.0], [0.0, 0.0, 1.0, 1.0]);
        let block = build_tree_billboard_glb(&quad, "atlas").unwrap();
        write(
            &staging.join("meshes/terrain/tamriel/tamriel.4.0.0.glb"),
            &block,
        );
        write(
            &staging.join("meshes/terrain/tamriel/objects/tamriel.4.0.0.glb"),
            &block,
        );
        write(
            &vfs.join("textures/terrain/tamriel/trees/tamrieltreelod.dds"),
            b"DDS ",
        );
        write(
            &vfs.join("meshes/terrain/tamriel/trees/tamriel.lst"),
            &lst_table(&[
                lst_entry(0, [128.0, 256.0], [0.0, 0.0, 0.25, 0.5]),
                lst_entry(1, [64.0, 512.0], [0.25, 0.5, 0.5, 1.0]),
            ]),
        );
        write(
            &vfs.join("meshes/terrain/tamriel/trees/tamriel.4.0.0.btt"),
            &btt_block(
                &[(
                    1,
                    vec![
                        btt_instance([100.0, 200.0, 30.0], 0.5, 1.0, 0x12),
                        btt_instance([400.0, 500.0, 60.0], 1.0, 0.5, 0x0200_0022),
                        btt_instance([700.0, 800.0, 90.0], 1.5, 1.25, 0x99),
                    ],
                )],
                &[],
            ),
        );
        write(&directory.join("Skyrim.esm"), &plugin_bytes(&[]));
        staging
    }

    #[test]
    fn records_the_inventory_of_a_fixture_asset_set() {
        let directory = tempfile::tempdir().unwrap();
        let staging = fixture_staging(directory.path());
        let plugins = [directory.path().join("Skyrim.esm")];
        let report = record_lod_inventory(&staging, &plugins).unwrap();
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(
            (
                report.worldspaces,
                report.grids,
                report.terrain_blocks,
                report.object_blocks,
                report.tree_types,
                report.tree_instances,
                report.tree_instances_unresolved,
            ),
            (1, 1, 1, 1, 2, 3, 1)
        );
        assert_eq!(report.billboards.len(), 2);

        // The billboard references the atlas by its sRGB alias, relative to
        // the mesh's own directory.
        let billboard = staging.join("meshes/terrain/tamriel/trees/tamriel.tree.1.glb");
        assert!(billboard.is_file());
        assert_eq!(
            MeshConverter::glb_texture_uris(&billboard).unwrap(),
            vec![
                "../../../../textures/terrain/tamriel/trees/tamrieltreelod.opensky-srgb.ktx2"
                    .to_owned()
            ]
        );

        let connection = Connection::open(staging.join("skyrim_world.db")).unwrap();
        let grid: (i16, i16, String) = connection
            .query_row(
                "SELECT origin_x, origin_y, levels FROM lod_grid WHERE worldspace_id = 60",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(grid, (-96, -96, "4,8,16,32".to_owned()));
        let terrain: (String, i32, i32, i32, String) = connection
            .query_row(
                "SELECT kind, level, block_x, block_y, mesh_path FROM lod_block WHERE kind = 'terrain'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            terrain,
            (
                "terrain".to_owned(),
                4,
                0,
                0,
                "meshes/terrain/tamriel/tamriel.4.0.0.glb".to_owned()
            )
        );
        let objects: String = connection
            .query_row(
                "SELECT mesh_path FROM lod_block WHERE kind = 'objects'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(objects, "meshes/terrain/tamriel/objects/tamriel.4.0.0.glb");
        let bounds: (f32, f32, f32, f32, f32, f32) = connection
            .query_row(
                "SELECT bounds_min_x, bounds_min_y, bounds_min_z, bounds_max_x, bounds_max_y, \
                 bounds_max_z FROM lod_block WHERE kind = 'terrain'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(bounds, (-2048.0, 0.0, -2048.0, 2048.0, 100.0, 2048.0));
        let tree: (String, f32, f32, f32, f32) = connection
            .query_row(
                "SELECT mesh_path, size_x, size_y, u0, v1 FROM lod_tree_type WHERE tree_index = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            tree,
            (
                "meshes/terrain/tamriel/trees/tamriel.tree.1.glb".to_owned(),
                64.0,
                512.0,
                0.25,
                1.0
            )
        );
        let instances: Vec<(i32, i32, i64, f32, f32, f32)> = {
            let mut statement = connection
                .prepare(
                    "SELECT block_x, block_y, tree_index, pos_x, pos_z, scale FROM lod_tree_instance \
                     ORDER BY pos_x",
                )
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            instances,
            vec![
                (0, 0, 1, 100.0, 30.0, 1.0),
                (0, 0, 1, 400.0, 60.0, 0.5),
                (0, 0, 1, 700.0, 90.0, 1.25),
            ]
        );

        // A second pass replaces the rows instead of duplicating them.
        let repeated = record_lod_inventory(&staging, &plugins).unwrap();
        assert_eq!(repeated.tree_instances, 3);
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM lod_tree_instance", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 3);
    }

    #[test]
    fn skips_blocks_without_a_converted_mesh_and_reports_them() {
        let directory = tempfile::tempdir().unwrap();
        let staging = fixture_staging(directory.path());
        fs::remove_file(staging.join("meshes/terrain/tamriel/tamriel.4.0.0.glb")).unwrap();
        let report = record_lod_inventory(&staging, &[]).unwrap();
        assert_eq!(report.terrain_blocks, 0);
        assert_eq!(report.missing_blocks, 1);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("tamriel.4.0.0.glb")),
            "{:?}",
            report.errors
        );
        let connection = Connection::open(staging.join("skyrim_world.db")).unwrap();
        let rows: i64 = connection
            .query_row(
                "SELECT count(*) FROM lod_block WHERE kind = 'terrain'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "a block without a mesh must not be recorded");
    }

    #[test]
    fn records_blocks_without_bounds_as_unbounded() {
        let directory = tempfile::tempdir().unwrap();
        let staging = fixture_staging(directory.path());
        // Valid glTF whose POSITION accessor carries no bounds: the converter
        // could not measure the block, but the block still exists.
        let glb = encode_glb(
            &json!({
                "asset": { "version": "2.0" },
                "scene": 0,
                "scenes": [{ "nodes": [0] }],
                "nodes": [{ "mesh": 0 }],
                "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 } }] }],
                "accessors": [{ "componentType": 5126, "count": 1, "type": "VEC3" }],
                "bufferViews": [{ "buffer": 0, "byteLength": 12 }],
                "buffers": [{ "byteLength": 12 }],
            }),
            &[0u8; 12],
        )
        .unwrap();
        fs::write(
            staging.join("meshes/terrain/tamriel/tamriel.4.0.0.glb"),
            glb,
        )
        .unwrap();
        let report = record_lod_inventory(&staging, &[]).unwrap();
        assert_eq!(report.terrain_blocks, 1);
        assert_eq!(report.missing_blocks, 0);
        assert_eq!(report.unbounded_blocks, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let connection = Connection::open(staging.join("skyrim_world.db")).unwrap();
        let bounds: (Option<f32>, Option<f32>) = connection
            .query_row(
                "SELECT bounds_min_x, bounds_max_z FROM lod_block WHERE kind = 'terrain'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(bounds, (None, None));
    }

    #[test]
    fn reports_lod_assets_without_a_worldspace_and_without_a_database() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = directory.path().join("vfs");
        write(
            &vfs.join("lodsettings/unknown.lod"),
            &lod_header([0, 0], 32, [4, 32]),
        );
        assert_eq!(
            record_lod_inventory(directory.path(), &[])
                .unwrap()
                .worldspaces,
            0,
            "without a database there is nothing to inventory"
        );
        let connection = Connection::open(directory.path().join("skyrim_world.db")).unwrap();
        create_tables(&connection).unwrap();
        drop(connection);
        let report = record_lod_inventory(directory.path(), &[]).unwrap();
        assert_eq!(report.worldspaces, 0);
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].contains("unknown"), "{:?}", report.issues);
    }

    #[test]
    fn records_runtime_unaligned_paths_relative_to_the_mesh() {
        assert_eq!(
            runtime_texture_uri(
                Path::new("meshes/terrain/tamriel/trees/tamriel.tree.0.glb"),
                "textures/terrain/tamriel/trees/tamrieltreelod.ktx2"
            ),
            "../../../../textures/terrain/tamriel/trees/tamrieltreelod.ktx2"
        );
        assert_eq!(
            runtime_texture_uri(Path::new("meshes/a.glb"), "textures/a.ktx2"),
            "../textures/a.ktx2"
        );
    }
}
