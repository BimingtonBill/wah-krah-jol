//! NIF controller sequences (`Open`, `Close`, ...) exported as glTF animation clips.
//!
//! A Skyrim NIF that animates - a load door, a gate, a secret door - carries a
//! `NiControllerManager` whose `NiControllerSequence` list names the model's clips (`Open`,
//! `Close`, `playanim01`, ...). Every sequence lists `ControlledBlock`s that pair an animated
//! node's *name* with a `NiTransformInterpolator`, and the interpolator points at the
//! `NiTransformData` that holds the keys.
//!
//! The vendored NIF parser has no arms for those three types (`NiTransformController`,
//! `NiTransformInterpolator` and `NiTransformData` are empty stubs), so this module decodes the
//! blocks from their raw bytes. The layout below was fitted against every animated model in the
//! converted asset tree; a block that does not consume exactly is an error, never a guess.
//!
//! ```text
//! NiTransformData
//!   u32 Num Rotation Keys
//!   [u32 Rotation Type]                      # written only when the count is nonzero
//!   rotation keys                            # 4 = XYZ_ROTATION_KEY: three euler KeyGroups
//!   u32 Num Translation Keys
//!   [u32 Translation Type]                   # written only when the count is nonzero
//!   translation keys                         # f32 time + three floats
//!   u32 Num Scale Keys
//!   [u32 Scale Type]                         # written only when the count is nonzero
//!   scale keys                               # f32 time + one float
//! ```
//!
//! **A group writes its interpolation type only when it has keys.** That one rule is what makes
//! the blocks line up end to end: a rotation-only door's transform data ends in a bare `0, 0`
//! (no translation keys, no scale keys), a sliding door that does not rotate starts
//! `0, 95, 1, ...` (no rotation keys, 95 linear translation keys), and the word after a
//! zero-count group is always the next group's count. Reading the rotation type unconditionally
//! misreads 53 transform-data blocks across 13 door models in the converted tree.
//!
//! `XYZ_ROTATION_KEY` stores one `KeyGroup` per euler axis (X, then Y, then Z) instead of
//! quaternions; every other rotation type stores quaternion keys. A `KeyGroup` is
//! `u32 Num Keys, u32 Key Type` followed by its keys, and a key is `f32 Time, <value>` plus
//! `f32 Forward, f32 Backward` for `QUADRATIC_KEY` or three TBC floats for `TBC_KEY`. Times are
//! seconds from the sequence's start; euler values are radians about the node's own axes.
//!
//! One more shape is worth naming: a `NiTransformInterpolator`'s own translation/rotation/scale
//! are the "invalid" sentinel `-FLT_MAX` on animated doors, so the node's rest transform in the
//! exported scene *is* the pose and the curves are absolute - the curve at t = 0 equals the
//! node's rest transform.
//!
//! glTF cannot express a euler curve, so a rotation channel is baked: sampled at 30 Hz plus every
//! authored key time, emitted with `LINEAR` interpolation. Translation and scale keep their own
//! key times (also `LINEAR`); their `QUADRATIC_KEY`/`TBC_KEY` tangents shape the source curve
//! between keys and are not reproduced, while every authored key - and so every authored pose -
//! is. Node transforms are local and the Creation-to-runtime basis change lives on the exported
//! scene's root node, so the animated values are the NIF's own and no coordinate conversion is
//! applied here.

use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use project_wormhole_nif::nif_header::NifHeader;
use serde_json::{Value, json};
use std::{collections::HashMap, fs, path::Path};

/// Sampling rate of a baked rotation channel, in samples per second.
pub const ROTATION_SAMPLES_PER_SECOND: f32 = 30.0;

/// The sampled form of one animated node inside one clip.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipTrack {
    pub node: String,
    pub rotation: Option<SampledChannel<[f32; 4]>>,
    pub translation: Option<SampledChannel<[f32; 3]>>,
    pub scale: Option<SampledChannel<f32>>,
}

impl ClipTrack {
    pub fn is_empty(&self) -> bool {
        self.rotation.is_none() && self.translation.is_none() && self.scale.is_none()
    }
}

/// A channel as it is written to glTF: parallel time and value arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct SampledChannel<T> {
    pub times: Vec<f32>,
    pub values: Vec<T>,
}

/// One `NiControllerSequence`, ready to be written as a glTF animation.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub name: String,
    pub duration: f32,
    pub tracks: Vec<ClipTrack>,
}

/// Key interpolation, as stored in a group's `Key Type` word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Linear = 1,
    Quadratic = 2,
    Tbc = 3,
    XyzRotation = 4,
    Const = 5,
}

impl KeyType {
    fn from_bits(bits: u32) -> Result<Self> {
        match bits {
            1 => Ok(KeyType::Linear),
            2 => Ok(KeyType::Quadratic),
            3 => Ok(KeyType::Tbc),
            4 => Ok(KeyType::XyzRotation),
            5 => Ok(KeyType::Const),
            other => Err(color_eyre::eyre::eyre!("unknown NIF key type {other}")),
        }
    }

    /// Whether the curve steps between keys instead of interpolating.
    fn is_step(self) -> bool {
        matches!(self, KeyType::Const)
    }

    /// Bytes one key of this type occupies for a single float value.
    fn scalar_stride(self) -> usize {
        match self {
            KeyType::Quadratic => 16,
            KeyType::Tbc => 20,
            _ => 8,
        }
    }

    /// Bytes one key of this type occupies for a three-float value.
    fn vector_stride(self) -> usize {
        match self {
            KeyType::Quadratic => 40,
            KeyType::Tbc => 28,
            _ => 16,
        }
    }

    /// Bytes one key of this type occupies for a four-float quaternion value.
    fn quaternion_stride(self) -> usize {
        match self {
            KeyType::Quadratic => 52,
            KeyType::Tbc => 32,
            _ => 20,
        }
    }
}

/// A single-float curve.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarCurve {
    pub key_type: KeyType,
    pub keys: Vec<(f32, f32)>,
}

/// A three-float curve.
#[derive(Debug, Clone, PartialEq)]
pub struct Vec3Curve {
    pub key_type: KeyType,
    pub keys: Vec<(f32, [f32; 3])>,
}

/// A quaternion curve, components ordered `x, y, z, w`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuatCurve {
    pub key_type: KeyType,
    pub keys: Vec<(f32, [f32; 4])>,
}

/// The rotation half of `NiTransformData`: quaternion keys or three euler curves.
#[derive(Debug, Clone, PartialEq)]
pub enum RotationKeys {
    Quaternion(QuatCurve),
    Euler {
        x: ScalarCurve,
        y: ScalarCurve,
        z: ScalarCurve,
    },
}

/// One `NiTransformData` block, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct TransformKeys {
    pub rotation: RotationKeys,
    pub translation: Vec3Curve,
    pub scale: ScalarCurve,
}

/// The index of the left key of the pair bracketing `time`, and the blend towards the right key.
///
/// Only called for a `time` strictly inside the keyed range, so a bracketing pair exists.
fn bracket<V>(keys: &[(f32, V)], time: f32) -> (usize, f32) {
    let mut index = 1;
    while index + 1 < keys.len() && keys[index].0 < time {
        index += 1;
    }
    let (left_time, _) = keys[index - 1];
    let (right_time, _) = keys[index];
    if right_time <= left_time {
        return (index, 0.0);
    }
    (index, (time - left_time) / (right_time - left_time))
}

impl ScalarCurve {
    /// The curve's value at `time`, held flat outside the keyed range.
    pub fn value_at(&self, time: f32) -> f32 {
        let Some((first_time, first_value)) = self.keys.first().copied() else {
            return 0.0;
        };
        if time <= first_time {
            return first_value;
        }
        let Some(&(last_time, last_value)) = self.keys.last() else {
            return first_value;
        };
        if time >= last_time {
            return last_value;
        }
        let (index, blend) = bracket(&self.keys, time);
        let (_, left) = self.keys[index - 1];
        let (_, right) = self.keys[index];
        if self.key_type.is_step() {
            return left;
        }
        left + (right - left) * blend
    }

    fn times(&self) -> impl Iterator<Item = f32> + '_ {
        self.keys.iter().map(|key| key.0)
    }
}

impl Vec3Curve {
    /// The curve's value at `time`, held flat outside the keyed range.
    pub fn value_at(&self, time: f32) -> [f32; 3] {
        let Some((first_time, first_value)) = self.keys.first().copied() else {
            return [0.0; 3];
        };
        if time <= first_time {
            return first_value;
        }
        let Some(&(last_time, last_value)) = self.keys.last() else {
            return first_value;
        };
        if time >= last_time {
            return last_value;
        }
        let (index, blend) = bracket(&self.keys, time);
        let (_, left) = self.keys[index - 1];
        let (_, right) = self.keys[index];
        if self.key_type.is_step() {
            return left;
        }
        [
            left[0] + (right[0] - left[0]) * blend,
            left[1] + (right[1] - left[1]) * blend,
            left[2] + (right[2] - left[2]) * blend,
        ]
    }

    fn times(&self) -> impl Iterator<Item = f32> + '_ {
        self.keys.iter().map(|key| key.0)
    }
}

