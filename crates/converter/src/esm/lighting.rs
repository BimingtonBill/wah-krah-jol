//! Interior lighting (`CELL`'s `XCLL`, `LTMP` -> `LGTM`) and sky lighting
//! (`WRLD`'s `CNAM` -> `CLMT` -> `WTHR`).
//!
//! Nothing in the converter read any of this before; the layouts below were
//! taken from the web documentation in `docs/research/visual-gaps-spec.md` and
//! then checked against the records in `Skyrim.esm` (see
//! `tools/research/space_lighting_dump.py`, which prints the same fields from
//! the bytes). Where the bytes disagreed with the documentation, the bytes won:
//!
//! - A `CELL`'s `XCLL` and an `LGTM`'s `DATA` hold the same 92-byte struct.
//!   Every one of the 590 cells with an `XCLL` in Skyrim.esm carries 92 bytes
//!   (one dev cell carries 64); of the 97 `LGTM` records, 92 carry 92 bytes,
//!   4 carry 72 and 1 carries 64. A shorter record simply stops before the
//!   later fields, so the parser accepts 40 bytes and up and leaves the rest
//!   absent. Only the 92-byte form has the inherit flags.
//! - The `LGTM`'s payload lives in `DATA`, not in an `XCLL` subrecord: every
//!   `LGTM` in Skyrim.esm is `EDID` + `DATA` (the 92-byte struct) + `DALC`,
//!   and none of the 97 names a parent template.
//! - `DALC` (32 bytes in 92 templates, 24 in 5) is where SSE keeps the six
//!   directional-ambient tints, the specular colour and the specular power; in
//!   an `LGTM`'s `DATA` those same slots are zero. Cell records carry no `DALC`
//!   and keep the tints inside their own `XCLL`. The `space_lighting` table
//!   this module feeds has no column for the tints, so they are not published;
//!   an engine that wants a faithful look needs them, and this is the note to
//!   say so.
//! - The inherit flags are **one bit per field, set = the field is taken from
//!   the lighting template**. Settled by arithmetic, not by documentation:
//!   read the other way round, 127 of the 554 templated cells resolve to
//!   ambient `(0,0,0)` - `KilkreathRuins03` stores a black ambient with all
//!   eleven bits set while its template `IceCaveMedium` supplies `(45,79,83)` -
//!   and Alftand02 would resolve 6.8x darker than Alftand01 where their
//!   reference screenshots differ by 1.25x. With "set = inherited" no cell
//!   resolves to black, and Alftand01/Alftand02 come out 66.6 vs 55.8, the
//!   ratio their references have.
//! - `WTHR` `FNAM` is eight floats interleaved day/night rather than grouped in
//!   halves: `day_near, day_far, night_near, night_far, day_power, night_power,
//!   day_max, night_max`. `SkyrimCloudy` holds `0, 100000, 1000, 50000, 0.4,
//!   0.3, 0.875, 0.875`; grouped the documented way its day fog power would be
//!   1000.
//! - `NAM0` is 272 bytes: 17 groups of four colours, one per time of day, in
//!   the group order the spec lists (0 sky-upper, 1 fog-near, 3 ambient,
//!   4 sunlight, 5 sun, 7 sky-lower, 12 fog-far). Index 1 is day - for
//!   `SkyrimCloudy` the ambient group is `(143,156,158)`, `(203,220,220)`,
//!   `(197,175,178)`, `(55,96,136)`, and only the second is a daylight colour.
//!
//! The weather roll is not implemented: the first `WLST` entry with a non-zero
//! chance is the weather, deterministically, as the spec requires.

/// Smallest `XCLL`/`LGTM DATA` the parser reads: the fields through fog power.
pub const LIGHTING_MIN_LENGTH: usize = 40;
/// The full struct. Only this length has the inherit flags.
pub const LIGHTING_FULL_LENGTH: usize = 92;

