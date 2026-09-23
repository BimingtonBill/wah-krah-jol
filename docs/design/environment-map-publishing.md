# Environment maps: what the converter publishes, and what the engine must do

**Source:** `research-077-environment-map-design.1` (design), corrected by
`research-076-converter-dropped-fields.1`'s addendum and reviewed by `lead-070-converter-materials.1`.
This is the contract for the oldest open visual gap in the project ("ice reads as rock",
`docs/research/visual-gaps-spec.md` gap 3). It stops at the converter boundary: everything under
"the engine's half" is a specification for an engine task, not a claim that it exists.

## The gap in one paragraph

Skyrim gives an environment-mapped material a cube map, a mask and a scale. The converter already
publishes the cube as a real KTX2 cubemap (`texture.rs` assembles the six DDS faces and patches
`face_count = 6`) and lists it in `OPEN_SKYRIM_material.textureSlots` - **7,823 materials carry an
`environment_cube` slot, 3,096 an `environment_mask`, 655 an `inner_layer` (measured)** - but the
engine reads none of it (`render.rs:602` and `:698-723` are the only consumers of that extension),
so ice, Dwemer bronze and the DLC1 icebergs render as flat matte rock. Metalness stays `0.0` until
the cube is bound (research-058 §5.1).

## What Skyrim does (documented, from the vendored nif.xml text)

| source | says |
|---|---|
| `vendor/.../nif_enum.rs:1795` | shader type 1 *Environment Map*: "Enables EnvMap Mask(TS6), EnvMap Scale" |
| `nif_enum.rs:1805` | type 11 *MultiLayer Parallax*: "EnvMap Mask(TS6), Layer(TS7), Parallax Layer Thickness, Parallax Refraction Scale, Parallax Inner Layer U/V Scale, EnvMap Scale" |
| `nif_enum.rs:1810` | type 16 *Eye Envmap*: "EnvMap Mask(TS6), Eye EnvMap Scale" |
| `nif_flags.rs:45` | `SLSF1_ENVIRONMENT_MAPPING` (bit 7): "Environment mapping (uses Envmap Scale)" |
| `nif_enum.rs:1930`, `:1951` | `LightingShaderControlledFloat::EnvironmentMapScale = 8` - an *animatable float of this block* |

So the cube is a **reflection added to the surface, gated by a mask, scaled by an exposure-like
scalar**: slot 4 (0-based) is the cube, slot 5 is the mask (`environment_mask` in the published
extension), slot 6 is the MLP inner layer, and slot 0 stays the base colour - the cube is not the
colour source. *Not established anywhere the project can reach:* whether the sample direction is
perturbed by the normal map, and whether the mask multiplies the reflection, the specular term, or
both; and where "Envmap Scale" sits in the block (see the open question below).

## The published contract (converter side)

**Keep `OPEN_SKYRIM_material.textureSlots` as the one place a material's environment binding lives.**
No new key: the cube and mask are already there with their real image indices and semantics, and the
wrapper already carries `lightingShaderType` and `shaderFlags1/2`. Two one-line fixes make the
label, the URI and the file agree (today the cube is published with the sRGB transfer function but
labelled `"colorSpace":"linear"` and registered with `is_srgb = false`):

| # | change | where |
|---|---|---|
| 1 | `is_srgb = matches!(semantic, Detail \| EnvironmentCube)` for `registry.texture(...)` | `crates/converter/src/material.rs:694-698` |
| 2 | `"colorSpace"` is `"srgb"` for those same semantics, `"linear"` otherwise | `crates/converter/src/material.rs:700` |

After this the engine has one rule: **`is_srgb = (colorSpace == "srgb")`**, and a cube that is also
used as a non-colour semantic gets both URIs (`<cube>.ktx2` and the existing
`<cube>.opensky-srgb.ktx2` alias, published by `pipeline.rs:902-925`).

Example, Dwemer roof (`dwefacadetowerroof01.glb`, material `DweFacadeTowerRoof01:6`) - only the bold
part changes:

```json
"OPEN_SKYRIM_material": {"shaderFamily":"lighting","lightingShaderType":"environment_map",
  "shaderFlags1":2185233281,"shaderFlags2":32801,
  "textureSlots":[{"slot":4,"semantic":"environment_cube","texture":2,
                   "required":true,**"colorSpace":"srgb"**}]}
```
with `images[2].uri` ending `bronze_e.opensky-srgb.ktx2`. Example, the real env-mapped ice material
(`caveirhalldoor01.glb`, `CaveIRHallDoor01:2`, shader type `multi_layer_parallax`): slot 4 →
`shinydull_e` cube, slot 6 → `inner_layer`; the ice tint stays where it is, in
`KHR_materials_specular.specularColorTexture`.

**Do not publish in this batch:**

- **The environment-map scale.** It is reachable - `lighting_effect_1`/`lighting_effect_2` are
  already parsed (`vendor/.../nif_block.rs:701-702`) and handed to `build_lighting_material`; the
  converter just never copies them - but which of the two is the scale is documented, not measured,
  and Bevy has no per-material environment intensity to spend it on. Publishing a number the engine
  multiplies by nothing, with no way to check it, is a defect of exactly the kind the audit lists.
  Measure first (below), then decide.
