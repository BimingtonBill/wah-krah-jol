// Directional snow: Skyrim's projected material on a snow-covered static, as `crates/engine/src/
// snow.rs` models it. The formula is documented there in full, with the reasons for each parameter;
// `snow_coverage` below is its mirror and the two have to be changed together.
//
// The base material is untouched apart from its colour: the mesh keeps the texture, the normal map
// and the alpha mode the converter and the glTF material handler gave it, and the snow is mixed
// into the base colour by how much of the surface faces the fall axis.

#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
}

struct SnowSettings {
    // xyz: the axis the snow falls along (-dir_proj), unit length, world space.
    // w:   cos(max angle) - the half-angle of the cone the material covers.
    axis_and_cos_max: vec4<f32>,
    // x: falloff scale, y: falloff bias, z: normal dampener, w: the record's single-pass flag.
    falloff_and_dampener: vec4<f32>,
    // The projected material's colour, linear.
    color: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> snow: SnowSettings;

// The surface normal pulled `dampener` of the way towards the fall axis, unit length. Two inputs
// have no direction to normalise and both fall back to the axis, which reads as a fully covered
// surface: a dampener of 1 against a normal pointing straight down the axis cancels exactly, and a
// degenerate or non-finite normal is not a direction at all. Leaving a division through would put a
// NaN in the colour mix, and a NaN base colour is a whole surface of undefined colour.
fn dampened_normal(normal: vec3<f32>, up: vec3<f32>, dampener: f32) -> vec3<f32> {
    let raw = normal + up * max(dampener, 0.0);
    let length_raw = length(raw);
    // `raw - raw` is zero for a finite value and NaN for a NaN or an infinity, which is the exact
    // finiteness test WGSL has no builtin for - and the one `dampened_normal` in `snow.rs` makes.
    // It has to be the first thing the condition checks: a NaN slips through any `<=` test, so
    // `length_raw > 1e-6` on its own would let one into the division.
    let difference = raw - raw;
    let finite = difference.x == 0.0 && difference.y == 0.0 && difference.z == 0.0;
    return select(up, raw / max(length_raw, 1e-6), finite && length_raw > 1e-6);
}

// 0 on a bare surface, 1 where the projected material covers it completely.
fn snow_coverage(normal: vec3<f32>) -> f32 {
    let up = normalize(snow.axis_and_cos_max.xyz);
    let damped = dampened_normal(normal, up, snow.falloff_and_dampener.z);
    let cos_tilt = dot(damped, up);
    let cos_max = snow.axis_and_cos_max.w;
    let window = 1.0 - cos_max;
    if (window <= 1e-6) {
        return 0.0;
    }
    let drive = clamp((cos_tilt - cos_max) / window, 0.0, 1.0);
    let scale = snow.falloff_and_dampener.x;
    if (scale <= 1e-6) {
        return select(0.0, 1.0, drive >= snow.falloff_and_dampener.y);
    }
    return clamp((drive - snow.falloff_and_dampener.y) / scale, 0.0, 1.0);
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    let bare_coverage = snow_coverage(pbr_input.world_normal);
#ifdef VERTEX_COLORS
    // The art's own per-vertex coverage, where the mesh carries one. Converted meshes do carry
    // vertex colours - 1,337 models publish `COLOR_0` - but their alpha is a Skyrim shader
    // parameter and not opacity (`docs/research/foliage-alpha-test.md`), so the engine forces it to
    // 1.0 as the mesh loads (`force_opaque_vertex_colours` in `crates/engine/src/render.rs`,
    // impl-063) and this product is the identity for every streamed mesh. The multiply stays for a
    // mesh whose vertex alpha really is coverage, and it is behind the same `#ifdef` Bevy uses.
    let coverage = bare_coverage * in.color.a;
#else
    let coverage = bare_coverage;
#endif
    let base = pbr_input.material.base_color;
    pbr_input.material.base_color = vec4<f32>(
        mix(base.rgb, snow.color.rgb, coverage),
        base.a,
    );
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
