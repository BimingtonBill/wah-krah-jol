//! Light that spills through an open doorway, both ways, so the frame and the threshold do not
//! change lighting at a hard line.
//!
//! # Why
//!
//! A doorway the portal draws through is two images meeting at a plane: the main view, lit by the
//! space the player stands in, and the portal image, lit by the destination's own ambient, fog, sun
//! and lights (`crate::portal`'s `update_destination_atmosphere`). Each side's lighting is set on
//! its own, so the geometry that runs across the doorway - the frame, the threshold, the floor
//! boards - changes colour and brightness exactly at the doorway's plane (the user's captures
//! `local/captures/2026-09-25_10-27-59/01.png` and `03.png`, Sven's House; research-205). Real light
//! crosses an open doorway: daylight falls across the floor just inside it, and firelight falls on
//! the porch just outside.
//!
//! # What this does
//!
//! Two [`SpotLight`]s per open doorway ([`SpillLight`]), present only while the portal draws
//! through a door ([`crate::portal::PortalState::open_doorway`]) and faded in and out with the
//! door's swing ([`step_openness`]):
//!
//! * **into the destination**, on the portal camera's layer
//!   ([`DESTINATION_LAYER`](crate::portal::DESTINATION_LAYER)), standing a little outside the
//!   destination doorway and aimed in through it, in the *source* side's light;
//! * **into the source**, on the main camera's layers (0 and 1), standing a little inside the
//!   source doorway and aimed out through it, in the *destination* side's light.
//!
//! A side's light ([`side_light`]) is measured where the seam is seen: on the threshold floor,
//! [`THRESHOLD_PATCH_DEPTH`] into that side from the bottom edge of the doorway quad (which stands
//! on the doorway's anchored threshold, `crate::portal`'s `anchored_threshold`). It is the light
//! that floor is drawn with: the side's ambient, every one of its own `LIGH` lights by the inverse
//! square law and the floor's cosine to it (they are drawn unshadowed, [`lights_on_floor`]), and
//! its sun by the floor's cosine and the share of the threshold a ray toward the sun reaches
//! ([`measure_threshold_sun`]). Before impl-232 it was the brightest light alone, at the doorway's
//! centre, at a tenth of its weight. Each spill is sized to put its light on that same floor point
//! ([`spot_delivery`]). The spill narrows the gaps between the two sides rather than adding light
//! to both ([`spill_illuminance`]): only the dimmer side is brightened, by a share of the gap, and
//! each side is tinted toward the other's colour by the same small amount, in proportion to how far
//! apart the two hues are. Two sides with the same light get nothing; a daylit porch beside a dim
//! house lights the floor inside, a house lit brighter than the night lights the porch. The rule is
//! the same both ways, so when the player crosses and the roles swap with the worlds, each side
//! keeps the spill it had - only the layer the light is on changes ([`spill_pair`]).
//!
//! The lights cast no shadows and are not [`crate::lights::SkyrimLight`]s, so the light budget does
//! not touch them. Bevy's clustered lights respect `RenderLayers` per view
//! (`bevy_light-0.19.0/src/cluster/assign.rs:459`), so each light reaches only its own view.
//!
//! # Tuning
//!
//! The placement constants (standoff, tilt) were fitted by impl-210. The amount of light
//! ([`SPILL_SHARE`], [`SPILL_TINT`]) by impl-232 on the threshold-floor measure, on square views of
//! Sven's House (`0001CBB0`) and the Riverwood Trader (`0001341F`); Gerdur's House (`00013423`) and
//! Honningbrew Meadery (`0007D7A8`) held out (shots and scripts in `local/t232/`). The score is the
//! luminance step across the threshold line in the render: the mean linear luma of a 10 px band on
//! each side of a hand-marked line, larger over smaller. No spill, then shares 0.25 / 0.5 / 1:
//!
//! | door | none | 0.25 | 0.5 | 1 |
//! |---|---|---|---|---|
//! | Sven's House | 1.53x | 1.64x | - | 1.93x |
//! | the Trader | 1.11x | 1.12x | 1.33x | 1.73x |
//! | Gerdur's House (held out) | 1.50x | 1.40x | 1.31x | 1.17x |
//! | Honningbrew (held out) | 1.55x | 1.53x | 1.51x | 1.47x |
//!
//! The measure agrees with the render at Gerdur's, at Honningbrew and at the Trader's floor inside
//! (luma over measured light about 2.8e-5 to 4.3e-5 on each), but it cannot see a surface's
//! albedo: the Trader's sill draws at half the luma per unit of light of the others, so the two
//! sides look equal while the measure calls the inside half as bright. And Sven's House draws its
//! floor inside in one of two states from run to run (luma 0.089 or 0.033 at the same measured
//! light, the doorway anchored differently: research-229's anchor flake); the measure matches the
//! bright one. No share narrows every door, so the spill stays opt-in (`--portal-light-spill`).

use crate::{
    doors::{DoorState, LoadDoor},
    lights::SkyrimLight,
    portal::{DESTINATION_LAYER, OpenDoorway, PortalFrame, PortalQuad, PortalState},
    streaming::ActiveCell,
    world::lighting::{SpaceAtmosphere, SpaceKey, SpaceLightingCatalog, luma, space_key},
};
use bevy::{
    camera::visibility::RenderLayers,
    picking::mesh_picking::ray_cast::{MeshRayCast, MeshRayCastSettings, RayCastVisibility},
    prelude::*,
};
use std::f32::consts::PI;

/// How far off the doorway's plane each spill light stands, on the side the light comes from, in
/// Creation units. Far enough that the cone through the opening is not a hemisphere, near enough
/// that the frame's reveals are inside it. Fitted (see "Tuning" above): 32 scored worse than 64.
const SPILL_STANDOFF: f32 = 64.0;

/// How far below the doorway's own normal each spill is aimed, in degrees: light through a doorway
/// falls on the floor inside it rather than on the far wall. The cone is widened by the same angle
/// ([`spill_through`]), so the frame's head stays inside it. Fitted (see "Tuning" above): 10, 15,
/// 20 and 30 were tried, and 15 scored best.
const SPILL_DOWN_TILT_DEGREES: f32 = 15.0;

/// The widest a spill's cone may be, in radians (Bevy's maximum is a half turn's half).
const SPILL_MAX_CONE: f32 = 1.3;

