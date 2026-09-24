//! Deterministic NIF static-shape fixtures for Skyrim SE (`20.2.0.7`).
//!
//! The writer emits the minimal block set the converter renders:
//! `BSFadeNode` → `BSTriShape` → `BSLightingShaderProperty` → `BSShaderTextureSet`.
//! [`terrain_lod`] and [`object_lod`] emit the distant-LOD containers Skyrim
//! ships as `.btr`/`.bto`: a `BSMultiBoundNode` (with its `BSMultiBound` +
//! `BSMultiBoundAABB`) holding an optionally nested shape. LOD geometry can
//! carry per-vertex tint, so both writers emit vertex colours (the shipped
//! object blocks use them; the shipped terrain blocks do not).
//! Geometry is validated before serialization and the output is byte-stable
//! for identical input.

use crate::bytes::{push_u16, push_u32, push_u64};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};

const NIF_VERSION: u32 = 0x1402_0007;
const USER_VERSION: u32 = 12;
const BETHESDA_VERSION: u32 = 100;
const NULL_REF: u32 = u32::MAX;
const SHADER_TYPE_DEFAULT: u32 = 0;
const VERTEX_FLAGS: u16 = 0x0001 | 0x0002 | 0x0008;
const VERTEX_COLOR_FLAG: u16 = 0x0020;
const VERTEX_STRIDE: u8 = 6;
/// Offsets in the vertex descriptor are counted in four-byte units.
const UV_OFFSET: u64 = 4;
const NORMAL_OFFSET: u64 = 5;
const COLOR_OFFSET: u64 = 6;
/// Culling mode stored in a `BSMultiBoundNode` (a `SkyrimLayer` value).
const CULLING_MODE: u32 = 3;
/// Bytes a shape block writes before its vertex and triangle data: `NiAVObject`,
/// the bounds, the raw property references, the vertex descriptor and the three
/// payload counts.
const SHAPE_BLOCK_HEADER: usize = 116;

/// Block indices of the static (`.nif`) layout.
const STATIC_SHADER_BLOCK: u32 = 2;
const STATIC_TEXTURE_SET_BLOCK: u32 = 3;

/// Block indices and node names of the terrain (`.btr`) layout.
const TERRAIN_SHADER_BLOCK: u32 = 2;
const TERRAIN_TEXTURE_SET_BLOCK: u32 = 3;
const TERRAIN_BOUND_BLOCK: u32 = 4;
const TERRAIN_AABB_BLOCK: u32 = 5;
const TERRAIN_ROOT_STRING: u32 = 1;
const TERRAIN_ROOT_NAME: &str = "TerrainLodBlock";

/// Block indices and node names of the object (`.bto`) layout.
const OBJECT_SHADER_BLOCK: u32 = 3;
const OBJECT_TEXTURE_SET_BLOCK: u32 = 4;
const OBJECT_BOUND_BLOCK: u32 = 5;
const OBJECT_AABB_BLOCK: u32 = 6;
const OBJECT_ROOT_STRING: u32 = 1;
const OBJECT_BOUND_STRING: u32 = 2;
const OBJECT_ROOT_NAME: &str = "ObjectLodRoot";
const OBJECT_BOUND_NAME: &str = "ObjectLodBlock";
/// `KeyType::QuadraticKey`: keys carry a forward and a backward tangent.
const KEY_TYPE_QUADRATIC: u32 = 2;

/// A triangle mesh rendered as a single static shape.
#[derive(Debug, Clone, PartialEq)]
pub struct StaticShape<'a> {
    /// Shape name stored in the NIF string table.
    pub name: &'a str,
    /// Vertex positions.
    pub positions: &'a [[f32; 3]],
    /// Per-vertex normals; must match `positions`.
    pub normals: &'a [[f32; 3]],
    /// Per-vertex UVs; must match `positions`.
    pub uvs: &'a [[f32; 2]],
    /// Triangle indices into `positions`.
    pub indices: &'a [[u16; 3]],
    /// Diffuse texture path, for example `textures/generated_color.dds`.
    pub diffuse: &'a str,
    /// Normal texture path.
    pub normal_texture: &'a str,
}

/// Generates a minimal static NIF containing one triangle mesh.
pub fn static_shape(shape: &StaticShape<'_>) -> Result<Vec<u8>> {
    validate(shape)?;
    let strings = [shape.name];
    let blocks = [
        fade_node(),
        write_shape_block(&geometry(shape), STATIC_SHADER_BLOCK)?,
        lighting_shader_property(STATIC_TEXTURE_SET_BLOCK),
        texture_set(shape.diffuse, shape.normal_texture)?,
    ];
    write_nif(
        &[
            "BSFadeNode",
            "BSTriShape",
            "BSLightingShaderProperty",
            "BSShaderTextureSet",
        ],
        &blocks,
        &strings,
    )
}

/// Geometry shared by the static and LOD shape writers.
struct Geometry<'a> {
    positions: &'a [[f32; 3]],
    normals: &'a [[f32; 3]],
    uvs: &'a [[f32; 2]],
    indices: &'a [[u16; 3]],
    /// Per-vertex RGBA colours, written only when present.
    colors: Option<&'a [[u8; 4]]>,
}

