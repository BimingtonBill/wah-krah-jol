//! Deterministic Skyrim SE plugin (`.esm`) fixtures.
//!
//! The writer emits a minimal worldspace with exterior cells, terrain, a
//! static, a texture set and landscape texture, plus one placement reference
//! per cell. A spec can also carry one interior cell joined to an exterior
//! cell by a reciprocal pair of load doors, which is what a caller needs to
//! exercise interiors, `XTEL` door links and cell-to-cell crossings without a
//! local game installation. Only the record types consumed by the converter's
//! ESM parser, exporter and cell cache are produced.
//!
//! The `DOOR` bases carry a `MODL` the way retail data does, but the converter's
//! exporter fills its `statics` table from `STAT`, `MSTT` and `FURN` only, so a
//! reference that places one of them exports with no model at all until that
//! changes; see [`Door::model_path`].

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
/// The `DOOR` base record the exterior door of an [`Interior`] places.
const EXTERIOR_DOOR_FORM_ID: u32 = 0x0000_0005;
/// The `DOOR` base record the interior door of an [`Interior`] places.
const INTERIOR_DOOR_FORM_ID: u32 = 0x0000_0006;
const CELL_BASE_FORM_ID: u32 = 0x0000_0010;
const CELL_FORM_STRIDE: u32 = 0x10;
/// A door reference's offset inside its cell's block of [`CELL_FORM_STRIDE`]
/// FormIDs: past the cell itself, its `LAND` and the static reference.
const DOOR_REF_OFFSET: u32 = 3;
/// The interior cell's FormID, past every id [`MAX_CELLS`] exterior cells can
/// hand out.
const INTERIOR_CELL_FORM_ID: u32 = 0x0001_0000;
const INTERIOR_DOOR_REF_FORM_ID: u32 = INTERIOR_CELL_FORM_ID + 1;
const LAND_SIDE: usize = 33;
const CELL_SIZE: f32 = 4096.0;
const RECORD_VERSION: u16 = 44;
const HEADER_RECORD_SIZE: usize = 24;
const GROUP_HEADER_SIZE: usize = 24;
const MAX_CELLS: usize = 0x0f00;
/// The highest FormID [`MAX_CELLS`] exterior cells can hand out is the last
/// cell's door reference; every exterior id has to stay below the interior
/// cell's own block, or an interior record would collide with an exterior one.
const _: () = assert!(
    CELL_BASE_FORM_ID + (MAX_CELLS as u32 - 1) * CELL_FORM_STRIDE + DOOR_REF_OFFSET
        < INTERIOR_CELL_FORM_ID,
    "the exterior FormID block has grown into the interior block"
);
/// `XTEL`'s length: the destination reference's FormID, the arrival position
/// and rotation as six little-endian `f32`s, then a four-byte flag word.
const XTEL_SIZE: usize = 32;
/// `CELL` `DATA` flag `0x01`: the cell is an interior, so it has no grid square
/// and belongs to no worldspace.
const INTERIOR_CELL_FLAG: u8 = 0x01;

/// `DOOR` `FNAM` flag `0x02`: an auto-load door, which crosses the moment an
/// actor walks into it rather than when the use key is pressed.
pub const AUTO_LOAD_FLAG: u8 = 0x02;

/// One exterior cell of the generated worldspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Grid X coordinate.
    pub grid_x: i32,
    /// Grid Y coordinate.
    pub grid_y: i32,
}

/// A load door: the `DOOR` base record a reference places, where that reference
/// stands, and the arrival frame the door's own `XTEL` carries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Door<'a> {
    /// `EDID` of the `DOOR` base record. The two doors of one [`Interior`] need
    /// two distinct editor ids, because they are two base records.
    pub editor_id: &'a str,
    /// `MODL` model path of the `DOOR` base record.
    ///
    /// The path is written to the base record, but the converter's exporter
    /// fills its `statics` table from `STAT`, `MSTT` and `FURN` records only,
    /// so a `REFR` that places this door exports as a reference without a model:
    /// `world-inspect` counts it under `references_without_model`, its entry in
    /// an `assets` listing never appears, and its mesh never reaches the GLB
    /// pipeline. That is a converter gap, not a fixture one - the fixture writes
    /// the `MODL` a retail plugin carries - and it stays until the exporter
    /// learns `DOOR`. `crates/converter/tests/fixture_interior_pipeline.rs`
    /// asserts the resulting count so the gap cannot go quiet.
    pub model_path: &'a str,
    /// `FNAM` flags of the `DOOR` base record; [`AUTO_LOAD_FLAG`] marks an
    /// auto-load door.
    pub flags: u8,
    /// `DATA` position of the reference, in Creation units.
    pub position: [f32; 3],
    /// `DATA` rotation of the reference, in radians.
    pub rotation: [f32; 3],
    /// Arrival position this door's own `XTEL` stores: where the player lands
    /// after using the door, expressed in the destination cell. Deliberately
    /// not the destination door's position - the game stores its own frame,
    /// and the two differ by tens to hundreds of units in retail data.
    pub arrival_position: [f32; 3],
    /// Arrival rotation this door's own `XTEL` stores, in radians.
    pub arrival_rotation: [f32; 3],
}

