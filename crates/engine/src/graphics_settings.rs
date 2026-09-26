//! The demo's graphics settings: one resource that says how every camera renders, with presets,
//! command-line flags and a settings file (impl-219; the user's ask via the Fun Q&A session,
//! 2026-09-25, plan `local/research/fun/rendering-plan.md` step 1).
//!
//! [`GraphicsSettings`] is applied whenever it changes, and to every camera spawned after it:
//!
//! * the **main camera** ([`StreamingCamera`]) gets all of it;
//! * the **portal camera** ([`GraphicsCamera::Portal`]) gets every setting that is part of a mesh
//!   pipeline's view key - SSAO and its normal prepass, contact shadows, the shadow filter, and
//!   under TAA the motion-vector prepass and a (never moved) `TemporalJitter` - plus the fixed
//!   exposure. Its pipelines have to be the main view's, or every material the doorway shows is
//!   specialised again on the first open (impl-202's hitch). What it does *not* get is what the
//!   main camera already does to the whole frame, doorway included: the post-process
//!   anti-aliasing and TAA's resolve, bloom, auto exposure (a correction the main camera's
//!   tonemapping pass applies) and the tonemapper (it hands its image over untonemapped);
//! * the **water-reflection camera** ([`GraphicsCamera::WaterReflection`]) gets the exposure and
//!   the shadow filter only: no AA, no SSAO.
//!
//! The sun's shadow map size is the [`DirectionalLightShadowMap`] resource, its cascade count and
//! reach are written to the engine sun ([`MainSun`]) - the doorway's own sun follows the count
//! (`crate::portal`'s `place_destination_sun`, which runs after this in the same frame, so the
//! counts never differ at extraction: Bevy needs every shadowed directional light to share it,
//! impl-188) - and contact shadows are switched on every directional light.
//!
//! # Presets
//!
//! * `current`: exactly what the demo rendered before this module (no AA, no SSAO, Bevy's fixed
//!   EV100 9.7, TonyMcMapface, `Bloom::NATURAL`, a 2048 shadow map over four cascades fitted to
//!   the stream radius, Gaussian shadow filtering). The default.
//! * `bevy`: every built-in on at a sensible quality: SMAA (high), SSAO (high) at Bevy's own
//!   radius converted to Creation units, auto exposure, contact shadows. The auto exposure has
//!   Bevy's metering and speeds, but a compensation curve ([`exposure_compensation`]) that keeps
//!   the brightness near `current`'s: Bevy's flat default brings every scene to an average
//!   luminance of 1.0, overexposing the demo's calibrated lighting by about three stops (impl-231).
//!   `--exposure-target 0 --exposure-adaptation 1` is Bevy's own.
//! * `custom`: whatever the file and the flags set on top of `current`. A named preset that the
//!   file or the flags change is reported as `custom` too.
//!
//! # Input, in the order it is applied
//!
//! 1. the preset: `--graphics <name>`, else the file's `graphics` key, else `current`;
//! 2. the settings file: `--graphics-file <path.toml|path.json>`, else [`DEFAULT_FILE`] when it
//!    exists (not on a run that times frames, where a forgotten file would change the numbers);
//! 3. the per-knob flags ([`KNOBS`], e.g. `--aa smaa`, `--ssao medium`, `--exposure auto`);
//! 4. `--tonemapper` (`crate::tonemapper`), and `M` cycles it in the running demo.
//!
//! A file's keys are the flags' names without the dashes (`ssao_radius` or `ssao-radius`), one
//! `key = value` per line in the TOML file (flat: no tables) or one flat object in the JSON file.
//!
//! # SSAO's radius
//!
//! Bevy 0.19 has no radius on `ScreenSpaceAmbientOcclusion`: its shaders hard-code an effect
//! radius of 0.73 world units, which is 1 cm in a world whose unit is 1.43 cm, so SSAO would find
//! no occluders at all. The two lines are rewritten when the shaders load, the way
//! `crate::light_falloff` rewrites the point-light falloff ([`patch_ssao_shaders`]).

use std::path::{Path, PathBuf};

use bevy::{
    anti_alias::{
        fxaa::Fxaa,
        smaa::{Smaa, SmaaPreset},
        taa::TemporalAntiAliasing,
    },
    camera::Exposure,
    core_pipeline::{
        prepass::{MotionVectorPrepass, NormalPrepass},
        tonemapping::Tonemapping,
    },
    light::{CascadeShadowConfig, DirectionalLightShadowMap, ShadowFilteringMethod},
    math::cubic_splines::LinearSpline,
    pbr::{
        ContactShadows, ScreenSpaceAmbientOcclusion, ScreenSpaceAmbientOcclusionQualityLevel,
        ViewContactShadowsUniformOffset,
    },
    post_process::{
        auto_exposure::{AutoExposure, AutoExposureCompensationCurve},
        bloom::Bloom,
    },
    prelude::*,
    render::{
        Extract, ExtractSchedule, RenderApp,
        camera::{MipBias, TemporalJitter},
        sync_world::RenderEntity,
    },
    shader::Source,
};
use serde::{Serialize, Serializer};

use crate::{config::EngineConfig, demo_hud, world::components::StreamingCamera};

/// The settings file read at start when `--graphics-file` is not given and it exists.
pub const DEFAULT_FILE: &str = "local/graphics.toml";

/// Creation units in a metre (`crate::app`'s `CREATION_UNITS_PER_METRE`): Bevy's defaults are
/// metre-scale and are converted with this.
pub const UNITS_PER_METRE: f32 = 70.0;

/// Every per-knob flag, as `--<name> <value>`, and the file key it shares (dashes or
/// underscores).
pub const KNOBS: [&str; 19] = [
    "aa",
    "ssao",
    "ssao-radius",
    "ssao-thickness",
    "exposure",
    "exposure-min",
    "exposure-max",
    "exposure-speed",
    "exposure-speed-down",
    "exposure-target",
    "exposure-adaptation",
    "bloom",
    "bloom-intensity",
    "shadow-map-size",
    "shadow-cascades",
    "shadow-distance",
    "shadow-filter",
    "contact-shadows",
    "portal-scale",
];

/// Whether `name` (a flag without its `--`) is one of the [`KNOBS`].
pub fn is_knob(name: &str) -> bool {
    KNOBS.contains(&name)
}

/// The EV100 the demo has always rendered at: Bevy's default (`Exposure::EV100_BLENDER`), which
/// the lights and the ambient were calibrated against.
pub const CURRENT_EV100: f32 = Exposure::EV100_BLENDER;

/// Bevy's SSAO effect radius (`0.5 * 1.457` world units, `bevy_pbr-0.19.0/src/ssao/ssao.wgsl`)
/// read as metres and converted to Creation units.
pub const BEVY_SSAO_RADIUS: f32 = 0.5 * 1.457 * UNITS_PER_METRE;

/// Bevy's default `constant_object_thickness` (0.25) read as metres, in Creation units.
pub const BEVY_SSAO_THICKNESS: f32 = 0.25 * UNITS_PER_METRE;

/// The average log2 luminance the demo's auto exposure aims for ([`exposure_compensation`]),
/// see [`GraphicsSettings::exposure_target`]. Fitted by rendering (impl-231, `--shots` of
/// Riverwood's RW-02 street and RW-09 Sleeping Giant, checked on RW-07 and RW-10): at -2.5 a
/// daylight street keeps `current`'s mean brightness within a few percent.
pub const AUTO_EXPOSURE_TARGET: f32 = -2.5;

/// How much of a scene's distance from the target the demo's auto exposure takes out
/// ([`GraphicsSettings::exposure_adaptation`]). Fitted with the target: 0.5 and 0.3 lifted the
/// dim inn well over `current`; at 0.2 its mean display luminance is 1.27x `current`'s (the street
/// 0.99x; the holdouts RW-07 1.13x and RW-10 1.06x), against 3-7x with Bevy's flat default.
pub const AUTO_EXPOSURE_ADAPTATION: f32 = 0.2;

/// `Bloom::NATURAL`'s intensity: the demo's bloom since the glow work (`crate::app`).
pub const CURRENT_BLOOM_INTENSITY: f32 = 0.15;

/// The sun's shadow map size the demo has always used (`crate::app`'s `SUN_SHADOW_MAP_SIZE`).
pub const CURRENT_SHADOW_MAP_SIZE: usize = crate::app::SUN_SHADOW_MAP_SIZE;

