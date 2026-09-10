use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use ddsfile::{Caps2, D3DFormat, Dds, MiscFlag, PixelFormatFlags};
use memmap2::Mmap;
use std::{
    ffi::c_void,
    fs::{self, File},
    io::Cursor,
    path::Path,
    sync::Once,
};

const KTX2_IDENTIFIER: &[u8; 12] = b"\xABKTX 20\xBB\r\n\x1A\n";
const FLAG_KTX2: u32 = 1 << 11;
const FLAG_SRGB: u32 = 1 << 13;
const FLAG_GENERATE_MIPS_CLAMP: u32 = 1 << 14;
const FLAG_UASTC: u32 = 1 << 17;
const UASTC_LEVEL_DEFAULT: u8 = 2;
const ETC1S_QUALITY_DEFAULT: u8 = 192;
static BASIS_INIT: Once = Once::new();
type EncodedVolumeLevels = Vec<Vec<Vec<u8>>>;
type EncodedVolume = (EncodedVolumeLevels, Vec<u8>);

unsafe extern "C" {
    fn opensky_basis_compress_ktx2(
        rgba: *const u8,
        width: u32,
        height: u32,
        flags_and_quality: u32,
        uastc_rdo_quality: f32,
        size: *mut usize,
    ) -> *mut c_void;
    fn opensky_basis_free(data: *mut c_void);
}

pub struct TextureConverter;

impl TextureConverter {
    pub fn convert_dds_to_ktx2(input: &Path, output: &Path, normal_map: bool) -> Result<()> {
        Self::convert_dds_to_ktx2_with_options(
            input,
            output,
            normal_map,
            ETC1S_QUALITY_DEFAULT,
            UASTC_LEVEL_DEFAULT,
        )
    }