/// An interior cell joined to one exterior cell of the worldspace by a
/// reciprocal pair of load doors: each door's `XTEL` names the other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interior<'a> {
    /// `EDID` of the interior cell.
    pub editor_id: &'a str,
    /// `FULL` display name of the interior cell.
    pub full_name: &'a str,
    /// The exterior cell the outside door stands in; one of
    /// [`Plugin::cells`].
    pub exterior_cell: Cell,
    /// The door in the exterior cell, leading in.
    pub outside: Door<'a>,
    /// The door in the interior cell, leading back out.
    pub inside: Door<'a>,
}

/// Description of a generated plugin.
///
/// The worldspace and its exterior cells, with the assets they reference. An
/// interior cell and its load doors are described by [`Interior`] and written
/// by [`plugin_with_interior`].
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
    write_plugin(spec, None)
}

/// Generates the same plugin as [`plugin`], with `interior` and its two load
/// doors.
///
/// The interior part is written after the exterior world, and the exterior
/// records above it are exactly the ones [`plugin`] writes: the only byte an
/// interior moves is the world group's own size field, which grows by the
/// reference of the door standing in the exterior cell.
pub fn plugin_with_interior(spec: &Plugin<'_>, interior: &Interior<'_>) -> Result<Vec<u8>> {
    write_plugin(spec, Some(interior))
}

fn write_plugin(spec: &Plugin<'_>, interior: Option<&Interior<'_>>) -> Result<Vec<u8>> {
    validate(spec, interior)?;

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
        if let Some(interior) = interior
            && interior.exterior_cell == *cell
        {
            children.extend_from_slice(&door_reference_record(
                cell_form_id + DOOR_REF_OFFSET,
                EXTERIOR_DOOR_FORM_ID,
                &interior.outside,
                INTERIOR_DOOR_REF_FORM_ID,
            )?);
        }
        world_children.extend_from_slice(&group(8, cell_form_id, &children)?);
    }
    bytes.extend_from_slice(&group(1, WRLD_FORM_ID, &world_children)?);

    if let Some(interior) = interior {
        let index = exterior_cell_index(spec, interior)?;
        let exterior_door_ref = cell_form_id(index)? + DOOR_REF_OFFSET;
        bytes.extend_from_slice(&door_record(EXTERIOR_DOOR_FORM_ID, &interior.outside)?);
        bytes.extend_from_slice(&door_record(INTERIOR_DOOR_FORM_ID, &interior.inside)?);
        bytes.extend_from_slice(&interior_group(interior, exterior_door_ref)?);
    }
    Ok(bytes)
}

fn validate(spec: &Plugin<'_>, interior: Option<&Interior<'_>>) -> Result<()> {
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

    if let Some(interior) = interior {
        // The outside door is a reference of an exterior cell, so that cell has
        // to be one of the generated ones.
        exterior_cell_index(spec, interior)?;
        for (label, value) in [
            ("interior editor id", interior.editor_id),
            ("interior display name", interior.full_name),
            ("exterior door editor id", interior.outside.editor_id),
            ("interior door editor id", interior.inside.editor_id),
        ] {
            ensure!(!value.is_empty(), "ESM {label} is empty");
            ensure!(
                value.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
                "ESM {label} is not printable ASCII: {value:?}"
            );
        }
        ensure!(
            interior.outside.editor_id != interior.inside.editor_id,
            "ESM door pair reuses the editor id {:?}; two DOOR records need two names",
            interior.outside.editor_id
        );
        for (side, door) in [
            ("exterior", &interior.outside),
            ("interior", &interior.inside),
        ] {
            split_asset_name(door.model_path, &format!("ESM {side} door model"))?;
            for (field, values) in [
                ("position", door.position),
                ("rotation", door.rotation),
                ("arrival position", door.arrival_position),
                ("arrival rotation", door.arrival_rotation),
            ] {
                ensure!(
                    values.iter().all(|value| value.is_finite()),
                    "ESM {side} door {field} is not finite: {values:?}"
                );
            }
        }
    }
    Ok(())
}

