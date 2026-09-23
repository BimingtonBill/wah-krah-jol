//! The engine's sky, fog, ambient and per-space lighting: what one space is lit, fogged and drawn
//! against, and the system that writes it onto the camera, the global ambient and the sun.
//!
//! A space's answer comes from the game's own records where the database has them - the
//! `space_lighting` rows of the converter, read by [`crate::world::lighting`] - and every constant
//! here is either what a record is scaled to or what a space falls back on when the database has no
//! row at all ([`fallback_atmosphere`]). The calibrations, and the reference screenshots they were
//! fitted against, are in the comments below and in `docs/research/light-and-exposure-audit.md`.
//!
//! `AtmospherePlugin` is added by `crate::app::run` for exactly the runs [`atmosphere_applies`]
//! names. It is a module of its own rather than part of `app.rs` because this is the shape the
//! lighting goes upstream in (`docs/design/portal-plugin.md`, step 6).

use crate::{
    // The terrain ring's fog and far plane. They are `app.rs`'s today; the step after this one
    // moves them to `crate::terrain_ring`, and then only this line changes.
    app::{exterior_fog, fog_off},
    config::EngineConfig,
    streaming::ActiveCell,
    world::{
        components::StreamingCamera,
        lighting::{
            AmbientBases, DAY_ILLUMINANCE_REFERENCE, SpaceAtmosphere, SpaceFog, SpaceKey,
            SpaceLighting, SpaceLightingCatalog, SunLight, luma, packed_luma, scale_to_luma,
            space_key, toward_white,
        },
    },
};
use bevy::{camera::ClearColorConfig, prelude::*};

/// The sky, the fog, the ambient and the per-space lighting, in one plugin.
///
/// This is the registration `crate::app::run` used to do inline for the runs that draw a sky: the
/// clear colour a space's backdrop replaces, and [`update_atmosphere`], which writes that backdrop
/// - the space's fog, ambient and sun with it - once the camera knows which space it stands in.
pub struct AtmospherePlugin;

impl Plugin for AtmospherePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(SKY_COLOR))
            .add_systems(Update, update_atmosphere);
    }
}

/// Whether a run gets the sky, the ambient and the distance fog. Interactive runs are looked at, so
/// they always do; a measured run keeps the flat lighting its numbers were taken with - unless it
/// draws the terrain ring, which without the fog would end in a cliff of terrain against whatever
/// colour the frame was cleared with. A measured run therefore asks for the ring by name
/// (`--terrain-radius`) and takes the fog with it.
pub(crate) fn atmosphere_applies(interactive: bool, config: &EngineConfig) -> bool {
    interactive || config.terrain_radius > config.stream_radius
}

pub(crate) const SKY_COLOR: Color = Color::srgb(0.52, 0.64, 0.80);
pub(crate) const UNDERGROUND_COLOR: Color = Color::srgb(0.015, 0.02, 0.035);

/// The interior ambient brightness, in Bevy's ambient units. [`crate::lights::LIGHT_EXPOSURE`] is
/// measured in multiples of the ambient an interior actually applies - this times
/// [`INTERIOR_AMBIENT_LEVEL`], 480 - so the two knobs stay tied together if either one moves.
///
/// Measured against the UESP reference screenshots (2026-09); see [`INTERIOR_AMBIENT_COLOR`].
pub const INTERIOR_AMBIENT_BRIGHTNESS: f32 = 800.0;
/// A pale green, not the warm Dwemer lamplight it used to be. It is the floor under the
/// `LIGH` references of `crate::lights`, the light that reaches where no `LIGH` reference stands,
/// and the references say that floor is cold: Alftand is a glacial ruin whose ice and stone read
/// green-teal in every screenshot. Measured as the colour of the dimmest ordinary surfaces (the
/// 15th..45th percentile band) of the Alftand02 and AlftandZCell references against the same band
/// of the render: red over green 0.63 where the render had 1.30, blue over green 0.90 where it had
/// 0.58. The torches and lamps that *should* be warm are warm on their own, through
/// `crate::lights` - the ambient does not have to carry that.
pub const INTERIOR_AMBIENT_COLOR: Color = Color::srgb(0.73, 0.87, 0.86);
/// Blackreach and the Alftand cavern: the green-teal of glowing fungus and water rather than the
/// blue it used to be. The blue was too blue - on the AlftandWorld reference's dim surfaces
/// the render's blue over green was 2.55 where the reference's was 0.94, and on Blackreach's 2.09
/// against 1.18 - and a little too red as well. Blackreach wants less red than AlftandWorld does,
/// because one huge orange light of its own reaches the camera there (see
/// `crate::lights::LIGHT_EXPOSURE`), so this is one colour between the two.
pub const CAVERN_AMBIENT_COLOR: Color = Color::srgb(0.35, 0.65, 0.73);
/// The cavern ambient brightness. Measured against the Blackreach and AlftandWorld references,
/// whose median pixel is several times darker than an interior's.
pub const CAVERN_AMBIENT_BRIGHTNESS: f32 = 650.0;
/// Exterior worldspaces that are underground in Skyrim.esm: Blackreach (WRLD 0001EE62) and the
/// Alftand cavern it is reached through (WRLD 00069857).
///
/// The list is the fallback, not the rule: a space whose `space_lighting` row carries a `has_sky`
/// of its own is lit and drawn by the row (see [`space_atmosphere`]), and the two worldspaces here
/// are exactly the two whose weather has a daylight of zero. A database converted before schema 16
/// has no rows at all and every space keeps using this list.
const UNDERGROUND_WORLDSPACES: [u32; 2] = [0x0001_EE62, 0x0006_9857];

/// The daylight ambient of a worldspace drawn against a sky: the sun does the work outside, so this
/// is a fill light, not a level. Kept from before the `space_lighting` table existed, and used both
/// as that space's fallback ambient and as the magnitude a row's own ambient colour is scaled to.
pub(crate) const SKY_AMBIENT_COLOR: Color = Color::srgb(0.48, 0.55, 0.7);
pub(crate) const SKY_AMBIENT_BRIGHTNESS: f32 = 160.0;

/// How much of the ambient's colour and brightness Bevy's ambient puts on a surface: the diffuse
/// half of `EnvBRDFApprox`, at the roughness its call site asks for it at.
///
/// `bevy_pbr/src/render/pbr_ambient.wgsl` writes
/// `EnvBRDFApprox(diffuse_color, F_AB(1.0, NdotV)) * lights.ambient_color.rgb`, and
/// `EnvBRDFApprox(F0, ab)` is `F0 * ab.x + ab.y` (`bevy_pbr/src/render/pbr_lighting.wgsl`). The
/// `1.0` there is the roughness the *call site* passes, not the material's, so it is the same
/// number for every material in the scene - which is what makes this reducible to one constant.
/// `F_AB(1.0, NdotV)` gives `r = (0.0, 0.015, 0.468, -0.018)` whatever `NdotV` is, so
/// `a004 = r.y = 0.015` and `ab = (-1.04, 1.04) * 0.015 + (0.468, -0.018) = (0.4524, -0.0024)`.
/// A surface therefore gets `albedo * (0.4524 * ambient - 0.0024)`.
///
/// The offset is dropped: it is under 2% of the term beside it at the dimmest ambient this engine
/// lights a space with, and dropping it keeps [`SKY_FILL`]'s arithmetic a plain product.
///
/// Nothing here is `NdotL`: ambient light falls on a surface facing away from the sun as much as on
/// one facing it. That is the shape the fill has to be solved against - and the reason the fill is
/// flat. The specular half of the same expression (`F_AB(perceptual_roughness, NdotV)` against the
/// material's own `F0`) is not albedo-proportional and is not part of this constant.
const AMBIENT_DIFFUSE_WEIGHT: f32 = 0.4524;

