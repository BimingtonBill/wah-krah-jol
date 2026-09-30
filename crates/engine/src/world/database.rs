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

/// The oldest world database schema the runtime reads. Schema 4 (the current
/// [`shared::WORLD_DATABASE_SCHEMA_VERSION`]) only adds tables and columns — `lights`,
/// `references.radius_override`, the movement tables and the water fresnel columns — and every
/// query probes for them, so a schema 3 database (written by converter schema 15) still loads.
pub const MIN_RUNTIME_DATABASE_SCHEMA_VERSION: u32 = 3;

/// The newest world database schema the runtime reads: the one this revision's converter writes.
pub const MAX_RUNTIME_DATABASE_SCHEMA_VERSION: u32 = shared::WORLD_DATABASE_SCHEMA_VERSION;

/// Whether the runtime (engine and `world-inspect`) reads a world database of this schema.
pub fn supports_runtime_database_schema(version: u32) -> bool {
    (MIN_RUNTIME_DATABASE_SCHEMA_VERSION..=MAX_RUNTIME_DATABASE_SCHEMA_VERSION).contains(&version)
}

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
    /// Authoritative base record type; `statics` also contains movable clutter.
    pub base_record_type: Option<String>,
    pub model_path: Option<String>,
    pub position: [f32; 3],
    pub rotation: [f32; 3],
    pub scale: f32,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub bounds_valid: bool,
    /// The `lights` row of the reference's base record, when the database has one and the record is
    /// a `LIGH`. `None` for every other reference, and for every reference in a database converted
    /// before lights were exported.
    pub light: Option<LightRow>,
    /// The reference's own light radius (`XRDS`), which wins over [`LightRow::radius`]. `None` when
    /// the reference carries no override, and in a database converted before the column existed.
    pub light_radius_override: Option<f32>,
}

