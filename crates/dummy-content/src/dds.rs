//! Deterministic DDS texture fixtures.

use crate::rng::Rng;
use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure},
};
use ddsfile::{
    AlphaMode, D3D10ResourceDimension, D3DFormat, Dds, DxgiFormat, NewD3dParams, NewDxgiParams,
};

/// Pixel format of a generated texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Uncompressed 32-bit BGRA color data (`X8R8G8B8`).
    X8R8G8B8,
    /// BC1 block-compressed color data.
    Bc1Unorm,
    /// BC5 block-compressed two-channel data.
    Bc5Unorm,
    /// BC7 block-compressed color data.
    Bc7Unorm,
}

/// Description of a DDS texture to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// Pixel format.
    pub format: Format,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Optional depth for volume textures.
    pub depth: Option<u32>,
    /// Mip level count, at least one.
    pub mip_levels: u32,
    /// Whether the texture contains the six cube map faces.
    pub cubemap: bool,
}

impl Spec {
    /// Creates a single-mip 2D texture description.
    #[must_use]
    pub const fn new(format: Format, width: u32, height: u32) -> Self {
        Self {
            format,
            width,
            height,
            depth: None,
            mip_levels: 1,
            cubemap: false,
        }
    }

    /// Sets the mip level count.
    #[must_use]
    pub const fn with_mip_levels(mut self, mip_levels: u32) -> Self {
        self.mip_levels = mip_levels;
        self
    }

    /// Turns the texture into a volume texture with `depth` slices.
    #[must_use]
    pub const fn with_depth(mut self, depth: u32) -> Self {
        self.depth = Some(depth);
        self
    }

    /// Turns the texture into a cube map.
    #[must_use]
    pub const fn as_cubemap(mut self) -> Self {
        self.cubemap = true;
        self
    }
}

/// Generates a deterministic DDS texture from `spec`.
pub fn generate(spec: &Spec, rng: &mut Rng) -> Result<Vec<u8>> {
    validate(spec)?;
    let mut dds = if spec.format == Format::X8R8G8B8 {
        Dds::new_d3d(NewD3dParams {
            height: spec.height,
            width: spec.width,
            depth: None,
            format: D3DFormat::X8R8G8B8,
            mipmap_levels: Some(spec.mip_levels),
            caps2: None,
        })
        .wrap_err("failed to allocate X8R8G8B8 DDS fixture")?
    } else {
        Dds::new_dxgi(NewDxgiParams {
            height: spec.height,
            width: spec.width,
            depth: spec.depth,
            format: dxgi_format(spec.format)?,
            mipmap_levels: Some(spec.mip_levels),
            array_layers: Some(if spec.cubemap { 6 } else { 1 }),
            caps2: None,
            is_cubemap: spec.cubemap,
            resource_dimension: if spec.depth.is_some() {
                D3D10ResourceDimension::Texture3D
            } else {
                D3D10ResourceDimension::Texture2D
            },
            alpha_mode: AlphaMode::Straight,
        })
        .wrap_err("failed to allocate DXGI DDS fixture")?
    };
    rng.fill(&mut dds.data);
    let mut bytes = Vec::new();
    dds.write(&mut bytes)
        .wrap_err("failed to encode DDS fixture")?;
    Ok(bytes)
}

fn validate(spec: &Spec) -> Result<()> {
    ensure!(
        spec.width > 0 && spec.height > 0,
        "DDS dimensions must be non-zero"
    );
    ensure!(spec.mip_levels >= 1, "DDS requires at least one mip level");
    ensure!(
        !(spec.cubemap && spec.depth.is_some()),
        "a DDS texture cannot be both a cube map and a volume texture"
    );
    if let Some(depth) = spec.depth {
        ensure!(depth > 0, "DDS volume depth must be non-zero");
    }
    if spec.format == Format::X8R8G8B8 {
        ensure!(
            spec.depth.is_none() && !spec.cubemap,
            "X8R8G8B8 fixtures support only 2D textures"
        );
    } else {
        ensure!(
            spec.width.is_multiple_of(4) && spec.height.is_multiple_of(4),
            "block-compressed DDS dimensions must be multiples of four"
        );
        if let Some(depth) = spec.depth {
            ensure!(
                depth.is_multiple_of(4),
                "block-compressed DDS volume depth must be a multiple of four"
            );
        }
    }
    let max_dimension = spec.width.max(spec.height).max(spec.depth.unwrap_or(1));
    let full_chain = u32::BITS - max_dimension.leading_zeros();
    ensure!(
        spec.mip_levels <= full_chain,
        "DDS mip level count {} exceeds the full chain of {full_chain}",
        spec.mip_levels
    );
    Ok(())
}

