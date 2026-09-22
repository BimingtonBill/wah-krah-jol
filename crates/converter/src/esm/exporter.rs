use crate::esm::{
    extractors::{SubrecordView, extract_cell_info, extract_land_data, serialize_subrecords},
    records::RawRecord,
};
use crate::{
    asset_path::{AssetKind, canonical_asset_path},
    esm::records::record_type::vmad::parse_vmad,
};
use rusqlite::{Connection, Result, Transaction, params};
use std::{collections::HashMap, str::from_utf8};

const CELL_SIZE: f32 = 4096.0;

pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS schema_info (version INTEGER NOT NULL);
         INSERT INTO schema_info(version) SELECT 5 WHERE NOT EXISTS (SELECT 1 FROM schema_info);
         CREATE TABLE IF NOT EXISTS plugins (
             id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, priority INTEGER NOT NULL, checksum BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS records (
             form_id INTEGER PRIMARY KEY, record_type TEXT NOT NULL, cell_id INTEGER,
             worldspace_id INTEGER, load_order INTEGER NOT NULL, data BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_records_type ON records(record_type);
         CREATE INDEX IF NOT EXISTS idx_records_cell_id ON records(cell_id) WHERE cell_id IS NOT NULL;
         CREATE TABLE IF NOT EXISTS worldspaces (
             id INTEGER PRIMARY KEY, editor_id TEXT NOT NULL, parent_world INTEGER, flags INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS cells (
             id INTEGER PRIMARY KEY, worldspace_id INTEGER, grid_x INTEGER, grid_y INTEGER,
             interior_name TEXT, flags INTEGER NOT NULL, data BLOB
         );
         CREATE INDEX IF NOT EXISTS idx_cells_grid ON cells(worldspace_id, grid_x, grid_y);
         CREATE TABLE IF NOT EXISTS "references" (
             id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL, worldspace_id INTEGER,
             base_form_id INTEGER NOT NULL, is_exterior INTEGER NOT NULL,
             pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
             local_x REAL, local_y REAL, rot_x REAL NOT NULL, rot_y REAL NOT NULL,
             rot_z REAL NOT NULL, scale REAL NOT NULL DEFAULT 1.0,
             radius_override REAL, data BLOB
         );
         CREATE INDEX IF NOT EXISTS idx_references_cell ON "references"(cell_id);
         CREATE VIRTUAL TABLE IF NOT EXISTS exterior_spatial USING rtree(
             id, minX, maxX, minY, maxY, minZ, maxZ, +cell_id, +worldspace_id
         );
         CREATE TABLE IF NOT EXISTS door_links (
             ref_id INTEGER PRIMARY KEY,            -- the source door REFR FormID (load-order remapped)
             destination_ref_id INTEGER NOT NULL,   -- XTEL bytes 0..4, remapped like any FormID
             pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,   -- XTEL 4..16, arrival
             rot_x REAL NOT NULL, rot_y REAL NOT NULL, rot_z REAL NOT NULL,   -- XTEL 16..28, arrival
             destination_cell_id INTEGER,           -- resolved post-pass from references.cell_id, NULL if unresolved
             destination_worldspace_id INTEGER      -- resolved post-pass from references.worldspace_id (NULL = interior)
         );
         CREATE TABLE IF NOT EXISTS land (
             cell_id INTEGER PRIMARY KEY, heightmap BLOB NOT NULL, vtex BLOB, vclr BLOB, normals BLOB
         );
         CREATE TABLE IF NOT EXISTS statics (
             id INTEGER PRIMARY KEY, editor_id TEXT, model_path TEXT, flags INTEGER NOT NULL,
             bounds_min_x REAL NOT NULL DEFAULT -64, bounds_min_y REAL NOT NULL DEFAULT -64,
             bounds_min_z REAL NOT NULL DEFAULT -64, bounds_max_x REAL NOT NULL DEFAULT 64,
             bounds_max_y REAL NOT NULL DEFAULT 64, bounds_max_z REAL NOT NULL DEFAULT 64,
             bounds_valid INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_statics_editor_id ON statics(editor_id);
         CREATE TABLE IF NOT EXISTS lights (
             id INTEGER PRIMARY KEY,        -- LIGH FormID
             editor_id TEXT,
             radius REAL NOT NULL,          -- Creation units
             color_r INTEGER NOT NULL, color_g INTEGER NOT NULL, color_b INTEGER NOT NULL,
             flags INTEGER NOT NULL,        -- DATA flags (dynamic, can carry, negative, flicker, off by default, ...)
             falloff REAL NOT NULL,
             fade REAL                      -- FNAM, if present
         );
         CREATE TABLE IF NOT EXISTS npcs (
             id INTEGER PRIMARY KEY, editor_id TEXT, full_name TEXT,
             race_id INTEGER, class_id INTEGER, flags INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_npcs_editor_id ON npcs(editor_id);
         CREATE TABLE IF NOT EXISTS lod (
             cell_id INTEGER NOT NULL, lod_level INTEGER NOT NULL, mesh_data BLOB NOT NULL,
             PRIMARY KEY (cell_id, lod_level)
         );
         CREATE TABLE IF NOT EXISTS waters (
             id INTEGER PRIMARY KEY, editor_id TEXT, opacity INTEGER, flags INTEGER NOT NULL,
             shallow_color INTEGER, deep_color INTEGER, reflection_color INTEGER,
             flow_normal_path TEXT, data BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS texture_sets (
             id INTEGER PRIMARY KEY, editor_id TEXT, diffuse_path TEXT, normal_path TEXT,
             glow_path TEXT, height_path TEXT, environment_path TEXT, mask_path TEXT,
             specular_path TEXT, detail_path TEXT
         );
         CREATE TABLE IF NOT EXISTS landscape_textures (
             id INTEGER PRIMARY KEY, editor_id TEXT, texture_set_id INTEGER,
             material_type INTEGER, friction REAL, restitution REAL
         );
         CREATE TABLE IF NOT EXISTS scripts (
             form_id INTEGER NOT NULL, script_name TEXT NOT NULL,
             vmad BLOB NOT NULL, properties_json TEXT NOT NULL,
             PRIMARY KEY (form_id, script_name)
         );
         CREATE TABLE IF NOT EXISTS formid_map (
             form_id INTEGER PRIMARY KEY, plugin_name TEXT NOT NULL, internal_id INTEGER NOT NULL, record_type TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS conversion_cache (
             plugin_path TEXT PRIMARY KEY, file_hash BLOB NOT NULL, last_converted INTEGER NOT NULL
         );"#
    )?;
    Ok(())
}

type CellMetadata = (Option<i32>, Option<i32>, Option<u32>);

pub fn export_to_db(conn: &Connection, master: &HashMap<u32, RawRecord>) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let mut cells: HashMap<u32, CellMetadata> = HashMap::new();

    for (&form_id, record) in master
        .iter()
        .filter(|(_, record)| &record.record_type == b"CELL")
    {
        let (grid_x, grid_y, interior_name) = extract_cell_info(&record.subrecords);
        let data = serialize_subrecords(&record.subrecords);
        insert_cell(
            &tx,
            CellRow {
                form_id,
                worldspace_id: record.worldspace_form_id,
                grid_x,
                grid_y,
                interior_name: interior_name.as_deref(),
                flags: record.flags,
                data: &data,
            },
        )?;
        cells.insert(form_id, (grid_x, grid_y, record.worldspace_form_id));
    }

    let mut ordered: Vec<_> = master.iter().collect();
    ordered.sort_unstable_by_key(|(form_id, _)| **form_id);
    for (&form_id, record) in ordered {
        let type_str = from_utf8(&record.record_type).unwrap_or("UNKN");
        let blob = serialize_subrecords(&record.subrecords);
        tx.execute(
            "INSERT OR REPLACE INTO records(form_id, record_type, cell_id, worldspace_id, load_order, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![form_id, type_str, record.cell_form_id, record.worldspace_form_id, record.load_order, blob],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO formid_map(form_id, plugin_name, internal_id, record_type) VALUES (?1, 'merged', ?1, ?2)",
            params![form_id, type_str],
        )?;

        let view = SubrecordView::new(&record.subrecords);
        if let Some(vmad_bytes) = view.find(b"VMAD")
            && let Ok((_, vmad)) = parse_vmad(vmad_bytes, &record.record_type)
        {
            for script in vmad.scripts {
                let properties = serde_json::to_string(&script.properties)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                tx.execute(
                    "INSERT OR REPLACE INTO scripts(form_id, script_name, vmad, properties_json) VALUES (?1, ?2, ?3, ?4)",
                    params![form_id, script.name, vmad_bytes, properties],
                )?;
            }
        }

        match type_str {
            "WRLD" => {
                let view = SubrecordView::new(&record.subrecords);
                let editor_id = view
                    .get_string(b"EDID")
                    .unwrap_or_else(|| format!("WRLD_{form_id:08X}"));
                tx.execute(
                    "INSERT OR REPLACE INTO worldspaces(id, editor_id, parent_world, flags) VALUES (?1, ?2, ?3, ?4)",
                    params![form_id, editor_id, view.get_form_id(b"WNAM"), record.flags],
                )?;
            }
            "REFR" | "ACHR" | "ACRE" | "PGRE" | "PMIS" => {
                let cell_id = record.cell_form_id.unwrap_or(0);
                insert_reference(
                    &tx,
                    form_id,
                    cell_id,
                    cells.get(&cell_id).copied(),
                    &record.subrecords,
                )?;
            }
            "LAND" => {
                let (heightmap, vtex, vclr, normals) = extract_land_data(&record.subrecords);
                let cell_id = record.cell_form_id.unwrap_or(form_id);
                tx.execute("INSERT OR REPLACE INTO land(cell_id, heightmap, vtex, vclr, normals) VALUES (?1, ?2, ?3, ?4, ?5)", params![cell_id, heightmap, vtex, vclr, normals])?;
            }
            "STAT" | "MSTT" | "FURN" | "DOOR" | "ACTI" | "FLOR" | "CONT" | "TREE" | "LIGH" => {
                let view = SubrecordView::new(&record.subrecords);
                // Every `LIGH` lights the space whether or not it has
                // geometry, so it gets a `lights` row regardless.
                if record.record_type == *b"LIGH" {
                    insert_light(&tx, form_id, &view)?;
                }
                let model_path = view.get_string(b"MODL").filter(|path| !path.is_empty());
                // Most `LIGH` records carry no model: they light the space with
                // nothing to draw. Storing one row per invisible light would put
                // a meshless entry in `statics` for every candle in the game, so
                // those are skipped; a light with geometry is stored like any
                // other base object.
                if record.record_type == *b"LIGH" && model_path.is_none() {
                    continue;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO statics(id, editor_id, model_path, flags) VALUES (?1, ?2, ?3, ?4)",
                    params![form_id, view.get_string(b"EDID"), model_path, record.flags],
                )?;
            }
            "NPC_" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO npcs(id, editor_id, full_name, race_id, class_id, flags) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![form_id, view.get_string(b"EDID"), view.get_string(b"FULL"), view.get_form_id(b"RNAM"), view.get_form_id(b"CNAM"), record.flags],
                )?;
            }
            "WATR" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO waters(id,editor_id,opacity,flags,shallow_color,deep_color,reflection_color,flow_normal_path,data) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        form_id,
                        view.get_string(b"EDID"),
                        view.find(b"ANAM").and_then(|bytes| bytes.first()).copied(),
                        record.flags,
                        packed_color(view.find(b"NAM0")),
                        packed_color(view.find(b"NAM1")),
                        packed_color(view.find(b"NAM2")),
                        water_flow_normal_path(&view),
                        blob,
                    ],
                )?;
            }
            "TXST" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO texture_sets(id,editor_id,diffuse_path,normal_path,glow_path,height_path,environment_path,mask_path,specular_path,detail_path) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        form_id,
                        view.get_string(b"EDID"),
                        view.get_string(b"TX00"),
                        view.get_string(b"TX01"),
                        view.get_string(b"TX02"),
                        view.get_string(b"TX03"),
                        view.get_string(b"TX04"),
                        view.get_string(b"TX05"),
                        view.get_string(b"TX06"),
                        view.get_string(b"TX07"),
                    ],
                )?;
            }
            "LTEX" => {
                let view = SubrecordView::new(&record.subrecords);
                let material = view
                    .find(b"HNAM")
                    .filter(|bytes| bytes.len() >= 2)
                    .map(|bytes| {
                        u16::from_le_bytes(bytes[..2].try_into().expect("two-byte material type"))
                    });
                tx.execute(
                    "INSERT OR REPLACE INTO landscape_textures(id,editor_id,texture_set_id,material_type,friction,restitution) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![form_id, view.get_string(b"EDID"), view.get_form_id(b"TNAM"), material, Option::<f32>::None, Option::<f32>::None],
                )?;
            }
            _ => {}
        }
    }
    // Resolve every door's destination from the reference row it points at. A
    // destination that is not a reference among the exported plugins (a
    // master's record when that master was not converted, an uninstalled mod)
    // stays NULL, and the engine reads that as "this door leads nowhere it
    // knows". The destination's own position is deliberately not used: the
    // arrival point of a load door is its own, not the destination ref's.
    tx.execute(
        "UPDATE door_links SET
             destination_cell_id = (SELECT r.cell_id FROM \"references\" r WHERE r.id = door_links.destination_ref_id),
             destination_worldspace_id = (SELECT r.worldspace_id FROM \"references\" r WHERE r.id = door_links.destination_ref_id)",
        [],
    )?;
    tx.commit()
}

/// A load door's `XTEL`: which reference the door leads to, and where in it the
/// player arrives. The arrival position and rotation are the plugin's own
/// values, not the destination reference's transform - for the Alftand route
/// they differ by tens to hundreds of units.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DoorLink {
    destination_ref_id: u32,
    position: [f32; 3],
    rotation: [f32; 3],
}

