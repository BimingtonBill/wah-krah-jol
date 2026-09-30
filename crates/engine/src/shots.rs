//! `--shots`: render a list of exact camera poses, one PNG each, then exit.
//!
//! A shots file is a small JSON document (`docs/specs/engine/reference-shots.md`): a frame size,
//! `{ "width", "height" }`, and a list of poses in Creation units - `position`, `yaw` (a heading in
//! degrees clockwise from north), `pitch` (degrees, positive looking down) and `hfov` (degrees).
//! Each pose is rendered to `<out>/<name>.png` at exactly the frame size the file asks for, so the
//! image can be put beside a reference screenshot of the same view.
//!
//! Nothing here is interactive: the run poses the camera, waits for the streamer to finish loading
//! around the pose, takes the screenshot, and exits when the file runs out of shots. The run streams
//! one worldspace, the one its first exterior shot names; a shot in an interior or in another
//! worldspace is skipped, and the log says so.
//!
//! `shots.log`, next to the images, has one line per shot: its name, how many frames it waited to
//! settle, whether it settled or the timeout took it, and the image's path.

use crate::{
    render::RendererMetrics,
    streaming::{RenderOrigin, StreamingMetrics},
    world::components::{CELL_SIZE, StreamingCamera},
};
use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::WindowResolution,
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fmt, fs,
    path::{Path, PathBuf},
};

/// Frames a view has to report nothing pending for, in a row, before it is photographed.
///
/// A single quiet frame is not enough: the counts fall to zero when the last response is committed,
/// and the frames right after that are the ones in which the terrain's textures, the sun's shadow
/// cascades and the renderer's indirect draw buffers catch up with the cells that just arrived.
pub const SETTLE_QUIET_FRAMES: u32 = 10;

/// Seconds after start-up before any shot is taken, as for the acceptance screenshot: the first
/// frames compile pipelines, and a mesh whose pipeline is not ready yet is simply not drawn.
pub const WARM_UP_SECONDS: f32 = 2.0;

/// Seconds a shot's view may take to settle before the shot is taken anyway.
///
/// A pose the streamer cannot finish - an asset that never loads, a cell the database refuses -
/// must not hang a file of forty shots: the shot is taken with whatever is resident and its log
/// line says `timed_out` with what was still pending.
pub const SETTLE_TIMEOUT_SECONDS: f32 = 30.0;

/// Seconds a requested screenshot may take to reach the disk before the shot is given up on.
pub const CAPTURE_TIMEOUT_SECONDS: f32 = 60.0;

// ---------------------------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------------------------

/// A shots file: the frame every image is rendered at, and the poses to render.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ShotsFile {
    /// Window width in pixels, and the width of every PNG.
    pub width: u32,
    /// Window height in pixels, and the height of every PNG.
    pub height: u32,
    pub shots: Vec<Shot>,
}

impl ShotsFile {
    /// Reads and checks a shots file. The failure carries the path, because a run that cannot read
    /// its poses has nothing to fall back on.
    pub fn load(path: &Path) -> Result<Self, ShotsError> {
        let text = fs::read_to_string(path)
            .map_err(|error| ShotsError(format!("could not read {}: {error}", path.display())))?;
        Self::parse(&text).map_err(|error| ShotsError(format!("in {}: {error}", path.display())))
    }

    /// Parses a shots file's text. Fields the engine does not know (a comparison tool's own
    /// annotations) are ignored, as `reference` and `note` are.
    pub fn parse(text: &str) -> Result<Self, ShotsError> {
        let file: Self = serde_json::from_str(text)
            .map_err(|error| ShotsError(format!("not a shots file: {error}")))?;
        file.validate()?;
        Ok(file)
    }

    fn validate(&self) -> Result<(), ShotsError> {
        if self.width == 0 || self.height == 0 {
            let frame = format!("{}x{}", self.width, self.height);
            let message = format!("the frame is {frame}: a screenshot of no pixels is not a shot");
            return Err(ShotsError(message));
        }
        if self.shots.is_empty() {
            return Err(ShotsError("the file has no shots".to_owned()));
        }
        let mut names = HashSet::new();
        for shot in &self.shots {
            shot.validate()?;
            if !names.insert(shot.name.as_str()) {
                let message = format!(
                    "two shots are named \"{}\": the second image would replace the first",
                    shot.name
                );
                return Err(ShotsError(message));
            }
        }
        Ok(())
    }

    /// The window size the file asks for, in physical pixels: the scale factor is pinned to 1, so
    /// the window - and so the PNG - is exactly this many pixels whatever the display is set to.
    pub fn window_resolution(&self) -> WindowResolution {
        WindowResolution::new(self.width, self.height).with_scale_factor_override(1.0)
    }

