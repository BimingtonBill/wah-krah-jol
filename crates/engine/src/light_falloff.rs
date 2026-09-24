//! Skyrim's point-light falloff for the converted `LIGH` lights.
//!
//! Skyrim attenuates a point light as `max(0, 1 - (d/r)^2)` with `r` the light's radius: nearly
//! full strength across most of the radius and a hard stop at its edge (Community Shaders'
//! `Lighting.hlsl`, the vanilla shader it reimplements; websearch-171). Bevy's PBR attenuates as
//! inverse square times a range window. With one intensity per radius the two agree at one
//! distance at most: calibrated at half the radius, Bevy's curve gave 21 times Skyrim's light at a
//! tenth of the radius and a sixth of it at nine tenths (QnA's `local/research/
//! bevy-skyrim-light-falloff.md`). Interiors were too bright next to a light and too dark between
//! lights, and their fog was standing in for the missing fill (`docs/research/look-gaps-2026-09-24.md`,
//! item 1).
//!
//! No setting of `range`, `intensity` or `radius` approximates the Skyrim curve, so the one line
//! of Bevy's `pbr_lighting.wgsl` that computes a point light's attenuation is rewritten when the
//! shader library loads ([`patch_pbr_lighting`]). The rewrite applies only to lights whose
//! `affects_lightmapped_mesh_diffuse` is off - a flag Bevy reads only under `LIGHTMAP`, which this
//! engine never uses - so every other point light keeps Bevy's curve. `crate::lights::point_light`
//! clears it on the converted lights. If a Bevy upgrade moves the line, the patch logs an error
//! and the lights fall back to Bevy's curve at their Skyrim intensity: brighter rooms, not a crash.

use bevy::{prelude::*, shader::Source};

/// Where Bevy 0.19 embeds the shader library that holds `point_light()`.
pub const PBR_LIGHTING_PATH: &str = "embedded://bevy_pbr/render/pbr_lighting.wgsl";

/// The line of `point_light()` in Bevy 0.19.0's `pbr_lighting.wgsl` (line 652) that the patch
/// replaces.
pub const ATTENUATION_LINE: &str = "let rangeAttenuation = getDistanceAttenuation(distance_square, (*light).color_inverse_square_range.w);";

/// What it becomes: Skyrim's curve for a light with `AFFECTS_LIGHTMAPPED_MESH_DIFFUSE` (bit 3)
/// clear, Bevy's own for the rest. `color_inverse_square_range.w` is `1 / range^2`.
pub const SKYRIM_ATTENUATION_LINE: &str = "let rangeAttenuation = select(saturate(1.0 - distance_square * (*light).color_inverse_square_range.w), getDistanceAttenuation(distance_square, (*light).color_inverse_square_range.w), ((*light).flags & 8u) != 0u); // OpenSkyrim: Skyrim point-light falloff";

/// Skyrim's attenuation at a fraction `d/r` of a light's radius.
pub fn skyrim_attenuation(fraction_of_radius: f32) -> f32 {
    (1.0 - fraction_of_radius * fraction_of_radius).clamp(0.0, 1.0)
}

/// The shader source with the attenuation line replaced, or `None` when the line is not there
/// (already patched, or a Bevy version that moved it).
pub fn patched_source(source: &str) -> Option<String> {
    source
        .contains(ATTENUATION_LINE)
        .then(|| source.replacen(ATTENUATION_LINE, SKYRIM_ATTENUATION_LINE, 1))
}

pub struct SkyrimLightFalloffPlugin;

impl Plugin for SkyrimLightFalloffPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, patch_pbr_lighting);
    }
}

/// Rewrites Bevy's attenuation line once the shader library is loaded. Changing the asset in place
/// keeps its id and import path, so every pipeline that imports it recompiles on the `Modified`
/// event: `StandardMaterial` and every extended material that lights through
/// `apply_pbr_lighting`.
fn patch_pbr_lighting(
    asset_server: Res<AssetServer>,
    mut shaders: ResMut<Assets<Shader>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let handle: Handle<Shader> = asset_server.load(PBR_LIGHTING_PATH);
    let Some(shader) = shaders.get(&handle) else {
        return;
    };
    *done = true;
    let Source::Wgsl(source) = &shader.source else {
        error!(
            path = PBR_LIGHTING_PATH,
            "Bevy's lighting shader is not WGSL; converted lights keep Bevy's falloff"
        );
        return;
    };
    let Some(patched) = patched_source(source) else {
        error!(
            path = PBR_LIGHTING_PATH,
            "Bevy's point-light attenuation line was not found (a Bevy upgrade?); converted lights keep Bevy's falloff"
        );
        return;
    };
    if let Some(mut shader) = shaders.get_mut(&handle) {
        shader.source = Source::Wgsl(patched.into());
        info!("point lights with the Skyrim flag use Skyrim's 1 - (d/r)^2 falloff");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anchor is the line Bevy 0.19.0 ships, read from the crate's own source file, so an
    /// upgrade that moves it fails here rather than silently at runtime.
    #[test]
    fn the_anchor_is_in_bevys_lighting_shader() {
        let registry = std::env::var("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|_| {
                std::env::var("USERPROFILE").map(|home| std::path::Path::new(&home).join(".cargo"))
            })
            .unwrap()
            .join("registry")
            .join("src");
        let Some(source) = std::fs::read_dir(&registry).ok().and_then(|indices| {
            indices.filter_map(Result::ok).find_map(|index| {
                std::fs::read_to_string(
                    index
                        .path()
                        .join("bevy_pbr-0.19.0/src/render/pbr_lighting.wgsl"),
                )
                .ok()
            })
        }) else {
            eprintln!("bevy_pbr 0.19.0's source is not in the cargo registry; skipped");
            return;
        };
        let patched = patched_source(&source).expect("the attenuation line is in Bevy's shader");
        assert!(patched.contains(SKYRIM_ATTENUATION_LINE));
        assert!(!patched.contains(ATTENUATION_LINE));
        assert!(
            patched_source(&patched).is_none(),
            "a patched shader is not patched twice"
        );
    }

    #[test]
    fn skyrims_curve_is_near_full_across_the_radius_and_stops_at_its_edge() {
        assert_eq!(skyrim_attenuation(0.0), 1.0);
        assert!((skyrim_attenuation(0.5) - 0.75).abs() < 1e-6);
        assert!((skyrim_attenuation(0.9) - 0.19).abs() < 1e-6);
        assert_eq!(skyrim_attenuation(1.0), 0.0);
        assert_eq!(skyrim_attenuation(1.5), 0.0);
    }
}
