use bevy::prelude::{IVec2, Resource};
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
}

#[derive(Debug, Clone)]
pub struct CellPayload {
    pub generation: u64,
    pub key: CellKey,
    pub cell_id: u32,
    pub references: Vec<ReferenceRow>,
}

/// The two distant-LOD block kinds `lod_block.kind` stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LodBlockKind {
    Terrain,
    Objects,
}

impl LodBlockKind {
    /// The `lod_block.kind` spelling.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Terrain => "terrain",
            Self::Objects => "objects",
        }
    }

    /// Parses a `lod_block.kind` value. Unknown kinds are skipped by the table.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "terrain" => Some(Self::Terrain),
            "objects" => Some(Self::Objects),
            _ => None,
        }
    }
}

/// Identifies one distant-LOD block of one worldspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LodBlockKey {
    pub worldspace_id: u32,
    pub kind: LodBlockKind,
    pub level: u8,
    pub block_x: i32,
    pub block_y: i32,
}

/// One converted distant-LOD block: the mesh to instantiate and the model-space
/// bounds the converter measured for it.
#[derive(Debug, Clone)]
pub struct LodBlockPayload {
    pub key: LodBlockKey,
    pub mesh_path: String,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    /// False when `lod_block` carries NULL bounds, which the schema allows for a
    /// block whose converted mesh could not be measured.
    pub bounds_valid: bool,
}

/// The `lod_block` keys of one worldspace and the LOD grid they are laid out on,
/// read once at startup.
///
/// The LOD planner needs block availability for every band on every frame and
/// must never issue SQL on the main thread, so the keys are read up front
/// through a dedicated read-only connection, the way [`AssetCatalog::open`]
/// reads texture paths. The row payloads still travel through the worker.
///
/// The grid origin is read with the keys because a block is named by its
/// south-west cell measured from that origin, not from cell 0: the planner
/// cannot turn a cell into a block without it.
#[derive(Resource, Debug, Default, Clone)]
pub struct LodBlockTable {
    keys: std::collections::HashSet<LodBlockKey>,
    origin: IVec2,
}