/// A distant-LOD container mesh rendered as a single shape with vertex colours.
///
/// Shipped object LOD blocks carry per-vertex tint; the terrain blocks do not,
/// but the converter must preserve the attribute wherever it appears, so unlike
/// [`StaticShape`] every LOD fixture carries it.
#[derive(Debug, Clone, PartialEq)]
pub struct LodShape<'a> {
    /// Shape name stored in the NIF string table.
    pub name: &'a str,
    /// Vertex positions.
    pub positions: &'a [[f32; 3]],
    /// Per-vertex normals; must match `positions`.
    pub normals: &'a [[f32; 3]],
    /// Per-vertex UVs; must match `positions`.
    pub uvs: &'a [[f32; 2]],
    /// Triangle indices into `positions`.
    pub indices: &'a [[u16; 3]],
    /// Per-vertex RGBA colours; must match `positions`.
    pub colors: &'a [[u8; 4]],
    /// Diffuse or atlas texture path, for example `textures/terrain/generated.dds`.
    pub diffuse: &'a str,
    /// Normal texture path.
    pub normal_texture: &'a str,
}

/// Generates a `.btr`-shaped terrain LOD container: one `BSMultiBoundNode` root
/// holding a `BSTriShape`, with a `BSMultiBound` → `BSMultiBoundAABB` pair
/// describing the block's bounding volume.
pub fn terrain_lod(shape: &LodShape<'_>) -> Result<Vec<u8>> {
    validate_lod(shape)?;
    let strings = [shape.name, TERRAIN_ROOT_NAME];
    let (center, extent) = center_and_extent(shape.positions);
    let blocks = [
        multi_bound_node(TERRAIN_ROOT_STRING, 1, TERRAIN_BOUND_BLOCK, CULLING_MODE),
        write_lod_shape_block(&lod_geometry(shape), TERRAIN_SHADER_BLOCK)?,
        lighting_shader_property(TERRAIN_TEXTURE_SET_BLOCK),
        texture_set(shape.diffuse, shape.normal_texture)?,
        multi_bound(TERRAIN_AABB_BLOCK),
        multi_bound_aabb(center, extent),
    ];
    write_nif(
        &[
            "BSMultiBoundNode",
            "BSTriShape",
            "BSLightingShaderProperty",
            "BSShaderTextureSet",
            "BSMultiBound",
            "BSMultiBoundAABB",
        ],
        &blocks,
        &strings,
    )
}

/// Generates a `.bto`-shaped object LOD container: an `NiNode` root holding a
/// `BSMultiBoundNode` that owns a `BSSubIndexTriShape`, with the same
/// `BSMultiBound` → `BSMultiBoundAABB` pair as the terrain container.
pub fn object_lod(shape: &LodShape<'_>) -> Result<Vec<u8>> {
    validate_lod(shape)?;
    let strings = [shape.name, OBJECT_ROOT_NAME, OBJECT_BOUND_NAME];
    let (center, extent) = center_and_extent(shape.positions);
    let blocks = [
        child_node(OBJECT_ROOT_STRING, 1),
        multi_bound_node(OBJECT_BOUND_STRING, 2, OBJECT_BOUND_BLOCK, CULLING_MODE),
        sub_index_tri_shape(&lod_geometry(shape), OBJECT_SHADER_BLOCK)?,
        lighting_shader_property(OBJECT_TEXTURE_SET_BLOCK),
        texture_set(shape.diffuse, shape.normal_texture)?,
        multi_bound(OBJECT_AABB_BLOCK),
        multi_bound_aabb(center, extent),
    ];
    write_nif(
        &[
            "NiNode",
            "BSMultiBoundNode",
            "BSSubIndexTriShape",
            "BSLightingShaderProperty",
            "BSShaderTextureSet",
            "BSMultiBound",
            "BSMultiBoundAABB",
        ],
        &blocks,
        &strings,
    )
}