/// The sun's cascade count the demo has always used (`crate::app`'s `SUN_SHADOW_CASCADES`).
pub const CURRENT_SHADOW_CASCADES: usize = crate::app::SUN_SHADOW_CASCADES;

/// Bevy 0.19's ceiling on a directional light's cascades (`MAX_CASCADES_PER_LIGHT`, native).
pub const MAX_SHADOW_CASCADES: usize = 4;

/// Which preset a [`GraphicsSettings`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Current,
    Bevy,
    Custom,
}

impl Preset {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name.trim().to_ascii_lowercase().as_str() {
            "current" => Ok(Self::Current),
            "bevy" => Ok(Self::Bevy),
            "custom" => Ok(Self::Custom),
            _ => Err(format!(
                "unknown graphics preset {name:?}; valid: current, bevy, custom"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Bevy => "bevy",
            Self::Custom => "custom",
        }
    }

    /// The preset's settings. `custom` starts from `current`.
    pub fn settings(self) -> GraphicsSettings {
        match self {
            Self::Current => GraphicsSettings::current(),
            Self::Bevy => GraphicsSettings::bevy(),
            Self::Custom => GraphicsSettings {
                preset: Self::Custom,
                ..GraphicsSettings::current()
            },
        }
    }
}

/// The anti-aliasing of the main camera. MSAA stays off: the portal and TAA need it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AntiAliasing {
    Off,
    Fxaa,
    Smaa,
    Taa,
}

/// SSAO's quality level, or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Ssao {
    Off,
    Low,
    Medium,
    High,
    Ultra,
}

impl Ssao {
    fn quality(self) -> Option<ScreenSpaceAmbientOcclusionQualityLevel> {
        match self {
            Self::Off => None,
            Self::Low => Some(ScreenSpaceAmbientOcclusionQualityLevel::Low),
            Self::Medium => Some(ScreenSpaceAmbientOcclusionQualityLevel::Medium),
            Self::High => Some(ScreenSpaceAmbientOcclusionQualityLevel::High),
            Self::Ultra => Some(ScreenSpaceAmbientOcclusionQualityLevel::Ultra),
        }
    }
}

/// A fixed exposure, or Bevy's histogram auto exposure on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExposureMode {
    Fixed,
    Auto,
}

/// How shadow-map edges are filtered (`ShadowFilteringMethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShadowFilter {
    Gaussian,
    Hardware2x2,
    Temporal,
}

impl ShadowFilter {
    fn method(self) -> ShadowFilteringMethod {
        match self {
            Self::Gaussian => ShadowFilteringMethod::Gaussian,
            Self::Hardware2x2 => ShadowFilteringMethod::Hardware2x2,
            Self::Temporal => ShadowFilteringMethod::Temporal,
        }
    }
}

/// How every camera renders. Distances are Creation units.
#[derive(Resource, Debug, Clone, PartialEq, Serialize)]
pub struct GraphicsSettings {
    pub preset: Preset,
    pub aa: AntiAliasing,
    pub ssao: Ssao,
    /// SSAO's effect radius ([`patch_ssao_shaders`]).
    pub ssao_radius: f32,
    /// SSAO's `constant_object_thickness`: how far behind a surface a ray passes behind it.
    pub ssao_thickness: f32,
    pub exposure: ExposureMode,
    /// The camera's fixed EV100; auto exposure corrects on top of it.
    pub ev100: f32,
    /// Auto exposure's metering range, in stops (`AutoExposure::range`): the scene luminance it
    /// can adapt to, so the correction stays within `-max..=-min` stops.
    pub exposure_min: f32,
    pub exposure_max: f32,
    /// Auto exposure's adaptation from dark to bright, stops a second.
    pub exposure_speed: f32,
    /// Auto exposure's adaptation from bright to dark, stops a second.
    pub exposure_speed_down: f32,
    /// The average log2 luminance auto exposure brings a scene towards, in stops
    /// ([`exposure_compensation`]). Bevy's own is 0 - an average luminance of 1.0 - which is about
    /// three stops brighter than the demo's calibrated lighting renders a daylight exterior.
    pub exposure_target: f32,
    /// How much of a scene's distance from [`Self::exposure_target`] auto exposure takes out, from
    /// 0 (none: it renders as the fixed exposure would) to 1 (all of it: Bevy's own behaviour, every
    /// scene the same average brightness).
    pub exposure_adaptation: f32,
    #[serde(serialize_with = "serialize_tonemapper")]
    pub tonemapper: Tonemapping,
    pub bloom: bool,
    pub bloom_intensity: f32,
    pub shadow_map_size: usize,
    pub shadow_cascades: usize,
    /// The sun's last cascade's reach; `None` is the reach fitted to the stream radius
    /// (`crate::app::sun_shadow_cascades`).
    pub shadow_distance: Option<f32>,
    pub shadow_filter: ShadowFilter,
    pub contact_shadows: bool,
    /// The portal's render target, as a fraction of the doorway's rectangle on screen.
    ///
    /// Recorded but **not applied yet**: the target is sized in `crate::portal`'s
    /// `resize_portal_target`, which impl-218 owned while this was written. The hook is
    /// [`scaled_portal_size`] around its `portal_target_size(rect.size(), ...)` argument.
    pub portal_scale: f32,
}

fn serialize_tonemapper<S: Serializer>(
    value: &Tonemapping,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&crate::tonemapper::name_of(*value))
}

impl Default for GraphicsSettings {
    fn default() -> Self {
        Self::current()
    }
}

impl GraphicsSettings {
    /// Today's look, exactly.
    pub fn current() -> Self {
        Self {
            preset: Preset::Current,
            aa: AntiAliasing::Off,
            ssao: Ssao::Off,
            ssao_radius: BEVY_SSAO_RADIUS,
            ssao_thickness: BEVY_SSAO_THICKNESS,
            exposure: ExposureMode::Fixed,
            ev100: CURRENT_EV100,
            exposure_min: -8.0,
            exposure_max: 8.0,
            exposure_speed: 3.0,
            exposure_speed_down: 1.0,
            exposure_target: AUTO_EXPOSURE_TARGET,
            exposure_adaptation: AUTO_EXPOSURE_ADAPTATION,
            tonemapper: crate::tonemapper::DEFAULT_TONEMAPPER,
            bloom: true,
            bloom_intensity: CURRENT_BLOOM_INTENSITY,
            shadow_map_size: CURRENT_SHADOW_MAP_SIZE,
            shadow_cascades: CURRENT_SHADOW_CASCADES,
            shadow_distance: None,
            shadow_filter: ShadowFilter::Gaussian,
            contact_shadows: false,
            portal_scale: 1.0,
        }
    }

    /// Bevy's built-ins on at a sensible quality.
    pub fn bevy() -> Self {
        Self {
            preset: Preset::Bevy,
            aa: AntiAliasing::Smaa,
            ssao: Ssao::High,
            exposure: ExposureMode::Auto,
            contact_shadows: true,
            ..Self::current()
        }
    }

    /// The same settings labelled `preset`, for comparing knobs only.
    fn labelled(&self, preset: Preset) -> Self {
        Self {
            preset,
            ..self.clone()
        }
    }

    /// Labels the settings with the named preset they equal, keeping `custom` when that was
    /// asked for, and `custom` for anything a file, flag or key changed.
    pub fn relabel(&mut self) {
        let base = self.preset;
        self.preset = if base != Preset::Custom && self.labelled(base) == base.settings() {
            base
        } else if base != Preset::Custom && self.labelled(Preset::Current) == Self::current() {
            Preset::Current
        } else if base != Preset::Custom && self.labelled(Preset::Bevy) == Self::bevy() {
            Preset::Bevy
        } else {
            Preset::Custom
        };
    }