fn dxgi_format(format: Format) -> Result<DxgiFormat> {
    Ok(match format {
        Format::Bc1Unorm => DxgiFormat::BC1_UNorm,
        Format::Bc5Unorm => DxgiFormat::BC5_UNorm,
        Format::Bc7Unorm => DxgiFormat::BC7_UNorm,
        Format::X8R8G8B8 => bail!("X8R8G8B8 does not use a DXGI format"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dx10_misc_flag(bytes: &[u8]) -> u32 {
        u32::from_le_bytes(bytes[136..140].try_into().unwrap())
    }

    #[test]
    fn generates_mipped_bc1_texture() {
        let spec = Spec::new(Format::Bc1Unorm, 8, 8).with_mip_levels(4);
        let bytes = generate(&spec, &mut Rng::new(1)).unwrap();
        let dds = Dds::read(bytes.as_slice()).unwrap();
        assert_eq!(dds.get_width(), 8);
        assert_eq!(dds.get_height(), 8);
        assert_eq!(dds.get_num_mipmap_levels(), 4);
        assert_eq!(dds.get_dxgi_format(), Some(DxgiFormat::BC1_UNorm));
        assert_eq!(dds.data.len(), 56);
    }

    #[test]
    fn generates_cube_map_with_six_faces() {
        let spec = Spec::new(Format::Bc1Unorm, 4, 4).as_cubemap();
        let bytes = generate(&spec, &mut Rng::new(2)).unwrap();
        let dds = Dds::read(bytes.as_slice()).unwrap();
        assert_eq!(dds.data.len(), 48);
        assert_eq!(dx10_misc_flag(&bytes) & 0x4, 0x4, "cubemap flag is unset");
        assert_eq!(
            u32::from_le_bytes(bytes[140..144].try_into().unwrap()),
            1,
            "DX10 array size must count cubes"
        );
    }

    #[test]
    fn generates_volume_texture() {
        let spec = Spec::new(Format::Bc1Unorm, 4, 4).with_depth(4);
        let bytes = generate(&spec, &mut Rng::new(3)).unwrap();
        let dds = Dds::read(bytes.as_slice()).unwrap();
        assert_eq!(dds.get_depth(), 4);
        assert_eq!(dds.data.len(), 32);
    }

    #[test]
    fn generates_uncompressed_color_texture() {
        let spec = Spec::new(Format::X8R8G8B8, 2, 1);
        let bytes = generate(&spec, &mut Rng::new(4)).unwrap();
        let dds = Dds::read(bytes.as_slice()).unwrap();
        assert_eq!(dds.get_d3d_format(), Some(D3DFormat::X8R8G8B8));
        assert_eq!(dds.data.len(), 8);
    }

    #[test]
    fn same_seed_is_byte_identical() {
        let spec = Spec::new(Format::Bc7Unorm, 4, 4);
        assert_eq!(
            generate(&spec, &mut Rng::new(5)).unwrap(),
            generate(&spec, &mut Rng::new(5)).unwrap()
        );
        assert_ne!(
            generate(&spec, &mut Rng::new(5)).unwrap(),
            generate(&spec, &mut Rng::new(6)).unwrap()
        );
    }

    #[test]
    fn rejects_invalid_specs() {
        let mut rng = Rng::new(7);
        for spec in [
            Spec::new(Format::Bc1Unorm, 0, 4),
            Spec::new(Format::Bc1Unorm, 4, 4).with_mip_levels(0),
            Spec::new(Format::Bc1Unorm, 4, 4).with_mip_levels(4),
            Spec::new(Format::Bc1Unorm, 6, 4),
            Spec::new(Format::Bc1Unorm, 4, 4).with_depth(0),
            Spec::new(Format::X8R8G8B8, 4, 4).as_cubemap(),
        ] {
            assert!(generate(&spec, &mut rng).is_err(), "{spec:?} was accepted");
        }
    }
}