/// Generates a `.bto`-shaped object LOD container whose `BSSubIndexTriShape`
/// carries an explicit per-cell segment table instead of [`object_lod`]'s
/// single segment covering the whole mesh.
///
/// `segment_counts[i]` is cell `i`'s triangle count (`i` in `0..segment_counts.len()`,
/// which must be at most 16); a `0` entry is an empty cell, exactly like a
/// shipped block where a cell in the middle of the table holds nothing. The
/// counts must sum to `shape.indices.len()`, and `shape.indices` must already
/// be grouped by segment in that order: cell 0's triangles first, then cell
/// 1's, and so on, matching how LODGen and the real segment tables are laid
/// out (`local/research/lod-hiding-under-loaded-cells.md`).
pub fn object_lod_segmented(shape: &LodShape<'_>, segment_counts: &[u32]) -> Result<Vec<u8>> {
    validate_lod(shape)?;
    ensure!(
        segment_counts.len() <= 16,
        "NIF segment table has {} entries, more than a level-4 block's 16 cells",
        segment_counts.len()
    );
    let total: u64 = segment_counts.iter().map(|&count| u64::from(count)).sum();
    ensure!(
        total == shape.indices.len() as u64,
        "segment triangle counts ({total}) do not sum to the shape's triangle count ({})",
        shape.indices.len()
    );
    let strings = [shape.name, OBJECT_ROOT_NAME, OBJECT_BOUND_NAME];
    let (center, extent) = center_and_extent(shape.positions);
    let blocks = [
        child_node(OBJECT_ROOT_STRING, 1),
        multi_bound_node(OBJECT_BOUND_STRING, 2, OBJECT_BOUND_BLOCK, CULLING_MODE),
        sub_index_tri_shape_with_segments(
            &lod_geometry(shape),
            OBJECT_SHADER_BLOCK,
            segment_counts,
        )?,
        lighting_shader_property(OBJECT_TEXTURE_SET_BLOCK),
        texture_set(shape.diffuse, shape.normal_texture)?,
        multi_bound(OBJECT_AABB_BLOCK),
        multi_bound_aabb(center, extent),
    ];
    write_nif(
        &[
            "NiNode",
            "BSMultiBoundNode",
            "BSSubIndexTriShape",
            "BSLightingShaderProperty",
            "BSShaderTextureSet",
            "BSMultiBound",
            "BSMultiBoundAABB",
        ],
        &blocks,
        &strings,
    )
}

fn validate_lod(shape: &LodShape<'_>) -> Result<()> {
    ensure!(!shape.name.is_empty(), "NIF shape name is empty");
    ensure!(
        shape.name.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
        "NIF shape name is not printable ASCII: {:?}",
        shape.name
    );
    validate_geometry(&lod_geometry(shape))?;
    ensure!(
        !shape.diffuse.is_empty(),
        "NIF shape needs a diffuse texture"
    );
    for texture in [shape.diffuse, shape.normal_texture] {
        ensure!(
            texture.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
            "NIF texture path is not printable ASCII: {texture:?}"
        );
    }
    for name in [TERRAIN_ROOT_NAME, OBJECT_ROOT_NAME, OBJECT_BOUND_NAME] {
        ensure!(
            name.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
            "NIF block name is not printable ASCII: {name:?}"
        );
    }
    Ok(())
}

fn lod_geometry<'a>(shape: &LodShape<'a>) -> Geometry<'a> {
    Geometry {
        positions: shape.positions,
        normals: shape.normals,
        uvs: shape.uvs,
        indices: shape.indices,
        colors: Some(shape.colors),
    }
}

/// One shader float controller of a fixture effect shape.
///
/// One entry becomes a `NiTimeController` → `NiFloatInterpolator` →
/// `NiFloatData` triple, chained through `next_controller` in list order.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatController<'a> {
    /// `NiTimeController` flags; bits 1-2 are the cycle mode.
    pub flags: u16,
    pub frequency: f32,
    pub phase: f32,
    pub start_time: f32,
    pub stop_time: f32,
    /// Controlled variable index, as `EffectShaderControlledVariable` numbers it.
    pub variable: u32,
    /// Key type as `KeyType` numbers it: `1` linear, `2` quadratic, `5` constant.
    pub key_type: u32,
    /// `(time, value, forward tangent, backward tangent)` per key. The tangents
    /// are written only for the quadratic key type.
    pub keys: &'a [[f32; 4]],
}

