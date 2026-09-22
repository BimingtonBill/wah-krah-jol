//! The generated interior cell and its reciprocal load door pair, through the
//! real ESM parser and the database export.

use converter::esm::{
    EsmParser,
    binary::{parse_group, parse_plugin_file, parse_record_header},
    exporter::validate_database,
    records::RawRecord,
};
use dummy_content::esm::{self, Plugin};
use rusqlite::{Connection, params};
use std::{fs, path::Path};

/// The bytes `dummy-content gen --with-interior` writes: one exterior cell,
/// its auto-load door into one interior cell, and the return door.
fn preset_plugin() -> Vec<u8> {
    let cells = [esm::PRESET_EXTERIOR_CELL];
    esm::plugin_with_interior(
        &Plugin {
            author: "OpenSkyrim dummy-content",
            worldspace: "GeneratedWorld",
            cells: &cells,
            model_path: "meshes/generated.nif",
            diffuse: "textures/generated_color.dds",
            normal_texture: "textures/generated_normal.dds",
        },
        &esm::PRESET_INTERIOR,
    )
    .unwrap()
}

fn write_plugin(directory: &Path) -> std::path::PathBuf {
    let path = directory.join("Skyrim.esm");
    fs::write(&path, preset_plugin()).unwrap();
    path
}

fn subrecord<'a>(record: &'a RawRecord, tag: &[u8; 4]) -> Option<&'a [u8]> {
    record
        .subrecords
        .iter()
        .find(|(candidate, _)| candidate.as_slice() == tag)
        .map(|(_, data)| data.as_slice())
}

fn subrecord_or_panic<'a>(record: &'a RawRecord, tag: &[u8; 4]) -> &'a [u8] {
    subrecord(record, tag).unwrap_or_else(|| {
        panic!(
            "{} record {:08X} has no {}",
            String::from_utf8_lossy(&record.record_type),
            record.form_id,
            String::from_utf8_lossy(tag)
        )
    })
}

/// An `EDID` as the parser hands it back: the bytes as they were written, NUL
/// terminator included.
fn editor_id(record: &RawRecord) -> String {
    String::from_utf8_lossy(subrecord(record, b"EDID").unwrap_or_default()).into_owned()
}

fn form_id(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

fn floats(bytes: &[u8]) -> [f32; 3] {
    let mut values = [0.0f32; 3];
    for (index, value) in values.iter_mut().enumerate() {
        *value = f32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap());
    }
    values
}

