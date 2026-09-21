use bevy::prelude::Resource;
use color_eyre::{Result, eyre::WrapErr};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

pub(crate) const EXTERIOR_CELL_ID_SQL: &str = "SELECT c.id FROM cells c
     LEFT JOIN land l ON l.cell_id=c.id
     WHERE c.worldspace_id=?1 AND c.grid_x=?2 AND c.grid_y=?3
     ORDER BY (l.cell_id IS NOT NULL) DESC, c.id DESC
     LIMIT 1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellKey {
    Exterior {
        worldspace_id: u32,
        grid_x: i32,
        grid_y: i32,
    },
    Interior(u32),
}

#[derive(Debug, Clone)]
pub struct ReferenceRow {
    pub form_id: u32,
    pub cell_id: u32,
    pub base_form_id: u32,
    pub model_path: Option<String>,
    pub position: [f32; 3],
    pub rotation: [f32; 3],
    pub scale: f32,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub bounds_valid: bool,
    /// The reference's `door_links` row, when the database has one. `None` for every ordinary
    /// reference, and for every reference in a database converted before doors were exported.
    pub door: Option<DoorLinkRow>,
}

/// A `door_links` row as the converted database stores it, plus the destination's label.
///
/// The destination is unresolved when [`Self::destination_cell_id`] is `None`: the converter
/// found the link but not the cell it points at. The engine turns a resolved row into a
/// [`DoorDestination`](crate::doors::DoorDestination); this type stays the table's shape.
#[derive(Debug, Clone, PartialEq)]
pub struct DoorLinkRow {
    /// The destination door reference (`XTEL` bytes 0..4).
    pub destination_ref_id: u32,
    /// The destination reference's cell, `None` when the converter could not resolve it.
    pub destination_cell_id: Option<u32>,
    /// The destination reference's worldspace; `None` means the destination is an interior.
    pub destination_worldspace_id: Option<u32>,
    /// Arrival position in Creation-engine units (`XTEL` bytes 4..16).
    pub arrival_position: [f32; 3],
    /// Arrival rotation in Creation-engine radians (`XTEL` bytes 16..28).
    pub arrival_rotation: [f32; 3],
    /// The destination interior's `interior_name`, else the destination worldspace's
    /// `editor_id`, else empty.
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct CellPayload {
    pub generation: u64,
    pub key: CellKey,
    pub cell_id: u32,
    pub references: Vec<ReferenceRow>,
}

#[derive(Debug)]
pub enum DatabaseRequest {
    Load {
        generation: u64,
        key: CellKey,
        queued_at: Instant,
    },
    Shutdown,
}

#[derive(Debug)]
pub struct DatabaseResponse {
    pub generation: u64,
    pub key: CellKey,
    pub result: std::result::Result<CellPayload, String>,
    pub query_micros: u64,
    pub queue_wait_micros: u64,
    pub total_request_micros: u64,
    pub row_count: usize,
}

#[derive(Resource)]
pub struct WorldDatabase {
    requests: Sender<DatabaseRequest>,
    responses: Receiver<DatabaseResponse>,
    worker: Option<thread::JoinHandle<()>>,
    worker_stopped: Arc<AtomicBool>,
}

#[derive(Resource, Default)]
pub struct AssetCatalog {
    landscape_diffuse: std::collections::HashMap<u32, String>,
    water_flow: std::collections::HashMap<u32, String>,
}

impl AssetCatalog {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut statement = connection.prepare(
            "SELECT l.id,t.diffuse_path FROM landscape_textures l JOIN texture_sets t ON t.id=l.texture_set_id WHERE t.diffuse_path IS NOT NULL",
        )?;
        let landscape_diffuse = statement
            .query_map([], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(std::result::Result::ok)
            .filter_map(|(id, path)| converted_texture_path(path).map(|path| (id, path)))
            .collect();
        drop(statement);
        let mut statement = connection.prepare(
            "SELECT id,flow_normal_path FROM waters WHERE flow_normal_path IS NOT NULL AND flow_normal_path <> ''",
        )?;
        let water_flow = statement
            .query_map([], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(std::result::Result::ok)
            .filter_map(|(id, path)| converted_texture_path(path).map(|path| (id, path)))
            .collect();
        Ok(Self {
            landscape_diffuse,
            water_flow,
        })
    }

    pub fn landscape_diffuse(&self, form_id: u32) -> Option<&str> {
        self.landscape_diffuse.get(&form_id).map(String::as_str)
    }

    pub fn water_flow(&self, form_id: u32) -> Option<&str> {
        self.water_flow.get(&form_id).map(String::as_str)
    }
}

fn converted_texture_path(path: String) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let without_prefix = normalized
        .strip_prefix("textures/")
        .or_else(|| normalized.strip_prefix("Textures/"))
        .unwrap_or(&normalized);
    if without_prefix.is_empty() {
        return None;
    }
    let mut converted = std::path::PathBuf::from("textures").join(without_prefix);
    converted.set_extension("ktx2");
    Some(converted.to_string_lossy().replace('\\', "/"))
}

impl WorldDatabase {
    pub fn open(path: &Path) -> Result<Self> {
        validate(path)?;
        let path = path.to_owned();
        let (request_tx, request_rx) = bounded(128);
        // Responses must not block shutdown if the main world stops polling.
        let (response_tx, response_rx) = unbounded();
        let worker_stopped = Arc::new(AtomicBool::new(false));
        let stopped = worker_stopped.clone();
        let worker = thread::Builder::new()
            .name("openskyrim-world-db".into())
            .spawn(move || {
                worker(path, request_rx, response_tx);
                stopped.store(true, Ordering::Release);
            })
            .wrap_err("failed to start world database worker")?;
        Ok(Self {
            requests: request_tx,
            responses: response_rx,
            worker: Some(worker),
            worker_stopped,
        })
    }

