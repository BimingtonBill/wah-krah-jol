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
         INSERT INTO schema_info(version) SELECT 3 WHERE NOT EXISTS (SELECT 1 FROM schema_info);
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
             rot_z REAL NOT NULL, scale REAL NOT NULL DEFAULT 1.0, data BLOB
         );
         CREATE INDEX IF NOT EXISTS idx_references_cell ON "references"(cell_id);
         CREATE VIRTUAL TABLE IF NOT EXISTS exterior_spatial USING rtree(
             id, minX, maxX, minY, maxY, minZ, maxZ, +cell_id, +worldspace_id
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
            "STAT" | "MSTT" | "FURN" => {
                let view = SubrecordView::new(&record.subrecords);
                tx.execute(
                    "INSERT OR REPLACE INTO statics(id, editor_id, model_path, flags) VALUES (?1, ?2, ?3, ?4)",
                    params![form_id, view.get_string(b"EDID"), view.get_string(b"MODL"), record.flags],
                )?;
            }
            // `statics` is the model catalogue the loader streams, and a door,
            // an activator, a flora, a container, a tree or a light is as
            // static as a statue: without these rows a reference to one of
            // them spawns with no model at all. Only records that name a model
            // are catalogued - a base object with no `MODL` has nothing to
            // draw, and a row for it would be a meshless entry (most `LIGH`
            // records are exactly that: they light the space with nothing to
            // render). `STAT`, `MSTT` and `FURN` keep their existing behaviour
            // of storing a row with a NULL model path.
            "DOOR" | "ACTI" | "FLOR" | "CONT" | "TREE" | "LIGH" => {
                let view = SubrecordView::new(&record.subrecords);
                let model_path = view.get_string(b"MODL").filter(|path| !path.is_empty());
                let Some(model_path) = model_path else {
                    continue;
                };
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
    tx.commit()
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

    tx.execute(
        "INSERT OR REPLACE INTO \"references\"(id, cell_id, worldspace_id, base_form_id, is_exterior, pos_x, pos_y, pos_z, local_x, local_y, rot_x, rot_y, rot_z, scale, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![form_id, cell_id, worldspace_id, base_form_id, is_exterior, pos[0], pos[1], pos[2], local_x, local_y, rot[0], rot[1], rot[2], scale, blob],
    )?;
    if is_exterior {
        tx.execute("INSERT OR REPLACE INTO exterior_spatial(id, minX, maxX, minY, maxY, minZ, maxZ, cell_id, worldspace_id) VALUES (?1, ?2, ?2, ?3, ?3, ?4, ?4, ?5, ?6)", params![form_id, pos[0], pos[1], pos[2], cell_id, worldspace_id])?;
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
            "land",
            "statics",
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

    /// A NUL-terminated string subrecord, as `MODL` and `EDID` are stored.
    fn cstring(value: &str) -> Vec<u8> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    /// A catalogue record with its subrecords and no cell or worldspace, which
    /// is how the base object types are stored in a plugin.
    fn base_record(
        form_id: u32,
        record_type: &[u8; 4],
        subrecords: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> RawRecord {
        RawRecord {
            form_id,
            record_type: *record_type,
            flags: 0,
            subrecords,
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        }
    }

    fn catalogued_model(conn: &Connection, form_id: u32, record_type: &[u8; 4]) -> Option<String> {
        conn.query_row(
            "SELECT model_path FROM statics WHERE id=?1",
            [form_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{} {form_id:08X} was not catalogued: {error}",
                from_utf8(record_type).unwrap()
            )
        })
    }

    #[test]
    fn catalogues_the_models_of_the_renderable_base_types() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let cases: [(u32, &[u8; 4], &str); 6] = [
            (0x100, b"DOOR", "Architecture\\Doors\\AutoLoadDoor01.nif"),
            (0x200, b"ACTI", "Clutter\\Lever.nif"),
            (0x300, b"FLOR", "Plants\\FloraBluebell.nif"),
            (0x400, b"CONT", "Clutter\\Chest.nif"),
            (0x500, b"TREE", "Trees\\PineTree.nif"),
            (0x600, b"LIGH", "Clutter\\Lantern.nif"),
        ];
        let mut master = HashMap::new();
        for (form_id, record_type, model) in cases {
            master.insert(
                form_id,
                base_record(
                    form_id,
                    record_type,
                    vec![
                        (b"EDID".to_vec(), cstring("BaseObject")),
                        (b"MODL".to_vec(), cstring(model)),
                    ],
                ),
            );
        }

        export_to_db(&conn, &master).unwrap();

        for (form_id, record_type, model) in cases {
            assert_eq!(
                catalogued_model(&conn, form_id, record_type).as_deref(),
                Some(model),
                "{} {form_id:08X}",
                from_utf8(record_type).unwrap()
            );
        }
    }

    #[test]
    fn does_not_catalogue_a_base_object_without_a_model() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        for (index, record_type) in [b"DOOR", b"ACTI", b"FLOR", b"CONT", b"TREE", b"LIGH"]
            .into_iter()
            .enumerate()
        {
            // The record names no model at all, or names one that is empty
            // (a lone NUL), which is what a stub `MODL` looks like.
            for (offset, subrecords) in [
                vec![(b"EDID".to_vec(), cstring("BaseObject"))],
                vec![
                    (b"EDID".to_vec(), cstring("BaseObject")),
                    (b"MODL".to_vec(), vec![0]),
                ],
            ]
            .into_iter()
            .enumerate()
            {
                let form_id = 0x100 + (index * 2 + offset) as u32;
                master.insert(form_id, base_record(form_id, record_type, subrecords));
            }
        }

        export_to_db(&conn, &master).unwrap();

        let catalogued: i64 = conn
            .query_row("SELECT count(*) FROM statics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            catalogued, 0,
            "a model-less base object has nothing to draw"
        );
    }

    #[test]
    fn leaves_the_original_three_catalogue_types_unchanged() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut master = HashMap::new();
        let mut cases = Vec::new();
        for (index, record_type) in [b"STAT", b"MSTT", b"FURN"].into_iter().enumerate() {
            let with_model = 0x100 + (index as u32) * 2;
            let model_less = with_model + 1;
            master.insert(
                with_model,
                base_record(
                    with_model,
                    record_type,
                    vec![
                        (b"EDID".to_vec(), cstring("BaseObject")),
                        (b"MODL".to_vec(), cstring("Architecture\\Wall.nif")),
                    ],
                ),
            );
            master.insert(
                model_less,
                base_record(
                    model_less,
                    record_type,
                    vec![(b"EDID".to_vec(), cstring("ModelLess"))],
                ),
            );
            cases.push((with_model, model_less, record_type));
        }

        export_to_db(&conn, &master).unwrap();

        for (with_model, model_less, record_type) in cases {
            assert_eq!(
                catalogued_model(&conn, with_model, record_type).as_deref(),
                Some("Architecture\\Wall.nif"),
                "{} {with_model:08X}",
                from_utf8(record_type).unwrap()
            );
            // A `STAT`, `MSTT` or `FURN` without a model keeps its row with a
            // NULL model path; only the six types added above skip it.
            assert_eq!(
                catalogued_model(&conn, model_less, record_type),
                None,
                "{} {model_less:08X}",
                from_utf8(record_type).unwrap()
            );
        }
    }
}
