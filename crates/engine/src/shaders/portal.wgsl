// The doorway quad of a load-door portal.
//
// The portal camera (`crates/engine/src/portal.rs`) renders the destination with the main camera's
// own projection, carried onto the arrival frame by a rigid transform, so a destination point and
// the source point it corresponds to project to the same pixel. A fragment of this quad therefore
// samples the portal render target at its own position in the frame - the screen-space UVs of
// `water.wgsl` - and the doorway becomes a window instead of a picture: the image lines up with the
// room around it and keeps lining up as the player moves, whatever the quad's own orientation.
//
// The target does not hold the whole main view, only the rectangle of it the doorway covers: the
// portal camera projects that rectangle alone (a sub-view of the main camera's frustum) into a
// target of its size, so a texel of the target is a pixel of the window. `doorway_rect` is that
// rectangle in window pixels (corner, then size), and a fragment samples the target at its own
// position within it. A zero rectangle - no doorway measured yet - is the whole main view.
//
// The quad is not mirrored (the door -> arrival map is a rotation), so unlike the water reflection
// the sample is not flipped vertically: uv.y grows downward in both the framebuffer and a Bevy
// render target's first texel row.
//
// The sampled color is display-referred and unlit: the portal camera does not tonemap its target
// (`Tonemapping::None`), and the main camera's tonemapper finishes the doorway exactly as it
// finishes the room around it.

#import bevy_pbr::forward_io::{VertexOutput, FragmentOutput}
#import bevy_pbr::mesh_view_bindings::view

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var portal_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var portal_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var<uniform> doorway_rect: vec4<f32>;

#ifdef PORTAL_DEPTH_COMPOSITE
// The depth composite (impl-211, `--portal-depth-composite`, a spike): the quad writes the depth
// of what the portal camera drew at this pixel instead of its own plane's, so the source geometry
// just behind the doorway plane - a jamb, the underside of a lintel - occludes the destination by
// depth wherever it is nearer, at whatever shape the opening really has.
//
// `portal_depth` is a copy of the portal camera's own depth buffer (reverse-Z, taken after its
// main pass), and `portal_view_from_clip` the inverse of the clip matrix it rendered with: its
// sub-view of the doorway rectangle, with the oblique near plane at the destination doorway. The
// oblique plane rewrites clip z, so the stored depth is not the main view's; it is turned back
// into a view-space point and projected again with the main camera's own `clip_from_view`. The
// portal camera is the main camera carried through the rigid door map, so the two view spaces are
// the same space.
//
// `composite.x` is the slab: the written depth is never further than `slab` units (along the view
// axis) behind the quad's own plane. Without it, anything of the source space behind the doorway
// - the far side of the house's shell, a tree behind it - that is nearer than the destination's
// back wall would show through the doorway. With it, only what stands within the slab of the
// opening (the frame's own depth) can.
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var portal_depth: texture_depth_2d;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var<uniform> portal_view_from_clip: mat4x4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(105) var<uniform> composite: vec4<f32>;

struct CompositeOutput {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

// The main view's depth of a point `distance` units in front of the eye along the view axis.
fn main_view_depth(distance: f32) -> f32 {
    let clip = view.clip_from_view * vec4<f32>(0.0, 0.0, -distance, 1.0);
    return clip.z / clip.w;
}

@fragment
fn fragment(in: VertexOutput) -> CompositeOutput {
    var rect = vec4<f32>(0.0, 0.0, view.viewport.zw);
    if (doorway_rect.z > 0.0 && doorway_rect.w > 0.0) {
        rect = doorway_rect;
    }
    let rect_uv = (in.position.xy - view.viewport.xy - rect.xy) / rect.zw;
    let uv = clamp(rect_uv, vec2<f32>(0.0), vec2<f32>(1.0));
    var out: CompositeOutput;
    out.color = vec4<f32>(textureSample(portal_texture, portal_sampler, uv).rgb, 1.0);

    // The quad's own distance along the view axis, and the furthest the composite may push it.
    let quad_distance = -(view.view_from_world * vec4<f32>(in.world_position.xyz, 1.0)).z;
    let slab_limit = quad_distance + composite.x;
    var distance = slab_limit;
    let size = vec2<i32>(textureDimensions(portal_depth));
    let texel = clamp(vec2<i32>(uv * vec2<f32>(size)), vec2<i32>(0), size - vec2<i32>(1));
    let stored = textureLoad(portal_depth, texel, 0);
    // Zero is the cleared value (reverse-Z far): the destination's sky, or nothing at all.
    if (stored > 0.0) {
        let ndc = vec4<f32>(rect_uv.x * 2.0 - 1.0, 1.0 - rect_uv.y * 2.0, stored, 1.0);
        let point = portal_view_from_clip * ndc;
        distance = min(-point.z / point.w, slab_limit);
    }
    // Never in front of the doorway plane: what the oblique plane clipped away cannot come back.
    distance = max(distance, quad_distance);
    out.depth = main_view_depth(distance);
    return out;
}
#else
@fragment
fn fragment(in: VertexOutput) -> FragmentOutput {
    var rect = vec4<f32>(0.0, 0.0, view.viewport.zw);
    if (doorway_rect.z > 0.0 && doorway_rect.w > 0.0) {
        rect = doorway_rect;
    }
    let rect_uv = (in.position.xy - view.viewport.xy - rect.xy) / rect.zw;
    let uv = clamp(rect_uv, vec2<f32>(0.0), vec2<f32>(1.0));
    var out: FragmentOutput;
    out.color = vec4<f32>(textureSample(portal_texture, portal_sampler, uv).rgb, 1.0);
    return out;
}
#endif
