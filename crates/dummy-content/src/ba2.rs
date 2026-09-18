//! Deterministic BA2 archive fixtures.
//!
//! Only the layouts consumed by the converter are produced: version 1 `GNRL`
//! archives and version 1 `DX10` archives with one chunk per texture. Payloads
//! are stored verbatim or as a zlib stream.

use crate::Entry;
use crate::bytes::{push_u16, push_u32, push_u64};
use crate::path::split_asset_name;
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure, eyre},
};
use std::collections::HashSet;

const HEADER_SIZE: u32 = 24;
const GENERAL_RECORD_SIZE: u32 = 36;
const DX10_RECORD_SIZE: u32 = 24;
const DX10_CHUNK_SIZE: u32 = 24;
const MAX_ENTRY_SIZE: usize = 1024 * 1024 * 1024;

/// Payload compression used when generating a BA2 archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Stores every payload verbatim.
    None,
    /// Stores every payload as a zlib stream.
    Zlib,
}

/// Generates a version 1 `GNRL` BA2 archive.
pub fn general(entries: &[Entry<'_>], compression: Compression) -> Result<Vec<u8>> {
    let mut seen = HashSet::with_capacity(entries.len());
    let mut payloads = Vec::with_capacity(entries.len());
    for entry in entries {
        split_asset_name(entry.name, "BA2")
            .wrap_err_with(|| format!("invalid BA2 entry {:?}", entry.name))?;
        ensure!(
            seen.insert(entry.name),
            "duplicate BA2 entry {:?}",
            entry.name
        );
        ensure!(
            entry.data.len() <= MAX_ENTRY_SIZE,
            "BA2 entry exceeds the 1 GiB fixture limit"
        );
        payloads.push(encode_payload(entry.data, compression)?);
    }

    let file_count = u32::try_from(entries.len()).map_err(|_| eyre!("BA2 file count overflow"))?;
    let names_offset = HEADER_SIZE
        .checked_add(
            u32::try_from(entries.len())
                .map_err(|_| eyre!("BA2 file count overflow"))?
                .checked_mul(GENERAL_RECORD_SIZE)
                .ok_or_else(|| eyre!("BA2 record table overflow"))?,
        )
        .ok_or_else(|| eyre!("BA2 record table overflow"))?;
    let names_size = names_size(entries.iter().map(|entry| entry.name))?;
    let payload_start = names_offset
        .checked_add(names_size)
        .ok_or_else(|| eyre!("BA2 name table overflow"))?;

    let mut payload_offsets = Vec::with_capacity(entries.len());
    let mut cursor = payload_start;
    for payload in &payloads {
        payload_offsets.push(u64::from(cursor));
        cursor = cursor
            .checked_add(
                u32::try_from(payload.len()).map_err(|_| eyre!("BA2 payload exceeds 4 GiB"))?,
            )
            .ok_or_else(|| eyre!("BA2 payload table overflow"))?;
    }
    let total_size = usize::try_from(cursor).map_err(|_| eyre!("BA2 archive exceeds usize"))?;

    let mut bytes = Vec::with_capacity(total_size);
    bytes.extend_from_slice(b"BTDX");
    push_u32(&mut bytes, 1);
    bytes.extend_from_slice(b"GNRL");
    push_u32(&mut bytes, file_count);
    push_u64(&mut bytes, u64::from(names_offset));

    for (index, entry) in entries.iter().enumerate() {
        push_u32(&mut bytes, 0);
        bytes.extend_from_slice(&[0; 4]);
        push_u32(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u64(&mut bytes, payload_offsets[index]);
        push_u32(
            &mut bytes,
            match compression {
                Compression::None => 0,
                Compression::Zlib => u32::try_from(payloads[index].len())
                    .map_err(|_| eyre!("BA2 packed size overflow"))?,
            },
        );
        push_u32(
            &mut bytes,
            u32::try_from(entry.data.len()).map_err(|_| eyre!("BA2 unpacked size overflow"))?,
        );
        push_u32(&mut bytes, 0);
    }
    debug_assert_eq!(bytes.len(), names_offset as usize);

    for entry in entries {
        push_u16(
            &mut bytes,
            u16::try_from(entry.name.len()).map_err(|_| eyre!("BA2 name exceeds 65535 bytes"))?,
        );
        bytes.extend_from_slice(entry.name.as_bytes());
    }
    for payload in &payloads {
        bytes.extend_from_slice(payload);
    }
    debug_assert_eq!(bytes.len(), total_size);
    Ok(bytes)
}

/// A texture record for [`dx10`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dx10Texture<'a> {
    /// Archive-relative path, for example `textures/generated.dds`.
    pub name: &'a str,
    /// Texture width in pixels.
    pub width: u16,
    /// Texture height in pixels.
    pub height: u16,
    /// Number of mip levels covered by `pixels`.
    pub mip_count: u8,
    /// DXGI format code stored in the record.
    pub format: u8,
    /// Whether the texture contains cube map faces.
    pub cubemap: bool,
    /// Concatenated pixel data for all mips.
    pub pixels: &'a [u8],
}

impl<'a> Dx10Texture<'a> {
    /// Creates a single-mip 2D texture record.
    #[must_use]
    pub const fn new(name: &'a str, width: u16, height: u16, format: u8, pixels: &'a [u8]) -> Self {
        Self {
            name,
            width,
            height,
            mip_count: 1,
            format,
            cubemap: false,
            pixels,
        }
    }

    /// Sets the number of mip levels covered by the pixel data.
    #[must_use]
    pub const fn with_mip_count(mut self, mip_count: u8) -> Self {
        self.mip_count = mip_count;
        self
    }

    /// Marks the texture as a cube map.
    #[must_use]
    pub const fn as_cubemap(mut self) -> Self {
        self.cubemap = true;
        self
    }
}

/// Generates a version 1 `DX10` BA2 archive with one chunk per texture.
pub fn dx10(textures: &[Dx10Texture<'_>]) -> Result<Vec<u8>> {
    let mut seen = HashSet::with_capacity(textures.len());
    for texture in textures {
        split_asset_name(texture.name, "BA2")
            .wrap_err_with(|| format!("invalid BA2 entry {:?}", texture.name))?;
        ensure!(
            seen.insert(texture.name),
            "duplicate BA2 entry {:?}",
            texture.name
        );
        ensure!(
            texture.width > 0 && texture.height > 0,
            "BA2 DX10 dimensions must be non-zero"
        );
        ensure!(texture.mip_count > 0, "BA2 DX10 mip count must be non-zero");
        ensure!(
            !texture.pixels.is_empty(),
            "BA2 DX10 texture payload is empty"
        );
        ensure!(
            texture.pixels.len() <= MAX_ENTRY_SIZE,
            "BA2 DX10 texture exceeds the 1 GiB fixture limit"
        );
    }

    let file_count = u32::try_from(textures.len()).map_err(|_| eyre!("BA2 file count overflow"))?;
    let names_offset = HEADER_SIZE
        .checked_add(
            u32::try_from(textures.len())
                .map_err(|_| eyre!("BA2 file count overflow"))?
                .checked_mul(DX10_RECORD_SIZE + DX10_CHUNK_SIZE)
                .ok_or_else(|| eyre!("BA2 DX10 record table overflow"))?,
        )
        .ok_or_else(|| eyre!("BA2 DX10 record table overflow"))?;
    let names_size = names_size(textures.iter().map(|texture| texture.name))?;
    let payload_start = names_offset
        .checked_add(names_size)
        .ok_or_else(|| eyre!("BA2 DX10 name table overflow"))?;

    let mut payload_offsets = Vec::with_capacity(textures.len());
    let mut cursor = payload_start;
    for texture in textures {
        payload_offsets.push(u64::from(cursor));
        cursor = cursor
            .checked_add(
                u32::try_from(texture.pixels.len())
                    .map_err(|_| eyre!("BA2 DX10 payload exceeds 4 GiB"))?,
            )
            .ok_or_else(|| eyre!("BA2 DX10 payload table overflow"))?;
    }
    let total_size = usize::try_from(cursor).map_err(|_| eyre!("BA2 archive exceeds usize"))?;
    let chunk_header_size =
        u16::try_from(DX10_CHUNK_SIZE).map_err(|_| eyre!("BA2 DX10 chunk header overflow"))?;

    let mut bytes = Vec::with_capacity(total_size);
    bytes.extend_from_slice(b"BTDX");
    push_u32(&mut bytes, 1);
    bytes.extend_from_slice(b"DX10");
    push_u32(&mut bytes, file_count);
    push_u64(&mut bytes, u64::from(names_offset));

    for (index, texture) in textures.iter().enumerate() {
        bytes.extend_from_slice(&[0; 13]);
        bytes.push(1);
        push_u16(&mut bytes, chunk_header_size);
        push_u16(&mut bytes, texture.height);
        push_u16(&mut bytes, texture.width);
        bytes.push(texture.mip_count);
        bytes.push(texture.format);
        push_u16(&mut bytes, if texture.cubemap { 1 } else { 0 });
        debug_assert_eq!(bytes.len() as u32 % DX10_RECORD_SIZE, 0);

        push_u64(&mut bytes, payload_offsets[index]);
        push_u32(&mut bytes, 0);
        push_u32(
            &mut bytes,
            u32::try_from(texture.pixels.len())
                .map_err(|_| eyre!("BA2 DX10 unpacked size overflow"))?,
        );
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, u16::from(texture.mip_count - 1));
        push_u32(&mut bytes, 0);
    }
    debug_assert_eq!(bytes.len(), names_offset as usize);

    for texture in textures {
        push_u16(
            &mut bytes,
            u16::try_from(texture.name.len()).map_err(|_| eyre!("BA2 name exceeds 65535 bytes"))?,
        );
        bytes.extend_from_slice(texture.name.as_bytes());
    }
    for texture in textures {
        bytes.extend_from_slice(texture.pixels);
    }
    debug_assert_eq!(bytes.len(), total_size);
    Ok(bytes)
}

fn names_size<'a>(mut names: impl Iterator<Item = &'a str>) -> Result<u32> {
    names.try_fold(0u32, |total, name| {
        total
            .checked_add(
                2 + u32::try_from(name.len()).map_err(|_| eyre!("BA2 name length overflow"))?,
            )
            .ok_or_else(|| eyre!("BA2 name table overflow"))
    })
}