/// One colour group in a weather's `NAM0`, by the group number's meaning.
pub const GROUP_SKY_UPPER: usize = 0;
pub const GROUP_FOG_NEAR: usize = 1;
pub const GROUP_AMBIENT: usize = 3;
pub const GROUP_SUNLIGHT: usize = 4;
pub const GROUP_SUN: usize = 5;
pub const GROUP_SKY_LOWER: usize = 7;
pub const GROUP_FOG_FAR: usize = 12;
/// The time-of-day index every published colour comes from: sunrise 0, day 1,
/// sunset 2, night 3. The engine has no clock, so a space gets daylight values.
pub const DAY_INDEX: usize = 1;
/// `NAM0` holds this many four-colour groups.
pub const NAM0_GROUPS: usize = 17;
/// Bytes per `NAM0` group: four colours.
pub const NAM0_GROUP_SIZE: usize = 16;

/// The `XCLL` struct, or as much of it as the record holds. A field is `None`
/// when the record is too short to carry it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Lighting {
    /// Packed `RGBA` bytes, little-endian, exactly as the record stores them.
    pub ambient: Option<u32>,
    pub directional: Option<u32>,
    pub fog_near_color: Option<u32>,
    pub fog_near: Option<f32>,
    pub fog_far: Option<f32>,
    pub direction_xy: Option<i32>,
    pub direction_z: Option<i32>,
    pub direction_fade: Option<f32>,
    pub fog_clip: Option<f32>,
    pub fog_power: Option<f32>,
    pub fog_far_color: Option<u32>,
    pub fog_max: Option<f32>,
    pub light_fade_begin: Option<f32>,
    pub light_fade_end: Option<f32>,
    /// The eleven inherit flags. `None` when the record is shorter than 92
    /// bytes, which means nothing is inherited.
    pub inherit: Option<u32>,
}

/// The inherit flags in bit order. Bit 5 covers both rotation fields and bit 10
/// both light-fade distances; every other bit covers exactly one field.
const INHERIT_MASKS: [u32; 11] = [
    1 << 0,
    1 << 1,
    1 << 2,
    1 << 3,
    1 << 4,
    1 << 5,
    1 << 6,
    1 << 7,
    1 << 8,
    1 << 9,
    1 << 10,
];

fn subrecord<'a>(subs: &'a [(Vec<u8>, Vec<u8>)], tag: &[u8; 4]) -> Option<&'a [u8]> {
    subs.iter()
        .find(|(candidate, _)| candidate.as_slice() == tag)
        .map(|(_, data)| data.as_slice())
}

fn subrecords<'a>(subs: &'a [(Vec<u8>, Vec<u8>)], tag: &[u8; 4]) -> Vec<&'a [u8]> {
    subs.iter()
        .filter(|(candidate, _)| candidate.as_slice() == tag)
        .map(|(_, data)| data.as_slice())
        .collect()
}

fn editor_id(subs: &[(Vec<u8>, Vec<u8>)]) -> Option<String> {
    let bytes = subrecord(subs, b"EDID")?;
    Some(
        String::from_utf8_lossy(bytes)
            .trim_matches('\0')
            .to_string(),
    )
}

fn color(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(slice.try_into().expect("four bytes")))
}

fn float(bytes: &[u8], offset: usize) -> Option<f32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(f32::from_le_bytes(slice.try_into().expect("four bytes")))
}

fn integer(bytes: &[u8], offset: usize) -> Option<i32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(i32::from_le_bytes(slice.try_into().expect("four bytes")))
}

/// Reads an `XCLL` (in a `CELL`) or an `LGTM`'s `DATA`. Anything shorter than
/// [`LIGHTING_MIN_LENGTH`] is not a lighting struct and yields `None`.
pub fn parse_lighting(bytes: &[u8]) -> Option<Lighting> {
    if bytes.len() < LIGHTING_MIN_LENGTH {
        return None;
    }
    Some(Lighting {
        ambient: color(bytes, 0x00),
        directional: color(bytes, 0x04),
        fog_near_color: color(bytes, 0x08),
        fog_near: float(bytes, 0x0C),
        fog_far: float(bytes, 0x10),
        direction_xy: integer(bytes, 0x14),
        direction_z: integer(bytes, 0x18),
        direction_fade: float(bytes, 0x1C),
        fog_clip: float(bytes, 0x20),
        fog_power: float(bytes, 0x24),
        fog_far_color: color(bytes, 0x48),
        fog_max: float(bytes, 0x4C),
        light_fade_begin: float(bytes, 0x50),
        light_fade_end: float(bytes, 0x54),
        inherit: integer(bytes, 0x58).map(|flags| flags as u32),
    })
}

