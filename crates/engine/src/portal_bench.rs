//! `--portal-bench <doors.json> [--bench-out <file.csv>]`: a quick, automated benchmark of the
//! portal at a fixed list of dense doors, measured in GPU and CPU time as well as frame time.
//!
//! The doors come from `tools/bench/portal_bench_doors.json`, which
//! `tools/bench/portal_bench_doors.py` picks from the world database: the city doors (Whiterun,
//! Solitude, Windhelm, Markarth, Riften and their interiors) with the most references around them
//! and beyond them. The run does not walk and does not use the demo tour. For each door it
//!
//! 1. moves the camera into the door's space ([`switch_space`], as a crossing and a `--shots` shot
//!    do) and waits for the door to stream in;
//! 2. stands [`STANDOFF`] units in front of it, facing it, and waits until streaming has settled
//!    (the `--shots` rule, held for [`DOOR_QUIET_FRAMES`] frames so the door's clips resolve);
//! 3. times [`BenchState`]s for [`TIMING_SECONDS`] each, after [`STATE_SETTLE_SECONDS`]: `closed`;
//!    then opens the door (the [`OpenDoor`] message a player's `E` writes), waits until it is fully
//!    `Open` and the view has settled again, and times `open-in-view`, `open-behind` (turned half a
//!    turn) and `open-occluded` (standing behind the door's wall, facing the doorway);
//! 4. closes the door and moves on.
//!
//! At the end it writes one CSV row per door and state ([`CSV_HEADER`]) and a summary beside it
//! (`<csv>.summary.txt`), then exits.
//!
//! # What is measured
//!
//! The machine this was written for forces V-Sync in the driver, so frame time never drops below
//! the display's refresh (impl-194's `--tour-bench` read 16.67 ms in every state). The bench asks
//! for `AutoNoVsync` and a continuous event loop, as benchmark runs do, and records numbers V-Sync
//! cannot hide:
//!
//! * **GPU time**, from Bevy's `RenderDiagnosticsPlugin` timestamp queries. Its spans are named by
//!   pass, not by camera: the main camera, the portal camera and the water reflection camera all
//!   write `render/main_opaque_pass_3d/elapsed_gpu`, as separate measurements of one frame. The
//!   bench groups the measurements of one frame by the instant they were stored at (one sync per
//!   frame) and records the sum of every top-level pass (`gpu`), the opaque pass summed over the
//!   cameras (`opaque_gpu`), the last opaque pass of the frame - the main camera, which renders last
//!   (order 0, after the portal's -2 and the water's -1) - (`main_opaque_gpu`), and the rest
//!   (`offscreen_opaque_gpu`: the portal camera's, plus the water reflection's when
//!   `water_active` says it was on).
//! * **CPU time**: the main world's frame, from the start of `First` to the end of `Last`. With
//!   pipelined rendering the present wait happens on the render thread, so it is not in this.
//! * **Frame time**: `Time<Real>`'s delta, as before.

use crate::{
    config::grid_of,
    doors::{DoorState, LoadDoor},
    metrics::percentile,
    portal::PortalTexture,
    shots::{HexFormId, SETTLE_QUIET_FRAMES, settle_counts},
    streaming::{ActiveCell, RenderOrigin, StreamingMetrics, StreamingWorld, creation_to_bevy},
    transition::{OpenDoor, SpaceTarget, switch_space},
    world::{
        components::{CELL_SIZE, ExteriorCellGrid, StreamingCamera},
        database::CellKey,
    },
};
use bevy::{
    camera::RenderTarget, diagnostic::DiagnosticsStore, platform::time::Instant, prelude::*,
};
use color_eyre::{Result, eyre::WrapErr};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

/// How far in front of the door the camera stands, in Creation units: the demo tour's standoff.
pub const STANDOFF: f32 = 320.0;
/// The camera's height above the door's origin: a player's eye.
pub const EYE_HEIGHT: f32 = 120.0;
/// How long a state is held before its frames are timed, so a turn or a door's last frame of swing
/// does not land in the numbers.
pub const STATE_SETTLE_SECONDS: f32 = 1.0;
/// How long each state is timed for.
pub const TIMING_SECONDS: f32 = 3.0;
/// Quiet frames before a door is timed closed and asked to open: [`crate::shots`]' own wait for a
/// door's clips to resolve (`DOOR_QUIET_FRAMES`), so the door swings rather than opening as a
/// static leaf.
pub const DOOR_QUIET_FRAMES: u32 = 120;
/// How long a door may take to stream in after the camera is moved to it.
pub const DOOR_SPAWN_TIMEOUT_SECONDS: f32 = 60.0;
/// How long a view may take to settle before it is timed anyway (and the log says so).
pub const SETTLE_TIMEOUT_SECONDS: f32 = 30.0;
/// How long an asked door may take to be fully open.
pub const OPEN_TIMEOUT_SECONDS: f32 = 10.0;
/// How long an asked door may take to close again.
pub const CLOSE_TIMEOUT_SECONDS: f32 = 10.0;

