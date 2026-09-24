//! Deterministic NIF static-shape fixtures for Skyrim SE (`20.2.0.7`).
//!
//! The writer emits the minimal block set the converter renders:
//! `BSFadeNode` → `BSTriShape` → `BSLightingShaderProperty` → `BSShaderTextureSet`.
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
const VERTEX_STRIDE: u8 = 6;
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
        triangle_shape(shape)?,
        lighting_shader_property(),
        texture_set(shape)?,
    ];
    let block_types = [
        "BSFadeNode",
        "BSTriShape",
        "BSLightingShaderProperty",
        "BSShaderTextureSet",
    ];
    write_file(&blocks, &block_types, &strings)
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
    write_file(&blocks, &block_types, &[shape.name])
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

fn write_file(blocks: &[Vec<u8>], block_types: &[&str], strings: &[&str]) -> Result<Vec<u8>> {
    ensure!(
        blocks.len() == block_types.len(),
        "NIF block type table does not match its block list"
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
        bytes.extend_from_slice(block.as_slice());
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
    ensure!(!shape.positions.is_empty(), "NIF shape has no positions");
    ensure!(
        shape.positions.len() == shape.normals.len(),
        "NIF shape has {} positions and {} normals",
        shape.positions.len(),
        shape.normals.len()
    );
    ensure!(
        shape.positions.len() == shape.uvs.len(),
        "NIF shape has {} positions and {} UVs",
        shape.positions.len(),
        shape.uvs.len()
    );
    ensure!(
        shape.positions.len() <= u16::MAX as usize,
        "NIF shape exceeds 65535 vertices"
    );
    ensure!(
        shape.indices.len() <= u16::MAX as usize,
        "NIF shape exceeds 65535 triangles"
    );
    ensure!(!shape.indices.is_empty(), "NIF shape has no triangles");
    ensure!(
        shape
            .positions
            .iter()
            .all(|position| position.iter().all(|value| value.is_finite())),
        "NIF shape contains a non-finite position"
    );
    ensure!(
        shape
            .uvs
            .iter()
            .all(|uv| uv.iter().all(|value| value.is_finite())),
        "NIF shape contains a non-finite UV"
    );
    let vertex_count = shape.positions.len();
    ensure!(
        shape
            .indices
            .iter()
            .flatten()
            .all(|index| { usize::from(*index) < vertex_count }),
        "NIF shape contains an out-of-range triangle index"
    );
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

fn fade_node() -> Vec<u8> {
    let mut block = Vec::with_capacity(84);
    push_av_object(&mut block, NULL_REF);
    push_u32(&mut block, 1);
    push_u32(&mut block, 1);
    push_u32(&mut block, 0);
    block
}

fn triangle_shape(shape: &StaticShape<'_>) -> Result<Vec<u8>> {
    triangle_shape_with_shader(shape, 2)
}

fn triangle_shape_with_shader(shape: &StaticShape<'_>, shader_property: u32) -> Result<Vec<u8>> {
    let vertex_stride = usize::from(VERTEX_STRIDE) * 4;
    let vertex_bytes = shape
        .positions
        .len()
        .checked_mul(vertex_stride)
        .ok_or_else(|| eyre!("NIF vertex data overflow"))?;
    let triangle_bytes = shape
        .indices
        .len()
        .checked_mul(6)
        .ok_or_else(|| eyre!("NIF triangle data overflow"))?;
    let data_size = vertex_bytes
        .checked_add(triangle_bytes)
        .ok_or_else(|| eyre!("NIF geometry size overflow"))?;

    let (center, radius) = bounds(shape.positions);
    let mut block = Vec::with_capacity(72 + 16 + 24 + data_size);
    push_av_object(&mut block, 0);
    for value in center {
        block.extend_from_slice(&value.to_le_bytes());
    }
    block.extend_from_slice(&radius.to_le_bytes());
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, shader_property);
    push_u32(&mut block, NULL_REF);
    let descriptor =
        u64::from(VERTEX_STRIDE) | (4 << 8) | (5 << 16) | (u64::from(VERTEX_FLAGS) << 44);
    push_u64(&mut block, descriptor);
    push_u16(
        &mut block,
        u16::try_from(shape.indices.len()).map_err(|_| eyre!("NIF triangle count overflow"))?,
    );
    push_u16(
        &mut block,
        u16::try_from(shape.positions.len()).map_err(|_| eyre!("NIF vertex count overflow"))?,
    );
    push_u32(
        &mut block,
        u32::try_from(data_size).map_err(|_| eyre!("NIF geometry size overflow"))?,
    );
    for (position, (normal, uv)) in shape
        .positions
        .iter()
        .zip(shape.normals.iter().zip(shape.uvs.iter()))
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
    }
    for triangle in shape.indices {
        for index in triangle {
            block.extend_from_slice(&index.to_le_bytes());
        }
    }
    Ok(block)
}

fn lighting_shader_property() -> Vec<u8> {
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
    push_u32(&mut block, 3);
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

fn texture_set(shape: &StaticShape<'_>) -> Result<Vec<u8>> {
    let slots = [
        shape.diffuse,
        shape.normal_texture,
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ];
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