    /// Sets one knob from its flag or file name (dashes or underscores, any case) and a value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let key = key.trim().to_ascii_lowercase().replace('_', "-");
        let value = value.trim();
        let bad = |what: &str| format!("--{key} {value:?}: {what}");
        match key.as_str() {
            "aa" => {
                self.aa = match value.to_ascii_lowercase().as_str() {
                    "off" | "none" => AntiAliasing::Off,
                    "fxaa" => AntiAliasing::Fxaa,
                    "smaa" => AntiAliasing::Smaa,
                    "taa" => AntiAliasing::Taa,
                    _ => return Err(bad("valid: off, fxaa, smaa, taa")),
                }
            }
            "ssao" => {
                self.ssao = match value.to_ascii_lowercase().as_str() {
                    "off" | "false" => Ssao::Off,
                    "low" => Ssao::Low,
                    "medium" => Ssao::Medium,
                    "high" | "on" | "true" => Ssao::High,
                    "ultra" => Ssao::Ultra,
                    _ => return Err(bad("valid: off, low, medium, high, ultra")),
                }
            }
            "ssao-radius" => self.ssao_radius = positive(value).map_err(|e| bad(&e))?,
            "ssao-thickness" => self.ssao_thickness = positive(value).map_err(|e| bad(&e))?,
            "exposure" => match value.to_ascii_lowercase().as_str() {
                "auto" => self.exposure = ExposureMode::Auto,
                "fixed" => self.exposure = ExposureMode::Fixed,
                number => {
                    self.ev100 = finite(number)
                        .map_err(|_| bad("valid: auto, fixed, or an EV100 number"))?;
                    self.exposure = ExposureMode::Fixed;
                }
            },
            "exposure-min" => self.exposure_min = finite(value).map_err(|e| bad(&e))?,
            "exposure-max" => self.exposure_max = finite(value).map_err(|e| bad(&e))?,
            "exposure-speed" => {
                let speed = positive(value).map_err(|e| bad(&e))?;
                self.exposure_speed = speed;
                self.exposure_speed_down = speed;
            }
            "exposure-speed-down" => {
                self.exposure_speed_down = positive(value).map_err(|e| bad(&e))?;
            }
            "exposure-target" => self.exposure_target = finite(value).map_err(|e| bad(&e))?,
            "exposure-adaptation" => {
                let adaptation = finite(value).map_err(|e| bad(&e))?;
                if !(0.0..=1.0).contains(&adaptation) {
                    return Err(bad("a fraction from 0 to 1"));
                }
                self.exposure_adaptation = adaptation;
            }
            "bloom" => self.bloom = switch(value).map_err(|e| bad(&e))?,
            "bloom-intensity" => {
                let intensity = finite(value).map_err(|e| bad(&e))?;
                if !(0.0..=1.0).contains(&intensity) {
                    return Err(bad("a number from 0 to 1"));
                }
                self.bloom_intensity = intensity;
            }
            "shadow-map-size" => {
                let size: usize = value.parse().map_err(|_| bad("a whole number"))?;
                if !size.is_power_of_two() || !(256..=8192).contains(&size) {
                    return Err(bad("a power of two from 256 to 8192"));
                }
                self.shadow_map_size = size;
            }
            "shadow-cascades" => {
                let count: usize = value.parse().map_err(|_| bad("a whole number"))?;
                if !(1..=MAX_SHADOW_CASCADES).contains(&count) {
                    return Err(bad("1 to 4 (Bevy 0.19's limit)"));
                }
                self.shadow_cascades = count;
            }
            "shadow-distance" => {
                self.shadow_distance = match value.to_ascii_lowercase().as_str() {
                    "auto" | "stream" => None,
                    number => Some(positive(number).map_err(|e| bad(&e))?),
                }
            }
            "shadow-filter" => {
                self.shadow_filter = match value.to_ascii_lowercase().as_str() {
                    "gaussian" => ShadowFilter::Gaussian,
                    "hardware2x2" | "hardware" => ShadowFilter::Hardware2x2,
                    "temporal" => ShadowFilter::Temporal,
                    _ => return Err(bad("valid: gaussian, hardware2x2, temporal")),
                }
            }
            "contact-shadows" => self.contact_shadows = switch(value).map_err(|e| bad(&e))?,
            "portal-scale" => {
                let scale = finite(value).map_err(|e| bad(&e))?;
                if !(0.1..=1.0).contains(&scale) {
                    return Err(bad("a fraction from 0.1 to 1"));
                }
                self.portal_scale = scale;
            }
            "tonemapper" => self.tonemapper = crate::tonemapper::parse(value)?,
            _ => {
                return Err(format!(
                    "unknown graphics setting {key:?}; valid: graphics, tonemapper, {}",
                    KNOBS.join(", ")
                ));
            }
        }
        if self.exposure_min >= self.exposure_max {
            return Err(format!(
                "exposure-min ({}) must be below exposure-max ({})",
                self.exposure_min, self.exposure_max
            ));
        }
        Ok(())
    }

    /// One line naming every setting, for logs.
    pub fn summary(&self) -> String {
        let exposure = match self.exposure {
            ExposureMode::Fixed => format!("fixed ev100={}", self.ev100),
            ExposureMode::Auto => format!(
                "auto ev100={} range={}..{} speed={}/{} target={} adaptation={}",
                self.ev100,
                self.exposure_min,
                self.exposure_max,
                self.exposure_speed,
                self.exposure_speed_down,
                self.exposure_target,
                self.exposure_adaptation
            ),
        };
        let ssao = match self.ssao {
            Ssao::Off => "off".to_owned(),
            level => format!(
                "{level:?} radius={} thickness={}",
                self.ssao_radius, self.ssao_thickness
            )
            .to_ascii_lowercase(),
        };
        let bloom = if self.bloom {
            format!("on({})", self.bloom_intensity)
        } else {
            "off".to_owned()
        };
        let distance = self
            .shadow_distance
            .map_or_else(|| "stream".to_owned(), |distance| distance.to_string());
        format!(
            "graphics: preset={} aa={:?} ssao={ssao} exposure={exposure} tonemapper={} \
             bloom={bloom} shadows={}px x{} to {distance} filter={:?} contact_shadows={} \
             portal_scale={}",
            self.preset.name(),
            self.aa,
            crate::tonemapper::name_of(self.tonemapper),
            self.shadow_map_size,
            self.shadow_cascades,
            self.shadow_filter,
            self.contact_shadows,
            self.portal_scale,
        )
        .replace("aa=Off", "aa=off")
        .replace("aa=Fxaa", "aa=fxaa")
        .replace("aa=Smaa", "aa=smaa")
        .replace("aa=Taa", "aa=taa")
    }
}

fn finite(value: &str) -> Result<f32, String> {
    value
        .parse::<f32>()
        .ok()
        .filter(|number| number.is_finite())
        .ok_or_else(|| "a number".to_owned())
}

fn positive(value: &str) -> Result<f32, String> {
    finite(value)
        .ok()
        .filter(|number| *number > 0.0)
        .ok_or_else(|| "a number above 0".to_owned())
}

fn switch(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err("on or off".to_owned()),
    }
}

/// The portal target's size at `scale` of the doorway's rectangle `size`, at least a pixel a side
/// (the hook for [`GraphicsSettings::portal_scale`]).
pub fn scaled_portal_size(size: Vec2, scale: f32) -> Vec2 {
    (size * scale.clamp(0.1, 1.0)).max(Vec2::ONE)
}

/// The exposure compensation, in stops, that Bevy's auto exposure adds at a metered average log2
/// luminance `lum`.
///
/// Bevy's auto exposure (`bevy_post_process-0.19.0/src/auto_exposure/auto_exposure.wgsl`) meters
/// the average log2 luminance `L` of the scene as rendered at the camera's fixed exposure and
/// corrects by `compensation(L) - L`, so the corrected average is `compensation(L)` itself. With
/// its default flat curve at 0 every scene is brought to an average luminance of 1.0: about three
/// stops over what the demo's lights are calibrated to at the fixed exposure (`lights.rs`
/// `EXPOSURE_CALIBRATION`, `render.rs` `EMISSIVE_EXPOSURE`), which is the `bevy` preset's
/// overexposure. This curve brings `L` only `adaptation` of the way to `target`: a scene at the
/// target is left as the fixed exposure renders it, and a darker or brighter one is lifted or
/// lowered by that fraction of its distance - still adapting, without flattening every place to
/// one brightness.
pub fn exposure_compensation(lum: f32, target: f32, adaptation: f32) -> f32 {
    target + (1.0 - adaptation.clamp(0.0, 1.0)) * (lum - target)
}

/// [`exposure_compensation`] as Bevy's compensation curve, over the metering range.
fn compensation_curve(settings: &GraphicsSettings) -> Option<AutoExposureCompensationCurve> {
    let point = |lum: f32| {
        Vec2::new(
            lum,
            exposure_compensation(lum, settings.exposure_target, settings.exposure_adaptation),
        )
    };
    AutoExposureCompensationCurve::from_curve(LinearSpline::new([
        point(settings.exposure_min),
        point(settings.exposure_max),
    ]))
    .ok()
}