/// The inner cone as a fraction of the outer: the edge of the patch of light fades rather than
/// stopping at a line of its own.
const SPILL_INNER_CONE_FRACTION: f32 = 0.5;

/// How far a spill reaches past the doorway, in Creation units: a room's first few paces, about
/// 7 m, the middle of the brief's 400-600. Not fitted.
const SPILL_RANGE: f32 = 500.0;

/// The distance from the light at which [`spot_intensity`] sizes a spill, in Creation units. Only a
/// fallback now: a spill is sized on the threshold floor it lights ([`spot_delivery`]), and this is
/// what a floor the cone does not reach falls back on.
const SPILL_REFERENCE_DISTANCE: f32 = 128.0;

/// How far into each side of the threshold its floor light is measured, in Creation units: the
/// floor just inside the doorway and the sill or porch just outside it, where the seam is seen.
const THRESHOLD_PATCH_DEPTH: f32 = 24.0;

/// How far above the threshold floor the measuring point stands, in Creation units, so a ray from it
/// does not start inside the floor.
const THRESHOLD_LIFT: f32 = 2.0;

/// How many points across the threshold's width the sun's shade is sampled at.
const SUN_SAMPLES: usize = 3;

/// How often the sun's shade at a doorway's two thresholds is measured again, in seconds, while the
/// same doorway stays open: cells stream in around it and the sun can move.
const SUN_REMEASURE_SECONDS: f64 = 0.5;

/// How far toward the sun a shade ray looks for a roof or a wall, in Creation units.
const SUN_RAY_REACH: f32 = 4096.0;

/// How much of the gap between the two sides' threshold-floor light ([`side_light`]) a spill into
/// the dimmer side puts on its threshold floor. Fitted (impl-232, see "Tuning" above): 0.25, 0.5
/// and 1 were tried; every share above 0 widens the Trader's step, 0.25 least (1.11x -> 1.12x).
const SPILL_SHARE: f32 = 0.25;

/// The most a spill may add to the dimmer side, as a multiple of that side's own light, so a
/// doorway whose bright side is misjudged (a lamp behind a wall) cannot flood the other. Fitted
/// (impl-221): 0.5, 1, 2, 4 and 8 were tried; 0.5 and 1 hold back the daylight the dim house needs,
/// and above 4 nothing changed on the fitting doorways.
const SPILL_MAX_GAIN: f32 = 4.0;

/// How strongly each side is tinted toward the other's colour: the tint light is this times the
/// dimmer side's light times the distance between the two lights' chromaticities
/// ([`colour_gap`]), so it is zero when the two sides have the same hue, and it adds the same light
/// to both sides. Hearth against daylight is a gap of about 0.17. impl-221 fitted 10 against its
/// doorway-centre measure, where the dimmer side's light was small; on the threshold-floor measure
/// the dimmer side is a whole ambient, 10 adds more light than the brightening does, and 1 already
/// widens the step at Sven's House and the Trader (impl-232), so the tint is off. It narrowed the
/// colour seam (impl-221), which impl-232's step score does not see: refit it on colour.
const SPILL_TINT: f32 = 0.0;

/// How much the side's own `LIGH` lights count toward the light on its threshold floor. They are
/// drawn unshadowed (`crate::lights`), so what the render puts on the floor is every light's
/// inverse-square share times the floor's cosine to it ([`lights_on_floor`]), walls or not.
const LIGHT_WEIGHT: f32 = 1.0;

/// How much of a side's sun counts toward the light on its threshold floor, times the floor's
/// cosine to the sun and the share of the threshold the sun reaches ([`ThresholdSun`]).
const SUN_WEIGHT: f32 = 1.0;

/// The nearest a `LIGH` light is taken to be to the threshold floor when its light there is
/// measured, in Creation units: a guard against a light standing on the floor point.
const LIGHT_MIN_DISTANCE: f32 = 32.0;

/// How long a spill takes to fade fully in or out, in seconds: about a door's swing to
/// [`crate::doors::OPEN_FRACTION`].
const SPILL_FADE_SECONDS: f32 = 0.6;

/// How much of Bevy's ambient brightness lands on a surface of unit albedo, as an illuminance that
/// a light would have to deliver for the same result: `PI * 0.4524`. `0.4524` is the diffuse half
/// of Bevy's `EnvBRDFApprox` at the roughness its ambient call site passes (derived in
/// `crate::atmosphere`, `AMBIENT_DIFFUSE_WEIGHT`), and a light's diffuse is `albedo / PI` of its
/// illuminance.
const AMBIENT_AS_ILLUMINANCE: f32 = PI * 0.4524;

/// Registers the two spill lights and the system that places them.
///
/// `app::run` adds it for the interactive runs, beside the lights; it does nothing without
/// [`PortalState`] (a run without the portal).
pub struct PortalSpillPlugin;

impl Plugin for PortalSpillPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpillMemory>()
            .init_resource::<ThresholdSun>()
            .add_systems(Startup, spawn_spill_lights)
            .add_systems(
                Update,
                (measure_threshold_sun, update_spill)
                    .chain()
                    .after(PortalFrame),
            );
    }
}

/// Which way a spill light carries light through the doorway.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpillLight {
    /// The source side's light, into the destination: drawn in the doorway image.
    IntoDestination,
    /// The destination side's light, into the source: drawn in the room the player stands in.
    IntoSource,
}

impl SpillLight {
    fn layers(self) -> RenderLayers {
        match self {
            Self::IntoDestination => RenderLayers::layer(DESTINATION_LAYER),
            Self::IntoSource => RenderLayers::from_layers(&[0, 1]),
        }
    }
}

/// The doorway the spill was last lit for, and how far it has faded in.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq)]
struct SpillMemory {
    /// The source door's reference id and the reference its link leads to.
    door: Option<DoorPair>,
    /// 0 (no spill) to 1 (the door fully open).
    openness: f32,
}

/// A door and the door its link leads to, as reference ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DoorPair {
    door: u32,
    leads_to: u32,
}

impl DoorPair {
    fn of(door: &LoadDoor) -> Self {
        Self {
            door: door.ref_id,
            leads_to: door.destination.destination_ref_id,
        }
    }

    /// Whether `next` is the same doorway: the same door, or the door at the other end of it -
    /// which is what the portal draws through once the player has crossed.
    fn same_doorway(self, next: Self) -> bool {
        self == next || (next.door == self.leads_to && next.leads_to == self.door)
    }
}

