//! Skyrim's animated shader values: the scrolling texture of a hearth's flames, a glow's pulse.
//!
//! A NIF animates a shader property's float values with a `BSEffectShaderPropertyFloatController`
//! (or its lighting-shader twin): a controlled variable - U or V offset, U or V scale, the emissive
//! multiple, alpha - driven by an `NiFloatInterpolator` over `NiFloatData` keys, looping under the
//! controller's timing. `FireplaceWood01Burning`'s two flame cards scroll their V offset by one
//! whole tile every 5.667 s and 4.267 s (Phase 2 Dev decoded the NIF; websearch-173). Without it a
//! flame is one still, pale frame of its atlas.
//!
//! The converter publishes the controllers as `OPEN_SKYRIM_material_animation` on the material
//! (the contract Phase 2 Dev proposed on 2026-09-25):
//!
//! ```json
//! "OPEN_SKYRIM_material_animation": { "channels": [{
//!     "variable": "vOffset", "interpolation": "LINEAR" | "QUADRATIC" | "STEP",
//!     "times": [0.0, 5.6667], "values": [0.0, 1.0], "tangents": [[outgoing, incoming], ...],
//!     "loop": "cycle" | "reverse" | "clamp",
//!     "frequency": 1.0, "phase": 0.0, "start": 0.0, "stop": 5.6667 }] }
//! ```
//!
//! Times are seconds, `reverse` plays back and forth, and a `QUADRATIC` channel carries a Hermite
//! tangent pair per key, `[outgoing, incoming]`. The animated value replaces the static one.
//!
//! A colour channel (`BSEffectShaderPropertyColorController`, `emissiveColor`) carries
//! `"components": 3`: `values` holds three floats per key and `tangents` three pairs per key, key
//! first, and each component plays like a float channel (Phase 2 Dev's spec, 2026-09-25).
//!
//! This module plays the four texture-coordinate variables everywhere, and on a palette effect
//! (`crate::effect_palette`) the emissive colour and emissive multiple too: the colour's red is the
//! palette row the effect reads, the multiple its brightness. The others are parsed and left for
//! later.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use bevy::prelude::*;

use crate::effect_palette::EffectPaletteMaterial;

/// A shader value a channel drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variable {
    UOffset,
    VOffset,
    UScale,
    VScale,
    /// Played on palette effects only, where it is the palette row and the colour.
    EmissiveColor,
    /// Played on palette effects only: the colour's multiplier.
    EmissiveMultiple,
    /// Parsed and not yet played: alpha, falloff and the lighting shader's own values.
    Other,
}

impl Variable {
    fn parse(name: &str) -> Self {
        match name {
            "uOffset" => Self::UOffset,
            "vOffset" => Self::VOffset,
            "uScale" => Self::UScale,
            "vScale" => Self::VScale,
            "emissiveColor" => Self::EmissiveColor,
            "emissiveMultiple" => Self::EmissiveMultiple,
            _ => Self::Other,
        }
    }

