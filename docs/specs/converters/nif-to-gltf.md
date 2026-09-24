# NIF to glTF 2.0 / GLB Transformation Specification

This document details the technical specification for converting Bethesda NetImmerse (`.nif`) 3D mesh files into modern, GPU-ready **glTF 2.0 (`.glb`)** binary files.

---

## 1. Overview & Objectives

- **Input:** Skyrim `.nif` file (NiHeader, BSTriShape / NiTriShape, BSLightingShaderProperty).
- **Output:** Standalone glTF 2.0 binary file (`.glb`).
- **Goal:** Convert legacy proprietary 3D geometry into standard PBR-compatible glTF primitives that render zero-copy inside Bevy.

---

## 2. Block Mapping Reference Table

| Skyrim NIF Block Type                            | glTF 2.0 Equivalent     | Conversion Logic                                                                  |
| :----------------------------------------------- | :---------------------- | :-------------------------------------------------------------------------------- |
| **`NiHeader`**                                   | `asset` metadata        | Copy generator & version tags                                                     |
| **`NiNode` / `BSFadeNode`**                      | `nodes`                 | Convert local transform matrix (`translation`, `rotation` quaternion, `scale`)    |
| **`BSTriShape` / `NiTriShape`**                  | `meshes` + `primitives` | Extract vertex positions, normals, UVs, tangents, and index buffers               |
| **`BSLightingShaderProperty`**                   | `materials`             | Map Bethesda shader flags to glTF PBR Metallic Roughness properties               |
| **`BSShaderTextureSet`**                         | `textures` + `images`   | Map Skyrim texture slots (`_d.dds`, `_n.dds`, `_s.dds`) to glTF URIs/KTX2 handles |
| **`NiSkinInstance` / `BSDismemberSkinInstance`** | `skins`                 | Map bone indices (`JOINTS_0`) and vertex weights (`WEIGHTS_0`)                    |

---

## 3. Detailed Data Extraction Steps