/// The CSV's columns, one row per door and state.
pub const CSV_HEADER: &str = "door,place,state,frames,frame_mean_ms,frame_p95_ms,cpu_mean_ms,\
cpu_p95_ms,gpu_frames,gpu_mean_ms,gpu_p95_ms,opaque_gpu_mean_ms,opaque_gpu_p95_ms,\
main_opaque_gpu_mean_ms,main_opaque_gpu_p95_ms,offscreen_opaque_gpu_mean_ms,\
offscreen_opaque_gpu_p95_ms,portal_active,water_active";

/// The doors file `tools/bench/portal_bench_doors.py` writes.
#[derive(Debug, Clone, Deserialize)]
pub struct DoorsFile {
    pub doors: Vec<BenchDoor>,
}

/// One door of the doors file. Fields the engine does not read (the counts, the reason) are
/// ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct BenchDoor {
    pub ref_id: HexFormId,
    #[serde(default)]
    pub place: Option<String>,
    #[serde(default)]
    pub worldspace_id: Option<u32>,
    #[serde(default)]
    pub interior_cell_id: Option<u32>,
    /// The door reference's position, in Creation units.
    pub position: [f32; 3],
}

impl BenchDoor {
    fn space(&self) -> Option<SpaceTarget> {
        match (self.interior_cell_id, self.worldspace_id) {
            (Some(cell), _) => Some(SpaceTarget::Interior(cell)),
            (None, Some(worldspace)) => Some(SpaceTarget::Exterior(worldspace)),
            (None, None) => None,
        }
    }

    fn place(&self) -> &str {
        self.place.as_deref().unwrap_or("")
    }
}

impl DoorsFile {
    pub fn from_json(text: &str) -> Result<Self> {
        let file: Self = serde_json::from_str(text).wrap_err("not a portal bench doors file")?;
        color_eyre::eyre::ensure!(!file.doors.is_empty(), "the doors file lists no doors");
        for door in &file.doors {
            color_eyre::eyre::ensure!(
                door.space().is_some(),
                "door {:08X} names neither a worldspace nor an interior cell",
                door.ref_id.0
            );
        }
        Ok(file)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        Self::from_json(&text).wrap_err_with(|| format!("in {}", path.display()))
    }
}

/// A state the bench times a door in, in the order it times them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchState {
    /// The door closed, the view facing it: the portal has nothing to draw.
    Closed,
    /// The door fully open, the view facing it: the portal draws the room beyond.
    OpenInView,
    /// The door open, the view turned half a turn: the doorway is off screen.
    OpenBehind,
    /// The door open, the view standing behind the door's wall and facing the doorway: in the
    /// frustum, but behind the wall the door stands in.
    OpenOccluded,
}

impl BenchState {
    pub const ALL: [BenchState; 4] = [
        BenchState::Closed,
        BenchState::OpenInView,
        BenchState::OpenBehind,
        BenchState::OpenOccluded,
    ];

    pub fn name(self) -> &'static str {
        match self {
            BenchState::Closed => "closed",
            BenchState::OpenInView => "open-in-view",
            BenchState::OpenBehind => "open-behind",
            BenchState::OpenOccluded => "open-occluded",
        }
    }

    /// What the bench does after timing this state.
    fn after(self) -> After {
        match self {
            BenchState::Closed => After::OpenTheDoor,
            BenchState::OpenInView => After::Time(BenchState::OpenBehind),
            BenchState::OpenBehind => After::Time(BenchState::OpenOccluded),
            BenchState::OpenOccluded => After::CloseTheDoor,
        }
    }

    /// Where the camera stands for this state: on the door's front or behind it, and whether it
    /// faces the door or away from it.
    fn pose(self) -> (Side, Facing) {
        match self {
            BenchState::Closed | BenchState::OpenInView => (Side::Front, Facing::Door),
            BenchState::OpenBehind => (Side::Front, Facing::Away),
            BenchState::OpenOccluded => (Side::Back, Facing::Door),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Time(BenchState),
    OpenTheDoor,
    CloseTheDoor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Front,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Facing {
    Door,
    Away,
}

/// The states a door is timed in when everything goes to plan, in order.
#[cfg(test)]
fn state_sequence() -> Vec<BenchState> {
    let mut order = vec![BenchState::Closed];
    let mut state = BenchState::Closed;
    loop {
        match state.after() {
            After::Time(next) => {
                order.push(next);
                state = next;
            }
            After::OpenTheDoor => {
                order.push(BenchState::OpenInView);
                state = BenchState::OpenInView;
            }
            After::CloseTheDoor => return order,
        }
    }
}

/// Where a timed state is, `timer` seconds after it began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Settling,
    Timing,
    Done,
}

fn step(timer: f32) -> Step {
    if timer < STATE_SETTLE_SECONDS {
        Step::Settling
    } else if timer < STATE_SETTLE_SECONDS + TIMING_SECONDS {
        Step::Timing
    } else {
        Step::Done
    }
}

// ---------------------------------------------------------------------------------------------
// GPU frames from the render diagnostics
// ---------------------------------------------------------------------------------------------

/// The render pass every camera draws its opaque geometry in.
const OPAQUE_PASS: &str = "main_opaque_pass_3d";

/// One frame's GPU time, from the render diagnostics measurements stored together for it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GpuFrame {
    /// Every top-level pass, over every camera.
    pub total_ms: f64,
    /// The opaque pass, over every camera.
    pub opaque_ms: f64,
    /// The last opaque pass of the frame: the main camera's.
    pub main_opaque_ms: f64,
    /// How many opaque passes the frame had: one per active camera.
    pub opaque_passes: u32,
}