/// Generates a minimal effect-shader NIF whose shader property is animated.
///
/// The block set is `BSFadeNode` → `BSTriShape` → `BSEffectShaderProperty`,
/// followed by one interpolator, one float data and one controller block per
/// entry in `controllers`, in that order. The shader property's controller
/// reference points at the first controller; each controller's `target` points
/// back at the shader property and its `next_controller` at the following one.
pub fn effect_shape_with_controllers(
    shape: &StaticShape<'_>,
    controllers: &[FloatController<'_>],
) -> Result<Vec<u8>> {
    validate(shape)?;
    let mut blocks = vec![
        fade_node(),
        triangle_shape_with_shader(shape, 2)?,
        effect_shader_property(shape, controller_block_index(0))?,
    ];
    for (index, controller) in controllers.iter().enumerate() {
        ensure!(
            !controller.keys.is_empty(),
            "NIF float controller {index} has no keys"
        );
        let interpolator = interpolator_block_index(index);
        let next_controller = if index + 1 < controllers.len() {
            controller_block_index(index + 1)
        } else {
            NULL_REF
        };
        blocks.push(float_interpolator(interpolator + 1));
        blocks.push(float_data(controller)?);
        blocks.push(float_controller(controller, interpolator, next_controller));
    }
    let mut block_types = vec![
        "BSFadeNode".to_owned(),
        "BSTriShape".to_owned(),
        "BSEffectShaderProperty".to_owned(),
    ];
    for _ in controllers {
        block_types.extend(
            [
                "NiFloatInterpolator",
                "NiFloatData",
                "BSEffectShaderPropertyFloatController",
            ]
            .map(str::to_owned),
        );
    }
    let block_types = block_types.iter().map(String::as_str).collect::<Vec<_>>();
    write_nif(&block_types, &blocks, &[shape.name])
}

/// Block indices of one controller's triple: the interpolator at block 3, its
/// float data at 4 and the controller itself at 5, then three blocks per
/// further controller.
fn interpolator_block_index(index: usize) -> u32 {
    u32::try_from(3 + index * 3).unwrap_or(NULL_REF)
}

fn controller_block_index(index: usize) -> u32 {
    interpolator_block_index(index).wrapping_add(2)
}

/// Writes an effect shape's `BSTriShape` with an explicit shader-property
/// reference, on top of the shared [`write_shape_block`] writer rather than
/// duplicating it.
fn triangle_shape_with_shader(shape: &StaticShape<'_>, shader_property: u32) -> Result<Vec<u8>> {
    write_shape_block(&geometry(shape), shader_property)
}

/// Serializes a NIF container around already-encoded blocks.
fn write_nif(block_types: &[&str], blocks: &[Vec<u8>], strings: &[&str]) -> Result<Vec<u8>> {
    ensure!(
        block_types.len() == blocks.len(),
        "NIF has {} block types for {} blocks",
        block_types.len(),
        blocks.len()
    );
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"Gamebryo File Format, Version 20.2.0.7\n");
    push_u32(&mut bytes, NIF_VERSION);
    bytes.push(1);
    push_u32(&mut bytes, USER_VERSION);
    push_u32(
        &mut bytes,
        u32::try_from(blocks.len()).map_err(|_| eyre!("NIF block count overflow"))?,
    );
    push_u32(&mut bytes, BETHESDA_VERSION);
    push_string8(&mut bytes, "OpenSkyrim dummy-content");
    push_string8(&mut bytes, "");
    push_string8(&mut bytes, "");
    push_u16(
        &mut bytes,
        u16::try_from(block_types.len()).map_err(|_| eyre!("NIF block type overflow"))?,
    );
    for block_type in block_types {
        push_u32(
            &mut bytes,
            u32::try_from(block_type.len()).map_err(|_| eyre!("NIF block type overflow"))?,
        );
        bytes.extend_from_slice(block_type.as_bytes());
    }
    for index in 0..blocks.len() {
        push_u16(
            &mut bytes,
            u16::try_from(index).map_err(|_| eyre!("NIF block index overflow"))?,
        );
    }
    for block in blocks {
        push_u32(
            &mut bytes,
            u32::try_from(block.len()).map_err(|_| eyre!("NIF block size overflow"))?,
        );
    }
    push_u32(
        &mut bytes,
        u32::try_from(strings.len()).map_err(|_| eyre!("NIF string count overflow"))?,
    );
    push_u32(&mut bytes, max_string_length(strings));
    for value in strings {
        push_u32(
            &mut bytes,
            u32::try_from(value.len()).map_err(|_| eyre!("NIF string overflow"))?,
        );
        bytes.extend_from_slice(value.as_bytes());
    }
    push_u32(&mut bytes, 0);
    for block in blocks {
        bytes.extend_from_slice(block);
    }
    Ok(bytes)
}

fn validate(shape: &StaticShape<'_>) -> Result<()> {
    ensure!(!shape.name.is_empty(), "NIF shape name is empty");
    ensure!(
        shape.name.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
        "NIF shape name is not printable ASCII: {:?}",
        shape.name
    );
    validate_geometry(&geometry(shape))?;
    ensure!(
        !shape.diffuse.is_empty(),
        "NIF shape needs a diffuse texture"
    );
    for texture in [shape.diffuse, shape.normal_texture] {
        ensure!(
            texture.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
            "NIF texture path is not printable ASCII: {texture:?}"
        );
    }
    Ok(())
}

fn geometry<'a>(shape: &StaticShape<'a>) -> Geometry<'a> {
    Geometry {
        positions: shape.positions,
        normals: shape.normals,
        uvs: shape.uvs,
        indices: shape.indices,
        colors: None,
    }
}

fn validate_geometry(geometry: &Geometry<'_>) -> Result<()> {
    ensure!(!geometry.positions.is_empty(), "NIF shape has no positions");
    ensure!(
        geometry.positions.len() == geometry.normals.len(),
        "NIF shape has {} positions and {} normals",
        geometry.positions.len(),
        geometry.normals.len()
    );
    ensure!(
        geometry.positions.len() == geometry.uvs.len(),
        "NIF shape has {} positions and {} UVs",
        geometry.positions.len(),
        geometry.uvs.len()
    );
    ensure!(
        geometry.positions.len() <= u16::MAX as usize,
        "NIF shape exceeds 65535 vertices"
    );
    ensure!(
        geometry.indices.len() <= u16::MAX as usize,
        "NIF shape exceeds 65535 triangles"
    );
    ensure!(!geometry.indices.is_empty(), "NIF shape has no triangles");
    ensure!(
        geometry
            .positions
            .iter()
            .all(|position| position.iter().all(|value| value.is_finite())),
        "NIF shape contains a non-finite position"
    );
    ensure!(
        geometry
            .uvs
            .iter()
            .all(|uv| uv.iter().all(|value| value.is_finite())),
        "NIF shape contains a non-finite UV"
    );
    let vertex_count = geometry.positions.len();
    ensure!(
        geometry
            .indices
            .iter()
            .flatten()
            .all(|index| { usize::from(*index) < vertex_count }),
        "NIF shape contains an out-of-range triangle index"
    );
    if let Some(colors) = geometry.colors {
        ensure!(
            colors.len() == vertex_count,
            "NIF shape has {} positions and {} vertex colors",
            vertex_count,
            colors.len()
        );
    }
    Ok(())
}