/// Reads an `XTEL` subrecord: a 4-byte destination FormID followed by the
/// arrival position and rotation as six little-endian `f32`s. Anything shorter
/// cannot hold the arrival point and yields `None`.
fn door_link(bytes: &[u8]) -> Option<DoorLink> {
    if bytes.len() < 28 {
        return None;
    }
    let mut values = [0.0f32; 6];
    for (index, value) in values.iter_mut().enumerate() {
        let start = 4 + index * 4;
        *value = f32::from_le_bytes(bytes[start..start + 4].try_into().ok()?);
    }
    Some(DoorLink {
        destination_ref_id: u32::from_le_bytes(bytes[..4].try_into().ok()?),
        position: [values[0], values[1], values[2]],
        rotation: [values[3], values[4], values[5]],
    })
}

fn water_flow_normal_path(view: &SubrecordView<'_>) -> Option<String> {
    view.get_string(b"NAM5").and_then(|path| {
        canonical_asset_path(&path, AssetKind::Texture, "dds")
            .ok()
            .map(|canonical| canonical.trim_start_matches("textures/").to_owned())
    })
}

fn packed_color(bytes: Option<&[u8]>) -> Option<u32> {
    bytes
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| u32::from_le_bytes(bytes[..4].try_into().expect("four-byte color")))
}