/// The light the sky puts on a surface, as a fraction of the light the sun puts on the same surface
/// facing it. A daylit space's ambient brightness is derived from that space's own sun with it (see
/// [`sky_fill_brightness`]), so this is the whole fill knob for a day.
///
/// The engine used to give every daylit space one fixed fill - [`SKY_AMBIENT_BRIGHTNESS`] at
/// [`SKY_AMBIENT_LEVEL`] - whatever its weather's sun was worth, which made the day the sun alone
/// and left a surface the sun does not reach with nothing to light it: the audit measures the
/// direct-to-ambient ratio of a Riverwood midday frame at 92:1 where the reference screenshots of
/// the same place read about 3:1, and the shadowed half of the frame nearly black
/// (`docs/research/light-and-exposure-audit.md`). The reference day is a soft overcast one, whose
/// shadow across the path still reads as the same surface as the sunlit ground beside it.
///
/// So the fill is a fraction of the space's own sun rather than a level of its own:
/// [`sky_fill_brightness`] solves `AMBIENT_DIFFUSE_WEIGHT * luma(colour) * brightness` against
/// `SKY_FILL * illuminance / PI`. At 0.5 a surface in shadow gets `SKY_FILL / (1 + SKY_FILL)` =
/// **1/3** of a surface in the sun, and [`DAY_SUN_LEVEL`] is divided by `1 + SKY_FILL` so that the
/// sunlit surface keeps the brightness the fit was taken at instead of gaining the fill on top.
///
/// Bevy's ambient is unoccluded and normal-independent (see [`AMBIENT_DIFFUSE_WEIGHT`]), so this
/// fill lands on a surface facing away from the sun exactly as much as on one facing it. That is
/// the flat, hazy look the reference day has; an occlusion or hemisphere term is a different change.
const SKY_FILL: f32 = 0.5;

/// The three calibrated ambient levels, one per kind of space, as the `space_lighting` resolver
/// wants them. These are the numbers fitted against the UESP reference screenshots (2026-09); a
/// space's own colour from the record is scaled to the luminance of the base its kind picks and
/// keeps the base's brightness, so the table moves the *hue* of a space and not the exposure the
/// references were signed off at.
///
/// This is the **fallback's** set, and a database without the `space_lighting` table draws exactly
/// what it drew before the table existed. The table's own set is [`SPACE_AMBIENT_BASES`], which the
/// same reference screenshots were re-fitted to once the records were in play.
const AMBIENT_BASES: AmbientBases = AmbientBases {
    interior: (INTERIOR_AMBIENT_COLOR, INTERIOR_AMBIENT_BRIGHTNESS),
    cavern: (CAVERN_AMBIENT_COLOR, CAVERN_AMBIENT_BRIGHTNESS),
    sky: (SKY_AMBIENT_COLOR, SKY_AMBIENT_BRIGHTNESS),
};

/// How much of each kind's calibrated ambient a space the `space_lighting` table lights gets.
///
/// The fallback keeps its own levels ([`AMBIENT_BASES`]) because a database without the table has
/// to draw what it drew before the table existed; these are the same three colours, one per kind,
/// re-fitted against the same UESP reference set now that a record brings every space its own hue
/// and the table lights spaces the engine's three hard-coded states used to lump together.
///
/// **Interior, 0.6.** The engine's one interior level was fitted on the wrong room. With the
/// record's own ambient every Alftand cell draws, Alftand02 (the Animonculory) sat at 1.12x its
/// reference, Alftand01's corridor at 1.78x and AlftandZCell at 4.81x - and the three do not
/// separate by anything in their records, so one shared floor has to carry all of them. 0.6 of the
/// level puts Alftand01 at 1.15x and ZCell at 2.97x, and takes Alftand02 to 0.82x over all seven
/// of its shots and 0.89x over the four of them that are even-numbered. ZCell cannot be reached by
/// this knob at all: its reference is a dark vaulted room while its pose (`area only`) renders a
/// flat lit wall, and the ambient is 95% of that frame's median - closing it would need a level
/// near 0.1, which would take Alftand02 and Alftand01 to a third of their references.
///
/// **Cavern, 1.0** - the magnitude the engine already had. It is the level Blackreach and the
/// Alftand cavern are lit at once their weather's zero daylight stops them taking a daylight fill
/// (`is_daylit`), and measured against their references they land at 1.33x and 1.14x.
///
/// **Sky, 4.0.** The outdoor ambient is a *fill*: the sun does the work, and the frame's median
/// moves 10% for a fourfold fill, so this is not a level knob for the whole exterior - which is
/// why it is set by the shadows and not by the median. The reference day is overcast, and the
/// surfaces the sun does not reach (the Ruined Tower's stone, the shadows under the terrain) are
/// what say how much fill there is: at the engine's 160 the daylight renders' 5th percentile was
/// 0.012 where the reference set's is 0.027 and 35.5% of their pixels were under 0.05 where the
/// reference has 29.7%; at 640 those are 0.025 and 30.7%. The 160 was fitted as a fill under a
/// white 12,000 sun when the camera wrote an 8-bit image, and this is the same dialogue with the
/// reference set the other constants are.
///
/// **This level is now the *fallback* fill.** A daylit space with a sun takes its brightness from
/// that sun instead ([`SKY_FILL`], [`sky_fill_brightness`]): one fixed level could not be right for
/// a weather's own daylight and it was the reason the sun carried the whole day. 4.0 stays because
/// it is what a daylit space that publishes no daylight at all keeps - the half-filled row whose
/// missing column must not become a black room - and because a database without the
/// `space_lighting` table still draws the day it drew before the table existed.
/// Not private like its two neighbours, because [`crate::lights`] states its intensity scale
/// against the ambient an interior applies - [`INTERIOR_AMBIENT_BRIGHTNESS`] *times this level* -
/// and a duplicate of the number there could drift away from the one the room is lit with.
pub const INTERIOR_AMBIENT_LEVEL: f32 = 0.6;
const CAVERN_AMBIENT_LEVEL: f32 = 1.0;
const SKY_AMBIENT_LEVEL: f32 = 4.0;

/// The three ambient levels the `space_lighting` resolver scales a record's colour to: the
/// calibrated colours above, at the level each kind measures at with a record in play.
const SPACE_AMBIENT_BASES: AmbientBases = AmbientBases {
    interior: (
        INTERIOR_AMBIENT_COLOR,
        INTERIOR_AMBIENT_BRIGHTNESS * INTERIOR_AMBIENT_LEVEL,
    ),
    cavern: (
        CAVERN_AMBIENT_COLOR,
        CAVERN_AMBIENT_BRIGHTNESS * CAVERN_AMBIENT_LEVEL,
    ),
    sky: (
        SKY_AMBIENT_COLOR,
        SKY_AMBIENT_BRIGHTNESS * SKY_AMBIENT_LEVEL,
    ),
};

/// The sun of a full day, in Bevy's illuminance units: the calibrated magnitude, which a
/// weather's own daylight scales (see [`sky_sun`]). The engine had this number inline before the
/// table existed.
pub const DAY_SUN_ILLUMINANCE: f32 = 12_000.0;

/// The daylight a space the `space_lighting` table lights is drawn at, as a multiple of
/// [`DAY_SUN_ILLUMINANCE`]: the magnitude the weather's own daylight scales.
///
/// The engine's 12,000 was fitted against the reference screenshots when the camera wrote an 8-bit
/// image directly. The glow change put the camera on an `Hdr` target with a tonemapping pass
/// (23adbb4), and the same illumination now renders darker: the backdrop colour, whose record value
/// is a known quantity, comes out at 0.68 of it. So the day's magnitude is re-fitted the way every
/// other constant here is, against the reference set. Not a change to [`DAY_SUN_ILLUMINANCE`]
/// itself, which the fallback path still draws ([`fallback_atmosphere`]) - a database without the
/// table keeps the day it had.
///
/// Measured on the nine Tamriel reference poses: at 1.0 the pooled median is 0.119, where the
/// reference set's is 0.171; at 1.6 it is 0.164, and the response is linear between them, so 1.6 is
/// the fitted answer rather than the largest one that still fits. The sun at Tamriel is then worth
/// 1.6 x 12,000 x 1.144, the weather's own daylight over the engine's reference day.
///
/// **That 1.6 is now the whole day's magnitude, sun and fill together.** The sky's fill is
/// [`SKY_FILL`] of the sun, so the sun carries `1 / (1 + SKY_FILL)` of it and the space's ambient
/// the remaining share: the sum on a surface facing the sun is the surface the 1.6 was fitted on,
/// and the surfaces the sun does not reach are lit by the fill rather than by nothing. Written as
/// the division rather than as 1.0667 so the two constants cannot drift apart.
const DAY_SUN_LEVEL: f32 = 1.6 / (1.0 + SKY_FILL);