/// A settings file's `key = value` pairs, in file order. `.json` is one flat object; anything
/// else is read as flat TOML: `key = value` lines, `#` comments, quoted or bare values, no tables.
pub fn parse_file(path: &Path, text: &str) -> Result<Vec<(String, String)>, String> {
    let is_json = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
    if is_json {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|error| format!("{}: {error}", path.display()))?;
        let serde_json::Value::Object(map) = value else {
            return Err(format!("{}: expected one flat JSON object", path.display()));
        };
        map.into_iter()
            .map(|(key, value)| match value {
                serde_json::Value::String(text) => Ok((key, text)),
                serde_json::Value::Number(number) => Ok((key, number.to_string())),
                serde_json::Value::Bool(flag) => Ok((key, flag.to_string())),
                other => Err(format!(
                    "{}: {key} is {other}; expected a string, number or boolean",
                    path.display()
                )),
            })
            .collect()
    } else {
        text.lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let line = strip_comment(line).trim();
                (!line.is_empty()).then_some((index + 1, line))
            })
            .map(|(number, line)| {
                if line.starts_with('[') {
                    return Err(format!(
                        "{}:{number}: tables are not supported; the file is flat key = value",
                        path.display()
                    ));
                }
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| format!("{}:{number}: expected key = value", path.display()))?;
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .or_else(|| {
                        value
                            .strip_prefix('\'')
                            .and_then(|value| value.strip_suffix('\''))
                    })
                    .unwrap_or(value);
                Ok((key.trim().to_owned(), value.to_owned()))
            })
            .collect()
    }
}

/// `line` without a `#` comment that is not inside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, character) in line.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), close) if open == close => quote = None,
            (None, '#') => return &line[..index],
            _ => {}
        }
    }
    line
}

/// The settings a run asked for: the preset, then the file's knobs, then the flags' knobs, then
/// the tonemapper flag, labelled with the preset they equal.
pub fn resolve(
    preset_flag: Option<&str>,
    file: Option<&[(String, String)]>,
    knobs: &[(String, String)],
    tonemapper: Option<&str>,
) -> Result<GraphicsSettings, String> {
    let is_preset_key = |key: &str| key.trim().eq_ignore_ascii_case("graphics");
    let file_preset = file.and_then(|pairs| {
        pairs
            .iter()
            .find(|(key, _)| is_preset_key(key))
            .map(|(_, value)| value.as_str())
    });
    let preset = preset_flag
        .or(file_preset)
        .map_or(Ok(Preset::Current), Preset::parse)?;
    let mut settings = preset.settings();
    for (key, value) in file.unwrap_or_default() {
        if !is_preset_key(key) {
            settings.set(key, value)?;
        }
    }
    for (key, value) in knobs {
        settings.set(key, value)?;
    }
    if let Some(name) = tonemapper {
        settings.tonemapper = crate::tonemapper::parse(name)?;
    }
    settings.relabel();
    Ok(settings)
}

/// The file a run reads: `--graphics-file`, else [`DEFAULT_FILE`] when it exists and the run does
/// not time frames.
pub fn settings_file(config: &EngineConfig) -> Option<PathBuf> {
    config.portal.graphics_file.clone().or_else(|| {
        let default = PathBuf::from(DEFAULT_FILE);
        (!config.times_frames() && default.is_file()).then_some(default)
    })
}

/// The run's settings from its command line and file, resolved before the window exists so a bad
/// value is fatal with a message rather than a silent default.
pub fn from_config(config: &EngineConfig) -> Result<GraphicsSettings, String> {
    let file = settings_file(config)
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            parse_file(&path, &text)
        })
        .transpose()?;
    resolve(
        config.portal.graphics.as_deref(),
        file.as_deref(),
        &config.portal.graphics_knobs,
        config.portal.tonemapper.as_deref(),
    )
}

/// A camera other than the main one that takes the settings it can use.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsCamera {
    /// The doorway's camera (`crate::portal`).
    Portal,
    /// The water's reflection camera (`crate::render`).
    WaterReflection,
}

/// The engine's sun (`crate::app::setup_world`), whose cascades the settings own.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct MainSun;

/// Applies [`GraphicsSettings`] to the cameras and lights, lets `M` cycle the tonemapper, and
/// records the settings in a `--shots` run's log.
pub struct GraphicsSettingsPlugin {
    pub settings: GraphicsSettings,
}

impl Plugin for GraphicsSettingsPlugin {
    fn build(&self, app: &mut App) {
        info!("{}", self.settings.summary());
        app.insert_resource(self.settings.clone())
            .insert_resource(DirectionalLightShadowMap {
                size: self.settings.shadow_map_size,
            })
            .init_resource::<demo_hud::Notices>()
            .add_systems(
                Update,
                (
                    crate::tonemapper::cycle_tonemapper,
                    apply_camera_settings,
                    apply_light_settings,
                    patch_ssao_shaders,
                    record_in_shots_log,
                )
                    .chain()
                    // Before the portal's frame, so the doorway's sun copies the engine sun's
                    // cascade count in the same frame it changes.
                    .before(crate::portal::PortalFrame),
            );
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.add_systems(ExtractSchedule, mark_contact_shadow_views);
        }
    }
}

/// Works round a Bevy 0.19 ordering gap that crashes a view the frame its contact shadows start.
///
/// The mesh pipelines' view key reads `Has<ViewContactShadowsUniformOffset>` in `PrepareAssets`
/// (`bevy_pbr-0.19.0/src/render/mesh.rs`, `check_views_need_specialization`), but the offset is
/// only inserted later, in `PrepareResources` (`src/contact_shadows.rs`), while the view's bind
/// group is built after that with it. So on a view's first frame with contact shadows its
/// pipelines are keyed without them and its bind group has them, and wgpu refuses the draw
/// ("Assigned entry with binding 16 not found in expected bind group layout") and Bevy quits. It
/// showed on the portal camera when a doorway opened under the `bevy` preset. A placeholder offset
/// from the extraction, which the real one then overwrites, gives the key the bit in time.
fn mark_contact_shadow_views(
    mut commands: Commands,
    cameras: Extract<Query<&RenderEntity, With<ContactShadows>>>,
) {
    for camera in &cameras {
        commands
            .entity(camera.id())
            .try_insert_if_new(ViewContactShadowsUniformOffset::default());
    }
}

/// Writes the settings to every camera when they change, and to a camera the frame it appears.
#[allow(clippy::too_many_arguments)]
fn apply_camera_settings(
    settings: Res<GraphicsSettings>,
    mut commands: Commands,
    main: Query<Entity, With<StreamingCamera>>,
    main_added: Query<Entity, Added<StreamingCamera>>,
    others: Query<(Entity, &GraphicsCamera)>,
    others_added: Query<(Entity, &GraphicsCamera), Added<GraphicsCamera>>,
    curves: Option<ResMut<Assets<AutoExposureCompensationCurve>>>,
    mut curve: Local<Option<([f32; 4], Handle<AutoExposureCompensationCurve>)>>,
) {
    let all = settings.is_changed();
    let main_cameras: Vec<Entity> = if all {
        main.iter().collect()
    } else {
        main_added.iter().collect()
    };
    // The auto exposure's compensation curve, built again only when the knobs it is made from
    // change. An app without Bevy's auto exposure plugin (a test) gets Bevy's flat default.
    let curve_handle = match curves {
        Some(mut curves) if settings.exposure == ExposureMode::Auto => {
            let knobs = [
                settings.exposure_min,
                settings.exposure_max,
                settings.exposure_target,
                settings.exposure_adaptation,
            ];
            if curve.as_ref().is_none_or(|(built, _)| *built != knobs) {
                let handle = match compensation_curve(&settings) {
                    Some(built) => curves.add(built),
                    None => {
                        error!("auto exposure's compensation curve could not be built");
                        Handle::default()
                    }
                };
                *curve = Some((knobs, handle));
            }
            curve
                .as_ref()
                .map(|(_, handle)| handle.clone())
                .unwrap_or_default()
        }
        _ => Handle::default(),
    };
    for entity in main_cameras {
        apply_main(&settings, &mut commands.entity(entity), &curve_handle);
    }
    let other_cameras: Vec<(Entity, GraphicsCamera)> = if all {
        others
            .iter()
            .map(|(entity, role)| (entity, *role))
            .collect()
    } else {
        others_added
            .iter()
            .map(|(entity, role)| (entity, *role))
            .collect()
    };
    for (entity, role) in other_cameras {
        let mut camera = commands.entity(entity);
        match role {
            GraphicsCamera::Portal => apply_portal(&settings, &mut camera),
            GraphicsCamera::WaterReflection => apply_water_reflection(&settings, &mut camera),
        }
    }
}