/// A `lights` row as the converted database stores it
/// (`docs/specs/converters/db-schema.md`, §12): one row per `LIGH` record, with or without a model.
#[derive(Debug, Clone, PartialEq)]
pub struct LightRow {
    /// Radius in Creation units, from `DATA`'s radius.
    pub radius: f32,
    /// `DATA`'s colour bytes, red first.
    pub color: [u8; 3],
    /// `DATA`'s flags, uninterpreted; see the flag bits in [`crate::lights`].
    pub flags: u32,
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
    /// Reads every `lod_block` key of `worldspace_id` and the `lod_grid` row its
    /// blocks are laid out from, through one read-only connection.
    ///
    /// The `lod_*` tables are optional, the way the `waters` columns are: a
    /// database converted before they existed still loads. `Ok(None)` is that
    /// case — the caller disables distant LOD with a warning rather than
    /// refusing the world — and an `Err` is a database that could not be read at
    /// all.
    pub fn open(path: &Path, worldspace_id: u32) -> Result<Option<Self>> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if !has_table(&connection, "lod_block")? {
            return Ok(None);
        }
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
        drop(statement);
        // `lod_grid` is optional on its own: a worldspace the converter recorded
        // no grid row for — or a database written before the table existed — is
        // read as a grid laid out from cell 0, which is what an unoffset
        // worldspace has.
        let origin = if has_table(&connection, "lod_grid")? {
            connection
                .query_row(
                    "SELECT origin_x,origin_y FROM lod_grid WHERE worldspace_id=?1",
                    [worldspace_id],
                    |row| Ok(IVec2::new(row.get::<_, i32>(0)?, row.get::<_, i32>(1)?)),
                )
                .unwrap_or(IVec2::ZERO)
        } else {
            IVec2::ZERO
        };
        Ok(Some(Self { keys, origin }))
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

/// Whether the database carries `table`.
///
/// Every optional table is probed this way rather than by attempting the query
/// and reading the "no such table" error: the probe says what is missing before
/// the query is built. An open database's schema cannot change underneath it, so
/// the answer is read once at startup and nothing re-probes it.
fn has_table(connection: &Connection, table: &str) -> Result<bool> {
    let count: i64 = connection
        .prepare_cached("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1")?
        .query_row([table], |row| row.get(0))?;
    Ok(count > 0)
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

/// A water's Skyrim colours and reflectivity factors, decoded by the converter from its WATR
/// record's `DNAM` (`crates/converter/src/esm/exporter.rs`). `fresnel`/`reflectivity` are
/// additive `waters` columns: a database converted before they existed lacks them, so
/// [`AssetCatalog::water_colors`] returns `None` for every water rather than erroring, and
/// callers fall back to Skyrim's DefaultWater values (see `render::DEFAULT_WATER_FRESNEL` /
/// `render::DEFAULT_WATER_REFLECTIVITY`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterColors {
    pub shallow: [u8; 3],
    pub deep: [u8; 3],
    pub reflection: [u8; 3],
    pub fresnel: f32,
    pub reflectivity: f32,
}

fn unpack_color(value: u32) -> [u8; 3] {
    let bytes = value.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}

#[derive(Resource, Default)]
pub struct AssetCatalog {
    landscape_diffuse: std::collections::HashMap<u32, String>,
    landscape_normal: std::collections::HashMap<u32, String>,
    water_flow: std::collections::HashMap<u32, String>,
    water_colors: std::collections::HashMap<u32, WaterColors>,
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
        // Each landscape texture's normal map, where its texture set has one. A database written
        // before `texture_sets` carried `normal_path` (the in-memory fixtures) has none, and the
        // terrain then draws with the geometric normal alone.
        let landscape_normal = connection
            .prepare(
                "SELECT l.id,t.normal_path FROM landscape_textures l JOIN texture_sets t ON t.id=l.texture_set_id WHERE t.normal_path IS NOT NULL AND t.normal_path <> ''",
            )
            .map(|mut statement| {
                statement
                    .query_map([], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?)))
                    .map(|rows| {
                        rows.filter_map(std::result::Result::ok)
                            .filter_map(|(id, path)| converted_texture_path(path).map(|path| (id, path)))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
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
        drop(statement);
        // A database converted before `fresnel`/`reflectivity` existed has no such columns, and
        // `prepare` fails with a "no such column" error rather than an empty result. Treat that
        // the same as no water colours at all instead of refusing to open the catalog.
        let water_colors = connection
            .prepare(
                "SELECT id,shallow_color,deep_color,reflection_color,fresnel,reflectivity FROM waters \
                 WHERE shallow_color IS NOT NULL AND deep_color IS NOT NULL \
                 AND reflection_color IS NOT NULL AND fresnel IS NOT NULL AND reflectivity IS NOT NULL",
            )
            .ok()
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, u32>(0)?,
                            WaterColors {
                                shallow: unpack_color(row.get::<_, u32>(1)?),
                                deep: unpack_color(row.get::<_, u32>(2)?),
                                reflection: unpack_color(row.get::<_, u32>(3)?),
                                fresnel: row.get::<_, f32>(4)?,
                                reflectivity: row.get::<_, f32>(5)?,
                            },
                        ))
                    })
                    .ok()
                    .map(|rows| rows.filter_map(std::result::Result::ok).collect())
            })
            .unwrap_or_default();
        Ok(Self {
            landscape_diffuse,
            landscape_normal,
            water_flow,
            water_colors,
        })
    }

    pub fn landscape_diffuse(&self, form_id: u32) -> Option<&str> {
        self.landscape_diffuse.get(&form_id).map(String::as_str)
    }

    /// The converted normal map of landscape texture `form_id`, if its texture set has one.
    pub fn landscape_normal(&self, form_id: u32) -> Option<&str> {
        self.landscape_normal.get(&form_id).map(String::as_str)
    }

    pub fn water_flow(&self, form_id: u32) -> Option<&str> {
        self.water_flow.get(&form_id).map(String::as_str)
    }

    pub fn water_colors(&self, form_id: u32) -> Option<WaterColors> {
        self.water_colors.get(&form_id).copied()
    }
}