impl QuatCurve {
    /// The curve's value at `time`, held flat outside the keyed range.
    pub fn value_at(&self, time: f32) -> [f32; 4] {
        let Some((first_time, first_value)) = self.keys.first().copied() else {
            return [0.0, 0.0, 0.0, 1.0];
        };
        if time <= first_time {
            return first_value;
        }
        let Some(&(last_time, last_value)) = self.keys.last() else {
            return first_value;
        };
        if time >= last_time {
            return last_value;
        }
        let (index, blend) = bracket(&self.keys, time);
        let (_, left) = self.keys[index - 1];
        let (_, right) = self.keys[index];
        if self.key_type.is_step() {
            return left;
        }
        // Shortest-path linear blend, normalised: the source keys of a quaternion curve are
        // already the authored poses, so this only fills in the resampled times between them.
        let mut right = right;
        if dot4(left, right) < 0.0 {
            right = [-right[0], -right[1], -right[2], -right[3]];
        }
        normalize4([
            left[0] + (right[0] - left[0]) * blend,
            left[1] + (right[1] - left[1]) * blend,
            left[2] + (right[2] - left[2]) * blend,
            left[3] + (right[3] - left[3]) * blend,
        ])
    }
}

fn dot4(left: [f32; 4], right: [f32; 4]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2] + left[3] * right[3]
}

fn normalize4(quaternion: [f32; 4]) -> [f32; 4] {
    let length = dot4(quaternion, quaternion).sqrt();
    if length <= f32::EPSILON || !length.is_finite() {
        return [0.0, 0.0, 0.0, 1.0];
    }
    quaternion.map(|component| component / length)
}

/// The euler triple of an `XYZ_ROTATION_KEY` group as a quaternion, `x, y, z, w`.
///
/// The three axis angles compose as `Rz(z) * Ry(y) * Rx(x)` in the node's own frame, which is
/// what the authored rest transforms in the exported scenes show: a node whose Z curve is
/// `-0.15` has the rest quaternion of a `-0.15` rad turn about Z, and a node whose X curve is
/// the constant `3.14159` has the rest quaternion `[1, 0, 0, 0]`.
pub fn euler_xyz_to_quaternion([x, y, z]: [f32; 3]) -> [f32; 4] {
    let (sx, cx) = (x * 0.5).sin_cos();
    let (sy, cy) = (y * 0.5).sin_cos();
    let (sz, cz) = (z * 0.5).sin_cos();
    // qz * qy * qx
    let qx = [sx, 0.0, 0.0, cx];
    let qy = [0.0, sy, 0.0, cy];
    let qz = [0.0, 0.0, sz, cz];
    let qzqy = [
        qz[3] * qy[0] + qz[0] * qy[3] + qz[1] * qy[2] - qz[2] * qy[1],
        qz[3] * qy[1] - qz[0] * qy[2] + qz[1] * qy[3] + qz[2] * qy[0],
        qz[3] * qy[2] + qz[0] * qy[1] - qz[1] * qy[0] + qz[2] * qy[3],
        qz[3] * qy[3] - qz[0] * qy[0] - qz[1] * qy[1] - qz[2] * qy[2],
    ];
    normalize4([
        qzqy[3] * qx[0] + qzqy[0] * qx[3] + qzqy[1] * qx[2] - qzqy[2] * qx[1],
        qzqy[3] * qx[1] - qzqy[0] * qx[2] + qzqy[1] * qx[3] + qzqy[2] * qx[0],
        qzqy[3] * qx[2] + qzqy[0] * qx[1] - qzqy[1] * qx[0] + qzqy[2] * qx[3],
        qzqy[3] * qx[3] - qzqy[0] * qx[0] - qzqy[1] * qx[1] - qzqy[2] * qx[2],
    ])
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

/// A bounds-checked cursor over one NIF block's bytes.
struct BlockCursor<'a> {
    bytes: &'a [u8],
    position: usize,
    what: &'static str,
}

impl<'a> BlockCursor<'a> {
    fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Self {
            bytes,
            position: 0,
            what,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        ensure!(
            self.remaining() >= count,
            "{} block ended {} bytes early",
            self.what,
            count - self.remaining()
        );
        let slice = &self.bytes[self.position..self.position + count];
        self.position += count;
        Ok(slice)
    }

    fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn vec3(&mut self) -> Result<[f32; 3]> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }

    fn quaternion(&mut self) -> Result<[f32; 4]> {
        Ok([self.f32()?, self.f32()?, self.f32()?, self.f32()?])
    }

    /// Requires that the whole block was consumed: a layout that leaves bytes behind is wrong.
    fn finish(&self) -> Result<()> {
        ensure!(
            self.remaining() == 0,
            "{} block has {} unread trailing bytes",
            self.what,
            self.remaining()
        );
        Ok(())
    }
}

fn read_key_header(cursor: &mut BlockCursor<'_>, what: &'static str) -> Result<(usize, KeyType)> {
    let count = cursor.u32()? as usize;
    let key_type = KeyType::from_bits(cursor.u32()?)?;
    ensure!(
        key_type != KeyType::XyzRotation || count == 0,
        "{what} uses XYZ_ROTATION_KEY, which is a rotation key type only"
    );
    Ok((count, key_type))
}

/// Reads one `KeyGroup` header followed by its key times and single-float values.
fn read_scalar_group(cursor: &mut BlockCursor<'_>, what: &'static str) -> Result<ScalarCurve> {
    let (count, key_type) = read_key_header(cursor, what)?;
    let stride = key_type.scalar_stride();
    ensure!(
        count.saturating_mul(stride) <= cursor.remaining(),
        "{what} declares {count} keys, more than the block holds"
    );
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        let time = cursor.f32()?;
        let value = cursor.f32()?;
        cursor.skip(stride - 8)?;
        keys.push((time, value));
    }
    sort_keys(&mut keys);
    Ok(ScalarCurve { key_type, keys })
}

/// Reads a run of quaternion keys: the count and type come from the `NiTransformData` rotation
/// header rather than from a `KeyGroup` of their own.
fn read_quaternion_keys(
    cursor: &mut BlockCursor<'_>,
    count: usize,
    key_type: KeyType,
    what: &'static str,
) -> Result<QuatCurve> {
    let stride = key_type.quaternion_stride();
    ensure!(
        count.saturating_mul(stride) <= cursor.remaining(),
        "{what} declares {count} keys, more than the block holds"
    );
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        let time = cursor.f32()?;
        let value = cursor.quaternion()?;
        cursor.skip(stride - 20)?;
        keys.push((time, value));
    }
    sort_keys(&mut keys);
    Ok(QuatCurve { key_type, keys })
}