```
┌──────────────────────┐
│  Skyrim NIF File     │
└──────────┬───────────┘
           │
           ▼ (Binary Reader / nom)
┌─────────────────────────────────────────────────────────────────────────────┐
│ 1. Extract Geometry Buffers (BSTriShape)                                    │
│    - Positions:  Vec3<f32>  ➔ glTF Accessor "POSITION"                     │
│    - UV Map:     Vec2<f32>  ➔ glTF Accessor "TEXCOORD_0"                   │
│    - Normals:    Vec3<f32>  ➔ glTF Accessor "NORMAL"                       │
│    - Tangents:   Vec4<f32>  ➔ glTF Accessor "TANGENT"                      │
│    - Indices:    u16 / u32  ➔ glTF Accessor "ELEMENT_ARRAY_BUFFER"         │
└──────────────────────────┬──────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ 2. Map Material & Textures (BSLightingShaderProperty)                       │
│    - Slot 0 (Diffuse)       ➔ baseColorTexture                             │
│    - Slot 1 (Normal Map)    ➔ normalTexture                                │
│    - Slot 2 (Subsurface/Env)➔ metallicRoughnessTexture                     │
│    - Alpha Flags            ➔ alphaMode ("OPAQUE" / "MASK" / "BLEND")      │
└──────────────────────────┬──────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ 3. Build & Write glTF 2.0 Binary (.glb)                                     │
│    - Write JSON Chunk (Nodes, Meshes, Materials, Accessors, Views)          │
│    - Write BIN Chunk  (Interleaved Vertex & Index Buffers)                  │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 4. Material Parameter Conversion Matrix

Before glTF publication, OpenSkyrim builds a validated material contract for every reachable shape.
The contract follows the shape's explicit shader, texture-set and alpha-property block references;
block order and filename suffixes are not used to associate or classify materials. Unsupported
properties are recorded as explicit exclusions, while invalid references and non-finite values fail
conversion with the source file, shape block and shader block in the diagnostic.

| Skyrim Shader Feature    | Skyrim Flag / Value                           | glTF PBR Property                                                                                       |
| :----------------------- | :-------------------------------------------- | :------------------------------------------------------------------------------------------------------ |
| **Base Color**           | Diffuse texture (`Slot 0`) + material alpha   | `pbrMetallicRoughness.baseColorTexture` + `baseColorFactor`, interpreted by glTF as sRGB color + alpha |
| **Normal Map**           | Normal texture (`Slot 1`)                     | `normalTexture`, interpreted by glTF as linear data                                                     |
| **Roughness / Specular** | Glossiness value + specular texture (`Slot 7`)| `roughnessFactor = 1.0 - clamp(glossiness / 100.0)` + `KHR_materials_specular`                          |
| **Metallic Factor**      | No validated metalness input in the SSE IR    | Fixed to `0.0`; environment mapping is not misclassified as metalness                                   |
| **Emissive / Glow**      | Glow map (`Slot 2`) or emissive color/strength| `emissiveTexture`, `emissiveFactor` and `KHR_materials_emissive_strength`                               |
| **Two-Sided Rendering**  | `SLSF2_Double_Sided` flag                     | `doubleSided: true` only when the flag is set                                                            |
| **Alpha Transparency**   | `NiAlphaProperty` and alpha-related flags     | `alphaMode: "MASK"` with normalized threshold, or `"BLEND"`                                           |

Height/detail, environment, environment-mask, inner-layer and greyscale slots remain in the
`OPEN_SKYRIM_material` extension because core glTF has no equivalent Skyrim shader semantics.
The extension also records premultiplied-alpha and screen-door-alpha requirements. Texture URIs
always target the canonical KTX2 hierarchy; the semantic DDS-to-KTX2 encoding itself is closed by
the following conversion stage.

### 4.1 Material animation (`OPEN_SKYRIM_material_animation`)

Skyrim animates hearth flames, lava, steam and glow cards by driving one shader variable from a
keyframe controller: a float controller on the shader property's `NiObjectNET` controller reference,
chained to further controllers through `next_controller`. Nothing in glTF animates a material
variable, so the channels are published on the shape's material as their own extension, beside
`OPEN_SKYRIM_material` (which a material with only channels may not have at all):

```json
"extensions": {
  "OPEN_SKYRIM_material_animation": {
    "channels": [{
      "variable": "vOffset",
      "interpolation": "LINEAR" | "QUADRATIC" | "STEP",
      "times": [0.0, 5.6667],
      "values": [0.0, 1.0],
      "tangents": [[0.0, 0.0], [0.0, 0.0]],
      "loop": "cycle" | "reverse" | "clamp",
      "frequency": 1.0, "phase": 0.0, "start": 0.0, "stop": 5.6667
    }]
  }
}
```

* One channel per float controller in the chain, in chain order.
* `variable` is the controlled shader variable in the camelCase spelling of the nif.xml enums.
  Effect shaders (`EffectShaderControlledVariable`): `emissiveMultiple`, `falloffStartAngle`,
  `falloffStopAngle`, `falloffStartOpacity`, `falloffStopOpacity`, `alpha`, `uOffset`, `uScale`,
  `vOffset`, `vScale`. Lighting shaders (`LightingShaderControlledVariable`) share those names for
  the variables the enums share and add `refractionStrength`, `environmentMapScale`, `glossiness`
  and `specularStrength`.
* `interpolation` is the key type: `LINEAR` (1) and `STEP` (5) keys store `(time, value)`;
  `QUADRATIC` (2) keys also store a forward and a backward tangent, published as `tangents`.
* `loop`, `frequency`, `phase`, `start` and `stop` come from the controller's `NiTimeController`
  fields; `loop` is the cycle mode in flags bits 1-2 (0 cycle, 1 reverse, 2 clamp).
* The extension is listed in `extensionsUsed`, never `extensionsRequired`: a consumer that does not
  play it renders the shape's still frame.
* A controller with no interpolator, no float data, an unknown variable, an unsupported key type,
  an unknown cycle mode or non-finite key data is dropped and counted in the mesh stage's
  `NifParseDiagnostics::animation_skipped_channels` (reason → count). Animation never fails a
  conversion.

---

## 5. Rust Implementation (`mesh_tools` Builder Architecture)

We use the **`mesh_tools`** crate (`GltfBuilder`), which provides an incredibly clean, ergonomic API for assembling vertices, normals, UVs, and PBR materials into binary `.glb` files.

```rust
use mesh_tools::GltfBuilder;

pub struct NifToGltfConverter;

impl NifToGltfConverter {
    /// Converts a parsed Skyrim NIF structure into a binary GLB file
    pub fn convert_and_export(nif: &SkyrimNif, output_path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = GltfBuilder::new();

        // 1. Create PBR Material
        let material = builder.add_pbr_material(
            Some("SkyrimMaterial".to_string()),
            Some([1.0, 1.0, 1.0, 1.0]), // Base Color (RGBA)
            Some(nif.material.roughness),
            Some(nif.material.metallic),
        );

        // 2. Add Mesh Primitives (Positions, Normals, UVs, Indices)
        let mesh_index = builder.add_custom_mesh(
            Some("SkyrimMesh".to_string()),
            &nif.positions, // Vec<[f32; 3]>
            &nif.normals,   // Vec<[f32; 3]>
            &nif.uvs,       // Vec<[f32; 2]>
            &nif.indices,   // Vec<u32>
            Some(material),
        );

        // 3. Create Scene Node with Transform
        let node_index = builder.add_node(
            Some("RootNode".to_string()),
            Some(mesh_index),
            Some(nif.translation), // [x, y, z]
            Some(nif.rotation),    // Quaternion [x, y, z, w]
            Some(nif.scale),       // [sx, sy, sz]
        );

        builder.add_scene(
            Some("SkyrimScene".to_string()),
            Some(vec![node_index]),
        );

        // 4. Export binary GLB directly to disk
        builder.export_glb(output_path)?;

        Ok(())
    }
}
```
