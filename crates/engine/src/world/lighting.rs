//! Per-space lighting: what Skyrim's own records say a room or a worldspace is lit and fogged
//! with, as the converter resolved it into the `space_lighting` table (converter schema 16).
//!
//! Before this module the engine had three hard-coded states - sky, interior and underground - and
//! two ambient colours fitted by hand ([`crate::app::INTERIOR_AMBIENT_COLOR`] and
//! [`crate::app::CAVERN_AMBIENT_COLOR`]). Alftand's two halves were the same colour, AlftandZCell
//! came out 4.5x too bright, interiors had their fog deliberately switched off, and Blackreach was
//! drawn against the near-black clear colour where every reference of it is teal
//! (`docs/research/visual-gaps-spec.md`, gap 2).
//!
//! # What the table holds
//!
//! One row per space, keyed by **the space's FormID**: an interior cell's `CELL` id, or a
//! worldspace's `WRLD` id ([`SpaceKey`]). Every chain was resolved once at conversion time - a
//! cell's `XCLL` against the `LGTM` its `LTMP` names, a worldspace's `CNAM` climate to its first
//! `WLST` weather - so there is nothing to look up here but the row itself.
//!
//! The colours are the record's own packed `RGBA` bytes, low byte first ([`packed_rgb`]).
//!
//! # What this module is not
//!
//! It does not decide what a space *looks* like. A row is data; what the engine does with it -
//! which of them is scaled to which calibrated magnitude, what the backdrop is, how far the fog
//! reaches - is [`crate::app::space_atmosphere`], next to the constants it was calibrated with.
//! This module is what the database says, and the arithmetic the two share.
//!
//! A database without the table is not an error. [`SpaceLightingCatalog::open`] probes for it and
//! yields an empty catalog, and every caller falls back to the behaviour the engine had before the
//! table existed: that is the state of the shipped assets until the schema-16 reconversion.

use bevy::prelude::*;
use rusqlite::{Connection, OpenFlags};
use std::{collections::HashMap, path::Path};

/// The three `RGB` bytes of a colour the database packs as one little-endian `u32`, in record
/// order: red in the lowest byte.
///
/// `LIGH`'s colour is read out of the database the same way in `crate::lights`, and the converter
/// writes both with `to_le_bytes`, so a red-low reading is what agrees with the data.
pub fn packed_rgb(packed: u32) -> [u8; 3] {
    [
        (packed & 0xff) as u8,
        ((packed >> 8) & 0xff) as u8,
        ((packed >> 16) & 0xff) as u8,
    ]
}

/// The relative luminance of a colour the database packed, in 0..1: the converter's arithmetic
/// (`crates/converter/src/esm/lighting.rs`), repeated here because the engine reads what it
/// published.
pub fn packed_luma(rgb: [u8; 3]) -> f32 {
    (0.2126 * f32::from(rgb[0]) + 0.7152 * f32::from(rgb[1]) + 0.0722 * f32::from(rgb[2])) / 255.0
}

/// `SkyrimCloudy`'s day sunlight colour, `(177, 155, 150)`: group 4 of `NAM0`, index 1. The weather
/// is the first `WLST` entry of `SkyrimClimate`, which is Tamriel's climate, so this is the daylight
/// the engine's sun was calibrated against (the UESP reference screenshots, 2026-09).
pub const FULL_DAY_SUNLIGHT: [u8; 3] = [177, 155, 150];

/// The `sun_illuminance` a full day measures: the luminance of [`FULL_DAY_SUNLIGHT`].
///
/// The table scales a weather's daylight against this, so the day the engine draws today is still
/// the day it drew before: [`crate::app::DAY_SUN_ILLUMINANCE`] at a luma of exactly this.
///
/// Derived from the record's bytes rather than written out by hand; the test below pins the two
/// against each other so a transcription slip cannot pass.
pub const DAY_ILLUMINANCE_REFERENCE: f32 = 0.624_769_4;

/// The relative luminance of a colour, in whatever space that colour is in: `LinearRgba::from`
/// converts first, so an sRGB author's colour and a linear one come out comparable.
pub fn luma(color: Color) -> f32 {
    let linear = LinearRgba::from(color);
    0.2126 * linear.red + 0.7152 * linear.green + 0.0722 * linear.blue
}