    /// Width over height: the shape of every frame in the file.
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height as f32
    }

    /// Where the run starts streaming: the worldspace the first exterior shot names (`None` leaves
    /// it to `--worldspace`) and the grid square its camera stands over. `None` when the file has
    /// no exterior shot.
    pub fn start(&self) -> Option<(Option<u32>, (i32, i32))> {
        let shot = self.shots.iter().find(|shot| shot.is_exterior())?;
        Some((shot.worldspace_id, shot.grid()))
    }
}

/// One camera pose, rendered to `<out>/<name>.png`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Shot {
    /// Output file stem: the engine writes `<out>/<name>.png`.
    pub name: String,
    /// The worldspace of an exterior shot, or `None` when the file leaves it to `--worldspace`.
    #[serde(default)]
    pub worldspace_id: Option<u32>,
    /// The interior cell of an interior shot, or `None` for an exterior one.
    #[serde(default)]
    pub interior_cell_id: Option<u32>,
    /// The camera's eye, Creation units, absolute (not relative to a cell or the render origin).
    pub position: [f32; 3],
    /// Skyrim heading in degrees: 0 looks north (Creation `+Y`), 90 looks east (`+X`), clockwise
    /// seen from above.
    pub yaw: f32,
    /// Skyrim player X angle in degrees: positive looks down, negative up.
    pub pitch: f32,
    /// Horizontal field of view in degrees, in this file's aspect.
    pub hfov: f32,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Shot {
    /// Whether this is an exterior pose, which is what the streamer holds cells for.
    pub fn is_exterior(&self) -> bool {
        self.interior_cell_id.is_none()
    }

    /// The exterior grid square the camera stands over.
    pub fn grid(&self) -> (i32, i32) {
        let [x, y, _] = self.position;
        (
            (x / CELL_SIZE).floor() as i32,
            (y / CELL_SIZE).floor() as i32,
        )
    }

    fn validate(&self) -> Result<(), ShotsError> {
        let name = &self.name;
        if name.is_empty() {
            return Err(ShotsError(
                "a shot has no name to name its image".to_owned(),
            ));
        }
        // The name is a file stem inside the output folder, never a path out of it.
        let plain = name
            .chars()
            .all(|character| !matches!(character, '/' | '\\' | ':') && !character.is_control())
            && name != "."
            && name != "..";
        if !plain {
            let message = format!("shot \"{name}\" has a name that is not a plain file name");
            return Err(ShotsError(message));
        }
        let finite = self.position.iter().all(|value| value.is_finite())
            && self.yaw.is_finite()
            && self.pitch.is_finite()
            && self.hfov.is_finite();
        if !finite {
            let message = format!("shot \"{name}\" has a pose that is not a number");
            return Err(ShotsError(message));
        }
        if self.hfov <= 0.0 || self.hfov >= 180.0 {
            let hfov = self.hfov;
            let message = format!(
                "shot \"{name}\" asks for a horizontal field of view of {hfov} degrees, \
                 which is not one between 0 and 180"
            );
            return Err(ShotsError(message));
        }
        Ok(())
    }
}

/// A shots file that could not be read or is not a shots file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShotsError(String);

impl fmt::Display for ShotsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ShotsError {}