/// The openness a spill has after `seconds` more of the door being in `state`: rising toward 1 while
/// the door opens or is open, falling toward 0 while it closes or is closed.
fn step_openness(openness: f32, state: Option<&DoorState>, seconds: f32) -> f32 {
    let step = seconds.max(0.0) / SPILL_FADE_SECONDS;
    let rising = matches!(state, Some(DoorState::Opening | DoorState::Open { .. }));
    let next = if rising {
        openness + step
    } else {
        openness - step
    };
    next.clamp(0.0, 1.0)
}

/// The openness to start this frame from: the one the spill had, when the portal still draws
/// through the same doorway (from either end), and none for a doorway it has just picked.
fn carried_openness(memory: &SpillMemory, next: DoorPair) -> f32 {
    match memory.door {
        Some(previous) if previous.same_doorway(next) => memory.openness,
        _ => 0.0,
    }
}

/// The two faces of the doorway in render space: where each side's opening is and which way is
/// into that side.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DoorwaySides {
    source_centre: Vec3,
    /// Out of the doorway into the source: the side the player stands on.
    into_source: Vec3,
    destination_centre: Vec3,
    /// In through the destination doorway, into the destination.
    into_destination: Vec3,
    /// The opening's width and height.
    size: Vec2,
    /// The point on the source side's floor where its light is measured: [`THRESHOLD_PATCH_DEPTH`]
    /// out from the threshold (the bottom edge of the doorway quad), [`THRESHOLD_LIFT`] above it.
    source_floor: Vec3,
    /// The same point inside the destination doorway, under the portal's map.
    destination_floor: Vec3,
    /// Along the threshold, in render space (the source side's; the destination's is its image).
    across: Vec3,
}

impl DoorwaySides {
    /// The doorway the portal placed, and its image under the portal's own map: the destination
    /// doorway is where the window says it is.
    fn of(doorway: &OpenDoorway) -> Self {
        let source_centre = doorway.quad.translation;
        let into_source = (doorway.map.frame * Vec3::NEG_Z).normalize_or(Vec3::NEG_Z);
        let (destination_centre, _) = doorway.map.pose(source_centre, Quat::IDENTITY);
        let (toward_source_image, _) = doorway
            .map
            .pose(source_centre + into_source, Quat::IDENTITY);
        let size = doorway.quad.scale.truncate().abs();
        let up = (doorway.quad.rotation * Vec3::Y).normalize_or(Vec3::Y);
        let across = (doorway.quad.rotation * Vec3::X).normalize_or(Vec3::X);
        let threshold = source_centre - up * (size.y * 0.5) + Vec3::Y * THRESHOLD_LIFT;
        let into_destination = (destination_centre - toward_source_image).normalize_or(Vec3::NEG_Z);
        let (destination_threshold, _) = doorway.map.pose(threshold, Quat::IDENTITY);
        Self {
            source_centre,
            into_source,
            destination_centre,
            // The image of the player's side is outside the destination room, where the portal
            // camera stands; the room is the other way.
            into_destination,
            size,
            source_floor: threshold + into_source * THRESHOLD_PATCH_DEPTH,
            destination_floor: destination_threshold + into_destination * THRESHOLD_PATCH_DEPTH,
            across,
        }
    }
}

/// The light one side of a doorway has at the doorway: a colour of luminance 1, and the
/// illuminance it carries.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SideLight {
    colour: LinearRgba,
    illuminance: f32,
}

/// The light on one side's threshold floor: its ambient, [`LIGHT_WEIGHT`] of `lights` - the
/// illuminance, per channel, its own `LIGH` lights put on that floor ([`lights_on_floor`]) - and
/// [`SUN_WEIGHT`] of `sun`, what its sun puts there ([`sun_on_floor`]).
fn side_light(atmosphere: &SpaceAtmosphere, lights: LinearRgba, sun: LinearRgba) -> SideLight {
    let ambient_colour = LinearRgba::from(atmosphere.ambient_color);
    let ambient = AMBIENT_AS_ILLUMINANCE * atmosphere.ambient_brightness;
    let total = ambient_colour * ambient + sun * SUN_WEIGHT + lights * LIGHT_WEIGHT;
    let illuminance = luma(Color::LinearRgba(total)).max(0.0);
    let colour = if illuminance > 0.0 {
        total * (1.0 / illuminance)
    } else {
        LinearRgba::WHITE
    };
    SideLight {
        colour: LinearRgba {
            alpha: 1.0,
            ..colour
        },
        illuminance,
    }
}

/// The illuminance, per channel, a point light puts on a surface facing it at `distance`, in the
/// units Bevy lights with: `intensity / (4 PI d^2)` times Bevy's range window
/// (`bevy_pbr/src/render/pbr_lighting.wgsl`, `getDistanceAttenuation`).
fn light_at(light: &PointLight, distance: f32) -> LinearRgba {
    let distance = distance.max(LIGHT_MIN_DISTANCE);
    let illuminance =
        light.intensity / (4.0 * PI * distance * distance) * range_window(distance, light.range);
    LinearRgba::from(light.color) * illuminance
}

/// The illuminance, per channel, a point light at `light_position` puts on an up-facing floor at
/// `floor`: [`light_at`] times the floor's cosine to the light (none from below).
fn light_on_floor(light: &PointLight, light_position: Vec3, floor: Vec3) -> LinearRgba {
    let to_light = light_position - floor;
    let distance = to_light.length();
    if distance <= f32::EPSILON {
        return LinearRgba::BLACK;
    }
    let cosine = (to_light.y / distance).max(0.0);
    light_at(light, distance) * cosine
}

/// The illuminance, per channel, a sun puts on an up-facing floor: its colour times its
/// illuminance, times the floor's cosine to it, times `lit`, the share of the threshold it reaches.
fn sun_on_floor(sun: &DirectionalLight, towards_sun: Vec3, lit: f32) -> LinearRgba {
    let cosine = towards_sun.normalize_or_zero().y.max(0.0);
    LinearRgba::from(sun.color) * (sun.illuminance * cosine * lit.clamp(0.0, 1.0))
}

/// Bevy's range window, `(1 - (d/r)^4)^2` clamped: 1 near the light, 0 at its range.
fn range_window(distance: f32, range: f32) -> f32 {
    if range <= 0.0 {
        return 0.0;
    }
    let fraction = distance / range;
    let window = (1.0 - fraction.powi(4)).clamp(0.0, 1.0);
    window * window
}