/// `color` scaled so its luminance is `luma` and its hue and saturation are unchanged.
///
/// A colour with no luminance of its own (black) has no hue to keep either, so it is returned as
/// it is rather than divided by zero. The result may exceed 1 in a channel: a saturated dark colour
/// brought up to a daylight luminance is brighter than white in that channel, which is the honest
/// reading of "the record's colour at the calibrated brightness".
pub fn scale_to_luma(color: Color, wanted: f32) -> Color {
    let linear = LinearRgba::from(color);
    let have = 0.2126 * linear.red + 0.7152 * linear.green + 0.0722 * linear.blue;
    if have <= 1.0e-5 || !have.is_finite() {
        return color;
    }
    let scale = wanted / have;
    Color::linear_rgba(
        linear.red * scale,
        linear.green * scale,
        linear.blue * scale,
        linear.alpha,
    )
}

/// `color` moved `amount` of the way towards white (0 keeps it, 1 is white), in linear light.
///
/// A colour at a luminance of 1 keeps that luminance through the blend, because white's is 1: this
/// moves a tint without moving a level. That is what a weather's sun colour needs - the illuminance
/// carries the day's magnitude and the colour only its cast - and the blend is what keeps a record's
/// bytes from being exaggerated by an exposure no game record was written for.
pub fn toward_white(color: Color, amount: f32) -> Color {
    let linear = LinearRgba::from(color);
    let amount = amount.clamp(0.0, 1.0);
    let towards_white = |channel: f32| channel + (1.0 - channel) * amount;
    Color::linear_rgba(
        towards_white(linear.red),
        towards_white(linear.green),
        towards_white(linear.blue),
        linear.alpha,
    )
}

/// The space a camera is in, as `space_lighting` keys it: one FormID, and whether it names a
/// `CELL` (interior) or a `WRLD`.
///
/// `interior` is the active cell id when the camera is inside one, exactly as
/// [`crate::streaming::ActiveCell`] carries it, so an exterior resolves to the worldspace the
/// camera is standing in and not to the grid cell under it - which is what the table stores, since
/// an exterior cell's own `XCLL` is not what the game lights it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpaceKey {
    pub space_id: u32,
    pub is_interior: bool,
}

/// The space named by the active cell's two ids.
pub fn space_key(worldspace_id: u32, interior: Option<u32>) -> SpaceKey {
    match interior {
        Some(cell_id) => SpaceKey {
            space_id: cell_id,
            is_interior: true,
        },
        None => SpaceKey {
            space_id: worldspace_id,
            is_interior: false,
        },
    }
}

/// One resolved `space_lighting` row.
///
/// Every field is optional on the database side and stays optional here: the table's columns are
/// all nullable except the key and `has_sky`, a record can be too short to carry the later fields,
/// and a weather need not publish every colour group. A `None` is "the data does not say", which
/// each caller resolves its own way - never "zero".
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SpaceLighting {
    /// A `CELL` or `WRLD` FormID.
    pub space_id: u32,
    /// Whether the space is an interior cell. The table's own flag, which the key that found the
    /// row has to agree with.
    pub is_interior: bool,
    /// The cell's `LTMP`, already applied.
    pub template_id: Option<u32>,
    /// `XCLL` ambient, or a weather's ambient group.
    pub ambient: Option<[u8; 3]>,
    /// `XCLL` directional, or a weather's sunlight colour.
    pub directional: Option<[u8; 3]>,
    /// `XCLL` fog-near colour, or a weather's fog-near colour.
    pub fog: Option<[u8; 3]>,
    /// Fog near and far distances, in Creation units.
    pub fog_near: Option<f32>,
    pub fog_far: Option<f32>,
    /// The record's fog power (`1` is linear). Carried, not applied: Bevy's [`DistanceFog`] ramps
    /// linearly and has no power, and the interiors this engine draws sit well inside the range
    /// where the difference is small. A space that needs it needs a shader, not this row.
    pub fog_power: Option<f32>,
    /// The record's fog *clip plane* distance, which is not a fog: the game stops drawing past it.
    /// Carried for the same reason.
    pub fog_clip: Option<f32>,
    /// The directional rotation and fade a cell stores for its own sun-ish light. Carried; the
    /// engine's sun is one directional light for the whole world.
    pub direction_rot_xy: Option<i32>,
    pub direction_rot_z: Option<i32>,
    pub direction_fade: Option<f32>,
    /// The weather's sky colours: upper, the far haze at the horizon, and lower.
    pub sky_upper: Option<[u8; 3]>,
    pub sky_fog: Option<[u8; 3]>,
    pub sky_lower: Option<[u8; 3]>,
    /// The weather's sun-disc colour.
    pub sun: Option<[u8; 3]>,
    /// The day's sunlight luminance, 0..1.
    pub sun_illuminance: Option<f32>,
    pub climate_id: Option<u32>,
    pub weather_id: Option<u32>,
    /// Whether a weather resolved for this space: the space is drawn against a sky. `0` replaces
    /// the engine's hard-coded list of underground worldspaces - from the data, and true of the two
    /// that list named, since `BlackreachWeather` is a weather like any other.
    pub has_sky: bool,
}