impl LodBlockTable {
    /// Reads every `lod_block` key of `worldspace_id` and the `lod_grid` row
    /// its blocks are laid out from, through one read-only connection.
    pub fn open(path: &Path, worldspace_id: u32) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut statement = connection
            .prepare("SELECT kind,level,block_x,block_y FROM lod_block WHERE worldspace_id=?1")?;
        let keys = statement
            .query_map([worldspace_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u8>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, i32>(3)?,
                ))
            })?
            .filter_map(std::result::Result::ok)
            .filter_map(|(kind, level, block_x, block_y)| {
                Some(LodBlockKey {
                    worldspace_id,
                    kind: LodBlockKind::parse(&kind)?,
                    level,
                    block_x,
                    block_y,
                })
            })
            .collect();
        // A worldspace the converter recorded no `lod_grid` row for — or a
        // database written before the table existed — is read as a grid laid
        // out from cell 0, which is what an unoffset worldspace has.
        let origin = connection
            .query_row(
                "SELECT origin_x,origin_y FROM lod_grid WHERE worldspace_id=?1",
                [worldspace_id],
                |row| Ok(IVec2::new(row.get::<_, i32>(0)?, row.get::<_, i32>(1)?)),
            )
            .unwrap_or(IVec2::ZERO);
        Ok(Self { keys, origin })
    }

    /// Builds a table from known keys, laid out from cell 0.
    pub fn from_keys(keys: impl IntoIterator<Item = LodBlockKey>) -> Self {
        Self::from_keys_at(keys, IVec2::ZERO)
    }

    /// Builds a table from known keys and the grid origin they are laid out
    /// from.
    pub fn from_keys_at(keys: impl IntoIterator<Item = LodBlockKey>, origin: IVec2) -> Self {
        Self {
            keys: keys.into_iter().collect(),
            origin,
        }
    }

    /// The south-west corner of the worldspace's LOD grid, in cells, from
    /// `lod_grid`; cell 0 when the worldspace has no such row.
    pub fn origin(&self) -> IVec2 {
        self.origin
    }

    /// Every key of `kind`, in no particular order.
    pub fn keys(&self, kind: LodBlockKind) -> impl Iterator<Item = LodBlockKey> + '_ {
        self.keys
            .iter()
            .copied()
            .filter(move |key| key.kind == kind)
    }

    /// Whether the table lists `key`.
    pub fn contains(&self, key: LodBlockKey) -> bool {
        self.keys.contains(&key)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[derive(Debug)]
pub enum DatabaseRequest {
    Load {
        generation: u64,
        key: CellKey,
        queued_at: Instant,
    },
    LoadLod {
        generation: u64,
        key: LodBlockKey,
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

/// One `lod_block` row read by the worker.
#[derive(Debug)]
pub struct LodResponse {
    pub generation: u64,
    pub key: LodBlockKey,
    pub result: std::result::Result<LodBlockPayload, String>,
    pub query_micros: u64,
    pub queue_wait_micros: u64,
    pub total_request_micros: u64,
}

#[derive(Resource)]
pub struct WorldDatabase {
    requests: Sender<DatabaseRequest>,
    responses: Receiver<DatabaseResponse>,
    lod_responses: Receiver<LodResponse>,
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
        // Cells and LOD blocks share the request queue but answer on separate
        // channels, so neither tier's commit budget can drain the other's work.
        let (lod_response_tx, lod_response_rx) = unbounded();
        let worker_stopped = Arc::new(AtomicBool::new(false));
        let stopped = worker_stopped.clone();
        let worker = thread::Builder::new()
            .name("openskyrim-world-db".into())
            .spawn(move || {
                worker(path, request_rx, response_tx, lod_response_tx);
                stopped.store(true, Ordering::Release);
            })
            .wrap_err("failed to start world database worker")?;
        Ok(Self {
            requests: request_tx,
            responses: response_rx,
            lod_responses: lod_response_rx,
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

    /// The next committed LOD block, if the worker has answered one.
    pub fn try_lod_response(&self) -> Option<LodResponse> {
        self.lod_responses.try_recv().ok()
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
    lod_responses: Sender<LodResponse>,
) {
    let connection = match Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(_) => return,
    };
    // The match is exhaustive on purpose: a `let ... else { break }` here would
    // stop the worker on the first request of any kind it does not handle, and
    // the main world would block on the next `request()`.
    while let Ok(request) = requests.recv() {
        let delivered = match request {
            DatabaseRequest::Load {
                generation,
                key,
                queued_at,
            } => {
                let queue_wait_micros = elapsed_micros(queued_at);
                let started = Instant::now();
                let result =
                    load_cell(&connection, generation, key).map_err(|error| format!("{error:#}"));
                let query_micros = elapsed_micros(started);
                let row_count = result
                    .as_ref()
                    .map_or(0, |payload| payload.references.len());
                responses
                    .send(DatabaseResponse {
                        generation,
                        key,
                        result,
                        query_micros,
                        queue_wait_micros,
                        total_request_micros: elapsed_micros(queued_at),
                        row_count,
                    })
                    .is_ok()
            }
            DatabaseRequest::LoadLod {
                generation,
                key,
                queued_at,
            } => {
                let queue_wait_micros = elapsed_micros(queued_at);
                let started = Instant::now();
                let result = load_lod_block(&connection, key).map_err(|error| format!("{error:#}"));
                let query_micros = elapsed_micros(started);
                lod_responses
                    .send(LodResponse {
                        generation,
                        key,
                        result,
                        query_micros,
                        queue_wait_micros,
                        total_request_micros: elapsed_micros(queued_at),
                    })
                    .is_ok()
            }
            DatabaseRequest::Shutdown => break,
        };
        if !delivered {
            // The main world dropped the receiving half: stop the thread.
            break;
        }
    }
}

/// Reads one `lod_block` row.
///
/// A missing row is an error rather than a panic: the planner only requests
/// blocks the startup table listed, so a miss means the database changed
/// underneath the engine, and the block must fail without stopping the worker.
fn load_lod_block(connection: &Connection, key: LodBlockKey) -> Result<LodBlockPayload> {
    let (mesh_path, bounds) = connection.query_row(
        "SELECT mesh_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z \
         FROM lod_block WHERE worldspace_id=?1 AND kind=?2 AND level=?3 AND block_x=?4 AND block_y=?5",
        params![
            key.worldspace_id,
            key.kind.name(),
            key.level,
            key.block_x,
            key.block_y
        ],
        |row| {
            let bounds = match (
                row.get::<_, Option<f32>>(1)?,
                row.get::<_, Option<f32>>(2)?,
                row.get::<_, Option<f32>>(3)?,
                row.get::<_, Option<f32>>(4)?,
                row.get::<_, Option<f32>>(5)?,
                row.get::<_, Option<f32>>(6)?,
            ) {
                (Some(min_x), Some(min_y), Some(min_z), Some(max_x), Some(max_y), Some(max_z)) => {
                    Some(([min_x, min_y, min_z], [max_x, max_y, max_z]))
                }
                // The converter writes all six bounds or none; a partial row is
                // treated as unbounded rather than as a hard read failure.
                _ => None,
            };
            Ok((row.get::<_, String>(0)?, bounds))
        },
    )?;
    Ok(LodBlockPayload {
        key,
        mesh_path,
        bounds_min: bounds.map_or([0.0; 3], |(min, _)| min),
        bounds_max: bounds.map_or([0.0; 3], |(_, max)| max),
        bounds_valid: bounds.is_some(),
    })
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

fn load_cell(connection: &Connection, generation: u64, key: CellKey) -> Result<CellPayload> {
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
            let sql =
        "SELECT r.id,r.cell_id,r.base_form_id,s.model_path,r.pos_x,r.pos_y,r.pos_z,r.rot_x,r.rot_y,r.rot_z,r.scale,
                COALESCE(s.bounds_min_x,-64),COALESCE(s.bounds_min_y,-64),COALESCE(s.bounds_min_z,-64),
                COALESCE(s.bounds_max_x,64),COALESCE(s.bounds_max_y,64),COALESCE(s.bounds_max_z,64),
                COALESCE(s.bounds_valid,0)
         FROM exterior_spatial x JOIN \"references\" r ON r.id=x.id
         LEFT JOIN statics s ON s.id=r.base_form_id
         WHERE x.worldspace_id=?1 AND x.minX>=?2 AND x.minX<?3 AND x.minY>=?4 AND x.minY<?5";
            let min_x = grid_x as f32 * 4096.0;
            let min_y = grid_y as f32 * 4096.0;
            connection
                .prepare_cached(sql)?
                .query_map(
                    params![worldspace_id, min_x, min_x + 4096.0, min_y, min_y + 4096.0],
                    map_reference,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        CellKey::Interior(_) => {
            let sql =
        "SELECT r.id,r.cell_id,r.base_form_id,s.model_path,r.pos_x,r.pos_y,r.pos_z,r.rot_x,r.rot_y,r.rot_z,r.scale,
                COALESCE(s.bounds_min_x,-64),COALESCE(s.bounds_min_y,-64),COALESCE(s.bounds_min_z,-64),
                COALESCE(s.bounds_max_x,64),COALESCE(s.bounds_max_y,64),COALESCE(s.bounds_max_z,64),
                COALESCE(s.bounds_valid,0)
         FROM \"references\" r LEFT JOIN statics s ON s.id=r.base_form_id WHERE r.cell_id=?1";
            connection
                .prepare_cached(sql)?
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(connection: &Connection) {
        connection
            .execute_batch(&format!(
                r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
                INSERT INTO schema_info VALUES({});
                CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER);
                CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                CREATE TABLE statics(id INTEGER PRIMARY KEY,model_path TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER,base_form_id INTEGER,pos_x REAL,pos_y REAL,pos_z REAL,rot_x REAL,rot_y REAL,rot_z REAL,scale REAL);
                CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
                CREATE TABLE lod_block(worldspace_id INTEGER NOT NULL,kind TEXT NOT NULL,level INTEGER NOT NULL,block_x INTEGER NOT NULL,block_y INTEGER NOT NULL,mesh_path TEXT NOT NULL,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,PRIMARY KEY (worldspace_id,kind,level,block_x,block_y));
                INSERT INTO cells VALUES(10,60,2,-3);
                INSERT INTO statics VALUES(20,'architecture/wall.nif',-1,-2,-3,1,2,3,1);
                INSERT INTO "references" VALUES(30,10,20,8200,-12200,50,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(30,8200,8200,-12200,-12200,50,50,10,60);
                INSERT INTO "references" VALUES(31,99,20,8250,-12150,55,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(31,8250,8250,-12150,-12150,55,55,99,60);
                INSERT INTO lod_block VALUES(60,'terrain',4,0,0,'meshes/terrain/tamriel/tamriel.4.0.0.glb',-16384,-512,-16384,0,2048,0);
                INSERT INTO lod_block VALUES(60,'terrain',8,-8,-8,'meshes/terrain/tamriel/tamriel.8.-8.-8.glb',NULL,NULL,NULL,NULL,NULL,NULL);
                INSERT INTO lod_block VALUES(60,'objects',4,0,0,'meshes/terrain/tamriel/objects/tamriel.4.0.0.glb',-16384,-512,-16384,0,2048,0);
                INSERT INTO lod_block VALUES(61,'terrain',4,0,0,'meshes/terrain/otherworld/otherworld.4.0.0.glb',0,0,0,1,1,1);"#,
                shared::WORLD_DATABASE_SCHEMA_VERSION,
            ))
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
            .execute_batch("INSERT INTO cells VALUES(9,60,2,-3); INSERT INTO land VALUES(10);")
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

    fn lod_key(level: u8, block_x: i32, block_y: i32) -> LodBlockKey {
        LodBlockKey {
            worldspace_id: 60,
            kind: LodBlockKind::Terrain,
            level,
            block_x,
            block_y,
        }
    }

    fn wait_for<T>(mut poll: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..400 {
            if let Some(value) = poll() {
                return Some(value);
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn loads_a_lod_block_row_with_its_bounds_and_mesh_path() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        let payload = load_lod_block(&connection, lod_key(4, 0, 0)).unwrap();
        assert_eq!(
            payload.mesh_path,
            "meshes/terrain/tamriel/tamriel.4.0.0.glb"
        );
        assert!(payload.bounds_valid);
        assert_eq!(payload.bounds_min, [-16384.0, -512.0, -16384.0]);
        assert_eq!(payload.bounds_max, [0.0, 2048.0, 0.0]);

        let unbounded = load_lod_block(&connection, lod_key(8, -8, -8)).unwrap();
        assert!(!unbounded.bounds_valid);
        assert_eq!(unbounded.key.level, 8);
    }

    #[test]
    fn lod_table_lists_only_the_requested_worldspace_and_kind() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        drop(connection);

        let table = LodBlockTable::open(&path, 60).unwrap();
        let mut terrain: Vec<_> = table.keys(LodBlockKind::Terrain).collect();
        terrain.sort_by_key(|key| (key.level, key.block_x, key.block_y));
        assert_eq!(terrain, vec![lod_key(4, 0, 0), lod_key(8, -8, -8)]);
        assert_eq!(table.len(), 3);
        assert!(table.contains(lod_key(4, 0, 0)));
        assert!(!table.contains(LodBlockKey {
            worldspace_id: 61,
            ..lod_key(4, 0, 0)
        }));
        assert_eq!(table.keys(LodBlockKind::Objects).count(), 1);
        // The fixture writes no `lod_grid` row, so the grid starts at cell 0.
        assert_eq!(table.origin(), IVec2::ZERO);
    }

    #[test]
    fn lod_table_reads_the_grid_origin_with_the_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        connection
            .execute_batch(
                "CREATE TABLE lod_grid(worldspace_id INTEGER PRIMARY KEY,origin_x INTEGER NOT NULL,origin_y INTEGER NOT NULL,levels TEXT NOT NULL);
                INSERT INTO lod_grid VALUES(60,-23,-9,'4,8,16,32');",
            )
            .unwrap();
        drop(connection);

        // Blackreach's origin: its blocks are named from (-23, -9).
        let table = LodBlockTable::open(&path, 60).unwrap();
        assert_eq!(table.origin(), IVec2::new(-23, -9));
        assert_eq!(table.len(), 3);
        // A worldspace the table does not cover keeps the cell-0 grid.
        let other = LodBlockTable::open(&path, 61).unwrap();
        assert_eq!(other.origin(), IVec2::ZERO);
    }

    #[test]
    fn a_lod_request_for_a_missing_block_reports_an_error_without_stopping_the_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        drop(connection);

        let database = WorldDatabase::open(&path).unwrap();
        database
            .request(DatabaseRequest::LoadLod {
                generation: 1,
                key: lod_key(4, 512, 512),
                queued_at: Instant::now(),
            })
            .unwrap();
        database
            .request(DatabaseRequest::Load {
                generation: 2,
                key: CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 2,
                    grid_y: -3,
                },
                queued_at: Instant::now(),
            })
            .unwrap();

        let lod =
            wait_for(|| database.try_lod_response()).expect("worker answered the LOD request");
        assert!(lod.result.is_err());
        assert_eq!(lod.key, lod_key(4, 512, 512));
        // The trap this guards: an early worker exit would leave the cell
        // response unanswered forever.
        let cell =
            wait_for(|| database.try_response()).expect("worker still answers cell requests");
        assert!(cell.result.is_ok());
    }

    #[test]
    fn drop_drains_a_full_queue_with_lod_requests_and_joins_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        drop(connection);

        let database = WorldDatabase::open(&path).unwrap();
        let stopped = database.worker_stopped.clone();
        for generation in 0..128 {
            database
                .request(DatabaseRequest::LoadLod {
                    generation,
                    key: lod_key(4, 0, 0),
                    queued_at: Instant::now(),
                })
                .unwrap();
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
}
