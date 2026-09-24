// Distant-LOD terrain: clipped under the full-detail cells, shaded from its model-space normal map.
// One file serves the main pass and the depth prepass (`PREPASS_PIPELINE`).

#ifdef PREPASS_PIPELINE
#import bevy_pbr::prepass_io::{VertexOutput, FragmentOutput}
#else
#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
}
#endif

struct LodTerrainSettings {
    // x, y: the clip window's south-west cell, relative to the render origin; z: 1 when
    // `model_space_normal` is bound.
    window: vec4<i32>,
    // One bit per window cell: bit x of row y is `mask[y / 4][y % 4] >> x`.
    mask: array<vec4<u32>, 8>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> lod: LodTerrainSettings;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var model_space_normal: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var model_space_normal_sampler: sampler;

const CELL_SIZE: f32 = 4096.0;
const CLIP_WINDOW: i32 = 32;

// Whether a full-detail cell's terrain covers this render-space point. Cells run east along +X
// and north along -Z from the render origin, as `camera_cell` reads them.
fn clipped(world: vec3<f32>) -> bool {
    let cell = vec2<i32>(floor(vec2<f32>(world.x, -world.z) / CELL_SIZE)) - lod.window.xy;
    if any(cell < vec2<i32>(0)) || any(cell >= vec2<i32>(CLIP_WINDOW)) {
        return false;
    }
    let row = lod.mask[cell.y / 4][cell.y % 4];
    return ((row >> u32(cell.x)) & 1u) != 0u;
}

// The block's normal in render space. Skyrim stores a model-space normal swizzled - red +X,
// green +Z (up), blue +Y (north) - which NifSkope's `sk_msn.frag` reads as `normal.rbg`. The
// block's Creation-to-glTF basis turns Creation (x, y, z) into render (x, z, -y), and a block
// root is only translated and uniformly scaled, so the texel maps to (r, g, -b).
fn block_normal(uv: vec2<f32>) -> vec3<f32> {
    let texel = textureSample(model_space_normal, model_space_normal_sampler, uv).rgb * 2.0 - 1.0;
    return normalize(vec3<f32>(texel.r, texel.g, -texel.b));
}

#ifdef PREPASS_PIPELINE
#ifdef PREPASS_FRAGMENT
@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    if clipped(in.world_position.xyz) {
        discard;
    }
    var out: FragmentOutput;
#ifdef NORMAL_PREPASS
    // A terrain block has no vertex normals, so `in.world_normal` is empty there: the normal
    // prepass gets the same model-space normal the main pass shades with.
    var normal = in.world_normal;
#ifdef VERTEX_UVS_A
    if lod.window.z != 0 {
        normal = block_normal(in.uv);
    }
#endif
    out.normal = vec4<f32>(normal * 0.5 + vec3<f32>(0.5), 1.0);
#endif
#ifdef UNCLIPPED_DEPTH_ORTHO_EMULATION
    out.frag_depth = in.unclipped_depth;
#endif
    return out;
}
#else
@fragment
fn fragment(in: VertexOutput) {
    if clipped(in.world_position.xyz) {
        discard;
    }
}
#endif // PREPASS_FRAGMENT
#else
@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    if clipped(in.world_position.xyz) {
        discard;
    }
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    // The base is alpha-masked only so the prepass runs this clip; the block itself is opaque.
    pbr_input.material.base_color.a = 1.0;
#ifdef VERTEX_UVS_A
    if lod.window.z != 0 {
        let normal = block_normal(in.uv);
        pbr_input.N = normal;
        pbr_input.world_normal = normal;
    }
#endif
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
#endif // PREPASS_PIPELINE