/// The `space_lighting` table, preloaded at startup.
///
/// Empty - and every caller unchanged - when the database has no such table, which is every
/// database converted before schema 16. The table is small (one row per interior cell with an
/// `XCLL` and one per worldspace: about 600 rows on `Skyrim.esm`) and read once, because
/// `update_atmosphere` asks for one row per camera per frame.
#[derive(Resource, Default, Debug)]
pub struct SpaceLightingCatalog {
    spaces: HashMap<u32, SpaceLighting>,
}

impl SpaceLightingCatalog {
    /// Reads the table out of a converted database.
    ///
    /// A missing file, table or column is not an error: the converter publishes the table with the
    /// rest of schema 16, so a database without it is one the engine draws exactly as it did
    /// before the table existed. A database that merely predates it says so at debug level;
    /// anything else that stops the read is a warning, because that is not the shape of the data
    /// but something wrong with the file.
    pub fn open(path: &Path) -> Self {
        let connection = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(connection) => connection,
            Err(error) => {
                warn!(?path, %error, "cannot open the world database for space lighting; every space keeps the engine's own atmosphere");
                return Self::default();
            }
        };
        if !has_table(&connection, "space_lighting") {
            debug!(
                "the database predates the space lighting table; every space keeps the engine's own atmosphere"
            );
            return Self::default();
        }
        match read_spaces(&connection) {
            Ok(spaces) => {
                info!(
                    spaces = spaces.len(),
                    "space lighting loaded (per-space ambient, fog and sky)"
                );
                Self { spaces }
            }
            Err(error) => {
                warn!(%error, "cannot read the `space_lighting` table; every space keeps the engine's own atmosphere");
                Self::default()
            }
        }
    }

    /// The row for a space, or `None` for a space the table does not carry: an interior cell with
    /// no `XCLL` and no lighting template, a worldspace with no climate, or any space at all in a
    /// database converted before schema 16.
    pub fn get(&self, space_id: u32) -> Option<&SpaceLighting> {
        self.spaces.get(&space_id)
    }

    pub fn len(&self) -> usize {
        self.spaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spaces.is_empty()
    }
}

fn has_table(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .is_ok_and(|count| count > 0)
}

fn read_spaces(connection: &Connection) -> rusqlite::Result<HashMap<u32, SpaceLighting>> {
    let mut statement = connection.prepare(
        "SELECT space_id,is_interior,template_id,ambient,directional,fog,\
         fog_near,fog_far,fog_power,fog_clip,direction_rot_xy,direction_rot_z,direction_fade,\
         sky_upper,sky_fog,sky_lower,sun,sun_illuminance,climate_id,weather_id,has_sky \
         FROM space_lighting",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(SpaceLighting {
            space_id: row.get(0)?,
            is_interior: row.get::<_, i64>(1)? != 0,
            template_id: row.get(2)?,
            ambient: row.get::<_, Option<u32>>(3)?.map(packed_rgb),
            directional: row.get::<_, Option<u32>>(4)?.map(packed_rgb),
            fog: row.get::<_, Option<u32>>(5)?.map(packed_rgb),
            fog_near: row.get(6)?,
            fog_far: row.get(7)?,
            fog_power: row.get(8)?,
            fog_clip: row.get(9)?,
            direction_rot_xy: row.get(10)?,
            direction_rot_z: row.get(11)?,
            direction_fade: row.get(12)?,
            sky_upper: row.get::<_, Option<u32>>(13)?.map(packed_rgb),
            sky_fog: row.get::<_, Option<u32>>(14)?.map(packed_rgb),
            sky_lower: row.get::<_, Option<u32>>(15)?.map(packed_rgb),
            sun: row.get::<_, Option<u32>>(16)?.map(packed_rgb),
            sun_illuminance: row.get(17)?,
            climate_id: row.get(18)?,
            weather_id: row.get(19)?,
            has_sky: row.get::<_, i64>(20)? != 0,
        })
    })?;
    let mut spaces = HashMap::new();
    for space in rows {
        let space = space?;
        spaces.insert(space.space_id, space);
    }
    Ok(spaces)
}