/// What the real parser reads out of the generated plugin: an interior `CELL`
/// with no grid and no worldspace, two `DOOR` base records whose `FNAM` carries
/// the auto-load bit, and a reciprocal pair of `XTEL`s whose arrival frames are
/// each door's own.
#[test]
fn generated_interior_plugin_holds_an_interior_cell_and_a_door_pair() {
    let directory = tempfile::tempdir().unwrap();
    let records = parse_plugin_file(&write_plugin(directory.path())).unwrap();

    let interior = records
        .iter()
        .find(|record| &record.record_type == b"CELL" && record.worldspace_form_id.is_none())
        .expect("the interior CELL record");
    assert!(
        interior.cell_form_id.is_none(),
        "an interior cell is no group's child"
    );
    assert_eq!(editor_id(interior), "GeneratedInterior\0");
    assert_eq!(
        subrecord(interior, b"FULL"),
        Some(b"Generated Interior\0".as_slice())
    );
    // `DATA`'s 0x01 bit marks the cell as an interior, and an interior has no
    // `XCLC` grid square.
    assert_eq!(subrecord(interior, b"DATA"), Some([0x01].as_slice()));
    assert_eq!(subrecord(interior, b"XCLC"), None);

    let exterior = records
        .iter()
        .find(|record| &record.record_type == b"CELL" && record.worldspace_form_id.is_some())
        .expect("the exterior CELL record");
    assert!(subrecord(exterior, b"XCLC").is_some());

    let doors: Vec<&RawRecord> = records
        .iter()
        .filter(|record| &record.record_type == b"DOOR")
        .collect();
    assert_eq!(doors.len(), 2, "one DOOR base record per door");
    let auto_load = doors
        .iter()
        .find(|record| editor_id(record) == "AutoLoadDoor01\0")
        .expect("the auto-load door's base record");
    let ordinary = doors
        .iter()
        .find(|record| editor_id(record) == "GeneratedDoor01\0")
        .expect("the ordinary door's base record");
    assert_eq!(
        subrecord(auto_load, b"FNAM"),
        Some([esm::AUTO_LOAD_FLAG].as_slice()),
        "the auto-load bit survives the write"
    );
    assert_eq!(subrecord(ordinary, b"FNAM"), Some([0x00].as_slice()));
    assert_eq!(
        subrecord(auto_load, b"MODL"),
        Some(b"meshes/generated.nif\0".as_slice())
    );

    let references: Vec<&RawRecord> = records
        .iter()
        .filter(|record| &record.record_type == b"REFR")
        .collect();
    assert_eq!(
        references.len(),
        3,
        "the static placement and the two doors"
    );
    let inside_ref = references
        .iter()
        .find(|record| record.cell_form_id == Some(interior.form_id))
        .expect("the interior door's reference");
    let outside_ref = references
        .iter()
        .find(|record| {
            record.cell_form_id == Some(exterior.form_id) && subrecord(record, b"XTEL").is_some()
        })
        .expect("the exterior door's reference");
    assert_eq!(
        form_id(subrecord_or_panic(inside_ref, b"NAME")),
        ordinary.form_id
    );
    assert_eq!(
        form_id(subrecord_or_panic(outside_ref, b"NAME")),
        auto_load.form_id
    );

    let inside_xtel = subrecord_or_panic(inside_ref, b"XTEL");
    let outside_xtel = subrecord_or_panic(outside_ref, b"XTEL");
    assert_eq!(inside_xtel.len(), 32, "Skyrim SE's XTEL");
    assert_eq!(outside_xtel.len(), 32);
    assert_eq!(
        form_id(inside_xtel),
        outside_ref.form_id,
        "the doors point at each other"
    );
    assert_eq!(form_id(outside_xtel), inside_ref.form_id);
    // The arrival frame is the door's own, not the position of the door it
    // leads to, and no `XTEL` flag bits are set.
    assert_eq!(floats(&inside_xtel[4..16]), [2048.0, 512.0, 0.0]);
    assert_eq!(floats(&inside_xtel[16..28]), [0.0, 0.0, 0.0]);
    assert_eq!(floats(&outside_xtel[4..16]), [128.0, 256.0, 0.0]);
    assert_eq!(&inside_xtel[28..], &[0, 0, 0, 0]);
    assert_eq!(
        floats(subrecord_or_panic(inside_ref, b"DATA")),
        [128.0, 512.0, 0.0],
        "the arrival point is not the door's own position"
    );
}

/// One `door_links` row: where it leads, and where the player arrives.
struct Link {
    destination_ref_id: i64,
    destination_cell_id: Option<i64>,
    destination_worldspace_id: Option<i64>,
    arrival: [f64; 3],
}

fn door_link(connection: &Connection, ref_id: i64) -> Link {
    connection
        .query_row(
            "SELECT destination_ref_id,destination_cell_id,destination_worldspace_id,pos_x,pos_y,pos_z
             FROM door_links WHERE ref_id=?1",
            [ref_id],
            |row| {
                Ok(Link {
                    destination_ref_id: row.get(0)?,
                    destination_cell_id: row.get(1)?,
                    destination_worldspace_id: row.get(2)?,
                    arrival: [row.get(3)?, row.get(4)?, row.get(5)?],
                })
            },
        )
        .unwrap()
}

