//! Vertex-colour alpha in the depth prepass's alpha test.
//!
//! Bevy 0.19's main pass multiplies a mesh's `COLOR_0` into the base colour before an
//! `AlphaMode::Mask` material tests its alpha against the cutoff (`pbr_fragment.wgsl`), but the
//! prepass tests only the material colour and its texture (`prepass_alpha_discard` in
//! `pbr_prepass_functions.wgsl`). A masked shape whose vertex alpha fades it out therefore writes
//! depth in the prepass where the main pass later discards the fragment, and whatever lies behind
//! it fails the depth test: the clear colour shows through in the fragment's place. The shadow
//! passes use the same function, so the same fragments cast shadows they do not draw.
//!
//! Skyrim's alpha-tested shapes fade this way wherever their shader reads vertex alpha: the gravel
//! skirts around rocks and the moss and plaster decals on walls. So the prepass is made to test the
//! same alpha as the main pass: one line is inserted into Bevy's shader library when it loads
//! ([`patch_prepass_functions`]). If a Bevy upgrade moves the anchor, the patch logs an error and
//! the prepass keeps Bevy's test.

use bevy::{prelude::*, shader::Source};

/// Where Bevy 0.19 embeds the shader library that holds `prepass_alpha_discard()`.
pub const PREPASS_FUNCTIONS_PATH: &str = "embedded://bevy_pbr/render/pbr_prepass_functions.wgsl";

/// The line of `prepass_alpha_discard()` in Bevy 0.19.0 that follows the base colour's texture
/// sample and precedes the alpha test. The vertex colour is multiplied in just before it.
pub const ANCHOR_LINE: &str =
    "let alpha_mode = flags & pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_RESERVED_BITS;";

/// What is inserted before the anchor. The prepass's `VertexOutput` carries `color` exactly when
/// the mesh has `COLOR_0` (`VERTEX_COLORS`).
pub const VERTEX_COLOUR_LINES: &str = "#ifdef VERTEX_COLORS
    output_color = output_color * in.color; // OpenSkyrim: test the alpha the main pass tests
#endif
    ";

/// The shader source with the vertex colour multiplied in, or `None` when the anchor is not there
/// or the source is already patched.
pub fn patched_source(source: &str) -> Option<String> {
    (source.contains(ANCHOR_LINE) && !source.contains(VERTEX_COLOUR_LINES)).then(|| {
        source.replacen(
            ANCHOR_LINE,
            &format!("{VERTEX_COLOUR_LINES}{ANCHOR_LINE}"),
            1,
        )
    })
}

pub struct PrepassVertexAlphaPlugin;

impl Plugin for PrepassVertexAlphaPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, patch_prepass_functions);
    }
}

/// Patches the prepass library once it is loaded. Changing the asset in place keeps its id and
/// import path, so every prepass and shadow pipeline that imports it recompiles on the `Modified`
/// event.
fn patch_prepass_functions(
    asset_server: Res<AssetServer>,
    mut shaders: ResMut<Assets<Shader>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let handle: Handle<Shader> = asset_server.load(PREPASS_FUNCTIONS_PATH);
    let Some(shader) = shaders.get(&handle) else {
        return;
    };
    *done = true;
    let Source::Wgsl(source) = &shader.source else {
        error!(
            path = PREPASS_FUNCTIONS_PATH,
            "Bevy's prepass functions are not WGSL; masked shapes keep Bevy's prepass alpha test"
        );
        return;
    };
    let Some(patched) = patched_source(source) else {
        error!(
            path = PREPASS_FUNCTIONS_PATH,
            "the prepass alpha test's anchor was not found (a Bevy upgrade?); masked shapes keep Bevy's prepass alpha test"
        );
        return;
    };
    if let Some(mut shader) = shaders.get_mut(&handle) {
        shader.source = Source::Wgsl(patched.into());
        info!("the depth prepass tests vertex-colour alpha like the main pass");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anchor is read from the bevy_pbr 0.19.0 source in the cargo registry, so an upgrade that
    /// moves it fails here rather than silently at runtime.
    #[test]
    fn the_anchor_is_in_bevys_prepass_functions() {
        let registry = std::env::var("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|_| {
                std::env::var("USERPROFILE")
                    .or_else(|_| std::env::var("HOME"))
                    .map(|home| std::path::Path::new(&home).join(".cargo"))
            })
            .unwrap()
            .join("registry")
            .join("src");
        let Some(source) = std::fs::read_dir(&registry).ok().and_then(|indices| {
            indices.filter_map(Result::ok).find_map(|index| {
                std::fs::read_to_string(
                    index
                        .path()
                        .join("bevy_pbr-0.19.0/src/render/pbr_prepass_functions.wgsl"),
                )
                .ok()
            })
        }) else {
            eprintln!("bevy_pbr 0.19.0's source is not in the cargo registry; skipped");
            return;
        };
        let patched = patched_source(&source).expect("the anchor is in Bevy's prepass functions");
        let discard = patched
            .find("fn prepass_alpha_discard")
            .expect("prepass_alpha_discard exists");
        let multiply = patched
            .find("output_color = output_color * in.color;")
            .expect("the multiply was inserted");
        let test = patched.find(ANCHOR_LINE).expect("the anchor is kept");
        assert!(discard < multiply && multiply < test);
        assert!(
            patched_source(&patched).is_none(),
            "a patched shader is not patched twice"
        );
    }

    #[test]
    fn a_source_without_the_anchor_is_left_alone() {
        assert!(patched_source("fn prepass_alpha_discard(in: VertexOutput) {}").is_none());
    }
}
