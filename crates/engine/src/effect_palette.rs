//! Skyrim's greyscale-to-palette effect shaders: a fire, a light shaft or a magic effect whose
//! texture is greyscale and whose colour comes from a second, *palette* texture.
//!
//! A `BSEffectShaderProperty` with `ShaderFlags1` bit 4 (`Greyscale_To_PaletteColor`) draws the
//! palette's colour at `u = the source texture's green, v = the base colour's red`, times the base
//! colour scale; bit 5 (`Greyscale_To_PaletteAlpha`) does the same for alpha with
//! `u = the source's alpha, v = the base colour's alpha`. That is Community Shaders' `Effect.hlsl`
//! reading (websearch-169, `tasks/deepseek/websearch-169-portal-greyscale-palette.md`); no
//! Bethesda source exists. Without it every hearth fire, candle flame and light shaft rendered as a
//! white-grey shape: the flame atlas `textures/effects/fxfireatlas04` is greyscale
//! (`docs/research/look-gaps-2026-09-24.md`, item 3).
//!
//! The converter publishes what this needs on the effect material (Phase 2's `082a9e4`): the
//! source texture as the emissive texture, the base colour as `emissiveFactor`, the base colour
//! scale as `KHR_materials_emissive_strength`, the coverage alpha in `baseColorFactor`, and the
//! palette as a `textureSlots` entry of semantic `greyscale` in `OPEN_SKYRIM_material`. The glTF
//! handler in `crate::render` records an [`EffectPalette`] for the material in the
//! [`EffectPaletteRegistry`], and `crate::streaming` swaps a validated mesh onto
//! [`EffectPaletteMaterial`].

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use bevy::{
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
};

/// `ShaderFlags1` bit 4: the palette gives the effect its colour.
pub const GREYSCALE_TO_PALETTE_COLOR: u64 = 1 << 4;
/// `ShaderFlags1` bit 5: the palette gives the effect its alpha.
pub const GREYSCALE_TO_PALETTE_ALPHA: u64 = 1 << 5;

/// How many times the palette colour (already times the base colour scale) an effect is drawn at.
///
/// The same unit gap [`crate::lights::EMISSIVE_EXPOSURE`] closes for the lighting-shader glows: a
/// published emissive of about 1 is a thousandth of the light an interior is exposed for.
/// 8 (2026-09-25, on the schema-22 data with the flames scrolling): at 25 and above the flame
/// core's palette colour times its 1.75 scale was bright enough for the tonemapper to bleach it
/// white; at 8 the hearth reads orange, at 4 dull. The grey translucent sheet over the Sleeping
/// Giant's flames is not the emission: it is the `Flames:0` card drawn without the vertex colours
/// the converter drops (their alpha is the palette's alpha row) and without its billboard node,
/// both converter gaps (2026-09-25; `local/team/2026-09-25.md`).
pub const EFFECT_EMISSIVE_EXPOSURE: f32 = 8.0;

/// What a palette effect material needs besides its standard material: the palette and the values
/// that pick where in it the effect samples. Recorded by the glTF handler in the
/// [`EffectPaletteRegistry`] under the path of the material it belongs to.
#[derive(Debug, Clone)]
pub struct EffectPalette {
    pub palette: Handle<Image>,
    /// The source texture, sampled for the grey value (the material's emissive texture).
    pub source: Handle<Image>,
    pub settings: PaletteSettings,
}

/// The values [`EffectPalette`] carries, apart from the textures. Parsed by [`palette_settings`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaletteSettings {
    pub color: bool,
    pub alpha: bool,
    /// The palette row the colour is read from: the base colour's red.
    pub v_color: f32,
    /// The palette row the alpha is read from: the base colour's alpha.
    pub v_alpha: f32,
    /// The base colour scale the palette colour is multiplied by.
    pub scale: f32,
}

/// The palette settings and the glTF texture index of the palette, for an effect material whose
/// flags ask for the palette and whose extension names a `greyscale` slot; `None` for every other
/// material, which draws as it did.
pub fn palette_settings(
    extension: &serde_json::Value,
    emissive_factor: [f32; 3],
    emissive_strength: f32,
    base_alpha: f32,
) -> Option<(usize, PaletteSettings)> {
    let family = extension.get("shaderFamily")?.as_str()?;
    if !family.eq_ignore_ascii_case("effect") {
        return None;
    }
    let flags = extension.get("shaderFlags1")?.as_u64()?;
    let color = flags & GREYSCALE_TO_PALETTE_COLOR != 0;
    let alpha = flags & GREYSCALE_TO_PALETTE_ALPHA != 0;
    if !color && !alpha {
        return None;
    }
    let texture = extension
        .get("textureSlots")?
        .as_array()?
        .iter()
        .find(|slot| {
            slot.get("semantic")
                .and_then(|semantic| semantic.as_str())
                .is_some_and(|semantic| semantic.eq_ignore_ascii_case("greyscale"))
        })?
        .get("texture")?
        .as_u64()?;
    Some((
        usize::try_from(texture).ok()?,
        PaletteSettings {
            color,
            alpha,
            v_color: emissive_factor[0].clamp(0.0, 1.0),
            v_alpha: base_alpha.clamp(0.0, 1.0),
            scale: emissive_strength.max(0.0),
        },
    ))
}