    pub fn request(&self, request: DatabaseRequest) -> Result<()> {
        self.requests
            .send(request)
            .wrap_err("world database worker stopped")
    }

    pub fn try_response(&self) -> Option<DatabaseResponse> {
        self.responses.try_recv().ok()
    }
}

impl Drop for WorldDatabase {
    fn drop(&mut self) {
        let _ = self.requests.send(DatabaseRequest::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        debug_assert!(self.worker_stopped.load(Ordering::Acquire));
    }
}

fn validate(path: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .wrap_err_with(|| format!("failed to open {}", path.display()))?;
    let version: u32 = connection
        .query_row("SELECT version FROM schema_info LIMIT 1", [], |row| {
            row.get(0)
        })
        .wrap_err("world database has no schema version")?;
    color_eyre::eyre::ensure!(
        version == shared::WORLD_DATABASE_SCHEMA_VERSION,
        "world database schema {version} is unsupported; reconvert assets for version {}",
        shared::WORLD_DATABASE_SCHEMA_VERSION
    );
    Ok(())
}

fn worker(
    path: std::path::PathBuf,
    requests: Receiver<DatabaseRequest>,
    responses: Sender<DatabaseResponse>,
) {
    let connection = match Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(_) => return,
    };
    while let Ok(request) = requests.recv() {
        let DatabaseRequest::Load {
            generation,
            key,
            queued_at,
        } = request
        else {
            break;
        };
        let queue_wait_micros = elapsed_micros(queued_at);
        let started = Instant::now();
        let result = load_cell(&connection, generation, key).map_err(|error| format!("{error:#}"));
        let query_micros = elapsed_micros(started);
        let row_count = result
            .as_ref()
            .map_or(0, |payload| payload.references.len());
        if responses
            .send(DatabaseResponse {
                generation,
                key,
                result,
                query_micros,
                queue_wait_micros,
                total_request_micros: elapsed_micros(queued_at),
                row_count,
            })
            .is_err()
        {
            break;
        }
    }
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

/// The reference columns every cell query selects, in the order [`map_reference`] reads them.
const REFERENCE_COLUMNS: &str = concat!(
    "r.id,r.cell_id,r.base_form_id,s.model_path,",
    "r.pos_x,r.pos_y,r.pos_z,r.rot_x,r.rot_y,r.rot_z,r.scale,",
    "COALESCE(s.bounds_min_x,-64),COALESCE(s.bounds_min_y,-64),COALESCE(s.bounds_min_z,-64),",
    "COALESCE(s.bounds_max_x,64),COALESCE(s.bounds_max_y,64),COALESCE(s.bounds_max_z,64),",
    "COALESCE(s.bounds_valid,0)",
);

/// The `door_links` row of the reference and the destination's label, in the order
/// [`map_reference`] reads them. The label is the destination interior's `interior_name`, else
/// the destination worldspace's `editor_id`.
const DOOR_COLUMNS: &str = concat!(
    "d.destination_ref_id,d.destination_cell_id,d.destination_worldspace_id,",
    "d.pos_x,d.pos_y,d.pos_z,d.rot_x,d.rot_y,d.rot_z,",
    "COALESCE(NULLIF(dc.interior_name,''),NULLIF(ws.editor_id,''),'')",
);

/// Stand-in for [`DOOR_COLUMNS`] in a database that predates the `door_links` table: every
/// reference reads as a non-door, with the column order unchanged.
const ABSENT_DOOR_COLUMNS: &str = "NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL";

const REFERENCE_JOIN: &str = " LEFT JOIN statics s ON s.id=r.base_form_id";

const DOOR_JOIN: &str = concat!(
    " LEFT JOIN door_links d ON d.ref_id=r.id",
    " LEFT JOIN cells dc ON dc.id=d.destination_cell_id",
    " LEFT JOIN worldspaces ws ON ws.id=d.destination_worldspace_id",
);

/// Whether the database carries the `door_links` table. A database converted before doors were
/// exported still loads; every reference then reads as a non-door.
fn has_door_links(connection: &Connection) -> Result<bool> {
    let count: i64 = connection
        .prepare_cached(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='door_links'",
        )?
        .query_row([], |row| row.get(0))?;
    Ok(count > 0)
}

fn load_cell(connection: &Connection, generation: u64, key: CellKey) -> Result<CellPayload> {
    let (columns, joins) = if has_door_links(connection)? {
        (
            format!("{REFERENCE_COLUMNS},{DOOR_COLUMNS}"),
            format!("{REFERENCE_JOIN}{DOOR_JOIN}"),
        )
    } else {
        (
            format!("{REFERENCE_COLUMNS},{ABSENT_DOOR_COLUMNS}"),
            REFERENCE_JOIN.to_owned(),
        )
    };
    let cell_id: u32 = match key {
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        } => connection.query_row(
            EXTERIOR_CELL_ID_SQL,
            params![worldspace_id, grid_x, grid_y],
            |row| row.get(0),
        )?,
        CellKey::Interior(cell_id) => cell_id,
    };
    let references = match key {
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        } => {
            let sql = format!(
                "SELECT {columns} FROM exterior_spatial x JOIN \"references\" r ON r.id=x.id{joins} \
WHERE x.worldspace_id=?1 AND x.minX>=?2 AND x.minX<?3 AND x.minY>=?4 AND x.minY<?5"
            );
            let min_x = grid_x as f32 * 4096.0;
            let min_y = grid_y as f32 * 4096.0;
            connection
                .prepare_cached(&sql)?
                .query_map(
                    params![worldspace_id, min_x, min_x + 4096.0, min_y, min_y + 4096.0],
                    map_reference,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        CellKey::Interior(_) => {
            let sql = format!("SELECT {columns} FROM \"references\" r{joins} WHERE r.cell_id=?1");
            connection
                .prepare_cached(&sql)?
                .query_map([cell_id], map_reference)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
    };
    Ok(CellPayload {
        generation,
        key,
        cell_id,
        references,
    })
}

fn map_reference(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReferenceRow> {
    let destination_ref_id: Option<u32> = row.get(18)?;
    let destination_cell_id: Option<u32> = row.get(19)?;
    let destination_worldspace_id: Option<u32> = row.get(20)?;
    let door = match destination_ref_id {
        Some(destination_ref_id) => Some(DoorLinkRow {
            destination_ref_id,
            destination_cell_id,
            destination_worldspace_id,
            arrival_position: [row.get(21)?, row.get(22)?, row.get(23)?],
            arrival_rotation: [row.get(24)?, row.get(25)?, row.get(26)?],
            label: row.get(27)?,
        }),
        None => None,
    };
    Ok(ReferenceRow {
        form_id: row.get(0)?,
        cell_id: row.get(1)?,
        base_form_id: row.get(2)?,
        model_path: row.get(3)?,
        position: [row.get(4)?, row.get(5)?, row.get(6)?],
        rotation: [row.get(7)?, row.get(8)?, row.get(9)?],
        scale: row.get(10)?,
        bounds_min: [row.get(11)?, row.get(12)?, row.get(13)?],
        bounds_max: [row.get(14)?, row.get(15)?, row.get(16)?],
        bounds_valid: row.get(17)?,
        door,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(connection: &Connection) {
        connection
            .execute_batch(&format!(
                r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
                INSERT INTO schema_info VALUES({version});
                CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER,interior_name TEXT);
                CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                CREATE TABLE statics(id INTEGER PRIMARY KEY,model_path TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER,base_form_id INTEGER,pos_x REAL,pos_y REAL,pos_z REAL,rot_x REAL,rot_y REAL,rot_z REAL,scale REAL);
                CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
                INSERT INTO cells VALUES(10,60,2,-3,NULL);
                INSERT INTO statics VALUES(20,'architecture/wall.nif',-1,-2,-3,1,2,3,1);
                INSERT INTO "references" VALUES(30,10,20,8200,-12200,50,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(30,8200,8200,-12200,-12200,50,50,10,60);
                INSERT INTO "references" VALUES(31,99,20,8250,-12150,55,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(31,8250,8250,-12150,-12150,55,55,99,60);"#,
                version = shared::WORLD_DATABASE_SCHEMA_VERSION
            ))
            .unwrap();
    }

    /// The schema-4 door tables, the interior cell a door leads into, and its `door_links` row.
    /// A database without any of this is the pre-door shape [`load_cell`] must still handle.
    fn door_fixture(connection: &Connection) {
        connection
            .execute_batch(
                r#"CREATE TABLE worldspaces(id INTEGER PRIMARY KEY,editor_id TEXT NOT NULL,parent_world INTEGER,flags INTEGER NOT NULL);
                CREATE TABLE door_links(ref_id INTEGER PRIMARY KEY,destination_ref_id INTEGER NOT NULL,
                    pos_x REAL NOT NULL,pos_y REAL NOT NULL,pos_z REAL NOT NULL,
                    rot_x REAL NOT NULL,rot_y REAL NOT NULL,rot_z REAL NOT NULL,
                    destination_cell_id INTEGER,destination_worldspace_id INTEGER);
                INSERT INTO cells VALUES(99,NULL,NULL,NULL,'Alftand01');
                INSERT INTO worldspaces VALUES(614,'Blackreach',60,0);
                INSERT INTO door_links VALUES(30,77,-947.038,3958.835,591.917,0,0,2.96989,99,NULL);"#,
            )
            .unwrap();
    }

    #[test]
    fn loads_exterior_cell_through_spatial_index() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        let payload = load_cell(
            &connection,
            9,
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 2,
                grid_y: -3,
            },
        )
        .unwrap();
        assert_eq!(payload.generation, 9);
        assert_eq!(payload.cell_id, 10);
        assert_eq!(payload.references.len(), 2);
        assert!(
            payload
                .references
                .iter()
                .any(|reference| reference.cell_id == 99)
        );
        assert_eq!(
            payload.references[0].model_path.as_deref(),
            Some("architecture/wall.nif")
        );
        assert_eq!(payload.references[0].bounds_max, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn catalog_rewrites_landscape_texture_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT); CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER); CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT); INSERT INTO texture_sets VALUES(2,'textures/land/grass.dds'); INSERT INTO landscape_textures VALUES(1,2); INSERT INTO waters VALUES(9,'textures/water/flow.dds');",
            )
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&path).unwrap();
        assert_eq!(
            catalog.landscape_diffuse(1),
            Some("textures/land/grass.ktx2")
        );
        assert_eq!(catalog.water_flow(9), Some("textures/water/flow.ktx2"));
    }

