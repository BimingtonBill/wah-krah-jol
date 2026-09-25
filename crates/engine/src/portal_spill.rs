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
//! A side's light ([`side_light`]) is its ambient plus its sun (a share of it: what reaches a
//! doorway is mostly sky and bounce, not the disc) plus the brightest of its own `LIGH` lights at
//! the doorway. What spills from one side into the other is a share of that, scaled by the ratio of
//! the two sides' ambients where the side it comes from is the dimmer one ([`spill_illuminance`]):
//! a lit house spills a little firelight onto a daylit porch, and much more at night. The rule is
//! the same both ways, so when the player crosses and the roles swap with the worlds, each side
//! keeps the spill it had - only the layer the light is on changes ([`spill_pair`]).
//!
//! The lights cast no shadows and are not [`crate::lights::SkyrimLight`]s, so the light budget does
//! not touch them. Bevy's clustered lights respect `RenderLayers` per view
//! (`bevy_light-0.19.0/src/cluster/assign.rs:459`), so each light reaches only its own view.
//!
//! # Tuning
//!
//! The constants marked as fitted were fitted (impl-210) on two Riverwood house doors in daylight:
//! Sven's House (`0001CBB0`) and the house door `00013424`, each from the user's close-up capture
//! (`local/captures/2026-09-25_10-27-59/01.png`, `03.png`) and from a square view 260 units out.
//! The score was, per shot, the luminance step (larger over smaller, logged) plus the chroma
//! difference between two patches either side of the doorway's plane on the threshold or the
//! frame, summed over the four shots: 4.40 with no spill, 2.87 at the values below. They were then
//! checked on doorways the fit never saw (Gerdur's House, the Riverwood Trader, Honningbrew
//! Meadery and an Alftand interior door): the same score went from 4.02 to 2.82 over the three
//! Riverwood shots, all of it from the colour (the chroma difference roughly thirds). The
//! luminance step did **not** hold: where the floor inside is already the brighter side (Gerdur's
//! close-up 1.63x -> 2.51x, the Trader 1.31x -> 1.60x) the spill adds light to it. A spill that
//! only brightens the dimmer side, or that tints rather than adds, is the next step.

use crate::{
    doors::{DoorState, LoadDoor},
    lights::SkyrimLight,
    portal::{DESTINATION_LAYER, OpenDoorway, PortalFrame, PortalState},
    streaming::ActiveCell,
    world::lighting::{SpaceAtmosphere, SpaceKey, SpaceLightingCatalog, luma, space_key},
};
use bevy::{camera::visibility::RenderLayers, prelude::*};
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

/// The distance from the light at which a spill delivers [`SPILL_SHARE`] of the side's light, in
/// Creation units: about the threshold's distance from the light along the aim, so the threshold
/// is what the share is fitted on.
const SPILL_REFERENCE_DISTANCE: f32 = 128.0;

/// How much of a side's light ([`side_light`]) a spill delivers at [`SPILL_REFERENCE_DISTANCE`].
/// Fitted (see "Tuning" above): 0.2 to 0.8 were tried; the chroma difference falls steadily with
/// it (0.32 -> 0.02 on capture 01) and the luminance step is flat past 0.6, so 0.8 is the best
/// total.
const SPILL_SHARE: f32 = 0.8;

/// The share of a side's sun that counts toward the light at its doorway. The doorway sees the
/// sky and the lit ground, not the sun's disc, and a porch is often in the shade of its own roof.
/// Fitted (see "Tuning" above): 0, 0.02, 0.05 and 0.15 were tried, and every share above 0 scored
/// worse: the sky fill already carries a tenth of the sun (`crate::atmosphere`'s `SKY_FILL`), and
/// any more makes the floor inside brighter than the shaded porch outside it. Kept as a named zero
/// so the term is there to fit again once the sun has a time of day.
const SUN_SPILL_SHARE: f32 = 0.0;