/// What a spill from `from` into `into` delivers at [`SPILL_REFERENCE_DISTANCE`], scaled by how
/// far the door has opened. Two parts, both zero when the two sides' light is the same:
///
/// * **brightening**, only when `into` is the dimmer side: [`SPILL_SHARE`] of the gap between the
///   two sides, at most [`SPILL_MAX_GAIN`] times `into`'s own light. The brighter side gets none,
///   so the spill narrows the brightness step and never widens it where the measure is right.
/// * **tint**, both ways alike: [`SPILL_TINT`] times the dimmer side's light times the hue gap
///   ([`colour_gap`]). Each side gets the same amount, in the other side's colour, so the colours
///   meet at the doorway while the step between the two stays much as it was.
fn spill_illuminance(from: &SideLight, into: &SideLight, openness: f32) -> f32 {
    let brighten = if from.illuminance > into.illuminance {
        (SPILL_SHARE * (from.illuminance - into.illuminance)).min(SPILL_MAX_GAIN * into.illuminance)
    } else {
        0.0
    };
    let tinted =
        SPILL_TINT * from.illuminance.min(into.illuminance) * colour_gap(from.colour, into.colour);
    (brighten + tinted).max(0.0) * openness.clamp(0.0, 1.0)
}

/// How far apart two lights' hues are: the distance between their chromaticities
/// `(r, g, b) / (r + g + b)`, 0 for the same hue.
fn colour_gap(a: LinearRgba, b: LinearRgba) -> f32 {
    let chromaticity = |c: LinearRgba| {
        let sum = (c.red + c.green + c.blue).max(1e-6);
        Vec3::new(c.red, c.green, c.blue) / sum
    };
    chromaticity(a).distance(chromaticity(b))
}

/// The intensity (Bevy's lumens) a spot light needs to deliver `illuminance` at
/// [`SPILL_REFERENCE_DISTANCE`] within [`SPILL_RANGE`] - the inverse of [`light_at`].
fn spot_intensity(illuminance: f32) -> f32 {
    let distance = SPILL_REFERENCE_DISTANCE;
    4.0 * PI * distance * distance * illuminance / range_window(distance, SPILL_RANGE)
}

/// The illuminance per unit of intensity a spot light at `transform` with `outer_angle` puts on an
/// up-facing floor at `floor`, as Bevy draws it: [`light_at`]'s inverse square and range window,
/// the floor's cosine to the light, and the cone's falloff between its inner and outer angles
/// (`bevy_pbr-0.19.0/src/render/pbr_lighting.wgsl`, the spot attenuation; the scale and offset from
/// `render/light.rs`).
fn spot_delivery(transform: &Transform, outer_angle: f32, floor: Vec3) -> f32 {
    let to_floor = floor - transform.translation;
    let distance = to_floor.length();
    if distance <= f32::EPSILON {
        return 0.0;
    }
    let towards_floor = to_floor / distance;
    let cos_outer = outer_angle.cos();
    let cos_inner = (outer_angle * SPILL_INNER_CONE_FRACTION).cos();
    let scale = 1.0 / (cos_inner - cos_outer).max(1e-4);
    let offset = -cos_outer * scale;
    let spot = (transform.forward().dot(towards_floor) * scale + offset).clamp(0.0, 1.0);
    let cosine = (-towards_floor.y).max(0.0);
    let distance = distance.max(LIGHT_MIN_DISTANCE);
    range_window(distance, SPILL_RANGE) * spot * spot * cosine / (4.0 * PI * distance * distance)
}

/// One spill light, as it should be this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Spill {
    transform: Transform,
    colour: LinearRgba,
    intensity: f32,
    outer_angle: f32,
}

/// A spill through the opening at `centre` (of `size`) into the side `into` points to: standing
/// [`SPILL_STANDOFF`] back from the plane, aimed [`SPILL_DOWN_TILT_DEGREES`] below `into`, its cone
/// the opening's corners seen from there, widened by the tilt so the head of the frame stays in it.
/// Its intensity puts `illuminance` on the threshold floor at `floor` ([`spot_delivery`]); a floor
/// the cone barely reaches falls back on [`spot_intensity`]'s reference distance.
fn spill_through(
    centre: Vec3,
    into: Vec3,
    size: Vec2,
    floor: Vec3,
    from: &SideLight,
    illuminance: f32,
) -> Spill {
    let tilt = SPILL_DOWN_TILT_DEGREES.to_radians();
    let standoff = SPILL_STANDOFF;
    let aim = (into * tilt.cos() - Vec3::Y * tilt.sin()).normalize_or(into);
    let position = centre - into * standoff;
    let half_opening = size.length() * 0.5;
    let transform = Transform::from_translation(position).looking_to(aim, Vec3::Y);
    let outer_angle = ((half_opening / standoff).atan() + tilt).min(SPILL_MAX_CONE);
    let reference = 1.0 / spot_intensity(1.0);
    let delivery = spot_delivery(&transform, outer_angle, floor);
    let intensity = if delivery >= reference * 0.1 {
        illuminance / delivery
    } else {
        spot_intensity(illuminance)
    };
    Spill {
        transform,
        colour: from.colour,
        intensity,
        outer_angle,
    }
}

/// The two spills of a doorway: the source's light into the destination, and the destination's
/// light into the source.
fn spill_pair(
    sides: &DoorwaySides,
    source: &SideLight,
    destination: &SideLight,
    openness: f32,
) -> (Spill, Spill) {
    (
        spill_through(
            sides.destination_centre,
            sides.into_destination,
            sides.size,
            sides.destination_floor,
            source,
            spill_illuminance(source, destination, openness),
        ),
        spill_through(
            sides.source_centre,
            sides.into_source,
            sides.size,
            sides.source_floor,
            destination,
            spill_illuminance(destination, source, openness),
        ),
    )
}

fn spawn_spill_lights(mut commands: Commands) {
    for spill in [SpillLight::IntoDestination, SpillLight::IntoSource] {
        commands.spawn((
            Name::new(match spill {
                SpillLight::IntoDestination => "Portal spill into destination",
                SpillLight::IntoSource => "Portal spill into source",
            }),
            spill,
            SpotLight {
                intensity: 0.0,
                range: SPILL_RANGE,
                shadow_maps_enabled: false,
                ..default()
            },
            Transform::default(),
            Visibility::Hidden,
            spill.layers(),
        ));
    }
}

