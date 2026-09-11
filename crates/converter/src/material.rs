use crate::asset_path::{AssetKind, canonical_asset_path};
use color_eyre::{Result, eyre::ensure};
use project_wormhole_nif::{
    bs::prelude::{BSShaderTextureSet, BSTriShape},
    nif_block::{BSEffectShaderProperty, BSLightingShaderProperty, NiAlphaProperty, NifBlock},
    nif_file::NifFile,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

const NULL_BLOCK: u32 = u32::MAX;
const SLSF1_ENVIRONMENT_MAPPING: u32 = 1 << 7;
const SLSF1_OWN_EMIT: u32 = 1 << 22;
const SLSF2_DOUBLE_SIDED: u32 = 1 << 4;
const SLSF2_GLOW_MAP: u32 = 1 << 6;
const ALPHA_ROUNDING_TOLERANCE: f32 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NifShaderFamily {
    Lighting,
    Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightingShaderType {
    Default,
    EnvironmentMap,
    Glow,
    Parallax,
    FaceTint,
    SkinTint,
    HairTint,
    ParallaxOcclusion,
    MultitextureLandscape,
    LodLandscape,
    Snow,
    MultiLayerParallax,
    TreeAnimation,
    LodObjects,
    SparkleSnow,
    LodObjectsHd,
    EyeEnvironmentMap,
    Cloud,
    LodLandscapeNoise,
    MultitextureLandscapeLodBlend,
    Fo4Dismemberment,
}

impl TryFrom<u32> for LightingShaderType {
    type Error = color_eyre::Report;

    fn try_from(value: u32) -> Result<Self> {
        Ok(match value {
            0 => Self::Default,
            1 => Self::EnvironmentMap,
            2 => Self::Glow,
            3 => Self::Parallax,
            4 => Self::FaceTint,
            5 => Self::SkinTint,
            6 => Self::HairTint,
            7 => Self::ParallaxOcclusion,
            8 => Self::MultitextureLandscape,
            9 => Self::LodLandscape,
            10 => Self::Snow,
            11 => Self::MultiLayerParallax,
            12 => Self::TreeAnimation,
            13 => Self::LodObjects,
            14 => Self::SparkleSnow,
            15 => Self::LodObjectsHd,
            16 => Self::EyeEnvironmentMap,
            17 => Self::Cloud,
            18 => Self::LodLandscapeNoise,
            19 => Self::MultitextureLandscapeLodBlend,
            20 => Self::Fo4Dismemberment,
            _ => {
                return Err(color_eyre::eyre::eyre!(
                    "unknown lighting shader type {value}"
                ));
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NifTextureSemantic {
    Diffuse,
    Normal,
    Glow,
    Height,
    Detail,
    Specular,
    EnvironmentCube,
    EnvironmentMask,
    InnerLayer,
    Greyscale,
    Unclassified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NifTextureSlot {
    pub slot: u8,
    pub semantic: NifTextureSemantic,
    pub path: String,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NifAlphaMode {
    Opaque,
    Cutout,
    Blend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedNifMaterial {
    pub shader_family: NifShaderFamily,
    pub lighting_shader_type: Option<LightingShaderType>,
    pub shader_block: u32,
    pub texture_set_block: Option<u32>,
    pub alpha_property_block: Option<u32>,
    pub shader_flags_1: u32,
    pub shader_flags_2: u32,
    pub base_color: [f32; 4],
    pub alpha: f32,
    pub alpha_mode: NifAlphaMode,
    pub alpha_threshold: Option<u8>,
    pub glossiness: f32,
    pub specular_color: [f32; 3],
    pub specular_strength: f32,
    pub emissive_color: [f32; 3],
    pub emissive_multiple: f32,
    pub double_sided: bool,
    pub textures: Vec<NifTextureSlot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NifMaterialDisposition {
    Validated { material: ValidatedNifMaterial },
    Excluded { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NifShapeMaterial {
    pub shape_block: u32,
    pub shape_name: Option<String>,
    pub shader_property_block: Option<u32>,
    pub alpha_property_block: Option<u32>,
    pub disposition: NifMaterialDisposition,
}

pub fn build_nif_material_contract(nif: &NifFile, source: &Path) -> Result<Vec<NifShapeMaterial>> {
    let mut contract = Vec::new();
    for (index, block) in nif.blocks.iter().enumerate() {
        let shape = modern_shape(block);
        let legacy = match block {
            NifBlock::NiTriShape(shape) => {
                Some((shape.name, shape.shader_property, shape.alpha_property))
            }
            _ => None,
        };
        let Some((name_index, shader_property, alpha_property)) = shape
            .map(|shape| {
                (
                    shape.av.object.name,
                    shape.shader_property,
                    shape.alpha_property,
                )
            })
            .or(legacy)
        else {
            continue;
        };
        let shape_block = u32::try_from(index)?;
        let shape_name = nif
            .header
            .get_string(name_index as usize)
            .ok()
            .map(str::to_owned);
        let disposition = build_shape_material(
            nif,
            source,
            shape_block,
            shape_name.as_deref(),
            shader_property,
            alpha_property,
        )?;
        contract.push(NifShapeMaterial {
            shape_block,
            shape_name,
            shader_property_block: (shader_property != NULL_BLOCK).then_some(shader_property),
            alpha_property_block: (alpha_property != NULL_BLOCK).then_some(alpha_property),
            disposition,
        });
    }
    Ok(contract)
}

fn modern_shape(block: &NifBlock) -> Option<&BSTriShape> {
    match block {
        NifBlock::BSTriShape(shape) => Some(shape),
        NifBlock::BSDynamicTriShape(shape) => Some(&shape.bs_tri_shape),
        NifBlock::BSSubIndexTriShape(shape) => Some(&shape.bs_tri_shape),
        NifBlock::BSLODTriShape(shape) => Some(&shape.bs_tri_shape),
        _ => None,
    }
}

fn build_shape_material(
    nif: &NifFile,
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    shader_reference: u32,
    alpha_reference: u32,
) -> Result<NifMaterialDisposition> {
    if shader_reference == NULL_BLOCK {
        return Ok(NifMaterialDisposition::Excluded {
            reason: "shape has no shader property".to_owned(),
        });
    }
    let shader_index = usize::try_from(shader_reference)?;
    let shader = nif.blocks.get(shader_index).ok_or_else(|| {
        material_error(
            source,
            shape_block,
            shape_name,
            format!("shader reference {shader_reference} is out of range"),
        )
    })?;
    let alpha = resolve_alpha(nif, source, shape_block, shape_name, alpha_reference)?;
    let material = match shader {
        NifBlock::BSLightingShaderProperty(property) => build_lighting_material(
            nif,
            source,
            shape_block,
            shape_name,
            shader_reference,
            property,
            alpha,
        )?,
        NifBlock::BSEffectShaderProperty(property) => build_effect_material(
            source,
            shape_block,
            shape_name,
            shader_reference,
            property,
            alpha,
        )?,
        NifBlock::Unhandled => {
            let kind = nif.header.get_block_type(shader_index).unwrap_or("unknown");
            return Ok(NifMaterialDisposition::Excluded {
                reason: format!("{kind} is outside the static material contract"),
            });
        }
        _ => {
            let kind = nif.header.get_block_type(shader_index).unwrap_or("unknown");
            return Err(material_error(
                source,
                shape_block,
                shape_name,
                format!("shader reference {shader_reference} points to incompatible {kind}"),
            ));
        }
    };
    validate_material(source, shape_block, shape_name, &material)?;
    Ok(NifMaterialDisposition::Validated { material })
}

fn resolve_alpha<'a>(
    nif: &'a NifFile,
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    reference: u32,
) -> Result<Option<(u32, &'a NiAlphaProperty)>> {
    if reference == NULL_BLOCK {
        return Ok(None);
    }
    match nif.blocks.get(reference as usize) {
        Some(NifBlock::NiAlphaProperty(property)) => Ok(Some((reference, property))),
        Some(_) => Err(material_error(
            source,
            shape_block,
            shape_name,
            format!("alpha reference {reference} does not point to NiAlphaProperty"),
        )),
        None => Err(material_error(
            source,
            shape_block,
            shape_name,
            format!("alpha reference {reference} is out of range"),
        )),
    }
}

fn build_lighting_material(
    nif: &NifFile,
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    shader_block: u32,
    property: &BSLightingShaderProperty,
    alpha: Option<(u32, &NiAlphaProperty)>,
) -> Result<ValidatedNifMaterial> {
    let shader_type = LightingShaderType::try_from(property.shader_type).map_err(|error| {
        material_error(
            source,
            shape_block,
            shape_name,
            format!("shader block {shader_block}: {error}"),
        )
    })?;
    let flags_1 = property.shader_flags_1.raw();
    let flags_2 = property.shader_flags_2.raw();
    let material_alpha = normalize_alpha(
        source,
        shape_block,
        shape_name,
        shader_block,
        property.alpha,
    )?;
    let texture_set =
        resolve_texture_set(nif, source, shape_block, shape_name, property.texture_set)?;
    let textures = texture_set
        .map(|(block, set)| {
            lighting_texture_slots(set, shader_type, flags_1, flags_2)
                .map(|slots| (block, slots))
                .map_err(|error| {
                    material_error(
                        source,
                        shape_block,
                        shape_name,
                        format!("shader block {shader_block}: {error:#}"),
                    )
                })
        })
        .transpose()?;
    let (alpha_mode, alpha_threshold, alpha_property_block) = alpha_contract(alpha);
    Ok(ValidatedNifMaterial {
        shader_family: NifShaderFamily::Lighting,
        lighting_shader_type: Some(shader_type),
        shader_block,
        texture_set_block: textures.as_ref().map(|(block, _)| *block),
        alpha_property_block,
        shader_flags_1: flags_1,
        shader_flags_2: flags_2,
        base_color: [1.0, 1.0, 1.0, material_alpha],
        alpha: material_alpha,
        alpha_mode,
        alpha_threshold,
        glossiness: property.glossiness,
        specular_color: property.specular_color.0.to_array(),
        specular_strength: property.specular_strength,
        emissive_color: property.emissive_color.0.to_array(),
        emissive_multiple: property.emissive_multiple,
        double_sided: flags_2 & SLSF2_DOUBLE_SIDED != 0,
        textures: textures.map(|(_, slots)| slots).unwrap_or_default(),
    })
}

fn build_effect_material(
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    shader_block: u32,
    property: &BSEffectShaderProperty,
    alpha: Option<(u32, &NiAlphaProperty)>,
) -> Result<ValidatedNifMaterial> {
    let flags_1 = property.shader_flags_1.raw();
    let flags_2 = property.shader_flags_2.raw();
    let mut textures = Vec::new();
    push_effect_texture(
        &mut textures,
        0,
        NifTextureSemantic::Diffuse,
        &property.source_texture.0,
        true,
    )
    .map_err(|error| {
        material_error(
            source,
            shape_block,
            shape_name,
            format!("shader block {shader_block}: {error:#}"),
        )
    })?;
    push_effect_texture(
        &mut textures,
        1,
        NifTextureSemantic::Greyscale,
        &property.greyscale_texture.0,
        false,
    )
    .map_err(|error| {
        material_error(
            source,
            shape_block,
            shape_name,
            format!("shader block {shader_block}: {error:#}"),
        )
    })?;
    let (alpha_mode, alpha_threshold, alpha_property_block) = alpha_contract(alpha);
    let mut color = property.base_color.0.to_array();
    color[3] = normalize_alpha(source, shape_block, shape_name, shader_block, color[3])?;
    let material = ValidatedNifMaterial {
        shader_family: NifShaderFamily::Effect,
        lighting_shader_type: None,
        shader_block,
        texture_set_block: None,
        alpha_property_block,
        shader_flags_1: flags_1,
        shader_flags_2: flags_2,
        base_color: color,
        alpha: color[3],
        alpha_mode,
        alpha_threshold,
        glossiness: 0.0,
        specular_color: [0.0; 3],
        specular_strength: 0.0,
        emissive_color: [color[0], color[1], color[2]],
        emissive_multiple: property.base_color_scale,
        double_sided: flags_2 & SLSF2_DOUBLE_SIDED != 0,
        textures,
    };
    validate_material(source, shape_block, shape_name, &material)?;
    Ok(material)
}

fn resolve_texture_set<'a>(
    nif: &'a NifFile,
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    reference: u32,
) -> Result<Option<(u32, &'a BSShaderTextureSet)>> {
    if reference == NULL_BLOCK {
        return Ok(None);
    }
    match nif.blocks.get(reference as usize) {
        Some(NifBlock::BSShaderTextureSet(set)) => Ok(Some((reference, set))),
        Some(_) => Err(material_error(
            source,
            shape_block,
            shape_name,
            format!("texture-set reference {reference} does not point to BSShaderTextureSet"),
        )),
        None => Err(material_error(
            source,
            shape_block,
            shape_name,
            format!("texture-set reference {reference} is out of range"),
        )),
    }
}

fn lighting_texture_slots(
    set: &BSShaderTextureSet,
    shader_type: LightingShaderType,
    flags_1: u32,
    flags_2: u32,
) -> Result<Vec<NifTextureSlot>> {
    let environment = matches!(
        shader_type,
        LightingShaderType::EnvironmentMap
            | LightingShaderType::EyeEnvironmentMap
            | LightingShaderType::MultiLayerParallax
    ) || flags_1 & SLSF1_ENVIRONMENT_MAPPING != 0;
    let glow = matches!(shader_type, LightingShaderType::Glow)
        || flags_1 & SLSF1_OWN_EMIT != 0
        || flags_2 & SLSF2_GLOW_MAP != 0;
    let height = matches!(
        shader_type,
        LightingShaderType::Parallax
            | LightingShaderType::ParallaxOcclusion
            | LightingShaderType::MultiLayerParallax
    );
    let mut slots = Vec::new();
    for (index, path) in set.textures.iter().enumerate() {
        let Some(path) = path else { continue };
        let semantic = match index {
            0 => NifTextureSemantic::Diffuse,
            1 => NifTextureSemantic::Normal,
            2 if glow => NifTextureSemantic::Glow,
            2 => NifTextureSemantic::Unclassified,
            3 if height => NifTextureSemantic::Height,
            3 => NifTextureSemantic::Detail,
            4 if environment => NifTextureSemantic::EnvironmentCube,
            4 => NifTextureSemantic::Unclassified,
            5 if environment => NifTextureSemantic::EnvironmentMask,
            5 => NifTextureSemantic::Unclassified,
            6 => NifTextureSemantic::InnerLayer,
            7 => NifTextureSemantic::Specular,
            _ => NifTextureSemantic::Unclassified,
        };
        slots.push(texture_slot(
            index,
            semantic,
            path,
            index == 0 || (environment && matches!(index, 4 | 5)),
        )?);
    }
    Ok(slots)
}

fn push_effect_texture(
    slots: &mut Vec<NifTextureSlot>,
    slot: usize,
    semantic: NifTextureSemantic,
    path: &str,
    required: bool,
) -> Result<()> {
    if !path.is_empty() {
        slots.push(texture_slot(slot, semantic, path, required)?);
    }
    Ok(())
}

fn texture_slot(
    index: usize,
    semantic: NifTextureSemantic,
    path: &str,
    required: bool,
) -> Result<NifTextureSlot> {
    Ok(NifTextureSlot {
        slot: u8::try_from(index)?,
        semantic,
        path: canonical_asset_path(path, AssetKind::Texture, "dds")?,
        required,
    })
}

fn alpha_contract(
    alpha: Option<(u32, &NiAlphaProperty)>,
) -> (NifAlphaMode, Option<u8>, Option<u32>) {
    let Some((block, property)) = alpha else {
        return (NifAlphaMode::Opaque, None, None);
    };
    if property.flags.test_enabled() {
        (NifAlphaMode::Cutout, Some(property.threshold), Some(block))
    } else if property.flags.blend_enabled() {
        (NifAlphaMode::Blend, None, Some(block))
    } else {
        (NifAlphaMode::Opaque, None, Some(block))
    }
}

fn validate_material(
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    material: &ValidatedNifMaterial,
) -> Result<()> {
    let values = material
        .base_color
        .into_iter()
        .chain(material.specular_color)
        .chain(material.emissive_color)
        .chain([
            material.alpha,
            material.glossiness,
            material.specular_strength,
            material.emissive_multiple,
        ]);
    ensure!(
        values.clone().all(f32::is_finite),
        "{}",
        material_error(
            source,
            shape_block,
            shape_name,
            format!(
                "shader block {} contains a non-finite material value",
                material.shader_block
            )
        )
    );
    ensure!(
        (0.0..=1.0).contains(&material.alpha),
        "{}",
        material_error(
            source,
            shape_block,
            shape_name,
            format!(
                "shader block {} alpha {} is outside [0, 1]",
                material.shader_block, material.alpha
            )
        )
    );
    ensure!(
        material.glossiness >= 0.0
            && material.specular_strength >= 0.0
            && material.emissive_multiple >= 0.0,
        "{}",
        material_error(
            source,
            shape_block,
            shape_name,
            format!(
                "shader block {} contains a negative intensity",
                material.shader_block
            )
        )
    );
    Ok(())
}

fn normalize_alpha(
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    shader_block: u32,
    alpha: f32,
) -> Result<f32> {
    ensure!(
        alpha.is_finite(),
        "{}",
        material_error(
            source,
            shape_block,
            shape_name,
            format!("shader block {shader_block} alpha is non-finite"),
        )
    );
    ensure!(
        (-ALPHA_ROUNDING_TOLERANCE..=1.0 + ALPHA_ROUNDING_TOLERANCE).contains(&alpha),
        "{}",
        material_error(
            source,
            shape_block,
            shape_name,
            format!("shader block {shader_block} alpha {alpha} is outside [0, 1]"),
        )
    );
    Ok(alpha.clamp(0.0, 1.0))
}

fn material_error(
    source: &Path,
    shape_block: u32,
    shape_name: Option<&str>,
    detail: String,
) -> color_eyre::Report {
    color_eyre::eyre::eyre!(
        "invalid NIF material in {}: shape block {} ({:?}): {}",
        source.display(),
        shape_block,
        shape_name,
        detail
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(
        mode: NifAlphaMode,
        emissive: bool,
        double_sided: bool,
        environment: bool,
    ) -> ValidatedNifMaterial {
        ValidatedNifMaterial {
            shader_family: NifShaderFamily::Lighting,
            lighting_shader_type: Some(if environment {
                LightingShaderType::EnvironmentMap
            } else {
                LightingShaderType::Default
            }),
            shader_block: 4,
            texture_set_block: Some(5),
            alpha_property_block: (mode != NifAlphaMode::Opaque).then_some(6),
            shader_flags_1: 0,
            shader_flags_2: 0,
            base_color: [1.0; 4],
            alpha: 1.0,
            alpha_mode: mode,
            alpha_threshold: (mode == NifAlphaMode::Cutout).then_some(128),
            glossiness: 32.0,
            specular_color: [1.0; 3],
            specular_strength: 1.0,
            emissive_color: if emissive { [1.0, 0.5, 0.25] } else { [0.0; 3] },
            emissive_multiple: if emissive { 2.0 } else { 0.0 },
            double_sided,
            textures: Vec::new(),
        }
    }

    #[test]
    fn validates_six_canonical_material_fixtures() {
        for material in [
            fixture(NifAlphaMode::Opaque, false, false, false),
            fixture(NifAlphaMode::Cutout, false, false, false),
            fixture(NifAlphaMode::Blend, false, false, false),
            fixture(NifAlphaMode::Opaque, true, false, false),
            fixture(NifAlphaMode::Opaque, false, true, false),
            fixture(NifAlphaMode::Opaque, false, false, true),
        ] {
            validate_material(Path::new("fixture.nif"), 3, Some("fixture"), &material).unwrap();
        }
    }

    #[test]
    fn rejects_non_finite_and_impossible_values_with_context() {
        let mut material = fixture(NifAlphaMode::Opaque, false, false, false);
        material.glossiness = f32::NAN;
        let error = validate_material(Path::new("broken.nif"), 7, Some("bad shape"), &material)
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("broken.nif"));
        assert!(message.contains("shape block 7"));
        assert!(message.contains("non-finite"));
    }

    #[test]
    fn clamps_only_small_serialization_overshoot_in_alpha() {
        assert_eq!(
            normalize_alpha(Path::new("fixture.nif"), 3, None, 4, -0.001).unwrap(),
            0.0
        );
        assert!(normalize_alpha(Path::new("fixture.nif"), 3, None, 4, -0.1).is_err());
    }

    #[test]
    fn assigns_texture_semantics_from_shader_contract_not_names() {
        let set = BSShaderTextureSet {
            textures: vec![
                Some("odd/a.dds".to_owned()),
                Some("odd/b.dds".to_owned()),
                None,
                None,
                Some("odd/c.dds".to_owned()),
                Some("odd/d.dds".to_owned()),
            ],
            diffuse: None,
            normal: None,
            glow: None,
            height_or_detail: None,
            environment: None,
            environment_mask: None,
            inner_layer: None,
            specular: None,
        };
        let slots = lighting_texture_slots(&set, LightingShaderType::EnvironmentMap, 0, 0).unwrap();
        assert_eq!(slots[0].semantic, NifTextureSemantic::Diffuse);
        assert_eq!(slots[1].semantic, NifTextureSemantic::Normal);
        assert_eq!(slots[2].semantic, NifTextureSemantic::EnvironmentCube);
        assert_eq!(slots[3].semantic, NifTextureSemantic::EnvironmentMask);
    }
}
