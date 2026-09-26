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
    // x: the colour's multiplier (base colour scale times the engine's effect exposure),
    // y: 1 when the card blends additively.
    scale: vec4<f32>,
    // xy: the source texture's offset, zw: its scale, which an animated effect moves each frame.
    uv_offset_scale: vec4<f32>,
    // Use_Falloff: x start, y stop (cosines of the angle to the view), z start opacity,
    // w stop opacity; x == y turns it off.
    falloff: vec4<f32>,
}

// A palette is a lookup table: Skyrim samples it clamped. The glTF names no sampler, so the image
// takes glTF's default Repeat wrap, and a lookup at column 0 or row 0 would blend in the far edge
// (column 255's near-white alpha: a sheet over the whole card; row 63: a blue band at the tips).
// Clamping to the texel centres makes the wrap irrelevant, and level 0 avoids the mip halo that
// derivative-picked mips give where the source's alpha jumps.
fn palette_lookup(u: f32, v: f32) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(palette_texture, 0));
    let lo = vec2<f32>(0.5) / size;
    let uv = clamp(vec2<f32>(u, v), lo, vec2<f32>(1.0) - lo);
    return textureSampleLevel(palette_texture, palette_sampler, uv, 0.0);
}

// Effect.hlsl's falloff: smoothstep of |N.V| from start to stop, from the start to the stop opacity.
fn falloff_opacity(n: vec3<f32>, v: vec3<f32>) -> f32 {
    let span = effect.falloff.y - effect.falloff.x;
    if (abs(span) < 1e-6) {
        return 1.0;
    }
    let f = saturate((abs(dot(normalize(n), normalize(v))) - effect.falloff.x) / span);
    return mix(effect.falloff.z, effect.falloff.w, f * f * (3.0 - 2.0 * f));
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
    let source_uv = in.uv * effect.uv_offset_scale.zw + effect.uv_offset_scale.xy;
    let source = textureSample(source_texture, source_sampler, source_uv);
#else
    let source = vec4<f32>(1.0);
#endif
    var row = effect.flags_and_rows.z;
#ifdef VERTEX_COLORS
    row = row * in.color.r;
#endif
    if (effect.flags_and_rows.x > 0.5) {
        let colour = palette_lookup(source.g, row).rgb;
        pbr_input.material.emissive = vec4<f32>(colour * effect.scale.x, pbr_input.material.emissive.a);
    }
    if (effect.flags_and_rows.y > 0.5) {
        // The alpha row is the base alpha times the vertex alpha (Community Shaders' Effect.hlsl:
        // the vertex alpha picks the row; the texture's own alpha is the column only).
        var alpha_row = effect.flags_and_rows.w * falloff_opacity(pbr_input.world_normal, pbr_input.V);
#ifdef VERTEX_COLORS
        alpha_row = alpha_row * in.color.a;
#endif
        let alpha = palette_lookup(source.a, alpha_row).a;
        pbr_input.material.base_color.a = alpha;
    } else {
        pbr_input.material.base_color.a = pbr_input.material.base_color.a
            * falloff_opacity(pbr_input.world_normal, pbr_input.V);
    }
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);
    var out: FragmentOutput;
    let lit = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, lit);
    if (effect.scale.y > 0.5) {
        // Skyrim dims an additive effect toward black in fog (`lightColor * (1 - fog)`), where
        // Bevy's fog mixes it toward the fog colour, which added a fog-coloured sheet. Fog is
        // linear in the colour, so taking away what black would receive leaves `colour * (1 - fog)`.
        let fog_only = main_pass_post_lighting_processing(pbr_input, vec4<f32>(vec3<f32>(0.0), lit.a));
        out.color = vec4<f32>(max(out.color.rgb - fog_only.rgb, vec3<f32>(0.0)), out.color.a);
    }
    return out;
}
