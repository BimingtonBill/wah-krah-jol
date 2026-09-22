use bevy::prelude::Resource;
use color_eyre::{Result, eyre::WrapErr};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    collections::HashMap,
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

/// How much of a cell a request asks for. The full-detail grid is loaded as [`CellDetail::Full`];
/// the terrain-only ring beyond it is loaded as [`CellDetail::Terrain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellDetail {
    /// The whole cell: the landscape, its water plane, and every reference with its light and its
    /// load door.
    Full,
    /// The landscape and its water plane only. A terrain-only cell is drawn and never entered, so
    /// nothing in it can be seen, lit, walked through or opened - and its references are never
    /// queried, which is most of the cost of a request.
    ///
    /// Exteriors only: an interior has no terrain to fall back on, so [`load_cell`] loads a
    /// terrain request for one in full.
    Terrain,
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
    /// The `lights` row of the reference's base record, when the database has one and the record is
    /// a `LIGH`. `None` for every other reference, and for every reference in a database converted
    /// before lights were exported.
    pub light: Option<LightRow>,
    /// The reference's own light radius (`XRDS`), which wins over [`LightRow::radius`]. `None` when
    /// the reference carries no override, and in a database converted before the column existed.
    pub light_radius_override: Option<f32>,
    /// Whether the reference's base object is one of Skyrim's auto-load door markers - an invisible
    /// `AutoLoadDoor*` that crosses on contact instead of on the `E` key. Read only for references
    /// that carry a [`door`](Self::door); see [`AUTO_LOAD_COLUMN`] for how the base identifies one.
    pub auto_load: bool,
}

/// A `lights` row as the converted database stores it (`docs/design/lights-and-auto-doors.md`, the
/// converter's schema 5): one row per `LIGH` record, with or without a model.
#[derive(Debug, Clone, PartialEq)]
pub struct LightRow {
    /// Radius in Creation units, from `DATA`'s radius.
    pub radius: f32,
    /// `DATA`'s colour bytes, red first.
    pub color: [u8; 3],
    /// `DATA`'s flags, uninterpreted; see the flag bits in [`crate::lights`].
    pub flags: u32,
    /// `DATA`'s falloff exponent.
    pub falloff: f32,
    /// `FNAM`'s fade, when the record has one.
    pub fade: Option<f32>,
}

/// A `door_links` row as the converted database stores it, plus the destination's label.
///
/// The destination is unresolved when [`Self::destination_cell_id`] is `None`: the converter
/// found the link but not the cell it points at. The engine turns a resolved row into a
/// [`DoorDestination`](crate::doors::DoorDestination); this type stays the table's shape.
/// The arrival frame of a link that leads back into a door: where the player lands coming through
/// it and which way they face, `(position, rotation)` in Creation-engine units and radians, exactly
/// as the `XTEL` of the door that leads there stores it. See [`DoorLinkRow::return_arrival`].
pub type ReturnArrival = ([f32; 3], [f32; 3]);

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
    /// The arrival frame of the link that leads **back** to this door, when the database has one:
    /// the `XTEL` of a door that opens into this one. The game puts that arrival point in front of
    /// this door, facing away from it, which is what gives the door its own outward direction
    /// without trusting the door model's axes ([`crate::doors::LoadDoor::outward`]). `None` for a
    /// door nothing leads back to - a one-way link, or a database whose door links the converter
    /// could not resolve.
    pub return_arrival: Option<ReturnArrival>,
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
        detail: CellDetail,
        queued_at: Instant,
    },
    Shutdown,
}