fn fade_node() -> Vec<u8> {
    let mut block = Vec::with_capacity(84);
    push_av_object(&mut block, NULL_REF);
    push_u32(&mut block, 1);
    push_u32(&mut block, 1);
    push_u32(&mut block, 0);
    block
}

/// Writes an `NiNode` with the given string index and one child.
fn child_node(name: u32, child: u32) -> Vec<u8> {
    let mut block = Vec::with_capacity(88);
    push_av_object(&mut block, name);
    push_u32(&mut block, 1);
    push_u32(&mut block, child);
    push_u32(&mut block, 0);
    block
}

/// Writes a `BSMultiBoundNode`: an `NiNode` followed by the reference to its
/// bounding volume and, for Bethesda version 100 as written here, a culling
/// mode.
fn multi_bound_node(name: u32, child: u32, bound: u32, culling_mode: u32) -> Vec<u8> {
    let mut block = child_node(name, child);
    push_u32(&mut block, bound);
    push_u32(&mut block, culling_mode);
    block
}

/// Writes `BSMultiBound`, which carries only the reference to its bound data.
fn multi_bound(data: u32) -> Vec<u8> {
    let mut block = Vec::with_capacity(4);
    push_u32(&mut block, data);
    block
}

/// Writes `BSMultiBoundAABB`: the volume centre and its per-axis extent.
fn multi_bound_aabb(center: [f32; 3], extent: [f32; 3]) -> Vec<u8> {
    let mut block = Vec::with_capacity(24);
    for value in [
        center[0], center[1], center[2], extent[0], extent[1], extent[2],
    ] {
        block.extend_from_slice(&value.to_le_bytes());
    }
    block
}

/// Writes a distant-LOD shape: the ordinary triangle payload plus the trailing
/// zero `u32` every shipped LOD shape carries. A terrain (`.btr`) shape ends
/// there; an object (`.bto`) shape continues with its segment table.
fn write_lod_shape_block(geometry: &Geometry<'_>, shader_property: u32) -> Result<Vec<u8>> {
    let mut block = write_shape_block(geometry, shader_property)?;
    push_u32(&mut block, 0);
    Ok(block)
}

/// Writes a `BSSubIndexTriShape`: a LOD shape followed by its segment table,
/// which maps each merged object onto a triangle range. Declares a single
/// segment covering the whole mesh, the shape most object LOD blocks take, so
/// its primitive count also adds up to the shape's triangle count.
fn sub_index_tri_shape(geometry: &Geometry<'_>, shader_property: u32) -> Result<Vec<u8>> {
    let count = u32::try_from(geometry.indices.len())
        .map_err(|_| eyre!("NIF segment primitive count overflow"))?;
    sub_index_tri_shape_with_segments(geometry, shader_property, &[count])
}

/// Writes a `BSSubIndexTriShape` whose segment table has one entry per
/// `segment_counts` value, in order (table index `i` holds `segment_counts[i]`
/// triangles). The flag byte and the table's unused middle `u32` are `0`,
/// matching every shipped entry.
fn sub_index_tri_shape_with_segments(
    geometry: &Geometry<'_>,
    shader_property: u32,
    segment_counts: &[u32],
) -> Result<Vec<u8>> {
    let mut block = write_lod_shape_block(geometry, shader_property)?;
    push_u32(
        &mut block,
        u32::try_from(segment_counts.len())
            .map_err(|_| eyre!("NIF segment table entry count overflow"))?,
    );
    for &count in segment_counts {
        block.push(0);
        push_u32(&mut block, 0);
        push_u32(&mut block, count);
    }
    Ok(block)
}

/// The centre and the per-axis size of `positions`.
fn center_and_extent(positions: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for position in positions {
        for (axis, value) in position.iter().enumerate() {
            min[axis] = min[axis].min(*value);
            max[axis] = max[axis].max(*value);
        }
    }
    let mut center = [0.0f32; 3];
    let mut extent = [0.0f32; 3];
    for axis in 0..3 {
        center[axis] = (min[axis] + max[axis]) * 0.5;
        extent[axis] = max[axis] - min[axis];
    }
    (center, extent)
}