    fn moves_texture(self) -> bool {
        matches!(
            self,
            Self::UOffset | Self::VOffset | Self::UScale | Self::VScale
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolation {
    Linear,
    Quadratic,
    Step,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Cycle,
    Reverse,
    Clamp,
}

/// One animated value: keys, how to interpolate them, and the controller's timing.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub variable: Variable,
    pub interpolation: Interpolation,
    pub times: Vec<f32>,
    /// How many values a key has: 1, or 3 for a colour.
    pub components: usize,
    /// `components` values per key, key first.
    pub values: Vec<f32>,
    /// `[outgoing, incoming]` per key and component, for `QUADRATIC` only: a segment uses its start
    /// key's outgoing and its end key's incoming tangent, in value units per segment. NifSkope's
    /// evaluator reads them from `NiFloatData` as the start key's `Backward` and the end key's
    /// `Forward`; the converter publishes them already in this order (Phase 2 Dev, 2026-09-25).
    pub tangents: Vec<[f32; 2]>,
    pub loop_mode: LoopMode,
    pub frequency: f32,
    pub phase: f32,
    pub start: f32,
    pub stop: f32,
}

impl Channel {
    /// The controller's local time for a clock of `seconds`: scaled by the frequency, shifted by
    /// the phase, and brought into `start..=stop` by the loop mode.
    pub fn local_time(&self, seconds: f32) -> f32 {
        let t = seconds * self.frequency + self.phase;
        let span = self.stop - self.start;
        if !(span.is_finite() && span > 0.0 && t.is_finite()) {
            return self.start;
        }
        match self.loop_mode {
            LoopMode::Cycle => self.start + (t - self.start).rem_euclid(span),
            LoopMode::Reverse => {
                let u = (t - self.start).rem_euclid(2.0 * span);
                self.start + if u > span { 2.0 * span - u } else { u }
            }
            LoopMode::Clamp => t.clamp(self.start, self.stop),
        }
    }

    /// The channel's value at controller time `t`: its first component.
    pub fn value_at(&self, t: f32) -> f32 {
        self.component_at(t, 0)
    }

    /// Component `c` of the channel's value at controller time `t`.
    pub fn component_at(&self, t: f32, c: usize) -> f32 {
        let n = self.components.max(1);
        let keys = self.times.len();
        if c >= n || keys == 0 || self.values.len() != keys * n {
            return self.values.first().copied().unwrap_or(0.0);
        }
        let value = |key: usize| self.values[key * n + c];
        if t <= self.times[0] {
            return value(0);
        }
        if t >= self.times[keys - 1] {
            return value(keys - 1);
        }
        let i = self
            .times
            .windows(2)
            .position(|w| t >= w[0] && t <= w[1])
            .unwrap_or(0);
        let (t0, t1) = (self.times[i], self.times[i + 1]);
        let (v0, v1) = (value(i), value(i + 1));
        let s = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
        match self.interpolation {
            Interpolation::Step => v0,
            Interpolation::Linear => v0 + (v1 - v0) * s,
            Interpolation::Quadratic => {
                let m0 = self.tangents.get(i * n + c).map_or(v1 - v0, |pair| pair[0]);
                let m1 = self
                    .tangents
                    .get((i + 1) * n + c)
                    .map_or(v1 - v0, |pair| pair[1]);
                let (s2, s3) = (s * s, s * s * s);
                (2.0 * s3 - 3.0 * s2 + 1.0) * v0
                    + (s3 - 2.0 * s2 + s) * m0
                    + (-2.0 * s3 + 3.0 * s2) * v1
                    + (s3 - s2) * m1
            }
        }
    }

    pub fn value(&self, seconds: f32) -> f32 {
        self.value_at(self.local_time(seconds))
    }

    /// A three-component channel's value at `seconds`.
    pub fn vec3(&self, seconds: f32) -> Vec3 {
        let t = self.local_time(seconds);
        Vec3::new(
            self.component_at(t, 0),
            self.component_at(t, 1),
            self.component_at(t, 2),
        )
    }
}

/// A material's animation: its channels and the static texture transform they replace.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialAnimation {
    pub channels: Vec<Channel>,
    pub base_offset: Vec2,
    pub base_scale: Vec2,
}

impl MaterialAnimation {
    /// The texture offset and scale at `seconds`: each animated coordinate replaces its static one.
    pub fn uv_offset_scale(&self, seconds: f32) -> (Vec2, Vec2) {
        let (mut offset, mut scale) = (self.base_offset, self.base_scale);
        for channel in &self.channels {
            let value = channel.value(seconds);
            match channel.variable {
                Variable::UOffset => offset.x = value,
                Variable::VOffset => offset.y = value,
                Variable::UScale => scale.x = value,
                Variable::VScale => scale.y = value,
                Variable::EmissiveColor | Variable::EmissiveMultiple | Variable::Other => {}
            }
        }
        (offset, scale)
    }

    /// The animated emissive colour at `seconds`, when a three-component channel drives it.
    pub fn emissive_color(&self, seconds: f32) -> Option<Vec3> {
        self.channels
            .iter()
            .find(|channel| channel.variable == Variable::EmissiveColor && channel.components == 3)
            .map(|channel| channel.vec3(seconds))
    }

    /// The animated emissive multiple at `seconds`, when a channel drives it.
    pub fn emissive_multiple(&self, seconds: f32) -> Option<f32> {
        self.channels
            .iter()
            .find(|channel| channel.variable == Variable::EmissiveMultiple)
            .map(|channel| channel.value(seconds))
    }

    /// Whether any channel drives the texture transform.
    pub fn moves_texture(&self) -> bool {
        self.channels
            .iter()
            .any(|channel| channel.variable.moves_texture())
    }

    /// Whether any channel drives a value this module plays somewhere; an animation of only values
    /// it does not play yet is not worth recording.
    pub fn plays_anything(&self) -> bool {
        self.channels
            .iter()
            .any(|channel| channel.variable != Variable::Other)
    }