/// The `XCLL` of a `CELL` record.
pub fn cell_lighting(subs: &[(Vec<u8>, Vec<u8>)]) -> Option<Lighting> {
    parse_lighting(subrecord(subs, b"XCLL")?)
}

/// The lighting of an `LGTM` record: its `DATA` subrecord.
pub fn template_lighting(subs: &[(Vec<u8>, Vec<u8>)]) -> Option<Lighting> {
    parse_lighting(subrecord(subs, b"DATA")?)
}

/// Resolves a cell's lighting against its template.
///
/// An inherit bit set means the field comes from the template; clear means the
/// record's own value stands (see the module docs for how that was settled). A
/// bit set with no template value to take - no `LTMP`, no `LGTM`, a template
/// whose `DATA` is too short for the field - falls back to the cell's own value
/// rather than inventing one, and a cell whose `XCLL` is too short for the flags
/// inherits nothing.
pub fn resolve_lighting(cell: &Lighting, template: Option<&Lighting>) -> Lighting {
    let (Some(flags), Some(template)) = (cell.inherit, template) else {
        return *cell;
    };
    let mut resolved = *cell;
    for (index, mask) in INHERIT_MASKS.iter().enumerate() {
        if flags & mask == 0 {
            continue;
        }
        match index {
            0 => resolved.ambient = template.ambient.or(cell.ambient),
            1 => resolved.directional = template.directional.or(cell.directional),
            2 => resolved.fog_near_color = template.fog_near_color.or(cell.fog_near_color),
            3 => resolved.fog_near = template.fog_near.or(cell.fog_near),
            4 => resolved.fog_far = template.fog_far.or(cell.fog_far),
            5 => {
                resolved.direction_xy = template.direction_xy.or(cell.direction_xy);
                resolved.direction_z = template.direction_z.or(cell.direction_z);
            }
            6 => resolved.direction_fade = template.direction_fade.or(cell.direction_fade),
            7 => resolved.fog_clip = template.fog_clip.or(cell.fog_clip),
            8 => resolved.fog_power = template.fog_power.or(cell.fog_power),
            9 => resolved.fog_max = template.fog_max.or(cell.fog_max),
            10 => {
                resolved.light_fade_begin = template.light_fade_begin.or(cell.light_fade_begin);
                resolved.light_fade_end = template.light_fade_end.or(cell.light_fade_end);
            }
            unreachable => unreachable!("{unreachable} is not an inherit bit"),
        }
    }
    resolved
}

/// A `CLMT`: the weather choices of a worldspace.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Climate {
    pub editor_id: Option<String>,
    pub weathers: Vec<ClimateWeather>,
}

/// One `WLST` entry: a weather, its chance in percent, and a global that gates
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClimateWeather {
    pub weather: u32,
    pub chance: u32,
    pub global: u32,
}

impl Climate {
    /// The weather to use: the first entry with a non-zero chance, or the first
    /// entry when every chance is zero. The spec fixes this choice so that a
    /// conversion is reproducible.
    pub fn first_weather(&self) -> Option<ClimateWeather> {
        self.weathers
            .iter()
            .find(|entry| entry.chance > 0)
            .or_else(|| self.weathers.first())
            .copied()
    }
}

/// Reads a `CLMT`. A `WLST` entry is 12 bytes: weather FormID, chance, global.
pub fn parse_climate(subs: &[(Vec<u8>, Vec<u8>)]) -> Climate {
    let weathers = subrecords(subs, b"WLST")
        .into_iter()
        .filter_map(|bytes| {
            Some(ClimateWeather {
                weather: u32::from_le_bytes(bytes.get(..4)?.try_into().expect("four bytes")),
                chance: bytes
                    .get(4..8)
                    .map(|slice| u32::from_le_bytes(slice.try_into().expect("four bytes")))
                    .unwrap_or(0),
                global: bytes
                    .get(8..12)
                    .map(|slice| u32::from_le_bytes(slice.try_into().expect("four bytes")))
                    .unwrap_or(0),
            })
        })
        .collect();
    Climate {
        editor_id: editor_id(subs),
        weathers,
    }
}