/// The ambient every space of one kind falls back to: the colour and brightness measured against
/// the UESP reference screenshots (2026-09), which a row's own colour is scaled to.
///
/// Three of them, because the engine has three states to keep: an interior, a space with no sky under
/// it, and a space drawn against daylight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmbientBases {
    pub interior: (Color, f32),
    pub cavern: (Color, f32),
    pub sky: (Color, f32),
}

impl AmbientBases {
    /// The base for a space: an interior's own, or a worldspace's by whether its sky *lights* it.
    ///
    /// `daylit` rather than `has_sky`, because a weather that resolved is not the same thing as a
    /// sun: `BlackreachWeather` is published, drawn as a sky and has a measured daylight of zero,
    /// and the space under it is a cave lit by its own ambient whatever its `has_sky` says. Which
    /// is which is the caller's - `crate::app::is_daylit` - and a row that says nothing about its
    /// daylight at all keeps the sky's ambient, because a missing column is not a black sun.
    pub fn for_space(&self, is_interior: bool, daylit: bool) -> (Color, f32) {
        if is_interior {
            self.interior
        } else if daylit {
            self.sky
        } else {
            self.cavern
        }
    }
}

/// A space's sun: one directional light's colour and illuminance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunLight {
    /// The tint, scaled so its luminance is 1: the illuminance carries the magnitude, so a colour
    /// taken from a dark record does not dim the day it is the colour of.
    pub color: Color,
    pub illuminance: f32,
}

impl SunLight {
    /// No sun at all, which is what an interior and an underground worldspace have.
    pub const OFF: SunLight = SunLight {
        color: Color::WHITE,
        illuminance: 0.0,
    };
}

/// Where a space's fog distances come from.
///
/// The split is interior against exterior and **not** sky against underground: the terrain ring is
/// drawn outside, over cells the player is nowhere near, and the fog that hides its far edge has to
/// be the ring's distances whatever the weather says - an underground worldspace has a ring in
/// front of it too. Blackreach is the case a `has_sky` split gets wrong: it is a cavern, and the
/// engine's own terrain ring around it still has to fade into the teal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpaceFog {
    /// The record's own fog, in Creation units.
    Own { near: f32, far: f32 },
    /// An interior the record gives no usable range for: fog that cannot reach anything the space
    /// draws, so a room three thousand units across has no haze in it.
    Unreachable,
    /// An exterior: the terrain ring's distances, which are the ring's "no cliff" guarantee.
    Ring,
}

/// What one space is lit, fogged and drawn against: the numbers a camera is set up with.
///
/// Resolved by [`crate::app::space_atmosphere`] from a row and the engine's calibration. Every
/// field is a decision rather than a record value, which is why the record type above and this one
/// are separate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpaceAtmosphere {
    /// The ambient light of the space, already at its calibrated magnitude.
    pub ambient_color: Color,
    pub ambient_brightness: f32,
    /// The backdrop the space is drawn against where nothing is in front of the camera - the
    /// camera's clear colour - and the colour the fog fades geometry into.
    pub backdrop: Color,
    pub fog: SpaceFog,
    /// The sun. [`SunLight::OFF`] for an interior, and for a worldspace whose weather has no
    /// daylight - which is how Blackreach and the Alftand cavern lose the sun the engine used to
    /// switch off by name.
    pub sun: SunLight,
    /// Whether the space is drawn against a sky. An interior has no sky whatever its row says.
    pub has_sky: bool,
}