/// How much of a weather's sun colour survives into the directional light: `1` is the record's own
/// tint and `0` a white sun; the rest is the blend towards white of [`toward_white`].
///
/// Not `1`, because this one directional light stands in for a whole sky. The record's `FNAM`/`NAM0`
/// sun colour describes the sun *disc* - a small, very warm object - while what lights the ground on
/// an overcast day like the one the references were shot on is the sky around it, which is pale and
/// nearly neutral. Taken at full strength the disc's `(129, 105, 107)` becomes a red-heavy
/// illuminant and the snow renders lilac, which is the one thing the reference screenshots never
/// show: the frame's mean red over green measured 1.07 where the reference's is 0.78, and the snow
/// in `local/reference/cmp-s17/SR-place-Alftand_Ruined_Tower.jpg` reads lilac. At three quarters
/// the same measure is 0.90 - on the cool side of neutral, with the record's warmth still in it.
///
/// Not `0` either: a fully white sun is a different weather's sky, and the record's warmth is data
/// this engine has no other reason to throw away.
const SUN_TINT_STRENGTH: f32 = 0.75;

/// What one space is lit, fogged and drawn against.
///
/// The space is [`SpaceKey`] - an interior cell id, or a worldspace id - and the answer comes from
/// its `space_lighting` row when the database has one. A database converted before schema 16 has
/// none, and every space then gets exactly what this engine drew before the table existed
/// ([`fallback_atmosphere`]), which is what keeps the current assets working unchanged.
pub(crate) fn space_atmosphere(
    catalog: Option<&SpaceLightingCatalog>,
    key: SpaceKey,
) -> SpaceAtmosphere {
    let Some(row) = catalog.and_then(|catalog| catalog.get(key.space_id)) else {
        return fallback_atmosphere(key);
    };
    // What the engine drew this space with before the table existed. It is the answer for every
    // field the row leaves out, so a row half-filled by a plugin still draws a space rather than a
    // black room with a hole where the sky was.
    let engine = fallback_atmosphere(key);
    // An interior has no sky whatever the row says: its `XCLL` carries no weather, the converter
    // leaves `has_sky` clear for it, and a space with a roof over it can never be drawn against
    // one. The flag is the converter's, so a `has_sky` set on a cell is a bug in the data and not
    // a sky to draw.
    let has_sky = row.has_sky && !key.is_interior;
    // Whether that sky is what lights the space. A weather whose daylight is a measured zero is
    // published, is drawn as a sky, and is not a sun: Blackreach and the Alftand cavern are lit by
    // their own teal ambient with no sun over them, and a space under one is a cave however its
    // `has_sky` reads. Its own record says so - `sun_illuminance` is 0 and the sun colour is black
    // - where the engine before the table had to name the two worldspaces by FormID
    // ([`UNDERGROUND_WORLDSPACES`], which stays the fallback for a database without the table).
    let daylit = has_sky && is_daylit(row);
    let sun = if has_sky { sky_sun(row) } else { SunLight::OFF };
    // The magnitude is the engine's calibrated one for this kind of space and the record brings the
    // hue. That is the whole calibration argument: the exposure was fitted against the reference
    // screenshots, and a record's ambient is a *colour*, not a level - taken raw it is ten times
    // darker than the reference they were fitted to. A *daylit* space is the one exception: its
    // level is its own sun's share of the day ([`sky_fill_brightness`]), because a daytime exterior
    // is lit by the sun over it and one fixed fill cannot be right for every weather's daylight. A
    // daylit space whose sun is off - a row that publishes no daylight column at all, so `sky_sun`
    // hands back [`SunLight::OFF`] - keeps the base level instead: a missing column is not a black
    // sky, and a half-filled row must not become a black room.
    let base = SPACE_AMBIENT_BASES.for_space(key.is_interior, daylit);
    let brightness = if daylit && sun.illuminance > 0.0 {
        sky_fill_brightness(sun.illuminance, base.0)
    } else {
        base.1
    };
    let (ambient_color, ambient_brightness) = match row.ambient {
        // A record with a black ambient has no hue to take, and `scale_to_luma` hands back the base
        // unchanged rather than a division by zero.
        Some(rgb) => (scale_to_luma(srgb_u8(rgb), luma(base.0)), brightness),
        None => (base.0, brightness),
    };
    // Only an interior's fog is its own: an exterior's fog is the terrain ring's, which is what
    // keeps the ring from ending in a cliff, and the weather's own near/far (0 to 100,000 for
    // Tamriel) is a distance the engine has nothing to draw at.
    let fog = if key.is_interior {
        match row
            .fog_near
            .zip(row.fog_far)
            .filter(|(near, far)| usable_fog(*near, *far))
        {
            Some((near, far)) => SpaceFog::Own { near, far },
            None => SpaceFog::Unreachable,
        }
    } else {
        SpaceFog::Ring
    };
    SpaceAtmosphere {
        ambient_color,
        ambient_brightness,
        backdrop: backdrop_of(row, has_sky, engine.backdrop),
        fog,
        sun,
        has_sky,
    }
}

/// The ambient brightness a daylit space is lit at: [`SKY_FILL`] of the light its own sun puts on a
/// surface facing it, spread over every surface the way Bevy's ambient spreads it.
///
/// Bevy's sun contributes `albedo * NdotL * illuminance / PI` to a surface (the `1/PI` is Lambert's;
/// `Fd_Burley` in `bevy_pbr/src/render/pbr_lighting.wgsl`) and Bevy's ambient contributes
/// `albedo * AMBIENT_DIFFUSE_WEIGHT * colour * brightness` with no `NdotL` at all. Asking the second
/// to be [`SKY_FILL`] of the first at `NdotL = 1`, the albedo cancels and
///
/// ```text
/// AMBIENT_DIFFUSE_WEIGHT * luma(colour) * brightness = SKY_FILL * illuminance / PI
/// ```
///
/// which is the division below. It is stated against the *colour's luminance* because a colour
/// carries a hue and not a magnitude on both sides of that comparison: the record's ambient is
/// scaled to `base`'s luminance before it gets here, exactly as the weather's sun tint is scaled to
/// one before it reaches `sky_sun`. `base` is the calibrated colour of the space's kind
/// ([`SPACE_AMBIENT_BASES`]).
///
/// The sun of a daylit space always exists ([`sky_sun`]), so the caller only has to keep a sun that
/// is *off* - a row that publishes no daylight at all - away from this: a missing column is not a
/// black sky, and the space keeps the level it had.
fn sky_fill_brightness(sun_illuminance: f32, base: Color) -> f32 {
    SKY_FILL * sun_illuminance / (std::f32::consts::PI * AMBIENT_DIFFUSE_WEIGHT * luma(base))
}

/// Whether a weather gives a space daylight: a positive luminance, or a sunlight colour that is not
/// black.
///
/// A row that publishes neither is *not* the same as one whose daylight is a measured zero, and the
/// difference is which way the space is lit: `None` says the data does not say, and a space whose
/// row leaves the columns out keeps the sky's ambient the engine gave it before the table existed,
/// rather than being turned into a cave by a column that is missing. Only a weather that says its
/// sun is black - which is what `BlackreachWeather` says - makes the space a cave.
fn is_daylit(row: &SpaceLighting) -> bool {
    daylight_of(row).is_none_or(|daylight| daylight > 0.0)
}

/// The luminance of a weather's daylight, from whichever of the two columns carries it.
///
/// `sun_illuminance` is the converter's own reading of the sunlight group and is what the table
/// publishes; the sunlight colour is the fallback for a row that has the colour and not the
/// luminance, and is what the converter computes the luminance *from*.
fn daylight_of(row: &SpaceLighting) -> Option<f32> {
    row.sun_illuminance.or(row.directional.map(packed_luma))
}