/// Every palette the glTF handler has recorded, keyed by the asset path of the standard material it
/// belongs to (`meshes/...glb#Material0/std`).
///
/// A registry rather than a labelled asset beside the material: a labelled asset nothing holds a
/// handle to is dropped as soon as the load finishes, so the swap could never find it. The handler
/// runs on the asset loader's tasks, hence the lock; it is written once per effect material and
/// read once per distinct material by the swap in `crate::streaming`.
#[derive(Resource, Clone, Default)]
pub struct EffectPaletteRegistry(Arc<Mutex<HashMap<String, EffectPalette>>>);

impl EffectPaletteRegistry {
    pub fn insert(&self, material_path: String, palette: EffectPalette) {
        if let Ok(mut palettes) = self.0.lock() {
            palettes.insert(material_path, palette);
        }
    }

    pub fn get(&self, material_path: &str) -> Option<EffectPalette> {
        self.0.lock().ok()?.get(material_path).cloned()
    }
}

/// The material a palette effect's meshes are drawn with.
pub type EffectPaletteMaterial = ExtendedMaterial<StandardMaterial, EffectPaletteExtension>;

/// The palette, the source texture and the uniform `effect_palette.wgsl` reads.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct EffectPaletteExtension {
    #[uniform(100)]
    settings: EffectPaletteUniform,
    #[texture(101)]
    #[sampler(102)]
    palette: Handle<Image>,
    #[texture(103)]
    #[sampler(104)]
    source: Handle<Image>,
    /// The published colour row and multiple, which an animated effect's missing channel keeps.
    static_row: f32,
    static_multiple: f32,
}

/// The uniform's layout, which `effect_palette.wgsl` declares field for field.
#[derive(ShaderType, Reflect, Debug, Clone, PartialEq)]
struct EffectPaletteUniform {
    /// x: 1 when the palette gives the colour, y: 1 when it gives the alpha, z: the colour row,
    /// w: the alpha row.
    flags_and_rows: Vec4,
    /// x: the colour's multiplier, the base colour scale times [`EFFECT_EMISSIVE_EXPOSURE`].
    scale: Vec4,
    /// xy: the source texture's offset, zw: its scale (`uv * zw + xy`), which an animated effect
    /// moves every frame (`crate::material_animation`).
    uv_offset_scale: Vec4,
}

impl EffectPaletteExtension {
    pub fn new(palette: &EffectPalette) -> Self {
        let settings = palette.settings;
        Self {
            settings: EffectPaletteUniform {
                flags_and_rows: Vec4::new(
                    if settings.color { 1.0 } else { 0.0 },
                    if settings.alpha { 1.0 } else { 0.0 },
                    settings.v_color,
                    settings.v_alpha,
                ),
                scale: Vec4::new(settings.scale * EFFECT_EMISSIVE_EXPOSURE, 0.0, 0.0, 0.0),
                uv_offset_scale: Vec4::new(0.0, 0.0, 1.0, 1.0),
            },
            palette: palette.palette.clone(),
            source: palette.source.clone(),
            static_row: settings.v_color,
            static_multiple: settings.scale,
        }
    }

    /// Marks the card as additive, so fog dims it toward black as Skyrim's does.
    pub fn set_additive(&mut self, additive: bool) {
        self.settings.scale.y = if additive { 1.0 } else { 0.0 };
    }

    /// Plays an animated emissive colour and multiple (`crate::material_animation`): the colour's
    /// red is the palette row, as the published colour's is; `None` keeps the published value.
    pub fn set_emissive(&mut self, red: Option<f32>, multiple: Option<f32>) {
        self.settings.flags_and_rows.z = red.unwrap_or(self.static_row).clamp(0.0, 1.0);
        self.settings.scale.x =
            multiple.unwrap_or(self.static_multiple).max(0.0) * EFFECT_EMISSIVE_EXPOSURE;
    }

    /// `(colour flag, alpha flag, colour row, alpha row)`, for the tests.
    pub fn flags_and_rows(&self) -> Vec4 {
        self.settings.flags_and_rows
    }

    /// The colour multiplier, for the tests.
    pub fn colour_scale(&self) -> f32 {
        self.settings.scale.x
    }

    /// Moves the source texture: `uv * scale + offset`.
    pub fn set_uv(&mut self, offset: Vec2, scale: Vec2) {
        self.settings.uv_offset_scale = Vec4::new(offset.x, offset.y, scale.x, scale.y);
    }

