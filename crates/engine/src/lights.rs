//! Point lights from Skyrim `LIGH` references: the torches, braziers, Dwemer lamps and glowing
//! fungus that light Alftand and Blackreach in the game. See `docs/design/lights-and-auto-doors.md`.
//!
//! `streaming::spawn_cell` gives every reference whose base record has a row in the converter's
//! `lights` table a [`PointLight`] child. The light entity is a descendant of the cell root, so it
//! follows the render-origin rebases and the portal isolation of `crate::portal` that move the cell
//! hierarchy. This module owns the record-to-light conversion, the intensity scale, and the budget
//! that bounds how many lights are enabled at once.
//!
//! # The intensity scale: Creation units are not metres
//!
//! A `LIGH` row carries its radius in Creation units and this engine renders one Creation unit as
//! one Bevy world unit ([`CELL_SIZE`](crate::world::components::CELL_SIZE) is 4096 units across a
//! cell the game defines as 4096 Creation units), so `range` is the radius unchanged. Intensity is
//! what needs converting: Bevy's defaults are tuned for a metre-scale world, and in this world the
//! same distance is about 70 times larger, which loses a factor of 4900 to the inverse-square term.
//!
//! Bevy's point light contributes, at distance `d` from it,
//!
//! ```text
//! Lout = albedo * NdotL * (colour * intensity / (4*PI)) * window(d) / (PI * d^2)
//!      = albedo * NdotL * colour * intensity * window(d) / (4 * PI^2 * d^2)
//! window(d) = (1 - (d/range)^4)^2
//! ```
//!
//! The two `PI`s are easy to lose, and an earlier version of this derivation lost one of them.
//! `bevy_pbr/src/render/light.rs` divides a point light's intensity by `4*PI` on the CPU - Bevy's
//! `PointLight::intensity` is luminous power in lumens and the shader wants lumens per steradian
//! (`ExtractedPointLight`, `intensity: point_light.intensity / (4.0 * PI)`) - and `Fd_Burley` in
//! `bevy_pbr/src/render/pbr_lighting.wgsl` supplies the Lambert `1/PI`. Written out with only the
//! second one, as this module did, the light comes out `4*PI` too dim for the intensity asked for.
//!
//! The ambient light of `app.rs` contributes `albedo * ambient_colour * brightness` - no `NdotL`
//! and no `1/PI` (`bevy_pbr/src/render/light.rs`, `ambient_color: ... * ambient_light.brightness`,
//! and `pbr_ambient.wgsl`, `EnvBRDFApprox(diffuse_color, ..) * lights.ambient_color.rgb`). A
//! converted light is therefore given the intensity that makes the two equal at half its own
//! radius, times [`LIGHT_EXPOSURE`]:
//!
//! ```text
//! intensity = 4 * PI^2 * brightness * LIGHT_EXPOSURE * (radius/2)^2 / window(radius/2)
//! window(radius/2) = (1 - 1/16)^2 = 225/256
//! ```
//!
//! so `LIGHT_EXPOSURE` is the whole brightness knob for every converted light, in units of the
//! interior ambient the engine actually applies
//! ([`crate::atmosphere::INTERIOR_AMBIENT_BRIGHTNESS`] times
//! [`crate::atmosphere::INTERIOR_AMBIENT_LEVEL`]), and a 512-unit torch (a common `LIGH` radius)
//! gets about `1.4e10`. The light's own colour scales what a surface receives on top of that, as
//! the ambient's colour does on its side.
//!
//! # Why the reference distance stops at 256 units
//!
//! Sizing every light's intensity from its own `radius/2` reads the radius as brightness as well as
//! reach, and a `LIGH` radius is only reach: Skyrim's brightness scale is `FNAM` fade, and vanilla's
//! falloff is not inverse-square (UESP, quoted in `docs/research/visual-gaps-spec.md`, gap 4.3).
//! One record shows the difference at a glance - Blackreach's `FalmerCityLight02NS` (`000D9051`,
//! radius 4334, colour (216,128,39)) came out 71 times a 512-unit torch and flooded the cavern
//! ceiling orange in every reference-pose render (measured against the UESP reference screenshots,
//! 2026-09) - so the reference distance is capped at [`INTENSITY_REFERENCE_RADIUS`]. Every radius
//! up to twice that cap keeps exactly the intensity the fit was calibrated with, and a bigger
//! light keeps its reach without a brightness that grows with the square of it.
//!
//! # Emissive: the same unit mismatch, the other way round
//!
//! [`EMISSIVE_EXPOSURE`] lives here beside [`LIGHT_EXPOSURE`] because it is the same problem: a
//! streamed material's emissive arrives at the magnitude the converter wrote, which is ~1000 times
//! below what the ambient of `app.rs` lights the world in. `crate::render` applies it.
//!
//! # The budget counts the lights a camera draws
//!
//! [`budget_lights`] enables the [`ENABLED_LIGHT_BUDGET`] nearest lights **that a view of the engine
//! renders**, so a slot can only be spent on light the player can see. Which those are is the
//! portal's answer rather than a second opinion about the world: `crate::portal::isolate_cells`
//! moves every resident cell that is not the active space off the main camera's render layers, and a
//! light's own layers are what each view of the engine tests it against.
//!
//! * A light of the **active space** keeps the layers it was spawned with - none at all, which is
//!   Bevy's layer 0, the main camera's - and is counted.
//! * A light of the **portal's destination** carries
//!   [`DESTINATION_LAYER`](crate::portal::DESTINATION_LAYER), the portal camera's own layer, and is
//!   counted as well: a doorway is a view of that cell, drawn into the room the player stands in,
//!   and the torches behind it are part of what that view shows. Leaving them out would leave every
//!   doorway image lit by the destination's ambient alone. The cost is the one
//!   `docs/research/portal-prior-art.md` problem D records - Bevy does not respect `RenderLayers`
//!   for lights, so a destination light also reaches the active space through the wall the doorway
//!   is set into - and that leak is the same before and after this rule: what is decided here is
//!   only whether those lights may hold one of the 64 slots, and the doorway image is why they may.
//! * A light of a **hidden cell** - any other resident cell, which is what a door's destination is
//!   while the player is not looking through it - carries `RenderLayers::none()` and is counted by
//!   nothing: no camera draws it, and its `LIGH` record lights a room the player cannot be in. Those
//!   are the lights that used to take slots from the space the player stands in.
//!
//! `crate::portal`'s own `CellRole` is the same division of the world, per frame and per cell root;
//! a light carries the answer on itself, which is what lets the budget be one system in one module.
//!
//! The choice is cached, and re-made when the camera has moved [`BUDGET_RECHOOSE_DISTANCE`], when a
//! light has spawned or despawned, or when the set of lights that can be seen has changed in any
//! other way - a cell streaming in or out at an equal count, a cell changing role while the player
//! stands still ([`LightBudget::chosen`]). Keying the cache on the number of spawned lights alone
//! left a room walked into and stopped in dark until the player moved another 256 units, and left a
//! doorway's own lights on after the doorway stopped being drawn.
//!
//! # What is not done here
//!
//! `LIGH` `DATA`'s falloff exponent, `FOV` and near clip are loaded but not applied: Bevy's point
//! light decays inverse-square and has neither a cone nor a near clip. Flicker, pulse and the
//! `DATA` time field are not implemented either - a torch burns steadily. Shadows are off for every
//! converted light (`PointLight::shadow_maps_enabled` is one cube map per light, which a 64-light
//! budget cannot afford and Skyrim's lights do not cast), so a light also lights the far side of
//! the wall it is mounted on. The reference's `XRDS` radius override *is* applied; see
//! [`radius_of`].