/// Reads an empty-or-keyed group of the shape `u32 count, [u32 type], keys`.
///
/// The interpolation type word is written only when the group has keys - every rotation-only door
/// model ends its transform data with a bare `0, 0` - so an empty group is a single count word.
fn read_optional_group<T>(
    cursor: &mut BlockCursor<'_>,
    what: &'static str,
    empty: T,
    read: impl FnOnce(&mut BlockCursor<'_>, usize, KeyType) -> Result<T>,
) -> Result<T> {
    let count = cursor.u32()? as usize;
    if count == 0 {
        return Ok(empty);
    }
    let key_type = KeyType::from_bits(cursor.u32()?)?;
    ensure!(
        key_type != KeyType::XyzRotation,
        "{what} uses XYZ_ROTATION_KEY, which is a rotation key type only"
    );
    read(cursor, count, key_type)
}

/// Decodes a whole `NiTransformData` block, consuming every byte of it.
///
/// A block that does not fit the fitted layout exactly is an error: the caller drops that node's
/// track and warns, rather than exporting a guess.
pub fn decode_transform_data(bytes: &[u8]) -> Result<TransformKeys> {
    let mut cursor = BlockCursor::new(bytes, "NiTransformData");

    let rotation_count = cursor.u32()? as usize;
    let rotation = if rotation_count == 0 {
        // A group with no keys writes no interpolation type at all, so the next word is already
        // the translation count. This is the shape of every model that animates without rotating:
        // a sliding secret door, an amulet hanging off a door.
        RotationKeys::Quaternion(QuatCurve {
            key_type: KeyType::Linear,
            keys: Vec::new(),
        })
    } else {
        let rotation_type = KeyType::from_bits(cursor.u32()?)?;
        if rotation_type == KeyType::XyzRotation {
            ensure!(
                rotation_count == 1,
                "XYZ_ROTATION_KEY transform data with {rotation_count} rotation key sets is not \
                 supported (every door model checked has exactly one)"
            );
            let x = read_scalar_group(&mut cursor, "NiTransformData euler X")?;
            let y = read_scalar_group(&mut cursor, "NiTransformData euler Y")?;
            let z = read_scalar_group(&mut cursor, "NiTransformData euler Z")?;
            RotationKeys::Euler { x, y, z }
        } else {
            RotationKeys::Quaternion(read_quaternion_keys(
                &mut cursor,
                rotation_count,
                rotation_type,
                "NiTransformData rotation",
            )?)
        }
    };

    let translation = read_optional_group(
        &mut cursor,
        "translation keys",
        Vec3Curve {
            key_type: KeyType::Linear,
            keys: Vec::new(),
        },
        |cursor, count, key_type| {
            let stride = key_type.vector_stride();
            ensure!(
                count.saturating_mul(stride) <= cursor.remaining(),
                "translation declares {count} keys, more than the block holds"
            );
            let mut keys = Vec::with_capacity(count);
            for _ in 0..count {
                let time = cursor.f32()?;
                let value = cursor.vec3()?;
                cursor.skip(stride - 16)?;
                keys.push((time, value));
            }
            sort_keys(&mut keys);
            Ok(Vec3Curve { key_type, keys })
        },
    )?;

    let scale = read_optional_group(
        &mut cursor,
        "scale keys",
        ScalarCurve {
            key_type: KeyType::Linear,
            keys: Vec::new(),
        },
        |cursor, count, key_type| {
            let stride = key_type.scalar_stride();
            ensure!(
                count.saturating_mul(stride) <= cursor.remaining(),
                "scale declares {count} keys, more than the block holds"
            );
            let mut keys = Vec::with_capacity(count);
            for _ in 0..count {
                let time = cursor.f32()?;
                let value = cursor.f32()?;
                cursor.skip(stride - 8)?;
                keys.push((time, value));
            }
            sort_keys(&mut keys);
            Ok(ScalarCurve { key_type, keys })
        },
    )?;

    cursor.finish()?;
    let keys = TransformKeys {
        rotation,
        translation,
        scale,
    };
    // A non-finite key would be written straight into the glTF as a NaN and poison the reader;
    // the block is rejected instead. Sorting afterwards is safe because times are finite, and it
    // makes the sampled curve independent of the order the exporter wrote the keys in.
    validate(&keys)?;
    Ok(keys)
}

fn sort_keys<V>(keys: &mut [(f32, V)]) {
    keys.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn validate(keys: &TransformKeys) -> Result<()> {
    let check = |what: &str, time: f32, values: &[f32]| -> Result<()> {
        ensure!(time.is_finite(), "{what} key time {time} is not finite");
        for value in values {
            ensure!(value.is_finite(), "{what} key value {value} is not finite");
        }
        Ok(())
    };
    match &keys.rotation {
        RotationKeys::Quaternion(curve) => {
            for (time, value) in &curve.keys {
                check("rotation", *time, value)?;
            }
        }
        RotationKeys::Euler { x, y, z } => {
            for (axis, curve) in [("X", x), ("Y", y), ("Z", z)] {
                for (time, value) in &curve.keys {
                    check(&format!("rotation {axis}"), *time, &[*value])?;
                }
            }
        }
    }
    for (time, value) in &keys.translation.keys {
        check("translation", *time, value)?;
    }
    for (time, value) in &keys.scale.keys {
        check("scale", *time, &[*value])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// NIF block walking
// ---------------------------------------------------------------------------------------------

/// One `NiControllerSequence`, reduced to what a clip needs.
struct SequenceSpec {
    name: String,
    stop_time: f32,
    /// `(node name, interpolator block index)`.
    targets: Vec<(String, usize)>,
}

fn blocks_of<'a>(block_bytes: &'a [u8], header: &NifHeader) -> Result<Vec<&'a [u8]>> {
    let mut slices = Vec::with_capacity(header.block_size_index.len());
    let mut rest = block_bytes;
    for (index, size) in header.block_size_index.iter().enumerate() {
        let size = *size as usize;
        ensure!(
            rest.len() >= size,
            "NIF block {index} is truncated ({} bytes left for {size})",
            rest.len()
        );
        let (block, remaining) = rest.split_at(size);
        slices.push(block);
        rest = remaining;
    }
    Ok(slices)
}

fn type_of(header: &NifHeader, index: usize) -> &str {
    header.get_block_type(index).unwrap_or("<invalid>")
}

fn string_of(header: &NifHeader, index: u32) -> Option<String> {
    let index = usize::try_from(index).ok()?;
    header.strings.get(index).map(|name| name.0.clone())
}

/// Reads one `NiControllerSequence` block.
fn read_sequence(block: &[u8], header: &NifHeader) -> Result<SequenceSpec> {
    let mut cursor = BlockCursor::new(block, "NiControllerSequence");
    let name_index = cursor.u32()?;
    let controlled_count = cursor.u32()? as usize;
    cursor.skip(4)?; // array grow by
    ensure!(
        controlled_count.saturating_mul(29) <= cursor.remaining(),
        "NiControllerSequence declares {controlled_count} controlled blocks, more than the block \
         holds"
    );
    let mut targets = Vec::with_capacity(controlled_count);
    for _ in 0..controlled_count {
        let interpolator = cursor.u32()?;
        cursor.skip(4)?; // controller
        cursor.skip(1)?; // priority
        let node_name = cursor.u32()?;
        cursor.skip(16)?; // property type, controller type, controller id, interpolator id
        if let Some(node) = string_of(header, node_name) {
            targets.push((node, interpolator as usize));
        }
    }
    cursor.skip(4)?; // weight
    cursor.skip(4)?; // text keys
    cursor.skip(4)?; // cycle type
    cursor.skip(4)?; // frequency
    cursor.skip(4)?; // start time
    let stop_time = cursor.f32()?;
    cursor.skip(8)?; // manager, accumulation root name
    let note_count = cursor.u16()? as usize;
    cursor.skip(note_count * 4)?;
    cursor.finish()?;
    Ok(SequenceSpec {
        name: string_of(header, name_index).unwrap_or_else(|| format!("sequence_{name_index}")),
        stop_time,
        targets,
    })
}

/// Reads the sequence lists of every `NiControllerManager` in a NIF.
fn read_sequences(blocks: &[&[u8]], header: &NifHeader) -> Result<Vec<SequenceSpec>> {
    let mut sequences = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if type_of(header, index) != "NiControllerManager" {
            continue;
        }
        let mut cursor = BlockCursor::new(block, "NiControllerManager");
        cursor.skip(26)?; // NiTimeController
        cursor.skip(1)?; // cumulative
        let count = cursor.u32()? as usize;
        // Bound the count by what the block can hold (4 bytes per reference), so a malformed file
        // is an error here rather than a multi-gigabyte allocation that aborts the process.
        ensure!(
            count.saturating_mul(4) <= cursor.remaining(),
            "NiControllerManager declares {count} sequences, more than the block holds"
        );
        let mut refs = Vec::with_capacity(count);
        for _ in 0..count {
            refs.push(cursor.u32()? as usize);
        }
        for sequence_index in refs {
            let Some(sequence) = blocks.get(sequence_index) else {
                continue;
            };
            if type_of(header, sequence_index) != "NiControllerSequence" {
                continue;
            }
            sequences.push(
                read_sequence(sequence, header)
                    .wrap_err_with(|| format!("NiControllerSequence block {sequence_index}"))?,
            );
        }
    }
    Ok(sequences)
}

/// Decodes a `NiTransformData` through the `NiTransformInterpolator` that references it.
///
/// `Ok(None)` says the block controls something other than a node's transform - a `NiVisController`
/// through a `NiBoolInterpolator`, a particle emitter's rate through a `NiFloatInterpolator`, or an
/// interpolator with no data at all. Those are not transform tracks and are not a problem.
fn transform_keys_of(
    blocks: &[&[u8]],
    header: &NifHeader,
    interpolator: usize,
) -> Result<Option<TransformKeys>> {
    let Some(block) = blocks.get(interpolator) else {
        return Err(color_eyre::eyre::eyre!(
            "controlled block references interpolator {interpolator}, which does not exist"
        ));
    };
    if type_of(header, interpolator) != "NiTransformInterpolator" {
        return Ok(None);
    }
    ensure!(
        block.len() >= 36,
        "NiTransformInterpolator block is {} bytes, shorter than its 36 byte layout",
        block.len()
    );
    let data_index = u32::from_le_bytes([block[32], block[33], block[34], block[35]]) as usize;
    if data_index == u32::MAX as usize {
        return Ok(None);
    }
    let Some(data) = blocks.get(data_index) else {
        return Err(color_eyre::eyre::eyre!(
            "transform interpolator references transform data {data_index}, which does not exist"
        ));
    };
    ensure!(
        type_of(header, data_index) == "NiTransformData",
        "transform interpolator references block {data_index} of type {}, not transform data",
        type_of(header, data_index)
    );
    decode_transform_data(data)
        .map(Some)
        .wrap_err_with(|| format!("NiTransformData block {data_index}"))
}

/// Every animation clip a NIF declares, in the order its sequences are listed.
///
/// The second return value holds one warning per sequence or node track that could not be read.
/// A model with no animation returns nothing, and nothing here is ever fatal to a conversion.
pub fn clips_from_blocks(
    block_bytes: &[u8],
    header: &NifHeader,
) -> Result<(Vec<Clip>, Vec<String>)> {
    let blocks = blocks_of(block_bytes, header)?;
    let sequences = read_sequences(&blocks, header)?;
    let mut clips = Vec::new();
    let mut warnings = Vec::new();
    for sequence in sequences {
        let mut tracks: Vec<ClipTrack> = Vec::new();
        let mut has_transform_block = false;
        for (node, interpolator) in &sequence.targets {
            let keys = match transform_keys_of(&blocks, header, *interpolator) {
                Ok(Some(keys)) => keys,
                // A block that controls something other than a node's transform - a visibility
                // flag, a particle emitter's rate - is not a transform track to drop.
                Ok(None) => continue,
                Err(error) => {
                    has_transform_block = true;
                    // `{:#}` keeps the cause on one line: "NiTransformData block 6: <reason>".
                    warnings.push(format!("sequence '{}': {error:#}", sequence.name));
                    continue;
                }
            };
            has_transform_block = true;
            let track = build_track(node, &keys, sequence.stop_time);
            if track.is_empty() {
                continue;
            }
            match tracks.iter_mut().find(|existing| &existing.node == node) {
                // A node controlled twice: the later block wins, which is what the game's
                // controller priority would do for the only case the install shows.
                Some(existing) => *existing = track,
                None => tracks.push(track),
            }
        }
        if tracks.is_empty() {
            if has_transform_block {
                warnings.push(format!(
                    "sequence '{}' has no readable transform track",
                    sequence.name
                ));
            }
            continue;
        }
        let duration = sequence.stop_time.max(track_end(&tracks)).max(0.0);
        if duration <= 0.0 {
            continue;
        }
        clips.push(Clip {
            name: sequence.name,
            duration,
            tracks,
        });
    }
    Ok((clips, warnings))
}

fn track_end(tracks: &[ClipTrack]) -> f32 {
    let mut end: f32 = 0.0;
    for track in tracks {
        if let Some(channel) = &track.rotation {
            end = end.max(channel.times.last().copied().unwrap_or(0.0));
        }
        if let Some(channel) = &track.translation {
            end = end.max(channel.times.last().copied().unwrap_or(0.0));
        }
        if let Some(channel) = &track.scale {
            end = end.max(channel.times.last().copied().unwrap_or(0.0));
        }
    }
    end
}

/// Samples one `NiTransformData` into the channels of a clip track.
fn build_track(node: &str, keys: &TransformKeys, duration: f32) -> ClipTrack {
    let duration = duration.max(keys_end(keys));
    ClipTrack {
        node: node.to_owned(),
        rotation: sample_rotation(&keys.rotation, duration),
        translation: sample_translation(&keys.translation),
        scale: sample_scale(&keys.scale),
    }
}

/// The time of a block's last key, whichever curve carries it.
fn keys_end(keys: &TransformKeys) -> f32 {
    let mut end: f32 = 0.0;
    match &keys.rotation {
        RotationKeys::Quaternion(curve) => {
            for (time, _) in &curve.keys {
                end = end.max(*time);
            }
        }
        RotationKeys::Euler { x, y, z } => {
            for curve in [x, y, z] {
                for (time, _) in &curve.keys {
                    end = end.max(*time);
                }
            }
        }
    }
    for (time, _) in &keys.translation.keys {
        end = end.max(*time);
    }
    for (time, _) in &keys.scale.keys {
        end = end.max(*time);
    }
    end
}

/// The rotation of a clip at `time`: an euler triple converted, or the quaternion key itself.
fn rotation_value_at(rotation: &RotationKeys, time: f32) -> [f32; 4] {
    match rotation {
        RotationKeys::Quaternion(curve) => curve.value_at(time),
        RotationKeys::Euler { x, y, z } => {
            euler_xyz_to_quaternion([x.value_at(time), y.value_at(time), z.value_at(time)])
        }
    }
}

/// Bakes a rotation to quaternion samples at 30 Hz plus every authored key time.
fn sample_rotation(rotation: &RotationKeys, duration: f32) -> Option<SampledChannel<[f32; 4]>> {
    let key_times: Vec<f32> = match rotation {
        RotationKeys::Quaternion(curve) => curve.keys.iter().map(|key| key.0).collect(),
        RotationKeys::Euler { x, y, z } => x.times().chain(y.times()).chain(z.times()).collect(),
    };
    if key_times.is_empty() {
        // No rotation keys: the node keeps its rest rotation, and a channel of identity poses
        // would only fight the rest pose the scene already carries.
        return None;
    }
    let times = sample_times(key_times.into_iter(), duration, false);
    if times.is_empty() {
        return None;
    }
    let values = times
        .iter()
        .map(|time| rotation_value_at(rotation, *time))
        .collect();
    Some(SampledChannel { times, values })
}

/// The key times of a translation curve, with `t = 0` prepended when the curve starts later.
fn sample_translation(curve: &Vec3Curve) -> Option<SampledChannel<[f32; 3]>> {
    let times = sample_times(curve.times(), f32::INFINITY, true);
    if times.is_empty() {
        return None;
    }
    let values = times.iter().map(|time| curve.value_at(*time)).collect();
    Some(SampledChannel { times, values })
}

/// The key times of a scale curve, with `t = 0` prepended when the curve starts later.
fn sample_scale(curve: &ScalarCurve) -> Option<SampledChannel<f32>> {
    let times = sample_times(curve.times(), f32::INFINITY, true);
    if times.is_empty() {
        return None;
    }
    let values = times.iter().map(|time| curve.value_at(*time)).collect();
    Some(SampledChannel { times, values })
}

/// The sample times of one channel: the authored key times, plus - for a baked channel - a
/// 30 Hz grid up to `duration`.
///
/// `prepend_zero` adds a leading `t = 0` sample when a curve's first key is later than that, so
/// the channel's first value is the pose at the clip's start rather than the value at its first
/// key.
fn sample_times(
    key_times: impl Iterator<Item = f32>,
    duration: f32,
    prepend_zero: bool,
) -> Vec<f32> {
    let mut times: Vec<f32> = key_times
        .filter(|time| time.is_finite() && *time >= 0.0 && *time <= duration.max(0.0) + 1.0e-6)
        .collect();
    if duration.is_finite() && duration > 0.0 {
        let step = 1.0 / ROTATION_SAMPLES_PER_SECOND;
        let mut time = 0.0;
        while time < duration - 1.0e-6 {
            times.push(time);
            time += step;
        }
        times.push(duration);
    }
    times.retain(|time| time.is_finite());
    times.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|left, right| (*left - *right).abs() <= 1.0e-6);
    if times.is_empty() {
        return times;
    }
    if prepend_zero && times[0] > 1.0e-6 {
        times.insert(0, 0.0);
    }
    times
}