fn write_shape_block(geometry: &Geometry<'_>, shader_property: u32) -> Result<Vec<u8>> {
    let vertex_stride = VERTEX_STRIDE + u8::from(geometry.colors.is_some());
    let vertex_bytes = geometry
        .positions
        .len()
        .checked_mul(usize::from(vertex_stride) * 4)
        .ok_or_else(|| eyre!("NIF vertex data overflow"))?;
    let triangle_bytes = geometry
        .indices
        .len()
        .checked_mul(6)
        .ok_or_else(|| eyre!("NIF triangle data overflow"))?;
    let data_size = vertex_bytes
        .checked_add(triangle_bytes)
        .ok_or_else(|| eyre!("NIF geometry size overflow"))?;

    let (center, radius) = bounds(geometry.positions);
    let mut block = Vec::with_capacity(SHAPE_BLOCK_HEADER + data_size);
    push_av_object(&mut block, 0);
    for value in center {
        block.extend_from_slice(&value.to_le_bytes());
    }
    block.extend_from_slice(&radius.to_le_bytes());
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, shader_property);
    push_u32(&mut block, NULL_REF);
    let color_offset = if geometry.colors.is_some() {
        COLOR_OFFSET
    } else {
        0
    };
    let vertex_flags = VERTEX_FLAGS
        | if geometry.colors.is_some() {
            VERTEX_COLOR_FLAG
        } else {
            0
        };
    let descriptor = u64::from(vertex_stride)
        | (UV_OFFSET << 8)
        | (NORMAL_OFFSET << 16)
        | (color_offset << 24)
        | (u64::from(vertex_flags) << 44);
    push_u64(&mut block, descriptor);
    push_u16(
        &mut block,
        u16::try_from(geometry.indices.len()).map_err(|_| eyre!("NIF triangle count overflow"))?,
    );
    push_u16(
        &mut block,
        u16::try_from(geometry.positions.len()).map_err(|_| eyre!("NIF vertex count overflow"))?,
    );
    push_u32(
        &mut block,
        u32::try_from(data_size).map_err(|_| eyre!("NIF geometry size overflow"))?,
    );
    let empty_colors = [[0u8; 4]; 0];
    let colors = geometry.colors.unwrap_or(&empty_colors);
    for (index, (position, (normal, uv))) in geometry
        .positions
        .iter()
        .zip(geometry.normals.iter().zip(geometry.uvs.iter()))
        .enumerate()
    {
        for value in position {
            block.extend_from_slice(&value.to_le_bytes());
        }
        block.extend_from_slice(&0.0f32.to_le_bytes());
        block.extend_from_slice(&encode_half(uv[0]).to_le_bytes());
        block.extend_from_slice(&encode_half(uv[1]).to_le_bytes());
        block.push(pack_normal(normal[0]));
        block.push(pack_normal(normal[1]));
        block.push(pack_normal(normal[2]));
        block.push(0);
        if let Some(color) = colors.get(index) {
            block.extend_from_slice(color);
        }
    }
    for triangle in geometry.indices {
        for index in triangle {
            block.extend_from_slice(&index.to_le_bytes());
        }
    }
    Ok(block)
}

fn lighting_shader_property(texture_set: u32) -> Vec<u8> {
    let mut block = Vec::with_capacity(100);
    push_u32(&mut block, SHADER_TYPE_DEFAULT);
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, 0);
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, 0);
    push_u32(&mut block, 0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_u32(&mut block, texture_set);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 1.0);
    push_u32(&mut block, 0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 80.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 0.3);
    push_f32(&mut block, 2.0);
    debug_assert_eq!(block.len(), 100);
    block
}

/// A `BSEffectShaderProperty` whose controller reference is `controller`.
///
/// Field order is the vendored parser's (`vendor/project-wormhole-nif`):
/// the inherited `NiObjectNET` header, the flags, the UV transform, the source
/// texture, four clamp/lighting bytes, four falloff floats, the base colour,
/// the base colour scale, the soft falloff depth and the greyscale texture.
fn effect_shader_property(shape: &StaticShape<'_>, controller: u32) -> Result<Vec<u8>> {
    let mut block = Vec::with_capacity(96);
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, 0);
    push_u32(&mut block, controller);
    push_u32(&mut block, 0);
    push_u32(&mut block, 0);
    for value in [0.0f32, 0.0, 1.0, 1.0] {
        push_f32(&mut block, value);
    }
    push_sized_string(&mut block, shape.diffuse)?;
    block.extend_from_slice(&[0, 0, 0, 0]);
    for value in [0.0f32, 0.0, 1.0, 0.0] {
        push_f32(&mut block, value);
    }
    for value in [1.0f32, 1.0, 1.0, 1.0, 1.0, 0.0] {
        push_f32(&mut block, value);
    }
    push_sized_string(&mut block, "")?;
    Ok(block)
}