- **The environment mask anywhere new.** Bevy's `StandardMaterial` has no env-mask input, and the
  one slot that could fake it (`KHR_materials_specular.specularTexture`, alpha-sampled) is committed
  to the normal map's alpha (impl-064).
- **Metalness.** Unchanged: `0.0` until the engine binds the cube.

**Schema:** the change is a corrected label and an alias URI - no new key and no changed meaning -
but it changes GLB bytes, so it rides the batch's 18 -> 19 bump (`cache.rs:11`, `:67`,
`pipeline.rs:92`); it does not earn a bump of its own.

## The engine's half (Bevy 0.19 only)

1. **Load** the cube from the extension: `textureSlots[semantic == "environment_cube"].texture` is an
   index into the GLB's `textures`. Load it **with settings**, not plain `load`:
   `is_srgb = colorSpace == "srgb"`, because Bevy's KTX2 loader picks the format from the *settings*
   (`bevy_image-0.19.0/src/ktx2.rs:1204-1327`), not from the file's own flag.
2. **Bind** `GeneratedEnvironmentMapLight` (`bevy::pbr::generate`), **not** `EnvironmentMapLight`:
   Skyrim's cube is a plain mip chain, not a Lambertian+GGX-filtered pair, so Bevy must filter it at
   runtime (`EnvironmentMapGenerationPlugin` must be added by the app - `PbrPlugin` does not add it).
   `generate_environment_map_light` inserts the matching `EnvironmentMapLight` itself.
3. **Where.** Binding is per view or per light probe, never per material. The cheap version is one
   `GeneratedEnvironmentMapLight` per camera, chosen for the active space beside the atmosphere
   update; the faithful version groups references by cube URI and uses one `LightProbe` per group -
   **at most 8 probes are considered per view** (`light_probe/mod.rs:52`).
4. **Image requirements** (all already true of the published cubes): `faceCount == 6`,
   `layerCount == 0`, at least one mip level (Bevy reads only level 0 and builds its own chain),
   UASTC is fine, and the cube must be **square, power-of-two and <= 8192 or Bevy panics**
   (`bevy_pbr-0.19.0/src/light_probe/generate.rs:1032-1041`). A converter-side guard for that is a
   reasonable follow-up; it is not in this batch because a malformed cube should not fail an
   18-minute conversion until the engine can actually bind one.
5. **Rotation and intensity are fits, not data**: the engine is Y-up and Creation is Z-up, so the
   cube needs approximately `Quat::from_rotation_x(-PI/2)`; the sign is verifiable in one frame (the
   "up" in the ice's reflection must be the ceiling, not the floor). Intensity is a constant to fit
   like `LIGHT_EXPOSURE`, against the UESP reference shots, with the ice and Dwemer poses held out
   from tuning.

## Holdout check (after a reconversion)

| glb | must be true |
|---|---|
| `meshes/dungeons/dwemer/facades/dwefacadetowerroof01.glb` | slot 4 publishes `"colorSpace":"srgb"`, `images[2].uri` ends `bronze_e.opensky-srgb.ktx2`, and that alias exists and is byte-identical to `bronze_e.ktx2` |
| `meshes/dungeons/caves/ice/largehall/caveirhalldoor01.glb` | `CaveIRHallDoor01:2/:5` keep slot 4 → cube and slot 6 → inner layer, now sRGB-labelled |
| `.../ice/largewall/caveilwallstraight01.glb`, `caveilfloor01.glb`, `caveilceiling01.glb`, `.../ice/pillars/caveilpillar01.glb`, `.../ice/largeroom/caveilroomwall01.glb` | **no** cube appears: the fix must not invent one where the NIF has none (these are the negative controls - most of the ice set is *not* env-mapped, only the doors are) |
| every material with `shaderFlags1 & 0x80` | carries slot 4 |
| render: the Alftand ice hallway, `--demo alftand --walk` | ice stops reading as flat grey; the interior's p50 moves by a few percent, not double (a doubling means a colour-space mismatch) |

## Open questions

- **Which float is the env-map scale, and does a shader-type trailer exist?** `lighting_effect_1/2`
  are the last two floats of the block. `research-076`'s layout says the payload is `100 + 4·extra`
  bytes with alpha at +64, `research-077`'s says 97 with alpha at +61 (`TexClampMode` u32 vs u8);
  published data supports `100 + 4·extra` (a `roughnessFactor` back-solving to exactly glossiness
  400.0, alphas exactly 1.0). **The measurement that closes both questions:** dump one
  `BSLightingShaderProperty`'s declared block size and the two trailing floats for ~20 env-mapped
  NIFs and ~20 controls. If the pair is constant across both sets, publishing it would publish a
  constant; if a block exceeds `100 + 4·extra`, a trailer exists and the scale is probably in it.
- **Is the cube sampled sRGB by Skyrim?** Unproven; the converter already treats it as a colour
  semantic, so the recommendation only removes the contradiction. If a measurement later says
  linear, move `EnvironmentCube` out of the colour set in `texture.rs:44-53` and the label follows.
- **The MLP inner layer is published linear** (`InnerLayer` → `DataLinear`) while it is a colour map
  in the game - if the ice reads too dark once the cube is bound, that is the first thing to check.