/// What one side's own `LIGH` lights put on its threshold floor at `floor`: the lights on `layers`,
/// drawn this frame, each by [`light_on_floor`] (unshadowed, as they are drawn; a light past its
/// range adds nothing).
fn lights_on_floor<'a>(
    lights: impl IntoIterator<
        Item = (
            &'a GlobalTransform,
            &'a PointLight,
            Option<&'a RenderLayers>,
            &'a InheritedVisibility,
        ),
    >,
    layers: &RenderLayers,
    floor: Vec3,
) -> LinearRgba {
    lights
        .into_iter()
        .filter(|(_, _, own, visible)| {
            visible.get() && own.cloned().unwrap_or_default().intersects(layers)
        })
        .map(|(transform, light, ..)| light_on_floor(light, transform.translation(), floor))
        .fold(LinearRgba::BLACK, |sum, light| sum + light)
}

/// The sun of one side, as drawn: the directional light on `layers` and the way toward it.
fn side_sun<'a>(
    suns: impl IntoIterator<
        Item = (
            &'a GlobalTransform,
            &'a DirectionalLight,
            Option<&'a RenderLayers>,
        ),
    >,
    layers: &RenderLayers,
) -> Option<(&'a DirectionalLight, Vec3)> {
    suns.into_iter()
        .find(|(_, _, own)| own.cloned().unwrap_or_default().intersects(layers))
        .map(|(transform, sun, _)| (sun, -transform.forward().as_vec3()))
}

/// How much of each threshold the sun reaches, measured by ray casts toward it
/// ([`measure_threshold_sun`]) for the doorway in [`ThresholdSun::pair`].
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq)]
struct ThresholdSun {
    pair: Option<DoorPair>,
    /// The share, 0 to 1, of the source threshold's samples the sun reaches.
    source: f32,
    /// The same inside the destination doorway.
    destination: f32,
    /// When it was measured, in seconds of [`Time::elapsed_secs_f64`].
    measured_at: f64,
}

/// The sample points across a threshold: [`SUN_SAMPLES`] along `across`, over the middle two
/// thirds of the opening's `width`.
fn threshold_samples(floor: Vec3, across: Vec3, width: f32) -> impl Iterator<Item = Vec3> {
    (0..SUN_SAMPLES).map(move |i| {
        let t = if SUN_SAMPLES > 1 {
            i as f32 / (SUN_SAMPLES - 1) as f32 - 0.5
        } else {
            0.0
        };
        floor + across * (t * width * (2.0 / 3.0))
    })
}

/// Measures how much of each of the open doorway's thresholds the sun reaches: from each sample
/// point ([`threshold_samples`]) a ray toward the side's sun, against the meshes drawn on that
/// side's layers (the doorway quad aside). Measured when the doorway is new and every
/// [`SUN_REMEASURE_SECONDS`] after.
#[allow(clippy::too_many_arguments)]
fn measure_threshold_sun(
    time: Res<Time>,
    portal: Option<Res<PortalState>>,
    doors: Query<&LoadDoor>,
    suns: Query<(&GlobalTransform, &DirectionalLight, Option<&RenderLayers>)>,
    layers: Query<Option<&RenderLayers>>,
    quads: Query<(), With<PortalQuad>>,
    mut ray_cast: MeshRayCast,
    mut measured: ResMut<ThresholdSun>,
) {
    let Some(doorway) = portal.as_deref().and_then(PortalState::open_doorway) else {
        if measured.pair.is_some() {
            *measured = ThresholdSun::default();
        }
        return;
    };
    let Ok(door) = doors.get(doorway.door) else {
        return;
    };
    let pair = DoorPair::of(door);
    let now = time.elapsed_secs_f64();
    let fresh = measured
        .pair
        .is_some_and(|previous| previous.same_doorway(pair))
        && now - measured.measured_at < SUN_REMEASURE_SECONDS;
    if fresh {
        return;
    }
    let sides = DoorwaySides::of(&doorway);
    let mut lit = |side: SpillLight, floor: Vec3| -> f32 {
        let wanted = side.layers();
        let Some((_, towards_sun)) = side_sun(suns.iter(), &wanted) else {
            return 0.0;
        };
        let Ok(direction) = Dir3::new(towards_sun) else {
            return 0.0;
        };
        if towards_sun.y <= 0.0 {
            return 0.0;
        }
        let filter = |entity: Entity| {
            !quads.contains(entity)
                && layers
                    .get(entity)
                    .is_ok_and(|own| own.cloned().unwrap_or_default().intersects(&wanted))
        };
        let settings = MeshRayCastSettings::default()
            .with_filter(&filter)
            .with_visibility(RayCastVisibility::Visible)
            .always_early_exit();
        let samples: Vec<Vec3> = threshold_samples(floor, sides.across, sides.size.x).collect();
        let reached = samples
            .iter()
            .filter(|origin| {
                let hits = ray_cast.cast_ray(Ray3d::new(**origin, direction), &settings);
                hits.first()
                    .is_none_or(|(_, hit)| hit.distance > SUN_RAY_REACH)
            })
            .count();
        reached as f32 / samples.len().max(1) as f32
    };
    let source = lit(SpillLight::IntoSource, sides.source_floor);
    let destination = lit(SpillLight::IntoDestination, sides.destination_floor);
    *measured = ThresholdSun {
        pair: Some(pair),
        source,
        destination,
        measured_at: now,
    };
}