use crate::world::{components::StreamingCamera, database::LightRow};
use bevy::{camera::visibility::RenderLayers, prelude::*};
use std::collections::HashSet;

/// `LIGH` `DATA` flag bit: the record is off until something turns it on, so a reference to it is
/// not lit by default (UESP, "Skyrim Mod:Mod File Format/LIGH").
///
/// The bit's meaning is UESP's, not measured: no `LIGH` record in the 506 of `Skyrim.esm`,
/// `Dawnguard.esm`, `Dragonborn.esm`, `HearthFires.esm` or the installed Creation Club plugins
/// sets it (an unmodded Anniversary install was scanned), so skipping these can never take a light
/// out of the shipped world.
pub const LIGHT_FLAG_OFF_BY_DEFAULT: u32 = 0x0000_0020;

/// `LIGH` `DATA` flag bit: the light removes light instead of adding it (UESP). Skyrim uses these
/// to darken a room; this engine has no way to subtract a clustered light, so they are skipped.
///
/// Like [`LIGHT_FLAG_OFF_BY_DEFAULT`], no shipped record sets it.
pub const LIGHT_FLAG_NEGATIVE: u32 = 0x0000_0004;

/// How many times the ambient an interior applies a converted light puts on a surface at half its
/// own radius. This one constant is the brightness knob for every converted light.
///
/// The delivery is per radius and not per record: a light delivers `LIGHT_EXPOSURE` times the
/// ambient at `radius/2`, falls to zero at `radius`, and is inverse-square in between, so the value
/// sets how bright a torch pool is against the room around it. The intensity scale is chosen per
/// radius so that every `LIGH` record delivers the same surface brightness at half its own reach,
/// which is what makes one number usable for a 75-unit Dwarven lamp and a 3300-unit Blackreach water
/// light alike.
///
/// # Why 10 rather than 50
///
/// 50 was fitted on the Alftand and Blackreach spaces, and every surface in those sits **outside** a
/// light's near field: a cave is tens of thousands of units across and its radius-256 records put
/// its surfaces at a small fraction of `radius/2`, where a light is a pool on the wall it is mounted
/// on and not a wash over the room. 50 was never validated in a space whose surfaces are all
/// *inside* that near field - and the four Riverwood houses are exactly such spaces: about 900 units
/// across, five to eight radius-512 records each, so every surface in them is at 0.2 to 0.6 of a
/// radius, where the delivered ratio ran to 137x the ambient at 150 units and 48x at 256. That is
/// why `local/reference/rw6/RW-10-trader-shop-floor.png` and `RW-12-faendals-house.png` are a flat
/// cream wash with no dark anywhere in them.
///
/// The size of the cut is measured on the frames it changes rather than fitted on them
/// (`docs/research/light-and-exposure-audit.md`, after `research-074-interior-light-audit.1`):
///
/// * `rw6/RW-11-alvors-house`'s dim band reads R/G 1.505 and B/G 0.432 - the house lamp's
///   fingerprint, where the four houses' own `space_lighting` ambient is `(45, 48, 48)` at an R/G of
///   0.89. Solving that band as a mix of two illuminants puts the lights at 7-15x the ambient.
/// * the same frame's dim band is 11.3x its UESP reference's.
/// * in `render-cal/SR-interior-Alftand_12.png` the floor is 15-25x that frame's own dim band where
///   the reference's is 2-5x. The Alftand fit passed regardless because it was fitted on pooled
///   *medians*, which an evenly lit floor does not move.
///
/// That is a band of 4 to 20x, best estimate 10x, and this takes its conservative end. With
/// [`HALF_RADIUS_ILLUMINANCE`] stated against the ambient an interior actually applies, 50 of the
/// old brightness (800) and 10 of the new (480) are an **8.3x cut** of what a light delivers; the
/// houses' own frames ask for 10-11x, and stopping at 8.3x is what keeps the Alftand poses - the
/// ones this was *not* measured on - as close to where they were as the cut allows.
///
/// The name is not new. Before the `4*PI` fix the constant was `EXPOSURE_CALIBRATION = 50` and the
/// formula it scaled was missing a factor of `4*PI` (see the module documentation), so the lights
/// delivered about `50 / 4*PI` = 4 times the interior ambient where the number said 50. That is why
/// they read as invisible next to the camera lantern, and why the lantern was left in to compensate.
/// The 50 this replaces is a different mistake of the same shape: a number that was read off the
/// spaces where a light is never near the surfaces it lights.
pub const LIGHT_EXPOSURE: f32 = 10.0;

/// The largest reference distance [`intensity_for_radius`] sizes a light's intensity from: a light
/// with a radius up to `2 * INTENSITY_REFERENCE_RADIUS` is lit at its own `radius/2`, a bigger one
/// at this distance whatever its radius.
///
/// The cap is what keeps a radius from meaning brightness as well as reach (see the module
/// documentation). 512 units of radius is the widest common `LIGH` radius and the one the fit was
/// calibrated on, so every light up to it is byte-identical to the calibrated formula; Blackreach's
/// 4334-unit `FalmerCityLight02NS` drops from 71 times a torch to exactly one torch, at the same
/// reach.
pub const INTENSITY_REFERENCE_RADIUS: f32 = 256.0;

/// How many times its published `emissiveFactor` a streamed Skyrim *glow* is rendered at.
///
/// The other half of the unit mismatch [`LIGHT_EXPOSURE`] undoes: the converter writes emissive at
/// the magnitude Skyrim's own material files carry (the Blackreach mushroom caps are 2.0 to 3.6,
/// `docs/research/visual-gaps-spec.md` gap 1), the ambient an interior of `crate::atmosphere`
/// applies is [`crate::atmosphere::INTERIOR_AMBIENT_BRIGHTNESS`] at
/// [`crate::atmosphere::INTERIOR_AMBIENT_LEVEL`] = 480 and a converted light is [`LIGHT_EXPOSURE`]
/// times that, 4,800, so an unscaled glow is about a
/// thousandth of what it has to be seen against and reads as black. This is the brightness knob for
/// every glow of the game, as [`LIGHT_EXPOSURE`] is for its lights;
/// `crate::render::SkyrimMaterialHandler` multiplies each streamed glow's emissive by it.
///
/// It applies only to the materials the converter marks as deliberate emitters - the ones whose
/// emissive multiple is above 1, which is what `KHR_materials_emissive_strength` publishes
/// (`crate::render::is_deliberate_glow`). The rest of the emissives a Skyrim model carries are
/// own-emit *surface* materials - the snow-covered trees, the ice of the Alftand ravine, the
/// landscape ice - which are already at the brightness the game gives them and which this constant
/// turns white.
///
/// Fitted on the UESP Blackreach reference poses (2026-09): the value that puts the mushroom caps
/// and the `BlackreachSun01` orb at the brightness their reference frames give them, without taking
/// a reference frame's clipped fraction above what the reference itself has.
pub const EMISSIVE_EXPOSURE: f32 = 100.0;