/// The atmosphere of a space the database says nothing about: the engine before the
/// `space_lighting` table existed.
///
/// Three states and one hard-coded list of underground worldspaces, exactly as `update_atmosphere`
/// was written before this: a sky blue backdrop outdoors, the near-black one underground, no sun
/// where there is no sky to see, and no interior fog at all.
fn fallback_atmosphere(key: SpaceKey) -> SpaceAtmosphere {
    let interior = key.is_interior;
    let underground = interior || UNDERGROUND_WORLDSPACES.contains(&key.space_id);
    let (ambient_color, ambient_brightness) = AMBIENT_BASES.for_space(interior, !underground);
    SpaceAtmosphere {
        ambient_color,
        ambient_brightness,
        backdrop: if underground {
            UNDERGROUND_COLOR
        } else {
            SKY_COLOR
        },
        // An interior's fog is deliberately out of reach; an exterior's is the ring's, underground
        // or not - the ring is drawn outside Blackreach in the teal it is cleared to.
        fog: if interior {
            SpaceFog::Unreachable
        } else {
            SpaceFog::Ring
        },
        sun: if underground {
            SunLight::OFF
        } else {
            SunLight {
                color: Color::WHITE,
                illuminance: DAY_SUN_ILLUMINANCE,
            }
        },
        has_sky: !underground,
    }
}

/// The colour the space is cleared to where nothing is drawn in front of the camera.
///
/// A space drawn against a sky takes the weather's horizon haze - `sky_fog`, the `WTHR` fog-far
/// group the sun sets behind - and falls back to its upper sky, then to its fog. A space with no
/// sky has no horizon to fade to and takes its fog colour instead.
///
/// This is what fixes Blackreach's backdrop. Its teal is in the weather's fog, not in its ambient -
/// the ambient is `(10, 11, 12)` - so a backdrop taken from the ambient is black, and one taken
/// from `sky_fog` is `(14, 156, 156)`, which is what every reference of the place shows.
fn backdrop_of(row: &SpaceLighting, has_sky: bool, fallback: Color) -> Color {
    let chosen = if has_sky {
        row.sky_fog.or(row.sky_upper).or(row.fog)
    } else {
        row.fog.or(row.sky_fog)
    };
    chosen.map_or(fallback, srgb_u8)
}

/// The sun a worldspace's weather gives it.
///
/// A `WTHR` carries no illuminance of its own, so the magnitude is the day's sunlight luminance
/// scaled against [`DAY_ILLUMINANCE_REFERENCE`] - the luminance of the weather this engine's
/// [`DAY_SUN_ILLUMINANCE`] was measured on, `SkyrimCloudy`, which is Tamriel's. A full day is
/// therefore still 12,000 and the daylight of a weather whose sun is black is no sun at all, which
/// is how Blackreach and the Alftand cavern lose the sun the engine used to switch off by name.
///
/// The colour is the weather's sun-disc colour, scaled to a luminance of 1 before it is handed to
/// the light: the illuminance carries the day's magnitude and the colour only its tint, so a
/// weather whose sun is a dark red does not dim the daylight it colours.
fn sky_sun(row: &SpaceLighting) -> SunLight {
    let illuminance = daylight_of(row).map_or(0.0, |daylight| {
        DAY_SUN_ILLUMINANCE * (daylight / DAY_ILLUMINANCE_REFERENCE)
    });
    let illuminance = illuminance * DAY_SUN_LEVEL;
    if !illuminance.is_finite() || illuminance <= 0.0 {
        return SunLight::OFF;
    }
    SunLight {
        color: toward_white(
            row.sun
                .map_or(Color::WHITE, |rgb| scale_to_luma(srgb_u8(rgb), 1.0)),
            SUN_TINT_STRENGTH,
        ),
        illuminance,
    }
}

/// Whether a row's fog pair is a fog Bevy can draw: both finite, past the eye, and with an end
/// after its start. A record that fails this is one whose near/far the engine cannot use, and the
/// space is better off with the fog it had than with a NaN over every surface.
fn usable_fog(near: f32, far: f32) -> bool {
    near.is_finite() && far.is_finite() && near >= 0.0 && far > near
}

fn srgb_u8(rgb: [u8; 3]) -> Color {
    Color::srgb_u8(rgb[0], rgb[1], rgb[2])
}

/// The ambient light of a space, per camera.
///
/// [`GlobalAmbientLight`] is the same for every view; the portal camera needs its own, because the
/// room behind a doorway is lit by its own ambient and not by the one the player stands in
/// (`crate::portal`). `AmbientLight` on a camera overrides the global for that camera only, which
/// is the whole mechanism: the main camera keeps taking the global resource, and only the portal
/// camera carries a component of its own.
pub(crate) fn ambient_light(atmosphere: &SpaceAtmosphere) -> AmbientLight {
    AmbientLight {
        color: atmosphere.ambient_color,
        brightness: atmosphere.ambient_brightness,
        ..default()
    }
}

/// The distance fog of a space: its own range where the record gave one (an interior), the terrain
/// ring's outside, and one that cannot be reached where there is neither ([`SpaceFog`]).
///
/// Shared by the main camera and the portal camera, so a doorway is hazed exactly as the space
/// behind it is. The fog always fades into [`SpaceAtmosphere::backdrop`], which is also the
/// camera's clear colour: the ring and the sky behind it are then the same colour and the world
/// does not end in a band of something else.
pub(crate) fn atmosphere_fog(
    atmosphere: &SpaceAtmosphere,
    stream_radius: i32,
    terrain_radius: i32,
) -> DistanceFog {
    match atmosphere.fog {
        SpaceFog::Own { near, far } => DistanceFog {
            color: atmosphere.backdrop,
            falloff: FogFalloff::Linear {
                start: near,
                end: far,
            },
            directional_light_color: Color::NONE,
            directional_light_exponent: 8.0,
        },
        SpaceFog::Ring => exterior_fog(stream_radius, terrain_radius, atmosphere.backdrop),
        SpaceFog::Unreachable => fog_off(atmosphere.backdrop),
    }
}