impl GpuFrame {
    /// The opaque passes of every camera but the main one: the portal's and the water
    /// reflection's.
    pub fn offscreen_opaque_ms(&self) -> f64 {
        (self.opaque_ms - self.main_opaque_ms).max(0.0)
    }

    /// Adds one measurement, `path` as the diagnostics store names it.
    fn add(&mut self, path: &str, value: f64) {
        let Some(name) = path
            .strip_prefix("render/")
            .and_then(|rest| rest.strip_suffix("/elapsed_gpu"))
        else {
            return;
        };
        // A nested span (`render/outer/inner/elapsed_gpu`) is inside its parent's time already.
        if name.contains('/') {
            return;
        }
        self.total_ms += value;
        if name == OPAQUE_PASS {
            self.opaque_ms += value;
            self.main_opaque_ms = value;
            self.opaque_passes += 1;
        }
    }
}

/// Groups GPU measurements into frames by the instant they were stored at, oldest first.
///
/// `measurements` is `(stored at, path, value)` in the store's order, which for one path is the
/// order the spans were recorded in: a later camera's opaque pass after an earlier camera's.
pub fn gpu_frames<'a>(
    measurements: impl IntoIterator<Item = (Instant, &'a str, f64)>,
) -> Vec<GpuFrame> {
    let mut frames: BTreeMap<Instant, GpuFrame> = BTreeMap::new();
    for (time, path, value) in measurements {
        if path.ends_with("/elapsed_gpu") {
            frames.entry(time).or_default().add(path, value);
        }
    }
    frames.into_values().filter(|f| f.total_ms > 0.0).collect()
}

// ---------------------------------------------------------------------------------------------
// Samples, rows and the CSV
// ---------------------------------------------------------------------------------------------

/// The samples of one door in one state.
#[derive(Debug, Clone, Default)]
pub struct Samples {
    pub frame_ms: Vec<f64>,
    pub cpu_ms: Vec<f64>,
    pub gpu: Vec<GpuFrame>,
    pub portal_active_frames: usize,
    pub water_active_frames: usize,
}

/// Mean and 95th percentile.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stat {
    pub mean: f64,
    pub p95: f64,
}

impl Stat {
    pub fn of(samples: impl IntoIterator<Item = f64>) -> Self {
        let mut sorted: Vec<f64> = samples.into_iter().collect();
        if sorted.is_empty() {
            return Self::default();
        }
        sorted.sort_by(f64::total_cmp);
        Self {
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p95: percentile(&sorted, 0.95),
        }
    }
}

/// One row of the CSV.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchRow {
    pub door: u32,
    pub place: String,
    pub state: BenchState,
    pub frames: usize,
    pub frame: Stat,
    pub cpu: Stat,
    pub gpu_frames: usize,
    pub gpu: Stat,
    pub opaque: Stat,
    pub main_opaque: Stat,
    pub offscreen_opaque: Stat,
    /// The share of timed frames the portal camera was rendering in.
    pub portal_active: f64,
    /// The share of timed frames the water reflection camera was rendering in.
    pub water_active: f64,
}

impl BenchRow {
    pub fn from_samples(door: u32, place: &str, state: BenchState, samples: &Samples) -> Self {
        let frames = samples.frame_ms.len();
        let share = |count: usize| {
            if frames == 0 {
                0.0
            } else {
                count as f64 / frames as f64
            }
        };
        Self {
            door,
            place: place.to_owned(),
            state,
            frames,
            frame: Stat::of(samples.frame_ms.iter().copied()),
            cpu: Stat::of(samples.cpu_ms.iter().copied()),
            gpu_frames: samples.gpu.len(),
            gpu: Stat::of(samples.gpu.iter().map(|f| f.total_ms)),
            opaque: Stat::of(samples.gpu.iter().map(|f| f.opaque_ms)),
            main_opaque: Stat::of(samples.gpu.iter().map(|f| f.main_opaque_ms)),
            offscreen_opaque: Stat::of(samples.gpu.iter().map(GpuFrame::offscreen_opaque_ms)),
            portal_active: share(samples.portal_active_frames),
            water_active: share(samples.water_active_frames),
        }
    }