    #[test]
    fn rejects_previous_database_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("old.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_info(version INTEGER); INSERT INTO schema_info VALUES(2);",
            )
            .unwrap();
        drop(connection);
        assert!(validate(&path).is_err());
    }

    #[test]
    fn prefers_exterior_cell_with_land_over_persistent_cell_at_same_grid() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        connection
            .execute_batch("INSERT INTO cells VALUES(9,60,2,-3,NULL); INSERT INTO land VALUES(10);")
            .unwrap();

        let payload = load_cell(
            &connection,
            1,
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 2,
                grid_y: -3,
            },
        )
        .unwrap();

        assert_eq!(payload.cell_id, 10);
        assert_eq!(payload.references.len(), 2);
    }

    #[test]
    fn rejects_truncated_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("truncated.db");
        std::fs::write(&path, b"SQLite format 3\0truncated").unwrap();
        assert!(WorldDatabase::open(&path).is_err());
    }

    #[test]
    fn drop_drains_a_full_request_queue_and_joins_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        drop(connection);

        let database = WorldDatabase::open(&path).unwrap();
        let stopped = database.worker_stopped.clone();
        for generation in 0..256 {
            database
                .request(DatabaseRequest::Load {
                    generation,
                    key: CellKey::Exterior {
                        worldspace_id: 60,
                        grid_x: 2,
                        grid_y: -3,
                    },
                    queued_at: Instant::now(),
                })
                .unwrap();
        }
        drop(database);
        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn loads_interior_cell_without_spatial_lookup() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        let payload = load_cell(&connection, 4, CellKey::Interior(99)).unwrap();
        assert_eq!(payload.cell_id, 99);
        assert_eq!(payload.references.len(), 1);
        assert_eq!(payload.references[0].form_id, 31);
    }

    #[test]
    fn returns_the_door_link_of_a_reference_and_the_interior_label() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        door_fixture(&connection);

        let payload = load_cell(
            &connection,
            1,
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 2,
                grid_y: -3,
            },
        )
        .unwrap();

        let door = payload
            .references
            .iter()
            .find(|reference| reference.form_id == 30)
            .expect("reference 30 is in the cell")
            .door
            .clone()
            .expect("reference 30 has a door_links row");
        assert_eq!(door.destination_ref_id, 77);
        assert_eq!(door.destination_cell_id, Some(99));
        assert_eq!(
            door.destination_worldspace_id, None,
            "an interior destination has no worldspace"
        );
        assert_eq!(door.arrival_position, [-947.038, 3958.835, 591.917]);
        assert_eq!(door.arrival_rotation, [0.0, 0.0, 2.96989]);
        assert_eq!(door.label, "Alftand01");

        assert!(
            payload
                .references
                .iter()
                .find(|reference| reference.form_id == 31)
                .expect("reference 31 is in the cell")
                .door
                .is_none(),
            "a reference without a door_links row is not a door"
        );
    }

    #[test]
    fn labels_an_exterior_destination_with_the_worldspace_editor_id() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        door_fixture(&connection);
        connection
            .execute_batch("INSERT INTO door_links VALUES(31,78,21088.559,18512.045,2434.0,0,0,-1.8708,120,614);")
            .unwrap();

        let payload = load_cell(&connection, 1, CellKey::Interior(99)).unwrap();
        let door = payload.references[0]
            .door
            .clone()
            .expect("reference 31 has a door_links row");
        assert_eq!(door.destination_worldspace_id, Some(614));
        assert_eq!(
            door.destination_cell_id,
            Some(120),
            "the destination ref's own cell is kept"
        );
        assert_eq!(door.label, "Blackreach");
        assert_eq!(
            door.arrival_position,
            [21088.559, 18512.045, 2434.0],
            "the arrival point is the XTEL position, not the destination door's"
        );
    }

    #[test]
    fn loads_a_database_whose_references_are_all_non_doors() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);

        assert!(!has_door_links(&connection).unwrap());
        let payload = load_cell(
            &connection,
            1,
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 2,
                grid_y: -3,
            },
        )
        .unwrap();
        assert_eq!(payload.references.len(), 2);
        assert!(
            payload
                .references
                .iter()
                .all(|reference| reference.door.is_none())
        );
        assert_eq!(
            payload.references[0].model_path.as_deref(),
            Some("architecture/wall.nif"),
            "the plain query still joins the base object"
        );
    }
}