    pub fn convert_dds_to_ktx2_with_options(
        input: &Path,
        output: &Path,
        normal_map: bool,
        etc1s_quality: u8,
        uastc_level: u8,
    ) -> Result<()> {
        let file =
            File::open(input).wrap_err_with(|| format!("failed to open {}", input.display()))?;
        let mmap = unsafe { Mmap::map(&file) }
            .wrap_err_with(|| format!("failed to memory-map {}", input.display()))?;

        let ktx2 = Self::convert_with_options(&mmap, normal_map, etc1s_quality, uastc_level)?;
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, ktx2).wrap_err_with(|| format!("failed to write {}", output.display()))
    }

    pub fn convert(dds_bytes: &[u8], normal_map: bool) -> Result<Vec<u8>> {
        Self::convert_with_options(
            dds_bytes,
            normal_map,
            ETC1S_QUALITY_DEFAULT,
            UASTC_LEVEL_DEFAULT,
        )
    }

    pub fn convert_with_options(
        dds_bytes: &[u8],
        normal_map: bool,
        etc1s_quality: u8,
        uastc_level: u8,
    ) -> Result<Vec<u8>> {
        let dds = Dds::read(Cursor::new(dds_bytes)).wrap_err("invalid DDS")?;
        let depth = dds.get_depth();
        let is_cubemap = dds.header.caps2.contains(Caps2::CUBEMAP)
            || dds
                .header10
                .as_ref()
                .is_some_and(|header| header.misc_flag.contains(MiscFlag::TEXTURECUBE));
        let layer_count = dds.get_num_array_layers();
        ensure!(
            layer_count <= 1 || (is_cubemap && layer_count == 6),
            "DDS texture arrays are not supported"
        );
        ensure!(
            !is_cubemap || layer_count == 6,
            "DDS cubemap does not contain exactly six faces"
        );
        let generate_mips = dds.get_num_mipmap_levels() > 1;
        if depth > 1 {
            ensure!(!is_cubemap, "DDS cannot be both a volume and a cubemap");
            ensure!(layer_count <= 1, "volume DDS arrays are not supported");
            let (encoded_levels, template) = match image_dds::SurfaceRgba8::decode_dds(&dds) {
                Ok(surface) => {
                    encode_decoded_volume(&surface, normal_map, etc1s_quality, uastc_level)?
                }
                Err(_) if is_l8_volume(&dds) => {
                    encode_l8_volume(&dds, normal_map, etc1s_quality, uastc_level)?
                }
                Err(error) => return Err(error).wrap_err("DDS volume cannot be decoded"),
            };
            let result = combine_ktx2_volume(
                &template,
                &encoded_levels,
                dds.get_width(),
                dds.get_height(),
                depth,
            )?;
            validate_ktx2(&result, normal_map)?;
            return Ok(result);
        }
        if is_cubemap {
            let mut encoded_faces = Vec::with_capacity(6);
            for face in 0..6 {
                let surface =
                    image_dds::SurfaceRgba8::decode_layers_mipmaps_dds(&dds, face..face + 1, 0..1)
                        .wrap_err_with(|| format!("DDS cubemap face {face} cannot be decoded"))?;
                let image = surface
                    .into_image()
                    .wrap_err_with(|| format!("DDS cubemap face {face} is invalid"))?;
                encoded_faces.push(encode_basis_ktx2(
                    image.width(),
                    image.height(),
                    &image.into_raw(),
                    normal_map,
                    generate_mips,
                    etc1s_quality,
                    uastc_level,
                )?);
            }
            let result = combine_ktx2_cubemap_faces(&encoded_faces)?;
            validate_ktx2(&result, normal_map)?;
            return Ok(result);
        }

        let (width, height, rgba) = match image_dds::image_from_dds(&dds, 0) {
            Ok(image) => (image.width(), image.height(), image.into_raw()),
            Err(_) if dds.get_d3d_format() == Some(D3DFormat::X8R8G8B8) => {
                let rgba = decode_x8r8g8b8(&dds)?;
                (dds.get_width(), dds.get_height(), rgba)
            }
            Err(error) => return Err(error).wrap_err("DDS pixel format cannot be decoded"),
        };
        let result = encode_basis_ktx2(
            width,
            height,
            &rgba,
            normal_map,
            generate_mips,
            etc1s_quality,
            uastc_level,
        )?;
        validate_ktx2(&result, normal_map)?;
        Ok(result)
    }

    pub fn is_normal_map(path: &Path) -> bool {
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        stem.ends_with("_n") || stem.ends_with("_normal") || stem.contains("normalmap")
    }
}

