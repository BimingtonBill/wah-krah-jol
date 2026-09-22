#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
}

// Samples per overlay weight grid and how they are packed into `weights`: 17x17 samples, four to
// a vec4, padded to `73` vec4s per overlay so every overlay starts on a 16-byte boundary.
const WEIGHT_GRID_SIDE: u32 = 17u;
const WEIGHT_GRID_WORDS: u32 = 73u;

struct TerrainSettings {
    tiling_and_layer_count: vec4<f32>,
    // xy = this quadrant's origin in cell units (0 or 1 per axis), from which `in.uv * 2.0` gives
    // the quadrant-local coordinate in `[0, 1]` exactly, with no wrap at the quadrant's far edge.
    quadrant_origin: vec4<f32>,
    fallback_weights_0: vec4<f32>,
    fallback_weights_1: vec4<f32>,
    // x > 0.5 when `weights` holds this quadrant's overlay grids (streamed cells); otherwise the
    // packed vertex attributes above are the only source (the synthetic fixtures).
    weight_source: vec4<f32>,
    weights: array<vec4<f32>, 365>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var layer_0: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var layer_0_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var layer_1: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var layer_1_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var layer_2: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(105) var layer_2_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(106) var layer_3: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(107) var layer_3_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(108) var layer_4: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(109) var layer_4_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(110) var layer_5: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(111) var layer_5_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(112) var<uniform> terrain: TerrainSettings;

/// One sample of an overlay's weight grid. Sample order is the one LAND's `VTXT` entries use:
/// `y * 17 + x` on the quadrant's own 17x17 grid, 128 units apart.
fn packed_weight(overlay: u32, sample: u32) -> f32 {
    return terrain.weights[overlay * WEIGHT_GRID_WORDS + sample / 4u][sample % 4u];
}

/// One overlay's opacity at a quadrant-local coordinate: bilinear across the sample grid, clamped
/// at the quadrant's edge. This is the interpolation Skyrim applies to `VTXT` opacities, and it is
/// linear between samples - the packed vertex attributes it replaces lost weight to Bevy's
/// normalization of the tangent, which sharpened every transition into a step.
fn grid_weight(overlay: u32, coordinate: vec2<f32>) -> f32 {
    let corner = floor(coordinate);
    let blend = coordinate - corner;
    let base = vec2<u32>(clamp(corner, vec2<f32>(0.0), vec2<f32>(f32(WEIGHT_GRID_SIDE - 1u))));
    let next = min(base + vec2<u32>(1u), vec2<u32>(WEIGHT_GRID_SIDE - 1u));
    let west = base.x;
    let east = next.x;
    let north = base.y;
    let south = next.y;
    let top = mix(
        packed_weight(overlay, north * WEIGHT_GRID_SIDE + west),
        packed_weight(overlay, north * WEIGHT_GRID_SIDE + east),
        blend.x,
    );
    let bottom = mix(
        packed_weight(overlay, south * WEIGHT_GRID_SIDE + west),
        packed_weight(overlay, south * WEIGHT_GRID_SIDE + east),
        blend.x,
    );
    return mix(top, bottom, blend.y);
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    var weights = array<f32, 6>(
        terrain.fallback_weights_0.x,
        terrain.fallback_weights_0.y,
        terrain.fallback_weights_0.z,
        terrain.fallback_weights_0.w,
        terrain.fallback_weights_1.x,
        terrain.fallback_weights_1.y,
    );
#ifdef VERTEX_TANGENTS
    // The packed-tangent fallback, for materials that carry no weight field (the synthetic
    // fixtures). Weights 1-3 travel as a unit direction plus its magnitude, and Bevy re-normalizes
    // the direction between the vertex and fragment stages, so the three weights shrink towards
    // zero mid-edge - see `build_terrain_quadrant_mesh`.
    let packed_length = abs(in.world_tangent.w);
    weights[1] = max(0.0, in.world_tangent.x * packed_length);
    weights[2] = max(0.0, in.world_tangent.y * packed_length);
    weights[3] = max(0.0, in.world_tangent.z * packed_length);
#endif
#ifdef VERTEX_UVS_B
    weights[4] = in.uv_b.x;
    weights[5] = in.uv_b.y;
#endif
    if terrain.weight_source.x > 0.5 {
        let coordinate = (in.uv * 2.0 - terrain.quadrant_origin.xy) * f32(WEIGHT_GRID_SIDE - 1u);
        weights[1] = grid_weight(0u, coordinate);
        weights[2] = grid_weight(1u, coordinate);
        weights[3] = grid_weight(2u, coordinate);
        weights[4] = grid_weight(3u, coordinate);
        weights[5] = grid_weight(4u, coordinate);
    }
    weights[0] = max(0.0, 1.0 - weights[1] - weights[2] - weights[3] - weights[4] - weights[5]);
    let total = max(0.0001, weights[0] + weights[1] + weights[2] + weights[3] + weights[4] + weights[5]);
    let uv = in.uv * terrain.tiling_and_layer_count.xy;
    var color = textureSample(layer_0, layer_0_sampler, uv) * weights[0];
    color += textureSample(layer_1, layer_1_sampler, uv) * weights[1];
    color += textureSample(layer_2, layer_2_sampler, uv) * weights[2];
    color += textureSample(layer_3, layer_3_sampler, uv) * weights[3];
    color += textureSample(layer_4, layer_4_sampler, uv) * weights[4];
    color += textureSample(layer_5, layer_5_sampler, uv) * weights[5];
    pbr_input.material.base_color *= color / total;
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