/// Maps a texture path stored in the world database to the converted `.ktx2` under `textures/`.
///
/// The database is input the engine did not write, so a path that could leave `textures/` - a
/// `..` or `.` segment, an absolute or drive-prefixed path, an empty segment, or a `:` (a Windows
/// drive or stream, or an asset source such as `embedded://`) - is refused rather than loaded.
fn converted_texture_path(path: String) -> Option<String> {
    // Converted assets are published with lowercase canonical paths, so the
    // lookup must lowercase too (matching world-inspect's resolver).
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let without_prefix = normalized.strip_prefix("textures/").unwrap_or(&normalized);
    if without_prefix.is_empty()
        || !is_safe_relative_asset_path(without_prefix)
        || without_prefix.contains(':')
        || without_prefix
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return None;
    }
    let mut converted = std::path::PathBuf::from("textures").join(without_prefix);
    converted.set_extension("ktx2");
    Some(converted.to_string_lossy().replace('\\', "/"))
}

/// Rejects a database-supplied relative path that could escape the assets
/// root once re-rooted under `textures/` or `meshes/` with `PathBuf::join`.
/// A `..` segment walks back out of the base directory, and a rooted or
/// drive-prefixed path makes `PathBuf::join` replace the base entirely
/// instead of appending to it (see the `std::path::PathBuf::push` docs).
/// Every component must therefore be a plain, non-empty path segment.
fn is_safe_relative_asset_path(path: &str) -> bool {
    std::path::Path::new(path)
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
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
        supports_runtime_database_schema(version),
        "world database schema {version} is unsupported; supported versions are {} through {}",
        MIN_RUNTIME_DATABASE_SCHEMA_VERSION,
        MAX_RUNTIME_DATABASE_SCHEMA_VERSION
    );
    Ok(())
}