/// How far from a doorway a side's own `LIGH` light may stand and still count as its brightest
/// nearby light, in Creation units.
const LIGHT_SEARCH_RADIUS: f32 = 768.0;

/// The nearest a `LIGH` light is taken to be to the doorway when its light there is measured, in
/// Creation units: a torch mounted beside the frame would otherwise count as the whole room.
const LIGHT_MIN_DISTANCE: f32 = 96.0;

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
            .add_systems(Startup, spawn_spill_lights)
            .add_systems(Update, update_spill.after(PortalFrame));
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
        Self {
            source_centre,
            into_source,
            destination_centre,
            // The image of the player's side is outside the destination room, where the portal
            // camera stands; the room is the other way.
            into_destination: (destination_centre - toward_source_image).normalize_or(Vec3::NEG_Z),
            size: doorway.quad.scale.truncate().abs(),
        }
    }
}

/// The light one side of a doorway has at the doorway: a colour of luminance 1, and the
/// illuminance it carries; and the side's ambient alone, as an illuminance, which the spill's
/// ratio is taken on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SideLight {
    colour: LinearRgba,
    illuminance: f32,
    ambient: f32,
}

/// The light at one side's doorway: its ambient, [`SUN_SPILL_SHARE`] of its sun, and `brightest` -
/// the illuminance, per channel, its brightest nearby `LIGH` light puts on the doorway
/// ([`light_at`]).
fn side_light(atmosphere: &SpaceAtmosphere, brightest: Option<LinearRgba>) -> SideLight {
    let ambient_colour = LinearRgba::from(atmosphere.ambient_color);
    let ambient = AMBIENT_AS_ILLUMINANCE * atmosphere.ambient_brightness;
    let sun =
        LinearRgba::from(atmosphere.sun.color) * (atmosphere.sun.illuminance * SUN_SPILL_SHARE);
    let total = ambient_colour * ambient + sun + brightest.unwrap_or(LinearRgba::BLACK);
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
        ambient: ambient * luma(atmosphere.ambient_color).max(0.0),
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

/// Bevy's range window, `(1 - (d/r)^4)^2` clamped: 1 near the light, 0 at its range.
fn range_window(distance: f32, range: f32) -> f32 {
    if range <= 0.0 {
        return 0.0;
    }
    let fraction = distance / range;
    let window = (1.0 - fraction.powi(4)).clamp(0.0, 1.0);
    window * window
}

/// What a spill from `from` into `into` delivers at [`SPILL_REFERENCE_DISTANCE`]: [`SPILL_SHARE`]
/// of the light on the side it comes from, scaled by the ratio of the two ambients when the side it
/// comes from is the dimmer one - a daylit porch is not lit up by a hearth, a night-time one is -
/// and by how far the door has opened.
fn spill_illuminance(from: &SideLight, into: &SideLight, openness: f32) -> f32 {
    let ratio = if into.ambient > from.ambient && into.ambient > 0.0 {
        from.ambient / into.ambient
    } else {
        1.0
    };
    SPILL_SHARE * from.illuminance * ratio * openness.clamp(0.0, 1.0)
}

/// The intensity (Bevy's lumens) a spot light needs to deliver `illuminance` at
/// [`SPILL_REFERENCE_DISTANCE`] within [`SPILL_RANGE`] - the inverse of [`light_at`].
fn spot_intensity(illuminance: f32) -> f32 {
    let distance = SPILL_REFERENCE_DISTANCE;
    4.0 * PI * distance * distance * illuminance / range_window(distance, SPILL_RANGE)
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
fn spill_through(
    centre: Vec3,
    into: Vec3,
    size: Vec2,
    from: &SideLight,
    illuminance: f32,
) -> Spill {
    let tilt = SPILL_DOWN_TILT_DEGREES.to_radians();
    let standoff = SPILL_STANDOFF;
    let aim = (into * tilt.cos() - Vec3::Y * tilt.sin()).normalize_or(into);
    let position = centre - into * standoff;
    let half_opening = size.length() * 0.5;
    Spill {
        transform: Transform::from_translation(position).looking_to(aim, Vec3::Y),
        colour: from.colour,
        intensity: spot_intensity(illuminance),
        outer_angle: ((half_opening / standoff).atan() + tilt).min(SPILL_MAX_CONE),
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
            source,
            spill_illuminance(source, destination, openness),
        ),
        spill_through(
            sides.source_centre,
            sides.into_source,
            sides.size,
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

/// The brightest nearby `LIGH` light of one side at its doorway: the lights on `layers`, drawn this
/// frame, within [`LIGHT_SEARCH_RADIUS`] of `centre`.
fn brightest_light<'a>(
    lights: impl IntoIterator<
        Item = (
            &'a GlobalTransform,
            &'a PointLight,
            Option<&'a RenderLayers>,
            &'a InheritedVisibility,
        ),
    >,
    layers: &RenderLayers,
    centre: Vec3,
) -> Option<LinearRgba> {
    lights
        .into_iter()
        .filter(|(_, _, own, visible)| {
            visible.get() && own.cloned().unwrap_or_default().intersects(layers)
        })
        .filter_map(|(transform, light, ..)| {
            let distance = transform.translation().distance(centre);
            (distance <= LIGHT_SEARCH_RADIUS).then(|| light_at(light, distance))
        })
        .max_by(|a, b| luma(Color::LinearRgba(*a)).total_cmp(&luma(Color::LinearRgba(*b))))
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
        let source = side_light(
            &crate::atmosphere::space_atmosphere(catalog.as_deref(), source_key),
            brightest_light(
                lights.iter(),
                &SpillLight::IntoSource.layers(),
                sides.source_centre,
            ),
        );
        let destination = side_light(
            &crate::atmosphere::space_atmosphere(catalog.as_deref(), destination_key),
            brightest_light(
                lights.iter(),
                &SpillLight::IntoDestination.layers(),
                sides.destination_centre,
            ),
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

    fn hearth() -> LinearRgba {
        LinearRgba::rgb(3000.0, 1800.0, 700.0)
    }

    fn sides() -> DoorwaySides {
        DoorwaySides {
            source_centre: Vec3::new(0.0, 100.0, 0.0),
            into_source: Vec3::NEG_Z,
            destination_centre: Vec3::new(5000.0, 100.0, 0.0),
            into_destination: Vec3::X,
            size: Vec2::new(120.0, 220.0),
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

        let from = side_light(&daylit_exterior(), None);
        let into = side_light(&house_interior(), Some(hearth()));
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
        let exterior = side_light(&daylit_exterior(), None);
        let interior = side_light(&house_interior(), Some(hearth()));
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
        let interior = side_light(&house_interior(), Some(hearth()));
        let day = side_light(&daylit_exterior(), None);
        let night = side_light(&night_exterior(), None);
        let by_day = spill_illuminance(&interior, &day, 1.0);
        let by_night = spill_illuminance(&interior, &night, 1.0);
        assert!(by_day > 0.0);
        assert!(by_night > 3.0 * by_day, "{by_night} against {by_day}");
        // Daylight into the house is not scaled down: the house is the dimmer side.
        assert_eq!(
            spill_illuminance(&day, &interior, 1.0),
            SPILL_SHARE * day.illuminance
        );
    }

    #[test]
    fn the_spills_swap_with_the_worlds_at_a_crossing() {
        let exterior = side_light(&daylit_exterior(), None);
        let interior = side_light(&house_interior(), Some(hearth()));
        let outside = sides();
        // Across the doorway the destination is the side the player now stands in.
        let inside = DoorwaySides {
            source_centre: outside.destination_centre,
            into_source: outside.into_destination,
            destination_centre: outside.source_centre,
            into_destination: outside.into_source,
            size: outside.size,
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