/// The fog distances and exponents of a `WTHR`'s `FNAM`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WeatherFog {
    pub day_near: f32,
    pub day_far: f32,
    pub night_near: f32,
    pub night_far: f32,
    pub day_power: f32,
    pub night_power: f32,
    pub day_max: f32,
    pub night_max: f32,
}

/// A `WTHR`: the colours and fog a worldspace's sky is drawn with.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Weather {
    pub editor_id: Option<String>,
    /// One packed colour per `NAM0` group, at the group's day index. Shorter
    /// than [`NAM0_GROUPS`] when the record does not carry every group, and
    /// `None` for a group the record truncates.
    pub day_colors: Vec<Option<u32>>,
    pub fog: Option<WeatherFog>,
}

impl Weather {
    /// The day colour of one `NAM0` group.
    pub fn group(&self, group: usize) -> Option<u32> {
        self.day_colors.get(group).copied().flatten()
    }
}

/// Reads a `WTHR`.
pub fn parse_weather(subs: &[(Vec<u8>, Vec<u8>)]) -> Weather {
    let day_colors = match subrecord(subs, b"NAM0") {
        Some(nam0) => (0..NAM0_GROUPS.min(nam0.len() / NAM0_GROUP_SIZE))
            .map(|group| color(nam0, group * NAM0_GROUP_SIZE + DAY_INDEX * 4))
            .collect(),
        None => Vec::new(),
    };
    let fog = subrecord(subs, b"FNAM").and_then(|bytes| {
        // Day and night alternate per property, not per half.
        if bytes.len() < 32 {
            return None;
        }
        Some(WeatherFog {
            day_near: float(bytes, 0x00)?,
            day_far: float(bytes, 0x04)?,
            night_near: float(bytes, 0x08)?,
            night_far: float(bytes, 0x0C)?,
            day_power: float(bytes, 0x10)?,
            night_power: float(bytes, 0x14)?,
            day_max: float(bytes, 0x18)?,
            night_max: float(bytes, 0x1C)?,
        })
    });
    Weather {
        editor_id: editor_id(subs),
        day_colors,
        fog,
    }
}