fn worker(
    path: std::path::PathBuf,
    requests: Receiver<DatabaseRequest>,
    responses: Sender<DatabaseResponse>,
    lod_responses: Sender<LodResponse>,
) {
    // Which optional tables and columns this database has does not change while it is open, so
    // the reference query is built once for the connection. If the database cannot be opened or
    // probed, every request is still answered, with that error, so the cells fail visibly instead
    // of waiting forever.
    let setup = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(color_eyre::eyre::Report::from)
    .and_then(|connection| {
        let query = ReferenceQuery::for_connection(&connection)?;
        Ok((connection, query))
    })
    .map_err(|error| format!("world database {} is unusable: {error:#}", path.display()));
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
                let result = match &setup {
                    Ok((connection, query)) => load_cell(connection, query, generation, key)
                        .map_err(|error| format!("{error:#}")),
                    Err(error) => Err(error.clone()),
                };
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
                let result = match &setup {
                    Ok((connection, _)) => {
                        load_lod_block(connection, key).map_err(|error| format!("{error:#}"))
                    }
                    Err(error) => Err(error.clone()),
                };
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

/// The reference columns [`map_reference`] reads, in order: the placement itself and the base
/// object's model and bounds. Every table this query can be missing is joined on after them, so the
/// indices below are the same whichever tables a database has.
const REFERENCE_COLUMNS: &str = "r.id,r.cell_id,r.base_form_id,s.model_path,r.pos_x,r.pos_y,r.pos_z,\
     r.rot_x,r.rot_y,r.rot_z,r.scale,\
     COALESCE(s.bounds_min_x,-64),COALESCE(s.bounds_min_y,-64),COALESCE(s.bounds_min_z,-64),\
     COALESCE(s.bounds_max_x,64),COALESCE(s.bounds_max_y,64),COALESCE(s.bounds_max_z,64),\
     COALESCE(s.bounds_valid,0)";

const REFERENCE_JOIN: &str = " LEFT JOIN statics s ON s.id=r.base_form_id";

/// The `lights` row of the reference's base record, in the order [`map_reference`] reads them.
const LIGHT_COLUMNS: &str = "l.radius,l.color_r,l.color_g,l.color_b,l.flags";

/// Stand-in for [`LIGHT_COLUMNS`] in a database that predates the `lights` table: every reference
/// reads as unlit, with the column order unchanged.
const ABSENT_LIGHT_COLUMNS: &str = "NULL,NULL,NULL,NULL,NULL";

/// The reference's own `XRDS` light radius, which wins over the record's. The column arrived with
/// the `lights` table in conversion schema 4; a database from before it has no such column.
const RADIUS_OVERRIDE_COLUMN: &str = "r.radius_override";
const ABSENT_RADIUS_OVERRIDE_COLUMN: &str = "NULL";

/// The `lights` row of the reference's base record: one `LIGH` record can be placed many times,
/// each reference lighting the space at its own radius.
const LIGHT_JOIN: &str = " LEFT JOIN lights l ON l.id=r.base_form_id";

fn has_records(connection: &Connection) -> Result<bool> {
    let count: i64 = connection
        .prepare_cached("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='records'")?
        .query_row([], |row| row.get(0))?;
    Ok(count > 0)
}

/// Whether the database carries the `lights` table. A database converted before lights were
/// exported still loads; every reference then reads as unlit.
fn has_lights(connection: &Connection) -> Result<bool> {
    let count: i64 = connection
        .prepare_cached("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='lights'")?
        .query_row([], |row| row.get(0))?;
    Ok(count > 0)
}

/// Whether `"references"` carries the `XRDS` light radius override. It arrived with the `lights`
/// table, but the two are detected separately: the override is a reference column, and a database
/// with the table and without the column must still load.
fn has_radius_override(connection: &Connection) -> Result<bool> {
    let count: i64 = connection
        .prepare_cached(
            "SELECT COUNT(*) FROM pragma_table_info('references') WHERE name='radius_override'",
        )?
        .query_row([], |row| row.get(0))?;
    Ok(count > 0)
}

/// The reference query's column list and joins for one database: the `lights` table and the
/// `radius_override` column are joined when the database has them and read as `NULL` when it does
/// not, so [`map_reference`]'s column indices are the same either way.
struct ReferenceQuery {
    columns: String,
    joins: String,
}

impl ReferenceQuery {
    fn for_connection(connection: &Connection) -> Result<Self> {
        let has_records = has_records(connection)?;
        let record_column = if has_records { "b.record_type" } else { "NULL" };
        let has_lights = has_lights(connection)?;
        let light_columns = if has_lights {
            LIGHT_COLUMNS
        } else {
            ABSENT_LIGHT_COLUMNS
        };
        let override_column = if has_radius_override(connection)? {
            RADIUS_OVERRIDE_COLUMN
        } else {
            ABSENT_RADIUS_OVERRIDE_COLUMN
        };
        let mut joins = String::from(REFERENCE_JOIN);
        if has_records {
            joins.push_str(" LEFT JOIN records b ON b.form_id=r.base_form_id");
        }
        if has_lights {
            joins.push_str(LIGHT_JOIN);
        }
        Ok(Self {
            columns: format!(
                "{REFERENCE_COLUMNS},{record_column},{light_columns},{override_column}"
            ),
            joins,
        })
    }
}

fn load_cell(
    connection: &Connection,
    query: &ReferenceQuery,
    generation: u64,
    key: CellKey,
) -> Result<CellPayload> {
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
    let ReferenceQuery { columns, joins } = query;
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
    let base_record_type: Option<String> = row.get(18)?;
    let radius: Option<f32> = row.get(19)?;
    let light = match radius {
        Some(radius) => Some(LightRow {
            radius,
            color: [row.get(20)?, row.get(21)?, row.get(22)?],
            flags: row.get(23)?,
        }),
        None => None,
    };
    Ok(ReferenceRow {
        form_id: row.get(0)?,
        cell_id: row.get(1)?,
        base_form_id: row.get(2)?,
        base_record_type,
        model_path: row.get(3)?,
        position: [row.get(4)?, row.get(5)?, row.get(6)?],
        rotation: [row.get(7)?, row.get(8)?, row.get(9)?],
        scale: row.get(10)?,
        bounds_min: [row.get(11)?, row.get(12)?, row.get(13)?],
        bounds_max: [row.get(14)?, row.get(15)?, row.get(16)?],
        bounds_valid: row.get(17)?,
        light,
        light_radius_override: row.get(24)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `load_cell` with the reference query built for `connection` as it is now, the way the
    /// worker builds it when it opens a database.
    fn load_cell(connection: &Connection, generation: u64, key: CellKey) -> Result<CellPayload> {
        super::load_cell(
            connection,
            &ReferenceQuery::for_connection(connection)?,
            generation,
            key,
        )
    }

    fn fixture(connection: &Connection) {
        connection
            .execute_batch(
                r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
                INSERT INTO schema_info VALUES(4);
                CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER);
                CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                CREATE TABLE statics(id INTEGER PRIMARY KEY,model_path TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER,base_form_id INTEGER,pos_x REAL,pos_y REAL,pos_z REAL,rot_x REAL,rot_y REAL,rot_z REAL,scale REAL);
                CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
                INSERT INTO cells VALUES(10,60,2,-3);
                INSERT INTO statics VALUES(20,'architecture/wall.nif',-1,-2,-3,1,2,3,1);
                INSERT INTO "references" VALUES(30,10,20,8200,-12200,50,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(30,8200,8200,-12200,-12200,50,50,10,60);
                INSERT INTO "references" VALUES(31,99,20,8250,-12150,55,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(31,8250,8250,-12150,-12150,55,55,99,60);"#,
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
    fn rejects_traversal_and_rooted_texture_paths() {
        assert_eq!(
            converted_texture_path("textures/../../secrets.dds".to_owned()),
            None
        );
        assert_eq!(
            converted_texture_path("textures//etc/passwd".to_owned()),
            None
        );
        assert_eq!(
            converted_texture_path(r"textures\..\..\secrets.dds".to_owned()),
            None
        );
        // Windows treats a drive-prefixed path as absolute (and `PathBuf::join`
        // would let it replace the base path entirely); Rust's path parsing is
        // OS-native, so this case only bites on the Windows target this engine
        // ships for.
        #[cfg(windows)]
        assert_eq!(
            converted_texture_path("textures/C:/Windows/evil.dds".to_owned()),
            None
        );
        // A plain relative path is unaffected.
        assert_eq!(
            converted_texture_path("textures/land/grass.dds".to_owned()),
            Some("textures/land/grass.ktx2".to_owned())
        );
    }

    #[test]
    fn base_record_type_distinguishes_fixed_static_from_movable_model() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        connection
            .execute_batch(
                "CREATE TABLE records(form_id INTEGER PRIMARY KEY,record_type TEXT NOT NULL);
                 INSERT INTO records VALUES(20,'STAT');
                 INSERT INTO statics VALUES(22,'clutter/barrel.nif',-1,-1,-1,1,1,1,1);
                 INSERT INTO records VALUES(22,'MISC');
                 INSERT INTO \"references\" VALUES(40,10,22,8250,-12150,55,0,0,0,1);
                 INSERT INTO exterior_spatial VALUES(40,8250,8250,-12150,-12150,55,55,10,60);",
            )
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
        let fixed = payload.references.iter().find(|r| r.form_id == 30).unwrap();
        let movable = payload.references.iter().find(|r| r.form_id == 40).unwrap();
        assert_eq!(fixed.base_record_type.as_deref(), Some("STAT"));
        assert_eq!(movable.base_record_type.as_deref(), Some("MISC"));
    }

    #[test]
    fn catalog_rewrites_landscape_texture_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT); CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER); CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT); INSERT INTO texture_sets VALUES(2,'Textures\\Landscape\\Tundra02.DDS'); INSERT INTO landscape_textures VALUES(1,2); INSERT INTO waters VALUES(9,'textures/water/flow.dds');",
            )
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&path).unwrap();
        assert_eq!(
            catalog.landscape_diffuse(1),
            Some("textures/landscape/tundra02.ktx2")
        );
        assert_eq!(catalog.water_flow(9), Some("textures/water/flow.ktx2"));
        // This fixture's `waters` table predates the fresnel/reflectivity columns entirely, the
        // same shape a database converted before this change has; the catalog must tolerate it
        // rather than fail to open.
        assert_eq!(catalog.water_colors(9), None);
    }

    #[test]
    fn catalog_returns_water_colors_and_factors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT); \
                 CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER); \
                 CREATE TABLE waters(id INTEGER PRIMARY KEY,shallow_color INTEGER,deep_color INTEGER,reflection_color INTEGER,fresnel REAL,reflectivity REAL,flow_normal_path TEXT);",
            )
            .unwrap();
        let shallow = u32::from_le_bytes([38, 39, 24, 0]);
        let deep = u32::from_le_bytes([5, 14, 18, 0]);
        let reflection = u32::from_le_bytes([119, 140, 157, 0]);
        connection
            .execute(
                "INSERT INTO waters(id,shallow_color,deep_color,reflection_color,fresnel,reflectivity,flow_normal_path) VALUES (?1,?2,?3,?4,?5,?6,NULL)",
                params![18u32, shallow, deep, reflection, 0.10f32, 0.8f32],
            )
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&path).unwrap();
        assert_eq!(
            catalog.water_colors(18),
            Some(WaterColors {
                shallow: [38, 39, 24],
                deep: [5, 14, 18],
                reflection: [119, 140, 157],
                fresnel: 0.10,
                reflectivity: 0.8,
            })
        );
    }

    #[test]
    fn texture_paths_that_could_leave_the_textures_folder_are_refused() {
        for path in [
            "textures/../../secret.dds",
            "..\\..\\secret.dds",
            "textures/land/../../../secret.dds",
            "/etc/secret.dds",
            "C:/Windows/secret.dds",
            "C:secret.dds",
            "textures//grass.dds",
            "textures/./grass.dds",
            "embedded://grass.dds",
            "textures/grass.dds:stream",
            "",
            "textures/",
        ] {
            assert_eq!(converted_texture_path(path.to_owned()), None, "{path}");
        }
        assert_eq!(
            converted_texture_path("Textures\\Land\\Grass01.dds".to_owned()).as_deref(),
            Some("textures/land/grass01.ktx2")
        );
        assert_eq!(
            converted_texture_path("land/grass..old.dds".to_owned()).as_deref(),
            Some("textures/land/grass..old.ktx2")
        );
    }

    #[test]
    fn catalog_finds_landscape_normal_maps_and_tolerates_their_absence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT,normal_path TEXT); CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER); CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT); INSERT INTO texture_sets VALUES(2,'textures/land/snow.dds','textures/land/snow_n.dds'); INSERT INTO texture_sets VALUES(3,'textures/land/dirt.dds',''); INSERT INTO landscape_textures VALUES(1,2); INSERT INTO landscape_textures VALUES(4,3);",
            )
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&path).unwrap();
        assert_eq!(
            catalog.landscape_normal(1),
            Some("textures/land/snow_n.ktx2")
        );
        assert_eq!(
            catalog.landscape_normal(4),
            None,
            "an empty normal path is no normal map"
        );

        // A database whose texture sets have no normal_path column at all still opens; the
        // terrain then draws with its geometric normal.
        let older = directory.path().join("older.db");
        let connection = Connection::open(&older).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT); CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER); CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT); INSERT INTO texture_sets VALUES(2,'textures/land/grass.dds'); INSERT INTO landscape_textures VALUES(1,2);",
            )
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&older).unwrap();
        assert_eq!(
            catalog.landscape_diffuse(1),
            Some("textures/land/grass.ktx2")
        );
        assert_eq!(catalog.landscape_normal(1), None);
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
    fn accepts_schema_four_database_with_legacy_query_columns() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        connection
            .execute("UPDATE schema_info SET version=4", [])
            .unwrap();
        drop(connection);
        validate(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
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
        connection
            .execute("UPDATE schema_info SET version=5", [])
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
    fn an_unusable_database_answers_every_request_with_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.db");
        let (request_tx, request_rx) = bounded(4);
        let (response_tx, response_rx) = unbounded();
        let (lod_response_tx, _lod_response_rx) = unbounded();
        let key = CellKey::Interior(99);
        for generation in 0..2 {
            request_tx
                .send(DatabaseRequest::Load {
                    generation,
                    key,
                    queued_at: Instant::now(),
                })
                .unwrap();
        }
        request_tx.send(DatabaseRequest::Shutdown).unwrap();

        worker(path, request_rx, response_tx, lod_response_tx);

        let responses: Vec<DatabaseResponse> = response_rx.try_iter().collect();
        assert_eq!(responses.len(), 2, "every request is answered");
        for response in responses {
            let error = response
                .result
                .expect_err("the cell fails instead of loading");
            assert!(error.contains("is unusable"), "{error}");
        }
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

    /// Adds the optional `lod_block`/`lod_grid` tables and a few rows to a fixture connection.
    ///
    /// Kept out of [`fixture`] on purpose: the LOD tables are optional to the engine, so the tests
    /// that care about their absence have to be able to build a database without them.
    fn lod_fixture(connection: &Connection) {
        connection
            .execute_batch(
                r#"CREATE TABLE lod_block(worldspace_id INTEGER NOT NULL,kind TEXT NOT NULL,level INTEGER NOT NULL,block_x INTEGER NOT NULL,block_y INTEGER NOT NULL,mesh_path TEXT NOT NULL,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,PRIMARY KEY (worldspace_id,kind,level,block_x,block_y));
                INSERT INTO lod_block VALUES(60,'terrain',4,0,0,'meshes/terrain/tamriel/tamriel.4.0.0.glb',-16384,-512,-16384,0,2048,0);
                INSERT INTO lod_block VALUES(60,'terrain',8,-8,-8,'meshes/terrain/tamriel/tamriel.8.-8.-8.glb',NULL,NULL,NULL,NULL,NULL,NULL);
                INSERT INTO lod_block VALUES(60,'objects',4,0,0,'meshes/terrain/tamriel/objects/tamriel.4.0.0.glb',-16384,-512,-16384,0,2048,0);
                INSERT INTO lod_block VALUES(61,'terrain',4,0,0,'meshes/terrain/otherworld/otherworld.4.0.0.glb',0,0,0,1,1,1);"#,
            )
            .unwrap();
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
        lod_fixture(&connection);
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
        lod_fixture(&connection);
        drop(connection);

        let table = LodBlockTable::open(&path, 60)
            .unwrap()
            .expect("the table is there");
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
        lod_fixture(&connection);
        connection
            .execute_batch(
                "CREATE TABLE lod_grid(worldspace_id INTEGER PRIMARY KEY,origin_x INTEGER NOT NULL,origin_y INTEGER NOT NULL,levels TEXT NOT NULL);
                INSERT INTO lod_grid VALUES(60,-23,-9,'4,8,16,32');",
            )
            .unwrap();
        drop(connection);

        // Blackreach's origin: its blocks are named from (-23, -9).
        let table = LodBlockTable::open(&path, 60).unwrap().unwrap();
        assert_eq!(table.origin(), IVec2::new(-23, -9));
        assert_eq!(table.len(), 3);
        // A worldspace the table does not cover keeps the cell-0 grid.
        let other = LodBlockTable::open(&path, 61).unwrap().unwrap();
        assert_eq!(other.origin(), IVec2::ZERO);
    }

    /// The `lod_*` tables are additive: a database converted before the LOD stage existed keeps
    /// loading, and the tier that needs them disables itself rather than refusing the world.
    #[test]
    fn a_database_without_the_lod_tables_still_opens() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        drop(connection);

        assert!(
            LodBlockTable::open(&path, 60).unwrap().is_none(),
            "a database with no lod_block table has no LOD blocks"
        );
        let database = WorldDatabase::open(&path).expect("the world still opens");
        database
            .request(DatabaseRequest::Load {
                generation: 1,
                key: CellKey::Exterior {
                    worldspace_id: 60,
                    grid_x: 2,
                    grid_y: -3,
                },
                queued_at: Instant::now(),
            })
            .unwrap();
        let response = wait_for(|| database.try_response()).expect("cells still stream");
        assert!(response.result.is_ok());
    }

    #[test]
    fn a_lod_request_for_a_missing_block_reports_an_error_without_stopping_the_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("world.db");
        let connection = Connection::open(&path).unwrap();
        fixture(&connection);
        lod_fixture(&connection);
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
        lod_fixture(&connection);
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

    #[test]
    fn loads_interior_cell_without_spatial_lookup() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        let payload = load_cell(&connection, 4, CellKey::Interior(99)).unwrap();
        assert_eq!(payload.cell_id, 99);
        assert_eq!(payload.references.len(), 1);
        assert_eq!(payload.references[0].form_id, 31);
    }

    /// A `lights` table and an `XRDS` override, with the columns the runtime reads (the converter's
    /// table carries more, all of them unread). Reference 30 is lit - its base 20 has a `lights` row
    /// and it carries an override - and reference 40 is not, because its base 22 has no row.
    fn light_fixture(connection: &Connection) {
        connection
            .execute_batch(
                r#"CREATE TABLE lights(id INTEGER PRIMARY KEY,
                    radius REAL NOT NULL,color_r INTEGER NOT NULL,color_g INTEGER NOT NULL,
                    color_b INTEGER NOT NULL,flags INTEGER NOT NULL);
                INSERT INTO lights VALUES(20,256.0,255,150,80,8);
                INSERT INTO statics(id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid) VALUES(22,'clutter/barrel.nif',-2,-3,-4,2,3,4,1);
                INSERT INTO "references" VALUES(40,10,22,8250,-12150,55,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(40,8250,8250,-12150,-12150,55,55,10,60);
                ALTER TABLE "references" ADD COLUMN radius_override REAL;
                UPDATE "references" SET radius_override=850.8 WHERE id=30;"#,
            )
            .unwrap();
    }

    #[test]
    fn returns_the_light_row_of_a_lit_reference_and_its_radius_override() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        light_fixture(&connection);
        assert!(has_lights(&connection).unwrap());
        assert!(has_radius_override(&connection).unwrap());

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

        let lit = payload
            .references
            .iter()
            .find(|reference| reference.form_id == 30)
            .expect("reference 30 is in the cell");
        assert_eq!(
            lit.light,
            Some(LightRow {
                radius: 256.0,
                color: [255, 150, 80],
                flags: 8,
            })
        );
        assert_eq!(
            lit.light_radius_override,
            Some(850.8),
            "the reference's own XRDS radius comes back with it"
        );

        let unlit = payload
            .references
            .iter()
            .find(|reference| reference.form_id == 40)
            .expect("reference 40 is in the cell");
        assert_eq!(
            unlit.light, None,
            "a reference whose base has no lights row is not a light"
        );
        assert_eq!(unlit.light_radius_override, None);
        assert_eq!(
            unlit.model_path.as_deref(),
            Some("clutter/barrel.nif"),
            "and it still joins its base object"
        );
    }

    /// A reference's light and its override are separate columns of separate tables, so a database
    /// converted between the two still loads.
    #[test]
    fn loads_lights_from_a_database_without_the_radius_override_column() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        connection
            .execute_batch(
                r#"CREATE TABLE lights(id INTEGER PRIMARY KEY,
                    radius REAL NOT NULL,color_r INTEGER NOT NULL,color_g INTEGER NOT NULL,
                    color_b INTEGER NOT NULL,flags INTEGER NOT NULL);
                INSERT INTO lights VALUES(20,512.0,255,200,120,0);"#,
            )
            .unwrap();
        assert!(!has_radius_override(&connection).unwrap());

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

        let lit = payload
            .references
            .iter()
            .find(|reference| reference.form_id == 30)
            .expect("reference 30 is in the cell");
        assert_eq!(
            lit.light.as_ref().map(|light| light.radius),
            Some(512.0),
            "the light still loads without the override column"
        );
        assert_eq!(lit.light_radius_override, None);
    }

    #[test]
    fn loads_a_database_whose_references_are_all_unlit() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);

        assert!(!has_lights(&connection).unwrap());
        assert!(!has_radius_override(&connection).unwrap());
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
        assert!(payload.references.iter().all(
            |reference| reference.light.is_none() && reference.light_radius_override.is_none()
        ));
        assert_eq!(
            payload.references[0].model_path.as_deref(),
            Some("architecture/wall.nif"),
            "the plain query still joins the base object"
        );
    }
}