#[derive(Debug)]
pub struct DatabaseResponse {
    pub generation: u64,
    pub key: CellKey,
    /// The detail the request asked for, echoed back so the commit knows what it is holding even
    /// when the request that asked for it is no longer the one the plan wants.
    pub detail: CellDetail,
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
    /// The receiving end [`WorldDatabase::saturated_queue`] keeps open so its one request slot
    /// fills. `None` in every run: a real database's worker owns the receiving end. See
    /// [`Drop`], which releases it before the shutdown request.
    #[cfg(test)]
    held_receiver: Option<Receiver<DatabaseRequest>>,
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
            #[cfg(test)]
            held_receiver: None,
        })
    }

    pub fn request(&self, request: DatabaseRequest) -> Result<()> {
        self.requests
            .send(request)
            .wrap_err("world database worker stopped")
    }

    /// [`Self::request`] without blocking, `false` when the worker's queue is full.
    ///
    /// The planner asks for a whole ring of cells at once, hundreds of them, and the worker drains
    /// its bounded queue at one query per answer: blocking on a full queue would stall the frame
    /// for as long as that takes. A request that does not fit is left for the next frame - the
    /// plan's wanted list is ordered nearest first, so the cells closest to the camera are always
    /// the ones that do.
    pub fn try_request(&self, request: DatabaseRequest) -> bool {
        self.requests.try_send(request).is_ok()
    }

    /// A database with no worker running and a request queue of one slot that nothing drains: the
    /// first request fills it and every later one is refused with `Full`, which is what the planner
    /// sees on a frame where the worker is behind. Tests only - a run would never get a cell out of
    /// this.
    #[cfg(test)]
    pub(crate) fn saturated_queue() -> Self {
        let (requests, held_receiver) = bounded(1);
        let (_responses, responses) = unbounded();
        Self {
            requests,
            responses,
            worker: None,
            worker_stopped: Arc::new(AtomicBool::new(true)),
            // Held open and never read, so the one slot fills: dropping the receiver instead would
            // refuse requests as `Disconnected`, which is the stopped-worker case rather than a
            // full queue.
            held_receiver: Some(held_receiver),
        }
    }

    pub fn try_response(&self) -> Option<DatabaseResponse> {
        self.responses.try_recv().ok()
    }
}

