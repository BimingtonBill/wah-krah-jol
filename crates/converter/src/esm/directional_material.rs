//! Directional materials: a `STAT`'s `DNAM` and the `MATO` it points at.
//!
//! A snow-covered static is the plain model with a projected material: no
//! `*snow*.nif` exists for the Dwemer facades, and `DweFacadeTowerRoof01SnowHeavy`
//! names `Dungeons\Dwemer\Facades\DweFacadeTowerRoof01.nif` like the bare roof
//! does (`docs/research/visual-gaps-spec.md`, gap 3). Coverage is the whole
//! object, evaluated from the surface normal against the material's projection
//! vector and the static's max angle.
//!
//! The layout was checked against the bytes (`tools/research/space_lighting_dump.py`):
//!
//! - `STAT` `DNAM` is 8 bytes: a little-endian `f32` max angle and a `MATO`
//!   FormID. `DweFacadeTowerRoof01SnowHeavy` holds `00 00 f0 42 29 51 02 00`:
//!   120.0 degrees and `MATO` 0x25129. The three other snow roofs hold 90.0.
//! - `MATO` `DATA` is 48 bytes: eleven floats and a trailing 32-bit flag. The
//!   snow roof's `SnowMaterialObject1P` holds falloff scale 0.35, falloff bias
//!   0.4, noise UV scale 48, material UV scale 170.667, a projection vector of
//!   `(0, 0, -1)` - straight down in Creation space, which is what snow on a
//!   roof needs - normal dampener 0.4, an RGB single-pass colour of
//!   `(0.4196, 0.4549, 0.4941)`, and a `single_pass` flag of 1. Read as a
//!   float that last field is a denormal that prints as zero, so it is an
//!   integer flag, not a twelfth float.
//! - All 32 `MATO` records in Skyrim.esm carry 48 bytes, and every `STAT`
//!   `DNAM` carries 8. A record that does not is dropped rather than guessed at.

/// `DNAM` is exactly this long in every `STAT` of Skyrim.esm.
pub const STAT_DNAM_LENGTH: usize = 8;
/// `MATO` `DATA` is exactly this long in every `MATO` of Skyrim.esm:
/// eleven floats and the trailing 32-bit flag.
pub const MATO_DATA_LENGTH: usize = 48;

/// A `MATO`: the projected material a snow-covered static is drawn with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectionalMaterial {
    pub falloff_scale: f32,
    pub falloff_bias: f32,
    pub noise_uv_scale: f32,
    pub material_uv_scale: f32,
    /// The projection vector, in the object's own space.
    pub direction: [f32; 3],
    pub normal_dampener: f32,
    /// The single-pass colour, packed like every other colour in this database:
    /// `r | g << 8 | b << 16`, scaled from the record's linear floats to 8 bits.
    pub single_pass_color: u32,
    pub single_pass: bool,
}

/// The `MATO` of a `STAT`: its `DNAM`'s second field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticDirectionalMaterial {
    /// Degrees, as `DNAM` stores them: how much of the object the material
    /// covers. 90 for most snow statics, 120 for the heavy roof.
    pub max_angle: f32,
    pub material_object: u32,
}

fn float(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .expect("bounds checked by the caller")
            .try_into()
            .expect("four bytes"),
    )
}

/// Reads a `STAT` `DNAM`: the max angle and the `MATO` FormID. Anything shorter
/// than 8 bytes is not this struct and yields `None`.
pub fn parse_static_dnam(bytes: &[u8]) -> Option<StaticDirectionalMaterial> {
    if bytes.len() < STAT_DNAM_LENGTH {
        return None;
    }
    Some(StaticDirectionalMaterial {
        max_angle: float(bytes, 0),
        material_object: u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes")),
    })
}

/// Reads a `MATO`'s `DATA`. Anything shorter than 48 bytes yields `None`.
pub fn parse_directional_material(bytes: &[u8]) -> Option<DirectionalMaterial> {
    if bytes.len() < MATO_DATA_LENGTH {
        return None;
    }
    let channels = [float(bytes, 32), float(bytes, 36), float(bytes, 40)]
        .map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8);
    let single_pass_color = u32::from_le_bytes([channels[0], channels[1], channels[2], 0]);
    Some(DirectionalMaterial {
        falloff_scale: float(bytes, 0),
        falloff_bias: float(bytes, 4),
        noise_uv_scale: float(bytes, 8),
        material_uv_scale: float(bytes, 12),
        direction: [float(bytes, 16), float(bytes, 20), float(bytes, 24)],
        normal_dampener: float(bytes, 28),
        single_pass_color,
        single_pass: u32::from_le_bytes(bytes[44..48].try_into().expect("four bytes")) != 0,
    })
}

/// The `DNAM` of a `STAT` record, if it has one.
pub fn static_directional_material(
    subs: &[(Vec<u8>, Vec<u8>)],
) -> Option<StaticDirectionalMaterial> {
    let bytes = subs
        .iter()
        .find(|(tag, _)| tag.as_slice() == b"DNAM")
        .map(|(_, data)| data.as_slice())?;
    parse_static_dnam(bytes)
}