/// The main camera: everything.
fn apply_main(
    settings: &GraphicsSettings,
    camera: &mut EntityCommands,
    compensation_curve: &Handle<AutoExposureCompensationCurve>,
) {
    camera.insert((
        settings.tonemapper,
        Exposure {
            ev100: settings.ev100,
        },
        settings.shadow_filter.method(),
    ));
    if settings.bloom {
        camera.insert(Bloom {
            intensity: settings.bloom_intensity,
            ..Bloom::NATURAL
        });
    } else {
        camera.remove::<Bloom>();
    }
    camera.remove::<(Fxaa, Smaa)>();
    match settings.aa {
        AntiAliasing::Off => remove_taa(camera),
        AntiAliasing::Fxaa => {
            remove_taa(camera);
            camera.insert(Fxaa::default());
        }
        AntiAliasing::Smaa => {
            remove_taa(camera);
            camera.insert(Smaa {
                preset: SmaaPreset::High,
            });
        }
        AntiAliasing::Taa => {
            camera.insert(TemporalAntiAliasing::default());
        }
    }
    apply_shared(settings, camera);
    if settings.exposure == ExposureMode::Auto {
        camera.insert(AutoExposure {
            range: settings.exposure_min..=settings.exposure_max,
            speed_brighten: settings.exposure_speed,
            speed_darken: settings.exposure_speed_down,
            compensation_curve: compensation_curve.clone(),
            ..default()
        });
    } else {
        camera.remove::<AutoExposure>();
    }
}

/// The portal camera: what is in the mesh pipelines' view key, and the fixed exposure.
fn apply_portal(settings: &GraphicsSettings, camera: &mut EntityCommands) {
    camera.insert((
        Exposure {
            ev100: settings.ev100,
        },
        settings.shadow_filter.method(),
    ));
    if settings.aa == AntiAliasing::Taa {
        // The view key's TEMPORAL_JITTER and MOTION_VECTOR_PREPASS bits, without TAA's resolve:
        // the jitter only moves on a camera with `TemporalAntiAliasing`, so the doorway image
        // holds still and the main camera's TAA resolves it with the room around it.
        camera.insert((TemporalJitter::default(), MotionVectorPrepass));
    } else {
        camera.remove::<(TemporalJitter, MotionVectorPrepass)>();
    }
    apply_shared(settings, camera);
}

/// The water reflection: the exposure and the shadow filter.
fn apply_water_reflection(settings: &GraphicsSettings, camera: &mut EntityCommands) {
    camera.insert((
        Exposure {
            ev100: settings.ev100,
        },
        settings.shadow_filter.method(),
    ));
}

/// TAA and the components it requires, which removing it alone would leave behind.
fn remove_taa(camera: &mut EntityCommands) {
    camera.remove::<(
        TemporalAntiAliasing,
        TemporalJitter,
        MipBias,
        MotionVectorPrepass,
    )>();
}

/// SSAO and contact shadows, which the main and portal cameras share.
fn apply_shared(settings: &GraphicsSettings, camera: &mut EntityCommands) {
    match settings.ssao.quality() {
        Some(quality_level) => {
            camera.insert(ScreenSpaceAmbientOcclusion {
                quality_level,
                constant_object_thickness: settings.ssao_thickness,
            });
        }
        None => {
            camera.remove::<(ScreenSpaceAmbientOcclusion, NormalPrepass)>();
        }
    }
    if settings.contact_shadows {
        camera.insert(contact_shadows());
    } else {
        camera.remove::<ContactShadows>();
    }
}

/// Bevy's contact-shadow defaults, read as metres and converted to Creation units.
pub fn contact_shadows() -> ContactShadows {
    let bevy = ContactShadows::default();
    ContactShadows {
        linear_steps: bevy.linear_steps,
        thickness: bevy.thickness * UNITS_PER_METRE,
        length: bevy.length * UNITS_PER_METRE,
    }
}

/// The shadow map size, the engine sun's cascades and every directional light's contact shadows.
fn apply_light_settings(
    settings: Res<GraphicsSettings>,
    config: Option<Res<EngineConfig>>,
    mut shadow_map: ResMut<DirectionalLightShadowMap>,
    mut suns: Query<(
        &mut DirectionalLight,
        Option<&mut CascadeShadowConfig>,
        Ref<MainSun>,
    )>,
    mut lights: Query<&mut DirectionalLight, Without<MainSun>>,
) {
    if settings.is_changed() && shadow_map.size != settings.shadow_map_size {
        shadow_map.size = settings.shadow_map_size;
    }
    let stream_radius = config.map_or(EngineConfig::default().stream_radius, |config| {
        config.stream_radius
    });
    for (mut light, cascades, sun) in &mut suns {
        if light.contact_shadows_enabled != settings.contact_shadows {
            light.contact_shadows_enabled = settings.contact_shadows;
        }
        if (settings.is_changed() || sun.is_added())
            && let Some(mut cascades) = cascades
        {
            let wanted = crate::app::sun_shadow_cascades(
                stream_radius,
                settings.shadow_cascades,
                settings.shadow_distance,
            );
            if cascades.bounds != wanted.bounds
                || cascades.minimum_distance != wanted.minimum_distance
            {
                *cascades = wanted;
            }
        }
    }
    for mut light in &mut lights {
        if light.contact_shadows_enabled != settings.contact_shadows {
            light.contact_shadows_enabled = settings.contact_shadows;
        }
    }
}

/// Where Bevy 0.19 embeds the SSAO shaders whose radius is rewritten.
pub const SSAO_SHADER_PATH: &str = "embedded://bevy_pbr/ssao/ssao.wgsl";
pub const SSAO_DEPTH_SHADER_PATH: &str = "embedded://bevy_pbr/ssao/preprocess_depth.wgsl";

/// The radius lines of Bevy 0.19.0's `ssao.wgsl` (line 137) and `preprocess_depth.wgsl` (line
/// 32), and what they become at a radius.
pub const SSAO_RADIUS_LINE: &str = "let effect_radius = 0.5 * 1.457;";
pub const SSAO_DEPTH_RADIUS_LINE: &str =
    "let effect_radius = depth_range_scale_factor * 0.5 * 1.457;";

/// `source` (Bevy's own, unpatched) with its radius line set to `radius`, or `None` when neither
/// line is in it (a Bevy upgrade moved them).
pub fn patched_ssao_source(source: &str, radius: f32) -> Option<String> {
    let radius = format!("{radius:.4}");
    if source.contains(SSAO_DEPTH_RADIUS_LINE) {
        Some(source.replacen(
            SSAO_DEPTH_RADIUS_LINE,
            &format!(
                "let effect_radius = depth_range_scale_factor * {radius}; // OpenSkyrim: SSAO radius"
            ),
            1,
        ))
    } else if source.contains(SSAO_RADIUS_LINE) {
        Some(source.replacen(
            SSAO_RADIUS_LINE,
            &format!("let effect_radius = {radius}; // OpenSkyrim: SSAO radius"),
            1,
        ))
    } else {
        None
    }
}

/// The two SSAO shaders' own sources, kept so a new radius is written into Bevy's text rather
/// than into an already patched one, and the radius they carry now.
#[derive(Default)]
struct SsaoPatch {
    shaders: Vec<(Handle<Shader>, Option<String>)>,
    applied: Option<f32>,
    failed: bool,
}