/// The illuminance a converted light is tuned to deliver at half its own radius, in Bevy's ambient
/// units: [`LIGHT_EXPOSURE`] times the ambient an interior applies - `crate::atmosphere`'s
/// [`crate::atmosphere::INTERIOR_AMBIENT_BRIGHTNESS`] *at the level it is applied at*
/// ([`crate::atmosphere::INTERIOR_AMBIENT_LEVEL`]), which is the ambient the room actually has.
///
/// The level belongs here. Stating the constant against the brightness alone read as 50 times the
/// ambient while the room was lit at 480 of its 800, so a light delivered 83x the ambient it was
/// standing in rather than the 50x the constant and its test both said - a 1.67x error by
/// construction, and one that no test could see because the test made the same assumption.
const HALF_RADIUS_ILLUMINANCE: f32 = 4.0
    * core::f32::consts::PI
    * core::f32::consts::PI
    * crate::atmosphere::INTERIOR_AMBIENT_BRIGHTNESS
    * crate::atmosphere::INTERIOR_AMBIENT_LEVEL
    * LIGHT_EXPOSURE;

/// Bevy's falloff window at half a light's range: `(1 - (d/range)^4)^2` at `d = range/2`
/// (`bevy_pbr/src/render/pbr_lighting.wgsl`, `getRangeFalloff`).
const HALF_RADIUS_WINDOW: f32 = 225.0 / 256.0;

/// How many of the lights a view renders are enabled at once. Bevy's clustered forward renderer
/// draws every enabled light in a cluster it reaches, and the demo route has whole halls of `LIGH`
/// references, so the far ones are switched off rather than paid for.
pub const ENABLED_LIGHT_BUDGET: usize = 64;

/// How far the camera moves before the enabled lights are chosen again. Re-choosing is a sort of
/// every spawned light, so it is not done per frame.
pub const BUDGET_RECHOOSE_DISTANCE: f32 = 256.0;

/// The Bevy light a `LIGH` row becomes, or `None` for a record the engine must not light the world
/// with: a negative light (`LIGHT_FLAG_NEGATIVE`), one that is off by default
/// (`LIGHT_FLAG_OFF_BY_DEFAULT`), or one whose effective radius is not a positive, finite number of
/// Creation units.
///
/// `radius_override` is the reference's `XRDS` radius when it carries one; see [`radius_of`].
///
/// The colour is the record's RGB in the byte order the converter read it - `DATA`'s four colour
/// bytes as sRGB, the way the engine treats every other colour of the game.
pub fn point_light(light: &LightRow, radius_override: Option<f32>) -> Option<PointLight> {
    if light.flags & (LIGHT_FLAG_NEGATIVE | LIGHT_FLAG_OFF_BY_DEFAULT) != 0 {
        return None;
    }
    let radius = radius_of(light, radius_override)?;
    let intensity = if SKYRIM_FALLOFF {
        skyrim_intensity_for_radius(radius)
    } else {
        intensity_for_radius(radius)
    };
    Some(PointLight {
        color: Color::srgb_u8(light.color[0], light.color[1], light.color[2]),
        intensity,
        // Clear: Skyrim's falloff (`crate::light_falloff`); set: Bevy's own.
        affects_lightmapped_mesh_diffuse: !SKYRIM_FALLOFF,
        range: radius,
        shadow_maps_enabled: false,
        ..default()
    })
}

/// The radius a reference lights its space with.
///
/// `LIGH` records share their radius, and a reference places the same light at wildly different
/// sizes: 10,810 of the 12,148 `LIGH` references in `Skyrim.esm` carry an `XRDS` radius of their
/// own (228 of the 231 on the Alftand -> Blackreach route), and one Alftand01 `DefaultCandleLight01`
/// is 850.8 units where another is 147.7. The reference's override therefore wins over the record,
/// and the record's own radius is the fallback.
///
/// An override that is not a positive, finite number is not used: `XRDS` has been seen negative,
/// and a radius is a size, not a switch - the flags carry the light's on/off state - so a nonsense
/// override leaves the record's radius in place instead of leaving the room dark.
fn radius_of(light: &LightRow, radius_override: Option<f32>) -> Option<f32> {
    let usable = |radius: f32| (radius.is_finite() && radius > 0.0).then_some(radius);
    radius_override
        .and_then(usable)
        .or_else(|| usable(light.radius))
}

/// The intensity a `LIGH` radius is lit with; see the module documentation for the derivation.
///
/// The reference distance is the light's own half radius up to [`INTENSITY_REFERENCE_RADIUS`] and
/// that cap above it, so a big light keeps its reach without a brightness that grows with the
/// square of the radius it is only meant to reach. Every radius up to twice the cap is unchanged.
pub fn intensity_for_radius(radius: f32) -> f32 {
    let reference = (radius * 0.5).min(INTENSITY_REFERENCE_RADIUS);
    HALF_RADIUS_ILLUMINANCE * reference * reference / HALF_RADIUS_WINDOW
}

/// Whether converted lights are drawn with Skyrim's `1 - (d/r)^2` falloff ([`crate::light_falloff`])
/// instead of Bevy's inverse square.
///
/// Off: measured on the graded interior shots (2026-09-24) it did not help on its own (score 0.514
/// -> 0.532 on the fit half; with the fog weakened so the lights carry the rooms, 0.53-0.61),
/// because the interiors' fog, not their lights, is what fills them today. It does read clearer in
/// the Alftand halls. Turning it on belongs with the interior fog and ambient rework
/// (`local/reference/look-gaps/interior-lighting-design.md`).
pub const SKYRIM_FALLOFF: bool = false;

/// The intensity a light drawn with Skyrim's falloff (`crate::light_falloff`) needs to put the same
/// light as [`intensity_for_radius`] at the reference distance: that curve has no `1/d^2`, so the
/// intensity is the illuminance itself over Skyrim's attenuation there (0.75 at half the radius).
pub fn skyrim_intensity_for_radius(radius: f32) -> f32 {
    let reference = (radius * 0.5).min(INTENSITY_REFERENCE_RADIUS);
    let attenuation = crate::light_falloff::skyrim_attenuation(reference / radius);
    HALF_RADIUS_ILLUMINANCE / attenuation.max(1.0e-3)
}

/// A [`PointLight`] that came from a Skyrim `LIGH` reference.
///
/// The budget and the tests tell Skyrim's lights from the engine's own by this marker: a
/// `PointLight` without it is never enabled or budgeted by [`budget_lights`]. The camera lantern
/// it used to leave alone is gone; the marker now keeps the budget from touching a light a fixture
/// or a future engine feature adds.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkyrimLight {
    /// The reference that carries the light (`REFR` form id).
    pub form_id: u32,
    /// The cell that reference belongs to.
    pub cell_id: u32,
}

/// The lights the budget enabled last, and where it chose them.
#[derive(Resource, Default)]
struct LightBudget {
    /// The camera position the current selection was made at; `None` before the first frame.
    chosen_at: Option<Vec3>,
    /// Every light that could be seen when the current selection was made - the whole set the
    /// ranking ran over, not only the 64 it enabled.
    ///
    /// The selection is re-made whenever that set changes, even while the camera stands still and
    /// even when the number of lights in the world does not: a light streaming in or out, one
    /// spawning or despawning, and a cell changing role (a doorway opening onto a room, the doorway
    /// closed again, a door crossed into that room) all move lights in or out of it. A cache keyed
    /// on the *count* alone kept a room walked into and stopped in dark until the player moved
    /// another [`BUDGET_RECHOOSE_DISTANCE`], and left a doorway that stopped being drawn holding
    /// slots for the room behind it.
    chosen: HashSet<Entity>,
}

