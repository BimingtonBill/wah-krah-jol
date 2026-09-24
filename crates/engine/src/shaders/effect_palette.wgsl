// Greyscale-to-palette effect shaders, as `crates/engine/src/effect_palette.rs` documents them:
// the source texture's green picks the palette's column and the base colour's red its row for the
// colour; the source's alpha and the base colour's alpha do the same for the alpha.
//
// Everything else is the standard material: its blend mode, its alpha discard and Bevy's lighting.
// An effect's base colour RGB is published as 0, so the emissive computed here is its whole colour.

#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
}

struct EffectPaletteSettings {
    // x: palette gives the colour, y: palette gives the alpha, z: colour row, w: alpha row.
    flags_and_rows: vec4<f32>,
    // x: the colour's multiplier (base colour scale times the engine's effect exposure).
    scale: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> effect: EffectPaletteSettings;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var palette_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var palette_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var source_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var source_sampler: sampler;

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);
#ifdef VERTEX_UVS_A
    let source = textureSample(source_texture, source_sampler, in.uv);
#else
    let source = vec4<f32>(1.0);
#endif
    var row = effect.flags_and_rows.z;
#ifdef VERTEX_COLORS
    row = row * in.color.r;
#endif
    if (effect.flags_and_rows.x > 0.5) {
        let colour = textureSample(palette_texture, palette_sampler, vec2<f32>(source.g, row)).rgb;
        pbr_input.material.emissive = vec4<f32>(colour * effect.scale.x, pbr_input.material.emissive.a);
    }
    if (effect.flags_and_rows.y > 0.5) {
        let alpha = textureSample(
            palette_texture,
            palette_sampler,
            vec2<f32>(source.a, effect.flags_and_rows.w),
        ).a;
        pbr_input.material.base_color.a = alpha;
    }
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