fn encode_payload(data: &[u8], compression: Compression) -> Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Zlib => Ok(miniz_oxide::deflate::compress_to_vec_zlib(data, 9)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    #[test]
    fn writes_general_archive_layout() {
        let entries = [Entry::new("textures/test.dds", b"DDS ")];
        let bytes = general(&entries, Compression::None).unwrap();
        assert_eq!(&bytes[..4], b"BTDX");
        assert_eq!(u32_at(&bytes, 4), 1);
        assert_eq!(&bytes[8..12], b"GNRL");
        assert_eq!(u32_at(&bytes, 12), 1);
        assert_eq!(u64_at(&bytes, 16), u64::from(24u32 + 36));
        assert_eq!(u64_at(&bytes, 40), u64::from(24u32 + 36 + 2 + 17));
        assert_eq!(u32_at(&bytes, 48), 0, "uncompressed packed size");
        assert_eq!(u32_at(&bytes, 52), 4, "unpacked size");
        assert_eq!(&bytes[62..], b"textures/test.ddsDDS ");
    }

    #[test]
    fn compresses_general_payloads() {
        let data = b"fixture payload".repeat(4);
        let entries = [Entry::new("meshes/test.nif", data.as_slice())];
        let bytes = general(&entries, Compression::Zlib).unwrap();
        let packed = u32_at(&bytes, 48) as usize;
        let unpacked = u32_at(&bytes, 52) as usize;
        let offset = u64_at(&bytes, 40) as usize;
        assert_eq!(unpacked, data.len());
        assert_ne!(packed, 0);

        let decoded =
            miniz_oxide::inflate::decompress_to_vec_zlib(&bytes[offset..offset + packed]).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn writes_dx10_record_and_chunk() {
        let pixels = [0xAB; 8];
        let texture = Dx10Texture::new("textures/test.dds", 4, 4, 71, &pixels);
        let bytes = dx10(&[texture]).unwrap();
        assert_eq!(&bytes[8..12], b"DX10");
        assert_eq!(bytes[24 + 13], 1, "chunk count");
        assert_eq!(u16::from_le_bytes(bytes[38..40].try_into().unwrap()), 24);
        assert_eq!(u16::from_le_bytes(bytes[40..42].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(bytes[42..44].try_into().unwrap()), 4);
        assert_eq!(bytes[44], 1, "mip count");
        assert_eq!(bytes[45], 71, "format");
        let offset = u64_at(&bytes, 48) as usize;
        assert_eq!(&bytes[offset..], &pixels);
    }

    #[test]
    fn writes_cube_map_dx10_flag() {
        let pixels = [0xCD; 8];
        let texture = Dx10Texture::new("textures/cube.dds", 4, 4, 71, &pixels).as_cubemap();
        let bytes = dx10(&[texture]).unwrap();
        assert_eq!(u16::from_le_bytes(bytes[46..48].try_into().unwrap()) & 1, 1);
    }

    #[test]
    fn rejects_unsafe_duplicate_or_empty_inputs() {
        assert!(general(&[Entry::new("../evil.dds", b"X")], Compression::None).is_err());
        assert!(
            general(
                &[
                    Entry::new("textures/a.dds", b"X"),
                    Entry::new("textures/a.dds", b"Y"),
                ],
                Compression::None
            )
            .is_err()
        );
        assert!(dx10(&[Dx10Texture::new("textures/a.dds", 0, 4, 71, &[1])]).is_err());
        assert!(dx10(&[Dx10Texture::new("textures/a.dds", 4, 4, 71, &[])]).is_err());
    }

    #[test]
    fn compressed_output_is_deterministic() {
        let entries = [Entry::new("textures/a.dds", b"payload")];
        assert_eq!(
            general(&entries, Compression::Zlib).unwrap(),
            general(&entries, Compression::Zlib).unwrap()
        );
    }
}