impl Drop for WorldDatabase {
    fn drop(&mut self) {
        // A test-only receiver goes first: the shutdown below is a blocking send, and with nothing
        // draining the queue it would wait for a slot forever.
        #[cfg(test)]
        drop(self.held_receiver.take());
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
            detail,
            queued_at,
        } = request
        else {
            break;
        };
        let queue_wait_micros = elapsed_micros(queued_at);
        let started = Instant::now();
        let result =
            load_cell(&connection, generation, key, detail).map_err(|error| format!("{error:#}"));
        let query_micros = elapsed_micros(started);
        let row_count = result
            .as_ref()
            .map_or(0, |payload| payload.references.len());
        if responses
            .send(DatabaseResponse {
                generation,
                key,
                detail,
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

/// The `lights` row of the reference's base record, in the order [`map_reference`] reads them.
const LIGHT_COLUMNS: &str = "l.radius,l.color_r,l.color_g,l.color_b,l.flags,l.falloff,l.fade";

/// Stand-in for [`LIGHT_COLUMNS`] in a database that predates the `lights` table: every reference
/// reads as unlit, with the column order unchanged.
const ABSENT_LIGHT_COLUMNS: &str = "NULL,NULL,NULL,NULL,NULL,NULL,NULL";

/// The reference's own `XRDS` light radius, which wins over the record's. The column arrived with
/// the `lights` table in conversion schema 5; a database from before it has no such column.
const RADIUS_OVERRIDE_COLUMN: &str = "r.radius_override";
const ABSENT_RADIUS_OVERRIDE_COLUMN: &str = "NULL";

/// Whether the reference's base object is an auto-load door marker: the `statics` row of the base
/// record, whose editor id starts `AutoLoadDoor` (`AutoLoadDoor01`, `AutoLoadDoorMinUse01`,
/// `AutoLoadDoorHiddenMinUse01` in Skyrim.esm) or whose model is an `AutoLoadMarker*.nif`. In the
/// converted database the two rules agree exactly - 315 of the 2204 placed load doors, and no door
/// matched one without the other (`tools/research/auto_load_doors.py`) - so an editor id is only
/// needed for a base the model rule cannot see. SQLite's `LIKE` is case-insensitive for ASCII, and
/// the model test is lowercased anyway so the rule does not rest on that.
const AUTO_LOAD_COLUMN: &str = "CASE WHEN s.editor_id LIKE 'AutoLoadDoor%' \
OR LOWER(COALESCE(s.model_path,'')) LIKE '%autoloadmarker%' THEN 1 ELSE 0 END";

/// Stand-in for [`AUTO_LOAD_COLUMN`] for a `statics` table with no `editor_id`: the model rule
/// alone, which is all such a database can answer. Every exported database has both columns.
const ABSENT_EDITOR_ID_AUTO_LOAD_COLUMN: &str =
    "CASE WHEN LOWER(COALESCE(s.model_path,'')) LIKE '%autoloadmarker%' THEN 1 ELSE 0 END";

/// The destination label a prompt draws. An interior's `interior_name` is the `FULL` subrecord's
/// bytes as the converter read them (`crates/converter/src/esm/extractors.rs`), so it ends in the
/// NUL the game's format terminates it with - an invisible character the prompt renders as a box
/// after `Alftand01`. Trailing NULs and whitespace are not part of anyone's cell name.
fn door_label(label: String) -> String {
    label
        .trim_end_matches(|character: char| character == '\0' || character.is_whitespace())
        .to_owned()
}

const REFERENCE_JOIN: &str = " LEFT JOIN statics s ON s.id=r.base_form_id";

const DOOR_JOIN: &str = concat!(
    " LEFT JOIN door_links d ON d.ref_id=r.id",
    " LEFT JOIN cells dc ON dc.id=d.destination_cell_id",
    " LEFT JOIN worldspaces ws ON ws.id=d.destination_worldspace_id",
);

/// The `lights` row of the reference's base record: one `LIGH` record can be placed many times,
/// each reference lighting the space at its own radius.
const LIGHT_JOIN: &str = " LEFT JOIN lights l ON l.id=r.base_form_id";

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

/// Whether `statics` carries `editor_id`. Exported databases have had it since the table was
/// written, and only the engine's own fixtures predate it; without the column the auto-load rule
/// falls back to the base's model, which is all such a table can answer.
fn has_statics_editor_id(connection: &Connection) -> Result<bool> {
    let count: i64 = connection
        .prepare_cached("SELECT COUNT(*) FROM pragma_table_info('statics') WHERE name='editor_id'")?
        .query_row([], |row| row.get(0))?;
    Ok(count > 0)
}

/// Every link that leads **to** a reference, keyed by that reference's FormID: what
/// [`DoorLinkRow::return_arrival`] is filled from.
///
/// This is the data a `LEFT JOIN door_links back ON back.destination_ref_id = r.id` would bring in,
/// read in one pass instead of one join per reference. The join is the shape a reader expects, but
/// `door_links` has no index on `destination_ref_id` (the converter creates the table with `ref_id`
/// as its only key, `crates/converter/src/esm/exporter.rs`), so it would scan the whole table once
/// for every reference of every cell - and it fans out: `destination_ref_id` is not unique, and two
/// doors that lead into the same door would return that door's reference twice, spawning two doors
/// where the game has one. One pass over the table cannot duplicate a reference.
///
/// When several links lead to the same door the lowest `ref_id` wins, so the answer does not depend
/// on the order the table happens to be in.
fn return_links(connection: &Connection) -> Result<HashMap<u32, ReturnArrival>> {
    let mut statement = connection.prepare_cached(
        "SELECT ref_id,destination_ref_id,pos_x,pos_y,pos_z,rot_x,rot_y,rot_z FROM door_links",
    )?;
    let mut rows = statement.query([])?;
    let mut links: HashMap<u32, (u32, [f32; 3], [f32; 3])> = HashMap::new();
    while let Some(row) = rows.next()? {
        let ref_id: u32 = row.get(0)?;
        let destination: u32 = row.get(1)?;
        if links
            .get(&destination)
            .is_some_and(|existing| existing.0 <= ref_id)
        {
            continue;
        }
        links.insert(
            destination,
            (
                ref_id,
                [row.get(2)?, row.get(3)?, row.get(4)?],
                [row.get(5)?, row.get(6)?, row.get(7)?],
            ),
        );
    }
    Ok(links
        .into_iter()
        .map(|(destination, (_, position, rotation))| (destination, (position, rotation)))
        .collect())
}

fn load_cell(
    connection: &Connection,
    generation: u64,
    key: CellKey,
    detail: CellDetail,
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
    // A terrain-only cell is drawn and never entered, so the landscape the cell cache holds for
    // `cell_id` is everything the request needs. Answering it here also skips the table probes and
    // the reference query below, which is the whole cost of a request at this distance.
    if detail == CellDetail::Terrain && matches!(key, CellKey::Exterior { .. }) {
        return Ok(CellPayload {
            generation,
            key,
            cell_id,
            references: Vec::new(),
        });
    }
    let has_doors = has_door_links(connection)?;
    let has_lights = has_lights(connection)?;
    let has_override = has_radius_override(connection)?;
    let has_editor_id = has_statics_editor_id(connection)?;
    let door_columns = if has_doors {
        DOOR_COLUMNS
    } else {
        ABSENT_DOOR_COLUMNS
    };
    let light_columns = if has_lights {
        LIGHT_COLUMNS
    } else {
        ABSENT_LIGHT_COLUMNS
    };
    let override_column = if has_override {
        RADIUS_OVERRIDE_COLUMN
    } else {
        ABSENT_RADIUS_OVERRIDE_COLUMN
    };
    let auto_load_column = if has_editor_id {
        AUTO_LOAD_COLUMN
    } else {
        ABSENT_EDITOR_ID_AUTO_LOAD_COLUMN
    };
    let columns = format!(
        "{REFERENCE_COLUMNS},{door_columns},{light_columns},{override_column},{auto_load_column}"
    );
    let mut joins = String::from(REFERENCE_JOIN);
    if has_doors {
        joins.push_str(DOOR_JOIN);
    }
    if has_lights {
        joins.push_str(LIGHT_JOIN);
    }
    let mut references = match key {
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
    // Which way each door of this cell faces, from the links that lead back to it: the doors of
    // this cell are the rows a `door_links` row's `destination_ref_id` can name.
    if has_doors && references.iter().any(|reference| reference.door.is_some()) {
        let links = return_links(connection)?;
        for reference in &mut references {
            if let Some(door) = reference.door.as_mut() {
                door.return_arrival = links.get(&reference.form_id).copied();
            }
        }
    }
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
            label: door_label(row.get(27)?),
            // Filled in after the query, from the links that lead back into this cell.
            return_arrival: None,
        }),
        None => None,
    };
    // The `lights` row is present exactly when the join found one and it has the radius the design
    // makes `NOT NULL`; anything else is a reference this database cannot light.
    let radius: Option<f32> = row.get(28)?;
    let light = match radius {
        Some(radius) => Some(LightRow {
            radius,
            color: [row.get(29)?, row.get(30)?, row.get(31)?],
            flags: row.get(32)?,
            falloff: row.get(33)?,
            fade: row.get(34)?,
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
        light,
        light_radius_override: row.get(35)?,
        auto_load: row.get(36)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three-argument shape the tests below were written against: every one of them asks for a
    /// whole cell. A test about the terrain ring calls [`super::load_cell`] with its own detail.
    fn load_cell(connection: &Connection, generation: u64, key: CellKey) -> Result<CellPayload> {
        super::load_cell(connection, generation, key, CellDetail::Full)
    }

    fn fixture(connection: &Connection) {
        connection
            .execute_batch(&format!(
                r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
                INSERT INTO schema_info VALUES({version});
                CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER,interior_name TEXT);
                CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                CREATE TABLE statics(id INTEGER PRIMARY KEY,editor_id TEXT,model_path TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER,base_form_id INTEGER,pos_x REAL,pos_y REAL,pos_z REAL,rot_x REAL,rot_y REAL,rot_z REAL,scale REAL);
                CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
                INSERT INTO cells VALUES(10,60,2,-3,NULL);
                INSERT INTO statics(id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid) VALUES(20,'architecture/wall.nif',-1,-2,-3,1,2,3,1);
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

    /// The terrain ring asks for the cell's landscape and nothing else: the cell id is what the
    /// cell cache is keyed by, and the references - which are most of a cell's cost - are not
    /// queried at all.
    #[test]
    fn loads_a_terrain_only_exterior_cell_without_its_references() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        door_fixture(&connection);
        auto_load_fixture(&connection);
        light_fixture(&connection);
        let key = CellKey::Exterior {
            worldspace_id: 60,
            grid_x: 2,
            grid_y: -3,
        };

        let terrain_only = super::load_cell(&connection, 7, key, CellDetail::Terrain).unwrap();
        assert_eq!(terrain_only.generation, 7);
        assert_eq!(terrain_only.cell_id, 10);
        assert_eq!(terrain_only.key, key);
        assert!(
            terrain_only.references.is_empty(),
            "a terrain-only cell carries no references, lights or doors to spawn"
        );

        // The same cell at full detail is the one the ring is a cheaper version of.
        let full = load_cell(&connection, 7, key).unwrap();
        assert_eq!(full.cell_id, terrain_only.cell_id);
        assert!(
            !full.references.is_empty(),
            "the fixture's cell has references, so an empty list above means they were skipped"
        );
    }

    /// An interior has no landscape to fall back on, so a terrain request for one loads it whole:
    /// the ring never asks for an interior, and a request that does must not return a cell with
    /// nothing in it.
    #[test]
    fn loads_a_terrain_request_for_an_interior_in_full() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);

        let payload =
            super::load_cell(&connection, 1, CellKey::Interior(99), CellDetail::Terrain).unwrap();
        assert_eq!(payload.cell_id, 99);
        assert_eq!(payload.references.len(), 1);
    }

    /// The ring covers a square of grid cells, and a worldspace does not fill it: a grid with no
    /// cell at all is an error at both details, so the plan marks it failed instead of spawning an
    /// empty root for it.
    #[test]
    fn fails_a_terrain_request_for_a_grid_the_worldspace_does_not_cover() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        let absent = CellKey::Exterior {
            worldspace_id: 60,
            grid_x: 900,
            grid_y: 900,
        };
        assert!(super::load_cell(&connection, 1, absent, CellDetail::Terrain).is_err());
        assert!(super::load_cell(&connection, 1, absent, CellDetail::Full).is_err());
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
                    detail: CellDetail::Full,
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

    /// A door whose front is known from the link that leads back into it, shaped like the Alftand
    /// ruined tower's door: the door itself at 8200, -12200 and a link from another door arriving
    /// 32 units east of it, heading 92 degrees.
    #[test]
    fn returns_the_link_that_leads_back_into_a_door() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        door_fixture(&connection);
        connection
            .execute_batch(
                r#"INSERT INTO door_links VALUES(77,30,8232.0,-12200.0,50.0,0,0,1.60570,10,60);
                INSERT INTO door_links VALUES(31,78,21088.559,18512.045,2434.0,0,0,-1.87080,120,614);"#,
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
        let door_of = |form_id| {
            payload
                .references
                .iter()
                .find(|reference| reference.form_id == form_id)
                .expect("the reference is in the cell")
                .door
                .clone()
                .expect("the reference is a door")
        };

        let tower = door_of(30);
        assert_eq!(
            tower.return_arrival,
            Some(([8232.0, -12200.0, 50.0], [0.0, 0.0, 1.60570])),
            "the arrival of the door that leads here is the door's front"
        );
        assert_eq!(
            tower.destination_ref_id, 77,
            "the door's own link is untouched: it still leads to 77"
        );
        assert_eq!(tower.arrival_position, [-947.038, 3958.835, 591.917]);

        assert_eq!(
            door_of(31).return_arrival,
            None,
            "nothing leads back to a door that only leads away"
        );

        // The link that leads back is read once for the whole cell, not joined per reference, so a
        // second door leading into the same door does not return that door's reference twice.
        connection
            .execute_batch(
                "INSERT INTO door_links VALUES(78,30,8232.0,-12200.0,50.0,0,0,0.7,10,60);",
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
        assert_eq!(
            payload
                .references
                .iter()
                .filter(|reference| reference.form_id == 30)
                .count(),
            1,
            "a second link into a door must not spawn the door twice"
        );
        let tower = payload
            .references
            .iter()
            .find(|reference| reference.form_id == 30)
            .unwrap();
        assert_eq!(
            tower.door.as_ref().unwrap().return_arrival,
            Some(([8232.0, -12200.0, 50.0], [0.0, 0.0, 1.60570])),
            "two links that lead to the same door are the same doorway: the lower ref_id decides"
        );
    }

    /// The destination label is the destination interior's `FULL` subrecord bytes, which the game
    /// terminates with a NUL - the prompt drew `Alftand01` and then an empty box.
    #[test]
    fn trims_the_terminator_off_a_door_label() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        door_fixture(&connection);
        connection
            .execute(
                "UPDATE cells SET interior_name=?1 WHERE id=99",
                params!["Alftand01\0"],
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
        let door = payload
            .references
            .iter()
            .find(|reference| reference.form_id == 30)
            .expect("reference 30 is in the cell")
            .door
            .clone()
            .expect("reference 30 has a door_links row");
        assert_eq!(door.label, "Alftand01");

        assert_eq!(door_label("Alftand01".to_owned()), "Alftand01");
        assert_eq!(door_label("Blackreach \0\0 ".to_owned()), "Blackreach");
        assert_eq!(
            door_label("\0".to_owned()),
            "",
            "a label that is only the terminator is empty, not invisible"
        );
    }

    /// Four load doors on the fixture's cell, one per way a base can look: Skyrim's
    /// `AutoLoadDoor01` with its `AutoLoadMarker01.nif` (reference 41), a `MinUse` variant with
    /// another model (42), a mod marker whose editor id says nothing but whose model says
    /// "autoloadmarker" (43), and a plain `DweDoorLarge01Load` (44). See [`AUTO_LOAD_COLUMN`].
    fn auto_load_fixture(connection: &Connection) {
        connection
            .execute_batch(
                r#"INSERT INTO statics(id,editor_id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid)
                    VALUES(41,'AutoLoadDoor01','architecture/doors/AutoLoadMarker01.nif',0,0,0,0,0,0,0);
                INSERT INTO statics(id,editor_id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid)
                    VALUES(42,'AutoLoadDoorMinUse01','architecture/doors/Marker.nif',0,0,0,0,0,0,0);
                INSERT INTO statics(id,editor_id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid)
                    VALUES(43,'SomeModDoor','architecture/doors/autoloadmarker01.nif',0,0,0,0,0,0,0);
                INSERT INTO statics(id,editor_id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid)
                    VALUES(44,'DweDoorLarge01Load','dungeons/dwemer/door/dwemerlargedoorload01.nif',0,0,0,0,0,0,0);
                INSERT INTO "references" VALUES(41,10,41,8200,-12200,50,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(41,8200,8200,-12200,-12200,50,50,10,60);
                INSERT INTO "references" VALUES(42,10,42,8250,-12150,55,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(42,8250,8250,-12150,-12150,55,55,10,60);
                INSERT INTO "references" VALUES(43,10,43,8300,-12100,55,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(43,8300,8300,-12100,-12100,55,55,10,60);
                INSERT INTO "references" VALUES(44,10,44,8300,-12200,55,0,0,0,1);
                INSERT INTO exterior_spatial VALUES(44,8300,8300,-12200,-12200,55,55,10,60);
                INSERT INTO door_links VALUES(41,42,-947.038,3958.835,591.917,0,0,2.96989,99,NULL);
                INSERT INTO door_links VALUES(42,41,-947.038,3958.835,591.917,0,0,2.96989,99,NULL);
                INSERT INTO door_links VALUES(43,41,-947.038,3958.835,591.917,0,0,2.96989,99,NULL);
                INSERT INTO door_links VALUES(44,41,-947.038,3958.835,591.917,0,0,2.96989,99,NULL);"#,
            )
            .unwrap();
    }

    fn auto_load_of(connection: &Connection, form_id: u32) -> bool {
        load_cell(
            connection,
            1,
            CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 2,
                grid_y: -3,
            },
        )
        .unwrap()
        .references
        .iter()
        .find(|reference| reference.form_id == form_id)
        .expect("the reference is in the cell")
        .auto_load
    }

    #[test]
    fn marks_auto_load_doors_by_their_base_editor_id_or_model() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        door_fixture(&connection);
        auto_load_fixture(&connection);

        assert!(
            auto_load_of(&connection, 41),
            "AutoLoadDoor01 with the AutoLoadMarker01 model is an auto-load door"
        );
        assert!(
            auto_load_of(&connection, 42),
            "AutoLoadDoorMinUse01 is one too, whatever its model is called"
        );
        assert!(
            auto_load_of(&connection, 43),
            "a mod's marker is found by its model, case-insensitively"
        );
        assert!(
            !auto_load_of(&connection, 44),
            "a plain DweDoorLarge01Load keeps its E key"
        );
        assert!(
            !auto_load_of(&connection, 30),
            "the fixture's plain wall is not an auto-load door"
        );

        // The same thing as the spawned door sees it: the flag the database put on the reference is
        // the one `LoadDoor` carries, which is what crosses the door on contact.
        let spawned = |form_id| {
            crate::streaming::load_door(
                load_cell(
                    &connection,
                    1,
                    CellKey::Exterior {
                        worldspace_id: 60,
                        grid_x: 2,
                        grid_y: -3,
                    },
                )
                .unwrap()
                .references
                .iter()
                .find(|reference| reference.form_id == form_id)
                .expect("the reference is in the cell"),
            )
            .expect("the reference has a resolved door link")
            .auto_load
        };
        assert!(spawned(41), "an AutoLoadDoor01 base crosses on contact");
        assert!(!spawned(44), "a DweDoorLarge01Load base keeps the E key");
    }

    /// A `statics` table without `editor_id` is not a shape the converter ever wrote, but the
    /// engine's own fixtures had it, and the rule falls back to the base's model rather than
    /// failing the cell load.
    #[test]
    fn falls_back_to_the_model_when_statics_has_no_editor_id() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        door_fixture(&connection);
        connection
            .execute_batch(
                r#"CREATE TABLE plain_statics(id INTEGER PRIMARY KEY,model_path TEXT,
                    bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,
                    bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL);
                INSERT INTO plain_statics(id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid) VALUES(20,'architecture/wall.nif',-1,-2,-3,1,2,3,1);
                DROP TABLE statics;
                ALTER TABLE plain_statics RENAME TO statics;"#,
            )
            .unwrap();

        assert!(!auto_load_of(&connection, 30));
        assert!(!has_statics_editor_id(&connection).unwrap());
    }

    /// The schema-5 light tables: a `LIGH` record placed as reference 40 with an `XRDS` radius of
    /// its own, and the plain statics reference 30, which is not a light at all.
    fn light_fixture(connection: &Connection) {
        connection
            .execute_batch(
                r#"CREATE TABLE lights(id INTEGER PRIMARY KEY,editor_id TEXT,
                    radius REAL NOT NULL,color_r INTEGER NOT NULL,color_g INTEGER NOT NULL,
                    color_b INTEGER NOT NULL,flags INTEGER NOT NULL,falloff REAL NOT NULL,fade REAL);
                ALTER TABLE "references" ADD COLUMN radius_override REAL;
                INSERT INTO lights VALUES(21,'DefaultCandleLight01',256.0,255,150,80,8,1.25,0.5);
                INSERT INTO statics(id,model_path,bounds_min_x,bounds_min_y,bounds_min_z,bounds_max_x,bounds_max_y,bounds_max_z,bounds_valid) VALUES(21,'clutter/candle.nif',-2,-3,-4,2,3,4,1);
                INSERT INTO "references" VALUES(40,10,21,8250,-12150,55,0,0,0,1,850.8);
                INSERT INTO exterior_spatial VALUES(40,8250,8250,-12150,-12150,55,55,10,60);"#,
            )
            .unwrap();
    }

    #[test]
    fn returns_the_light_row_of_a_lit_reference_and_its_radius_override() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        light_fixture(&connection);

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
            .find(|reference| reference.form_id == 40)
            .expect("reference 40 is in the cell");
        assert_eq!(
            lit.light,
            Some(LightRow {
                radius: 256.0,
                color: [255, 150, 80],
                flags: 8,
                falloff: 1.25,
                fade: Some(0.5),
            })
        );
        assert_eq!(
            lit.light_radius_override,
            Some(850.8),
            "the reference's own XRDS radius comes back with it"
        );

        let plain = payload
            .references
            .iter()
            .find(|reference| reference.form_id == 30)
            .expect("reference 30 is in the cell");
        assert_eq!(plain.light, None, "a statics reference is not a light");
        assert_eq!(plain.light_radius_override, None);
    }

    /// A reference's light and its override are separate columns of separate tables, so a database
    /// converted between the two still loads.
    #[test]
    fn loads_lights_from_a_database_without_the_radius_override_column() {
        let connection = Connection::open_in_memory().unwrap();
        fixture(&connection);
        connection
            .execute_batch(
                r#"CREATE TABLE lights(id INTEGER PRIMARY KEY,editor_id TEXT,
                    radius REAL NOT NULL,color_r INTEGER NOT NULL,color_g INTEGER NOT NULL,
                    color_b INTEGER NOT NULL,flags INTEGER NOT NULL,falloff REAL NOT NULL,fade REAL);
                INSERT INTO lights VALUES(20,'Torch01',512.0,255,200,120,0,1.0,NULL);"#,
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