/// Outdoors: sky blue behind the world and full daylight. Underground: a near-black backdrop, no
/// sun, and a dim green-teal ambient.
///
/// A warm camera lantern used to light these spaces too. It was a stopgap from before Skyrim's
/// `LIGH` references were real lights, and it made the engine unlike the game:
/// the real game carries no light for the player, so a room is lit by its own torches, braziers
/// and Dwemer lamps. Measured against the UESP references it was also the reason the demo's
/// Alftand01 arrival frame rendered as a white page: at the arrival the ice wall is within a few
/// hundred units of the eye, where a 2500-unit-range light at the camera is thousands of times
/// brighter than the ambient, and the whole frame clipped to the lantern's cream colour. The knobs
/// for these spaces are [`INTERIOR_AMBIENT_BRIGHTNESS`] / [`CAVERN_AMBIENT_BRIGHTNESS`] and
/// [`crate::lights::LIGHT_EXPOSURE`].
///
/// The three states above are what a space gets when the database has no
/// `space_lighting` row for it; with a row ([`space_atmosphere`]) the colours, the fog, the
/// backdrop and the sun all come from the game's own records, and the constants here are what
/// those records are scaled to and what a space without a row still falls back on.
#[allow(clippy::too_many_arguments)]
fn update_atmosphere(
    mut commands: Commands,
    config: Res<EngineConfig>,
    // The cell the camera is in. It is a resource the streaming plan owns, so a run whose world
    // does not stream cells has none, and the lighting has nothing to apply: the check that used to
    // be a `run_if(resource_exists::<ActiveCell>)` at the registration is this one.
    active: Option<Res<ActiveCell>>,
    catalog: Option<Res<SpaceLightingCatalog>>,
    mut clear: ResMut<ClearColor>,
    ambient: Option<ResMut<GlobalAmbientLight>>,
    mut suns: Query<&mut DirectionalLight>,
    mut cameras: Query<(Entity, &mut Camera), With<StreamingCamera>>,
    mut applied: Local<bool>,
) {
    let Some(active) = active else {
        return;
    };
    if cameras.is_empty() {
        return;
    }
    // Apply on the first frame that has a camera, then on every change of place. Starting inside
    // (--demo blackreach) never changes ActiveCell, so a change-only check left it in daylight.
    if *applied && !active.is_changed() {
        return;
    }
    *applied = true;
    let atmosphere = space_atmosphere(
        catalog.as_deref(),
        space_key(active.worldspace_id, active.interior),
    );
    // The backdrop is the clear colour of every view of this space - the water reflection camera
    // takes the resource, the main camera takes the component below, and the portal camera takes
    // the destination's own (`crate::portal`).
    clear.0 = atmosphere.backdrop;
    let fog = atmosphere_fog(&atmosphere, config.stream_radius, config.terrain_radius);
    for (entity, mut camera) in &mut cameras {
        camera.clear_color = ClearColorConfig::Custom(atmosphere.backdrop);
        commands.entity(entity).try_insert(fog.clone());
    }
    if let Some(mut ambient) = ambient {
        (ambient.color, ambient.brightness) =
            (atmosphere.ambient_color, atmosphere.ambient_brightness);
    }
    for mut sun in &mut suns {
        sun.color = atmosphere.sun.color;
        sun.illuminance = atmosphere.sun.illuminance;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{camera_far_plane, exterior_fog};
    use rusqlite::Connection;

    /// Which runs get the sky and the fog: the ones that are looked at, and a measured run that
    /// draws the ring by name. A benchmark run with the ring off - which is what the Phase 2 gates
    /// are taken with - keeps the flat lighting its numbers were measured under.
    #[test]
    fn the_atmosphere_follows_the_interactive_runs_and_the_ring() {
        let ring = EngineConfig {
            terrain_radius: 8,
            ..EngineConfig::default()
        };
        assert!(atmosphere_applies(true, &ring));
        assert!(
            atmosphere_applies(false, &ring),
            "a ring fades into the sky"
        );

        let no_ring = EngineConfig {
            terrain_radius: 2,
            ..EngineConfig::default()
        };
        assert!(atmosphere_applies(true, &no_ring));
        assert!(
            !atmosphere_applies(false, &no_ring),
            "a measured run with no ring keeps the lighting the acceptance numbers were taken with"
        );
    }

    // ---- per-space lighting, fog and sky ----
    //
    // The rows are the real values of the three Alftand interiors the demo walks through and of the
    // three worldspaces it crosses, from `tools/research/space_lighting_dump.py`; the fixture that
    // builds them is shared with `crate::world::lighting`, so these tests and the reader's are
    // written against one set of numbers.

    use crate::world::lighting::fixtures::{
        ALFTAND_WORLD, ALFTAND_ZCELL, ALFTAND01, ALFTAND02, BLACKREACH, SCHEMA, TAMRIEL, pack,
        real_spaces,
    };

    fn linear(color: Color) -> LinearRgba {
        LinearRgba::from(color)
    }

    /// Bevy's ambient on a surface of unit albedo: `AMBIENT_DIFFUSE_WEIGHT * luma(colour) *
    /// brightness`, written out from `bevy_pbr/src/render/pbr_ambient.wgsl` rather than from the
    /// constants the engine's own arithmetic is checked against - a helper that restated the
    /// engine's own assumption is the mistake `crate::lights`'s intensity test was written to catch
    /// once already. Nothing here is `NdotL`: that is the point of an ambient.
    fn ambient_on_surface(brightness: f32, colour: Color) -> f32 {
        AMBIENT_DIFFUSE_WEIGHT * brightness * luma(colour)
    }

    /// Bevy's sun on a surface of unit albedo facing it: `NdotL * illuminance / PI` at `NdotL = 1`,
    /// the Lambert `1/PI` of `Fd_Burley` in `bevy_pbr/src/render/pbr_lighting.wgsl`.
    fn sunlit_surface(illuminance: f32) -> f32 {
        illuminance / std::f32::consts::PI
    }

    /// A fog's two distances and its colour. Every fog this engine builds is linear, and a fog that
    /// is not is a bug rather than a case to handle.
    fn fog_parts(fog: &DistanceFog) -> (f32, f32, Color) {
        let DistanceFog {
            falloff: FogFalloff::Linear { start, end },
            color,
            ..
        } = fog
        else {
            panic!("the engine builds linear fog: {fog:?}");
        };
        (*start, *end, *color)
    }

    /// The atmosphere of an interior cell, from a catalog holding the fixture rows.
    fn interior_of(catalog: &SpaceLightingCatalog, cell_id: u32) -> SpaceAtmosphere {
        space_atmosphere(Some(catalog), space_key(TAMRIEL, Some(cell_id)))
    }

    /// The atmosphere of a worldspace, from a catalog holding the fixture rows.
    fn world_of(catalog: &SpaceLightingCatalog, worldspace_id: u32) -> SpaceAtmosphere {
        space_atmosphere(Some(catalog), space_key(worldspace_id, None))
    }

    /// A database with the `space_lighting` table and exactly the rows a test writes into it, for
    /// the shapes the real fixture does not have (a cavern with no sky, a half-lit day).
    fn lighting_database(path: &std::path::Path, rows: &str) -> SpaceLightingCatalog {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection.execute_batch(rows).unwrap();
        drop(connection);
        SpaceLightingCatalog::open(path)
    }

    /// A space the table says nothing about is drawn exactly as it was before the table existed,
    /// and that is what the shipped schema-15 database is: every space.
    #[test]
    fn a_space_without_a_row_is_the_engine_before_the_table() {
        // A catalog that is there but empty is the schema-15 database, and it has to answer the
        // same as no catalog at all - a run without the runtime data has none.
        let empty = SpaceLightingCatalog::default();
        for catalog in [None, Some(&empty)] {
            let alftand01 = space_atmosphere(catalog, space_key(TAMRIEL, Some(ALFTAND01)));
            assert_eq!(
                (
                    alftand01.ambient_color,
                    alftand01.ambient_brightness,
                    alftand01.backdrop,
                    alftand01.sun,
                    alftand01.has_sky,
                ),
                (
                    INTERIOR_AMBIENT_COLOR,
                    INTERIOR_AMBIENT_BRIGHTNESS,
                    UNDERGROUND_COLOR,
                    SunLight::OFF,
                    false,
                ),
                "an interior without a row keeps the calibrated interior ambient"
            );
            assert_eq!(
                alftand01.fog,
                SpaceFog::Unreachable,
                "and the fog that cannot reach its geometry"
            );

            let tamriel = space_atmosphere(catalog, space_key(TAMRIEL, None));
            assert_eq!(tamriel.backdrop, SKY_COLOR);
            assert_eq!(
                (tamriel.ambient_color, tamriel.ambient_brightness),
                (SKY_AMBIENT_COLOR, SKY_AMBIENT_BRIGHTNESS)
            );
            assert_eq!(
                tamriel.sun,
                SunLight {
                    color: Color::WHITE,
                    illuminance: DAY_SUN_ILLUMINANCE,
                }
            );
            assert!(tamriel.has_sky);

            // The hard-coded list of underground worldspaces is the fallback, and both of the
            // worldspaces it names are still underground without a row.
            let blackreach = space_atmosphere(catalog, space_key(BLACKREACH, None));
            assert_eq!(blackreach.backdrop, UNDERGROUND_COLOR);
            assert_eq!(
                (blackreach.ambient_color, blackreach.ambient_brightness),
                (CAVERN_AMBIENT_COLOR, CAVERN_AMBIENT_BRIGHTNESS)
            );
            assert_eq!(blackreach.sun, SunLight::OFF);
            assert!(!blackreach.has_sky);
            assert_eq!(
                fog_parts(&atmosphere_fog(&blackreach, 2, 8)),
                fog_parts(&exterior_fog(2, 8, UNDERGROUND_COLOR))
            );
        }
    }

    /// The table's whole point: each Alftand interior is lit and fogged with its own record at the
    /// calibrated magnitude, where one colour and no fog at all covered all of them.
    #[test]
    fn each_alftand_cell_takes_its_own_colour_at_the_calibrated_brightness() {
        let (_directory, catalog) = real_spaces();

        let alftand01 = interior_of(&catalog, ALFTAND01);
        let interior = SPACE_AMBIENT_BASES.interior;
        assert_eq!(alftand01.ambient_brightness, interior.1);
        assert!(
            (luma(alftand01.ambient_color) - luma(INTERIOR_AMBIENT_COLOR)).abs() < 1.0e-4,
            "the record moves the hue of the ambient, not the level the references were signed \
             off at"
        );
        assert!(
            linear(alftand01.ambient_color).blue / linear(alftand01.ambient_color).red
                > linear(INTERIOR_AMBIENT_COLOR).blue / linear(INTERIOR_AMBIENT_COLOR).red,
            "(40, 82, 87) is a colder colour than the hand-fitted one, and it has to survive the \
             scaling"
        );
        assert_eq!(
            alftand01.backdrop,
            Color::srgb_u8(153, 210, 238),
            "an interior is cleared to its own fog colour"
        );
        assert_eq!(
            alftand01.fog,
            SpaceFog::Own {
                near: 1100.0,
                far: 9000.0
            },
            "an interior's own haze, which the engine deliberately switched off before this"
        );
        assert_eq!(alftand01.sun, SunLight::OFF);
        assert!(!alftand01.has_sky);

        // The three cells are three different rooms. A table that read one of them for all three -
        // or that fell back to the engine's single interior colour - would give one answer here.
        let alftand02 = interior_of(&catalog, ALFTAND02);
        let zcell = interior_of(&catalog, ALFTAND_ZCELL);
        assert_ne!(alftand01.backdrop, alftand02.backdrop);
        assert_ne!(alftand02.backdrop, zcell.backdrop);
        assert_ne!(alftand01.fog, alftand02.fog);
        assert_ne!(alftand01.ambient_color, alftand02.ambient_color);
        assert_eq!(
            zcell.fog,
            SpaceFog::Own {
                near: 1100.0,
                far: 6000.0
            }
        );
        for atmosphere in [alftand01, alftand02, zcell] {
            assert_eq!(atmosphere.ambient_brightness, interior.1);
        }
    }

    /// Blackreach's backdrop. Its ambient is `(10, 11, 12)` - nearly black - and its teal is in the
    /// weather's horizon haze, so a backdrop taken from the ambient is the black the reference
    /// screenshots show this engine drawing.
    #[test]
    fn the_backdrop_of_blackreach_is_its_horizon_and_not_its_ambient() {
        let (_directory, catalog) = real_spaces();
        let blackreach = world_of(&catalog, BLACKREACH);

        assert_eq!(blackreach.backdrop, Color::srgb_u8(14, 156, 156));
        assert_ne!(blackreach.backdrop, UNDERGROUND_COLOR);
        assert!(
            luma(blackreach.backdrop) > 0.05,
            "the black backdrop fraction has to collapse, and a backdrop this dark cannot do it"
        );
        assert_ne!(
            blackreach.ambient_color,
            Color::srgb_u8(10, 11, 12),
            "the ambient is scaled to the calibrated magnitude, so it is not the raw record value"
        );
        assert_eq!(
            blackreach.sun,
            SunLight::OFF,
            "BlackreachWeather's daylight is (0, 0, 0): the sun is off because the data says so"
        );
        assert!(
            blackreach.has_sky,
            "a weather resolved, so it is drawn as a sky"
        );
        assert_eq!(
            blackreach.fog,
            SpaceFog::Ring,
            "a worldspace keeps the terrain ring's fog distances, not the weather's 2048..120000"
        );

        // The Alftand cavern is reached through the same climate and looks the same.
        assert_eq!(world_of(&catalog, ALFTAND_WORLD), blackreach);
    }

    /// The ring's "no cliff" guarantee survives the weather: an exterior takes the colour the fog
    /// fades into from the record and nothing else about it, so the ring is still clear where the
    /// full-detail grid ends and opaque at the far edge of the ring.
    #[test]
    fn an_exterior_keeps_the_rings_distances_and_takes_the_weathers_colour() {
        let (_directory, catalog) = real_spaces();
        let config = EngineConfig::default();
        let tamriel = world_of(&catalog, TAMRIEL);

        let (start, end, color) = fog_parts(&atmosphere_fog(
            &tamriel,
            config.stream_radius,
            config.terrain_radius,
        ));
        let (was_start, was_end, _) = fog_parts(&exterior_fog(
            config.stream_radius,
            config.terrain_radius,
            SKY_COLOR,
        ));
        assert_eq!(
            (start, end),
            (was_start, was_end),
            "the ring keeps its own distances, and the weather's own 0..100000 is not a range \
             this engine has anything to draw at"
        );
        assert_eq!(
            color,
            Color::srgb_u8(139, 175, 194),
            "the weather's horizon haze: `sky_fog`, the group the sun sets behind"
        );
        assert_ne!(
            color, SKY_COLOR,
            "the engine's own sky colour is the fallback"
        );
        assert!(end < camera_far_plane(&config));

        // With no ring there is nothing beyond the grid to fade, and the fog still cannot be
        // reached - whatever colour it is.
        let (start, _, color) = fog_parts(&atmosphere_fog(&tamriel, 2, 2));
        assert!(start > camera_far_plane(&config));
        assert_eq!(color, Color::srgb_u8(139, 175, 194));
    }

    /// A weather's own daylight scales the calibrated day: the table carries a luminance, not an
    /// illuminance.
    #[test]
    fn a_weathers_daylight_scales_the_calibrated_day() {
        let (_directory, catalog) = real_spaces();
        let tamriel = world_of(&catalog, TAMRIEL);
        assert!(
            (tamriel.sun.illuminance - DAY_SUN_ILLUMINANCE * DAY_SUN_LEVEL).abs() < 0.01,
            "{} is not the day this engine is calibrated at ({})",
            tamriel.sun.illuminance,
            DAY_SUN_ILLUMINANCE * DAY_SUN_LEVEL
        );
        assert!(
            (luma(tamriel.sun.color) - 1.0).abs() < 1.0e-4,
            "the sun's tint carries the colour and the illuminance carries the day, so a record \
             whose sun is a dark red cannot dim the daylight it colours"
        );
        assert_ne!(
            tamriel.sun.color,
            Color::WHITE,
            "and the tint is the record's, not the engine's"
        );
        assert_eq!(
            tamriel.sun.color,
            toward_white(
                scale_to_luma(Color::srgb_u8(129, 105, 107), 1.0),
                SUN_TINT_STRENGTH
            )
        );
        // The tint is the record's moved towards white, not the record's thrown away: the blue
        // channel of `(129, 105, 107)` still ends up over its green, which a white sun would not
        // have.
        let tint = linear(tamriel.sun.color);
        let record = linear(scale_to_luma(Color::srgb_u8(129, 105, 107), 1.0));
        assert!(tint.red < record.red, "the red cast has to come down");
        assert!(tint.red / tint.green < record.red / record.green);
        assert!(
            (luma(tamriel.sun.color) - 1.0).abs() < 1.0e-4,
            "the blend moves the tint and not the level: the illuminance is the day's"
        );

        // Half the daylight of the reference weather is half the illuminance - the rule that would
        // be one hard-coded illuminance for every space if this were wrong.
        let directory = tempfile::tempdir().unwrap();
        let catalog = lighting_database(
            &directory.path().join("half.db"),
            &format!(
                "INSERT INTO space_lighting (space_id, is_interior, has_sky, sun, sun_illuminance)
                 VALUES (4321, 0, 1, {}, {});",
                pack([255, 255, 255]),
                DAY_ILLUMINANCE_REFERENCE / 2.0,
            ),
        );
        let half = space_atmosphere(Some(&catalog), space_key(4321, None));
        assert!(
            (half.sun.illuminance - DAY_SUN_ILLUMINANCE * DAY_SUN_LEVEL / 2.0).abs() < 1.0e-3,
            "{} is not half of the calibrated day",
            half.sun.illuminance
        );
    }

    /// The defect this change exists for, outside: the shadowed half of a midday frame has to be
    /// lit by the sky over it rather than left with nothing. The ratio is asserted and not only the
    /// value, because the ratio is the whole design - `SKY_FILL` is a share of a daylit space's
    /// *own* sun, so a weather with half the daylight gets half the fill and the same shadows, where
    /// a fixed level of ambient could not do that.
    #[test]
    fn the_sky_fills_a_shadowed_surface_to_a_third_of_a_sunlit_one() {
        let (_directory, catalog) = real_spaces();
        let day = world_of(&catalog, TAMRIEL);

        let shadowed = ambient_on_surface(day.ambient_brightness, day.ambient_color);
        let sunlit = shadowed + sunlit_surface(day.sun.illuminance);
        let ratio = shadowed / sunlit;
        assert!(
            (ratio - SKY_FILL / (1.0 + SKY_FILL)).abs() < 1.0e-4,
            "a shadowed surface gets {ratio} of a sunlit one; SKY_FILL/(1+SKY_FILL) is {}",
            SKY_FILL / (1.0 + SKY_FILL)
        );
        assert!(
            (ratio - 1.0 / 3.0).abs() < 1.0e-4,
            "and the fit is the one the reference day was measured at: a third, got {ratio}"
        );

        // What the fill is made of: this space's own sun, at the one value all three constants have
        // to agree on - `SKY_FILL * illuminance / (PI * AMBIENT_DIFFUSE_WEIGHT * luma(base))`,
        // 0.5 * 12,800 / (PI * 0.4524 * 0.2623). A change to any of the three moves it.
        let fill = sky_fill_brightness(day.sun.illuminance, SKY_AMBIENT_COLOR);
        assert!(
            (day.ambient_brightness - fill).abs() < fill * 1.0e-6,
            "the daylit brightness is the fill for that row's sun: {} against {fill}",
            day.ambient_brightness
        );
        assert!(
            (fill - 17_167.0).abs() < 1.0,
            "the reference day's fill is {fill}"
        );
        assert!(
            fill > SPACE_AMBIENT_BASES.sky.1 * 20.0,
            "which is 27 times the fixed level it replaced - that level leaves the shadow about a \
             hundredth of the sunlit surface rather than a third, and that hundredth is the \
             near-black frame the audit measures: {fill} against {}",
            SPACE_AMBIENT_BASES.sky.1
        );

        // The record still brings the hue and the fill only the level: the ambient is the row's
        // `(203, 220, 220)` brought to the calibrated colour's luminance, not that colour itself.
        assert!((luma(day.ambient_color) - luma(SKY_AMBIENT_COLOR)).abs() < 1.0e-4);
        assert_ne!(
            day.ambient_color, SKY_AMBIENT_COLOR,
            "the record's hue survives the scaling"
        );
    }

    /// The other half of that design: a surface *facing* the sun has to keep the day the nine
    /// Tamriel poses were fitted at, or this is an exposure change to every exterior in the game.
    /// The old day is written out from the constants as they stood - `DAY_SUN_LEVEL` 1.6 and the sky
    /// base's 640 - and the new one taken from the resolver the engine runs.
    #[test]
    fn a_sunlit_surface_keeps_the_day_the_old_pair_gave_it() {
        let (_directory, catalog) = real_spaces();
        let day = world_of(&catalog, TAMRIEL);

        // The old pair: the fitted 1.6 of `DAY_SUN_ILLUMINANCE` and the fixed fill of `SKY_AMBIENT_LEVEL`.
        const OLD_DAY_SUN_LEVEL: f32 = 1.6;
        let old_sunlit = sunlit_surface(DAY_SUN_ILLUMINANCE * OLD_DAY_SUN_LEVEL);
        let old_fill = ambient_on_surface(
            SKY_AMBIENT_BRIGHTNESS * SKY_AMBIENT_LEVEL,
            SKY_AMBIENT_COLOR,
        );
        let old_total = old_sunlit + old_fill;

        let new_fill = ambient_on_surface(day.ambient_brightness, day.ambient_color);
        let new_total = sunlit_surface(day.sun.illuminance) + new_fill;

        // `DAY_SUN_LEVEL = 1.6 / (1 + SKY_FILL)` is what makes this hold: the sun and the fill it
        // derives add up to the day the fit was taken at.
        assert!(
            (new_total - old_sunlit).abs() < old_sunlit * 1.0e-3,
            "the sunlit surface was {old_sunlit} and is {new_total}"
        );
        // The difference against the old *total* is the old fill itself - the 640-level ambient
        // beside a sun of 19,200 - which is 1.2% of the surface. So the two days agree to well
        // inside 2%, and what the new day does with that 1.2% is light the surfaces the sun misses
        // with it instead of putting it all on the ones the sun already reaches.
        assert!(
            (old_total - new_total - old_fill).abs() < old_total * 1.0e-3,
            "the surface lost {} against the old day; the old fill was {old_fill}",
            old_total - new_total
        );
        assert!(
            (old_total - new_total) / old_total < 0.02,
            "the two days differ by {}, which is the old fill",
            (old_total - new_total) / old_total
        );
    }

    /// A row that publishes no daylight at all is not a space with a black sun: it keeps the level
    /// the engine lit it with before the fill existed. The same row *with* a day takes the fill, so
    /// what the brightness follows is the sun and not the row's presence.
    #[test]
    fn a_daylit_space_whose_sun_is_missing_keeps_the_sky_base_level() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = lighting_database(
            &directory.path().join("silent-sun.db"),
            &format!(
                "INSERT INTO space_lighting (space_id, is_interior, ambient, has_sky) \
                 VALUES (4326, 0, {}, 1);",
                pack([203, 220, 220]),
            ),
        );
        let no_sun = space_atmosphere(Some(&catalog), space_key(4326, None));
        assert!(no_sun.has_sky, "a weather resolved for it");
        assert_eq!(no_sun.sun, SunLight::OFF, "nothing says there is a sun");
        assert_eq!(
            no_sun.ambient_brightness, SPACE_AMBIENT_BASES.sky.1,
            "and a missing column is not a black sky: the space keeps the level it had, rather \
             than dividing a fill by a sun that is not there"
        );

        let catalog = lighting_database(
            &directory.path().join("real-sun.db"),
            &format!(
                "INSERT INTO space_lighting \
                   (space_id, is_interior, ambient, sun_illuminance, has_sky) \
                 VALUES (4327, 0, {}, {}, 1);",
                pack([203, 220, 220]),
                DAY_ILLUMINANCE_REFERENCE,
            ),
        );
        let lit = space_atmosphere(Some(&catalog), space_key(4327, None));
        assert!(lit.sun.illuminance > 0.0);
        let fill = sky_fill_brightness(lit.sun.illuminance, SKY_AMBIENT_COLOR);
        assert!(
            (lit.ambient_brightness - fill).abs() < fill * 1.0e-6,
            "the same row with a day takes the fill for it: {} against {fill}",
            lit.ambient_brightness
        );
        assert!(
            lit.ambient_brightness > no_sun.ambient_brightness * 20.0,
            "and it is not the level the sunless row keeps: {} against {}",
            lit.ambient_brightness,
            no_sun.ambient_brightness
        );
    }

    /// The holdout spaces: an interior and a cavern are lit at their own calibrated levels, to the
    /// bit. Their frames are what the cut in `crate::lights` was *not* fitted on and what the fill
    /// must not reach - and they are the two kinds of space whose ambient is a level rather than a
    /// share of a sun, because neither has one.
    #[test]
    fn an_interior_and_a_cavern_keep_the_brightness_they_had() {
        let (_directory, catalog) = real_spaces();
        for cell in [ALFTAND01, ALFTAND02, ALFTAND_ZCELL] {
            let interior = interior_of(&catalog, cell);
            assert_eq!(interior.sun, SunLight::OFF, "an interior has no sun");
            assert_eq!(
                interior.ambient_brightness,
                INTERIOR_AMBIENT_BRIGHTNESS * INTERIOR_AMBIENT_LEVEL,
                "and its ambient is the level it was fitted at, not a fill"
            );
        }
        for worldspace in [BLACKREACH, ALFTAND_WORLD] {
            let cavern = world_of(&catalog, worldspace);
            assert_eq!(
                cavern.ambient_brightness,
                CAVERN_AMBIENT_BRIGHTNESS * CAVERN_AMBIENT_LEVEL,
                "a cavern's weather has no daylight to fill it with"
            );
        }
        // To the bit, against the bases the resolver states: the numbers the Alftand and Blackreach
        // reference frames were signed off at.
        assert_eq!(
            SPACE_AMBIENT_BASES.interior.1,
            INTERIOR_AMBIENT_BRIGHTNESS * INTERIOR_AMBIENT_LEVEL
        );
        assert_eq!(
            SPACE_AMBIENT_BASES.cavern.1,
            CAVERN_AMBIENT_BRIGHTNESS * CAVERN_AMBIENT_LEVEL
        );
        // And the check above is not vacuous: the fill is an order of magnitude more than the level
        // these spaces keep, so a fill that leaked into one of them would be visible here.
        let would_be =
            sky_fill_brightness(DAY_SUN_ILLUMINANCE * DAY_SUN_LEVEL, INTERIOR_AMBIENT_COLOR);
        assert!(
            would_be > SPACE_AMBIENT_BASES.interior.1 * 10.0,
            "a sunlit interior's fill would be {would_be} against the {} it keeps",
            SPACE_AMBIENT_BASES.interior.1
        );
    }

    /// `has_sky` is what decides whether a worldspace is drawn as daylight or as a cavern, which is
    /// the job the engine's hard-coded list of two worldspaces did.
    #[test]
    fn a_worldspace_without_a_sky_keeps_the_cavern_ambient() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = lighting_database(
            &directory.path().join("cave.db"),
            &format!(
                "INSERT INTO space_lighting
                   (space_id, is_interior, ambient, fog, fog_near, fog_far, has_sky)
                 VALUES (4322, 0, {}, {}, 100.0, 200.0, 0);",
                pack([10, 11, 12]),
                pack([0, 169, 183]),
            ),
        );
        let cave = space_atmosphere(Some(&catalog), space_key(4322, None));
        assert!(!cave.has_sky);
        assert_eq!(cave.ambient_brightness, CAVERN_AMBIENT_BRIGHTNESS);
        assert!((luma(cave.ambient_color) - luma(CAVERN_AMBIENT_COLOR)).abs() < 1.0e-4);
        assert_eq!(cave.sun, SunLight::OFF);
        assert_eq!(
            cave.backdrop,
            Color::srgb_u8(0, 169, 183),
            "a space with no sky has no horizon to fade to and is cleared to its fog"
        );
        assert_eq!(
            cave.fog,
            SpaceFog::Ring,
            "and it is still an exterior: the fog it keeps is the ring's"
        );
    }

    /// A sky is not a sun. `BlackreachWeather` resolves, is drawn as a sky and has a daylight of
    /// zero, and the two worldspaces under it - Blackreach and the Alftand cavern the demo walks
    /// through - were drawn 3.5x too dark while they took the *daylight* ambient of a space the sun
    /// was supposed to light. The record says which one it is, so the engine does not have to.
    #[test]
    fn a_sky_whose_weather_has_no_daylight_is_lit_as_a_cavern() {
        let (_directory, catalog) = real_spaces();
        for worldspace in [BLACKREACH, ALFTAND_WORLD] {
            let atmosphere = world_of(&catalog, worldspace);
            assert!(
                atmosphere.has_sky,
                "the weather resolved, so the space is still drawn against its sky"
            );
            assert_eq!(atmosphere.sun, SunLight::OFF, "with no daylight over it");
            assert_eq!(
                atmosphere.ambient_brightness, CAVERN_AMBIENT_BRIGHTNESS,
                "and lit by its own ambient at the cavern magnitude, not a day's fill light"
            );
            assert!((luma(atmosphere.ambient_color) - luma(CAVERN_AMBIENT_COLOR)).abs() < 1.0e-4);
            assert_eq!(
                atmosphere.backdrop,
                Color::srgb_u8(14, 156, 156),
                "the backdrop is the weather's horizon, which is a sky the space has"
            );
        }

        // The two are one weather and one answer, which is what the data says: an engine that lit
        // them differently would be reading a difference into the records that is not there.
        assert_eq!(
            world_of(&catalog, ALFTAND_WORLD),
            world_of(&catalog, BLACKREACH)
        );

        // A row that publishes no daylight at all is not a measured zero: the space keeps the sky
        // the engine lit it with before the table existed, rather than becoming a cave because a
        // column is missing.
        let directory = tempfile::tempdir().unwrap();
        let silent = lighting_database(
            &directory.path().join("silent.db"),
            "INSERT INTO space_lighting (space_id, is_interior, has_sky) VALUES (4325, 0, 1);",
        );
        let atmosphere = space_atmosphere(Some(&silent), space_key(4325, None));
        assert!(atmosphere.has_sky);
        assert_eq!(atmosphere.ambient_brightness, SPACE_AMBIENT_BASES.sky.1);
        assert_eq!(atmosphere.sun, SunLight::OFF, "nothing says there is a sun");
    }

    /// An interior's fog now reaches its geometry, and a space without a row keeps the fog that
    /// cannot - the regression the whole change is judged on.
    #[test]
    fn an_interiors_row_fog_reaches_its_geometry_where_before_it_could_not() {
        let (_directory, catalog) = real_spaces();
        let far = camera_far_plane(&EngineConfig::default());

        let (start, end, color) =
            fog_parts(&atmosphere_fog(&interior_of(&catalog, ALFTAND01), 2, 2));
        assert_eq!((start, end), (1100.0, 9000.0));
        assert!(
            start < far && end < far,
            "the haze has to be inside the far plane {far} to be seen at all"
        );
        assert_eq!(color, interior_of(&catalog, ALFTAND01).backdrop);

        // No row: the old guarantee, unchanged.
        let (start, _, _) = fog_parts(&atmosphere_fog(
            &space_atmosphere(None, space_key(TAMRIEL, Some(ALFTAND01))),
            2,
            2,
        ));
        assert!(start > far);

        // A row whose pair is not a range Bevy can draw is a space the engine leaves as it was
        // rather than a NaN over every surface in it.
        let directory = tempfile::tempdir().unwrap();
        let catalog = lighting_database(
            &directory.path().join("bad-fog.db"),
            "INSERT INTO space_lighting
               (space_id, is_interior, fog, fog_near, fog_far, has_sky)
             VALUES (4323, 1, 16711680, 9000.0, 1100.0, 0);",
        );
        let backwards = space_atmosphere(Some(&catalog), space_key(TAMRIEL, Some(4323)));
        assert_eq!(backwards.fog, SpaceFog::Unreachable);
        let (start, _, _) = fog_parts(&atmosphere_fog(&backwards, 2, 2));
        assert!(start > far);

        // A row whose every colour column is NULL - a plugin's cell the converter had nothing to
        // resolve for - is not a black room: each field falls back to what the engine drew this
        // kind of space with.
        let catalog = lighting_database(
            &directory.path().join("bare.db"),
            "INSERT INTO space_lighting (space_id, is_interior, has_sky) VALUES (4324, 1, 0);",
        );
        let bare = space_atmosphere(Some(&catalog), space_key(TAMRIEL, Some(4324)));
        assert_eq!(bare.ambient_color, INTERIOR_AMBIENT_COLOR);
        assert_eq!(bare.ambient_brightness, SPACE_AMBIENT_BASES.interior.1);
        assert_eq!(bare.backdrop, UNDERGROUND_COLOR);
        assert_eq!(bare.sun, SunLight::OFF);
    }

    /// The wiring, not the arithmetic: the system writes the space's atmosphere onto the camera,
    /// the global ambient and the sun, and it is the *camera's* clear colour the doorway and the
    /// main view each get their own of.
    #[test]
    fn the_atmosphere_system_writes_the_space_onto_the_camera() {
        let (_directory, catalog) = real_spaces();
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .insert_resource(ClearColor(SKY_COLOR))
            .insert_resource(GlobalAmbientLight::default())
            .insert_resource(catalog)
            .init_resource::<ActiveCell>();
        app.world_mut().resource_mut::<ActiveCell>().interior = Some(ALFTAND01);
        let camera = app
            .world_mut()
            .spawn((Camera3d::default(), Transform::default(), StreamingCamera))
            .id();
        let sun = app.world_mut().spawn(DirectionalLight::default()).id();
        app.add_systems(Update, update_atmosphere);
        app.update();

        let camera = app.world().entity(camera);
        match camera
            .get::<Camera>()
            .expect("the camera is there")
            .clear_color
        {
            ClearColorConfig::Custom(color) => {
                assert_eq!(color, Color::srgb_u8(153, 210, 238));
            }
            other => panic!("the camera clears to its own space, not to {other:?}"),
        }
        let fog = camera.get::<DistanceFog>().expect("the fog was applied");
        assert_eq!(
            fog_parts(fog),
            (1100.0, 9000.0, Color::srgb_u8(153, 210, 238))
        );
        assert_eq!(
            app.world().resource::<ClearColor>().0,
            Color::srgb_u8(153, 210, 238),
            "the resource is what a view that is not the main camera renders against"
        );
        let ambient = app.world().resource::<GlobalAmbientLight>();
        assert_eq!(ambient.brightness, SPACE_AMBIENT_BASES.interior.1);
        assert_ne!(ambient.color, INTERIOR_AMBIENT_COLOR);
        assert_eq!(
            app.world()
                .entity(sun)
                .get::<DirectionalLight>()
                .unwrap()
                .illuminance,
            0.0,
            "an interior has no sun"
        );
    }
}
