//! Deterministic Skyrim SE plugin (`.esm`) fixtures.
//!
//! The writer emits a minimal worldspace with exterior cells, terrain, a
//! static, a texture set and landscape texture, plus one placement reference
//! per cell. Only the record types consumed by the converter's ESM parser,
//! exporter and cell cache are produced.

use crate::path::split_asset_name;
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};

const TES4_FORM_ID: u32 = 0;
const WRLD_FORM_ID: u32 = 0x0000_0001;
const TXST_FORM_ID: u32 = 0x0000_0002;
const STAT_FORM_ID: u32 = 0x0000_0003;
const LTEX_FORM_ID: u32 = 0x0000_0004;
const CELL_BASE_FORM_ID: u32 = 0x0000_0010;
const CELL_FORM_STRIDE: u32 = 0x10;
const LAND_SIDE: usize = 33;
const CELL_SIZE: f32 = 4096.0;
const RECORD_VERSION: u16 = 44;
const HEADER_RECORD_SIZE: usize = 24;
const GROUP_HEADER_SIZE: usize = 24;
const MAX_CELLS: usize = 0x0f00;

/// One exterior cell of the generated worldspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Grid X coordinate.
    pub grid_x: i32,
    /// Grid Y coordinate.
    pub grid_y: i32,
}

/// Description of a generated plugin.
#[derive(Debug, Clone, Copy)]
pub struct Plugin<'a> {
    /// Author string stored in the `TES4` header.
    pub author: &'a str,
    /// Editor id of the generated worldspace.
    pub worldspace: &'a str,
    /// Exterior cells to generate; all get flat terrain.
    pub cells: &'a [Cell],
    /// Model path referenced by the generated static.
    pub model_path: &'a str,
    /// Diffuse texture path referenced by the generated texture set.
    pub diffuse: &'a str,
    /// Normal texture path referenced by the generated texture set.
    pub normal_texture: &'a str,
}

/// Generates a minimal Skyrim SE plugin.
pub fn plugin(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    validate(spec)?;

    let mut bytes = header_record(spec)?;
    bytes.extend_from_slice(&texture_set_record(spec)?);
    bytes.extend_from_slice(&static_record(spec)?);
    bytes.extend_from_slice(&landscape_texture_record()?);
    bytes.extend_from_slice(&worldspace_record(spec)?);

    let mut world_children = Vec::new();
    for (index, cell) in spec.cells.iter().enumerate() {
        let cell_form_id = cell_form_id(index)?;
        world_children.extend_from_slice(&cell_record(cell_form_id, cell)?);
        let mut children = Vec::new();
        children.extend_from_slice(&land_record(cell_form_id + 1)?);
        children.extend_from_slice(&reference_record(cell_form_id + 2, cell)?);
        world_children.extend_from_slice(&group(8, cell_form_id, &children)?);
    }
    bytes.extend_from_slice(&group(1, WRLD_FORM_ID, &world_children)?);
    Ok(bytes)
}

fn validate(spec: &Plugin<'_>) -> Result<()> {
    for (label, value) in [("author", spec.author), ("worldspace", spec.worldspace)] {
        ensure!(!value.is_empty(), "ESM {label} is empty");
        ensure!(
            value.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
            "ESM {label} is not printable ASCII: {value:?}"
        );
    }
    ensure!(!spec.cells.is_empty(), "ESM worldspace has no cells");
    ensure!(
        spec.cells.len() <= MAX_CELLS,
        "ESM cell count exceeds {MAX_CELLS}"
    );
    split_asset_name(spec.model_path, "ESM model")?;
    split_asset_name(spec.diffuse, "ESM diffuse")?;
    split_asset_name(spec.normal_texture, "ESM normal")?;
    Ok(())
}

fn header_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    let mut hedr = Vec::with_capacity(12);
    hedr.extend_from_slice(&1.7f32.to_le_bytes());
    hedr.extend_from_slice(&0u32.to_le_bytes());
    hedr.extend_from_slice(&0u32.to_le_bytes());
    record(
        *b"TES4",
        TES4_FORM_ID,
        &[(*b"HEDR", hedr), (*b"CNAM", cstring(spec.author))],
    )
}

fn texture_set_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    record(
        *b"TXST",
        TXST_FORM_ID,
        &[
            (*b"EDID", cstring("GeneratedTextures")),
            (*b"TX00", cstring(spec.diffuse)),
            (*b"TX01", cstring(spec.normal_texture)),
        ],
    )
}

fn static_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    record(
        *b"STAT",
        STAT_FORM_ID,
        &[
            (*b"EDID", cstring("GeneratedStatic")),
            (*b"MODL", cstring(spec.model_path)),
        ],
    )
}

fn landscape_texture_record() -> Result<Vec<u8>> {
    record(
        *b"LTEX",
        LTEX_FORM_ID,
        &[
            (*b"EDID", cstring("GeneratedLandscape")),
            (*b"TNAM", TXST_FORM_ID.to_le_bytes().to_vec()),
            (*b"HNAM", 0u16.to_le_bytes().to_vec()),
        ],
    )
}

fn worldspace_record(spec: &Plugin<'_>) -> Result<Vec<u8>> {
    record(
        *b"WRLD",
        WRLD_FORM_ID,
        &[(*b"EDID", cstring(spec.worldspace))],
    )
}