/// The `MATO` record's data, if it has any.
pub fn directional_material(subs: &[(Vec<u8>, Vec<u8>)]) -> Option<DirectionalMaterial> {
    let bytes = subs
        .iter()
        .find(|(tag, _)| tag.as_slice() == b"DATA")
        .map(|(_, data)| data.as_slice())?;
    parse_directional_material(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `DweFacadeTowerRoof01SnowHeavy` (0xDC850): 120 degrees, `MATO` 0x25129.
    const HEAVY_ROOF_DNAM: [u8; 8] = [0x00, 0x00, 0xf0, 0x42, 0x29, 0x51, 0x02, 0x00];

    /// `DweFacadeTowerArch01Snow` (0x6DD66): 90 degrees, the same `MATO`.
    const ARCH_SNOW_DNAM: [u8; 8] = [0x00, 0x00, 0xb4, 0x42, 0x29, 0x51, 0x02, 0x00];

    /// `DATA` of `SnowMaterialObject1P` (0x25129).
    const SNOW_MATERIAL: [u8; 48] = [
        0x33, 0x33, 0xb3, 0x3e, 0xcd, 0xcc, 0xcc, 0x3e, 0x00, 0x00, 0x40, 0x42, 0xad, 0xaa, 0x2a,
        0x43, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xbf, 0xcd, 0xcc,
        0xcc, 0x3e, 0xd7, 0xd6, 0xd6, 0x3e, 0xe9, 0xe8, 0xe8, 0x3e, 0xfd, 0xfc, 0xfc, 0x3e, 0x01,
        0x00, 0x00, 0x00,
    ];

    fn packed(rgb: [u8; 3]) -> u32 {
        u32::from_le_bytes([rgb[0], rgb[1], rgb[2], 0])
    }

    #[test]
    fn reads_the_heavy_roof_max_angle_and_its_material_object() {
        let dnam = parse_static_dnam(&HEAVY_ROOF_DNAM).expect("8 bytes is a DNAM");
        assert_eq!(dnam.max_angle, 120.0);
        assert_eq!(dnam.material_object, 0x25129);

        let arch = parse_static_dnam(&ARCH_SNOW_DNAM).unwrap();
        assert_eq!(arch.max_angle, 90.0);
        assert_eq!(arch.material_object, 0x25129);
        // The two roofs share the material and differ only in the angle, which
        // is why the angle is published per static and not per material.
        assert_eq!(arch.material_object, dnam.material_object);
        assert_ne!(arch.max_angle, dnam.max_angle);
    }

    #[test]
    fn reads_the_snow_material_object() {
        let material = parse_directional_material(&SNOW_MATERIAL).expect("48 bytes is a DATA");
        assert_eq!(material.falloff_scale, 0.35);
        assert_eq!(material.falloff_bias, 0.4);
        assert_eq!(material.noise_uv_scale, 48.0);
        assert!((material.material_uv_scale - 170.666_67).abs() < 1e-3);
        // Straight down: snow falls, so the projection vector points at -Z in
        // Creation space. A misread layout would not be axis-aligned.
        assert_eq!(material.direction, [0.0, 0.0, -1.0]);
        assert_eq!(material.normal_dampener, 0.4);
        assert_eq!(material.single_pass_color, packed([107, 116, 126]));
        assert!(material.single_pass, "the trailing flag is 1");
    }

    #[test]
    fn a_short_or_missing_record_is_dropped() {
        for length in [0usize, 4, 7] {
            assert!(parse_static_dnam(&HEAVY_ROOF_DNAM[..length]).is_none());
        }
        for length in [0usize, 20, 44, 47] {
            assert!(parse_directional_material(&SNOW_MATERIAL[..length]).is_none());
        }
        // The trailing flag read as a float would be 1e-45, a denormal even the
        // shader would treat as zero: the record's 1 is what tells the two
        // readings apart, and reading it as a float loses the flag.
        let as_float = f32::from_le_bytes(SNOW_MATERIAL[44..48].try_into().unwrap());
        assert!(as_float > 0.0 && as_float < 1e-40, "{as_float}");
        assert_eq!(
            u32::from_le_bytes(SNOW_MATERIAL[44..48].try_into().unwrap()),
            1
        );

        let no_dnam = vec![(b"MODL".to_vec(), b"Architecture\\Wall.nif\0".to_vec())];
        assert!(static_directional_material(&no_dnam).is_none());
        let dnam = vec![(b"DNAM".to_vec(), HEAVY_ROOF_DNAM.to_vec())];
        assert_eq!(
            static_directional_material(&dnam).unwrap().material_object,
            0x25129
        );
        assert!(directional_material(&no_dnam).is_none());
        let data = vec![(b"DATA".to_vec(), SNOW_MATERIAL.to_vec())];
        assert!(directional_material(&data).unwrap().single_pass);
    }

    #[test]
    fn a_single_pass_colour_outside_zero_to_one_is_clamped() {
        let mut bytes = SNOW_MATERIAL;
        bytes[32..36].copy_from_slice(&2.0f32.to_le_bytes());
        bytes[36..40].copy_from_slice(&(-1.0f32).to_le_bytes());
        bytes[40..44].copy_from_slice(&0.5f32.to_le_bytes());
        let material = parse_directional_material(&bytes).unwrap();
        assert_eq!(material.single_pass_color, packed([255, 0, 128]));
    }
}