/// The FormID of the reference in `cell` whose base record has `editor_id`.
fn door_reference(connection: &Connection, cell: i64, editor_id: &str) -> i64 {
    connection
        .query_row(
            "SELECT r.id FROM \"references\" r JOIN statics s ON s.id=r.base_form_id
             WHERE r.cell_id=?1 AND s.editor_id=?2",
            params![cell, editor_id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn generated_interior_plugin_exports_a_reciprocal_door_pair() {
    let directory = tempfile::tempdir().unwrap();
    let plugin_path = write_plugin(directory.path());
    let db_path = directory.path().join("skyrim_world.db");
    EsmParser::convert_plugins(std::slice::from_ref(&plugin_path), &db_path).unwrap();
    let connection = Connection::open(&db_path).unwrap();
    validate_database(&connection).unwrap();

    // The exterior cell keeps its grid square and worldspace; the interior cell
    // has neither, which is what makes it an interior.
    let exterior: i64 = connection
        .query_row(
            "SELECT id FROM cells WHERE worldspace_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (grid_x, grid_y, worldspace): (i64, i64, i64) = connection
        .query_row(
            "SELECT grid_x,grid_y,worldspace_id FROM cells WHERE id=?1",
            [exterior],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((grid_x, grid_y), (0, 0));
    let interior: i64 = connection
        .query_row(
            "SELECT id FROM cells WHERE worldspace_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (interior_name, interior_grid): (String, Option<i64>) = connection
        .query_row(
            "SELECT interior_name,grid_x FROM cells WHERE id=?1",
            [interior],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    // `extract_cell_info` stores the `EDID` bytes as they are, trailing NUL
    // included.
    assert_eq!(interior_name, "GeneratedInterior\0");
    assert_eq!(interior_grid, None);

    // Exterior -> interior: the destination is a cell of no worldspace.
    let outside = door_reference(&connection, exterior, "AutoLoadDoor01");
    let inside = door_reference(&connection, interior, "GeneratedDoor01");
    let link = door_link(&connection, outside);
    assert_eq!(link.destination_ref_id, inside);
    assert_eq!(link.destination_cell_id, Some(interior));
    assert_eq!(link.destination_worldspace_id, None);
    assert_eq!(link.arrival, [128.0, 256.0, 0.0]);
    // Interior -> exterior: the destination carries the worldspace.
    let link = door_link(&connection, inside);
    assert_eq!(link.destination_ref_id, outside);
    assert_eq!(link.destination_cell_id, Some(exterior));
    assert_eq!(link.destination_worldspace_id, Some(worldspace));
    assert_eq!(link.arrival, [2048.0, 512.0, 0.0]);

    let (is_exterior, local_x, local_y): (i64, Option<f64>, Option<f64>) = connection
        .query_row(
            "SELECT is_exterior,local_x,local_y FROM \"references\" WHERE id=?1",
            [inside],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (is_exterior, local_x, local_y),
        (0, None, None),
        "a reference inside has no world-space index"
    );

    // The `DOOR` base records themselves, as the `statics` table holds them: the
    // auto-load door is named for the marker it stands for, so a consumer that
    // keys on the editor id (`crates/engine/src/world/database.rs`, its
    // auto-load column) tells the two doors of the pair apart.
    let bases: Vec<(String, Option<String>)> = connection
        .prepare("SELECT editor_id,model_path FROM statics ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        bases,
        vec![
            (
                "GeneratedStatic".to_owned(),
                Some("meshes/generated.nif".to_owned())
            ),
            (
                "AutoLoadDoor01".to_owned(),
                Some("meshes/generated.nif".to_owned())
            ),
            (
                "GeneratedDoor01".to_owned(),
                Some("meshes/generated.nif".to_owned())
            ),
        ]
    );
}

/// `parse_plugin_file` without file IO, so a prefix of the fixture can be swept.
fn parse_prefix(bytes: &[u8]) {
    let Ok((rest, header)) = parse_record_header(bytes) else {
        return;
    };
    if &header.type_tag != b"TES4" || header.data_size as usize > rest.len() {
        return;
    }
    let mut records = Vec::new();
    let _ = parse_group(&rest[header.data_size as usize..], None, None, &mut records);
}

/// Mirrors the sweep `crates/converter/src/esm/binary.rs` runs over the
/// exterior-only fixture, over the bytes that carry the interior cell group and
/// the two load doors.
#[test]
fn generated_interior_plugin_never_panics_under_truncation_or_mutation() {
    let bytes = preset_plugin();
    for length in 0..bytes.len() {
        assert!(
            std::panic::catch_unwind(|| parse_prefix(&bytes[..length])).is_ok(),
            "ESM parser panicked at length {length}"
        );
    }
    let mut rng = dummy_content::rng::Rng::new(39);
    for _ in 0..256 {
        let mut mutated = bytes.clone();
        let index = rng.next_u64() as usize % mutated.len();
        mutated[index] ^= 0xff;
        assert!(
            std::panic::catch_unwind(|| parse_prefix(&mutated)).is_ok(),
            "ESM parser panicked on mutation at {index}"
        );
    }
}