    pub fn csv(&self) -> String {
        format!(
            "{:08X},{},{},{},{:.3},{:.3},{:.3},{:.3},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.2},{:.2}",
            self.door,
            self.place.replace(',', " "),
            self.state.name(),
            self.frames,
            self.frame.mean,
            self.frame.p95,
            self.cpu.mean,
            self.cpu.p95,
            self.gpu_frames,
            self.gpu.mean,
            self.gpu.p95,
            self.opaque.mean,
            self.opaque.p95,
            self.main_opaque.mean,
            self.main_opaque.p95,
            self.offscreen_opaque.mean,
            self.offscreen_opaque.p95,
            self.portal_active,
            self.water_active,
        )
    }
}

pub fn bench_csv(rows: &[BenchRow]) -> String {
    let mut csv = format!("{CSV_HEADER}\n");
    for row in rows {
        csv.push_str(&row.csv());
        csv.push('\n');
    }
    csv
}

/// One line per state that has rows: the means and p95s averaged over the doors.
pub fn bench_summary(rows: &[BenchRow]) -> Vec<String> {
    BenchState::ALL
        .iter()
        .filter_map(|state| {
            let rows: Vec<_> = rows.iter().filter(|row| row.state == *state).collect();
            if rows.is_empty() {
                return None;
            }
            let average =
                |f: fn(&BenchRow) -> f64| rows.iter().map(|row| f(row)).sum::<f64>() / rows.len() as f64;
            Some(format!(
                "{:<14} {} door(s): gpu {:.2} ms (p95 {:.2}), opaque {:.2} ms (main {:.2}, offscreen {:.2}), cpu {:.2} ms (p95 {:.2}), frame {:.2} ms (p95 {:.2}), portal on {:.0}%",
                state.name(),
                rows.len(),
                average(|r| r.gpu.mean),
                average(|r| r.gpu.p95),
                average(|r| r.opaque.mean),
                average(|r| r.main_opaque.mean),
                average(|r| r.offscreen_opaque.mean),
                average(|r| r.cpu.mean),
                average(|r| r.cpu.p95),
                average(|r| r.frame.mean),
                average(|r| r.frame.p95),
                average(|r| r.portal_active) * 100.0,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

/// Adds the bench. `app::run` adds it for a `--portal-bench` run only, with the doors file it read
/// before the window existed.
pub struct PortalBenchPlugin {
    pub doors: DoorsFile,
    pub output: PathBuf,
}

impl Plugin for PortalBenchPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(PortalBench::new(self.doors.clone(), self.output.clone()))
            .init_resource::<MainWorldFrame>()
            .add_systems(First, start_main_world_frame)
            .add_systems(Last, end_main_world_frame)
            .add_systems(Update, run_portal_bench);
    }
}

/// The main world's frame work, `First` to `Last`.
#[derive(Resource, Default)]
struct MainWorldFrame {
    started: Option<Instant>,
    /// The last whole frame's CPU time, in milliseconds.
    last_ms: Option<f64>,
}

fn start_main_world_frame(mut frame: ResMut<MainWorldFrame>) {
    frame.started = Some(Instant::now());
}

fn end_main_world_frame(mut frame: ResMut<MainWorldFrame>) {
    if let Some(started) = frame.started.take() {
        frame.last_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Move into the door's space.
    Teleport,
    /// Wait for the door to stream in.
    FindDoor,
    /// Wait for the view in front of the closed door to settle.
    Settle,
    /// Hold a state and time it.
    Time(BenchState),
    /// The door has been asked to open; wait until it is fully open.
    Opening,
    /// Open: wait for the destination the portal draws to settle.
    OpenSettle,
    /// Ask the door to close and wait for it.
    Closing {
        asked: bool,
    },
    Done,
}

#[derive(Resource)]
pub struct PortalBench {
    doors: DoorsFile,
    output: PathBuf,
    index: usize,
    phase: Phase,
    timer: f32,
    quiet: u32,
    door: Option<Entity>,
    samples: Samples,
    rows: Vec<BenchRow>,
    log: String,
    last_gpu: Option<Instant>,
    started: Instant,
    written: bool,
}

impl PortalBench {
    fn new(doors: DoorsFile, output: PathBuf) -> Self {
        Self {
            doors,
            output,
            index: 0,
            phase: Phase::Teleport,
            timer: 0.0,
            quiet: 0,
            door: None,
            samples: Samples::default(),
            rows: Vec::new(),
            log: String::new(),
            last_gpu: None,
            started: Instant::now(),
            written: false,
        }
    }

    fn note(&mut self, line: impl AsRef<str>) {
        info!(target: "portal_bench", "{}", line.as_ref());
        let _ = writeln!(self.log, "{}", line.as_ref());
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.timer = 0.0;
        self.quiet = 0;
    }

    fn next_door(&mut self) {
        self.index += 1;
        self.door = None;
        if self.index < self.doors.doors.len() {
            self.enter(Phase::Teleport);
        } else {
            self.enter(Phase::Done);
        }
    }

    fn current(&self) -> Option<&BenchDoor> {
        self.doors.doors.get(self.index)
    }

    fn summary_path(&self) -> PathBuf {
        let mut name = self.output.as_os_str().to_owned();
        name.push(".summary.txt");
        PathBuf::from(name)
    }

    fn write(&mut self) {
        let elapsed = self.started.elapsed().as_secs_f64();
        self.note(format!(
            "portal bench: {} rows over {} doors in {elapsed:.0} s",
            self.rows.len(),
            self.doors.doors.len()
        ));
        for line in bench_summary(&self.rows) {
            self.note(line);
        }
        if let Some(parent) = self.output.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&self.output, bench_csv(&self.rows)) {
            Ok(()) => {
                let line = format!("portal bench: wrote {}", self.output.display());
                self.note(line);
            }
            Err(error) => error!("could not write {}: {error}", self.output.display()),
        }
        let summary = self.summary_path();
        if let Err(error) = std::fs::write(&summary, &self.log) {
            error!("could not write {}: {error}", summary.display());
        }
    }
}

/// The camera's pose for `state` in front of (or behind) the door at `transform`.
fn door_pose(transform: &GlobalTransform, door: &LoadDoor, state: BenchState) -> Transform {
    let position = transform.translation();
    let mut outward = match door.outward {
        Some(outward) => creation_to_bevy(Vec3::from_array(outward)),
        None => *transform.forward(),
    };
    outward.y = 0.0;
    let outward = outward.try_normalize().unwrap_or(Vec3::Z);
    let (side, facing) = state.pose();
    let away = match side {
        Side::Front => outward,
        Side::Back => -outward,
    };
    let eye = position + away * STANDOFF + Vec3::Y * EYE_HEIGHT;
    let target = position + Vec3::Y * EYE_HEIGHT;
    let mut pose = Transform::from_translation(eye).looking_at(target, Vec3::Y);
    if facing == Facing::Away {
        pose.rotate_y(std::f32::consts::PI);
    }
    pose
}

/// The cell the camera stands in, for the settle's residency test (as the demo tour reads it).
fn camera_cell(active: &ActiveCell, origin: &RenderOrigin, position: Vec3) -> CellKey {
    if let Some(cell_id) = active.interior {
        return CellKey::Interior(cell_id);
    }
    let (grid_x, grid_y) = grid_of(
        position.x + origin.0.x as f32 * CELL_SIZE,
        -position.z + origin.0.y as f32 * CELL_SIZE,
    );
    CellKey::Exterior {
        worldspace_id: active.worldspace_id,
        grid_x,
        grid_y,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_portal_bench(
    time: Res<Time>,
    real_time: Res<Time<Real>>,
    mut bench: ResMut<PortalBench>,
    cpu: Res<MainWorldFrame>,
    diagnostics: Option<Res<DiagnosticsStore>>,
    mut active: ResMut<ActiveCell>,
    mut origin: ResMut<RenderOrigin>,
    mut roots: Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    doors: Query<(Entity, &GlobalTransform, &LoadDoor, Option<&DoorState>)>,
    cameras: Query<(&Camera, &RenderTarget)>,
    portal_texture: Option<Res<PortalTexture>>,
    streaming: Option<Res<StreamingWorld>>,
    metrics: Option<Res<StreamingMetrics>>,
    (mut open_door, mut exit): (MessageWriter<OpenDoor>, MessageWriter<AppExit>),
) {
    bench.timer += time.delta_secs();
    // The GPU frames that arrived since last frame: always taken, so a state's timing starts with
    // the frames that finished in it rather than a backlog.
    let gpu = diagnostics.as_deref().map(|store| {
        let since = bench.last_gpu;
        let mut newest = since;
        let mut measurements = Vec::new();
        for diagnostic in store.iter() {
            let path = diagnostic.path().as_str();
            if !path.starts_with("render/") || !path.ends_with("/elapsed_gpu") {
                continue;
            }
            for measurement in diagnostic.measurements() {
                if since.is_some_and(|since| measurement.time <= since) {
                    continue;
                }
                newest =
                    Some(newest.map_or(measurement.time, |n: Instant| n.max(measurement.time)));
                measurements.push((measurement.time, path, measurement.value));
            }
        }
        let frames = gpu_frames(measurements);
        (frames, newest)
    });
    let gpu = match gpu {
        Some((frames, newest)) => {
            bench.last_gpu = newest;
            frames
        }
        None => Vec::new(),
    };
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    let door_row = bench.door.and_then(|entity| doors.get(entity).ok());
    let counts = settle_counts(
        Some(camera_cell(&active, &origin, camera.translation)),
        bench.quiet,
        streaming.as_deref(),
        metrics.as_deref(),
    );
    match bench.phase {
        Phase::Teleport => {
            let Some(door) = bench.current().cloned() else {
                bench.enter(Phase::Done);
                return;
            };
            let Some(target) = door.space() else {
                bench.next_door();
                return;
            };
            camera.translation = switch_space(
                target,
                Vec3::from_array(door.position),
                &mut active,
                &mut origin,
                &mut roots,
            ) + Vec3::Y * EYE_HEIGHT;
            let line = format!(
                "door {}/{} {:08X} ({}): moved into {target:?}",
                bench.index + 1,
                bench.doors.doors.len(),
                door.ref_id.0,
                door.place()
            );
            bench.note(line);
            bench.enter(Phase::FindDoor);
        }
        Phase::FindDoor => {
            let wanted = bench.current().map_or(0, |door| door.ref_id.0);
            if let Some((entity, transform, door, _)) =
                doors.iter().find(|(_, _, door, _)| door.ref_id == wanted)
            {
                bench.door = Some(entity);
                *camera = door_pose(transform, door, BenchState::Closed);
                let line = format!(
                    "door {wanted:08X} -> \"{}\" streamed in after {:.1} s",
                    door.label, bench.timer
                );
                bench.note(line);
                bench.enter(Phase::Settle);
            } else if bench.timer >= DOOR_SPAWN_TIMEOUT_SECONDS {
                let line = format!(
                    "SKIP door {wanted:08X}: not streamed in within {DOOR_SPAWN_TIMEOUT_SECONDS:.0} s"
                );
                bench.note(line);
                bench.next_door();
            }
        }
        Phase::Settle | Phase::OpenSettle => {
            let state = if bench.phase == Phase::Settle {
                BenchState::Closed
            } else {
                BenchState::OpenInView
            };
            let Some((_, transform, door, _)) = door_row else {
                let line = "SKIP: the door unloaded while the view settled";
                bench.note(line);
                bench.next_door();
                return;
            };
            *camera = door_pose(transform, door, state);
            bench.quiet = if counts.is_quiet() {
                bench.quiet.saturating_add(1)
            } else {
                0
            };
            let wanted = if state == BenchState::Closed {
                DOOR_QUIET_FRAMES
            } else {
                SETTLE_QUIET_FRAMES
            };
            if bench.quiet >= wanted || bench.timer >= SETTLE_TIMEOUT_SECONDS {
                let line = if bench.quiet >= wanted {
                    format!("  {} view settled after {:.1} s", state.name(), bench.timer)
                } else {
                    format!(
                        "  {} view not settled after {SETTLE_TIMEOUT_SECONDS:.0} s ({}); timing it anyway",
                        state.name(),
                        counts.describe()
                    )
                };
                bench.note(line);
                bench.samples = Samples::default();
                bench.enter(Phase::Time(state));
            }
        }
        Phase::Time(state) => {
            let Some((entity, transform, door, _)) = door_row else {
                bench.note("SKIP: the door unloaded while it was timed");
                bench.next_door();
                return;
            };
            *camera = door_pose(transform, door, state);
            match step(bench.timer) {
                Step::Settling => {}
                Step::Timing => {
                    let portal_on = portal_texture.as_deref().is_some_and(|texture| {
                        cameras.iter().any(|(camera, target)| {
                            camera.is_active
                                && matches!(target, RenderTarget::Image(image) if image.handle == texture.0)
                        })
                    });
                    let water_on = cameras
                        .iter()
                        .any(|(camera, _)| camera.is_active && camera.order == -1);
                    let samples = &mut bench.samples;
                    samples.frame_ms.push(real_time.delta_secs_f64() * 1000.0);
                    if let Some(ms) = cpu.last_ms {
                        samples.cpu_ms.push(ms);
                    }
                    samples.gpu.extend(gpu);
                    samples.portal_active_frames += usize::from(portal_on);
                    samples.water_active_frames += usize::from(water_on);
                }
                Step::Done => {
                    let (ref_id, place) = bench
                        .current()
                        .map(|door| (door.ref_id.0, door.place().to_owned()))
                        .unwrap_or_default();
                    let row = BenchRow::from_samples(ref_id, &place, state, &bench.samples);
                    bench.samples = Samples::default();
                    let line = format!("  {}", row.csv());
                    bench.note(line);
                    bench.rows.push(row);
                    match state.after() {
                        After::Time(next) => bench.enter(Phase::Time(next)),
                        After::OpenTheDoor => {
                            open_door.write(OpenDoor { door: entity });
                            bench.enter(Phase::Opening);
                        }
                        After::CloseTheDoor => bench.enter(Phase::Closing { asked: false }),
                    }
                }
            }
        }
        Phase::Opening => {
            let Some((_, transform, door, state)) = door_row else {
                bench.note("SKIP: the door unloaded while it opened");
                bench.next_door();
                return;
            };
            *camera = door_pose(transform, door, BenchState::OpenInView);
            match state {
                Some(DoorState::Open { animated }) => {
                    let line = format!(
                        "  door fully open after {:.1} s{}",
                        bench.timer,
                        if *animated {
                            ""
                        } else {
                            " (no clip: opened as a hole)"
                        }
                    );
                    bench.note(line);
                    bench.enter(Phase::OpenSettle);
                }
                _ if bench.timer >= OPEN_TIMEOUT_SECONDS => {
                    let line = format!(
                        "  the door did not open within {OPEN_TIMEOUT_SECONDS:.0} s; its open states are not timed"
                    );
                    bench.note(line);
                    bench.enter(Phase::Closing { asked: false });
                }
                _ => {}
            }
        }
        Phase::Closing { asked } => {
            let Some((entity, _, _, state)) = door_row else {
                bench.next_door();
                return;
            };
            match state.copied() {
                None | Some(DoorState::Closed) => bench.next_door(),
                Some(DoorState::Open { animated: false }) => {
                    bench.note(
                        "  the door has no clip of its own and cannot be closed; it stays open",
                    );
                    bench.next_door();
                }
                Some(DoorState::Open { animated: true }) if !asked => {
                    open_door.write(OpenDoor { door: entity });
                    bench.phase = Phase::Closing { asked: true };
                }
                Some(_) if bench.timer >= CLOSE_TIMEOUT_SECONDS => {
                    bench.note("  the door did not close in time; moving on");
                    bench.next_door();
                }
                Some(_) => {}
            }
        }
        Phase::Done => {
            if !bench.written {
                bench.written = true;
                bench.write();
            }
            exit.write(if bench.rows.is_empty() {
                AppExit::error()
            } else {
                AppExit::Success
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_door_is_timed_closed_then_open_in_view_behind_and_occluded() {
        assert_eq!(state_sequence(), BenchState::ALL.to_vec());
        assert_eq!(BenchState::Closed.after(), After::OpenTheDoor);
        assert_eq!(BenchState::OpenOccluded.after(), After::CloseTheDoor);
        assert_eq!(
            BenchState::ALL.map(BenchState::name),
            ["closed", "open-in-view", "open-behind", "open-occluded"]
        );
    }

    #[test]
    fn each_state_settles_then_times_for_three_seconds() {
        assert_eq!(step(0.0), Step::Settling);
        assert_eq!(step(STATE_SETTLE_SECONDS + 0.01), Step::Timing);
        assert_eq!(
            step(STATE_SETTLE_SECONDS + TIMING_SECONDS - 0.01),
            Step::Timing
        );
        assert_eq!(step(STATE_SETTLE_SECONDS + TIMING_SECONDS), Step::Done);
    }

    #[test]
    fn the_poses_face_the_door_turn_away_and_stand_behind_it() {
        let door = LoadDoor {
            ref_id: 1,
            destination: crate::doors::DoorDestination {
                destination_ref_id: 2,
                interior_cell_id: Some(3),
                worldspace_id: None,
                arrival_position: [0.0; 3],
                arrival_rotation: [0.0; 3],
            },
            label: String::new(),
            auto_load: false,
            // Creation +Y is Bevy -Z.
            outward: Some([0.0, 1.0, 0.0]),
        };
        let transform = GlobalTransform::from_translation(Vec3::ZERO);
        let front = door_pose(&transform, &door, BenchState::OpenInView);
        assert!((front.translation - Vec3::new(0.0, EYE_HEIGHT, -STANDOFF)).length() < 1e-3);
        assert!(front.forward().dot(Vec3::Z) > 0.99, "faces the door");
        let behind = door_pose(&transform, &door, BenchState::OpenBehind);
        assert_eq!(behind.translation, front.translation);
        assert!(behind.forward().dot(Vec3::NEG_Z) > 0.99, "faces away");
        let occluded = door_pose(&transform, &door, BenchState::OpenOccluded);
        assert!((occluded.translation - Vec3::new(0.0, EYE_HEIGHT, STANDOFF)).length() < 1e-3);
        assert!(
            occluded.forward().dot(Vec3::NEG_Z) > 0.99,
            "faces the door from its back"
        );
    }

    #[test]
    fn gpu_frames_group_by_frame_and_split_the_main_camera_from_the_others() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(5);
        let frames = gpu_frames([
            // Frame 0: portal then main camera, plus a nested span and a CPU timing.
            (t0, "render/main_opaque_pass_3d/elapsed_gpu", 2.0),
            (t0, "render/main_opaque_pass_3d/elapsed_gpu", 3.0),
            (t0, "render/early prepass/elapsed_gpu", 1.0),
            (t0, "render/early prepass/inner/elapsed_gpu", 0.5),
            (t0, "render/main_opaque_pass_3d/elapsed_cpu", 9.0),
            // Frame 1: the main camera only.
            (t1, "render/main_opaque_pass_3d/elapsed_gpu", 4.0),
        ]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].total_ms, 6.0);
        assert_eq!(frames[0].opaque_ms, 5.0);
        assert_eq!(frames[0].main_opaque_ms, 3.0);
        assert_eq!(frames[0].offscreen_opaque_ms(), 2.0);
        assert_eq!(frames[0].opaque_passes, 2);
        assert_eq!(frames[1].offscreen_opaque_ms(), 0.0);
    }

    fn samples(frame: &[f64], cpu: &[f64], gpu: &[f64]) -> Samples {
        Samples {
            frame_ms: frame.to_vec(),
            cpu_ms: cpu.to_vec(),
            gpu: gpu
                .iter()
                .map(|&ms| GpuFrame {
                    total_ms: ms,
                    opaque_ms: ms / 2.0,
                    main_opaque_ms: ms / 4.0,
                    opaque_passes: 2,
                })
                .collect(),
            portal_active_frames: frame.len() / 2,
            water_active_frames: 0,
        }
    }

    #[test]
    fn a_row_holds_the_mean_and_p95_of_each_measure() {
        let s = samples(&[16.0, 16.0, 16.0, 20.0], &[2.0, 4.0], &[1.0, 3.0]);
        let row = BenchRow::from_samples(0x0001_6E47, "Markarth", BenchState::OpenInView, &s);
        assert_eq!(row.frames, 4);
        assert_eq!(
            row.frame,
            Stat {
                mean: 17.0,
                p95: 20.0
            }
        );
        assert_eq!(
            row.cpu,
            Stat {
                mean: 3.0,
                p95: 4.0
            }
        );
        assert_eq!(
            row.gpu,
            Stat {
                mean: 2.0,
                p95: 3.0
            }
        );
        assert_eq!(row.offscreen_opaque.mean, 0.5);
        assert_eq!(row.portal_active, 0.5);
        let line = row.csv();
        assert!(line.starts_with("00016E47,Markarth,open-in-view,4,17.000,20.000,3.000,4.000,2,"));
        assert_eq!(
            line.split(',').count(),
            CSV_HEADER.split(',').count(),
            "one value per column"
        );
        let csv = bench_csv(&[row]);
        assert_eq!(csv.lines().next(), Some(CSV_HEADER));
        assert_eq!(csv.lines().count(), 2);
    }

    #[test]
    fn an_empty_state_writes_zeros_rather_than_failing() {
        let row = BenchRow::from_samples(1, "", BenchState::Closed, &Samples::default());
        assert_eq!(row.frames, 0);
        assert_eq!(row.gpu, Stat::default());
        assert_eq!(row.portal_active, 0.0);
    }

    #[test]
    fn the_summary_averages_each_state_over_the_doors() {
        let rows = [
            BenchRow::from_samples(
                1,
                "A",
                BenchState::Closed,
                &samples(&[10.0], &[1.0], &[2.0]),
            ),
            BenchRow::from_samples(
                2,
                "B",
                BenchState::Closed,
                &samples(&[10.0], &[3.0], &[4.0]),
            ),
            BenchRow::from_samples(
                1,
                "A",
                BenchState::OpenInView,
                &samples(&[10.0], &[1.0], &[8.0]),
            ),
        ];
        let summary = bench_summary(&rows);
        assert_eq!(summary.len(), 2, "one line per state with rows");
        assert!(summary[0].starts_with("closed"));
        assert!(
            summary[0].contains("2 door(s): gpu 3.00 ms"),
            "{}",
            summary[0]
        );
        assert!(summary[0].contains("cpu 2.00 ms"), "{}", summary[0]);
        assert!(summary[1].starts_with("open-in-view"));
        assert!(summary[1].contains("gpu 8.00 ms"), "{}", summary[1]);
    }

    #[test]
    fn the_checked_in_doors_file_parses() {
        let text = include_str!("../../../tools/bench/portal_bench_doors.json");
        let file = DoorsFile::from_json(text).unwrap();
        assert!(file.doors.len() >= 8);
        let places: std::collections::BTreeSet<_> =
            file.doors.iter().map(|door| door.place()).collect();
        assert!(places.len() >= 3, "spread over at least three places");
        assert!(file.doors.iter().all(|door| door.space().is_some()));
    }

    #[test]
    fn a_doors_file_with_no_space_is_refused() {
        let text = r#"{"doors": [{"ref_id": "0x00000001", "position": [0, 0, 0]}]}"#;
        assert!(DoorsFile::from_json(text).is_err());
        assert!(DoorsFile::from_json(r#"{"doors": []}"#).is_err());
    }
}