#[allow(clippy::too_many_arguments)]
fn update_spill(
    time: Res<Time>,
    portal: Option<Res<PortalState>>,
    active: Option<Res<ActiveCell>>,
    catalog: Option<Res<SpaceLightingCatalog>>,
    doors: Query<(&LoadDoor, Option<&DoorState>)>,
    lights: Query<
        (
            &GlobalTransform,
            &PointLight,
            Option<&RenderLayers>,
            &InheritedVisibility,
        ),
        With<SkyrimLight>,
    >,
    suns: Query<(&GlobalTransform, &DirectionalLight, Option<&RenderLayers>)>,
    shade: Option<Res<ThresholdSun>>,
    mut spills: Query<(&SpillLight, &mut Transform, &mut SpotLight, &mut Visibility)>,
    mut memory: ResMut<SpillMemory>,
) {
    let doorway = portal.as_deref().and_then(PortalState::open_doorway);
    let lit = doorway.zip(active).and_then(|(doorway, active)| {
        let (door, state) = doors.get(doorway.door).ok()?;
        let pair = DoorPair::of(door);
        let openness = step_openness(carried_openness(&memory, pair), state, time.delta_secs());
        let source_key = space_key(active.worldspace_id, active.interior);
        let destination_key: SpaceKey = space_key(
            door.destination.worldspace_id.unwrap_or_default(),
            door.destination.interior_cell_id,
        );
        let sides = DoorwaySides::of(&doorway);
        // The share of each threshold the sun reaches, when it was measured for this doorway.
        let (source_lit, destination_lit) = shade
            .as_deref()
            .filter(|shade| {
                shade
                    .pair
                    .is_some_and(|measured| measured.same_doorway(pair))
            })
            .map_or((0.0, 0.0), |shade| (shade.source, shade.destination));
        let floor_light = |side: SpillLight, floor: Vec3, lit: f32, key: SpaceKey| {
            let layers = side.layers();
            let sun = side_sun(suns.iter(), &layers).map_or(LinearRgba::BLACK, |(sun, toward)| {
                sun_on_floor(sun, toward, lit)
            });
            side_light(
                &crate::atmosphere::space_atmosphere(catalog.as_deref(), key),
                lights_on_floor(lights.iter(), &layers, floor),
                sun,
            )
        };
        let source = floor_light(
            SpillLight::IntoSource,
            sides.source_floor,
            source_lit,
            source_key,
        );
        let destination = floor_light(
            SpillLight::IntoDestination,
            sides.destination_floor,
            destination_lit,
            destination_key,
        );
        let previous = carried_openness(&memory, pair);
        let spills = spill_pair(&sides, &source, &destination, openness);
        memory.door = Some(pair);
        memory.openness = openness;
        if openness >= 1.0 && previous < 1.0 {
            info!(
                door = format_args!("{:08X}", pair.door),
                source_light = source.illuminance,
                destination_light = destination.illuminance,
                source_sun = source_lit,
                destination_sun = destination_lit,
                into_destination = spill_illuminance(&source, &destination, 1.0),
                into_source = spill_illuminance(&destination, &source, 1.0),
                "portal spill: doorway fully open"
            );
        }
        Some(spills)
    });
    if lit.is_none() {
        *memory = SpillMemory::default();
    }
    for (role, mut transform, mut light, mut visibility) in &mut spills {
        let wanted = lit.map(|(into_destination, into_source)| match role {
            SpillLight::IntoDestination => into_destination,
            SpillLight::IntoSource => into_source,
        });
        let Some(spill) = wanted.filter(|spill| spill.intensity > 0.0) else {
            if *visibility != Visibility::Hidden {
                *visibility = Visibility::Hidden;
            }
            continue;
        };
        if *visibility != Visibility::Inherited {
            *visibility = Visibility::Inherited;
        }
        if *transform != spill.transform {
            *transform = spill.transform;
        }
        let colour = Color::LinearRgba(spill.colour);
        if light.color != colour
            || light.intensity != spill.intensity
            || light.outer_angle != spill.outer_angle
        {
            light.color = colour;
            light.intensity = spill.intensity;
            light.outer_angle = spill.outer_angle;
            light.inner_angle = spill.outer_angle * SPILL_INNER_CONE_FRACTION;
            light.range = SPILL_RANGE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::lighting::{SpaceFog, SunLight};

    fn daylit_exterior() -> SpaceAtmosphere {
        SpaceAtmosphere {
            ambient_color: Color::srgb(0.48, 0.55, 0.7),
            // What `crate::atmosphere`'s sky fill makes of this sun: a tenth of it, over
            // `0.4524 * luma(colour)`.
            ambient_brightness: 5000.0,
            backdrop: Color::srgb(0.52, 0.64, 0.80),
            fog: SpaceFog::Ring,
            sun: SunLight {
                color: Color::srgb(1.0, 0.95, 0.9),
                illuminance: 19_967.5,
            },
            has_sky: true,
        }
    }

    fn house_interior() -> SpaceAtmosphere {
        SpaceAtmosphere {
            ambient_color: Color::srgb(0.73, 0.87, 0.86),
            ambient_brightness: 480.0,
            backdrop: Color::BLACK,
            fog: SpaceFog::Unreachable,
            sun: SunLight::OFF,
            has_sky: false,
        }
    }

    fn night_exterior() -> SpaceAtmosphere {
        SpaceAtmosphere {
            ambient_brightness: 60.0,
            sun: SunLight::OFF,
            ..daylit_exterior()
        }
    }

    /// What a hearth across the room puts on the threshold floor, per channel.
    fn hearth() -> LinearRgba {
        LinearRgba::rgb(300.0, 180.0, 70.0)
    }

    fn sides() -> DoorwaySides {
        DoorwaySides {
            source_centre: Vec3::new(0.0, 100.0, 0.0),
            into_source: Vec3::NEG_Z,
            destination_centre: Vec3::new(5000.0, 100.0, 0.0),
            into_destination: Vec3::X,
            size: Vec2::new(120.0, 220.0),
            source_floor: Vec3::new(0.0, 2.0, -24.0),
            destination_floor: Vec3::new(5024.0, 2.0, 0.0),
            across: Vec3::X,
        }
    }

    #[test]
    fn the_spill_fades_in_with_the_swing_and_out_with_the_close() {
        let mut openness = 0.0;
        openness = step_openness(
            openness,
            Some(&DoorState::Opening),
            SPILL_FADE_SECONDS * 0.5,
        );
        assert!(
            (openness - 0.5).abs() < 1e-5,
            "half way through the fade: {openness}"
        );
        openness = step_openness(
            openness,
            Some(&DoorState::Open { animated: true }),
            SPILL_FADE_SECONDS,
        );
        assert_eq!(openness, 1.0);
        openness = step_openness(
            openness,
            Some(&DoorState::Closing),
            SPILL_FADE_SECONDS * 0.25,
        );
        assert!((openness - 0.75).abs() < 1e-5);
        assert_eq!(step_openness(openness, Some(&DoorState::Closed), 10.0), 0.0);
        assert_eq!(step_openness(0.4, None, 10.0), 0.0, "no state is closed");

        let from = side_light(&daylit_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        let into = side_light(&house_interior(), hearth(), LinearRgba::BLACK);
        assert_eq!(spill_illuminance(&from, &into, 0.0), 0.0);
        let half = spill_illuminance(&from, &into, 0.5);
        let full = spill_illuminance(&from, &into, 1.0);
        assert!(full > 0.0 && (half * 2.0 - full).abs() < full * 1e-5);
    }

    #[test]
    fn the_spill_is_there_only_while_the_portal_draws_through_a_door() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<SpillMemory>()
            .init_resource::<PortalState>()
            .insert_resource(ActiveCell {
                worldspace_id: 0x3C,
                interior: None,
            })
            .add_systems(Startup, spawn_spill_lights)
            .add_systems(Update, update_spill);
        app.update();
        app.update();
        let mut query = app
            .world_mut()
            .query::<(&SpillLight, &Visibility, &RenderLayers)>();
        let lights: Vec<_> = query.iter(app.world()).collect();
        assert_eq!(lights.len(), 2);
        for (role, visibility, layers) in lights {
            assert_eq!(*visibility, Visibility::Hidden, "{role:?} with no doorway");
            assert_eq!(*layers, role.layers());
        }
        assert_eq!(
            *app.world().resource::<SpillMemory>(),
            SpillMemory::default()
        );
    }

    #[test]
    fn each_spill_is_on_its_own_sides_layers() {
        assert!(
            SpillLight::IntoDestination
                .layers()
                .intersects(&RenderLayers::layer(DESTINATION_LAYER))
        );
        assert!(
            !SpillLight::IntoDestination
                .layers()
                .intersects(&RenderLayers::from_layers(&[0, 1]))
        );
        assert!(
            SpillLight::IntoSource
                .layers()
                .intersects(&RenderLayers::layer(0))
        );
        assert!(
            !SpillLight::IntoSource
                .layers()
                .intersects(&RenderLayers::layer(DESTINATION_LAYER))
        );
    }

    #[test]
    fn the_spill_into_each_side_takes_the_other_sides_colour() {
        let exterior = side_light(&daylit_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        let interior = side_light(&house_interior(), hearth(), LinearRgba::BLACK);
        let (into_destination, into_source) = spill_pair(&sides(), &exterior, &interior, 1.0);
        assert_eq!(into_destination.colour, exterior.colour);
        assert_eq!(into_source.colour, interior.colour);
        // The hearth is warm, the day is not.
        assert!(interior.colour.red > interior.colour.blue);
        assert!(exterior.colour.blue > interior.colour.blue);
        // Each stands on the side its light comes from and aims into the other.
        let sides = sides();
        let aim = |spill: &Spill| spill.transform.forward().as_vec3();
        assert!(
            (into_destination.transform.translation - sides.destination_centre)
                .dot(sides.into_destination)
                < 0.0
        );
        assert!(aim(&into_destination).dot(sides.into_destination) > 0.5);
        assert!(aim(&into_destination).y < 0.0, "aimed down at the floor");
        assert!(
            (into_source.transform.translation - sides.source_centre).dot(sides.into_source) < 0.0
        );
        assert!(aim(&into_source).dot(sides.into_source) > 0.5);
    }

    #[test]
    fn firelight_spills_onto_a_porch_at_night_far_more_than_by_day() {
        let interior = side_light(&house_interior(), hearth(), LinearRgba::BLACK);
        let day = side_light(&daylit_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        let night = side_light(&night_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        assert!(day.illuminance > interior.illuminance);
        assert!(night.illuminance < interior.illuminance);
        let by_day = spill_illuminance(&interior, &day, 1.0);
        let by_night = spill_illuminance(&interior, &night, 1.0);
        assert!(by_night > 0.0, "the night porch takes the firelight");
        // Against the porch's own light: a lift at night, at most the tint by day.
        let lift_by_day = by_day / day.illuminance;
        let lift_by_night = by_night / night.illuminance;
        assert!(
            lift_by_night > 3.0 * lift_by_day,
            "{lift_by_night} against {lift_by_day}"
        );
    }

    #[test]
    fn two_sides_in_the_same_light_get_no_spill() {
        for atmosphere in [daylit_exterior(), house_interior(), night_exterior()] {
            let side = side_light(&atmosphere, hearth(), LinearRgba::BLACK);
            let (into_destination, into_source) = spill_pair(&sides(), &side, &side, 1.0);
            assert_eq!(spill_illuminance(&side, &side, 1.0), 0.0);
            assert_eq!(into_destination.intensity, 0.0);
            assert_eq!(into_source.intensity, 0.0);
        }
    }

    #[test]
    fn only_the_dimmer_side_is_brightened() {
        // Two sides of one hue, one brighter: no tint, so all the light is brightening.
        let bright = side_light(&daylit_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        let dim = side_light(
            &SpaceAtmosphere {
                ambient_brightness: 1000.0,
                ..daylit_exterior()
            },
            LinearRgba::BLACK,
            LinearRgba::BLACK,
        );
        assert!(colour_gap(bright.colour, dim.colour) < 1e-6);
        let into_dim = spill_illuminance(&bright, &dim, 1.0);
        assert_eq!(spill_illuminance(&dim, &bright, 1.0), 0.0);
        let gap = bright.illuminance - dim.illuminance;
        assert!(
            (into_dim - SPILL_SHARE * gap).abs() < gap * 1e-5,
            "{into_dim}"
        );
        // A far brighter side cannot flood the dim one.
        let dark = side_light(
            &SpaceAtmosphere {
                ambient_brightness: 10.0,
                ..daylit_exterior()
            },
            LinearRgba::BLACK,
            LinearRgba::BLACK,
        );
        let into_dark = spill_illuminance(&bright, &dark, 1.0);
        assert!((into_dark - SPILL_MAX_GAIN * dark.illuminance).abs() < into_dark * 1e-5);
    }

    #[test]
    fn the_tint_is_the_same_both_ways() {
        // A warm house brighter than a blue night: the porch gets the brightening and the tint,
        // the house only the tint, and the tint is the same amount on both sides.
        let interior = side_light(&house_interior(), hearth(), LinearRgba::BLACK);
        let night = side_light(&night_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        let into_house = spill_illuminance(&night, &interior, 1.0);
        let onto_porch = spill_illuminance(&interior, &night, 1.0);
        let tint = SPILL_TINT * night.illuminance * colour_gap(night.colour, interior.colour);
        assert!(
            (into_house - tint).abs() <= tint * 1e-4,
            "{into_house} against {tint}"
        );
        let brighten = (SPILL_SHARE * (interior.illuminance - night.illuminance))
            .min(SPILL_MAX_GAIN * night.illuminance);
        assert!((onto_porch - tint - brighten).abs() < onto_porch * 1e-4);
    }

    #[test]
    fn the_spills_swap_with_the_worlds_at_a_crossing() {
        let exterior = side_light(&daylit_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        let interior = side_light(&house_interior(), hearth(), LinearRgba::BLACK);
        let outside = sides();
        // Across the doorway the destination is the side the player now stands in.
        let inside = DoorwaySides {
            source_centre: outside.destination_centre,
            into_source: outside.into_destination,
            destination_centre: outside.source_centre,
            into_destination: outside.into_source,
            size: outside.size,
            source_floor: outside.destination_floor,
            destination_floor: outside.source_floor,
            across: outside.across,
        };
        let (daylight_in, firelight_out) = spill_pair(&outside, &exterior, &interior, 1.0);
        let (firelight_out_after, daylight_in_after) =
            spill_pair(&inside, &interior, &exterior, 1.0);
        assert_eq!(
            daylight_in, daylight_in_after,
            "the daylight stays on the floor"
        );
        assert_eq!(
            firelight_out, firelight_out_after,
            "the firelight stays on the porch"
        );

        // The fade carries across: the far door of the same doorway keeps the openness.
        let near = DoorPair {
            door: 0x1CBB0,
            leads_to: 0x1CBB1,
        };
        let far = DoorPair {
            door: 0x1CBB1,
            leads_to: 0x1CBB0,
        };
        let memory = SpillMemory {
            door: Some(near),
            openness: 0.8,
        };
        assert_eq!(carried_openness(&memory, near), 0.8);
        assert_eq!(carried_openness(&memory, far), 0.8);
        let other = DoorPair {
            door: 0x2000,
            leads_to: 0x2001,
        };
        assert_eq!(carried_openness(&memory, other), 0.0);
    }

    #[test]
    fn a_spill_puts_its_illuminance_on_the_threshold_floor() {
        let sides = sides();
        let side = side_light(&daylit_exterior(), LinearRgba::BLACK, LinearRgba::BLACK);
        for (centre, into, floor) in [
            (
                sides.destination_centre,
                sides.into_destination,
                sides.destination_floor,
            ),
            (sides.source_centre, sides.into_source, sides.source_floor),
        ] {
            let spill = spill_through(centre, into, sides.size, floor, &side, 1000.0);
            let delivered =
                spill.intensity * spot_delivery(&spill.transform, spill.outer_angle, floor);
            assert!((delivered - 1000.0).abs() < 1.0, "{delivered}");
        }
    }

    #[test]
    fn a_light_lights_the_floor_by_its_cosine_to_it() {
        let light = PointLight {
            intensity: 1.0e6,
            range: 1000.0,
            color: Color::WHITE,
            ..default()
        };
        let floor = Vec3::ZERO;
        let above = light_on_floor(&light, Vec3::new(0.0, 200.0, 0.0), floor);
        assert_eq!(above, light_at(&light, 200.0));
        let level = light_on_floor(&light, Vec3::new(200.0, 0.0, 0.0), floor);
        assert_eq!(
            luma(Color::LinearRgba(level)),
            0.0,
            "a light level with the floor"
        );
        let below = light_on_floor(&light, Vec3::new(0.0, -200.0, 0.0), floor);
        assert_eq!(luma(Color::LinearRgba(below)), 0.0, "nor one under it");
        let slant = light_on_floor(&light, Vec3::new(200.0, 200.0, 0.0), floor);
        let expected = luma(Color::LinearRgba(light_at(&light, 200.0 * 2f32.sqrt())))
            * std::f32::consts::FRAC_1_SQRT_2;
        assert!((luma(Color::LinearRgba(slant)) - expected).abs() < expected * 1e-4);

        let sun = DirectionalLight {
            illuminance: 10_000.0,
            color: Color::WHITE,
            ..default()
        };
        let overhead = luma(Color::LinearRgba(sun_on_floor(&sun, Vec3::Y, 1.0)));
        assert!((overhead - 10_000.0).abs() < 1.0);
        let shaded = luma(Color::LinearRgba(sun_on_floor(&sun, Vec3::Y, 0.0)));
        assert_eq!(shaded, 0.0);
    }

    #[test]
    fn the_sun_is_sampled_across_the_middle_of_the_threshold() {
        let samples: Vec<Vec3> = threshold_samples(Vec3::ZERO, Vec3::X, 120.0).collect();
        assert_eq!(samples.len(), SUN_SAMPLES);
        assert!(samples.iter().all(|p| p.x.abs() <= 40.0 + 1e-3));
        assert!(samples.iter().any(|p| p.x.abs() < 1e-3));
    }

    #[test]
    fn a_spot_delivers_the_illuminance_it_was_sized_for() {
        let intensity = spot_intensity(1000.0);
        let light = PointLight {
            intensity,
            range: SPILL_RANGE,
            color: Color::WHITE,
            ..default()
        };
        let delivered = luma(Color::LinearRgba(light_at(
            &light,
            SPILL_REFERENCE_DISTANCE,
        )));
        assert!((delivered - 1000.0).abs() < 1.0, "{delivered}");
    }

    #[test]
    fn the_destination_doorway_is_the_portals_image_of_the_source_one() {
        let map = crate::transition::DoorMap {
            pivot: Vec3::new(0.0, 0.0, 0.0),
            frame: Quat::IDENTITY,
            arrival_position: Vec3::new(1000.0, 0.0, 500.0),
            arrival_rotation: Quat::from_rotation_y(PI * 0.5),
        };
        let doorway = OpenDoorway {
            door: Entity::PLACEHOLDER,
            map,
            quad: Transform::from_xyz(0.0, 100.0, 0.0).with_scale(Vec3::new(120.0, 220.0, 1.0)),
        };
        let sides = DoorwaySides::of(&doorway);
        assert_eq!(sides.into_source, Vec3::NEG_Z);
        assert!(
            sides
                .destination_centre
                .distance(Vec3::new(1000.0, 100.0, 500.0))
                < 1e-3
        );
        // The destination doorway faces into the room along the arrival facing.
        let (_, arrival_facing) = map.destination_plane();
        assert!(sides.into_destination.dot(arrival_facing) > 0.999);
        assert_eq!(sides.size, Vec2::new(120.0, 220.0));
    }
}