/// Where the images and `shots.log` go when `--shots-out` is not given: a folder named after the
/// shots file, next to it (`reference/riverwood_shots.json` -> `reference/riverwood_shots-shots/`).
pub fn default_output_dir(shots_path: &Path) -> PathBuf {
    let stem = shots_path.file_stem().map_or_else(
        || "shots".into(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    let directory = format!("{stem}-shots");
    match shots_path.parent() {
        Some(parent) => parent.join(directory),
        None => PathBuf::from(directory),
    }
}

// ---------------------------------------------------------------------------------------------
// The pose
// ---------------------------------------------------------------------------------------------

/// The camera rotation for a Skyrim pose.
///
/// `yaw` is a Creation heading in degrees, clockwise from north (`+Y`) seen from above, where
/// Creation `+Y` is runtime `-Z`. `pitch` is Skyrim's player X angle, positive looking **down**.
pub fn shot_camera_rotation(yaw_degrees: f32, pitch_degrees: f32) -> Quat {
    // A camera looks down its own -Z, and runtime -Z is Creation north: turning -yaw about up puts
    // that along Creation `(sin yaw, cos yaw, 0)`, and the pitch turns the same forward down.
    let yaw = Quat::from_rotation_y(-yaw_degrees.to_radians());
    let pitch = Quat::from_rotation_x(-pitch_degrees.to_radians());
    yaw * pitch
}

/// The vertical field of view for a horizontal one at `aspect` (width / height), in degrees:
/// `tan(h / 2) / tan(v / 2) = aspect` for a rectilinear projection.
pub fn vertical_fov_degrees(hfov_degrees: f32, aspect: f32) -> f32 {
    let half_horizontal = (hfov_degrees.to_radians() * 0.5).tan();
    (2.0 * (half_horizontal / aspect).atan()).to_degrees()
}

/// Where a shot's camera stands in render space: the pose in Creation units converted to runtime
/// axes, measured from the floating origin, which is how the streamer places every cell.
pub fn shot_camera_translation(position: [f32; 3], origin: IVec2) -> Vec3 {
    let axes = shared::coordinates::creation_to_runtime_vector(position);
    let offset_x = origin.x as f32 * CELL_SIZE;
    let offset_z = -(origin.y as f32 * CELL_SIZE);
    Vec3::from_array(axes) - Vec3::new(offset_x, 0.0, offset_z)
}

/// Why a shot is passed over rather than rendered, or `None` when it can be rendered in a run that
/// streams `worldspace_id`. The streamer holds the exterior cells of one worldspace per run.
pub fn skip_reason(shot: &Shot, worldspace_id: u32) -> Option<String> {
    if !shot.is_exterior() {
        return Some("interior".to_owned());
    }
    match shot.worldspace_id {
        Some(named) if named != worldspace_id => Some(format!(
            "worldspace {named:08X}, not the {worldspace_id:08X} this run streams"
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The settle rule
// ---------------------------------------------------------------------------------------------

/// What the settle rule reads, so that the rule is a pure function of the counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleCounts {
    /// Cells submitted to the database but not yet resident.
    pub loading_cells: usize,
    /// Database requests in flight.
    pub active_requests: usize,
    /// Spawned model scenes whose assets have not finished loading.
    pub pending_asset_instances: usize,
    /// Terrain and water surfaces whose textures have not finished loading.
    pub pending_surface_instances: usize,
    /// Loaded models still waiting for their turn in the arming budget.
    pub arming_queue_depth: usize,
    /// Cells out of range still waiting for their turn in the unload budget: still drawn.
    pub retiring_cells: usize,
    /// The renderer's final path is running and the warm-up has passed.
    pub renderer_ready: bool,
    /// Frames the counts have been quiet for, before this frame.
    pub quiet_frames: u32,
}

impl SettleCounts {
    /// What the streamer and the renderer say is still pending, with the quiet window so far.
    pub fn read(streaming: &StreamingMetrics, renderer_ready: bool, quiet_frames: u32) -> Self {
        Self {
            loading_cells: streaming.loading_cells,
            active_requests: streaming.active_requests,
            pending_asset_instances: streaming.pending_asset_instances,
            pending_surface_instances: streaming.pending_surface_instances,
            arming_queue_depth: streaming.arming_queue_depth,
            retiring_cells: streaming.retiring_cells,
            renderer_ready,
            quiet_frames,
        }
    }

    /// Nothing is pending: every load the current view asked for has landed and is drawn.
    pub fn is_quiet(&self) -> bool {
        self.loading_cells == 0
            && self.active_requests == 0
            && self.pending_asset_instances == 0
            && self.pending_surface_instances == 0
            && self.arming_queue_depth == 0
            && self.retiring_cells == 0
            && self.renderer_ready
    }

    /// The pending work, for the log line of a shot that never settled.
    pub fn describe(&self) -> String {
        format!(
            "loading_cells={} active_requests={} pending_assets={} pending_surfaces={} \
             arming={} retiring={} renderer_ready={}",
            self.loading_cells,
            self.active_requests,
            self.pending_asset_instances,
            self.pending_surface_instances,
            self.arming_queue_depth,
            self.retiring_cells,
            self.renderer_ready
        )
    }
}

/// The settle state one frame on: the frame counted into the quiet window when nothing was
/// pending, the window started again when something was.
pub fn advance_settle(counts: SettleCounts) -> SettleCounts {
    let quiet_frames = if counts.is_quiet() {
        counts.quiet_frames.saturating_add(1)
    } else {
        0
    };
    SettleCounts {
        quiet_frames,
        ..counts
    }
}

/// Whether a view may be photographed: nothing pending for [`SETTLE_QUIET_FRAMES`] frames running.
pub fn shots_settled(counts: &SettleCounts) -> bool {
    counts.is_quiet() && counts.quiet_frames >= SETTLE_QUIET_FRAMES
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

/// Runs a shots file. Added by the app when `--shots` is given, beside `StreamingPlugin`: every
/// shot is a camera pose the streamer loads around, and the settle rule waits on it.
pub struct ShotsPlugin {
    pub run: ShotsRun,
}

impl Plugin for ShotsPlugin {
    fn build(&self, app: &mut App) {
        // In `Update`, so the pose is in the camera's `Transform` before `PostUpdate` reads it
        // (the sky dome follows the camera there, and transforms propagate).
        app.insert_resource(self.run.clone())
            .add_systems(Update, run_shots);
    }
}

/// The state of a `--shots` run: what to render, where it goes, and how far it has got.
#[derive(Resource, Debug, Clone)]
pub struct ShotsRun {
    pub file: ShotsFile,
    pub output_dir: PathBuf,
    /// The worldspace the run streams; shots in another one are skipped.
    pub worldspace_id: u32,
    shot: usize,
    phase: Phase,
    /// Seconds in the current phase; the settle and capture timeouts read it.
    timer: f32,
    /// Frames the current shot has waited for its view to settle, for the log.
    frames: u32,
    /// What was last pending, for the log of a shot that timed out.
    counts: SettleCounts,
    timed_out: bool,
    directory_made: bool,
    window_checked: bool,
    log: String,
    written: bool,
    failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Pose the camera on the shot and start settling.
    Move,
    /// Wait for the view to settle (or time out), then ask for the screenshot.
    Settle,
    /// The screenshot has been asked for; wait for it on disk.
    Capture,
    /// Write the log and exit.
    Done,
}

impl ShotsRun {
    pub fn new(file: ShotsFile, output_dir: PathBuf, worldspace_id: u32) -> Self {
        Self {
            file,
            output_dir,
            worldspace_id,
            shot: 0,
            phase: Phase::Move,
            timer: 0.0,
            frames: 0,
            counts: SettleCounts::default(),
            timed_out: false,
            directory_made: false,
            window_checked: false,
            log: String::new(),
            written: false,
            failed: false,
        }
    }

    pub fn shot_path(&self, shot: &Shot) -> PathBuf {
        self.output_dir.join(format!("{}.png", shot.name))
    }

    pub fn log_path(&self) -> PathBuf {
        self.output_dir.join("shots.log")
    }

    /// One line of `shots.log`: the shot's name, how many frames it waited to settle, whether it
    /// settled or the timeout took it - and with what still pending, when it did - and its image.
    pub fn log_line(&self, shot: &Shot, path: &Path) -> String {
        let outcome = if self.timed_out {
            format!("timed_out ({})", self.counts.describe())
        } else {
            "settled".to_owned()
        };
        format!(
            "{} frames={} {outcome} path={}",
            shot.name,
            self.frames,
            path.display()
        )
    }

    /// Records a line in `shots.log` and in the engine log.
    fn note(&mut self, line: impl AsRef<str>) {
        let line = line.as_ref();
        info!(target: "shots", "{line}");
        self.log.push_str(line);
        self.log.push('\n');
    }

    fn fail(&mut self, reason: impl AsRef<str>) {
        self.failed = true;
        self.note(format!("FAILED: {}", reason.as_ref()));
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.timer = 0.0;
    }

    /// Starts waiting for the shot's view to settle. The quiet window counts from the frame after
    /// the pose, because the streamer's plan may have read the camera before this frame's pose
    /// reached it; counting from the pose frame would let a view that has not asked for a single
    /// cell yet read as quiet.
    fn start_settle(&mut self) {
        self.frames = 0;
        self.counts = SettleCounts::default();
        self.timed_out = false;
        self.enter(Phase::Settle);
    }

    /// Moves on to the next shot in the file, or to the end when this was the last one.
    fn advance(&mut self) {
        self.shot += 1;
        if self.shot < self.file.shots.len() {
            self.enter(Phase::Move);
        } else {
            self.enter(Phase::Done);
        }
    }

    fn current_shot(&self) -> Option<Shot> {
        self.file.shots.get(self.shot).cloned()
    }
}

#[allow(clippy::too_many_arguments)]
fn run_shots(
    mut commands: Commands,
    time: Res<Time>,
    mut run: ResMut<ShotsRun>,
    origin: Res<RenderOrigin>,
    mut camera: Query<(&mut Transform, &mut Projection), With<StreamingCamera>>,
    streaming: Res<StreamingMetrics>,
    renderer: Res<RendererMetrics>,
    windows: Query<&Window>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok((mut transform, mut projection)) = camera.single_mut() else {
        return;
    };
    if run.phase == Phase::Done {
        if !run.written {
            run.written = true;
            write_log(&mut run);
        }
        exit.write(if run.failed {
            AppExit::error()
        } else {
            AppExit::Success
        });
        return;
    }
    let Some(shot) = run.current_shot() else {
        run.fail("there is no shot to render");
        run.enter(Phase::Done);
        return;
    };
    match run.phase {
        Phase::Move => {
            if let Some(reason) = skip_reason(&shot, run.worldspace_id) {
                run.note(format!("{} skipped: {reason}", shot.name));
                run.advance();
                return;
            }
            if !run.directory_made {
                run.directory_made = true;
                if let Err(error) = fs::create_dir_all(&run.output_dir) {
                    let message = format!(
                        "could not make the output directory {}: {error}",
                        run.output_dir.display()
                    );
                    run.fail(message);
                    run.enter(Phase::Done);
                    return;
                }
            }
            if !run.window_checked {
                run.window_checked = true;
                warn_window_size(&run, &windows);
            }
            place_camera(
                &shot,
                run.file.aspect(),
                origin.0,
                &mut transform,
                &mut projection,
            );
            run.start_settle();
        }
        Phase::Settle => {
            // Re-assert the pose every frame: a render-origin rebase moves the camera in render
            // space, and the Creation position is what has to be photographed.
            place_camera(
                &shot,
                run.file.aspect(),
                origin.0,
                &mut transform,
                &mut projection,
            );
            run.timer += time.delta_secs();
            run.frames = run.frames.saturating_add(1);
            let renderer_ready =
                renderer.final_path_active() && time.elapsed_secs() >= WARM_UP_SECONDS;
            let counts = SettleCounts::read(&streaming, renderer_ready, run.counts.quiet_frames);
            let settled = shots_settled(&counts);
            run.counts = advance_settle(counts);
            if settled || run.timer >= SETTLE_TIMEOUT_SECONDS {
                run.timed_out = !settled;
                if !settled {
                    warn!(
                        target: "shots",
                        "shot \"{}\" did not settle within {SETTLE_TIMEOUT_SECONDS:.0} s ({}); \
                         shooting anyway",
                        shot.name,
                        run.counts.describe()
                    );
                }
                let path = run.shot_path(&shot);
                discard_previous_shot(&path);
                commands
                    .spawn(Screenshot::primary_window())
                    .observe(save_to_disk(path));
                run.enter(Phase::Capture);
            }
        }
        Phase::Capture => {
            run.timer += time.delta_secs();
            let path = run.shot_path(&shot);
            // `save_to_disk` writes the image from its observer, so the image is complete the
            // moment the file exists.
            if path.is_file() {
                let line = run.log_line(&shot, &path);
                run.note(line);
                run.advance();
            } else if run.timer >= CAPTURE_TIMEOUT_SECONDS {
                let message = format!(
                    "shot \"{}\": no screenshot reached {} within {CAPTURE_TIMEOUT_SECONDS:.0} s",
                    shot.name,
                    path.display()
                );
                run.fail(message);
                run.advance();
            }
        }
        Phase::Done => {}
    }
}

/// Poses the camera on the shot: its position in Creation coordinates, its heading and pitch, and
/// the field of view the shot's horizontal angle is at this frame's aspect.
fn place_camera(
    shot: &Shot,
    aspect: f32,
    origin: IVec2,
    transform: &mut Transform,
    projection: &mut Projection,
) {
    transform.translation = shot_camera_translation(shot.position, origin);
    transform.rotation = shot_camera_rotation(shot.yaw, shot.pitch);
    if let Projection::Perspective(perspective) = projection {
        perspective.fov = vertical_fov_degrees(shot.hfov, aspect).to_radians();
        perspective.aspect_ratio = aspect;
    }
}

/// Says once whether the window is the size the file's images are supposed to be.
fn warn_window_size(run: &ShotsRun, windows: &Query<&Window>) {
    let Some(window) = windows.iter().next() else {
        return;
    };
    let width = window.resolution.physical_width();
    let height = window.resolution.physical_height();
    if (width, height) == (run.file.width, run.file.height) {
        return;
    }
    warn!(
        target: "shots",
        "the window is {width}x{height}, not the {}x{} the file asks for: the image will not match \
         the reference's frame",
        run.file.width,
        run.file.height
    );
}

/// Removes a PNG an earlier run left at this shot's path, so that the file appearing is proof that
/// this screenshot reached the disk.
fn discard_previous_shot(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!(
            target: "shots",
            "could not remove the previous {}: {error}; the shot may wait for that file instead \
             of this screenshot",
            path.display()
        ),
    }
}

/// Writes `shots.log`: the record of a run nobody watched.
fn write_log(run: &mut ShotsRun) {
    let path = run.log_path();
    let written = fs::create_dir_all(&run.output_dir).and_then(|()| fs::write(&path, &run.log));
    if let Err(error) = written {
        error!(target: "shots", "could not write {}: {error}", path.display());
        run.failed = true;
    } else {
        info!(target: "shots", "shots log written to {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shots file with fields this engine ignores - a long `note` and a comparison tool's own
    /// annotation - and both kinds of shot in one file.
    const REFERENCE_FILE: &str = r#"{
     "width": 1400,
     "height": 1050,
     "shots": [
      {
       "name": "tower-east-0",
       "worldspace_id": 60,
       "interior_cell_id": null,
       "position": [74950, 78431, -5050],
       "yaw": 270.0,
       "pitch": 2.0,
       "hfov": 75.0,
       "reference": "reference/ruined-tower.jpg",
       "note": "Tamriel, the small Dwemer tower east of the ravine",
       "confidence": "matched"
      },
      {
       "name": "y-05f",
       "worldspace_id": null,
       "interior_cell_id": 355355,
       "position": [7900.0, -4450.0, -4500.0],
       "yaw": 270.0,
       "pitch": 80.0,
       "hfov": 75.0
      }
     ]
    }"#;

    fn reference_file() -> ShotsFile {
        ShotsFile::parse(REFERENCE_FILE).expect("the reference shape parses")
    }

    /// A one-shot file with `shot` spliced in as the shot's fields.
    fn one_shot(shot: &str) -> String {
        format!(r#"{{"width": 100, "height": 100, "shots": [{{{shot}}}]}}"#)
    }

    /// Where a camera posed this way looks: a camera faces its own -Z.
    fn forward(rotation: Quat) -> Vec3 {
        rotation * Vec3::NEG_Z
    }

    fn assert_close(actual: Vec3, expected: Vec3) {
        let difference = (actual - expected).abs().max_element();
        assert!(difference < 1.0e-5, "{actual:?} != {expected:?}");
    }

    #[test]
    fn a_yaw_turns_clockwise_from_north() {
        // Creation axes as the runtime sees them: north is -Z, east is +X (shared::coordinates).
        let cases = [
            (0.0, Vec3::new(0.0, 0.0, -1.0)),
            (90.0, Vec3::new(1.0, 0.0, 0.0)),
            (180.0, Vec3::new(0.0, 0.0, 1.0)),
            (270.0, Vec3::new(-1.0, 0.0, 0.0)),
        ];
        for (yaw, expected) in cases {
            assert_close(forward(shot_camera_rotation(yaw, 0.0)), expected);
        }
        // The heading is the converted Creation direction it names, `(sin yaw, cos yaw, 0)`.
        for yaw in [12.0_f32, 137.0, 250.0, 355.0] {
            let radians = yaw.to_radians();
            let axes = [radians.sin(), radians.cos(), 0.0];
            let direction = shared::coordinates::creation_to_runtime_vector(axes);
            let rotation = shot_camera_rotation(yaw, 0.0);
            assert_close(forward(rotation), Vec3::from_array(direction).normalize());
        }
    }

    #[test]
    fn a_positive_pitch_looks_down() {
        for pitch in [5.0_f32, 30.0, 80.0] {
            let radians = pitch.to_radians();
            let expected = Vec3::new(0.0, -radians.sin(), -radians.cos());
            assert_close(forward(shot_camera_rotation(0.0, pitch)), expected);
        }
        assert!(forward(shot_camera_rotation(0.0, -30.0)).y > 0.0);
        // Yaw and pitch together: a heading of 90 (east) pitched 30 down looks east and down.
        let pitched = 30.0_f32.to_radians();
        let expected = Vec3::new(pitched.cos(), -pitched.sin(), 0.0);
        assert_close(forward(shot_camera_rotation(90.0, 30.0)), expected);
    }

    #[test]
    fn the_vertical_field_of_view_follows_the_aspect() {
        assert!((vertical_fov_degrees(75.0, 1.0) - 75.0).abs() < 1.0e-4);
        let vertical = vertical_fov_degrees(75.0, 4.0 / 3.0);
        assert!(
            (vertical - 59.83).abs() < 0.05,
            "75 degrees at 4:3 is {vertical}"
        );
        assert!(vertical_fov_degrees(75.0, 2.0) < vertical);
        assert!(vertical_fov_degrees(75.0, 0.75) > vertical);
        assert!((vertical_fov_degrees(90.0, 2.0) - 53.130_1).abs() < 1.0e-3);
    }

    #[test]
    fn a_pose_lands_where_the_streamer_measures_it() {
        // Creation (x, y, z) is runtime (x, z, -y), and the streamer reads a camera back as
        // `translation + origin * CELL`.
        let position = [74_950.0, 78_431.0, -5_050.0];
        for (x, y) in [(0, 0), (18, 19), (-4, 7)] {
            let translation = shot_camera_translation(position, IVec2::new(x, y));
            let world_x = translation.x + x as f32 * CELL_SIZE;
            let world_y = -translation.z + y as f32 * CELL_SIZE;
            assert!((world_x - position[0]).abs() < 1.0e-2);
            assert!((world_y - position[1]).abs() < 1.0e-2);
            assert!((translation.y - position[2]).abs() < 1.0e-2);
            // The streamer's own reading of that camera is the shot's grid square.
            let center = crate::streaming::streaming_center(translation, IVec2::new(x, y));
            let grid = Shot {
                position,
                ..reference_file().shots[0].clone()
            }
            .grid();
            assert_eq!((center.x, center.y), grid);
        }
    }

    #[test]
    fn the_reference_shape_parses_and_names_where_the_run_starts() {
        let file = reference_file();
        assert_eq!((file.width, file.height), (1400, 1050));
        assert_eq!(file.shots.len(), 2);

        let exterior = &file.shots[0];
        assert_eq!(exterior.worldspace_id, Some(60));
        assert!(exterior.is_exterior());
        assert_eq!(exterior.grid(), (18, 19));
        let expected = "reference/ruined-tower.jpg";
        assert_eq!(exterior.reference.as_deref(), Some(expected));
        assert!(!file.shots[1].is_exterior());

        // The run starts on the first exterior shot, whatever order the kinds come in.
        assert_eq!(file.start(), Some((Some(60), (18, 19))));
        let interiors_only = ShotsFile {
            shots: vec![file.shots[1].clone()],
            ..file.clone()
        };
        assert_eq!(interiors_only.start(), None);

        // A negative position floors into the square below zero.
        let west = Shot {
            position: [-1.0, -4097.0, 0.0],
            ..exterior.clone()
        };
        assert_eq!(west.grid(), (-1, -2));

        // The window is exactly the file's frame, whatever the display's scale factor.
        let resolution = file.window_resolution();
        let physical = (resolution.physical_width(), resolution.physical_height());
        assert_eq!(physical, (1400, 1050));
    }

    #[test]
    fn a_malformed_file_is_an_error_that_says_what_is_wrong() {
        let good = r#""name": "x", "position": [0, 0, 0], "yaw": 0, "pitch": 0, "hfov": 75"#;
        let broken = [
            (String::new(), "not a shots file"),
            ("not json".to_owned(), "not a shots file"),
            ("[]".to_owned(), "not a shots file"),
            (r#"{"width": 100, "height": 100}"#.to_owned(), "shots"),
            (
                r#"{"width": -1, "height": 100, "shots": []}"#.to_owned(),
                "not a shots file",
            ),
            (
                r#"{"width": 0, "height": 10, "shots": []}"#.to_owned(),
                "frame",
            ),
            (
                r#"{"width": 10, "height": 10, "shots": []}"#.to_owned(),
                "no shots",
            ),
            (
                one_shot(r#""name": "x", "position": [0, 0], "yaw": 0, "pitch": 0, "hfov": 75"#),
                "not a shots file",
            ),
            (
                one_shot(r#""name": "x", "position": [0, 0, 0], "pitch": 0, "hfov": 75"#),
                "yaw",
            ),
            (
                one_shot(
                    r#""name": "x", "position": [0, 0, 0], "yaw": "north", "pitch": 0, "hfov": 75"#,
                ),
                "not a shots file",
            ),
            (
                one_shot(
                    r#""name": "x", "position": [0, 0, 0], "yaw": 0, "pitch": 0, "hfov": 180"#,
                ),
                "field of view",
            ),
            (
                one_shot(
                    r#""name": "x", "position": [0, 0, 0], "yaw": 0, "pitch": 0, "hfov": 1e39"#,
                ),
                "not a number",
            ),
            (
                one_shot(r#""name": "", "position": [0, 0, 0], "yaw": 0, "pitch": 0, "hfov": 75"#),
                "no name",
            ),
            (
                one_shot(
                    r#""name": "../escape", "position": [0, 0, 0], "yaw": 0, "pitch": 0, "hfov": 75"#,
                ),
                "plain file name",
            ),
            (
                format!(r#"{{"width": 10, "height": 10, "shots": [{{{good}}}, {{{good}}}]}}"#),
                "two shots are named",
            ),
        ];
        for (text, expected) in broken {
            let error = ShotsFile::parse(&text).expect_err("this is not a shots file");
            let message = error.to_string();
            assert!(
                message.contains(expected),
                "{expected:?} is missing from {message:?}"
            );
        }
        assert!(ShotsFile::parse(&one_shot(good)).is_ok());
    }

    #[test]
    fn a_file_that_cannot_be_read_names_its_path() {
        let path = Path::new("no-such-folder/no-such-shots.json");
        let message = ShotsFile::load(path).expect_err("missing").to_string();
        assert!(message.contains("no-such-shots.json"), "{message}");
    }

    #[test]
    fn the_output_directory_defaults_to_the_file_stem_shots() {
        let cases = [
            (
                "reference/riverwood_shots.json",
                "reference/riverwood_shots-shots",
            ),
            ("shots.json", "shots-shots"),
            ("poses/east/tower_east.json", "poses/east/tower_east-shots"),
        ];
        for (path, expected) in cases {
            assert_eq!(default_output_dir(Path::new(path)), PathBuf::from(expected));
        }
    }

    #[test]
    fn interiors_and_other_worldspaces_are_skipped_with_a_reason() {
        let file = reference_file();
        let exterior = &file.shots[0];
        assert_eq!(skip_reason(exterior, 60), None);
        assert_eq!(skip_reason(exterior, 0x3c), None, "0x3c is 60");
        let other = skip_reason(exterior, 0x0001_EE62).expect("another worldspace is skipped");
        assert!(other.contains("0000003C"), "{other}");
        assert_eq!(skip_reason(&file.shots[1], 60).as_deref(), Some("interior"));
        // A shot that names no worldspace takes the one being streamed.
        let unnamed = Shot {
            worldspace_id: None,
            ..exterior.clone()
        };
        assert_eq!(skip_reason(&unnamed, 0x0001_EE62), None);
    }

    #[test]
    fn the_settle_rule_needs_a_quiet_window_and_every_count_at_zero() {
        let ready = SettleCounts {
            renderer_ready: true,
            ..SettleCounts::default()
        };
        assert!(
            !shots_settled(&ready),
            "a window of no frames is not quiet yet"
        );
        let quiet = SettleCounts {
            quiet_frames: SETTLE_QUIET_FRAMES,
            ..ready
        };
        assert!(shots_settled(&quiet));
        let short = SettleCounts {
            quiet_frames: SETTLE_QUIET_FRAMES - 1,
            ..ready
        };
        assert!(!shots_settled(&short));

        // Each kind of pending work on its own holds the shot back and starts the window again.
        let pending = [
            SettleCounts {
                loading_cells: 1,
                ..ready
            },
            SettleCounts {
                active_requests: 1,
                ..ready
            },
            SettleCounts {
                pending_asset_instances: 3,
                ..ready
            },
            SettleCounts {
                pending_surface_instances: 1,
                ..ready
            },
            SettleCounts {
                arming_queue_depth: 2,
                ..ready
            },
            SettleCounts {
                retiring_cells: 1,
                ..ready
            },
            SettleCounts {
                renderer_ready: false,
                ..ready
            },
        ];
        for counts in pending {
            assert!(!counts.is_quiet(), "{counts:?}");
            let counted = advance_settle(SettleCounts {
                quiet_frames: SETTLE_QUIET_FRAMES,
                ..counts
            });
            assert_eq!(counted.quiet_frames, 0, "the window starts again");
            assert!(!shots_settled(&counted));
        }

        // The window counts up while quiet, and reaches a settled view after ten frames.
        let mut counts = ready;
        for frame in 1..=SETTLE_QUIET_FRAMES {
            counts = advance_settle(counts);
            assert_eq!(counts.quiet_frames, frame);
            assert_eq!(shots_settled(&counts), frame >= SETTLE_QUIET_FRAMES);
        }

        // The streamer's metrics are read field by field.
        let metrics = StreamingMetrics {
            loading_cells: 1,
            active_requests: 2,
            pending_asset_instances: 3,
            pending_surface_instances: 4,
            arming_queue_depth: 5,
            retiring_cells: 6,
            ..StreamingMetrics::default()
        };
        let read = SettleCounts::read(&metrics, true, 7);
        let expected = SettleCounts {
            loading_cells: 1,
            active_requests: 2,
            pending_asset_instances: 3,
            pending_surface_instances: 4,
            arming_queue_depth: 5,
            retiring_cells: 6,
            renderer_ready: true,
            quiet_frames: 7,
        };
        assert_eq!(read, expected);
    }

    #[test]
    fn a_log_line_says_what_the_shot_did() {
        let file = reference_file();
        let shot = file.shots[0].clone();
        let output = PathBuf::from("reference/riverwood_shots-shots");
        let mut run = ShotsRun::new(file, output, 60);
        let path = run.shot_path(&shot);
        let display = path.display();

        run.frames = 42;
        let expected = format!("tower-east-0 frames=42 settled path={display}");
        assert_eq!(run.log_line(&shot, &path), expected);

        run.frames = 1_812;
        run.timed_out = true;
        run.counts = SettleCounts {
            loading_cells: 2,
            pending_asset_instances: 7,
            ..SettleCounts::default()
        };
        let timed_out = run.log_line(&shot, &path);
        assert!(timed_out.starts_with("tower-east-0 frames=1812 timed_out ("));
        assert!(timed_out.contains("loading_cells=2"));
        assert!(timed_out.contains("pending_assets=7"));
        assert!(timed_out.ends_with(&format!("path={display}")));
        assert_eq!(
            timed_out.lines().count(),
            1,
            "one line per shot: {timed_out}"
        );
    }
}