/// Keeps the number of enabled [`SkyrimLight`]s to [`ENABLED_LIGHT_BUDGET`] around the camera.
///
/// Add it after [`StreamingPlugin`](crate::streaming::StreamingPlugin).
pub struct LightsPlugin;

impl Plugin for LightsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LightBudget>()
            .add_systems(PostUpdate, budget_lights.after(TransformSystems::Propagate));
    }
}

/// Whether a light counts toward the budget at all: whether a view of the engine renders it.
///
/// A light's `RenderLayers` are where the portal's isolation writes which view its cell is for
/// (`crate::portal::isolate_cells`), and a light that belongs to no layer at all is drawn by no
/// camera: every descendant of a cell root the isolation hides - meshes and lights alike - is given
/// `RenderLayers::none()`. A light is such a descendant: `streaming::spawn_cell` puts it under its
/// reference entity, which is under the cell root, for exactly that reason. Its `LIGH` references
/// still spawn their lights, because they are part of a cell that is streamed in, so without this
/// rule a pre-streamed room behind a door the player is not looking through competes for the 64
/// slots with the room the player stands in.
///
/// A light with no `RenderLayers` component is not that case: no component is Bevy's layer 0, the
/// main camera's, which is what a light spawned in the active space carries. A light the portal has
/// put on the destination's layer is one too: the doorway is a view that draws it (see the module
/// documentation for why the destination's lights are counted).
fn counts_toward_budget(layers: Option<&RenderLayers>) -> bool {
    layers.is_none_or(|layers| layers.iter().next().is_some())
}