/// Writes [`GraphicsSettings::ssao_radius`] into Bevy's SSAO shaders while SSAO is on. Changing
/// the asset in place recompiles only the SSAO pipelines. Nothing is touched while SSAO is off.
fn patch_ssao_shaders(
    settings: Res<GraphicsSettings>,
    asset_server: Option<Res<AssetServer>>,
    shaders: Option<ResMut<Assets<Shader>>>,
    mut patch: Local<SsaoPatch>,
) {
    let (Some(asset_server), Some(mut shaders)) = (asset_server, shaders) else {
        return;
    };
    if settings.ssao == Ssao::Off || patch.failed || patch.applied == Some(settings.ssao_radius) {
        return;
    }
    if patch.shaders.is_empty() {
        patch.shaders = [SSAO_SHADER_PATH, SSAO_DEPTH_SHADER_PATH]
            .map(|path| (asset_server.load(path), None))
            .into();
    }
    // Bevy's own text first, from both, before either is rewritten.
    for (handle, original) in &mut patch.shaders {
        if original.is_none() {
            let Some(shader) = shaders.get(&*handle) else {
                return;
            };
            let Source::Wgsl(source) = &shader.source else {
                error!("Bevy's SSAO shader is not WGSL; SSAO keeps its 0.73-unit radius");
                patch.failed = true;
                return;
            };
            *original = Some(source.to_string());
        }
    }
    let radius = settings.ssao_radius;
    let mut patched = Vec::new();
    for (handle, original) in &patch.shaders {
        let Some(text) = original
            .as_deref()
            .and_then(|original| patched_ssao_source(original, radius))
        else {
            error!(
                "Bevy's SSAO radius line was not found (a Bevy upgrade?); SSAO keeps its \
                 0.73-unit radius"
            );
            patch.failed = true;
            return;
        };
        patched.push((handle.clone(), text));
    }
    for (handle, text) in patched {
        if let Some(mut shader) = shaders.get_mut(&handle) {
            shader.source = Source::Wgsl(text.into());
        }
    }
    patch.applied = Some(radius);
    info!(
        radius,
        "SSAO radius set in Bevy's SSAO shaders (Creation units)"
    );
}