/// `LIGH` `DATA`, per UESP "Skyrim Mod:Mod File Format/LIGH": time (i32),
/// radius (u32, Creation units), colour (RGB + one unused byte), flags (u32),
/// falloff exponent (f32), then FOV, near clip, flicker period, flicker
/// intensity amplitude, flicker movement amplitude, value (u32) and weight
/// (f32). Every one of the 435 `LIGH` records in Skyrim.esm carries exactly
/// 48 bytes, which is the sum of those fields.
///
/// The engine reads only the first four - radius, colour, flags, falloff - so
/// a `DATA` holding at least those 20 bytes is accepted and its tail ignored;
/// one shorter than that is a record this code cannot read and is dropped with
/// a warning rather than guessed at. A shorter `DATA` is accepted because the
/// leading fields are the same ones Oblivion-era light data has.
const LIGHT_DATA_MIN: usize = 20;

fn insert_light(tx: &Transaction<'_>, form_id: u32, view: &SubrecordView<'_>) -> Result<()> {
    let Some(data) = view.find(b"DATA") else {
        eprintln!("warning: LIGH {form_id:08X} has no DATA; no lights row");
        return Ok(());
    };
    if data.len() < LIGHT_DATA_MIN {
        eprintln!(
            "warning: LIGH {form_id:08X} DATA is {} bytes, expected at least {LIGHT_DATA_MIN}; no lights row",
            data.len()
        );
        return Ok(());
    }
    // The radius is a u32 even though the column is REAL: a radius of 256
    // reads as 256.0, where the same four bytes read as f32 are a denormal.
    let radius = u32::from_le_bytes(data[4..8].try_into().expect("four-byte light radius")) as f32;
    let flags = u32::from_le_bytes(data[12..16].try_into().expect("four-byte light flags"));
    let falloff = f32::from_le_bytes(data[16..20].try_into().expect("four-byte light falloff"));
    let fade = view
        .find(b"FNAM")
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| f32::from_le_bytes(bytes[..4].try_into().expect("four-byte light fade")));
    tx.execute(
        "INSERT OR REPLACE INTO lights(id, editor_id, radius, color_r, color_g, color_b, flags, falloff, fade)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            form_id,
            view.get_string(b"EDID"),
            radius,
            i64::from(data[8]),
            i64::from(data[9]),
            i64::from(data[10]),
            flags,
            falloff,
            fade,
        ],
    )?;
    Ok(())
}