/// An `NiTimeController` → `BSEffectShaderPropertyFloatController` block.
fn float_controller(
    controller: &FloatController<'_>,
    interpolator: u32,
    next_controller: u32,
) -> Vec<u8> {
    let mut block = Vec::with_capacity(40);
    push_u32(&mut block, next_controller);
    push_u16(&mut block, controller.flags);
    for value in [
        controller.frequency,
        controller.phase,
        controller.start_time,
        controller.stop_time,
    ] {
        push_f32(&mut block, value);
    }
    // The controller targets the shader property at block 2.
    push_u32(&mut block, 2);
    push_u32(&mut block, interpolator);
    push_u32(&mut block, controller.variable);
    block
}

fn float_interpolator(data: u32) -> Vec<u8> {
    let mut block = Vec::with_capacity(8);
    push_f32(&mut block, 0.0);
    push_u32(&mut block, data);
    block
}

fn float_data(controller: &FloatController<'_>) -> Result<Vec<u8>> {
    let stride = if controller.key_type == KEY_TYPE_QUADRATIC {
        4
    } else {
        2
    };
    let mut block = Vec::with_capacity(8 + stride * 4 * controller.keys.len());
    push_u32(
        &mut block,
        u32::try_from(controller.keys.len()).map_err(|_| eyre!("NIF key count overflow"))?,
    );
    push_u32(&mut block, controller.key_type);
    for key in controller.keys {
        for value in key.iter().take(stride) {
            push_f32(&mut block, *value);
        }
    }
    Ok(block)
}