/// A `space_lighting` table holding the real values of the spaces the reference screenshots cover.
///
/// Shared by the tests below and by the tests of the atmosphere the engine resolves out of a row
/// ([`crate::app::space_atmosphere`]), so the two are written against one set of numbers: a fixture
/// per module is a fixture that drifts.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// The `space_lighting` DDL, exactly as `crates/converter/src/esm/exporter.rs` writes it.
    /// A column added there and not here is a column this fixture does not test.
    pub(crate) const SCHEMA: &str = "
        CREATE TABLE space_lighting (
            space_id INTEGER PRIMARY KEY,
            is_interior INTEGER NOT NULL,
            template_id INTEGER,
            ambient INTEGER, directional INTEGER, fog INTEGER,
            fog_near REAL, fog_far REAL, fog_power REAL, fog_clip REAL,
            direction_rot_xy INTEGER, direction_rot_z INTEGER, direction_fade REAL,
            sky_upper INTEGER, sky_fog INTEGER, sky_lower INTEGER,
            sun INTEGER, sun_illuminance REAL,
            climate_id INTEGER, weather_id INTEGER,
            has_sky INTEGER NOT NULL
        );";

    /// `Alftand01` (0x152C3), the demo's first interior. Ambient `(40, 82, 87)` and fog
    /// `(153, 210, 238)` at 1100..9000 came from `IceCave_HobsFall_LightingTemplate` through the
    /// cell's `XCLL` inherit flags.
    pub(crate) const ALFTAND01: u32 = 0x0001_52C3;
    /// `Alftand02` (0x56C1B): `(35, 61, 65)` ambient, `(162, 208, 228)` fog, 1500..12000.
    pub(crate) const ALFTAND02: u32 = 0x0005_6C1B;
    /// `AlftandZCell` (0x69858): ambient `(45, 79, 83)` from `IceCaveMedium`, fog 1100..6000.
    pub(crate) const ALFTAND_ZCELL: u32 = 0x0006_9858;
    /// Tamriel (WRLD 0x3C), whose `SkyrimClimate` names `SkyrimCloudy`.
    pub(crate) const TAMRIEL: u32 = 0x0000_003C;
    /// Blackreach (WRLD 0x1EE62) and the Alftand cavern (WRLD 0x69857) share
    /// `BlackreachClimate` -> `BlackreachWeather`, whose daylight is black.
    pub(crate) const BLACKREACH: u32 = 0x0001_EE62;
    pub(crate) const ALFTAND_WORLD: u32 = 0x0006_9857;

    pub(crate) fn pack(rgb: [u8; 3]) -> u32 {
        u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16)
    }

    /// One space's row, as a body of a `space_lighting` insert.
    ///
    /// The columns are **named** rather than filled in the table's order: the table has twenty-one
    /// of them, most of them nullable, and a fixture that counted commas would go on passing after
    /// a column moved. The fields this module reads are the ones it sets.
    #[derive(Default)]
    struct Row {
        space_id: u32,
        is_interior: bool,
        ambient: Option<[u8; 3]>,
        directional: Option<[u8; 3]>,
        fog: Option<[u8; 3]>,
        fog_near: Option<f32>,
        fog_far: Option<f32>,
        fog_power: Option<f32>,
        sky_upper: Option<[u8; 3]>,
        sky_fog: Option<[u8; 3]>,
        sky_lower: Option<[u8; 3]>,
        sun: Option<[u8; 3]>,
        sun_illuminance: Option<f32>,
        has_sky: bool,
    }

    impl Row {
        fn insert(self, connection: &Connection) {
            let colour = |rgb: Option<[u8; 3]>| {
                rgb.map_or_else(|| "NULL".to_owned(), |rgb| pack(rgb).to_string())
            };
            let real = |value: Option<f32>| {
                value.map_or_else(|| "NULL".to_owned(), |value| value.to_string())
            };
            connection
                .execute_batch(&format!(
                    "INSERT INTO space_lighting
                       (space_id, is_interior, ambient, directional, fog, fog_near, fog_far,
                        fog_power, sky_upper, sky_fog, sky_lower, sun, sun_illuminance, has_sky)
                     VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {});",
                    self.space_id,
                    i32::from(self.is_interior),
                    colour(self.ambient),
                    colour(self.directional),
                    colour(self.fog),
                    real(self.fog_near),
                    real(self.fog_far),
                    real(self.fog_power),
                    colour(self.sky_upper),
                    colour(self.sky_fog),
                    colour(self.sky_lower),
                    colour(self.sun),
                    real(self.sun_illuminance),
                    i32::from(self.has_sky),
                ))
                .unwrap();
        }
    }

    /// The real values of the six spaces the brief names, taken from `Skyrim.esm` with
    /// `tools/research/space_lighting_dump.py`: three interiors the engine draws and three
    /// worldspaces, two of which share a climate.
    fn fixture(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        [
            // Alftand01: ambient (40,82,87), fog (153,210,238), 1100..9000, power 0.7. The cell's
            // inherit flags kept its own ambient and took the fog from its template.
            Row {
                space_id: ALFTAND01,
                is_interior: true,
                ambient: Some([40, 82, 87]),
                fog: Some([153, 210, 238]),
                fog_near: Some(1100.0),
                fog_far: Some(9000.0),
                fog_power: Some(0.7),
                ..Row::default()
            },
            // Alftand02: ambient (35,61,65), fog (162,208,228), 1500..12000.
            Row {
                space_id: ALFTAND02,
                is_interior: true,
                ambient: Some([35, 61, 65]),
                fog: Some([162, 208, 228]),
                fog_near: Some(1500.0),
                fog_far: Some(12000.0),
                fog_power: Some(0.7),
                ..Row::default()
            },
            // AlftandZCell: ambient (45,79,83) out of IceCaveMedium, fog (136,217,255) 1100..6000.
            Row {
                space_id: ALFTAND_ZCELL,
                is_interior: true,
                ambient: Some([45, 79, 83]),
                fog: Some([136, 217, 255]),
                fog_near: Some(1100.0),
                fog_far: Some(6000.0),
                fog_power: Some(0.7),
                ..Row::default()
            },
            // Tamriel: SkyrimCloudy at the day index. Ambient (203,220,220), sunlight
            // (177,155,150), fog-near (14,128,156), sky upper (41,97,117), sky fog (139,175,194),
            // sky lower (94,149,179), sun (129,105,107), fog 0..100000.
            Row {
                space_id: TAMRIEL,
                ambient: Some([203, 220, 220]),
                directional: Some(FULL_DAY_SUNLIGHT),
                fog: Some([14, 128, 156]),
                fog_near: Some(0.0),
                fog_far: Some(100_000.0),
                fog_power: Some(0.4),
                sky_upper: Some([41, 97, 117]),
                sky_fog: Some([139, 175, 194]),
                sky_lower: Some([94, 149, 179]),
                sun: Some([129, 105, 107]),
                sun_illuminance: Some(packed_luma(FULL_DAY_SUNLIGHT)),
                has_sky: true,
                ..Row::default()
            },
            // Blackreach and the Alftand cavern share BlackreachClimate -> BlackreachWeather: a
            // near-black ambient with a teal fog and a teal horizon, and a daylight of (0,0,0).
            Row {
                space_id: BLACKREACH,
                ambient: Some([10, 11, 12]),
                directional: Some([0, 0, 0]),
                fog: Some([0, 169, 183]),
                fog_near: Some(2048.0),
                fog_far: Some(120_000.0),
                fog_power: Some(0.4),
                sky_upper: Some([0, 169, 183]),
                sky_fog: Some([14, 156, 156]),
                sky_lower: Some([0, 0, 0]),
                sun: Some([0, 0, 0]),
                sun_illuminance: Some(0.0),
                has_sky: true,
                ..Row::default()
            },
            Row {
                space_id: ALFTAND_WORLD,
                ambient: Some([10, 11, 12]),
                directional: Some([0, 0, 0]),
                fog: Some([0, 169, 183]),
                fog_near: Some(2048.0),
                fog_far: Some(120_000.0),
                fog_power: Some(0.4),
                sky_upper: Some([0, 169, 183]),
                sky_fog: Some([14, 156, 156]),
                sky_lower: Some([0, 0, 0]),
                sun: Some([0, 0, 0]),
                sun_illuminance: Some(0.0),
                has_sky: true,
                ..Row::default()
            },
        ]
        .into_iter()
        .for_each(|row| row.insert(&connection));
    }

    /// The fixture rows, in a catalog every caller reads them out of. The directory is returned
    /// with it only because it owns the file.
    pub(crate) fn real_spaces() -> (tempfile::TempDir, SpaceLightingCatalog) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lighting.db");
        fixture(&path);
        let catalog = SpaceLightingCatalog::open(&path);
        (directory, catalog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::lighting::fixtures::{
        ALFTAND_WORLD, ALFTAND_ZCELL, ALFTAND01, ALFTAND02, BLACKREACH, SCHEMA, TAMRIEL, pack,
        real_spaces,
    };

    #[test]
    fn reads_the_space_rows_the_converter_publishes() {
        let (_directory, catalog) = real_spaces();
        assert_eq!(catalog.len(), 6);

        let alftand01 = catalog.get(ALFTAND01).expect("Alftand01 has a row");
        assert!(alftand01.is_interior);
        assert_eq!(alftand01.ambient, Some([40, 82, 87]));
        assert_eq!(alftand01.fog, Some([153, 210, 238]));
        assert_eq!(alftand01.fog_near, Some(1100.0));
        assert_eq!(alftand01.fog_far, Some(9000.0));
        assert_eq!(alftand01.fog_power, Some(0.7));
        assert!(
            !alftand01.has_sky,
            "an interior is never drawn against a sky"
        );
        assert_eq!(alftand01.sky_fog, None);

        // The two Alftand cells differ in every field the reference screenshots show: the engine's
        // one interior colour could not tell them apart, and that is the whole point of the table.
        let alftand02 = catalog.get(ALFTAND02).expect("Alftand02 has a row");
        assert_ne!(alftand01.ambient, alftand02.ambient);
        assert_ne!(alftand01.fog_far, alftand02.fog_far);

        // AlftandZCell is the darkest of the three in the record and the one the engine drew 4.5x
        // too bright: its ambient is (45, 79, 83) out of `IceCaveMedium` and its fog range is the
        // shortest of them.
        let zcell = catalog.get(ALFTAND_ZCELL).expect("AlftandZCell has a row");
        assert_eq!(zcell.ambient, Some([45, 79, 83]));
        assert_eq!(zcell.fog_far, Some(6000.0));

        let tamriel = catalog.get(TAMRIEL).expect("Tamriel has a row");
        assert!(!tamriel.is_interior);
        assert!(tamriel.has_sky);
        assert_eq!(tamriel.ambient, Some([203, 220, 220]));
        assert_eq!(tamriel.directional, Some([177, 155, 150]));
        assert_eq!(tamriel.sky_fog, Some([139, 175, 194]));
        assert_eq!(tamriel.sky_upper, Some([41, 97, 117]));
        assert_eq!(tamriel.sky_lower, Some([94, 149, 179]));
        assert_eq!(tamriel.sun, Some([129, 105, 107]));
        assert_eq!(tamriel.fog_far, Some(100_000.0));
        assert!(
            (tamriel.sun_illuminance.expect("a day") - DAY_ILLUMINANCE_REFERENCE).abs() < 1.0e-6
        );

        // A cavern's weather is a weather: `has_sky` is set, and everything that makes it a cavern
        // is in the colours - a near-black ambient and a daylight of zero.
        let blackreach = catalog.get(BLACKREACH).expect("Blackreach has a row");
        assert!(blackreach.has_sky);
        assert_eq!(blackreach.ambient, Some([10, 11, 12]));
        assert_eq!(blackreach.fog, Some([0, 169, 183]));
        assert_eq!(blackreach.sky_fog, Some([14, 156, 156]));
        assert_eq!(blackreach.sun_illuminance, Some(0.0));

        // The Alftand cavern shares Blackreach's climate and comes out the same in every field but
        // its key: the two worldspaces the engine used to switch off by name are one weather, and
        // the table says so rather than the engine.
        let alftand_world = catalog.get(ALFTAND_WORLD).expect("the cavern has a row");
        assert_eq!(alftand_world.ambient, blackreach.ambient);
        assert_eq!(alftand_world.fog, blackreach.fog);
        assert_eq!(alftand_world.sky_fog, blackreach.sky_fog);
        assert_eq!(alftand_world.sun, blackreach.sun);
        assert_eq!(alftand_world.sun_illuminance, blackreach.sun_illuminance);
        assert!(alftand_world.has_sky);

        // A space the table does not carry is not a row of zeroes.
        assert!(catalog.get(0x0009_9999).is_none());
        assert!(catalog.get(0x0002_D4E0).is_none(), "a cell with no XCLL");
    }

    #[test]
    fn the_packed_colour_is_read_with_red_in_the_low_byte() {
        // The one test that would catch reading the colour big-endian: a pure red record is
        // `0x000000FF`, which a big-endian read would call blue.
        assert_eq!(packed_rgb(0x0000_00FF), [255, 0, 0]);
        assert_eq!(packed_rgb(0x0000_FF00), [0, 255, 0]);
        assert_eq!(packed_rgb(0x00FF_0000), [0, 0, 255]);
        // And the value the fixture writes for Alftand01's ambient, byte for byte as the record
        // holds it (`28 52 57 00`).
        assert_eq!(packed_rgb(pack([40, 82, 87])), [40, 82, 87]);

        // The luminance is the converter's own weighted sum, not an average: green weighs more than
        // the other two together.
        assert!((packed_luma([0, 255, 0]) - 0.7152).abs() < 1.0e-6);
        assert!((packed_luma([255, 0, 0]) - 0.2126).abs() < 1.0e-6);
        assert_eq!(packed_luma([0, 0, 0]), 0.0);
    }

    #[test]
    fn the_day_the_sun_was_calibrated_on_is_skyrim_cloudys_daylight() {
        // The constant is transcribed from the record's bytes; the arithmetic is the converter's.
        assert!(
            (DAY_ILLUMINANCE_REFERENCE - packed_luma(FULL_DAY_SUNLIGHT)).abs() < 1.0e-6,
            "{DAY_ILLUMINANCE_REFERENCE} is not the luminance of {FULL_DAY_SUNLIGHT:?}"
        );
        assert!((DAY_ILLUMINANCE_REFERENCE - 0.6248).abs() < 1.0e-4);
        // It has to be a day, not a night: a reference below a quarter would make every weather's
        // sun brighter than the sun this engine drew before, and that is a property of the number
        // rather than of this run.
        const { assert!(DAY_ILLUMINANCE_REFERENCE > 0.25) };
    }

    #[test]
    fn a_color_is_scaled_to_a_luminance_without_moving_its_hue() {
        // Alftand01's ambient (40, 82, 87) is dark; brought up to the calibrated interior ambient's
        // luminance it keeps the ratio between its channels.
        let scaled = scale_to_luma(Color::srgb_u8(40, 82, 87), 0.6780);
        assert!((luma(scaled) - 0.6780).abs() < 1.0e-4);
        let linear = LinearRgba::from(scaled);
        let original = LinearRgba::from(Color::srgb_u8(40, 82, 87));
        let ratio = |a: f32, b: f32| a / b;
        assert!(
            (ratio(linear.green, linear.red) - ratio(original.green, original.red)).abs() < 1.0e-3,
            "the hue moved"
        );
        assert!(
            (ratio(linear.blue, linear.green) - ratio(original.blue, original.green)).abs()
                < 1.0e-3
        );

        // Black has no hue to keep and no luminance to divide by: it comes back as it went in
        // rather than as a NaN or an infinity.
        let black = scale_to_luma(Color::BLACK, 0.5);
        assert_eq!(black, Color::BLACK);
    }

    #[test]
    fn a_color_moved_towards_white_keeps_its_luminance_and_loses_its_cast() {
        // The sun the references were shot under: `SkyrimCloudy`'s `(129, 105, 107)` at the
        // luminance the daylight gives it. Half way to white is still a warm grey, and it is the
        // same amount of light - which is what makes the blend a tint and not an exposure.
        let record = scale_to_luma(Color::srgb_u8(129, 105, 107), 1.0);
        let half = toward_white(record, 0.5);
        assert!((luma(record) - 1.0).abs() < 1.0e-4);
        assert!((luma(half) - 1.0).abs() < 1.0e-4);
        assert!(LinearRgba::from(half).red < LinearRgba::from(record).red);
        assert!(LinearRgba::from(half).green > LinearRgba::from(record).green);

        // The ends of the blend are the two colours it is between.
        assert_eq!(toward_white(record, 0.0), record);
        assert_eq!(toward_white(record, 1.0), Color::WHITE);
        assert_eq!(toward_white(Color::WHITE, 0.5), Color::WHITE);
        // And an amount outside 0..1 is not a colour outside the space it is in.
        assert_eq!(toward_white(record, -1.0), record);
        assert_eq!(toward_white(record, 2.0), Color::WHITE);
    }

    #[test]
    fn the_key_of_a_space_is_the_cell_inside_and_the_worldspace_outside() {
        assert_eq!(
            space_key(TAMRIEL, Some(ALFTAND01)),
            SpaceKey {
                space_id: ALFTAND01,
                is_interior: true
            }
        );
        assert_eq!(
            space_key(TAMRIEL, None),
            SpaceKey {
                space_id: TAMRIEL,
                is_interior: false
            }
        );
        // The worldspace an interior was entered from is not the space the camera is in: a key that
        // took it would light every interior as Tamriel.
        assert_ne!(space_key(TAMRIEL, Some(ALFTAND01)).space_id, TAMRIEL);
    }

    #[test]
    fn a_database_without_the_table_is_empty_and_the_engine_is_unchanged() {
        // The published schema-15 database: the table the feature reads is not there, and every
        // caller has to fall back to the atmosphere the engine had before it existed.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("old.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE cells (id INTEGER PRIMARY KEY);")
            .unwrap();
        let catalog = SpaceLightingCatalog::open(&path);
        assert_eq!(catalog.len(), 0);
        assert!(catalog.is_empty());
        assert!(catalog.get(ALFTAND01).is_none());
        assert!(catalog.get(TAMRIEL).is_none());

        // A file that is not a database is not fatal either: a run with the wrong path still has to
        // draw something.
        let missing = SpaceLightingCatalog::open(&directory.path().join("not-there.db"));
        assert!(missing.is_empty());
    }

    #[test]
    fn a_row_missing_its_columns_is_read_as_the_columns_the_table_has() {
        // Every colour and distance column is nullable. A row that fills none of them still loads,
        // and the callers see `None` rather than a zero that would be a black room.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("partial.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(
                "INSERT INTO space_lighting (space_id, is_interior, has_sky) VALUES (7, 1, 0);",
            )
            .unwrap();
        let catalog = SpaceLightingCatalog::open(&path);
        let space = catalog.get(7).expect("the row is there");
        assert_eq!(space.ambient, None);
        assert_eq!(space.fog_near, None);
        assert_eq!(space.sun_illuminance, None);
    }
}
