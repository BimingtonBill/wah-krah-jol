// The doorway quad of a load-door portal.
//
// The portal camera (`crates/engine/src/portal.rs`) renders the destination with the main camera's
// own projection, carried onto the arrival frame by a rigid transform, so a destination point and
// the source point it corresponds to project to the same pixel. A fragment of this quad therefore
// samples the portal render target at its own position in the frame - the screen-space UVs of
// `water.wgsl` - and the doorway becomes a window instead of a picture: the image lines up with the
// room around it and keeps lining up as the player moves, whatever the quad's own orientation.
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

@fragment
fn fragment(in: VertexOutput) -> FragmentOutput {
    let viewport_uv = (in.position.xy - view.viewport.xy) / view.viewport.zw;
    let uv = clamp(viewport_uv, vec2<f32>(0.0), vec2<f32>(1.0));
    var out: FragmentOutput;
    out.color = vec4<f32>(textureSample(portal_texture, portal_sampler, uv).rgb, 1.0);
    return out;
}