fn push_sized_string(out: &mut Vec<u8>, value: &str) -> Result<()> {
    push_u32(
        out,
        u32::try_from(value.len()).map_err(|_| eyre!("NIF string overflow"))?,
    );
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn texture_set(diffuse: &str, normal_texture: &str) -> Result<Vec<u8>> {
    let slots = [diffuse, normal_texture, "", "", "", "", "", "", ""];
    let mut block = Vec::new();
    push_u32(
        &mut block,
        u32::try_from(slots.len()).map_err(|_| eyre!("NIF texture slot overflow"))?,
    );
    for slot in slots {
        push_u32(
            &mut block,
            u32::try_from(slot.len()).map_err(|_| eyre!("NIF texture path overflow"))?,
        );
        block.extend_from_slice(slot.as_bytes());
    }
    Ok(block)
}

fn push_av_object(out: &mut Vec<u8>, name: u32) {
    push_u32(out, name);
    push_u32(out, NULL_REF);
    push_u32(out, NULL_REF);
    push_u32(out, 0);
    for value in [0.0f32, 0.0, 0.0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&1.0f32.to_le_bytes());
    push_u32(out, NULL_REF);
}

fn push_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_string8(out: &mut Vec<u8>, value: &str) {
    out.push((value.len() + 1) as u8);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

fn max_string_length(strings: &[&str]) -> u32 {
    strings
        .iter()
        .map(|value| u32::try_from(value.len()).unwrap_or(u32::MAX))
        .max()
        .unwrap_or(0)
}

fn pack_normal(value: f32) -> u8 {
    let scaled = ((value + 1.0) * 0.5 * 255.0).round();
    scaled.clamp(0.0, 255.0) as u8
}

/// Encodes an `f32` as an IEEE 754 half-precision value (round to nearest).
fn encode_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x007f_ffff;
    if exponent <= 0 {
        if exponent < -10 {
            return sign;
        }
        let mantissa = (mantissa | 0x0080_0000) >> (1 - exponent + 13);
        return sign | mantissa as u16;
    }
    if exponent >= 31 {
        return sign | 0x7c00 | u16::from(mantissa != 0) << 9;
    }
    sign | ((exponent as u16) << 10) | ((mantissa >> 13) as u16)
}

fn bounds(positions: &[[f32; 3]]) -> ([f32; 3], f32) {
    let count = positions.len() as f32;
    let mut center = [0.0f32; 3];
    for position in positions {
        for (axis, value) in position.iter().enumerate() {
            center[axis] += value / count;
        }
    }
    let radius = positions
        .iter()
        .map(|position| {
            let dx = position[0] - center[0];
            let dy = position[1] - center[1];
            let dz = position[2] - center[2];
            (dx * dx + dy * dy + dz * dz).sqrt()
        })
        .fold(0.0f32, f32::max);
    (center, radius)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad() -> StaticShape<'static> {
        StaticShape {
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
        }
    }

    fn lod_quad() -> LodShape<'static> {
        LodShape {
            name: "GeneratedLodQuad",
            positions: &[
                [0.0, 0.0, 0.0],
                [4096.0, 0.0, 0.0],
                [4096.0, 0.0, 4096.0],
                [0.0, 0.0, 4096.0],
            ],
            normals: &[[0.0, 1.0, 0.0]; 4],
            uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
            indices: &[[0, 1, 2], [0, 2, 3]],
            colors: &[
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 255, 128],
            ],
            diffuse: "textures/terrain/generated/generated.dds",
            normal_texture: "textures/terrain/generated/generated_n.dds",
        }
    }

    /// The byte sequence every existing conversion depends on. It was captured
    /// before the distant-LOD writers landed, so this test fails if the static
    /// writer's output changes at all.
    const STATIC_SHAPE_BASELINE_HEX: &str = "47616d656272796f2046696c6520466f726d61742c2056657273696f6e2032302e322e302e370a07000214010c0000000400000064000000194f70656e536b7972696d2064756d6d792d636f6e74656e74000100010004000a0000004253466164654e6f64650a000000425354726953686170651800000042534c69676874696e6753686164657250726f706572747912000000425353686164657254657874757265536574000001000200030054000000e00000006400000061000000010000000d0000000d00000047656e6572617465645175616400000000ffffffffffffffffffffffff000000000000000000000000000000000000803f0000000000000000000000000000803f0000000000000000000000000000803f0000803fffffffff01000000010000000000000000000000ffffffffffffffff000000000000000000000000000000000000803f0000000000000000000000000000803f0000000000000000000000000000803f0000803fffffffff000000000000000000000000f304b53fffffffff02000000ffffffff0604050000b00000020004006c000000000080bf000080bf00000000000000000000003c8080ff000000803f000080bf0000000000000000003c003c8080ff000000803f0000803f0000000000000000003c00008080ff00000080bf0000803f0000000000000000000000008080ff0000000100020000000200030000000000ffffffff00000000ffffffff000000000000000000000000000000000000803f0000803f030000000000000000000000000000000000803f000000000000803f000000000000a0420000803f0000803f0000803f0000803f9a99993e00000040090000001c00000074657874757265732f67656e6572617465645f636f6c6f722e6464731d00000074657874757265732f67656e6572617465645f6e6f726d616c2e64647300000000000000000000000000000000000000000000000000000000";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn static_shape_output_is_unchanged() {
        assert_eq!(
            hex(&static_shape(&quad()).unwrap()),
            STATIC_SHAPE_BASELINE_HEX
        );
    }

    #[test]
    fn writes_lod_container_headers() {
        let line = b"Gamebryo File Format, Version 20.2.0.7\n";
        let terrain = terrain_lod(&lod_quad()).unwrap();
        let object = object_lod(&lod_quad()).unwrap();
        assert_eq!(terrain_lod(&lod_quad()).unwrap(), terrain);
        assert_eq!(object_lod(&lod_quad()).unwrap(), object);

        let cases: [(&Vec<u8>, &[&str]); 2] = [
            (
                &terrain,
                &[
                    "BSMultiBoundNode",
                    "BSTriShape",
                    "BSLightingShaderProperty",
                    "BSShaderTextureSet",
                    "BSMultiBound",
                    "BSMultiBoundAABB",
                ],
            ),
            (
                &object,
                &[
                    "NiNode",
                    "BSMultiBoundNode",
                    "BSSubIndexTriShape",
                    "BSLightingShaderProperty",
                    "BSShaderTextureSet",
                    "BSMultiBound",
                    "BSMultiBoundAABB",
                ],
            ),
        ];
        for (bytes, block_types) in cases {
            assert!(bytes.starts_with(line));
            assert_eq!(
                u32::from_le_bytes(bytes[line.len() + 9..line.len() + 13].try_into().unwrap()),
                block_types.len() as u32
            );
            let text = hex(bytes);
            for block_type in block_types {
                let marker: String = block_type
                    .bytes()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                assert!(text.contains(&marker), "missing block type {block_type}");
            }
        }
    }

    #[test]
    fn rejects_invalid_lod_geometry() {
        let mut shape = lod_quad();
        shape.colors = &[[0, 0, 0, 0]];
        assert!(terrain_lod(&shape).is_err());
        let mut shape = lod_quad();
        shape.name = "";
        assert!(object_lod(&shape).is_err());
        let mut shape = lod_quad();
        shape.diffuse = "textures/terrain/t\u{8}.dds";
        assert!(object_lod(&shape).is_err());
    }

    #[test]
    fn writes_the_expected_header() {
        let bytes = static_shape(&quad()).unwrap();
        let line = b"Gamebryo File Format, Version 20.2.0.7\n";
        assert!(bytes.starts_with(line));
        let version = u32::from_le_bytes(bytes[line.len()..line.len() + 4].try_into().unwrap());
        assert_eq!(version, NIF_VERSION);
        assert_eq!(bytes[line.len() + 4], 1);
        let user = u32::from_le_bytes(bytes[line.len() + 5..line.len() + 9].try_into().unwrap());
        assert_eq!(user, USER_VERSION);
        let blocks = u32::from_le_bytes(bytes[line.len() + 9..line.len() + 13].try_into().unwrap());
        assert_eq!(blocks, 4);
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(
            static_shape(&quad()).unwrap(),
            static_shape(&quad()).unwrap()
        );
    }

    #[test]
    fn rejects_invalid_geometry() {
        let mut shape = quad();
        shape.normals = &[];
        assert!(static_shape(&shape).is_err());
        let mut shape = quad();
        shape.indices = &[[0, 1, 9]];
        assert!(static_shape(&shape).is_err());
        let mut shape = quad();
        shape.diffuse = "";
        assert!(static_shape(&shape).is_err());
    }
}