/// Rec.709 luma of a packed colour, 0..=1. Used as a weather's sun strength: a
/// `WTHR` carries no illuminance of its own, so the day sunlight colour's luma
/// is the only per-weather sun quantity there is.
pub fn packed_luma(packed: u32) -> f32 {
    let [r, g, b, _] = packed.to_le_bytes();
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `XCLL` of Alftand01 (0x152C3), copied from the bytes
    /// `tools/research/space_lighting_dump.py` printed.
    const ALFTAND01_XCLL: [u8; 92] = [
        0x28, 0x52, 0x57, 0x00, 0x1a, 0x3c, 0x48, 0x00, 0x6a, 0x9f, 0xbf, 0x00, 0x00, 0x00, 0xaa,
        0x43, 0x00, 0xc0, 0xda, 0x45, 0x00, 0x00, 0x00, 0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xa0, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0x02, 0x0d, 0x0f, 0x00, 0x01,
        0x0d, 0x0e, 0x00, 0x02, 0x0d, 0x0f, 0x00, 0x02, 0x0c, 0x0f, 0x00, 0x00, 0x06, 0x06, 0x00,
        0x03, 0x13, 0x17, 0x00, 0x23, 0x39, 0x49, 0x00, 0x00, 0x00, 0x80, 0x3f, 0x23, 0x39, 0x49,
        0x00, 0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xfe, 0x07,
        0x00, 0x00,
    ];

    /// The `DATA` of the `LGTM` Alftand01 points at,
    /// `IceCave_HobsFall_LightingTemplate` (0x8E78E).
    const ALFTAND01_TEMPLATE: [u8; 92] = [
        0x34, 0x45, 0x56, 0x00, 0x3c, 0x50, 0x59, 0x00, 0x99, 0xd2, 0xee, 0x00, 0x00, 0x80, 0x89,
        0x44, 0x00, 0xa0, 0x0c, 0x46, 0xc8, 0x00, 0x00, 0x00, 0x5a, 0x00, 0x00, 0x00, 0xcd, 0xcc,
        0x4c, 0x3e, 0x00, 0xa0, 0x0c, 0x46, 0x33, 0x33, 0x33, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0xb5, 0xd3, 0xf2, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb5, 0xd3, 0xf2,
        0x00, 0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0xfa, 0x45, 0x00, 0xa0, 0x0c, 0x46, 0x00, 0x00,
        0x00, 0x00,
    ];

    /// The `XCLL` of Alftand02 (0x56C1B): every inherit bit set.
    const ALFTAND02_XCLL: [u8; 92] = [
        0x02, 0x0d, 0x0f, 0x00, 0x1a, 0x3c, 0x48, 0x00, 0x78, 0x98, 0x96, 0x00, 0x00, 0x00, 0xaa,
        0x43, 0x00, 0xc0, 0xda, 0x45, 0x00, 0x00, 0x00, 0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xa0, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0x02, 0x0d, 0x0f, 0x00, 0x01,
        0x0d, 0x0e, 0x00, 0x02, 0x0d, 0x0f, 0x00, 0x02, 0x0c, 0x0f, 0x00, 0x2b, 0x42, 0x30, 0x00,
        0x2f, 0x4a, 0x3a, 0x00, 0x78, 0x98, 0x96, 0x00, 0x00, 0x00, 0x80, 0x3f, 0x78, 0x98, 0x96,
        0x00, 0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x07,
        0x00, 0x00,
    ];

    /// The `DATA` of `IceCave_HobsFall_LightingTemplateFar` (0x906CD), the
    /// template Alftand02 points at and inherits everything from.
    const ALFTAND02_TEMPLATE: [u8; 92] = [
        0x23, 0x3d, 0x41, 0x00, 0x38, 0x56, 0x5c, 0x00, 0xa2, 0xd0, 0xe4, 0x00, 0x00, 0x80, 0xbb,
        0x44, 0x00, 0x80, 0x3b, 0x46, 0x00, 0x00, 0x00, 0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x80, 0x3b, 0x46, 0x33, 0x33, 0x33, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0xc0, 0xdb, 0xe7, 0x00, 0x00, 0x00, 0x80, 0x3f, 0xa2, 0xd0, 0xe4,
        0x00, 0x00, 0x00, 0x80, 0x3f, 0x00, 0x40, 0x1c, 0x46, 0x00, 0xe0, 0x2b, 0x46, 0x00, 0x00,
        0x00, 0x00,
    ];

    fn packed(rgb: [u8; 3]) -> u32 {
        u32::from_le_bytes([rgb[0], rgb[1], rgb[2], 0])
    }

    fn with_xcll(bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![(b"XCLL".to_vec(), bytes.to_vec())]
    }

    #[test]
    fn reads_the_lighting_of_a_real_cell_and_its_template() {
        let cell = parse_lighting(&ALFTAND01_XCLL).expect("92 bytes is a lighting struct");
        assert_eq!(cell.ambient, Some(packed([40, 82, 87])));
        assert_eq!(cell.directional, Some(packed([26, 60, 72])));
        assert_eq!(cell.fog_near_color, Some(packed([106, 159, 191])));
        assert_eq!(cell.fog_near, Some(340.0));
        assert_eq!(cell.fog_far, Some(7000.0));
        assert_eq!(cell.direction_xy, Some(0));
        assert_eq!(cell.direction_z, Some(90));
        assert_eq!(cell.direction_fade, Some(5.0));
        assert_eq!(cell.fog_clip, Some(0.0));
        assert_eq!(cell.fog_power, Some(0.5));
        assert_eq!(cell.fog_far_color, Some(packed([35, 57, 73])));
        assert_eq!(cell.fog_max, Some(1.0));
        assert_eq!(cell.inherit, Some(0x7FE));

        let template = parse_lighting(&ALFTAND01_TEMPLATE).expect("the LGTM DATA is the struct");
        assert_eq!(template.ambient, Some(packed([52, 69, 86])));
        assert_eq!(template.directional, Some(packed([60, 80, 89])));
        assert_eq!(template.fog_near, Some(1100.0));
        assert_eq!(template.fog_far, Some(9000.0));
        assert_eq!(template.light_fade_begin, Some(8000.0));
        assert_eq!(template.light_fade_end, Some(9000.0));
        // An LGTM's inherit-flag slot is zero in all 97 templates of Skyrim.esm.
        assert_eq!(template.inherit, Some(0));
    }

    #[test]
    fn the_record_level_readers_take_the_xcll_and_the_template_data() {
        let cell = with_xcll(&ALFTAND01_XCLL);
        assert_eq!(cell_lighting(&cell), parse_lighting(&ALFTAND01_XCLL));

        let template = vec![
            (
                b"EDID".to_vec(),
                b"IceCave_HobsFall_LightingTemplate\0".to_vec(),
            ),
            (b"DATA".to_vec(), ALFTAND01_TEMPLATE.to_vec()),
            (b"DALC".to_vec(), vec![0; 32]),
        ];
        assert_eq!(
            template_lighting(&template),
            parse_lighting(&ALFTAND01_TEMPLATE)
        );
        assert_eq!(
            parse_climate(&template).editor_id.as_deref(),
            Some("IceCave_HobsFall_LightingTemplate")
        );
        assert!(cell_lighting(&[]).is_none(), "no XCLL, no lighting");
        assert!(
            cell_lighting(&with_xcll(&[0; 4])).is_none(),
            "4 bytes is not a struct"
        );
        assert!(template_lighting(&[]).is_none());
    }

    #[test]
    fn a_set_inherit_bit_takes_the_template_and_a_clear_one_keeps_the_cell() {
        let cell = parse_lighting(&ALFTAND01_XCLL).unwrap();
        let template = parse_lighting(&ALFTAND01_TEMPLATE).unwrap();
        let resolved = resolve_lighting(&cell, Some(&template));

        // Bit 0 is clear on Alftand01: the cell's own ambient stands, and it
        // differs from the template's (52,69,86), so the bit is doing work.
        assert_eq!(resolved.ambient, Some(packed([40, 82, 87])));
        assert_ne!(resolved.ambient, template.ambient);
        // Bits 1..10 are set: everything else comes from the template.
        assert_eq!(resolved.directional, Some(packed([60, 80, 89])));
        assert_eq!(resolved.fog_near_color, Some(packed([153, 210, 238])));
        assert_eq!(resolved.fog_near, Some(1100.0));
        assert_eq!(resolved.fog_far, Some(9000.0));
        assert_eq!(resolved.direction_xy, Some(200));
        assert_eq!(resolved.direction_z, Some(90));
        assert_eq!(resolved.direction_fade, Some(0.2));
        assert_eq!(resolved.fog_clip, Some(9000.0));
        assert_eq!(resolved.fog_power, Some(0.7));
        assert_eq!(resolved.fog_max, Some(1.0));
        assert_eq!(resolved.light_fade_begin, Some(8000.0));
        assert_eq!(resolved.light_fade_end, Some(9000.0));
        assert_ne!(resolved.directional, cell.directional);
        assert_eq!(
            resolved.inherit, cell.inherit,
            "the flags are the cell's own"
        );
    }

    #[test]
    fn all_eleven_bits_set_resolves_entirely_to_the_template() {
        let cell = parse_lighting(&ALFTAND02_XCLL).unwrap();
        let template = parse_lighting(&ALFTAND02_TEMPLATE).unwrap();
        assert_eq!(cell.inherit, Some(0x7FF));
        let resolved = resolve_lighting(&cell, Some(&template));

        // Alftand02's own ambient is (2,13,15) - nearly black - and the reading
        // where a set bit means "this cell overrides" would render the room with
        // it. The template's (35,61,65) is what the reference shows.
        assert_eq!(resolved.ambient, Some(packed([35, 61, 65])));
        assert_eq!(resolved.directional, Some(packed([56, 86, 92])));
        assert_eq!(resolved.fog_near_color, Some(packed([162, 208, 228])));
        assert_eq!(resolved.fog_near, Some(1500.0));
        assert_eq!(resolved.fog_far, Some(12000.0));
        assert_eq!(resolved.direction_fade, Some(0.0));
        assert_eq!(resolved.fog_clip, Some(12000.0));
        assert_eq!(resolved.light_fade_begin, Some(10000.0));
        assert_eq!(resolved.light_fade_end, Some(11000.0));
    }

    #[test]
    fn without_a_template_the_cells_own_values_stand() {
        let cell = parse_lighting(&ALFTAND02_XCLL).unwrap();
        assert_eq!(resolve_lighting(&cell, None), cell);
    }

    #[test]
    fn a_short_record_leaves_the_later_fields_absent() {
        let full = parse_lighting(&ALFTAND01_TEMPLATE).unwrap();
        assert_eq!(full.ambient, Some(packed([52, 69, 86])));

        for length in [0usize, 4, 20, 39] {
            assert!(
                parse_lighting(&ALFTAND01_TEMPLATE[..length]).is_none(),
                "{length} bytes is not a lighting struct"
            );
        }
        // WindhelmLightingTemplate (0x7BA87) is the 64-byte shape: through the
        // six colour tints, with no specular, no fog-far colour and no flags.
        let short = parse_lighting(&ALFTAND01_TEMPLATE[..64]).expect("64 bytes is readable");
        assert_eq!(short.ambient, Some(packed([52, 69, 86])));
        assert_eq!(short.fog_near, Some(1100.0));
        assert_eq!(short.fog_far, Some(9000.0));
        assert_eq!(short.fog_far_color, None);
        assert_eq!(short.inherit, None, "no flags without the full struct");
        assert_eq!(short.light_fade_begin, None);

        // A cell whose own XCLL is short inherits nothing, whatever its
        // template says: there are no flags to read.
        let cell = parse_lighting(&ALFTAND02_XCLL[..64]).unwrap();
        let template = parse_lighting(&ALFTAND02_TEMPLATE).unwrap();
        assert_eq!(cell.fog_near, Some(340.0));
        assert_eq!(resolve_lighting(&cell, Some(&template)), cell);
    }

    #[test]
    fn a_bit_set_with_no_template_field_falls_back_to_the_cell() {
        // A template whose DATA is 40 bytes has no fog far colour or flags; the
        // cell inherits the fields it can and keeps its own for the rest.
        let cell = parse_lighting(&ALFTAND01_XCLL).unwrap();
        let template = parse_lighting(&ALFTAND01_TEMPLATE[..40]).unwrap();
        let resolved = resolve_lighting(&cell, Some(&template));
        assert_eq!(resolved.directional, Some(packed([60, 80, 89])));
        assert_eq!(resolved.fog_max, Some(1.0), "the cell's own fog max stands");
        assert_eq!(resolved.light_fade_begin, Some(0.0));
    }

    #[test]
    fn reads_the_climate_and_its_weather_list() {
        // SkyrimClimate (0x812): one WLST, SkyrimCloudy at 100%.
        let mut wlst = 0x0001_2F89u32.to_le_bytes().to_vec();
        wlst.extend_from_slice(&100u32.to_le_bytes());
        wlst.extend_from_slice(&0u32.to_le_bytes());
        let subrecords = vec![
            (b"EDID".to_vec(), b"SkyrimClimate\0".to_vec()),
            (b"WLST".to_vec(), wlst),
        ];
        let climate = parse_climate(&subrecords);
        assert_eq!(climate.editor_id.as_deref(), Some("SkyrimClimate"));
        assert_eq!(
            climate.first_weather(),
            Some(ClimateWeather {
                weather: 0x12F89,
                chance: 100,
                global: 0
            })
        );
    }

    #[test]
    fn a_zero_chance_weather_is_skipped_for_the_first_real_one() {
        let mut entries = Vec::new();
        for (weather, chance) in [(0x100u32, 0u32), (0x200, 50), (0x300, 50)] {
            let mut bytes = weather.to_le_bytes().to_vec();
            bytes.extend_from_slice(&chance.to_le_bytes());
            bytes.extend_from_slice(&7u32.to_le_bytes());
            entries.push((b"WLST".to_vec(), bytes));
        }
        let climate = parse_climate(&entries);
        assert_eq!(climate.weathers.len(), 3);
        assert_eq!(climate.first_weather().unwrap().weather, 0x200);

        // Every chance zero: the first entry stands rather than nothing.
        let all_zero: Vec<_> = entries
            .iter()
            .map(|(tag, bytes)| {
                let mut bytes = bytes.clone();
                bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
                (tag.clone(), bytes)
            })
            .collect();
        assert_eq!(
            parse_climate(&all_zero).first_weather().unwrap().weather,
            0x100
        );
        assert!(parse_climate(&[]).first_weather().is_none());
    }

    #[test]
    fn reads_weather_colours_at_the_day_index_and_the_interleaved_fog() {
        // A group's four colours are sunrise, day, sunset, night. Fill each
        // group so the day colour is identifiable.
        let mut nam0 = Vec::new();
        for group in 0..NAM0_GROUPS {
            for time in 0..4 {
                let value = (group * 4 + time + 1) as u8;
                nam0.extend_from_slice(&[value, value, value, 0]);
            }
        }
        let mut fnam = Vec::new();
        for value in [0.0f32, 100000.0, 1000.0, 50000.0, 0.4, 0.3, 0.875, 0.875] {
            fnam.extend_from_slice(&value.to_le_bytes());
        }
        let subrecords = vec![
            (b"EDID".to_vec(), b"SkyrimCloudy\0".to_vec()),
            (b"NAM0".to_vec(), nam0.clone()),
            (b"FNAM".to_vec(), fnam),
            (b"DATA".to_vec(), vec![0; 19]),
        ];
        let weather = parse_weather(&subrecords);

        assert_eq!(weather.editor_id.as_deref(), Some("SkyrimCloudy"));
        assert_eq!(weather.day_colors.len(), NAM0_GROUPS);
        assert_eq!(weather.group(GROUP_SKY_UPPER), Some(packed([2, 2, 2])));
        assert_eq!(weather.group(GROUP_FOG_NEAR), Some(packed([6, 6, 6])));
        assert_eq!(weather.group(GROUP_AMBIENT), Some(packed([14, 14, 14])));
        assert_eq!(weather.group(GROUP_SUNLIGHT), Some(packed([18, 18, 18])));
        assert_eq!(weather.group(GROUP_FOG_FAR), Some(packed([50, 50, 50])));
        assert_eq!(weather.group(NAM0_GROUPS), None, "there is no group 17");

        let fog = weather.fog.expect("32 bytes of FNAM is eight floats");
        assert_eq!(fog.day_near, 0.0);
        assert_eq!(fog.day_far, 100000.0);
        assert_eq!(fog.night_near, 1000.0);
        assert_eq!(fog.night_far, 50000.0);
        assert_eq!(fog.day_power, 0.4);
        assert_eq!(fog.night_power, 0.3);
        assert_eq!(fog.day_max, 0.875);
        assert_eq!(fog.night_max, 0.875);

        // A short NAM0 yields only the groups it holds; a short FNAM no fog.
        let short = parse_weather(&[
            (b"NAM0".to_vec(), nam0[..16].to_vec()),
            (b"FNAM".to_vec(), vec![0; 12]),
        ]);
        assert_eq!(short.day_colors.len(), 1);
        assert_eq!(short.group(0), Some(packed([2, 2, 2])));
        assert_eq!(short.fog, None);
        assert_eq!(parse_weather(&[]).day_colors.len(), 0);
    }

    #[test]
    fn the_day_sunlight_luma_scales_zero_to_one() {
        assert_eq!(packed_luma(packed([0, 0, 0])), 0.0);
        assert!((packed_luma(packed([255, 255, 255])) - 1.0).abs() < 1e-6);
        // BlackreachWeather's day sunlight colour, (0,169,183).
        let luma = packed_luma(packed([0, 169, 183]));
        assert!((luma - 0.526).abs() < 0.005, "{luma}");
    }
}