/// A `--shots` run's log opens with the settings it rendered with (and says so again if they
/// change).
fn record_in_shots_log(
    settings: Res<GraphicsSettings>,
    run: Option<ResMut<crate::shots::ShotsRun>>,
) {
    if let Some(mut run) = run
        && settings.is_changed()
    {
        run.note_line(settings.summary());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_current_preset_is_todays_look_and_bevy_turns_the_built_ins_on() {
        let current = GraphicsSettings::current();
        assert_eq!(current.preset, Preset::Current);
        assert_eq!(current.aa, AntiAliasing::Off);
        assert_eq!(current.ssao, Ssao::Off);
        assert_eq!(current.exposure, ExposureMode::Fixed);
        assert_eq!(current.ev100, Exposure::default().ev100);
        assert_eq!(current.tonemapper, Tonemapping::TonyMcMapface);
        assert!(current.bloom);
        assert_eq!(current.bloom_intensity, Bloom::NATURAL.intensity);
        assert_eq!(current.shadow_map_size, 2048);
        assert_eq!(current.shadow_cascades, 4);
        assert_eq!(current.shadow_distance, None);
        assert_eq!(current.shadow_filter, ShadowFilter::Gaussian);
        assert!(!current.contact_shadows);
        assert_eq!(current.portal_scale, 1.0);
        assert_eq!(GraphicsSettings::default(), current);

        let bevy = GraphicsSettings::bevy();
        assert_eq!(bevy.preset, Preset::Bevy);
        assert_eq!(bevy.aa, AntiAliasing::Smaa);
        assert_eq!(bevy.ssao, Ssao::High);
        assert_eq!(bevy.exposure, ExposureMode::Auto);
        assert!(bevy.contact_shadows && bevy.bloom);
        assert!(
            (bevy.ssao_radius - 51.0).abs() < 0.1,
            "{}",
            bevy.ssao_radius
        );
        assert_eq!(Preset::Custom.settings().labelled(Preset::Current), current);
    }

    #[test]
    fn flags_override_the_file_which_overrides_the_preset_and_changes_read_as_custom() {
        let settings = resolve(None, None, &[], None).unwrap();
        assert_eq!(settings, GraphicsSettings::current());

        let settings = resolve(Some("BEVY"), None, &[], None).unwrap();
        assert_eq!(settings, GraphicsSettings::bevy());

        let file = pairs(&[("graphics", "bevy"), ("aa", "fxaa"), ("ssao_radius", "80")]);
        let knobs = pairs(&[("aa", "taa"), ("exposure", "11.5")]);
        let settings = resolve(None, Some(&file), &knobs, Some("agx")).unwrap();
        assert_eq!(settings.preset, Preset::Custom);
        assert_eq!(
            settings.aa,
            AntiAliasing::Taa,
            "the flag wins over the file"
        );
        assert_eq!(settings.ssao, Ssao::High, "from the file's preset");
        assert_eq!(settings.ssao_radius, 80.0);
        assert_eq!(settings.exposure, ExposureMode::Fixed);
        assert_eq!(settings.ev100, 11.5);
        assert_eq!(settings.tonemapper, Tonemapping::AgX);

        let settings = resolve(Some("current"), Some(&file), &[], None).unwrap();
        assert_eq!(settings.aa, AntiAliasing::Fxaa, "the flag's preset wins");
        assert_eq!(settings.ssao, Ssao::Off);

        // A preset set knob by knob is that preset; `custom` stays custom.
        let knobs = pairs(&[
            ("aa", "smaa"),
            ("ssao", "high"),
            ("exposure", "auto"),
            ("contact-shadows", "on"),
        ]);
        assert_eq!(
            resolve(None, None, &knobs, None).unwrap().preset,
            Preset::Bevy
        );
        assert_eq!(
            resolve(Some("custom"), None, &[], None).unwrap().preset,
            Preset::Custom
        );
    }

    #[test]
    fn every_knob_parses_and_bad_values_are_refused_with_the_valid_ones() {
        let mut settings = GraphicsSettings::current();
        for (key, value) in [
            ("aa", "smaa"),
            ("ssao", "medium"),
            ("ssao-radius", "60"),
            ("ssao-thickness", "10"),
            ("exposure", "auto"),
            ("exposure-min", "-4"),
            ("exposure-max", "6"),
            ("exposure-speed", "2"),
            ("exposure-speed-down", "0.5"),
            ("bloom", "off"),
            ("bloom-intensity", "0.3"),
            ("shadow-map-size", "4096"),
            ("shadow-cascades", "3"),
            ("shadow-distance", "12000"),
            ("shadow-filter", "temporal"),
            ("contact-shadows", "on"),
            ("portal-scale", "0.5"),
        ] {
            assert!(is_knob(key), "{key}");
            settings
                .set(key, value)
                .unwrap_or_else(|error| panic!("{error}"));
        }
        assert_eq!(settings.aa, AntiAliasing::Smaa);
        assert_eq!(settings.ssao, Ssao::Medium);
        assert_eq!((settings.exposure_min, settings.exposure_max), (-4.0, 6.0));
        assert_eq!(
            (settings.exposure_speed, settings.exposure_speed_down),
            (2.0, 0.5)
        );
        assert!(!settings.bloom);
        assert_eq!(settings.shadow_map_size, 4096);
        assert_eq!(settings.shadow_cascades, 3);
        assert_eq!(settings.shadow_distance, Some(12000.0));
        assert_eq!(settings.shadow_filter, ShadowFilter::Temporal);
        assert!(settings.contact_shadows);
        assert_eq!(settings.portal_scale, 0.5);
        assert_eq!(KNOBS.len(), 19);

        for (key, value, says) in [
            ("aa", "msaa", "fxaa"),
            ("ssao", "extreme", "ultra"),
            ("shadow-cascades", "5", "1 to 4"),
            ("shadow-map-size", "3000", "power of two"),
            ("bloom", "maybe", "on or off"),
            ("portal-scale", "2", "0.1 to 1"),
            ("exposure-max", "-9", "below"),
            ("nope", "1", "unknown graphics setting"),
        ] {
            let error = GraphicsSettings::current().set(key, value).expect_err(key);
            assert!(error.contains(says), "{key}: {error}");
        }
        assert!(resolve(Some("ultra"), None, &[], None).is_err());
        assert!(resolve(None, None, &[], Some("filmic")).is_err());
    }

    #[test]
    fn the_flags_are_read_from_the_command_line() {
        let config = EngineConfig::from_args(
            [
                "--graphics",
                "bevy",
                "--aa",
                "taa",
                "--ssao-radius",
                "40",
                "--tonemapper",
                "AGX",
                "--graphics-file",
                "some/where.json",
            ]
            .map(str::to_owned),
        );
        assert_eq!(config.portal.graphics.as_deref(), Some("bevy"));
        assert_eq!(
            config.portal.graphics_file,
            Some(PathBuf::from("some/where.json"))
        );
        assert_eq!(
            config.portal.graphics_knobs,
            pairs(&[("aa", "taa"), ("ssao-radius", "40")])
        );
        let settings = resolve(
            config.portal.graphics.as_deref(),
            None,
            &config.portal.graphics_knobs,
            config.portal.tonemapper.as_deref(),
        )
        .unwrap();
        assert_eq!(settings.aa, AntiAliasing::Taa);
        assert_eq!(settings.ssao_radius, 40.0);
        assert_eq!(settings.tonemapper, Tonemapping::AgX);
        assert_eq!(settings.preset, Preset::Custom);

        // No flags: today's look, and no file unless one exists.
        let config = EngineConfig::from_args(Vec::<String>::new());
        assert!(config.portal.graphics_knobs.is_empty());
        let config = EngineConfig::from_args(["--tonemapper", "nope"].map(str::to_owned));
        assert!(
            resolve(None, None, &[], config.portal.tonemapper.as_deref()).is_err(),
            "a bad --tonemapper is still refused"
        );
    }

    #[test]
    fn a_settings_file_reads_as_flat_toml_or_json() {
        let toml = "# the demo's graphics\n\
                    graphics = \"bevy\"   # start here\n\
                    aa = 'taa'\n\
                    \n\
                    ssao_radius = 64\n\
                    contact-shadows = false\n";
        let pairs_read = parse_file(Path::new("local/graphics.toml"), toml).unwrap();
        assert_eq!(
            pairs_read,
            pairs(&[
                ("graphics", "bevy"),
                ("aa", "taa"),
                ("ssao_radius", "64"),
                ("contact-shadows", "false"),
            ])
        );
        let settings = resolve(None, Some(&pairs_read), &[], None).unwrap();
        assert_eq!(settings.aa, AntiAliasing::Taa);
        assert_eq!(settings.ssao, Ssao::High);
        assert_eq!(settings.ssao_radius, 64.0);
        assert!(!settings.contact_shadows);

        let json =
            r#"{"graphics": "current", "ssao": "low", "bloom": false, "shadow_cascades": 2}"#;
        let pairs_read = parse_file(Path::new("x.JSON"), json).unwrap();
        let settings = resolve(None, Some(&pairs_read), &[], None).unwrap();
        assert_eq!(settings.ssao, Ssao::Low);
        assert!(!settings.bloom);
        assert_eq!(settings.shadow_cascades, 2);

        assert!(parse_file(Path::new("a.toml"), "[graphics]\naa = 'taa'").is_err());
        assert!(parse_file(Path::new("a.toml"), "aa taa").is_err());
        assert!(parse_file(Path::new("a.json"), "[1, 2]").is_err());
        assert!(parse_file(Path::new("a.json"), r#"{"aa": [1]}"#).is_err());
    }

    #[test]
    fn the_settings_serialize_for_the_field_notes_and_summarise_for_the_shots_log() {
        let value = serde_json::to_value(GraphicsSettings::bevy()).unwrap();
        assert_eq!(value["preset"], "bevy");
        assert_eq!(value["aa"], "smaa");
        assert_eq!(value["ssao"], "high");
        assert_eq!(value["exposure"], "auto");
        assert_eq!(value["tonemapper"], "TonyMcMapface");
        assert_eq!(value["shadow_distance"], serde_json::Value::Null);
        let summary = GraphicsSettings::current().summary();
        assert!(
            summary.starts_with("graphics: preset=current aa=off ssao=off"),
            "{summary}"
        );
        assert!(summary.contains("tonemapper=TonyMcMapface"), "{summary}");
        assert!(summary.contains("bloom=on(0.15)"), "{summary}");
    }

    #[test]
    fn the_ssao_radius_lines_are_in_bevys_shaders() {
        let registry = std::env::var("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("USERPROFILE").map(|home| Path::new(&home).join(".cargo")))
            .unwrap()
            .join("registry")
            .join("src");
        let read = |file: &str| {
            std::fs::read_dir(&registry).ok().and_then(|indices| {
                indices.filter_map(Result::ok).find_map(|index| {
                    std::fs::read_to_string(
                        index.path().join("bevy_pbr-0.19.0/src/ssao").join(file),
                    )
                    .ok()
                })
            })
        };
        let (Some(ssao), Some(depth)) = (read("ssao.wgsl"), read("preprocess_depth.wgsl")) else {
            eprintln!("bevy_pbr 0.19.0's source is not in the cargo registry; skipped");
            return;
        };
        let patched = patched_ssao_source(&ssao, 51.0).expect("the radius line is in ssao.wgsl");
        assert!(
            patched.contains("let effect_radius = 51.0000;"),
            "{patched}"
        );
        assert!(!patched.contains(SSAO_RADIUS_LINE));
        let patched = patched_ssao_source(&depth, 51.0).expect("the line is in preprocess_depth");
        assert!(patched.contains("depth_range_scale_factor * 51.0000;"));
        assert!(!patched.contains(SSAO_DEPTH_RADIUS_LINE));
        assert!(
            patched_ssao_source(&patched, 60.0).is_none(),
            "patched from Bevy's text only"
        );
    }

    /// An app with the plugin, a main camera spawned the way `app::setup_world` spawns it, the
    /// portal camera's and the water camera's markers, and the engine sun.
    fn app_with(settings: GraphicsSettings) -> (App, Entity, Entity, Entity, Entity) {
        let mut app = App::new();
        app.init_resource::<ButtonInput<KeyCode>>()
            .add_plugins(GraphicsSettingsPlugin { settings });
        let main = app
            .world_mut()
            .spawn((StreamingCamera, Msaa::Off, bevy::camera::Hdr))
            .id();
        let portal = app
            .world_mut()
            .spawn((GraphicsCamera::Portal, Tonemapping::None, Msaa::Off))
            .id();
        let water = app.world_mut().spawn(GraphicsCamera::WaterReflection).id();
        let sun = app
            .world_mut()
            .spawn((
                DirectionalLight::default(),
                crate::app::sun_shadow_cascades(
                    EngineConfig::default().stream_radius,
                    CURRENT_SHADOW_CASCADES,
                    None,
                ),
                MainSun,
            ))
            .id();
        app.update();
        (app, main, portal, water, sun)
    }

    #[test]
    fn current_gives_the_cameras_exactly_todays_components() {
        let (app, main, portal, water, sun) = app_with(GraphicsSettings::current());
        let world = app.world();
        let camera = world.entity(main);
        assert_eq!(
            camera.get::<Tonemapping>(),
            Some(&Tonemapping::TonyMcMapface)
        );
        assert_eq!(
            camera.get::<Exposure>().map(|e| e.ev100),
            Some(Exposure::default().ev100)
        );
        assert_eq!(
            camera.get::<ShadowFilteringMethod>(),
            Some(&ShadowFilteringMethod::default())
        );
        let bloom = camera.get::<Bloom>().expect("Bloom::NATURAL");
        let natural = Bloom::NATURAL;
        assert_eq!(bloom.intensity, natural.intensity);
        assert_eq!(bloom.low_frequency_boost, natural.low_frequency_boost);
        assert_eq!(
            bloom.low_frequency_boost_curvature,
            natural.low_frequency_boost_curvature
        );
        assert_eq!(bloom.high_pass_frequency, natural.high_pass_frequency);
        assert_eq!(bloom.composite_mode, natural.composite_mode);
        assert_eq!(bloom.max_mip_dimension, natural.max_mip_dimension);
        assert_eq!(bloom.scale, natural.scale);
        assert_eq!(camera.get::<Msaa>(), Some(&Msaa::Off));
        assert!(!camera.contains::<Fxaa>());
        assert!(!camera.contains::<Smaa>());
        assert!(!camera.contains::<TemporalAntiAliasing>());
        assert!(!camera.contains::<TemporalJitter>());
        assert!(!camera.contains::<MotionVectorPrepass>());
        assert!(!camera.contains::<ScreenSpaceAmbientOcclusion>());
        assert!(!camera.contains::<NormalPrepass>());
        assert!(!camera.contains::<AutoExposure>());
        assert!(!camera.contains::<ContactShadows>());

        let portal = world.entity(portal);
        assert_eq!(portal.get::<Tonemapping>(), Some(&Tonemapping::None));
        assert!(!portal.contains::<Bloom>());
        assert!(!portal.contains::<ScreenSpaceAmbientOcclusion>());
        assert!(!portal.contains::<TemporalJitter>());
        assert!(!portal.contains::<ContactShadows>());
        assert_eq!(
            portal.get::<Exposure>().map(|e| e.ev100),
            Some(CURRENT_EV100)
        );
        let water = world.entity(water);
        assert!(
            !water.contains::<Msaa>(),
            "the water camera keeps Bevy's own MSAA"
        );
        assert_eq!(
            water.get::<ShadowFilteringMethod>(),
            Some(&ShadowFilteringMethod::Gaussian)
        );

        assert_eq!(world.resource::<DirectionalLightShadowMap>().size, 2048);
        let sun = world.entity(sun);
        let bounds = &sun.get::<CascadeShadowConfig>().unwrap().bounds;
        assert_eq!(bounds.len(), 4);
        let today = crate::app::sun_shadow_cascades(EngineConfig::default().stream_radius, 4, None);
        assert_eq!(bounds, &today.bounds);
        assert!(
            !sun.get::<DirectionalLight>()
                .unwrap()
                .contact_shadows_enabled
        );
    }

    #[test]
    fn bevy_turns_the_built_ins_on_and_the_portal_shares_the_view_key() {
        let mut settings = GraphicsSettings::bevy();
        settings.aa = AntiAliasing::Taa;
        let (mut app, main, portal, water, sun) = app_with(settings);
        {
            let world = app.world();
            let camera = world.entity(main);
            assert!(camera.contains::<TemporalAntiAliasing>());
            assert!(camera.contains::<MotionVectorPrepass>());
            assert!(camera.contains::<ScreenSpaceAmbientOcclusion>());
            assert!(camera.contains::<NormalPrepass>());
            assert!(camera.contains::<AutoExposure>());
            assert!(camera.contains::<ContactShadows>());
            let portal = world.entity(portal);
            for (name, has) in [
                ("ssao", portal.contains::<ScreenSpaceAmbientOcclusion>()),
                ("normal prepass", portal.contains::<NormalPrepass>()),
                ("motion vectors", portal.contains::<MotionVectorPrepass>()),
                ("jitter", portal.contains::<TemporalJitter>()),
                ("contact shadows", portal.contains::<ContactShadows>()),
            ] {
                assert!(has, "the portal camera has the main view's {name}");
            }
            for (name, has) in [
                ("taa resolve", portal.contains::<TemporalAntiAliasing>()),
                ("auto exposure", portal.contains::<AutoExposure>()),
                ("bloom", portal.contains::<Bloom>()),
            ] {
                assert!(!has, "the portal camera does not run its own {name}");
            }
            assert_eq!(portal.get::<Tonemapping>(), Some(&Tonemapping::None));
            let water = world.entity(water);
            assert!(!water.contains::<ScreenSpaceAmbientOcclusion>());
            assert!(!water.contains::<TemporalAntiAliasing>());
            assert!(
                world
                    .entity(sun)
                    .get::<DirectionalLight>()
                    .unwrap()
                    .contact_shadows_enabled
            );
        }

        // Back to today's look, live: everything the built-ins brought goes again.
        *app.world_mut().resource_mut::<GraphicsSettings>() = GraphicsSettings {
            shadow_cascades: 2,
            shadow_map_size: 1024,
            ..GraphicsSettings::current()
        };
        app.update();
        let world = app.world();
        for entity in [main, portal] {
            let camera = world.entity(entity);
            assert!(!camera.contains::<TemporalAntiAliasing>());
            assert!(!camera.contains::<TemporalJitter>());
            assert!(!camera.contains::<MotionVectorPrepass>());
            assert!(!camera.contains::<ScreenSpaceAmbientOcclusion>());
            assert!(!camera.contains::<NormalPrepass>());
            assert!(!camera.contains::<AutoExposure>());
            assert!(!camera.contains::<ContactShadows>());
        }
        assert!(world.entity(main).contains::<Bloom>());
        assert_eq!(world.resource::<DirectionalLightShadowMap>().size, 1024);
        assert_eq!(
            world
                .entity(sun)
                .get::<CascadeShadowConfig>()
                .unwrap()
                .bounds
                .len(),
            2
        );
    }

    #[test]
    fn the_key_cycles_the_main_cameras_tonemapper_only_and_names_it() {
        let settings = GraphicsSettings {
            tonemapper: Tonemapping::KhronosPbrNeutral,
            ..GraphicsSettings::current()
        };
        let (mut app, main, portal, _, _) = app_with(settings);
        assert_eq!(
            app.world().get::<Tonemapping>(main),
            Some(&Tonemapping::KhronosPbrNeutral),
            "the flag's tonemapper is applied from the first frame"
        );
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(crate::tonemapper::CYCLE_KEY);
        app.update();
        assert_eq!(
            app.world().get::<Tonemapping>(main),
            Some(&Tonemapping::AcesFitted)
        );
        assert_eq!(
            app.world().resource::<demo_hud::Notices>().text(),
            Some("Tonemapper: AcesFitted")
        );
        assert_eq!(
            app.world().resource::<GraphicsSettings>().preset,
            Preset::Custom
        );
        assert_eq!(
            app.world().get::<Tonemapping>(portal),
            Some(&Tonemapping::None),
            "the portal camera keeps its own"
        );
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.update();
        assert_eq!(
            app.world().get::<Tonemapping>(main),
            Some(&Tonemapping::AcesFitted)
        );
    }

    #[test]
    fn a_later_camera_is_caught_on_its_first_frame() {
        let (mut app, ..) = app_with(GraphicsSettings::bevy());
        let late = app.world_mut().spawn((StreamingCamera, Msaa::Off)).id();
        app.update();
        assert!(app.world().entity(late).contains::<Smaa>());
        assert!(
            app.world()
                .entity(late)
                .contains::<ScreenSpaceAmbientOcclusion>()
        );
    }

    /// Auto exposure's corrected average is the compensation itself: a scene metered at the target
    /// is left alone, others are moved the adaptation's fraction of the way to it, and Bevy's own
    /// behaviour (target 0, adaptation 1) is still one setting away.
    #[test]
    fn auto_exposure_moves_a_scene_part_of_the_way_to_the_calibrated_target() {
        let target = AUTO_EXPOSURE_TARGET;
        let adaptation = AUTO_EXPOSURE_ADAPTATION;
        assert_eq!(exposure_compensation(target, target, adaptation), target);
        let dark = target - 4.0;
        let lifted = exposure_compensation(dark, target, adaptation);
        assert!(
            lifted > dark && lifted < target,
            "a dark scene is lifted towards the target, not to it: {lifted}"
        );
        assert!((lifted - (dark + 4.0 * adaptation)).abs() < 1e-5);
        assert_eq!(exposure_compensation(dark, 0.0, 1.0), 0.0, "Bevy's own");
        assert_eq!(
            exposure_compensation(dark, target, 0.0),
            dark,
            "none: the fixed exposure"
        );

        let settings = GraphicsSettings::bevy();
        assert_eq!(settings.exposure_target, AUTO_EXPOSURE_TARGET);
        assert_eq!(settings.exposure_adaptation, AUTO_EXPOSURE_ADAPTATION);
        assert!(compensation_curve(&settings).is_some());
        let mut settings = GraphicsSettings::current();
        settings.set("exposure-target", "0").unwrap();
        settings.set("exposure-adaptation", "1").unwrap();
        assert_eq!(
            (settings.exposure_target, settings.exposure_adaptation),
            (0.0, 1.0)
        );
        assert!(settings.set("exposure-adaptation", "1.5").is_err());
    }

    /// The main camera's auto exposure carries the calibrated curve, not Bevy's flat default.
    #[test]
    fn the_main_camera_s_auto_exposure_carries_the_compensation_curve() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::asset::AssetPlugin::default()))
            .init_asset::<AutoExposureCompensationCurve>()
            .init_resource::<ButtonInput<KeyCode>>()
            .add_plugins(GraphicsSettingsPlugin {
                settings: GraphicsSettings::bevy(),
            });
        let main = app
            .world_mut()
            .spawn((StreamingCamera, Msaa::Off, bevy::camera::Hdr))
            .id();
        app.update();
        let handle = app
            .world()
            .get::<AutoExposure>(main)
            .expect("auto exposure on the bevy preset")
            .compensation_curve
            .clone();
        assert_ne!(handle, Handle::default());
        assert!(
            app.world()
                .resource::<Assets<AutoExposureCompensationCurve>>()
                .get(&handle)
                .is_some()
        );
    }

    #[test]
    fn the_portal_hook_scales_the_doorway_rectangle() {
        assert_eq!(
            scaled_portal_size(Vec2::new(800.0, 600.0), 0.5),
            Vec2::new(400.0, 300.0)
        );
        assert_eq!(scaled_portal_size(Vec2::new(4.0, 4.0), 0.1), Vec2::ONE);
    }
}