// ---------------------------------------------------------------------------------------------
// glTF output
// ---------------------------------------------------------------------------------------------

/// Reads a NIF's animation clips from disk.
pub fn clips_from_nif(path: &Path) -> Result<(Vec<Clip>, Vec<String>)> {
    let bytes = fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let (block_bytes, header) =
        crate::mesh::parse_skyrim_header(&bytes, path).wrap_err("failed to read the NIF header")?;
    clips_from_blocks(block_bytes, &header)
        .wrap_err_with(|| format!("failed to read animations from {}", path.display()))
}

/// Appends a NIF's clips to a converted GLB.
///
/// The GLB already carries the model's node names and rest transforms, so a clip only has to name
/// those nodes. Returns the rewritten GLB and one warning per track whose node is not in the
/// scene; a model with no animation comes back unchanged.
pub fn append_nif_animations(glb: &[u8], path: &Path) -> Result<(Vec<u8>, Vec<String>)> {
    let (clips, mut warnings) = clips_from_nif(path)?;
    if clips.is_empty() {
        return Ok((glb.to_vec(), warnings));
    }
    let (glb, more) = append_animations(glb, &clips)?;
    warnings.extend(more);
    Ok((glb, warnings))
}

/// Appends animation clips to a GLB's JSON and binary chunk.
pub fn append_animations(glb: &[u8], clips: &[Clip]) -> Result<(Vec<u8>, Vec<String>)> {
    let (mut document, mut bin) = split_glb(glb)?;
    // The new accessors point into buffer 0, which the animation data below extends; a model with
    // geometry always carries one.
    ensure!(
        document
            .get("buffers")
            .and_then(Value::as_array)
            .is_some_and(|buffers| !buffers.is_empty()),
        "the GLB has no buffer for the animation data"
    );
    let node_names = gltf_node_names(&document);
    let mut warnings = Vec::new();
    let mut animations = document
        .get("animations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut accessors = document
        .get("accessors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut buffer_views = document
        .get("bufferViews")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for clip in clips {
        let mut samplers = Vec::new();
        let mut channels = Vec::new();
        for track in &clip.tracks {
            let Some(&node) = node_names.get(&track.node) else {
                warnings.push(format!(
                    "clip '{}' animates '{}', which is not a node of the exported scene",
                    clip.name, track.node
                ));
                continue;
            };
            if let Some(rotation) = &track.rotation {
                let mut values = rotation.values.clone();
                orient_rotation_samples(&mut values, gltf_node_rotation(&document, node));
                let input = push_accessor(
                    &mut accessors,
                    &mut buffer_views,
                    &mut bin,
                    Cast::F32(&rotation.times),
                    AccessorType::Scalar,
                    true,
                )?;
                let output = push_accessor(
                    &mut accessors,
                    &mut buffer_views,
                    &mut bin,
                    Cast::Vec4(&values),
                    AccessorType::Vec4,
                    false,
                )?;
                samplers.push(json!({
                    "input": input,
                    "interpolation": "LINEAR",
                    "output": output,
                }));
                channels.push(json!({
                    "sampler": samplers.len() - 1,
                    "target": { "node": node, "path": "rotation" },
                }));
            }
            if let Some(translation) = &track.translation {
                let input = push_accessor(
                    &mut accessors,
                    &mut buffer_views,
                    &mut bin,
                    Cast::F32(&translation.times),
                    AccessorType::Scalar,
                    true,
                )?;
                let output = push_accessor(
                    &mut accessors,
                    &mut buffer_views,
                    &mut bin,
                    Cast::Vec3(&translation.values),
                    AccessorType::Vec3,
                    false,
                )?;
                samplers.push(json!({
                    "input": input,
                    "interpolation": "LINEAR",
                    "output": output,
                }));
                channels.push(json!({
                    "sampler": samplers.len() - 1,
                    "target": { "node": node, "path": "translation" },
                }));
            }
            if let Some(scale) = &track.scale {
                let input = push_accessor(
                    &mut accessors,
                    &mut buffer_views,
                    &mut bin,
                    Cast::F32(&scale.times),
                    AccessorType::Scalar,
                    true,
                )?;
                // glTF's `scale` channel is a VEC3 per key, while a `NiTransformData` scale key is
                // one uniform float: write each key as (s, s, s). A SCALAR output here is invalid
                // glTF, and a reader that takes the specification at its word rejects the whole
                // file rather than the channel (Bevy 0.19: "Animations without a sampler output
                // are not supported").
                let uniform: Vec<[f32; 3]> = scale
                    .values
                    .iter()
                    .map(|value| [*value, *value, *value])
                    .collect();
                let output = push_accessor(
                    &mut accessors,
                    &mut buffer_views,
                    &mut bin,
                    Cast::Vec3(&uniform),
                    AccessorType::Vec3,
                    false,
                )?;
                samplers.push(json!({
                    "input": input,
                    "interpolation": "LINEAR",
                    "output": output,
                }));
                channels.push(json!({
                    "sampler": samplers.len() - 1,
                    "target": { "node": node, "path": "scale" },
                }));
            }
        }
        if channels.is_empty() {
            warnings.push(format!("clip '{}' has no usable channels", clip.name));
            continue;
        }
        animations.push(json!({
            "name": clip.name,
            "channels": channels,
            "samplers": samplers,
        }));
    }

    document["accessors"] = Value::Array(accessors);
    document["bufferViews"] = Value::Array(buffer_views);
    document["animations"] = Value::Array(animations);
    if let Some(buffers) = document.get_mut("buffers").and_then(Value::as_array_mut)
        && let Some(buffer) = buffers.first_mut()
        && let Some(object) = buffer.as_object_mut()
    {
        object.insert("byteLength".to_owned(), json!(u64_from_usize(bin.len())?));
    }
    rebuild_glb(&document, &bin).map(|glb| (glb, warnings))
}

/// Writes the samples of a channel into the binary chunk, returning its accessor index.
#[allow(clippy::too_many_arguments)]
fn push_accessor(
    accessors: &mut Vec<Value>,
    buffer_views: &mut Vec<Value>,
    bin: &mut Vec<u8>,
    data: Cast<'_>,
    kind: AccessorType,
    with_bounds: bool,
) -> Result<usize> {
    if !bin.len().is_multiple_of(4) {
        bin.resize(bin.len() + (4 - bin.len() % 4), 0);
    }
    let offset = bin.len();
    let count = data.count();
    data.write_to(bin);
    let bounds = if with_bounds { data.bounds()? } else { None };
    buffer_views.push(json!({
        "buffer": 0,
        "byteOffset": u64_from_usize(offset)?,
        "byteLength": u64_from_usize(bin.len() - offset)?,
    }));
    let mut accessor = json!({
        "bufferView": buffer_views.len() - 1,
        "componentType": 5126,
        "count": count,
        "type": kind.component_type(),
    });
    if let Some((min, max)) = bounds {
        accessor["min"] = json!(min);
        accessor["max"] = json!(max);
    }
    accessors.push(accessor);
    Ok(accessors.len() - 1)
}

fn u64_from_usize(value: usize) -> Result<u64> {
    u64::try_from(value).wrap_err("GLB buffer exceeds 2^64 bytes")
}

/// Accessor data, borrowed from a sampled channel.
enum Cast<'a> {
    F32(&'a [f32]),
    Vec3(&'a [[f32; 3]]),
    Vec4(&'a [[f32; 4]]),
}

impl Cast<'_> {
    fn count(&self) -> usize {
        match self {
            Cast::F32(values) => values.len(),
            Cast::Vec3(values) => values.len(),
            Cast::Vec4(values) => values.len(),
        }
    }

    fn write_to(&self, bin: &mut Vec<u8>) {
        match self {
            Cast::F32(values) => {
                for value in *values {
                    bin.extend_from_slice(&value.to_le_bytes());
                }
            }
            Cast::Vec3(values) => {
                for value in *values {
                    for component in value {
                        bin.extend_from_slice(&component.to_le_bytes());
                    }
                }
            }
            Cast::Vec4(values) => {
                for value in *values {
                    for component in value {
                        bin.extend_from_slice(&component.to_le_bytes());
                    }
                }
            }
        }
    }

    fn bounds(&self) -> Result<Option<(Value, Value)>> {
        match self {
            Cast::F32(values) => {
                let mut min = f32::INFINITY;
                let mut max = f32::NEG_INFINITY;
                for value in *values {
                    min = min.min(*value);
                    max = max.max(*value);
                }
                ensure!(
                    min.is_finite() && max.is_finite(),
                    "animation accessor has no finite bounds"
                );
                // glTF requires min/max to be component arrays, not bare numbers.
                Ok(Some((json!([min]), json!([max]))))
            }
            _ => Ok(None),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccessorType {
    Scalar,
    Vec3,
    Vec4,
}

impl AccessorType {
    fn component_type(self) -> &'static str {
        match self {
            AccessorType::Scalar => "SCALAR",
            AccessorType::Vec3 => "VEC3",
            AccessorType::Vec4 => "VEC4",
        }
    }
}

fn gltf_node_names(document: &Value) -> HashMap<String, usize> {
    let mut names = HashMap::new();
    if let Some(nodes) = document.get("nodes").and_then(Value::as_array) {
        for (index, node) in nodes.iter().enumerate() {
            if let Some(name) = node.get("name").and_then(Value::as_str) {
                names.entry(name.to_owned()).or_insert(index);
            }
        }
    }
    names
}

fn gltf_node_rotation(document: &Value, node: usize) -> Option<[f32; 4]> {
    let rotation = document
        .get("nodes")?
        .get(node)?
        .get("rotation")?
        .as_array()?;
    let mut values = [0.0f32, 0.0, 0.0, 1.0];
    if rotation.len() != 4 {
        return None;
    }
    for (index, component) in rotation.iter().enumerate() {
        values[index] = component.as_f64()? as f32;
    }
    Some(values)
}

/// Puts every sample on the same hemisphere as the previous one, and the first one on the same
/// hemisphere as the node's rest rotation: `q` and `-q` are the same rotation, and a channel that
/// flips sign between two frames would interpolate the long way round.
fn orient_rotation_samples(values: &mut [[f32; 4]], rest: Option<[f32; 4]>) {
    if let (Some(rest), Some(first)) = (rest, values.first_mut())
        && dot4(*first, rest) < 0.0
    {
        for value in values.iter_mut() {
            *value = value.map(|component| -component);
        }
    }
    for index in 1..values.len() {
        let previous = values[index - 1];
        if dot4(previous, values[index]) < 0.0 {
            values[index] = values[index].map(|component| -component);
        }
    }
}

fn split_glb(glb: &[u8]) -> Result<(Value, Vec<u8>)> {
    ensure!(
        glb.len() >= 20 && &glb[..4] == b"glTF",
        "invalid GLB container"
    );
    ensure!(&glb[16..20] == b"JSON", "GLB JSON chunk is missing");
    let json_length = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
    let json_end = 20usize
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON range overflow"))?;
    let json = glb
        .get(20..json_end)
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?;
    let document: Value = serde_json::from_slice(json).wrap_err("invalid glTF JSON")?;
    let mut bin = Vec::new();
    if glb.len() >= json_end + 8 {
        let chunk_length = u32::from_le_bytes([
            glb[json_end],
            glb[json_end + 1],
            glb[json_end + 2],
            glb[json_end + 3],
        ]) as usize;
        let chunk_type = u32::from_le_bytes([
            glb[json_end + 4],
            glb[json_end + 5],
            glb[json_end + 6],
            glb[json_end + 7],
        ]);
        if chunk_type == 0x004E_4942 {
            let start = json_end + 8;
            let end = start
                .checked_add(chunk_length)
                .ok_or_else(|| color_eyre::eyre::eyre!("GLB binary range overflow"))?;
            bin = glb
                .get(start..end)
                .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB binary chunk"))?
                .to_vec();
        }
    }
    Ok((document, bin))
}

fn rebuild_glb(document: &Value, bin: &[u8]) -> Result<Vec<u8>> {
    let mut json = serde_json::to_vec(document).wrap_err("failed to serialize glTF JSON")?;
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let has_bin = !bin.is_empty();
    let total_length = 20usize
        .checked_add(json.len())
        .and_then(|length| length.checked_add(if has_bin { 8 + bin.len() } else { 0 }))
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB size overflow"))?;
    let total_length = u32::try_from(total_length).wrap_err("GLB exceeds 4 GiB")?;
    let json_length = u32::try_from(json.len()).wrap_err("GLB JSON exceeds 4 GiB")?;
    let mut output = Vec::with_capacity(total_length as usize);
    output.extend_from_slice(b"glTF");
    output.extend_from_slice(&2u32.to_le_bytes());
    output.extend_from_slice(&total_length.to_le_bytes());
    output.extend_from_slice(&json_length.to_le_bytes());
    output.extend_from_slice(b"JSON");
    output.extend_from_slice(&json);
    if has_bin {
        let bin_length = u32::try_from(bin.len()).wrap_err("GLB binary chunk exceeds 4 GiB")?;
        output.extend_from_slice(&bin_length.to_le_bytes());
        output.extend_from_slice(&0x004E_4942u32.to_le_bytes());
        output.extend_from_slice(&bin);
    }
    Ok(output)
}

/// Unit tests for the door-clip decoder, plus one opt-in test that walks a real
/// converted asset tree.
///
/// That test reads the NIFs extracted under `OPENSKYRIM_CONVERTED_DIR`'s
/// `vfs/meshes`; it is `#[ignore]`d and skips - printing why - when the variable
/// is unset or the tree is not there, so CI never needs proprietary data
/// (ADR-0002).
#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `NiTransformData` block byte by byte.
    #[derive(Default)]
    struct DataWriter {
        bytes: Vec<u8>,
    }

    impl DataWriter {
        fn u32(&mut self, value: u32) -> &mut Self {
            self.bytes.extend_from_slice(&value.to_le_bytes());
            self
        }

        fn f32(&mut self, value: f32) -> &mut Self {
            self.bytes.extend_from_slice(&value.to_le_bytes());
            self
        }

        fn linear_scalar(&mut self, time: f32, value: f32) -> &mut Self {
            self.f32(time).f32(value)
        }

        fn quadratic_scalar(
            &mut self,
            time: f32,
            value: f32,
            forward: f32,
            backward: f32,
        ) -> &mut Self {
            self.f32(time).f32(value).f32(forward).f32(backward)
        }

        fn tbc_scalar(&mut self, time: f32, value: f32, tbc: [f32; 3]) -> &mut Self {
            self.f32(time)
                .f32(value)
                .f32(tbc[0])
                .f32(tbc[1])
                .f32(tbc[2])
        }

        fn linear_vec3(&mut self, time: f32, value: [f32; 3]) -> &mut Self {
            self.f32(time).f32(value[0]).f32(value[1]).f32(value[2])
        }

        fn quadratic_vec3(
            &mut self,
            time: f32,
            value: [f32; 3],
            forward: [f32; 3],
            backward: [f32; 3],
        ) -> &mut Self {
            self.linear_vec3(time, value);
            self.f32(forward[0]).f32(forward[1]).f32(forward[2]);
            self.f32(backward[0]).f32(backward[1]).f32(backward[2])
        }

        fn linear_quat(&mut self, time: f32, value: [f32; 4]) -> &mut Self {
            self.f32(time)
                .f32(value[0])
                .f32(value[1])
                .f32(value[2])
                .f32(value[3])
        }

        /// A rotation-only door: one XYZ key set, then empty translation and scale groups.
        fn rotation_only(x_keys: &[(f32, f32)], z_keys: &[(f32, f32)]) -> Vec<u8> {
            let mut writer = DataWriter::default();
            writer.u32(1).u32(4);
            writer.u32(x_keys.len() as u32).u32(1);
            for (time, value) in x_keys {
                writer.linear_scalar(*time, *value);
            }
            writer.u32(1).u32(1);
            writer.linear_scalar(0.0, 0.0);
            writer.u32(z_keys.len() as u32).u32(2);
            for (time, value) in z_keys {
                writer.quadratic_scalar(*time, *value, 0.0, 0.0);
            }
            writer.u32(0).u32(0);
            writer.bytes
        }

        fn finish(&self) -> Vec<u8> {
            self.bytes.clone()
        }
    }

    fn close3(left: [f32; 3], right: [f32; 3], tolerance: f32) -> bool {
        (0..3).all(|axis| (left[axis] - right[axis]).abs() <= tolerance)
    }

    fn euler(keys: &TransformKeys) -> ([f32; 3], KeyType) {
        match &keys.rotation {
            RotationKeys::Euler { x, y, z } => {
                ([x.keys[0].1, y.keys[0].1, z.keys[0].1], z.key_type)
            }
            RotationKeys::Quaternion(curve) => panic!("expected euler keys, got {:?}", curve),
        }
    }

    #[test]
    fn decodes_xyz_euler_rotation_with_linear_and_quadratic_axes() {
        // The shape of a dwemer load door: an X axis holding a constant 180 degree turn, a flat Y
        // axis and a quadratic Z curve - the swing itself.
        let bytes = DataWriter::rotation_only(
            &[(0.0, std::f32::consts::PI)],
            &[(0.0, 0.0), (0.6, 0.0934676)],
        );
        let keys = decode_transform_data(&bytes).expect("the fitted layout consumes the block");
        let (values, key_type) = euler(&keys);
        assert!(close3(values, [std::f32::consts::PI, 0.0, 0.0], 1.0e-6));
        assert_eq!(key_type, KeyType::Quadratic, "the Z axis is quadratic");
        assert_eq!(keys.translation.keys.len(), 0);
        assert_eq!(keys.scale.keys.len(), 0);
        let RotationKeys::Euler { z, .. } = &keys.rotation else {
            unreachable!()
        };
        assert_eq!(z.keys[1].0, 0.6);
        assert!((z.keys[1].1 - 0.0934676).abs() < 1.0e-6);
    }

    #[test]
    fn decodes_linear_translation_and_quadratic_scale_keys() {
        let mut writer = DataWriter::default();
        writer.u32(0); // no rotation keys, so no rotation type word either
        writer.u32(2).u32(1); // two linear translation keys
        writer.linear_vec3(0.0, [0.0, 0.0, 0.0]);
        writer.linear_vec3(0.0333, [12.5, 0.0, -3.0]);
        writer.u32(2).u32(2); // two quadratic scale keys
        writer.quadratic_scalar(0.0, 1.0, 0.0, 0.0);
        writer.quadratic_scalar(0.3, 1.5, 0.25, -0.25);
        let keys = decode_transform_data(&writer.finish()).unwrap();
        assert!(matches!(
            keys.rotation,
            RotationKeys::Quaternion(ref curve) if curve.keys.is_empty()
        ));
        assert_eq!(keys.translation.keys.len(), 2);
        assert_eq!(keys.translation.key_type, KeyType::Linear);
        assert!(close3(
            keys.translation.keys[1].1,
            [12.5, 0.0, -3.0],
            1.0e-6
        ));
        assert_eq!(keys.scale.keys.len(), 2);
        assert_eq!(keys.scale.key_type, KeyType::Quadratic);
        assert!((keys.scale.keys[1].1 - 1.5).abs() < 1.0e-6);
        // The quadratic tangents are consumed even though only the key values are exported.
        assert_eq!(keys.scale.value_at(0.15), 1.25);
    }

    #[test]
    fn decodes_a_sliding_door_with_no_rotation_keys_at_all() {
        // `riftenrwthievesguilddoor01.nif`'s own shape: the group with no keys writes no
        // interpolation type, so the block reads `0, 95, 1, <95 keys>, 0` - a door that slides
        // along its local X without ever turning.
        let mut writer = DataWriter::default();
        writer.u32(0); // no rotation keys, and therefore no rotation type word
        writer.u32(2).u32(1); // two linear translation keys
        writer.linear_vec3(0.0, [0.0, 0.0, 0.0]);
        writer.linear_vec3(0.0333, [-0.1433, 0.0378, 0.0]);
        writer.u32(0); // no scale keys
        let keys = decode_transform_data(&writer.finish()).expect("the block consumes exactly");
        assert!(
            matches!(keys.rotation, RotationKeys::Quaternion(ref curve) if curve.keys.is_empty()),
            "no rotation keys: {:?}",
            keys.rotation
        );
        assert_eq!(keys.translation.keys.len(), 2);
        assert!((keys.translation.keys[1].1[0] + 0.1433).abs() < 1.0e-4);
        assert!(keys.scale.keys.is_empty());

        // A block with a rotation *and* a translation keeps both type words.
        let mut both = DataWriter::default();
        both.u32(1).u32(1);
        both.linear_quat(0.0, [0.0, 0.0, 0.0, 1.0]);
        both.u32(1).u32(2);
        both.quadratic_vec3(0.0, [1.0, 2.0, 3.0], [0.0; 3], [0.0; 3]);
        both.u32(0);
        let keys = decode_transform_data(&both.finish()).unwrap();
        let RotationKeys::Quaternion(curve) = &keys.rotation else {
            panic!("expected quaternion keys")
        };
        assert_eq!(curve.keys.len(), 1);
        assert_eq!(keys.translation.keys.len(), 1);
        assert_eq!(keys.translation.key_type, KeyType::Quadratic);
    }

    #[test]
    fn decodes_linear_and_constant_quaternion_rotation_keys() {
        let mut writer = DataWriter::default();
        writer.u32(2).u32(1);
        writer.linear_quat(0.0, [0.0, 0.0, 0.0, 1.0]);
        let quarter = std::f32::consts::FRAC_1_SQRT_2;
        writer.linear_quat(1.0, [0.0, quarter, 0.0, quarter]);
        writer.u32(0).u32(0);
        let keys = decode_transform_data(&writer.finish()).unwrap();
        let RotationKeys::Quaternion(curve) = &keys.rotation else {
            panic!("expected quaternion keys")
        };
        assert_eq!(curve.keys.len(), 2);
        assert_eq!(curve.value_at(0.0), [0.0, 0.0, 0.0, 1.0]);
        assert!(dot4(curve.value_at(1.0), [0.0, quarter, 0.0, quarter]) > 0.999);

        let mut constant = DataWriter::default();
        constant.u32(1).u32(5);
        constant.linear_quat(0.25, [1.0, 0.0, 0.0, 0.0]);
        constant.u32(0).u32(0);
        let keys = decode_transform_data(&constant.finish()).unwrap();
        match keys.rotation {
            RotationKeys::Quaternion(curve) => {
                assert_eq!(curve.key_type, KeyType::Const);
                assert_eq!(curve.value_at(0.0), [1.0, 0.0, 0.0, 0.0]);
                assert_eq!(curve.value_at(9.0), [1.0, 0.0, 0.0, 0.0]);
            }
            other => panic!("expected quaternion keys, got {other:?}"),
        }
    }

    #[test]
    fn decodes_tbc_keys_with_their_tangents() {
        let mut writer = DataWriter::default();
        writer.u32(0); // no rotation keys
        writer.u32(1).u32(3); // one TBC translation key: time, vec3, tension/bias/continuity
        writer.f32(0.0);
        writer.f32(1.0).f32(2.0).f32(3.0);
        writer.f32(0.25).f32(0.5).f32(0.75);
        writer.u32(1).u32(3);
        writer.tbc_scalar(0.0, 2.0, [0.1, 0.2, 0.3]);
        let keys = decode_transform_data(&writer.finish()).unwrap();
        assert_eq!(keys.translation.key_type, KeyType::Tbc);
        assert!(close3(keys.translation.keys[0].1, [1.0, 2.0, 3.0], 1.0e-6));
        assert_eq!(keys.scale.key_type, KeyType::Tbc);
        assert_eq!(keys.scale.keys[0].1, 2.0);
    }

    #[test]
    fn rejects_a_block_that_leaves_bytes_unread() {
        let mut bytes = DataWriter::rotation_only(&[(0.0, 0.0)], &[(0.0, 0.0)]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let error = decode_transform_data(&bytes).expect_err("trailing bytes are an error");
        assert!(
            error.to_string().contains("unread trailing bytes"),
            "{error}"
        );
    }

    #[test]
    fn rejects_an_unknown_key_type() {
        let mut writer = DataWriter::default();
        writer.u32(1).u32(9);
        let error = decode_transform_data(&writer.finish()).expect_err("key type 9 does not exist");
        assert!(
            error.to_string().contains("unknown NIF key type 9"),
            "{error}"
        );
    }

    #[test]
    fn rejects_truncated_key_data() {
        let bytes = DataWriter::rotation_only(&[(0.0, 0.0)], &[(0.0, 0.0)]);
        let error = decode_transform_data(&bytes[..bytes.len() - 3])
            .expect_err("a truncated block is an error");
        assert!(error.to_string().contains("bytes early"), "{error}");

        let mut writer = DataWriter::default();
        writer.u32(0); // no rotation keys
        writer.u32(4).u32(1); // four linear keys declared, none stored
        let error = decode_transform_data(&writer.finish()).expect_err("missing keys are an error");
        assert!(
            error.to_string().contains("more than the block holds"),
            "{error}"
        );
    }

    #[test]
    fn euler_xyz_composes_as_z_then_y_then_x() {
        // A single axis is a half-angle turn about that axis.
        let z = euler_xyz_to_quaternion([0.0, 0.0, -0.15]);
        assert!((z[2] - (-0.15f32 * 0.5).sin()).abs() < 1.0e-6);
        assert!((z[3] - (-0.15f32 * 0.5).cos()).abs() < 1.0e-6);
        let x = euler_xyz_to_quaternion([std::f32::consts::PI, 0.0, 0.0]);
        assert!(close3([x[0], x[1], x[2]], [1.0, 0.0, 0.0], 1.0e-6));

        // A mixed triple is Rz * Ry * Rx applied to a vector.
        let source = [0.3f32, -0.7, 1.1];
        let expected = rotate_xyz(source);
        let actual = rotate_quaternion(euler_xyz_to_quaternion(source), [1.0, 0.0, 0.0]);
        assert!(
            close3(expected, actual, 1.0e-5),
            "{expected:?} != {actual:?}"
        );
    }

    /// `Rz(z) * Ry(y) * Rx(x)` applied to the X axis, built from matrices so the expectation does
    /// not reuse the implementation's quaternion helpers.
    fn rotate_xyz([x, y, z]: [f32; 3]) -> [f32; 3] {
        let (sx, cx) = x.sin_cos();
        let (sy, cy) = y.sin_cos();
        let (sz, cz) = z.sin_cos();
        let rx = [[1.0, 0.0, 0.0], [0.0, cx, -sx], [0.0, sx, cx]];
        let ry = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
        let rz = [[cz, -sz, 0.0], [sz, cz, 0.0], [0.0, 0.0, 1.0]];
        let multiply = |left: [[f32; 3]; 3], right: [[f32; 3]; 3]| {
            let mut product = [[0.0f32; 3]; 3];
            for row in 0..3 {
                for column in 0..3 {
                    product[row][column] = (0..3)
                        .map(|inner| left[row][inner] * right[inner][column])
                        .sum();
                }
            }
            product
        };
        let matrix = multiply(rz, multiply(ry, rx));
        [matrix[0][0], matrix[1][0], matrix[2][0]]
    }

    fn rotate_quaternion(quaternion: [f32; 4], vector: [f32; 3]) -> [f32; 3] {
        let [x, y, z, w] = quaternion;
        let rotate = |v: [f32; 3]| {
            let uv = [
                y * v[2] - z * v[1],
                z * v[0] - x * v[2],
                x * v[1] - y * v[0],
            ];
            let uuv = [
                y * uv[2] - z * uv[1],
                z * uv[0] - x * uv[2],
                x * uv[1] - y * uv[0],
            ];
            [
                v[0] + 2.0 * (w * uv[0] + uuv[0]),
                v[1] + 2.0 * (w * uv[1] + uuv[1]),
                v[2] + 2.0 * (w * uv[2] + uuv[2]),
            ]
        };
        rotate(vector)
    }

    #[test]
    fn rotation_channel_bakes_at_thirty_hertz_and_keeps_key_times() {
        let bytes = DataWriter::rotation_only(&[(0.0, 0.0)], &[(0.0, 0.0), (0.6, -0.15)]);
        let keys = decode_transform_data(&bytes).unwrap();
        let channel = sample_rotation(&keys.rotation, 0.6).unwrap();
        assert_eq!(channel.values.len(), channel.times.len());
        assert_eq!(channel.times[0], 0.0);
        assert!((channel.times[channel.times.len() - 1] - 0.6).abs() < 1.0e-6);
        assert!(
            channel
                .times
                .iter()
                .any(|time| (*time - 0.3).abs() < 1.0e-6),
            "the 30 Hz grid covers the clip: {:?}",
            channel.times
        );
        assert!(
            channel
                .times
                .iter()
                .any(|time| (*time - 0.6).abs() < 1.0e-6),
            "the authored key times survive: {:?}",
            channel.times
        );
        // t = 0 is the rest pose, and the end of the clip is the authored key.
        assert!(dot4(channel.values[0], [0.0, 0.0, 0.0, 1.0]) > 0.999);
        let last = channel.values[channel.values.len() - 1];
        assert!((last[2] - (-0.075f32).sin()).abs() < 1.0e-4, "{last:?}");
    }

    #[test]
    fn translation_channel_starts_at_zero_when_its_first_key_is_later() {
        let curve = Vec3Curve {
            key_type: KeyType::Linear,
            keys: vec![(0.5, [1.0, 2.0, 3.0]), (1.0, [4.0, 5.0, 6.0])],
        };
        let channel = sample_translation(&curve).unwrap();
        assert_eq!(channel.times, vec![0.0, 0.5, 1.0]);
        assert!(close3(channel.values[0], [1.0, 2.0, 3.0], 1.0e-6));
        assert!(close3(channel.values[2], [4.0, 5.0, 6.0], 1.0e-6));
    }

    /// A minimal GLB with the two nodes a door model exports.
    fn tiny_glb() -> Vec<u8> {
        // The root carries the Z-up to Y-up basis change, as a converted model's does.
        let half = std::f32::consts::FRAC_1_SQRT_2;
        let document = json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "name": "DweSmallDoorLoad01", "children": [1], "rotation": [-half, 0.0, 0.0, half] },
                { "name": "Object02", "rotation": [1.0, 0.0, 0.0, 0.0] },
            ],
            "buffers": [{ "byteLength": 0 }],
        });
        let mut json = serde_json::to_vec(&document).unwrap();
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
        glb
    }

    fn glb_json(glb: &[u8]) -> Value {
        let (document, _) = split_glb(glb).unwrap();
        document
    }

    fn glb_bin(glb: &[u8]) -> Vec<u8> {
        let (_, bin) = split_glb(glb).unwrap();
        bin
    }

    #[test]
    fn appends_a_clip_to_a_glb_and_targets_the_named_node() {
        let clip = Clip {
            name: "Open".to_owned(),
            duration: 0.6,
            tracks: vec![ClipTrack {
                node: "Object02".to_owned(),
                rotation: Some(SampledChannel {
                    times: vec![0.0, 0.3, 0.6],
                    values: vec![
                        [1.0, 0.0, 0.0, 0.0],
                        [0.9996, 0.0, 0.0, 0.0288],
                        [0.9986, 0.0, 0.0, 0.0523],
                    ],
                }),
                translation: Some(SampledChannel {
                    times: vec![0.0, 0.6],
                    values: vec![[0.0, 0.0, 0.0], [0.0, 0.0, 12.0]],
                }),
                scale: None,
            }],
        };
        let (glb, warnings) = append_animations(&tiny_glb(), &[clip]).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let document = glb_json(&glb);
        assert_eq!(document["animations"][0]["name"], "Open");
        let channels = document["animations"][0]["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 2, "rotation and translation");
        for channel in channels {
            assert_eq!(channel["target"]["node"], 1, "Object02 is node 1");
        }
        assert_eq!(channels[0]["target"]["path"], "rotation");
        assert_eq!(channels[1]["target"]["path"], "translation");
        let samplers = document["animations"][0]["samplers"].as_array().unwrap();
        assert_eq!(samplers[0]["interpolation"], "LINEAR");
        let inputs = samplers[0]["input"].as_u64().unwrap() as usize;
        let accessor = &document["accessors"][inputs];
        assert_eq!(accessor["count"], 3);
        assert_eq!(accessor["componentType"], 5126);
        assert_eq!(accessor["type"], "SCALAR");
        assert!(accessor["min"][0].as_f64().unwrap().abs() < 1e-6);
        assert!((accessor["max"][0].as_f64().unwrap() - 0.6).abs() < 1e-6);

        // The accessor's bytes are inside the binary chunk, and the buffer's length covers them.
        let bin = glb_bin(&glb);
        assert!(document["buffers"][0]["byteLength"].as_u64().unwrap() <= bin.len() as u64);
        let view = document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize]
            .as_object()
            .unwrap();
        let offset = view["byteOffset"].as_u64().unwrap() as usize;
        let times = (0..3)
            .map(|index| {
                let start = offset + index * 4;
                f32::from_le_bytes(bin[start..start + 4].try_into().unwrap())
            })
            .collect::<Vec<_>>();
        assert_eq!(times, vec![0.0, 0.3, 0.6]);
        // The GLB is still a valid container.
        assert_eq!(
            u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize,
            glb.len()
        );
    }

    /// Every channel's output accessor has to be the type glTF fixes for its target path:
    /// rotation VEC4, translation and scale VEC3, weights SCALAR. A `NiTransformData` scale key is
    /// one uniform float, and writing it as a SCALAR output produced files that a conforming
    /// reader rejects whole ("Animations without a sampler output are not supported"), which took
    /// out every animated model in a converted install.
    #[test]
    fn every_channel_output_matches_the_type_its_path_requires() {
        let clip = Clip {
            name: "Open".to_owned(),
            duration: 0.6,
            tracks: vec![ClipTrack {
                node: "Object02".to_owned(),
                rotation: Some(SampledChannel {
                    times: vec![0.0, 0.6],
                    values: vec![[0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.3, 0.95]],
                }),
                translation: Some(SampledChannel {
                    times: vec![0.0, 0.6],
                    values: vec![[0.0, 0.0, 0.0], [0.0, 0.0, 12.0]],
                }),
                scale: Some(SampledChannel {
                    times: vec![0.0, 0.6],
                    values: vec![1.0, 2.0],
                }),
            }],
        };
        let (glb, warnings) = append_animations(&tiny_glb(), &[clip]).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let document = glb_json(&glb);
        let animation = &document["animations"][0];
        let samplers = animation["samplers"].as_array().unwrap();
        let mut seen = 0;
        for channel in animation["channels"].as_array().unwrap() {
            let path = channel["target"]["path"].as_str().unwrap();
            let sampler = &samplers[channel["sampler"].as_u64().unwrap() as usize];
            let output = &document["accessors"][sampler["output"].as_u64().unwrap() as usize];
            let expected = match path {
                "rotation" => "VEC4",
                "translation" | "scale" => "VEC3",
                "weights" => "SCALAR",
                other => panic!("unexpected channel path {other}"),
            };
            assert_eq!(output["type"], expected, "{path} output");
            assert_eq!(
                document["accessors"][sampler["input"].as_u64().unwrap() as usize]["count"],
                output["count"],
                "{path}: one output per key time"
            );
            seen += 1;
        }
        assert_eq!(seen, 3, "rotation, translation and scale");
    }

    #[test]
    fn skips_a_track_whose_node_is_missing_and_keeps_the_container_valid() {
        let clip = Clip {
            name: "Open".to_owned(),
            duration: 0.6,
            tracks: vec![
                ClipTrack {
                    node: "NotInTheScene".to_owned(),
                    rotation: Some(SampledChannel {
                        times: vec![0.0],
                        values: vec![[0.0, 0.0, 0.0, 1.0]],
                    }),
                    translation: None,
                    scale: None,
                },
                ClipTrack {
                    node: "Object02".to_owned(),
                    rotation: Some(SampledChannel {
                        times: vec![0.0],
                        values: vec![[1.0, 0.0, 0.0, 0.0]],
                    }),
                    translation: None,
                    scale: None,
                },
            ],
        };
        let (glb, warnings) = append_animations(&tiny_glb(), &[clip]).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("NotInTheScene"), "{warnings:?}");
        let document = glb_json(&glb);
        assert_eq!(
            document["animations"][0]["channels"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize,
            glb.len()
        );
    }

    /// The fitted layout decodes *every* animated door model in the converted tree.
    ///
    /// This is the instrument for the layout itself: the rule that a group writes its
    /// interpolation type only when it has keys is what makes every block in the install consume
    /// exactly, and one model that stops fitting here means the rule is wrong again. Walking the
    /// tree takes about twenty seconds, so it stays ignored unless the assets are being checked.
    #[test]
    #[ignore = "walks the extracted Skyrim NIFs (OPENSKYRIM_CONVERTED_DIR)"]
    fn real_every_animated_door_model_decodes() {
        let Some(converted) =
            std::env::var_os("OPENSKYRIM_CONVERTED_DIR").map(std::path::PathBuf::from)
        else {
            eprintln!("skipping: set OPENSKYRIM_CONVERTED_DIR to a converted asset tree");
            return;
        };
        let root = converted.join("vfs").join("meshes");
        if !root.is_dir() {
            eprintln!("skipping: {} is not present", root.display());
            return;
        }
        let mut files = 0usize;
        let mut animated = 0usize;
        let mut clips = 0usize;
        let mut tracks = 0usize;
        let mut warnings: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("nif") {
                continue;
            }
            if !path.to_string_lossy().to_lowercase().contains("door") {
                continue;
            }
            files += 1;
            let (clips_of_file, file_warnings) = match clips_from_nif(path) {
                Ok(result) => result,
                Err(error) => {
                    warnings.push(format!("{}: {error:#}", path.display()));
                    continue;
                }
            };
            if !clips_of_file.is_empty() {
                animated += 1;
            }
            for warning in file_warnings {
                warnings.push(format!("{}: {warning}", path.display()));
            }
            for clip in &clips_of_file {
                clips += 1;
                tracks += clip.tracks.len();
            }
        }
        eprintln!(
            "{files} door NIFs, {animated} animated, {clips} clips, {tracks} tracks, {} warnings",
            warnings.len()
        );
        for warning in warnings.iter().take(10) {
            eprintln!("  {warning}");
        }
        assert!(
            warnings.is_empty(),
            "{} animated door models no longer decode; the first is {}",
            warnings.len(),
            warnings.first().cloned().unwrap_or_default()
        );
        // The install's door models carry roughly 350 clips and 640 tracks; a floor well below
        // that still catches a systematic regression without pinning the asset tree's contents.
        assert!(
            clips >= 300 && tracks >= 550,
            "only {clips} clips and {tracks} tracks were read from {files} door NIFs"
        );
    }

    #[test]
    fn a_clip_with_no_nodes_in_the_scene_is_dropped_with_a_warning() {
        let clip = Clip {
            name: "Close".to_owned(),
            duration: 0.6,
            tracks: vec![ClipTrack {
                node: "Missing".to_owned(),
                rotation: Some(SampledChannel {
                    times: vec![0.0],
                    values: vec![[0.0, 0.0, 0.0, 1.0]],
                }),
                translation: None,
                scale: None,
            }],
        };
        let (glb, warnings) = append_animations(&tiny_glb(), &[clip]).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let document = glb_json(&glb);
        assert!(
            document["animations"].as_array().unwrap().is_empty(),
            "no clip survives"
        );
    }
}