fn encode_decoded_volume(
    surface: &image_dds::SurfaceRgba8<Vec<u8>>,
    normal_map: bool,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<EncodedVolume> {
    let mut encoded_levels = Vec::with_capacity(surface.mipmaps as usize);
    for mip in 0..surface.mipmaps {
        let mip_depth = (surface.depth >> mip).max(1);
        let mut encoded_slices = Vec::with_capacity(mip_depth as usize);
        for slice in 0..mip_depth {
            let image = surface.get_image(0, slice, mip).ok_or_else(|| {
                color_eyre::eyre::eyre!("DDS volume mip {mip} slice {slice} has invalid dimensions")
            })?;
            encoded_slices.push(encode_basis_ktx2(
                image.width(),
                image.height(),
                &image.into_raw(),
                normal_map,
                false,
                etc1s_quality,
                uastc_level,
            )?);
        }
        encoded_levels.push(encoded_slices);
    }
    let base_image = surface
        .get_image(0, 0, 0)
        .ok_or_else(|| color_eyre::eyre::eyre!("DDS volume has no base slice"))?;
    let template = encode_basis_ktx2(
        base_image.width(),
        base_image.height(),
        &base_image.into_raw(),
        normal_map,
        surface.mipmaps > 1,
        etc1s_quality,
        uastc_level,
    )?;
    Ok((encoded_levels, template))
}

fn is_l8_volume(dds: &Dds) -> bool {
    dds.header.spf.flags.contains(PixelFormatFlags::LUMINANCE)
        && dds.header.spf.rgb_bit_count == Some(8)
        && !dds.header.spf.flags.contains(PixelFormatFlags::ALPHA)
        && !dds
            .header
            .spf
            .flags
            .contains(PixelFormatFlags::ALPHA_PIXELS)
}

fn encode_l8_volume(
    dds: &Dds,
    normal_map: bool,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<EncodedVolume> {
    let mip_count = dds.get_num_mipmap_levels();
    let mut offset = 0usize;
    let mut encoded_levels = Vec::with_capacity(mip_count as usize);
    let mut base_rgba = None;
    for mip in 0..mip_count {
        let width = (dds.get_width() >> mip).max(1);
        let height = (dds.get_height() >> mip).max(1);
        let depth = (dds.get_depth() >> mip).max(1);
        let slice_len = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| color_eyre::eyre::eyre!("L8 DDS volume size overflow"))?;
        let mut encoded_slices = Vec::with_capacity(depth as usize);
        for slice in 0..depth {
            let end = offset
                .checked_add(slice_len)
                .ok_or_else(|| color_eyre::eyre::eyre!("L8 DDS volume size overflow"))?;
            let luminance = dds.data.get(offset..end).ok_or_else(|| {
                color_eyre::eyre::eyre!("L8 DDS volume is truncated at mip {mip} slice {slice}")
            })?;
            let mut rgba = Vec::with_capacity(slice_len * 4);
            for &value in luminance {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
            if base_rgba.is_none() {
                base_rgba = Some(rgba.clone());
            }
            encoded_slices.push(encode_basis_ktx2(
                width,
                height,
                &rgba,
                normal_map,
                false,
                etc1s_quality,
                uastc_level,
            )?);
            offset = end;
        }
        encoded_levels.push(encoded_slices);
    }
    ensure!(
        offset == dds.data.len(),
        "L8 DDS volume has {} unexpected trailing bytes",
        dds.data.len() - offset
    );
    let template = encode_basis_ktx2(
        dds.get_width(),
        dds.get_height(),
        &base_rgba.ok_or_else(|| color_eyre::eyre::eyre!("L8 DDS volume has no data"))?,
        normal_map,
        mip_count > 1,
        etc1s_quality,
        uastc_level,
    )?;
    Ok((encoded_levels, template))
}

fn combine_ktx2_volume(
    template: &[u8],
    levels: &[Vec<Vec<u8>>],
    width: u32,
    height: u32,
    depth: u32,
) -> Result<Vec<u8>> {
    ensure!(
        width > 0 && height > 0 && depth > 1,
        "invalid KTX2 volume dimensions"
    );
    ensure!(!levels.is_empty(), "KTX2 volume has no levels");
    let template_reader = ktx2::Reader::new(template)
        .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 volume template: {error:?}"))?;
    let reference = template_reader.header();
    ensure!(
        reference.level_count as usize == levels.len(),
        "KTX2 volume template has {} levels, but the DDS has {}",
        reference.level_count,
        levels.len()
    );
    ensure!(
        reference.supercompression_scheme != Some(ktx2::SupercompressionScheme::BasisLZ),
        "BasisLZ volume assembly is not supported"
    );
    let level_count = levels.len();
    let level_table_end = ktx2::Header::LENGTH
        .checked_add(
            level_count
                .checked_mul(ktx2::LevelIndex::LENGTH)
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume level table overflow"))?,
        )
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume level table overflow"))?;
    let first_data_offset = (0..level_count)
        .map(|level| {
            let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
            let bytes: &[u8; ktx2::LevelIndex::LENGTH] = template
                [start..start + ktx2::LevelIndex::LENGTH]
                .try_into()
                .expect("fixed-size KTX2 level index");
            ktx2::LevelIndex::from_bytes(bytes).byte_offset as usize
        })
        .min()
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume template has no levels"))?;
    ensure!(
        first_data_offset >= level_table_end && first_data_offset <= template.len(),
        "invalid KTX2 volume metadata layout"
    );

    let mut output = template[..first_data_offset].to_vec();
    output[20..24].copy_from_slice(&width.to_le_bytes());
    output[24..28].copy_from_slice(&height.to_le_bytes());
    output[28..32].copy_from_slice(&depth.to_le_bytes());
    output[32..36].copy_from_slice(&0u32.to_le_bytes());
    output[36..40].copy_from_slice(&1u32.to_le_bytes());
    let mut indexes = Vec::with_capacity(level_count);
    for (mip, slices) in levels.iter().enumerate() {
        let expected_depth = (depth >> mip).max(1) as usize;
        ensure!(
            slices.len() == expected_depth,
            "KTX2 volume mip {mip} has {} slices, expected {expected_depth}",
            slices.len()
        );
        while !output.len().is_multiple_of(16) {
            output.push(0);
        }
        let offset = output.len() as u64;
        let mut uncompressed_length = 0u64;
        for slice in slices {
            let reader = ktx2::Reader::new(slice)
                .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 volume slice: {error:?}"))?;
            let header = reader.header();
            ensure!(
                header.level_count == 1
                    && header.pixel_depth == 0
                    && header.layer_count == 0
                    && header.face_count == 1
                    && header.format == reference.format
                    && header.supercompression_scheme == reference.supercompression_scheme,
                "KTX2 volume slice has incompatible layout"
            );
            let level = reader
                .levels()
                .next()
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume slice has no level"))?;
            output.extend_from_slice(level.data);
            uncompressed_length = uncompressed_length
                .checked_add(level.uncompressed_byte_length)
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 volume size overflow"))?;
        }
        indexes.push(ktx2::LevelIndex {
            byte_offset: offset,
            byte_length: output.len() as u64 - offset,
            uncompressed_byte_length: uncompressed_length,
        });
    }
    for (level, index) in indexes.iter().enumerate() {
        let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
        output[start..start + ktx2::LevelIndex::LENGTH].copy_from_slice(&index.as_bytes());
    }
    Ok(output)
}

fn combine_ktx2_cubemap_faces(faces: &[Vec<u8>]) -> Result<Vec<u8>> {
    ensure!(
        faces.len() == 6,
        "a cubemap requires exactly six KTX2 faces"
    );
    let readers: Vec<_> = faces
        .iter()
        .map(|face| {
            ktx2::Reader::new(face)
                .map_err(|error| color_eyre::eyre::eyre!("invalid KTX2 cubemap face: {error:?}"))
        })
        .collect::<Result<_>>()?;
    let reference = readers[0].header();
    ensure!(reference.face_count == 1, "cubemap source face is not 2D");
    ensure!(
        reference.supercompression_scheme != Some(ktx2::SupercompressionScheme::BasisLZ),
        "BasisLZ cubemap assembly is not supported"
    );
    for (face, reader) in readers[1..].iter().enumerate() {
        let header = reader.header();
        ensure!(
            header.pixel_width == reference.pixel_width
                && header.pixel_height == reference.pixel_height
                && header.level_count == reference.level_count
                && header.format == reference.format
                && header.supercompression_scheme == reference.supercompression_scheme,
            "KTX2 cubemap face {} has incompatible layout: expected {reference:?}, got {header:?}",
            face + 1
        );
    }

    let level_count = reference.level_count.max(1) as usize;
    let level_table_end = ktx2::Header::LENGTH
        .checked_add(
            level_count
                .checked_mul(ktx2::LevelIndex::LENGTH)
                .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 cubemap level table overflow"))?,
        )
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 cubemap level table overflow"))?;
    let first_data_offset = (0..level_count)
        .map(|level| {
            let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
            let bytes: &[u8; ktx2::LevelIndex::LENGTH] = faces[0]
                [start..start + ktx2::LevelIndex::LENGTH]
                .try_into()
                .expect("fixed-size KTX2 level index");
            ktx2::LevelIndex::from_bytes(bytes).byte_offset as usize
        })
        .min()
        .ok_or_else(|| color_eyre::eyre::eyre!("KTX2 cubemap has no levels"))?;
    ensure!(
        first_data_offset >= level_table_end && first_data_offset <= faces[0].len(),
        "invalid KTX2 cubemap metadata layout"
    );

    let face_levels: Vec<Vec<_>> = readers
        .iter()
        .map(|reader| reader.levels().collect())
        .collect();
    let mut output = faces[0][..first_data_offset].to_vec();
    output[36..40].copy_from_slice(&6u32.to_le_bytes());
    let mut indexes = Vec::with_capacity(level_count);
    for level in 0..level_count {
        while !output.len().is_multiple_of(16) {
            output.push(0);
        }
        let offset = output.len() as u64;
        let face_length = face_levels[0][level].data.len();
        let face_uncompressed = face_levels[0][level].uncompressed_byte_length;
        for levels in &face_levels {
            ensure!(
                levels[level].data.len() == face_length
                    && levels[level].uncompressed_byte_length == face_uncompressed,
                "KTX2 cubemap face levels have incompatible sizes"
            );
            output.extend_from_slice(levels[level].data);
        }
        indexes.push(ktx2::LevelIndex {
            byte_offset: offset,
            byte_length: (face_length * 6) as u64,
            uncompressed_byte_length: face_uncompressed.saturating_mul(6),
        });
    }
    for (level, index) in indexes.iter().enumerate() {
        let start = ktx2::Header::LENGTH + level * ktx2::LevelIndex::LENGTH;
        output[start..start + ktx2::LevelIndex::LENGTH].copy_from_slice(&index.as_bytes());
    }
    Ok(output)
}

fn decode_x8r8g8b8(dds: &Dds) -> Result<Vec<u8>> {
    let width = usize::try_from(dds.get_width()).wrap_err("DDS width does not fit in memory")?;
    let height = usize::try_from(dds.get_height()).wrap_err("DDS height does not fit in memory")?;
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS row size overflow"))?;
    let pitch = usize::try_from(
        dds.get_pitch()
            .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS has no pitch"))?,
    )
    .wrap_err("DDS pitch does not fit in memory")?;
    ensure!(
        pitch >= row_bytes,
        "X8R8G8B8 DDS pitch is smaller than a row"
    );
    let source_size = pitch
        .checked_mul(height)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 DDS payload size overflow"))?;
    ensure!(
        dds.data.len() >= source_size,
        "truncated X8R8G8B8 DDS payload"
    );
    let output_size = row_bytes
        .checked_mul(height)
        .ok_or_else(|| color_eyre::eyre::eyre!("X8R8G8B8 RGBA size overflow"))?;
    let mut rgba = Vec::with_capacity(output_size);
    for row in dds.data[..source_size].chunks_exact(pitch) {
        let (pixels, remainder) = row[..row_bytes].as_chunks::<4>();
        debug_assert!(remainder.is_empty());
        for pixel in pixels {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    Ok(rgba)
}

fn encode_basis_ktx2(
    width: u32,
    height: u32,
    rgba: &[u8],
    normal_map: bool,
    generate_mips: bool,
    etc1s_quality: u8,
    uastc_level: u8,
) -> Result<Vec<u8>> {
    ensure!(
        width > 0 && height > 0,
        "texture dimensions must be non-zero"
    );
    ensure!(
        rgba.len() == width as usize * height as usize * 4,
        "RGBA payload size mismatch"
    );
    BASIS_INIT.call_once(basis_universal::encoder_init);
    ensure!(etc1s_quality > 0, "ETC1S quality must be greater than zero");
    ensure!(uastc_level <= 4, "UASTC level must be between 0 and 4");
    let mut flags = FLAG_KTX2;
    if generate_mips {
        flags |= FLAG_GENERATE_MIPS_CLAMP;
    }
    // Bevy 0.19 can transcode UASTC payloads from KTX2, but its KTX2 loader
    // explicitly rejects the BasisLZ supercompression used by ETC1S. Keep the
    // offline/runtime contract compatible by emitting UASTC for every texture.
    flags |= FLAG_UASTC | u32::from(uastc_level);
    if !normal_map {
        flags |= FLAG_SRGB;
    }
    let mut size = 0usize;
    // SAFETY: the encoder copies the complete RGBA slice during this call. The
    // returned allocation is owned by Basis and freed after copying below.
    let data =
        unsafe { opensky_basis_compress_ktx2(rgba.as_ptr(), width, height, flags, 0.0, &mut size) };
    ensure!(
        !data.is_null() && size > 0,
        "Basis Universal compression failed"
    );
    // SAFETY: a successful encoder call returns exactly `size` initialized bytes.
    let output = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec() };
    // SAFETY: `data` was allocated by basis_compress and has not been freed yet.
    unsafe { opensky_basis_free(data) };
    Ok(output)
}

fn validate_ktx2(bytes: &[u8], _normal_map: bool) -> Result<()> {
    ensure!(
        bytes.starts_with(KTX2_IDENTIFIER),
        "encoder did not produce KTX2"
    );
    let reader = ktx2::Reader::new(bytes)
        .map_err(|error| color_eyre::eyre::eyre!("generated invalid KTX2: {error:?}"))?;
    ensure!(
        reader.header().pixel_width > 0,
        "KTX2 has invalid dimensions"
    );
    ensure!(
        reader.header().supercompression_scheme != Some(ktx2::SupercompressionScheme::BasisLZ),
        "runtime-incompatible BasisLZ supercompression was emitted"
    );
    ensure!(
        reader.levels().next().is_some(),
        "KTX2 contains no image levels"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddsfile::NewD3dParams;

    #[test]
    fn creates_runtime_compatible_color_ktx2() {
        let pixels = [255, 0, 0, 255].repeat(16);
        let bytes = encode_basis_ktx2(4, 4, &pixels, false, false, 192, 2).unwrap();
        validate_ktx2(&bytes, false).unwrap();
        let reader = ktx2::Reader::new(&bytes).unwrap();
        assert_ne!(
            reader.header().supercompression_scheme,
            Some(ktx2::SupercompressionScheme::BasisLZ)
        );
    }

    #[test]
    fn creates_uastc_normal_map_ktx2() {
        let pixels = [128, 128, 255, 255].repeat(16);
        let bytes = encode_basis_ktx2(4, 4, &pixels, true, false, 192, 2).unwrap();
        validate_ktx2(&bytes, true).unwrap();
    }

    #[test]
    fn generates_mipmap_chain_for_mipped_source() {
        let pixels = [64, 128, 192, 255].repeat(64);
        let bytes = encode_basis_ktx2(8, 8, &pixels, false, true, 192, 2).unwrap();
        let reader = ktx2::Reader::new(&bytes).unwrap();
        assert_eq!(reader.header().level_count, 4);
        assert_eq!(reader.levels().count(), 4);
    }

    #[test]
    fn assembles_six_faces_into_a_cubemap_ktx2() {
        let faces: Vec<_> = (0..6)
            .map(|face| {
                let pixels = [face * 20, 64, 128, 255].repeat(16);
                encode_basis_ktx2(4, 4, &pixels, false, true, 192, 2).unwrap()
            })
            .collect();

        let cubemap = combine_ktx2_cubemap_faces(&faces).unwrap();
        let reader = ktx2::Reader::new(&cubemap).unwrap();
        assert_eq!(reader.header().face_count, 6);
        assert_eq!(reader.header().layer_count, 0);
        assert_eq!(reader.header().level_count, 3);
        for (combined, single) in reader
            .levels()
            .zip(ktx2::Reader::new(&faces[0]).unwrap().levels())
        {
            assert_eq!(combined.data.len(), single.data.len() * 6);
            assert_eq!(
                combined.uncompressed_byte_length,
                single.uncompressed_byte_length * 6
            );
        }
    }

    #[test]
    fn assembles_depth_slices_into_a_volume_ktx2() {
        let template =
            encode_basis_ktx2(4, 4, &[64, 64, 64, 255].repeat(16), false, true, 192, 2).unwrap();
        let levels: Vec<Vec<Vec<u8>>> = [(4, 4, 4), (2, 2, 2), (1, 1, 1)]
            .into_iter()
            .map(|(width, height, depth)| {
                (0..depth)
                    .map(|slice| {
                        encode_basis_ktx2(
                            width,
                            height,
                            &[slice as u8 * 20, 80, 120, 255].repeat((width * height) as usize),
                            false,
                            false,
                            192,
                            2,
                        )
                        .unwrap()
                    })
                    .collect()
            })
            .collect();

        let volume = combine_ktx2_volume(&template, &levels, 4, 4, 4).unwrap();
        let reader = ktx2::Reader::new(&volume).unwrap();
        assert_eq!(reader.header().pixel_width, 4);
        assert_eq!(reader.header().pixel_height, 4);
        assert_eq!(reader.header().pixel_depth, 4);
        assert_eq!(reader.header().level_count, 3);
        assert_eq!(reader.levels().count(), 3);
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_DDS_FIXTURE with a locally installed cubemap"]
    fn converts_installed_cubemap_fixture() {
        let path = std::env::var_os("OPENSKYRIM_DDS_FIXTURE")
            .map(std::path::PathBuf::from)
            .expect("set OPENSKYRIM_DDS_FIXTURE to a cubemap DDS");
        let bytes = std::fs::read(&path).unwrap();

        let converted = TextureConverter::convert(&bytes, false)
            .unwrap_or_else(|error| panic!("failed to convert {}: {error:#}", path.display()));
        let reader = ktx2::Reader::new(&converted).unwrap();
        assert_eq!(reader.header().face_count, 6);
    }

    #[test]
    #[ignore = "requires OPENSKYRIM_VOLUME_DDS_FIXTURE with a locally installed volume DDS"]
    fn converts_installed_volume_fixture() {
        let path = std::env::var_os("OPENSKYRIM_VOLUME_DDS_FIXTURE")
            .map(std::path::PathBuf::from)
            .expect("set OPENSKYRIM_VOLUME_DDS_FIXTURE to a volume DDS");
        let bytes = std::fs::read(&path).unwrap();

        let converted = TextureConverter::convert(&bytes, false)
            .unwrap_or_else(|error| panic!("failed to convert {}: {error:#}", path.display()));
        let reader = ktx2::Reader::new(&converted).unwrap();
        assert_eq!(reader.header().pixel_depth, 128);
        assert_eq!(reader.header().level_count, 8);
    }

    #[test]
    fn decodes_x8r8g8b8_as_opaque_rgba() {
        let mut dds = x8r8g8b8_fixture();
        dds.data.copy_from_slice(&[3, 2, 1, 99, 30, 20, 10, 88]);

        assert_eq!(
            decode_x8r8g8b8(&dds).unwrap(),
            [1, 2, 3, 255, 10, 20, 30, 255]
        );
    }

    #[test]
    fn rejects_truncated_x8r8g8b8() {
        let mut dds = x8r8g8b8_fixture();
        dds.data.pop();

        assert!(
            decode_x8r8g8b8(&dds)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
    }

    #[test]
    fn converts_x8r8g8b8_to_runtime_compatible_ktx2() {
        let mut dds = x8r8g8b8_fixture();
        dds.data.copy_from_slice(&[3, 2, 1, 0, 30, 20, 10, 0]);
        let mut bytes = Vec::new();
        dds.write(&mut bytes).unwrap();

        let ktx2 = TextureConverter::convert(&bytes, false).unwrap();
        validate_ktx2(&ktx2, false).unwrap();
    }

    fn x8r8g8b8_fixture() -> Dds {
        Dds::new_d3d(NewD3dParams {
            height: 1,
            width: 2,
            depth: None,
            format: D3DFormat::X8R8G8B8,
            mipmap_levels: None,
            caps2: None,
        })
        .unwrap()
    }
}