pub fn insert_reference(
    tx: &Transaction<'_>,
    form_id: u32,
    cell_id: u32,
    cell: Option<CellMetadata>,
    subs: &[(Vec<u8>, Vec<u8>)],
) -> Result<()> {
    let view = SubrecordView::new(subs);
    let transform = view.get_f32_slice(b"DATA").unwrap_or_default();
    let pos = [
        *transform.first().unwrap_or(&0.0),
        *transform.get(1).unwrap_or(&0.0),
        *transform.get(2).unwrap_or(&0.0),
    ];
    let rot = [
        *transform.get(3).unwrap_or(&0.0),
        *transform.get(4).unwrap_or(&0.0),
        *transform.get(5).unwrap_or(&0.0),
    ];
    let scale = view
        .find(b"XSCL")
        .filter(|data| data.len() >= 4)
        .map(|data| f32::from_le_bytes(data[..4].try_into().unwrap()))
        .unwrap_or(1.0);
    let base_form_id = view.get_form_id(b"NAME").unwrap_or(0);
    let (grid_x, grid_y, worldspace_id) = cell.unwrap_or((None, None, None));
    let is_exterior = worldspace_id.is_some() || (grid_x.is_some() && grid_y.is_some());
    // Exterior persistent references are owned by the worldspace's persistent
    // cell even when their position lies many cells away. Derive local
    // coordinates from the actual position and keep the R-Tree global so
    // streaming is spatial rather than tied to the owning CELL record.
    let local_x = is_exterior.then(|| pos[0] - (pos[0] / CELL_SIZE).floor() * CELL_SIZE);
    let local_y = is_exterior.then(|| pos[1] - (pos[1] / CELL_SIZE).floor() * CELL_SIZE);
    let blob = serialize_subrecords(subs);
    // `XRDS` is the reference's own radius, in the same Creation units as a
    // light's `DATA` radius and usually different from it: 10,810 of the
    // 12,148 `LIGH` references in Skyrim.esm carry one, including 228 of the
    // 231 on the Alftand -> Blackreach demo route. It rides on glow and beam
    // references as well, so any reference that has it keeps it; the engine
    // reads it for light references only, where it overrides the base
    // light's radius. It is a single little-endian `f32` and may be negative.
    let radius_override = view
        .find(b"XRDS")
        .filter(|bytes| bytes.len() >= 4)
        .map(|bytes| f32::from_le_bytes(bytes[..4].try_into().expect("four-byte XRDS radius")));

    tx.execute(
        "INSERT OR REPLACE INTO \"references\"(id, cell_id, worldspace_id, base_form_id, is_exterior, pos_x, pos_y, pos_z, local_x, local_y, rot_x, rot_y, rot_z, scale, radius_override, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![form_id, cell_id, worldspace_id, base_form_id, is_exterior, pos[0], pos[1], pos[2], local_x, local_y, rot[0], rot[1], rot[2], scale, radius_override, blob],
    )?;
    if is_exterior {
        tx.execute("INSERT OR REPLACE INTO exterior_spatial(id, minX, maxX, minY, maxY, minZ, maxZ, cell_id, worldspace_id) VALUES (?1, ?2, ?2, ?3, ?3, ?4, ?4, ?5, ?6)", params![form_id, pos[0], pos[1], pos[2], cell_id, worldspace_id])?;
    }
    if let Some(bytes) = view.find(b"XTEL") {
        // The destination's cell and worldspace are resolved once every
        // reference is in, at the end of the export.
        match door_link(bytes) {
            Some(link) => {
                tx.execute(
                    "INSERT OR REPLACE INTO door_links(ref_id, destination_ref_id, pos_x, pos_y, pos_z, rot_x, rot_y, rot_z, destination_cell_id, destination_worldspace_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
                    params![form_id, link.destination_ref_id, link.position[0], link.position[1], link.position[2], link.rotation[0], link.rotation[1], link.rotation[2]],
                )?;
            }
            None => eprintln!(
                "warning: XTEL of reference {form_id:08X} is {} bytes, expected at least 28; door link dropped",
                bytes.len()
            ),
        }
    }
    Ok(())
}

struct CellRow<'a> {
    form_id: u32,
    worldspace_id: Option<u32>,
    grid_x: Option<i32>,
    grid_y: Option<i32>,
    interior_name: Option<&'a str>,
    flags: u32,
    data: &'a [u8],
}

fn insert_cell(tx: &Transaction<'_>, row: CellRow<'_>) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO cells(id, worldspace_id, grid_x, grid_y, interior_name, flags, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![row.form_id, row.worldspace_id, row.grid_x, row.grid_y, row.interior_name, row.flags, row.data],
    )?;
    Ok(())
}