/// The index of the exterior cell an interior's door stands in.
fn exterior_cell_index(spec: &Plugin<'_>, interior: &Interior<'_>) -> Result<usize> {
    spec.cells
        .iter()
        .position(|cell| *cell == interior.exterior_cell)
        .ok_or_else(|| {
            eyre!(
                "ESM interior {} is not attached to a generated exterior cell",
                interior.editor_id
            )
        })
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

/// A `DOOR` base record: its editor id, the model a reference of it draws, and
/// its one-byte `FNAM` flags.
fn door_record(form_id: u32, door: &Door<'_>) -> Result<Vec<u8>> {
    record(
        *b"DOOR",
        form_id,
        &[
            (*b"EDID", cstring(door.editor_id)),
            (*b"MODL", cstring(door.model_path)),
            (*b"FNAM", vec![door.flags]),
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

/// The interior `CELL`: an editor id, a `FULL` display name and a `DATA` flag
/// byte whose [`INTERIOR_CELL_FLAG`] bit marks it as an interior. It carries no
/// `XCLC`, because an interior has no grid square, and it sits in no worldspace
/// group. The converter stores it with a NULL grid and worldspace and reads its
/// editor id as the cell's name.
fn interior_cell_record(interior: &Interior<'_>) -> Result<Vec<u8>> {
    record(
        *b"CELL",
        INTERIOR_CELL_FORM_ID,
        &[
            (*b"EDID", cstring(interior.editor_id)),
            (*b"FULL", cstring(interior.full_name)),
            (*b"DATA", vec![INTERIOR_CELL_FLAG]),
        ],
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

/// A `REFR` that places `base_form_id`'s `DOOR` record, with `door`'s
/// transform and the `XTEL` the door crosses on: the destination reference's
/// FormID, the arrival position and rotation, and a four-byte flag word -
/// [`XTEL_SIZE`] bytes, no flag bits set.
///
/// `destination_ref_id` is written as the fixture's own local id, and the
/// converter's load-order remap leaves it that way: remapping rewrites only the
/// subrecords `is_form_id_subrecord` recognises as 4-byte FormIDs, and `XTEL`
/// is not one of them, so the id reaches a database unchanged. That is correct
/// only while this plugin owns load-order index 0, the single-plugin case
/// `dummy-content gen` writes - a second plugin needs the remap extended to
/// `XTEL`. Nothing consumes `XTEL` yet either, so the link is only as good as
/// the fixture's own reader (`crates/converter/tests/fixture_doors.rs`).
fn door_reference_record(
    form_id: u32,
    base_form_id: u32,
    door: &Door<'_>,
    destination_ref_id: u32,
) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(24);
    for value in door.position.iter().chain(door.rotation.iter()) {
        data.extend_from_slice(&value.to_le_bytes());
    }
    let mut xtel = Vec::with_capacity(XTEL_SIZE);
    xtel.extend_from_slice(&destination_ref_id.to_le_bytes());
    xtel.extend_from_slice(&floats(&door.arrival_position));
    xtel.extend_from_slice(&floats(&door.arrival_rotation));
    xtel.extend_from_slice(&0u32.to_le_bytes());
    record(
        *b"REFR",
        form_id,
        &[
            (*b"NAME", base_form_id.to_le_bytes().to_vec()),
            (*b"DATA", data),
            (*b"XTEL", xtel),
        ],
    )
}

/// The interior cell group: `GRUP` type 2 (interior block) around type 3
/// (sub-block), the nesting a plugin puts an interior cell in, with the cell's
/// own type 6 (cell children) group holding the return door as a persistent
/// reference. Both block labels are 0: they only sort interior cells into
/// blocks for a reader that groups them, and the converter's group walk takes
/// an owned cell from the type 6 label below, never from a block label.
fn interior_group(interior: &Interior<'_>, exterior_door_ref_form_id: u32) -> Result<Vec<u8>> {
    let mut cell_children = Vec::new();
    cell_children.extend_from_slice(&door_reference_record(
        INTERIOR_DOOR_REF_FORM_ID,
        INTERIOR_DOOR_FORM_ID,
        &interior.inside,
        exterior_door_ref_form_id,
    )?);
    let mut sub_block = interior_cell_record(interior)?;
    sub_block.extend_from_slice(&group(
        6,
        INTERIOR_CELL_FORM_ID,
        &group(8, INTERIOR_CELL_FORM_ID, &cell_children)?,
    )?);
    group(2, 0, &group(3, 0, &sub_block)?)
}

fn floats(values: &[f32; 3]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
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

/// The exterior cell the `--with-interior` preset hangs its door on: grid
/// (0, 0) of the generated worldspace, the square the crate's other fixtures
/// place their static in.
pub const PRESET_EXTERIOR_CELL: Cell = Cell {
    grid_x: 0,
    grid_y: 0,
};

/// The interior cell and reciprocal load door pair the `--with-interior`
/// preset writes, and the fixture `crates/converter/tests/fixture_doors.rs`
/// converts.
///
/// The outside door is an auto-load door ([`AUTO_LOAD_FLAG`], editor id
/// `AutoLoadDoor01`) and the inside door is an ordinary one, so a fixture can
/// exercise both ways a load door opens. Neither door stands on the arrival
/// point that leads to it, as the game's own `XTEL` data does not. Both doors
/// draw the fixture's generated mesh ([`crate::layout::GENERATED_MODEL_PATH`],
/// the only model the crate's default data tree writes); to exercise a marker
/// model's own path a caller has to describe its own [`Interior`].
///
/// Both doors do carry a `MODL`, but the mesh never reaches the converted
/// world: the exporter fills `statics` from `STAT`, `MSTT` and `FURN` only, so
/// the two door references export without a model until that changes - see
/// [`Door::model_path`].
pub const PRESET_INTERIOR: Interior<'static> = Interior {
    editor_id: "GeneratedInterior",
    full_name: "Generated Interior",
    exterior_cell: PRESET_EXTERIOR_CELL,
    outside: Door {
        editor_id: "AutoLoadDoor01",
        model_path: crate::layout::GENERATED_MODEL_PATH,
        flags: AUTO_LOAD_FLAG,
        position: [2048.0, 1024.0, 0.0],
        rotation: [0.0, 0.0, 0.0],
        arrival_position: [128.0, 256.0, 0.0],
        arrival_rotation: [0.0, 0.0, 0.0],
    },
    inside: Door {
        editor_id: "GeneratedDoor01",
        model_path: crate::layout::GENERATED_MODEL_PATH,
        flags: 0,
        position: [128.0, 512.0, 0.0],
        rotation: [0.0, 0.0, 0.0],
        arrival_position: [2048.0, 512.0, 0.0],
        arrival_rotation: [0.0, 0.0, 0.0],
    },
};

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
            author: crate::layout::GENERATED_AUTHOR,
            worldspace: crate::layout::GENERATED_WORLDSPACE,
            cells: &CELLS,
            model_path: crate::layout::GENERATED_MODEL_PATH,
            diffuse: crate::layout::GENERATED_DIFFUSE_PATH,
            normal_texture: crate::layout::GENERATED_NORMAL_PATH,
        }
    }

    /// Every group, record and subrecord tag in `bytes`, in file order.
    ///
    /// Walks the header sizes rather than matching four-byte windows, so a tag
    /// spelled inside a payload - an `EDID`, a `MODL` path, the bytes of an
    /// `f32` - can never be counted as a record.
    fn tags(bytes: &[u8]) -> Vec<[u8; 4]> {
        /// Subrecords of one record's payload: tag, `u16` length, contents.
        fn subrecords(payload: &[u8], found: &mut Vec<[u8; 4]>) {
            let mut offset = 0;
            while offset + 6 <= payload.len() {
                let tag: [u8; 4] = payload[offset..offset + 4].try_into().unwrap();
                let length = u16::from_le_bytes(payload[offset + 4..offset + 6].try_into().unwrap())
                    as usize;
                found.push(tag);
                offset += 6 + length;
            }
        }

        fn walk(bytes: &[u8], found: &mut Vec<[u8; 4]>) {
            let mut offset = 0;
            while offset + HEADER_RECORD_SIZE <= bytes.len() {
                let tag: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
                let size =
                    u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
                found.push(tag);
                if tag == *b"GRUP" {
                    // A group's size counts its own 24-byte header, so the group
                    // ends at `offset + size` and its children start past the
                    // header; a record's size counts only its payload.
                    let children = bytes.get(offset + GROUP_HEADER_SIZE..offset + size);
                    let Some(children) = children else { return };
                    walk(children, found);
                    offset += size;
                } else {
                    let payload =
                        bytes.get(offset + HEADER_RECORD_SIZE..offset + HEADER_RECORD_SIZE + size);
                    if let Some(payload) = payload {
                        subrecords(payload, found);
                    }
                    offset += HEADER_RECORD_SIZE + size;
                }
            }
        }

        let mut found = Vec::new();
        walk(bytes, &mut found);
        found
    }

    /// How many tags in `bytes` are `tag`.
    fn count(bytes: &[u8], tag: &[u8; 4]) -> usize {
        tags(bytes).iter().filter(|found| *found == tag).count()
    }

    #[test]
    fn writes_tes4_header_and_groups() {
        let bytes = plugin(&spec()).unwrap();
        assert_eq!(&bytes[..4], b"TES4");
        let tags = tags(&bytes);
        for tag in [b"GRUP", b"WRLD", b"LAND"] {
            assert!(tags.contains(tag), "{tag:?} is missing from {tags:?}");
        }
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(plugin(&spec()).unwrap(), plugin(&spec()).unwrap());
        assert_eq!(
            plugin_with_interior(&spec(), &PRESET_INTERIOR).unwrap(),
            plugin_with_interior(&spec(), &PRESET_INTERIOR).unwrap()
        );
    }

    /// FNV-1a of [`plugin`]'s output, recorded from the writer that the
    /// interiors commit left byte-identical.
    ///
    /// The guard used to re-run a copy of the pre-interior writer; the copy is
    /// gone, and this constant stands in for it. A deliberate change to the
    /// exterior bytes refreshes it in the same commit, which is what keeps the
    /// change visible.
    const EXTERIOR_ONLY_HASH: u64 = 0xECE9_D84B_E35F_6B24;

    /// FNV-1a over every byte of `bytes`.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    #[test]
    fn exterior_only_output_is_unchanged() {
        assert_eq!(
            fnv1a(&plugin(&spec()).unwrap()),
            EXTERIOR_ONLY_HASH,
            "the exterior-only bytes moved; if that is intended, refresh the constant"
        );
    }

    #[test]
    fn an_interior_is_appended_after_the_exterior_world() {
        let exterior_only = plugin(&spec()).unwrap();
        let with_interior = plugin_with_interior(&spec(), &PRESET_INTERIOR).unwrap();
        assert_eq!(count(&exterior_only, b"DOOR"), 0);
        assert_eq!(count(&exterior_only, b"XTEL"), 0);
        // Every record above the world group is byte-identical, and the interior
        // part starts at or after the point the exterior-only plugin ends.
        let world = with_interior
            .windows(4)
            .position(|window| window == b"WRLD")
            .expect("the WRLD record");
        assert_eq!(&with_interior[..world], &exterior_only[..world]);
        let doors = with_interior
            .windows(4)
            .position(|window| window == b"DOOR")
            .expect("the DOOR records");
        assert!(doors >= exterior_only.len());
        assert_eq!(count(&with_interior, b"DOOR"), 2, "one record per door");
        assert_eq!(count(&with_interior, b"XTEL"), 2, "one link per door");
        assert_eq!(count(&with_interior, b"FNAM"), 2, "one FNAM per door");
        assert_eq!(count(&with_interior, b"FULL"), 1, "the interior's name");
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

    #[test]
    fn rejects_invalid_interiors() {
        for mutate in [
            (|interior: &mut Interior<'_>| interior.editor_id = "") as fn(&mut Interior<'_>),
            |interior| interior.full_name = "",
            |interior| interior.inside.editor_id = interior.outside.editor_id,
            |interior| interior.outside.model_path = "../escape.nif",
            |interior| interior.inside.position = [f32::NAN, 0.0, 0.0],
            |interior| interior.outside.arrival_rotation = [0.0, f32::INFINITY, 0.0],
            |interior| {
                interior.exterior_cell = Cell {
                    grid_x: 7,
                    grid_y: 7,
                }
            },
        ] {
            let mut invalid = PRESET_INTERIOR;
            mutate(&mut invalid);
            assert!(
                plugin_with_interior(&spec(), &invalid).is_err(),
                "invalid interior {invalid:?} was accepted"
            );
        }
    }
}