    /// The animation `OPEN_SKYRIM_material_animation` describes, with the static transform from
    /// `OPEN_SKYRIM_material` (`uvOffset`, `uvScale`) when it publishes one.
    pub fn from_extensions(
        animation: &serde_json::Value,
        material: Option<&serde_json::Value>,
    ) -> Option<Self> {
        let channels: Vec<Channel> = animation
            .get("channels")?
            .as_array()?
            .iter()
            .filter_map(parse_channel)
            .collect();
        if channels.is_empty() {
            return None;
        }
        let pair = |key: &str, default: Vec2| {
            material
                .and_then(|material| material.get(key))
                .and_then(|value| value.as_array())
                .and_then(|pair| {
                    Some(Vec2::new(
                        pair.first()?.as_f64()? as f32,
                        pair.get(1)?.as_f64()? as f32,
                    ))
                })
                .unwrap_or(default)
        };
        Some(Self {
            channels,
            base_offset: pair("uvOffset", Vec2::ZERO),
            base_scale: pair("uvScale", Vec2::ONE),
        })
    }
}

fn parse_channel(value: &serde_json::Value) -> Option<Channel> {
    let floats = |key: &str| -> Option<Vec<f32>> {
        value
            .get(key)?
            .as_array()?
            .iter()
            .map(|entry| entry.as_f64().map(|v| v as f32))
            .collect()
    };
    let number = |key: &str, default: f32| {
        value
            .get(key)
            .and_then(|entry| entry.as_f64())
            .map_or(default, |v| v as f32)
    };
    let times = floats("times")?;
    let values = floats("values")?;
    let components = value
        .get("components")
        .and_then(|entry| entry.as_u64())
        .map_or(1, |n| n as usize);
    if times.is_empty() || components == 0 || times.len() * components != values.len() {
        return None;
    }
    let interpolation = match value.get("interpolation").and_then(|v| v.as_str()) {
        Some("QUADRATIC") => Interpolation::Quadratic,
        Some("STEP") => Interpolation::Step,
        _ => Interpolation::Linear,
    };
    let tangents = value
        .get("tangents")
        .and_then(|v| v.as_array())
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    Some([
                        pair.first()?.as_f64()? as f32,
                        pair.get(1)?.as_f64()? as f32,
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    let loop_mode = match value.get("loop").and_then(|v| v.as_str()) {
        Some("reverse") => LoopMode::Reverse,
        Some("clamp") => LoopMode::Clamp,
        _ => LoopMode::Cycle,
    };
    let start = number("start", times[0]);
    let stop = number("stop", times[times.len() - 1]);
    Some(Channel {
        variable: Variable::parse(value.get("variable")?.as_str()?),
        interpolation,
        times,
        components,
        values,
        tangents,
        loop_mode,
        frequency: number("frequency", 1.0),
        phase: number("phase", 0.0),
        start,
        stop,
    })
}

/// Every material animation the glTF handler has recorded, keyed by the asset path of the
/// standard material it belongs to (`meshes/...glb#Material0/std`), as
/// [`crate::effect_palette::EffectPaletteRegistry`] is and for the same reason: the handler runs
/// on the loader's tasks, and a labelled asset nothing holds would be dropped.
#[derive(Resource, Clone, Default)]
pub struct MaterialAnimationRegistry(Arc<Mutex<HashMap<String, MaterialAnimation>>>);

impl MaterialAnimationRegistry {
    pub fn insert(&self, material_path: String, animation: MaterialAnimation) {
        if let Ok(mut animations) = self.0.lock() {
            animations.insert(material_path, animation);
        }
    }

    pub fn get(&self, material_path: &str) -> Option<MaterialAnimation> {
        self.0.lock().ok()?.get(material_path).cloned()
    }
}

/// Palette effect materials whose source texture is animated, with their animation. Filled by the
/// palette swap in `crate::streaming` as it builds each one.
#[derive(Resource, Default)]
pub struct AnimatedPaletteMaterials(pub Vec<(Handle<EffectPaletteMaterial>, MaterialAnimation)>);

/// Standard materials found to carry an animation, and every one already looked at.
#[derive(Resource, Default)]
struct AnimatedStandardMaterials {
    resolved: HashSet<AssetId<StandardMaterial>>,
    animated: Vec<(AssetId<StandardMaterial>, MaterialAnimation)>,
}

/// Plays material animations every frame.
pub struct MaterialAnimationPlugin;

impl Plugin for MaterialAnimationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AnimatedPaletteMaterials>()
            .init_resource::<AnimatedStandardMaterials>()
            .add_systems(
                Update,
                (find_animated_standard_materials, animate_materials).chain(),
            );
    }
}

/// Looks each newly loaded standard material up in the registry once.
fn find_animated_standard_materials(
    mut events: MessageReader<AssetEvent<StandardMaterial>>,
    asset_server: Res<AssetServer>,
    registry: Option<Res<MaterialAnimationRegistry>>,
    mut animated: ResMut<AnimatedStandardMaterials>,
) {
    let Some(registry) = registry else {
        events.clear();
        return;
    };
    for event in events.read() {
        let (AssetEvent::Added { id } | AssetEvent::LoadedWithDependencies { id }) = event else {
            continue;
        };
        if !animated.resolved.insert(*id) {
            continue;
        }
        let Some(path) = asset_server.get_path(*id) else {
            continue;
        };
        // A standard material plays its texture transform only; a colour it animates waits for
        // the palette swap, which is where effects are drawn.
        if let Some(animation) = registry
            .get(&path.to_string())
            .filter(MaterialAnimation::moves_texture)
        {
            animated.animated.push((*id, animation));
        }
    }
}

/// The texture transform `uv * scale + offset`, as Bevy's `uv_transform` holds it.
pub fn uv_transform(offset: Vec2, scale: Vec2) -> bevy::math::Affine2 {
    bevy::math::Affine2::from_scale_angle_translation(scale, 0.0, offset)
}

/// Writes each animated material's texture transform for this frame's time.
fn animate_materials(
    time: Res<Time>,
    mut standard: ResMut<Assets<StandardMaterial>>,
    animated_standard: Res<AnimatedStandardMaterials>,
    palettes: Option<ResMut<Assets<EffectPaletteMaterial>>>,
    animated_palettes: Res<AnimatedPaletteMaterials>,
) {
    let seconds = time.elapsed_secs();
    for (id, animation) in &animated_standard.animated {
        if let Some(mut material) = standard.get_mut(*id) {
            let (offset, scale) = animation.uv_offset_scale(seconds);
            material.uv_transform = uv_transform(offset, scale);
        }
    }
    let Some(mut palettes) = palettes else {
        return;
    };
    for (handle, animation) in &animated_palettes.0 {
        if let Some(mut material) = palettes.get_mut(handle) {
            if animation.moves_texture() {
                let (offset, scale) = animation.uv_offset_scale(seconds);
                material.extension.set_uv(offset, scale);
                material.base.uv_transform = uv_transform(offset, scale);
            }
            let colour = animation.emissive_color(seconds);
            let multiple = animation.emissive_multiple(seconds);
            if colour.is_some() || multiple.is_some() {
                material
                    .extension
                    .set_emissive(colour.map(|colour| colour.x), multiple);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FireplaceWood01Burning`'s first flame card, as Phase 2 Dev decoded it: V offset, one tile
    /// every 5.667 s, cycling.
    fn flame() -> serde_json::Value {
        serde_json::json!({"channels": [{
            "variable": "vOffset", "interpolation": "LINEAR",
            "times": [0.0, 5.6667], "values": [0.0, 1.0],
            "loop": "cycle", "frequency": 1.0, "phase": 0.0, "start": 0.0, "stop": 5.6667
        }]})
    }

    #[test]
    fn a_flame_scrolls_one_tile_per_loop_and_wraps() {
        let animation = MaterialAnimation::from_extensions(&flame(), None).unwrap();
        assert!(animation.moves_texture());
        let (offset, scale) = animation.uv_offset_scale(0.0);
        assert_eq!((offset, scale), (Vec2::ZERO, Vec2::ONE));
        let (offset, _) = animation.uv_offset_scale(5.6667 / 2.0);
        assert!(
            (offset.y - 0.5).abs() < 1e-4,
            "halfway through the loop, {offset}"
        );
        assert_eq!(offset.x, 0.0, "only V is animated");
        let (offset, _) = animation.uv_offset_scale(5.6667 + 1.0);
        let (again, _) = animation.uv_offset_scale(1.0);
        assert!((offset.y - again.y).abs() < 1e-4, "a cycle repeats");
    }

    #[test]
    fn reverse_plays_back_and_clamp_holds() {
        let mut channel = parse_channel(&flame()["channels"][0]).unwrap();
        channel.loop_mode = LoopMode::Reverse;
        assert!(
            (channel.value(5.6667 * 1.25) - 0.75).abs() < 1e-3,
            "on the way back"
        );
        channel.loop_mode = LoopMode::Clamp;
        assert!((channel.value(100.0) - 1.0).abs() < 1e-6, "held at the end");
        assert_eq!(channel.value(-3.0), 0.0, "and at the start");
    }

    #[test]
    fn step_and_quadratic_keys() {
        let mut channel = parse_channel(&serde_json::json!({
            "variable": "uScale", "interpolation": "STEP",
            "times": [0.0, 1.0, 2.0], "values": [1.0, 2.0, 3.0]
        }))
        .unwrap();
        assert_eq!(channel.value_at(0.5), 1.0);
        assert_eq!(channel.value_at(1.5), 2.0);
        channel.interpolation = Interpolation::Quadratic;
        channel.tangents = vec![[1.0, 1.0]; 3];
        // With tangents equal to the slope, Hermite is the straight line.
        assert!((channel.value_at(0.5) - 1.5).abs() < 1e-5);
    }

    #[test]
    fn the_static_transform_is_what_an_unanimated_coordinate_keeps() {
        let material = serde_json::json!({"uvOffset": [0.25, 0.5], "uvScale": [2.0, 3.0]});
        let animation = MaterialAnimation::from_extensions(&flame(), Some(&material)).unwrap();
        let (offset, scale) = animation.uv_offset_scale(0.0);
        assert_eq!(
            offset.x, 0.25,
            "U is not animated: its static offset stands"
        );
        assert_eq!(
            offset.y, 0.0,
            "V is animated: its channel replaces the static 0.5"
        );
        assert_eq!(scale, Vec2::new(2.0, 3.0));
    }

    #[test]
    fn a_colour_channel_plays_each_component_and_does_not_move_the_texture() {
        let glow = serde_json::json!({"channels": [{
            "variable": "emissiveColor", "components": 3, "interpolation": "LINEAR",
            "times": [0.0, 2.0], "values": [0.2, 0.4, 0.6, 1.0, 0.0, 0.6],
            "loop": "cycle", "start": 0.0, "stop": 2.0
        }, {
            "variable": "emissiveMultiple", "times": [0.0, 2.0], "values": [1.0, 3.0]
        }]});
        let animation = MaterialAnimation::from_extensions(&glow, None).unwrap();
        assert!(animation.plays_anything());
        assert!(!animation.moves_texture());
        let colour = animation.emissive_color(1.0).unwrap();
        assert!(
            (colour - Vec3::new(0.6, 0.2, 0.6)).length() < 1e-5,
            "{colour}"
        );
        assert!((animation.emissive_multiple(1.0).unwrap() - 2.0).abs() < 1e-5);
        let quadratic = parse_channel(&serde_json::json!({
            "variable": "emissiveColor", "components": 3, "interpolation": "QUADRATIC",
            "times": [0.0, 1.0], "values": [0.0, 0.0, 0.0, 1.0, 2.0, 3.0],
            "tangents": [[1.0, 1.0], [2.0, 2.0], [3.0, 3.0], [1.0, 1.0], [2.0, 2.0], [3.0, 3.0]]
        }))
        .unwrap();
        // Tangents equal to each component's slope: Hermite is the straight line per component.
        let halfway = quadratic.vec3(0.5);
        assert!(
            (halfway - Vec3::new(0.5, 1.0, 1.5)).length() < 1e-5,
            "{halfway}"
        );
        assert!(
            parse_channel(&serde_json::json!({
                "variable": "emissiveColor", "components": 3,
                "times": [0.0, 1.0], "values": [0.0, 1.0]
            }))
            .is_none(),
            "values must hold three per key"
        );
    }

    #[test]
    fn a_channel_only_for_values_not_played_yet_does_not_move_the_texture() {
        let only_alpha = serde_json::json!({"channels": [{
            "variable": "alpha", "times": [0.0, 1.0], "values": [0.0, 1.0]
        }]});
        let animation = MaterialAnimation::from_extensions(&only_alpha, None).unwrap();
        assert!(!animation.moves_texture());
        assert!(!animation.plays_anything());
        assert!(
            MaterialAnimation::from_extensions(&serde_json::json!({"channels": []}), None)
                .is_none()
        );
    }
}