    /// The source texture's `(offset, scale)`, for the tests.
    pub fn uv(&self) -> (Vec2, Vec2) {
        let v = self.settings.uv_offset_scale;
        (Vec2::new(v.x, v.y), Vec2::new(v.z, v.w))
    }
}

impl MaterialExtension for EffectPaletteExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/effect_palette.wgsl".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FireplaceWood01Burning`'s `Flames:0` as the converter publishes it: `0xC0000078` sets
    /// bits 3 to 6, so both palette flags.
    fn flames() -> serde_json::Value {
        serde_json::json!({
            "shaderFamily": "effect",
            "shaderFlags1": 3_221_225_592u64,
            "textureSlots": [{"semantic": "greyscale", "slot": 1, "texture": 1, "colorSpace": "linear"}]
        })
    }

    #[test]
    fn reads_the_palette_slot_and_both_flags_of_a_real_flame() {
        let (texture, settings) = palette_settings(&flames(), [0.22, 0.22, 0.22], 1.75, 1.0)
            .expect("the flame asks for its palette");
        assert_eq!(texture, 1);
        assert!(settings.color && settings.alpha);
        assert_eq!(settings.v_color, 0.22);
        assert_eq!(settings.v_alpha, 1.0);
        assert_eq!(settings.scale, 1.75);
    }

    #[test]
    fn each_flag_on_its_own_and_neither() {
        let mut only_color = flames();
        only_color["shaderFlags1"] = serde_json::json!(GREYSCALE_TO_PALETTE_COLOR);
        let (_, settings) = palette_settings(&only_color, [0.5; 3], 1.0, 1.0).unwrap();
        assert!(settings.color && !settings.alpha);

        let mut only_alpha = flames();
        only_alpha["shaderFlags1"] = serde_json::json!(GREYSCALE_TO_PALETTE_ALPHA);
        let (_, settings) = palette_settings(&only_alpha, [0.5; 3], 1.0, 1.0).unwrap();
        assert!(!settings.color && settings.alpha);

        let mut neither = flames();
        neither["shaderFlags1"] = serde_json::json!(0x08u64);
        assert!(palette_settings(&neither, [0.5; 3], 1.0, 1.0).is_none());
    }

    #[test]
    fn a_lighting_material_or_one_without_a_palette_slot_is_left_alone() {
        let mut lighting = flames();
        lighting["shaderFamily"] = serde_json::json!("lighting");
        assert!(palette_settings(&lighting, [0.5; 3], 1.0, 1.0).is_none());

        let mut no_slot = flames();
        no_slot["textureSlots"] = serde_json::json!([]);
        assert!(palette_settings(&no_slot, [0.5; 3], 1.0, 1.0).is_none());
    }

    #[test]
    fn the_uniform_carries_the_rows_and_the_exposed_scale() {
        let (_, settings) = palette_settings(&flames(), [0.22, 0.22, 0.22], 1.75, 0.8).unwrap();
        let extension = EffectPaletteExtension::new(&EffectPalette {
            palette: Handle::default(),
            source: Handle::default(),
            settings,
        });
        assert_eq!(extension.flags_and_rows(), Vec4::new(1.0, 1.0, 0.22, 0.8));
        assert_eq!(extension.colour_scale(), 1.75 * EFFECT_EMISSIVE_EXPOSURE);
    }

    #[test]
    fn an_animated_colour_moves_the_row_and_a_missing_channel_keeps_the_published_value() {
        let (_, settings) = palette_settings(&flames(), [0.22, 0.22, 0.22], 1.75, 0.8).unwrap();
        let mut extension = EffectPaletteExtension::new(&EffectPalette {
            palette: Handle::default(),
            source: Handle::default(),
            settings,
        });
        extension.set_emissive(Some(0.6), None);
        assert_eq!(extension.flags_and_rows().z, 0.6);
        assert_eq!(extension.colour_scale(), 1.75 * EFFECT_EMISSIVE_EXPOSURE);
        extension.set_emissive(None, Some(3.0));
        assert_eq!(
            extension.flags_and_rows().z,
            0.22,
            "the published row again"
        );
        assert_eq!(extension.colour_scale(), 3.0 * EFFECT_EMISSIVE_EXPOSURE);
    }

    #[test]
    fn the_source_texture_starts_unmoved_and_follows_its_animation() {
        let (_, settings) = palette_settings(&flames(), [0.22, 0.22, 0.22], 1.75, 1.0).unwrap();
        let mut extension = EffectPaletteExtension::new(&EffectPalette {
            palette: Handle::default(),
            source: Handle::default(),
            settings,
        });
        assert_eq!(extension.uv(), (Vec2::ZERO, Vec2::ONE));
        extension.set_uv(Vec2::new(0.0, 0.4), Vec2::new(1.0, 2.0));
        assert_eq!(extension.uv(), (Vec2::new(0.0, 0.4), Vec2::new(1.0, 2.0)));
    }
}