/// Enables the [`ENABLED_LIGHT_BUDGET`] lights nearest the camera among those a view renders, and
/// hides the rest.
///
/// Hidden is the switch: the lights are extracted to the render world only while they are visible
/// (`bevy_pbr/src/render/light.rs`), and a light set back to `Visibility::Inherited` still goes
/// dark whenever an ancestor is hidden - which is how a light of a cell the portal isolation hid
/// stays off without this system knowing about it.
///
/// This system owns the `Visibility` of a [`SkyrimLight`]. A light that another system hides and
/// this one re-enables - `streaming::hide_partial_scene`, which hides the descendants of a
/// reference whose model failed strict validation - comes back on: the budget cannot tell that
/// hiding from its own, and a light next to a missing model is not wrong. Hiding an *ancestor*
/// needs no such care, because `Visibility::Inherited` defers to it.
///
/// The lights no view draws are [`counts_toward_budget`]'s, and they are switched off along with
/// the ones past the budget: they are not this system's to turn on either way.
fn budget_lights(
    mut budget: ResMut<LightBudget>,
    camera: Query<&GlobalTransform, With<StreamingCamera>>,
    mut lights: Query<
        (
            Entity,
            &GlobalTransform,
            &mut Visibility,
            Option<&RenderLayers>,
        ),
        With<SkyrimLight>,
    >,
    seen: Query<(&ViewVisibility, &PointLight), With<SkyrimLight>>,
) {
    let Ok(camera) = camera.single() else {
        return;
    };
    let camera_position = camera.translation();
    // One pass over every spawned light, allocating nothing: how many of them a view renders, and
    // whether any of those is not in the set the current selection was made from.
    //
    // The second half is what the count alone cannot answer. A swap that keeps the number of lights
    // in the world the same is invisible to a count - a cell unloading as another streams in with
    // as many lights, a cell of a portal's destination going back to being hidden - and the player
    // standing still is the case the budget exists for: a room walked into and stopped in has to
    // light up, and a doorway that is drawn no longer must stop paying for the room behind it.
    let mut spawned = 0usize;
    let mut eligible = 0usize;
    let mut joined = false;
    for (entity, _, _, layers) in lights.iter() {
        spawned += 1;
        if counts_toward_budget(layers) {
            eligible += 1;
            joined |= !budget.chosen.contains(&entity);
        }
    }
    if !joined
        && let Some(chosen_at) = budget.chosen_at
        && budget.chosen.len() == eligible
        && chosen_at.distance(camera_position) <= BUDGET_RECHOOSE_DISTANCE
    {
        return;
    }

    let mut ranked: Vec<(Entity, f32)> = lights
        .iter()
        .filter(|(_, _, _, layers)| counts_toward_budget(*layers))
        .map(|(entity, transform, _, _)| {
            (
                entity,
                transform.translation().distance_squared(camera_position),
            )
        })
        .collect();
    // The entity breaks ties, so two lights at the same distance are chosen between the same way
    // every frame they are re-ranked in.
    ranked.sort_by(|(left_entity, left), (right_entity, right)| {
        left.total_cmp(right)
            .then_with(|| left_entity.cmp(right_entity))
    });
    let enabled: HashSet<Entity> = ranked
        .iter()
        .take(ENABLED_LIGHT_BUDGET)
        .map(|(entity, _)| *entity)
        .collect();

    for (entity, _, mut visibility, _) in &mut lights {
        let wanted = if enabled.contains(&entity) {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
    budget.chosen_at = Some(camera_position);
    budget.chosen = ranked.iter().map(|(entity, _)| *entity).collect();
    let visible = seen.iter().filter(|(view, _)| view.get()).count();
    if let Some((nearest, distance_squared)) = ranked.first() {
        let (range, intensity) = seen
            .get(*nearest)
            .map(|(_, light)| (light.range, light.intensity))
            .unwrap_or_default();
        debug!(
            spawned,
            eligible = ranked.len(),
            enabled = enabled.len(),
            visible_last_frame = visible,
            nearest_distance = distance_squared.sqrt(),
            nearest_range = range,
            nearest_intensity = intensity,
            "lights: budget re-chosen"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atmosphere::{INTERIOR_AMBIENT_BRIGHTNESS, INTERIOR_AMBIENT_LEVEL};
    use crate::world::components::CELL_SIZE;
    use bevy::{
        asset::AssetPlugin,
        camera::visibility::{RenderLayers, VisibilityPlugin, VisibilitySystems},
        transform::TransformPlugin,
    };

    fn light_row(radius: f32, flags: u32) -> LightRow {
        LightRow {
            radius,
            color: [255, 200, 120],
            flags,
            falloff: 1.0,
            fade: None,
        }
    }

    /// Bevy's own point light falloff, written out from the engine it runs in rather than from
    /// this module's derivation: `bevy_pbr/src/render/light.rs` divides a point light's intensity
    /// by `4*PI` (lumens to lumens per steradian) before it reaches the shader, and
    /// `Fd_Burley` in `bevy_pbr/src/render/pbr_lighting.wgsl` supplies the Lambert `1/PI`. The
    /// range window is `getDistanceAttenuation`'s.
    ///
    /// Both `PI`s belong here. This helper - and the derivation it restated - once carried only
    /// the Lambert one, which made the test pass while the lights delivered `4*PI` less than the
    /// constant they were aimed at.
    fn illuminance(light: &PointLight, distance: f32) -> f32 {
        let factor = distance * distance / (light.range * light.range);
        let window = (1.0 - factor * factor).max(0.0);
        light.intensity * window * window
            / (4.0 * core::f32::consts::PI * core::f32::consts::PI * distance * distance)
    }

    /// Relative luminance of a colour, the same Rec. 709 weights the pixels of a render are
    /// measured with: a light's colour scales its contribution linearly, so a warm light of a given
    /// intensity puts less luminance on a surface than a white one.
    fn luminance(color: Color) -> f32 {
        let linear = LinearRgba::from(color);
        0.2126 * linear.red + 0.7152 * linear.green + 0.0722 * linear.blue
    }

    /// The ambient of `app.rs` contributes `albedo * colour * brightness` to a surface; a converted
    /// light contributes what `illuminance` computes - already the light's colour times its
    /// intensity, since `illuminance` takes the colour out of the formula to keep the scale
    /// colour-free. A converted light has to put [`LIGHT_EXPOSURE`] times the ambient an interior
    /// applies on a surface at half its radius, and that is the whole point of the intensity scale -
    /// so this is the test that catches a wrong formula.
    ///
    /// The ambient an interior *applies* is [`INTERIOR_AMBIENT_BRIGHTNESS`] at
    /// [`INTERIOR_AMBIENT_LEVEL`], not the brightness alone. Taking the brightness was the engine's
    /// own mistake for as long as `LIGHT_EXPOSURE` was 50 against 800 rather than 480, so a test
    /// written against the same number agreed with the delivery it should have caught.
    ///
    /// Above [`INTENSITY_REFERENCE_RADIUS`] that stops being true on purpose: a light bigger than
    /// twice the cap is lit like the largest calibrated one, so what reaches a surface at half its
    /// own reach falls off with the square of its radius. The reach is what a big radius buys.
    #[test]
    fn a_light_lights_a_surface_at_half_its_radius_like_the_interior_ambient_does() {
        let wanted = LIGHT_EXPOSURE * INTERIOR_AMBIENT_BRIGHTNESS * INTERIOR_AMBIENT_LEVEL;
        for radius in [128.0, 512.0] {
            let light = point_light(&light_row(radius, 0), None).unwrap();
            let from_light = illuminance(&light, radius * 0.5);
            assert!(
                (from_light - wanted).abs() < wanted * 1.0e-3,
                "a {radius}-unit light gives {from_light} at half its radius; {LIGHT_EXPOSURE} \
                 times the interior ambient brightness of app.rs is {wanted}"
            );
        }
        // (512/radius)^2 of the calibrated 512-unit light, at that light's own half radius.
        let torch = illuminance(&point_light(&light_row(512.0, 0), None).unwrap(), 256.0);
        for radius in [1024.0, 4334.0] {
            let light = point_light(&light_row(radius, 0), None).unwrap();
            let from_light = illuminance(&light, radius * 0.5);
            let spread = (512.0 / radius).powi(2);
            assert!(
                (from_light - torch * spread).abs() < wanted * 1.0e-3,
                "a {radius}-unit light gives {from_light} at half its radius; a torch's own \
                 intensity spread over that reach gives {}",
                torch * spread
            );
        }
        assert!(
            (HALF_RADIUS_ILLUMINANCE
                - 4.0
                    * core::f32::consts::PI
                    * core::f32::consts::PI
                    * INTERIOR_AMBIENT_BRIGHTNESS
                    * INTERIOR_AMBIENT_LEVEL
                    * LIGHT_EXPOSURE)
                .abs()
                < 1.0,
            "the target illuminance is 4*PI^2 times the ambient an interior applies \
             (INTERIOR_AMBIENT_BRIGHTNESS * INTERIOR_AMBIENT_LEVEL) times LIGHT_EXPOSURE, got \
             {HALF_RADIUS_ILLUMINANCE}"
        );
    }

    /// The scale is per radius, so the surface brightness at half a light's radius does not depend
    /// on how big the record is: four times the radius is sixteen times the intensity.
    /// The light's own colour sits on the light's side of the comparison, exactly as the ambient's
    /// colour sits on its own: the intensity scale is colour-free, and a warm light of the same
    /// intensity puts less luminance on a surface than a white one of the same radius would.
    #[test]
    fn the_intensity_scale_ignores_the_lights_colour() {
        let warm = point_light(&light_row(512.0, 0), None).unwrap();
        let white = point_light(
            &LightRow {
                color: [255, 255, 255],
                ..light_row(512.0, 0)
            },
            None,
        )
        .unwrap();
        assert!(
            (warm.intensity - white.intensity).abs() < 1.0,
            "the scale is per radius, not per colour: {} and {}",
            warm.intensity,
            white.intensity
        );
        assert!(
            luminance(warm.color) < luminance(white.color),
            "and the warm light is the dimmer of the two on a surface"
        );
    }

    #[test]
    fn intensity_follows_the_square_of_the_radius() {
        let quarter = intensity_for_radius(256.0);
        let half = intensity_for_radius(512.0);
        assert!((half / quarter - 4.0).abs() < 1.0e-4, "{quarter} -> {half}");
        assert!(
            intensity_for_radius(512.0) > 1.0e7,
            "a metre-scale intensity would not reach across a 512-unit room: {}",
            intensity_for_radius(512.0)
        );
        // And it stops at the cap: twice the radius is no longer four times the intensity.
        assert_eq!(intensity_for_radius(1024.0), intensity_for_radius(512.0));
    }

    /// The cap has to be invisible to every light the fit was calibrated on: at or below
    /// `2 * INTENSITY_REFERENCE_RADIUS` the intensity is exactly the formula the calibration fitted,
    /// to the bit, so no torch, lamp or brazier of the demo or the calibrated renders moves. Above
    /// it the light keeps the intensity of the largest calibrated one.
    #[test]
    fn the_reference_cap_leaves_every_radius_up_to_512_exactly_as_it_was() {
        let uncapped = |radius: f32| {
            let half_radius = radius * 0.5;
            HALF_RADIUS_ILLUMINANCE * half_radius * half_radius / HALF_RADIUS_WINDOW
        };
        for radius in [0.5, 64.0, 128.0, 147.7, 256.0, 330.0, 511.0, 512.0] {
            assert_eq!(
                intensity_for_radius(radius),
                uncapped(radius),
                "a {radius}-unit light is byte-identical to the calibrated formula"
            );
        }
        assert_eq!(
            INTENSITY_REFERENCE_RADIUS * 2.0,
            512.0,
            "the cap is half of the widest calibrated radius, which is what makes the line above \
             cover every light that ever moved"
        );
        for radius in [512.5, 1024.0, 3300.0, 4334.0] {
            assert_eq!(
                intensity_for_radius(radius),
                intensity_for_radius(512.0),
                "a {radius}-unit light is lit like the largest calibrated one"
            );
            assert!(intensity_for_radius(radius) < uncapped(radius));
        }
    }

    /// The light the cap exists for. `000D9051` `FalmerCityLight02NS` is Blackreach's one huge
    /// light - radius 4334 at (2122,9014,4668), colour (216,128,39) - and with the intensity fitted
    /// to its own radius it came out 71 times a 512-unit torch, which lit the cavern ceiling orange
    /// in every reference-pose render and turned the dim surfaces of the frame orange with it
    /// (measured against the UESP Blackreach reference screenshots). It keeps its reach, and its
    /// colour is untouched: what it loses is the 71x.
    #[test]
    fn the_big_blackreach_light_is_one_torch_and_keeps_its_reach() {
        let falmer_city_light = LightRow {
            radius: 4334.0,
            color: [216, 128, 39],
            flags: 0,
            falloff: 1.0,
            fade: None,
        };
        let big = point_light(&falmer_city_light, None).unwrap();
        let torch = point_light(&light_row(512.0, 0), None).unwrap();
        assert_eq!(big.range, 4334.0, "the record's reach is unchanged");
        assert_eq!(
            big.intensity, torch.intensity,
            "and it is lit at a torch's intensity, not 71 of them"
        );
        assert!(
            big.range > torch.range * 8.0,
            "while still reaching eight times further than a torch: {}",
            big.range
        );
        assert_eq!(
            big.color,
            Color::srgb_u8(216, 128, 39),
            "its colour is the record's, orange as it is: the ceiling's hue was the intensity, not \
             the colour"
        );
    }

    /// [`EMISSIVE_EXPOSURE`] exists to take a converted glow from "a thousandth of the surfaces it
    /// sits among" to a glow - and it has to be neither of the two values that already failed.
    /// Unscaled is invisible, which is the gap this constant exists to close
    /// (`docs/research/visual-gaps-spec.md`, gap 1); at 1000 every frame with a glow in it clips
    /// (the Blackreach mushroom field measured 9 to 13 % of its pixels clipped, and the reference
    /// clips none). The band this is judged against is `crate::app::emissive_is_visible`, which is
    /// what validates the canonical material fixture, so the constant, the fixture and this test all
    /// agree on what a glow is.
    #[test]
    fn the_emissive_scale_lands_between_the_two_values_that_failed() {
        // The emissives the converter publishes for deliberate glows: the Blackreach mushroom caps
        // at 2.0 to 3.6, the `BlackreachSun01` orb at 3.0, a torch's flame card at 3.0.
        for converted in [2.0, 3.6] {
            let glow = LinearRgba::new(converted, converted, converted, 1.0);
            assert!(
                !crate::app::emissive_is_visible(glow),
                "a converted emissive of {converted} is the state this constant exists to fix"
            );
            assert!(
                crate::app::emissive_is_visible(crate::render::exposed_emissive(glow)),
                "and at {EMISSIVE_EXPOSURE} it is a glow of the size the engine lights its world in"
            );
        }
        // What those two assertions say about the constant itself: below 16 a converted glow is
        // still lost in the ambient, above 240 it is a lamp. The fitted value sits between them.
        assert!(
            (16.0..=240.0).contains(&EMISSIVE_EXPOSURE),
            "EMISSIVE_EXPOSURE has to be between the value that was invisible and the value that \
             clipped, got {EMISSIVE_EXPOSURE}"
        );
    }

    #[test]
    fn a_torch_becomes_a_range_coloured_unshadowed_light() {
        let light = point_light(&light_row(512.0, 0), None).unwrap();
        assert_eq!(light.range, 512.0);
        assert_eq!(light.color, Color::srgb_u8(255, 200, 120));
        assert!(!light.shadow_maps_enabled);
        assert_eq!(light.radius, 0.0, "no area, so no oversized specular");
        // A cell is 4096 Creation units across, about 58 metres: the range is in those units.
        assert!((CELL_SIZE / 512.0 - 8.0).abs() < 1.0e-3);
        // A metre-scale intensity would not reach across a 512-unit room.
        assert!(
            (light.intensity - intensity_for_radius(512.0)).abs() < 1.0,
            "{}",
            light.intensity
        );
    }

    #[test]
    fn negative_and_off_by_default_lights_spawn_nothing() {
        assert!(point_light(&light_row(512.0, LIGHT_FLAG_NEGATIVE), None).is_none());
        assert!(point_light(&light_row(512.0, LIGHT_FLAG_OFF_BY_DEFAULT), None).is_none());
        assert!(point_light(&light_row(512.0, 0x0001 | 0x0008), None).is_some());
        assert!(
            point_light(&light_row(0.0, 0), None).is_none(),
            "a light with no radius lights nothing"
        );
        assert!(point_light(&light_row(f32::NAN, 0), None).is_none());
        assert!(point_light(&light_row(-64.0, 0), None).is_none());
        assert!(
            point_light(&light_row(0.0, 0), Some(512.0)).is_some(),
            "an override rescues a record whose own radius is unusable"
        );
    }

    /// The reference's XRDS radius replaces the record's, and the intensity scale has to follow it:
    /// most route lights carry one (10,810 of the 12,148 `Skyrim.esm` references), and using the
    /// record default would light them at the wrong size.
    #[test]
    fn a_reference_radius_override_replaces_the_record_radius() {
        let record = light_row(256.0, 0);
        let overridden = point_light(&record, Some(850.8)).unwrap();
        assert_eq!(overridden.range, 850.8);
        assert!(
            (overridden.intensity / intensity_for_radius(850.8) - 1.0).abs() < 1.0e-5,
            "the intensity follows the radius that is used: {}",
            overridden.intensity
        );

        // A nonsense override is not a switch: the record's own radius stands.
        for override_radius in [-100.0, 0.0, f32::NAN, f32::INFINITY] {
            let light = point_light(&record, Some(override_radius)).unwrap();
            assert_eq!(light.range, 256.0, "override {override_radius}");
        }
    }

    /// A light budgeted around a camera, as `budget_lights` sees them.
    fn budget_app(camera: Vec3, light_positions: impl IntoIterator<Item = Vec3>) -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::mesh::MeshPlugin,
            TransformPlugin,
            VisibilityPlugin,
        ))
        .init_asset::<Mesh>()
        .init_resource::<LightBudget>()
        .add_systems(PostUpdate, budget_lights.after(TransformSystems::Propagate));
        app.world_mut().spawn((
            Transform::from_translation(camera),
            GlobalTransform::from_translation(camera),
            StreamingCamera,
        ));
        for position in light_positions {
            app.world_mut().spawn((
                SkyrimLight {
                    form_id: 1,
                    cell_id: 2,
                },
                // The real bundle: `PointLight` is what gives a light its `Transform`, its
                // `Visibility` and its `GlobalTransform`.
                point_light(&light_row(512.0, 0), None).unwrap(),
                Transform::from_translation(position),
            ));
        }
        app.update();
        app
    }

    fn enabled_lights(app: &mut App) -> Vec<Entity> {
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, (With<SkyrimLight>, With<Visibility>)>();
        query
            .iter(app.world())
            .filter(|entity| {
                *app.world().entity(*entity).get::<Visibility>().unwrap() == Visibility::Inherited
            })
            .collect()
    }

    fn move_camera(app: &mut App, camera: Vec3) {
        let mut query = app
            .world_mut()
            .query_filtered::<&mut Transform, With<StreamingCamera>>();
        for mut transform in query.iter_mut(app.world_mut()) {
            transform.translation = camera;
        }
        app.update();
    }

    fn visibility_of(app: &App, entity: Entity) -> Visibility {
        *app.world()
            .entity(entity)
            .get::<Visibility>()
            .expect("a light keeps its own `Visibility`")
    }

    /// One cell's lights, as `streaming::spawn_cell` builds them: a cell root with a light under it
    /// per position, spawned in the active space - the streaming side puts no `RenderLayers`
    /// anywhere, which is layer 0, the main camera's.
    fn spawn_cell(
        app: &mut App,
        cell_id: u32,
        positions: impl IntoIterator<Item = Vec3>,
    ) -> (Entity, Vec<Entity>) {
        let root = app
            .world_mut()
            .spawn((Transform::default(), Visibility::default()))
            .id();
        let lights = positions
            .into_iter()
            .map(|position| {
                app.world_mut()
                    .spawn((
                        SkyrimLight {
                            form_id: cell_id,
                            cell_id,
                        },
                        point_light(&light_row(512.0, 0), None).unwrap(),
                        Transform::from_translation(position),
                        ChildOf(root),
                    ))
                    .id()
            })
            .collect();
        (root, lights)
    }

    /// What a cell is to the frame's view: `crate::portal`'s three roles, as the portal's isolation
    /// leaves them on the entities of the cell.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Role {
        /// Part of the active space: drawn by the main camera.
        Active,
        /// The pre-streamed cell the portal renders through: drawn by the portal camera only.
        Destination,
        /// Any other resident cell: drawn by no camera.
        Hidden,
    }

    /// Moves a light's cell to another role, one frame of `crate::portal::isolate_cells`.
    ///
    /// The isolation writes a root's own `Visibility` and the role's `RenderLayers` on every one of
    /// its descendants - `CellRole::Hidden => RenderLayers::none()`, `CellRole::Destination =>
    /// RenderLayers::layer(DESTINATION_LAYER)`, and for the active space the layers the entity had
    /// before the isolation, which for a light is `RenderLayers::default()`. That the hidden half
    /// really takes a pre-streamed cell off every camera is
    /// `portal::tests::a_prestreamed_cell_is_off_the_main_camera_until_it_becomes_active`.
    fn set_role(app: &mut App, root: Entity, light: Entity, role: Role) {
        let (root_visibility, layers) = match role {
            Role::Active => (Visibility::default(), RenderLayers::default()),
            Role::Destination => (
                Visibility::default(),
                RenderLayers::layer(crate::portal::DESTINATION_LAYER),
            ),
            Role::Hidden => (Visibility::Hidden, RenderLayers::none()),
        };
        app.world_mut().entity_mut(root).insert(root_visibility);
        app.world_mut().entity_mut(light).insert(layers);
    }

    /// 100 lights on a line, the camera at the near end: exactly the 64 nearest stay on.
    #[test]
    fn the_budget_keeps_the_nearest_lights_enabled() {
        let positions: Vec<Vec3> = (0..100)
            .map(|index| Vec3::new(index as f32 * 1000.0, 0.0, 0.0))
            .collect();
        let mut app = budget_app(Vec3::ZERO, positions);

        let enabled = enabled_lights(&mut app);
        assert_eq!(enabled.len(), ENABLED_LIGHT_BUDGET);
        let hidden = 100 - enabled.len();
        assert_eq!(hidden, 36);
        // The 36 furthest are the ones switched off.
        let positions_of_hidden: Vec<f32> = {
            let mut query = app
                .world_mut()
                .query_filtered::<(Entity, &Transform), With<SkyrimLight>>();
            query
                .iter(app.world())
                .filter(|(entity, _)| !enabled.contains(entity))
                .map(|(_, transform)| transform.translation.x)
                .collect()
        };
        assert_eq!(positions_of_hidden.len(), hidden);
        assert!(
            positions_of_hidden.iter().all(|x| *x >= 64_000.0),
            "{positions_of_hidden:?}"
        );
    }

    /// A move shorter than [`BUDGET_RECHOOSE_DISTANCE`] leaves the selection alone, and one longer
    /// than it chooses again. The two lights just outside the budget sit on either side of the
    /// camera, so a re-choice swaps them: the pair makes the trigger observable in the visible
    /// state of a light, not only in the resource behind it.
    #[test]
    fn the_budget_rechooses_only_after_the_camera_has_moved() {
        let mut positions: Vec<Vec3> = (0..63)
            .map(|index| Vec3::new(1000.0 + index as f32, 0.0, 0.0))
            .collect();
        positions.push(Vec3::new(-10_000.0, 0.0, 0.0));
        positions.push(Vec3::new(10_001.0, 0.0, 0.0));
        let mut app = budget_app(Vec3::ZERO, positions);

        let visibility_at = |app: &mut App, x: f32| {
            let mut query = app
                .world_mut()
                .query_filtered::<(&Transform, &Visibility), With<SkyrimLight>>();
            query
                .iter(app.world())
                .find(|(transform, _)| (transform.translation.x - x).abs() < 0.5)
                .map(|(_, visibility)| *visibility)
                .expect("the light is still spawned")
        };

        assert_eq!(
            visibility_at(&mut app, -10_000.0),
            Visibility::Inherited,
            "the light behind the camera is the 64th"
        );
        assert_eq!(
            visibility_at(&mut app, 10_001.0),
            Visibility::Hidden,
            "and the one in front of it is 65th"
        );

        // A short move would swap them if the budget re-chose, so the selection must be unchanged.
        move_camera(&mut app, Vec3::new(100.0, 0.0, 0.0));
        assert_eq!(visibility_at(&mut app, -10_000.0), Visibility::Inherited);
        assert_eq!(visibility_at(&mut app, 10_001.0), Visibility::Hidden);

        // Past the threshold it chooses again, and now the light in front is nearer.
        move_camera(&mut app, Vec3::new(300.0, 0.0, 0.0));
        assert_eq!(visibility_at(&mut app, -10_000.0), Visibility::Hidden);
        assert_eq!(visibility_at(&mut app, 10_001.0), Visibility::Inherited);
    }

    /// A cell that streams in while the camera stands still has lights too, and the player walked
    /// into that room: the count changing is enough to choose again.
    #[test]
    fn streaming_lights_in_rechooses_without_camera_movement() {
        let mut app = budget_app(Vec3::ZERO, [Vec3::new(100.0, 0.0, 0.0)]);
        assert_eq!(enabled_lights(&mut app).len(), 1);

        let positions: Vec<Vec3> = (0..100)
            .map(|index| Vec3::new(index as f32 * 1000.0, 0.0, 0.0))
            .collect();
        for position in positions {
            app.world_mut().spawn((
                SkyrimLight {
                    form_id: 1,
                    cell_id: 3,
                },
                point_light(&light_row(512.0, 0), None).unwrap(),
                Transform::from_translation(position),
            ));
        }
        app.update();

        assert_eq!(enabled_lights(&mut app).len(), ENABLED_LIGHT_BUDGET);
    }

    /// A light of a cell no camera draws cannot be seen at all, so it must not take one of the 64
    /// slots from a light of the space the camera stands in.
    ///
    /// The hidden cell's light is the nearest of all 65 here: enabled, it is the first of the 64 and
    /// the active space's farthest light is switched off in its place.
    #[test]
    fn a_hidden_cells_light_does_not_take_a_slot_from_the_active_space() {
        let mut app = budget_app(Vec3::ZERO, []);
        let (_, active) = spawn_cell(
            &mut app,
            1,
            (0..ENABLED_LIGHT_BUDGET).map(|index| Vec3::new(1000.0 + index as f32, 0.0, 0.0)),
        );
        let (root, hidden) = spawn_cell(&mut app, 2, [Vec3::new(10.0, 0.0, 0.0)]);
        set_role(&mut app, root, hidden[0], Role::Hidden);
        app.update();

        assert_eq!(
            visibility_of(&app, hidden[0]),
            Visibility::Hidden,
            "a light of a cell no camera draws is off the budget"
        );
        for (index, light) in active.iter().enumerate() {
            assert_eq!(
                visibility_of(&app, *light),
                Visibility::Inherited,
                "light {index} of the {} in the active space was pushed out of the budget by a \
                 light nothing can see",
                active.len()
            );
        }
        assert_eq!(enabled_lights(&mut app).len(), ENABLED_LIGHT_BUDGET);
    }

    /// A cell unloads and another streams in within one frame with exactly as many lights between
    /// them, while the camera stands still: the total count is the same before and after, and the
    /// selection still has to move.
    ///
    /// The cell that leaves is the one whose 36 lights the budget had switched off, and the lights
    /// that arrive are all nearer the camera than anything else: a selection kept because the count
    /// did not change leaves all 100 spawned lights enabled.
    #[test]
    fn an_equal_count_swap_of_cells_re_chooses_without_camera_movement() {
        let mut app = budget_app(Vec3::ZERO, []);
        let (_, near) = spawn_cell(
            &mut app,
            1,
            (0..ENABLED_LIGHT_BUDGET).map(|index| Vec3::new(1000.0 + index as f32, 0.0, 0.0)),
        );
        // A second cell, all of whose lights are farther out: the ones the budget switches off.
        let (far_root, far) = spawn_cell(
            &mut app,
            2,
            (0..36).map(|index| Vec3::new(2000.0 + index as f32, 0.0, 0.0)),
        );
        app.update();
        assert_eq!(enabled_lights(&mut app).len(), ENABLED_LIGHT_BUDGET);
        for light in &far {
            assert_eq!(
                visibility_of(&app, *light),
                Visibility::Hidden,
                "the far cell's lights are the ones the budget switched off"
            );
        }

        // The far cell unloads - its root and its lights with it - and the cell that streams in has
        // the same 36 lights, all of them nearer than anything in the active space.
        app.world_mut().entity_mut(far_root).despawn();
        let (_, arriving) = spawn_cell(
            &mut app,
            3,
            (0..36).map(|index| Vec3::new(10.0 + index as f32, 0.0, 0.0)),
        );
        app.update();

        assert_eq!(
            enabled_lights(&mut app).len(),
            ENABLED_LIGHT_BUDGET,
            "the same count of lights is off the budget as before: the selection is the 64 \
             nearest, not the 64 that were nearest before the swap"
        );
        for light in &arriving {
            assert_eq!(
                visibility_of(&app, *light),
                Visibility::Inherited,
                "a light of the cell that just streamed in, nearer the camera than any other, is \
                 enabled"
            );
        }
        assert_eq!(
            near.iter()
                .filter(|light| visibility_of(&app, **light) == Visibility::Hidden)
                .count(),
            36,
            "and the 36 lights of the active space it displaced are off"
        );
    }

    /// A cell the portal stops drawing - the doorway closed, the player turned away, the door was
    /// crossed - goes back to being invisible, and the lights it was holding have to go back to the
    /// space the camera stands in. The camera does not move and no light enters or leaves the world:
    /// the only thing that changed is which of them can be seen.
    #[test]
    fn a_cell_leaving_the_portal_re_chooses_while_the_camera_stands_still() {
        let mut app = budget_app(Vec3::ZERO, []);
        let (standing_root, active) = spawn_cell(
            &mut app,
            1,
            (0..ENABLED_LIGHT_BUDGET).map(|index| Vec3::new(1000.0 + index as f32, 0.0, 0.0)),
        );
        // The room the doorway is showing: a view the player can see into, so its light is one of
        // the 64 - and the nearest of them all.
        let (root, cell) = spawn_cell(&mut app, 2, [Vec3::new(10.0, 0.0, 0.0)]);
        let destination = cell[0];
        set_role(&mut app, root, destination, Role::Destination);
        app.update();
        assert_eq!(
            visibility_of(&app, destination),
            Visibility::Inherited,
            "the doorway's light is enabled while the doorway is drawn"
        );
        assert_eq!(
            visibility_of(&app, active[ENABLED_LIGHT_BUDGET - 1]),
            Visibility::Hidden,
            "and the farthest light of the room around it is out of the budget for it"
        );

        // The portal renders through no doorway at all now: the cell is hidden again.
        set_role(&mut app, root, destination, Role::Hidden);
        app.update();

        assert_eq!(
            visibility_of(&app, destination),
            Visibility::Hidden,
            "a light of the cell the doorway was showing goes off the budget with it"
        );
        for (index, light) in active.iter().enumerate() {
            assert_eq!(
                visibility_of(&app, *light),
                Visibility::Inherited,
                "and light {index} of the active space has its slot back"
            );
        }
        assert_eq!(enabled_lights(&mut app).len(), ENABLED_LIGHT_BUDGET);

        // And then the player crosses into it: the room the doorway was showing becomes the space
        // the camera is in, and the room around the camera is left behind. Still no movement and no
        // light entering or leaving the world.
        for light in &active {
            set_role(&mut app, standing_root, *light, Role::Hidden);
        }
        set_role(&mut app, root, destination, Role::Active);
        app.update();

        assert_eq!(
            visibility_of(&app, destination),
            Visibility::Inherited,
            "the room crossed into is the active space, and its light is enabled"
        );
        for (index, light) in active.iter().enumerate() {
            assert_eq!(
                visibility_of(&app, *light),
                Visibility::Hidden,
                "and light {index} of the room left behind is off"
            );
        }
        assert_eq!(enabled_lights(&mut app).len(), 1);
    }

    /// The isolation the portal gives a pre-streamed cell reaches the light through the hierarchy:
    /// a light whose ancestor is hidden is invisible to the renderer, which is what
    /// `bevy_pbr::render::light::extract_lights` drops a light on. This pins the mechanism
    /// `crate::portal` relies on - if a Bevy update stops inheriting it, this fails.
    #[test]
    fn a_light_under_a_hidden_cell_root_lights_nothing() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::mesh::MeshPlugin,
            TransformPlugin,
            VisibilityPlugin,
        ))
        .init_asset::<Mesh>()
        .init_resource::<LightBudget>()
        .add_systems(
            PostUpdate,
            budget_lights
                .after(TransformSystems::Propagate)
                .after(VisibilitySystems::VisibilityPropagate),
        );
        let camera = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(5000.0, 0.0, 0.0)),
                Visibility::default(),
                StreamingCamera,
            ))
            .id();
        // A pre-streamed cell the portal isolation hid, with a light of its own next to the camera.
        let root = app
            .world_mut()
            .spawn((Transform::default(), Visibility::Hidden))
            .id();
        let light = app
            .world_mut()
            .spawn((
                SkyrimLight {
                    form_id: 1,
                    cell_id: 2,
                },
                point_light(&light_row(512.0, 0), None).unwrap(),
                Transform::default(),
                ChildOf(root),
            ))
            .id();
        // And one in the active space, to show the budget itself left both enabled.
        let active_light = app
            .world_mut()
            .spawn((
                SkyrimLight {
                    form_id: 3,
                    cell_id: 4,
                },
                point_light(&light_row(512.0, 0), None).unwrap(),
                Transform::default(),
                ChildOf(camera),
            ))
            .id();

        for _ in 0..2 {
            app.update();
        }

        assert_eq!(
            *app.world().entity(light).get::<Visibility>().unwrap(),
            Visibility::Inherited,
            "the budget does not switch the light off; the hidden cell does"
        );
        assert!(
            !app.world()
                .entity(light)
                .get::<InheritedVisibility>()
                .unwrap()
                .get(),
            "a light of a hidden cell is invisible, so it is never extracted"
        );
        assert!(
            app.world()
                .entity(active_light)
                .get::<InheritedVisibility>()
                .unwrap()
                .get(),
            "a light of the active space is lit"
        );
    }
}