pub fn validate_database(conn: &Connection) -> Result<()> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let version: u32 = conn.query_row("SELECT version FROM schema_info LIMIT 1", [], |row| {
        row.get(0)
    })?;
    if version != shared::WORLD_DATABASE_SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_hybrid_spatial_schema() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let interior_index: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_references_cell'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let exterior_table: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'exterior_spatial'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((interior_index, exterior_table), (1, 1));
        for table in [
            "worldspaces",
            "cells",
            "references",
            "door_links",
            "land",
            "statics",
            "lights",
            "npcs",
            "scripts",
            "waters",
            "texture_sets",
            "landscape_textures",
        ] {
            let present: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "missing semantic table {table}");
        }
        validate_database(&conn).unwrap();
    }

    #[test]
    fn reads_water_flow_normals_from_nam5_not_binary_dnam() {
        let subrecords = vec![
            (b"DNAM".to_vec(), vec![0, 1, 2, 3, 4, 5]),
            (
                b"NAM5".to_vec(),
                b"Data\\Textures\\Water\\RiverFlow.dds\0".to_vec(),
            ),
        ];
        let view = SubrecordView::new(&subrecords);

        assert_eq!(
            water_flow_normal_path(&view).as_deref(),
            Some("water/riverflow.dds")
        );
    }

    #[test]
    fn indexes_persistent_exterior_references_by_global_position() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let tx = conn.transaction().unwrap();
        let position = [147_182.05f32, 34_033.137f32, 80.0f32];
        let mut data = Vec::new();
        for value in position.into_iter().chain([0.0, 0.0, 0.0]) {
            data.extend_from_slice(&value.to_le_bytes());
        }
        insert_reference(
            &tx,
            0xE7F,
            1,
            Some((None, None, Some(0x3C))),
            &[(b"DATA".to_vec(), data)],
        )
        .unwrap();
        let (x, y, local_x, local_y): (f32, f32, f32, f32) = tx
            .query_row(
                "SELECT x.minX,x.minY,r.local_x,r.local_y FROM exterior_spatial x JOIN \"references\" r ON r.id=x.id WHERE x.id=?1",
                [0xE7Fu32],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert!((x - position[0]).abs() < 0.1);
        assert!((y - position[1]).abs() < 0.1);
        assert!((0.0..CELL_SIZE).contains(&local_x));
        assert!((0.0..CELL_SIZE).contains(&local_y));
    }

    type DoorLinkRow = (u32, f64, f64, f64, f64, f64, f64, Option<i64>, Option<i64>);

    /// A crossing of the real route: source door, its `XTEL` destination, the
    /// destination's cell and worldspace (None = interior), and the arrival
    /// position and rotation `XTEL` carries.
    type RouteDoor = (u32, u32, u32, Option<u32>, [f64; 3], [f64; 3]);

    fn cstr(value: &str) -> Vec<u8> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    fn floats(values: [f32; 3]) -> Vec<u8> {
        values.into_iter().flat_map(f32::to_le_bytes).collect()
    }

    /// `XCLC`: the cell's grid square.
    fn grid(x: i32, y: i32) -> Vec<u8> {
        [x, y].into_iter().flat_map(i32::to_le_bytes).collect()
    }

    /// `XTEL`: the destination reference's FormID, then the arrival position and
    /// rotation as six little-endian floats.
    fn xtel(destination_ref_id: u32, position: [f32; 3], rotation: [f32; 3]) -> Vec<u8> {
        let mut bytes = destination_ref_id.to_le_bytes().to_vec();
        bytes.extend_from_slice(&floats(position));
        bytes.extend_from_slice(&floats(rotation));
        bytes
    }

    fn record(
        form_id: u32,
        record_type: &[u8; 4],
        cell: Option<u32>,
        worldspace: Option<u32>,
        subrecords: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> RawRecord {
        RawRecord {
            form_id,
            record_type: *record_type,
            flags: 0,
            subrecords,
            cell_form_id: cell,
            worldspace_form_id: worldspace,
            load_order: 0,
        }
    }

    /// A `REFR` inside `cell`, with a `DATA` transform and an optional `XTEL`.
    /// The owning cell, not the record, supplies the worldspace the exporter
    /// stores, so it is not a parameter here.
    fn reference(
        form_id: u32,
        cell: u32,
        base_form_id: u32,
        position: [f32; 3],
        rotation: [f32; 3],
        xtel_bytes: Option<Vec<u8>>,
    ) -> RawRecord {
        let mut subrecords = vec![
            (b"NAME".to_vec(), base_form_id.to_le_bytes().to_vec()),
            (
                b"DATA".to_vec(),
                [floats(position), floats(rotation)].concat(),
            ),
        ];
        if let Some(bytes) = xtel_bytes {
            subrecords.push((b"XTEL".to_vec(), bytes));
        }
        record(form_id, b"REFR", Some(cell), None, subrecords)
    }

    fn door_link(conn: &Connection, ref_id: u32) -> DoorLinkRow {
        conn.query_row(
            "SELECT destination_ref_id,pos_x,pos_y,pos_z,rot_x,rot_y,rot_z,destination_cell_id,destination_worldspace_id
             FROM door_links WHERE ref_id=?1",
            [ref_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .unwrap()
    }

    /// Tamriel's cell at grid (19,18) in front of Alftand, joined to the
    /// interior Alftand01 by a reciprocal pair of load doors. Each door's
    /// arrival point is deliberately not the other door's position: `XTEL`
    /// carries its own transform, and storing the destination ref's would send
    /// the player to the wrong side of the door.
    fn reciprocal_door_plugin() -> HashMap<u32, RawRecord> {
        let mut master = HashMap::new();
        master.insert(
            0x3C,
            record(
                0x3C,
                b"WRLD",
                None,
                None,
                vec![(b"EDID".to_vec(), cstr("Tamriel"))],
            ),
        );
        master.insert(
            0x8F82,
            record(
                0x8F82,
                b"CELL",
                None,
                Some(0x3C),
                vec![(b"XCLC".to_vec(), grid(19, 18))],
            ),
        );
        master.insert(
            0x152C3,
            record(
                0x152C3,
                b"CELL",
                None,
                None,
                vec![(b"EDID".to_vec(), cstr("Alftand01"))],
            ),
        );
        master.insert(
            0x1AD00,
            record(
                0x1AD00,
                b"DOOR",
                None,
                None,
                vec![(
                    b"MODL".to_vec(),
                    cstr("Architecture\\Doors\\AutoLoadDoor01.nif"),
                )],
            ),
        );
        master.insert(
            0x15D48,
            reference(
                0x15D48,
                0x8F82,
                0x1AD00,
                [78049.18, 76985.0, -5859.11],
                [0.0, 0.0, 0.0],
                Some(xtel(
                    0x152CF,
                    [-947.038, 3958.835, 591.917],
                    [0.0, 0.0, 2.96989],
                )),
            ),
        );
        master.insert(
            0x152CF,
            reference(
                0x152CF,
                0x152C3,
                0x1AD00,
                [4057.0, 8792.0, -516.0],
                [0.0, 0.0, 0.0],
                Some(xtel(
                    0x15D48,
                    [77583.23, 77411.89, -5817.21],
                    [0.0, 0.0, 1.5],
                )),
            ),
        );
        master
    }

    #[test]
    fn writes_door_links_and_resolves_their_destinations() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();

        export_to_db(&conn, &reciprocal_door_plugin()).unwrap();

        // Exterior -> interior: the arrival floats are `XTEL`'s own, cell and
        // (NULL) worldspace come from the reference the door points at.
        assert_eq!(
            door_link(&conn, 0x15D48),
            (
                0x152CF,
                -947.038f32 as f64,
                3958.835f32 as f64,
                591.917f32 as f64,
                0.0,
                0.0,
                2.96989f32 as f64,
                Some(0x152C3i64),
                None
            )
        );
        // Interior -> exterior: the destination worldspace is Tamriel's.
        assert_eq!(
            door_link(&conn, 0x152CF),
            (
                0x15D48,
                77583.23f32 as f64,
                77411.89f32 as f64,
                -5817.21f32 as f64,
                0.0,
                0.0,
                1.5,
                Some(0x8F82i64),
                Some(0x3Ci64)
            )
        );
    }

    #[test]
    fn ignores_xtel_shorter_than_28_bytes_without_panicking() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        for (form_id, length) in [(0x1000u32, 27usize), (0x2000, 12), (0x4000, 4)] {
            let mut bytes = xtel(0xDEAD_BEEF, [1.0, 2.0, 3.0], [0.0, 0.0, 0.0]);
            bytes.truncate(length);
            master.insert(
                form_id,
                reference(form_id, 0x100, 0x900, [0.0; 3], [0.0; 3], Some(bytes)),
            );
        }
        master.insert(
            0x3000,
            reference(
                0x3000,
                0x100,
                0x900,
                [0.0; 3],
                [0.0; 3],
                Some(xtel(0x2000, [4.0, 5.0, 6.0], [0.0, 0.0, 0.0])),
            ),
        );

        export_to_db(&conn, &master).unwrap();

        let links: i64 = conn
            .query_row("SELECT count(*) FROM door_links", [], |row| row.get(0))
            .unwrap();
        assert_eq!(links, 1, "exactly the 28-byte XTEL carries a link");
        assert_eq!(door_link(&conn, 0x3000).0, 0x2000);
    }

    #[test]
    fn leaves_unresolved_destinations_null() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        // `XTEL` to a reference that is not in the exported plugins: a master's
        // record when only one plugin was converted, or an uninstalled mod.
        master.insert(
            0x1000,
            reference(
                0x1000,
                0x100,
                0x900,
                [0.0; 3],
                [0.0; 3],
                Some(xtel(0xDEAD_BEEF, [1.0, 2.0, 3.0], [0.0, 0.0, 0.25])),
            ),
        );

        export_to_db(&conn, &master).unwrap();

        assert_eq!(
            door_link(&conn, 0x1000),
            (0xDEAD_BEEF, 1.0, 2.0, 3.0, 0.0, 0.0, 0.25, None, None)
        );
    }

    #[test]
    fn stores_models_for_the_renderable_base_types() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let cases: [(u32, &[u8; 4], &str); 7] = [
            (0x100, b"DOOR", "Architecture\\Doors\\AutoLoadDoor01.nif"),
            (0x200, b"ACTI", "Clutter\\Lever.nif"),
            (0x300, b"FLOR", "Plants\\FloraBluebell.nif"),
            (0x400, b"CONT", "Clutter\\Chest.nif"),
            (0x500, b"TREE", "Trees\\PineTree.nif"),
            (0x600, b"LIGH", "Clutter\\InvisibleLightMarker.nif"),
            (0x700, b"STAT", "Architecture\\Wall.nif"),
        ];
        let mut master = HashMap::new();
        for (form_id, record_type, model) in cases {
            master.insert(
                form_id,
                record(
                    form_id,
                    record_type,
                    None,
                    None,
                    vec![(b"MODL".to_vec(), cstr(model))],
                ),
            );
        }
        // Most lights carry no model at all: they light the space without
        // anything to draw, and a `statics` row for each would be a meshless
        // reference in every candle-lit cell.
        master.insert(
            0x800,
            record(
                0x800,
                b"LIGH",
                None,
                None,
                vec![(b"EDID".to_vec(), cstr("FXLightInvisible"))],
            ),
        );

        export_to_db(&conn, &master).unwrap();

        for (form_id, record_type, model) in cases {
            let stored: Option<String> = conn
                .query_row(
                    "SELECT model_path FROM statics WHERE id=?1",
                    [form_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                stored.as_deref(),
                Some(model),
                "{} {form_id:08X}",
                from_utf8(record_type).unwrap()
            );
        }
        let model_less: i64 = conn
            .query_row("SELECT count(*) FROM statics WHERE id=0x800", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(model_less, 0, "a LIGH without MODL has no statics row");
    }

    /// A `LIGH` `DATA` built the way the game writes it: time, radius, colour
    /// (RGB + one unused byte), flags and falloff first, then the FOV, near
    /// clip, flicker period, two flicker amplitudes, value and weight the
    /// engine does not read - 48 bytes in all.
    fn light_data_bytes(radius: u32, color: [u8; 3], flags: u32, falloff: f32) -> Vec<u8> {
        let mut bytes = (-1i32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&radius.to_le_bytes());
        bytes.extend_from_slice(&[color[0], color[1], color[2], 0]);
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&falloff.to_le_bytes());
        for value in [90.0f32, 6.585, 0.333, 0.5, 16.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
        assert_eq!(bytes.len(), 48);
        bytes
    }

    /// A `LIGH` record: editor id, `DATA`, and `FNAM` when the light has one.
    fn light(
        form_id: u32,
        editor_id: &str,
        data: Vec<u8>,
        fade: Option<f32>,
        model: Option<&str>,
    ) -> RawRecord {
        let mut subrecords = vec![
            (b"EDID".to_vec(), cstr(editor_id)),
            (b"DATA".to_vec(), data),
        ];
        if let Some(fade) = fade {
            subrecords.push((b"FNAM".to_vec(), fade.to_le_bytes().to_vec()));
        }
        if let Some(model) = model {
            subrecords.push((b"MODL".to_vec(), cstr(model)));
        }
        record(form_id, b"LIGH", None, None, subrecords)
    }

    #[test]
    fn writes_a_lights_row_from_the_light_data() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        master.insert(
            0xCB3B0,
            light(
                0xCB3B0,
                "DefaultTorch01NS_FastSaturate",
                light_data_bytes(256, [0xE9, 0x9E, 0x4B], 0x2009, 1.0),
                Some(1.25),
                None,
            ),
        );

        export_to_db(&conn, &master).unwrap();

        type LightRow = (u32, String, f64, i64, i64, i64, i64, f64, f64);
        let row: LightRow = conn
            .query_row(
                "SELECT id,editor_id,radius,color_r,color_g,color_b,flags,falloff,fade FROM lights WHERE id=?1",
                [0xCB3B0u32],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                0xCB3B0,
                "DefaultTorch01NS_FastSaturate".to_owned(),
                256.0,
                233,
                158,
                75,
                0x2009,
                1.0,
                1.25,
            )
        );
    }

    #[test]
    fn gives_a_lights_row_to_a_light_with_no_model_and_none_to_a_light_without_data() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        // Invisible lights are the common case: they light the space with
        // nothing to draw, so they belong in `lights` and not in `statics`.
        master.insert(
            0x800,
            light(
                0x800,
                "FXLightInvisible",
                light_data_bytes(128, [255, 255, 255], 0x10, 1.0),
                None,
                None,
            ),
        );
        // A light that does have geometry is both.
        master.insert(
            0x600,
            light(
                0x600,
                "LightWithModel",
                light_data_bytes(512, [16, 32, 64], 0x1, 2.0),
                None,
                Some("Clutter\\InvisibleLightMarker.nif"),
            ),
        );
        // `radius`, `color_*`, `flags` and `falloff` are NOT NULL, so a record
        // whose DATA cannot supply them gets no row rather than invented ones.
        master.insert(
            0x900,
            record(
                0x900,
                b"LIGH",
                None,
                None,
                vec![(b"EDID".to_vec(), cstr("NoDataAtAll"))],
            ),
        );

        export_to_db(&conn, &master).unwrap();

        let rows: Vec<(u32, f64)> = {
            let mut statement = conn
                .prepare("SELECT id,radius FROM lights ORDER BY id")
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(rows, vec![(0x600, 512.0), (0x800, 128.0)]);
        let statics: Vec<u32> = {
            let mut statement = conn.prepare("SELECT id FROM statics ORDER BY id").unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<u32>>>()
                .unwrap()
        };
        assert_eq!(statics, vec![0x600], "only the modelled light is a static");
    }

    #[test]
    fn drops_a_truncated_light_data_without_panicking() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        let full = light_data_bytes(256, [1, 2, 3], 0x8, 1.0);
        for (form_id, length) in [(0x1000u32, 0usize), (0x2000, 4), (0x3000, 12), (0x4000, 19)] {
            let mut truncated = full.clone();
            truncated.truncate(length);
            master.insert(form_id, light(form_id, "Truncated", truncated, None, None));
        }
        // A light whose DATA is shorter than the four fields the row needs is
        // dropped; the rest of the export still runs.
        master.insert(
            0x5000,
            light(
                0x5000,
                "Complete",
                full,
                Some(1.0),
                Some("Clutter\\Candle.nif"),
            ),
        );

        export_to_db(&conn, &master).unwrap();

        let ids: Vec<u32> = {
            let mut statement = conn.prepare("SELECT id FROM lights").unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<u32>>>()
                .unwrap()
        };
        assert_eq!(ids, vec![0x5000], "only the complete DATA produced a row");
        let statics: i64 = conn
            .query_row("SELECT count(*) FROM statics WHERE id=0x5000", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(statics, 1);
    }

    #[test]
    fn stores_a_reference_radius_override_from_xrds() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        let mut overridden = reference(0x1000, 0x100, 0x900, [0.0; 3], [0.0; 3], None);
        overridden.subrecords.push((
            b"XRDS".to_vec(),
            544.3856f32.to_le_bytes().to_vec(), // the Alftand01 IceCandleLight
        ));
        master.insert(0x1000, overridden);
        master.insert(
            0x2000,
            reference(0x2000, 0x100, 0x900, [0.0; 3], [0.0; 3], None),
        );
        // `XRDS` shorter than the float it holds is not a radius.
        let mut truncated = reference(0x3000, 0x100, 0x900, [0.0; 3], [0.0; 3], None);
        truncated
            .subrecords
            .push((b"XRDS".to_vec(), vec![0xAE, 0x18]));
        master.insert(0x3000, truncated);

        export_to_db(&conn, &master).unwrap();

        let radius = |id: u32| -> Option<f64> {
            conn.query_row(
                "SELECT radius_override FROM \"references\" WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(radius(0x1000), Some(f64::from(544.3856f32)));
        assert_eq!(radius(0x2000), None, "a reference without XRDS stays NULL");
        assert_eq!(radius(0x3000), None, "a two-byte XRDS holds no radius");
    }

    /// The real `LIGH` records and their references, read through the same
    /// `export_to_db` path the converter uses. Two things were measured here
    /// before the table was written, and both are asserted so a change in the
    /// game data or in this code shows up as a failure rather than as wrong
    /// light in the engine:
    ///
    /// - `DATA` is 48 bytes in every `LIGH` record of Skyrim.esm, which is the
    ///   size of the layout UESP documents.
    /// - `XRDS`, the reference's own radius, rides on almost every light
    ///   reference (10,810 of 12,148 in Skyrim.esm, 228 of the 231 inside
    ///   Alftand01 / Alftand02 / AlftandWorld / Blackreach), which is why the
    ///   `references` table has a `radius_override` column at all.
    ///
    /// The expected torch values are the bytes `DefaultTorch01NS_FastSaturate`
    /// (000CB3B0) holds: `FF FF FF FF` time -1, `00 01 00 00` radius 256,
    /// `E9 9E 4B 00` colour (233, 158, 75), `09 20 00 00` flags 0x2009,
    /// `00 00 80 3F` falloff 1.0, and `FNAM` 1.0.
    ///
    /// Reads the game install, so it is opt-in:
    /// `cargo test -p converter --lib -- --ignored`.
    #[test]
    #[ignore = "reads Skyrim.esm from the Skyrim SE install"]
    fn lights_of_the_real_plugin_decode_through_export() {
        use std::collections::HashSet;
        let data_dir = std::env::var("SKYRIM_DATA_DIR").unwrap_or_else(|_| {
            "<Skyrim SE install>/Data".to_owned()
        });
        let plugin = std::path::Path::new(&data_dir).join("Skyrim.esm");
        if !plugin.is_file() {
            eprintln!("skipping: no Skyrim.esm at {}", plugin.display());
            return;
        }
        let records = crate::esm::binary::parse_plugin_file(&plugin).unwrap();

        let mut light_count = 0usize;
        for record in records.iter().filter(|r| r.record_type == *b"LIGH") {
            light_count += 1;
            let data_length = record
                .subrecords
                .iter()
                .find(|(tag, _)| tag.as_slice() == b"DATA")
                .map(|(_, data)| data.len());
            assert_eq!(
                data_length,
                Some(48),
                "LIGH {:08X} DATA is {data_length:?} bytes, expected 48",
                record.form_id
            );
        }
        eprintln!("LIGH records with a 48-byte DATA: {light_count}");
        assert!(light_count > 400, "only {light_count} LIGH records found");

        // Alftand01 cell, Alftand02 cell, AlftandWorld worldspace, Blackreach.
        const ROUTE: [u32; 4] = [0x152C3, 0x56C1B, 0x69857, 0x1EE62];
        let light_ids: HashSet<u32> = records
            .iter()
            .filter(|record| record.record_type == *b"LIGH")
            .map(|record| record.form_id)
            .collect();
        let (mut all_refs, mut all_overrides) = (0usize, 0usize);
        let (mut route_refs, mut route_overrides) = (0usize, 0usize);
        for record in records.iter().filter(|r| r.record_type == *b"REFR") {
            let base = record
                .subrecords
                .iter()
                .find(|(tag, _)| tag.as_slice() == b"NAME")
                .filter(|(_, data)| data.len() >= 4)
                .map(|(_, data)| u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
            if !base.is_some_and(|base| light_ids.contains(&base)) {
                continue;
            }
            let has_override = record
                .subrecords
                .iter()
                .any(|(tag, data)| tag.as_slice() == b"XRDS" && data.len() >= 4);
            all_refs += 1;
            all_overrides += usize::from(has_override);
            let on_route = record
                .cell_form_id
                .is_some_and(|cell| ROUTE.contains(&cell))
                || record
                    .worldspace_form_id
                    .is_some_and(|world| ROUTE.contains(&world));
            if on_route {
                route_refs += 1;
                route_overrides += usize::from(has_override);
            }
        }
        eprintln!(
            "LIGH references with an XRDS radius override: {all_overrides}/{all_refs} of the \
             plugin, {route_overrides}/{route_refs} on the Alftand -> Blackreach route"
        );
        assert!(
            all_overrides * 10 > all_refs * 8,
            "an override is common in the plugin itself"
        );
        assert!(
            route_overrides * 20 > route_refs * 19,
            "an override is nearly universal on the demo route"
        );

        const TORCH: u32 = 0xCB3B0;
        const ROUTE_REF: u32 = 0x56D56;
        const ROUTE_LIGHT: u32 = 0x194F4;
        let wanted: HashSet<u32> = [TORCH, ROUTE_REF, ROUTE_LIGHT, 0x152C3]
            .into_iter()
            .collect();
        let master: HashMap<u32, RawRecord> = records
            .into_iter()
            .filter(|record| wanted.contains(&record.form_id))
            .map(|record| (record.form_id, record))
            .collect();
        for form_id in &wanted {
            assert!(
                master.contains_key(form_id),
                "{form_id:08X} is not in Skyrim.esm"
            );
        }

        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_to_db(&conn, &master).unwrap();

        type TorchRow = (f64, i64, i64, i64, i64, f64, f64);
        let torch: TorchRow = conn
            .query_row(
                "SELECT radius,color_r,color_g,color_b,flags,falloff,fade FROM lights WHERE id=?1",
                [TORCH],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            torch,
            (256.0, 233, 158, 75, 0x2009, 1.0, 1.0),
            "DefaultTorch01NS_FastSaturate"
        );
        // A torch is warm: reading the colour at the wrong offset would not be.
        assert!(
            torch.1 > torch.2 && torch.2 > torch.3,
            "torch colour is not warm: ({}, {}, {})",
            torch.1,
            torch.2,
            torch.3
        );

        // The reference's override is its own, not its base light's: 544.4
        // units against the 256 its `IceCandleLight01` base asks for.
        let override_radius: Option<f64> = conn
            .query_row(
                "SELECT radius_override FROM \"references\" WHERE id=?1",
                [ROUTE_REF],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(override_radius, Some(f64::from(544.3856f32)));
        let base_radius: f64 = conn
            .query_row(
                "SELECT radius FROM lights WHERE id=?1",
                [ROUTE_LIGHT],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(base_radius, 256.0);
    }

    /// The four doors of the Alftand -> Blackreach route in the real plugin,
    /// decoded through the same `export_to_db` path the converter uses. The
    /// expected values are the ones `tools/research/esm_route.py` printed for
    /// the t02 note (`docs/research/worldspace-transition-demo.md`, section 2.2),
    /// which was measured independently of this code. Reads the game install, so
    /// it is opt-in: `cargo test -p converter --lib -- --ignored`.
    #[test]
    #[ignore = "reads Skyrim.esm from the Skyrim SE install"]
    fn route_doors_of_the_real_plugin_decode_through_export() {
        use std::collections::HashSet;
        let data_dir = std::env::var("SKYRIM_DATA_DIR").unwrap_or_else(|_| {
            "<Skyrim SE install>/Data".to_owned()
        });
        let plugin = std::path::Path::new(&data_dir).join("Skyrim.esm");
        if !plugin.is_file() {
            eprintln!("skipping: no Skyrim.esm at {}", plugin.display());
            return;
        }
        let records = crate::esm::binary::parse_plugin_file(&plugin).unwrap();

        // source door, its XTEL destination, destination cell, destination
        // worldspace (None = interior), arrival position, arrival rotation.
        const ROUTE: [RouteDoor; 4] = [
            (
                0x15D48,
                0x152CF,
                0x152C3,
                None,
                [-947.038, 3958.835, 591.917],
                [0.0, 0.0, 2.96989],
            ),
            (
                0x92809,
                0x5704B,
                0x56C1B,
                None,
                [2879.831, 2718.830, -1828.0],
                [0.0, 0.0, 2.87979],
            ),
            (
                0x9256A,
                0x699E8,
                0x69869,
                Some(0x69857),
                [3693.815, 3074.645, 290.530],
                [0.0, 0.0, -1.83260],
            ),
            (
                0x6998D,
                0x4E504,
                0x2D4E0,
                Some(0x1EE62),
                [21088.559, 18512.045, 2434.0],
                [0.0, 0.0, -1.87080],
            ),
        ];
        let mut wanted: HashSet<u32> = ROUTE
            .iter()
            .flat_map(|(source, destination, ..)| [*source, *destination])
            .collect();
        // The cells holding those references and the worldspaces they belong to:
        // a reference takes its worldspace from its cell row.
        wanted.extend([0xD74, 0x152C3, 0x56C1B, 0x69869, 0x2D4E0]);
        wanted.extend([0x3C, 0x69857, 0x1EE62]);
        let master: HashMap<u32, RawRecord> = records
            .into_iter()
            .filter(|record| wanted.contains(&record.form_id))
            .map(|record| (record.form_id, record))
            .collect();
        for (source, ..) in ROUTE {
            assert!(
                master.contains_key(&source),
                "route door {source:08X} is not in Skyrim.esm"
            );
        }

        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        export_to_db(&conn, &master).unwrap();

        for (source, destination, cell, worldspace, arrival, rotation) in ROUTE {
            let xtel_bytes = master[&source]
                .subrecords
                .iter()
                .find(|(tag, _)| tag.as_slice() == b"XTEL")
                .map(|(_, data)| data.as_slice())
                .expect("route door has an XTEL");
            // These subrecords are 32 bytes, not the 28 UESP documents: an
            // extra four bytes follow the arrival rotation. The exporter reads
            // the first 28 and ignores the tail, which is what the decoded
            // values below check.
            assert!(
                xtel_bytes.len() >= 28,
                "route door {source:08X} XTEL is {} bytes",
                xtel_bytes.len()
            );
            eprintln!(
                "route door {source:08X} XTEL ({} bytes): {:02X?}",
                xtel_bytes.len(),
                xtel_bytes
            );
            let row = door_link(&conn, source);
            assert_eq!(row.0, destination, "route door {source:08X} destination");
            for (axis, expected) in arrival.into_iter().enumerate() {
                let decoded = [row.1, row.2, row.3][axis];
                assert!(
                    (decoded - expected).abs() < 1e-2,
                    "route door {source:08X} arrival axis {axis}: {decoded} != {expected}"
                );
            }
            // The yaw is what actually aims the player into the destination;
            // reading it at the wrong offset would still land near the door.
            for (axis, expected) in rotation.into_iter().enumerate() {
                let decoded = [row.4, row.5, row.6][axis];
                assert!(
                    (decoded - expected).abs() < 1e-3,
                    "route door {source:08X} arrival rotation axis {axis}: {decoded} != {expected}"
                );
            }
            assert_eq!(
                row.7,
                Some(i64::from(cell)),
                "route door {source:08X} destination cell"
            );
            assert_eq!(
                row.8,
                worldspace.map(i64::from),
                "route door {source:08X} destination worldspace"
            );
        }
        // The route is reciprocal: every destination door links back.
        for (source, destination, ..) in ROUTE {
            assert_eq!(
                door_link(&conn, destination).0,
                source,
                "route door {destination:08X} does not lead back"
            );
        }
    }
}