fn cell_record(form_id: u32, cell: &Cell) -> Result<Vec<u8>> {
    let mut xclc = Vec::with_capacity(8);
    xclc.extend_from_slice(&cell.grid_x.to_le_bytes());
    xclc.extend_from_slice(&cell.grid_y.to_le_bytes());
    record(
        *b"CELL",
        form_id,
        &[(*b"EDID", cstring("GeneratedCell")), (*b"XCLC", xclc)],
    )
}

fn land_record(form_id: u32) -> Result<Vec<u8>> {
    let mut vhgt = Vec::with_capacity(4 + LAND_SIDE * LAND_SIDE + 3);
    vhgt.extend_from_slice(&0.0f32.to_le_bytes());
    vhgt.extend(std::iter::repeat_n(0u8, LAND_SIDE * LAND_SIDE));
    vhgt.extend_from_slice(&[0u8; 3]);
    let mut btxt = Vec::with_capacity(8);
    btxt.extend_from_slice(&LTEX_FORM_ID.to_le_bytes());
    btxt.extend_from_slice(&[0u8, 0u8]);
    btxt.extend_from_slice(&0u16.to_le_bytes());
    record(*b"LAND", form_id, &[(*b"VHGT", vhgt), (*b"BTXT", btxt)])
}

fn reference_record(form_id: u32, cell: &Cell) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(24);
    let center_x = cell.grid_x as f32 * CELL_SIZE + CELL_SIZE * 0.5;
    let center_y = cell.grid_y as f32 * CELL_SIZE + CELL_SIZE * 0.5;
    for value in [center_x, center_y, 0.0, 0.0, 0.0, 0.0] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    record(
        *b"REFR",
        form_id,
        &[
            (*b"NAME", STAT_FORM_ID.to_le_bytes().to_vec()),
            (*b"DATA", data),
        ],
    )
}

fn cell_form_id(index: usize) -> Result<u32> {
    let offset = u32::try_from(index)
        .ok()
        .and_then(|index| index.checked_mul(CELL_FORM_STRIDE))
        .ok_or_else(|| eyre!("ESM cell index overflow"))?;
    CELL_BASE_FORM_ID
        .checked_add(offset)
        .ok_or_else(|| eyre!("ESM cell form id overflow"))
}

fn record(tag: [u8; 4], form_id: u32, subrecords: &[([u8; 4], Vec<u8>)]) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    for (sub_tag, data) in subrecords {
        let length = u16::try_from(data.len())
            .map_err(|_| eyre!("ESM subrecord {:?} exceeds 65535 bytes", sub_tag))?;
        payload.extend_from_slice(sub_tag);
        payload.extend_from_slice(&length.to_le_bytes());
        payload.extend_from_slice(data);
    }
    let mut bytes = Vec::with_capacity(HEADER_RECORD_SIZE + payload.len());
    bytes.extend_from_slice(&tag);
    bytes.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| eyre!("ESM record payload overflow"))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&form_id.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&RECORD_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

fn group(group_type: i32, label: u32, content: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        content.len() < u32::MAX as usize,
        "ESM group payload overflow"
    );
    let data_size = u32::try_from(content.len())
        .ok()
        .and_then(|length| length.checked_add(GROUP_HEADER_SIZE as u32))
        .ok_or_else(|| eyre!("ESM group payload overflow"))?;
    let mut bytes = Vec::with_capacity(GROUP_HEADER_SIZE + content.len());
    bytes.extend_from_slice(b"GRUP");
    bytes.extend_from_slice(&data_size.to_le_bytes());
    bytes.extend_from_slice(&label.to_le_bytes());
    bytes.extend_from_slice(&group_type.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 8]);
    bytes.extend_from_slice(content);
    Ok(bytes)
}

fn cstring(value: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(value.len() + 1);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELLS: [Cell; 4] = [
        Cell {
            grid_x: 0,
            grid_y: 0,
        },
        Cell {
            grid_x: 1,
            grid_y: 0,
        },
        Cell {
            grid_x: 0,
            grid_y: 1,
        },
        Cell {
            grid_x: 1,
            grid_y: 1,
        },
    ];

    fn spec() -> Plugin<'static> {
        Plugin {
            author: "OpenSkyrim dummy-content",
            worldspace: "GeneratedWorld",
            cells: &CELLS,
            model_path: "meshes/generated.nif",
            diffuse: "textures/generated_color.dds",
            normal_texture: "textures/generated_normal.dds",
        }
    }

    #[test]
    fn writes_tes4_header_and_groups() {
        let bytes = plugin(&spec()).unwrap();
        assert_eq!(&bytes[..4], b"TES4");
        assert!(bytes.windows(4).any(|window| window == b"GRUP"));
        assert!(bytes.windows(4).any(|window| window == b"WRLD"));
        assert!(bytes.windows(4).any(|window| window == b"LAND"));
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(plugin(&spec()).unwrap(), plugin(&spec()).unwrap());
    }

    #[test]
    fn rejects_invalid_specs() {
        let mut invalid = spec();
        invalid.cells = &[];
        assert!(plugin(&invalid).is_err());
        let mut invalid = spec();
        invalid.model_path = "../escape.nif";
        assert!(plugin(&invalid).is_err());
        let mut invalid = spec();
        invalid.worldspace = "";
        assert!(plugin(&invalid).is_err());
    }
}
