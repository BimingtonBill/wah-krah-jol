//! Reference shots: `engine.exe --assets <dir> --shots <file.json> [--shots-out <dir>]` renders
//! each camera pose in the file to `<out>/<name>.png` and exits.
//!
//! The file format is the contract in `docs/design/reference-shots.md`. Angles are degrees and
//! positions are Creation-engine units, the coordinates `skyrim_world.db` stores, so a pose can be
//! read off the database and edited by hand. The run renders the same view as a reference
//! screenshot of the real game, so the pose, the field of view and the aspect have to match the
//! reference rather than merely look plausible: the camera sits at `position` exactly (no eye
//! height), yaw uses the same clockwise heading as [`arrival_camera_rotation`], and the vertical
//! field of view is derived from `hfov` at the file's aspect.
//!
//! For each shot the camera moves into the shot's space exactly as a door crossing does, the run
//! waits until streaming has nothing pending **for that view**, and only then is the screenshot
//! requested. The app exits once the last image is on disk, not when it was requested.
//!
//! [`arrival_camera_rotation`]: crate::transition::arrival_camera_rotation

use crate::{
    config::grid_of,
    streaming::{ActiveCell, RenderOrigin, StreamingMetrics, StreamingWorld},
    transition::{SpaceTarget, switch_space},
    world::{
        components::{ExteriorCellGrid, StreamingCamera},
        database::CellKey,
    },
};
use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::{PrimaryWindow, WindowResolution},
};
use serde::Deserialize;
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// The steepest pitch a shot may ask for, in degrees, negative looking up: Skyrim's player X angle
/// stops just short of straight down and straight up.
pub const MIN_PITCH_DEGREES: f32 = -89.0;
pub const MAX_PITCH_DEGREES: f32 = 89.0;

/// The narrowest and widest horizontal field of view a shot may ask for, in degrees.
pub const MIN_HFOV_DEGREES: f32 = 10.0;
pub const MAX_HFOV_DEGREES: f32 = 170.0;

/// How long a view may take to settle before the shot is taken anyway (and logged as a timeout).
pub const SETTLE_TIMEOUT_SECONDS: f32 = 30.0;

/// Frames of a quiet streaming state after which the view counts as settled, on top of the quiet
/// state itself, so lights, shadows and the first drawn frame have caught up.
pub const SETTLE_QUIET_FRAMES: u32 = 10;

/// How long to wait for a requested screenshot to reach the disk.
pub const CAPTURE_TIMEOUT_SECONDS: f32 = 60.0;

/// A shots file: the window size every shot is rendered at, and the shots in order.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ShotsFile {
    /// Window width in pixels, and the width of every PNG.
    pub width: u32,
    /// Window height in pixels, and the height of every PNG.
    pub height: u32,
    pub shots: Vec<Shot>,
}

/// One camera pose, rendered to `<out>/<name>.png`.
///
/// `reference` and `note` are carried for the comparison tools and humans; the engine ignores
/// them, as it ignores any field it does not know.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Shot {
    /// Output file stem: the engine writes `<out>/<name>.png`.
    pub name: String,
    /// The worldspace of an exterior shot, or `None` for an interior one.
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
    /// Horizontal field of view in degrees at this file's aspect.
    pub hfov: f32,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl ShotsFile {
    /// Reads and validates a shots file. An unreadable, malformed or impossible file is an error:
    /// the engine exits non-zero rather than rendering something other than what was asked for.
    pub fn load(path: &Path) -> Result<Self, ShotsError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            ShotsError::new(format!("failed to read {}: {error}", path.display()))
        })?;
        Self::from_json(&text)
            .map_err(|error| ShotsError::new(format!("{}: {error}", path.display())))
    }

    /// Parses and validates a shots file's text.
    pub fn from_json(text: &str) -> Result<Self, ShotsError> {
        let file: Self =
            serde_json::from_str(text).map_err(|error| ShotsError::new(error.to_string()))?;
        file.validate()?;
        Ok(file)
    }

    pub fn validate(&self) -> Result<(), ShotsError> {
        if self.width == 0 || self.height == 0 {
            return Err(ShotsError::new(format!(
                "window size {}x{} is not a size to render",
                self.width, self.height
            )));
        }
        if self.shots.is_empty() {
            return Err(ShotsError::new("the shots file has no shots"));
        }
        for shot in &self.shots {
            shot.validate()?;
        }
        Ok(())
    }

    /// The aspect every shot in this file is rendered at (width / height).
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height as f32
    }
}

impl Shot {
    /// The space this shot is in. A validated file has exactly one of the two ids; if a caller
    /// builds a `Shot` by hand anyway, an interior wins over an exterior, which is the order a door
    /// crossing checks its destination in.
    pub(crate) fn space(&self) -> Option<SpaceTarget> {
        match (self.interior_cell_id, self.worldspace_id) {
            (Some(cell_id), _) => Some(SpaceTarget::Interior(cell_id)),
            (None, Some(worldspace_id)) => Some(SpaceTarget::Exterior(worldspace_id)),
            (None, None) => None,
        }
    }

    /// The cell the shot's camera stands in, which streaming has to have resident before the view
    /// can be photographed.
    pub fn space_key(&self) -> Option<CellKey> {
        match self.space()? {
            SpaceTarget::Interior(cell_id) => Some(CellKey::Interior(cell_id)),
            SpaceTarget::Exterior(worldspace_id) => {
                let (grid_x, grid_y) = grid_of(self.position[0], self.position[1]);
                Some(CellKey::Exterior {
                    worldspace_id,
                    grid_x,
                    grid_y,
                })
            }
        }
    }

    pub fn validate(&self) -> Result<(), ShotsError> {
        let label = if self.name.is_empty() {
            "a shot with no name".to_owned()
        } else {
            format!("shot \"{}\"", self.name)
        };
        let invalid = |reason: String| Err(ShotsError::new(format!("{label}: {reason}")));
        if self.name.is_empty()
            || self.name.contains(['/', '\\'])
            || self.name == "."
            || self.name == ".."
        {
            return invalid(
                "the name is the output file stem, so it cannot be empty or a path".into(),
            );
        }
        match (self.worldspace_id, self.interior_cell_id) {
            (Some(_), Some(_)) => {
                return invalid("set worldspace_id or interior_cell_id, not both".into());
            }
            (None, None) => {
                return invalid("set worldspace_id (an exterior) or interior_cell_id".into());
            }
            _ => {}
        }
        if !self.position.iter().all(|value| value.is_finite()) {
            return invalid(format!("position {:?} is not finite", self.position));
        }
        if !self.yaw.is_finite() {
            return invalid(format!("yaw {} is not a finite number", self.yaw));
        }
        if !self.pitch.is_finite() || !(MIN_PITCH_DEGREES..=MAX_PITCH_DEGREES).contains(&self.pitch)
        {
            return invalid(format!(
                "pitch {} is outside {MIN_PITCH_DEGREES}..{MAX_PITCH_DEGREES} degrees",
                self.pitch
            ));
        }
        if !self.hfov.is_finite() || !(MIN_HFOV_DEGREES..=MAX_HFOV_DEGREES).contains(&self.hfov) {
            return invalid(format!(
                "hfov {} is outside {MIN_HFOV_DEGREES}..{MAX_HFOV_DEGREES} degrees",
                self.hfov
            ));
        }
        Ok(())
    }
}

/// Why a shots file could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShotsError {
    message: String,
}

impl ShotsError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ShotsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ShotsError {}

/// Where the images and `shots.log` go when `--shots-out` is not given: a folder named after the
/// shots file, next to it (`reference/tamriel.json` -> `reference/tamriel/`). A file with no
/// extension would name a folder the same as itself, so it gets one instead
/// (`reference/uesp` -> `reference/uesp.shots/`).
pub fn default_output_dir(shots_path: &Path) -> PathBuf {
    match shots_path.extension() {
        Some(_) => shots_path.with_extension(""),
        None => shots_path.with_extension("shots"),
    }
}

/// The camera rotation for a Skyrim pose.
///
/// `yaw` is a Creation heading in degrees, measured clockwise from north (`+Y`) seen from above -
/// the same convention as [`arrival_camera_rotation`] and a player's `GetAngle Z`. `pitch` is
/// Skyrim's player X angle, positive looking **down**. At yaw 0 / pitch 0 the camera looks along
/// Creation `+Y`; at yaw 90 it looks along `+X`; at pitch 30 its forward has a Creation `Z`
/// component of `-sin 30`.
///
/// [`arrival_camera_rotation`]: crate::transition::arrival_camera_rotation
pub fn shot_camera_rotation(yaw_degrees: f32, pitch_degrees: f32) -> Quat {
    // A camera looks down its own -Z: turning -yaw about up puts that along Creation
    // `(sin yaw, cos yaw, 0)`, and the pitch turns the same forward down by `pitch`.
    Quat::from_rotation_y(-yaw_degrees.to_radians())
        * Quat::from_rotation_x(-pitch_degrees.to_radians())
}

/// The vertical field of view for a horizontal one at `aspect` (width / height), in degrees.
pub fn vertical_fov_degrees(hfov_degrees: f32, aspect: f32) -> f32 {
    let half_horizontal = (hfov_degrees.to_radians() * 0.5).tan();
    (2.0 * (half_horizontal / aspect).atan()).to_degrees()
}

/// What the settle rule reads, so the rule is a pure function of the counts rather than of the
/// world.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleCounts {
    /// Whether the cell the shot's camera stands in is streamed in and spawned.
    pub space_resident: bool,
    /// Cells of any space submitted to the database but not yet resident.
    pub loading_cells: usize,
    /// Database requests in flight.
    pub active_requests: usize,
    /// Spawned model scenes whose assets have not finished loading.
    pub pending_asset_instances: usize,
    /// Terrain and water surfaces whose textures have not finished loading.
    pub pending_surface_instances: usize,
    /// Frames the counts have been quiet for, before this frame.
    pub quiet_frames: u32,
}

impl SettleCounts {
    /// Nothing is pending: every load the current view asked for has landed.
    pub fn is_quiet(&self) -> bool {
        self.space_resident
            && self.loading_cells == 0
            && self.active_requests == 0
            && self.pending_asset_instances == 0
            && self.pending_surface_instances == 0
    }

    /// The pending work, for the log line of a shot that never settled.
    pub fn describe(&self) -> String {
        format!(
            "cell_resident={} loading_cells={} active_requests={} pending_assets={} pending_surfaces={}",
            self.space_resident,
            self.loading_cells,
            self.active_requests,
            self.pending_asset_instances,
            self.pending_surface_instances
        )
    }
}

/// Whether a view may be photographed: nothing pending, and nothing pending for a few frames
/// after that, so the first drawn frames, lights and shadows have caught up.
pub fn shots_settled(counts: &SettleCounts) -> bool {
    counts.is_quiet() && counts.quiet_frames >= SETTLE_QUIET_FRAMES
}

/// One line of `shots.log`: the shot's name, how long its view took to settle, whether that settle
/// timed out, and how many meshes were resident when the screenshot was requested.
pub fn log_line(
    name: &str,
    settle_seconds: f32,
    timed_out: bool,
    resident_meshes: usize,
) -> String {
    format!(
        "{name} settle={settle_seconds:.2}s resident_meshes={resident_meshes} timed_out={}",
        if timed_out { "yes" } else { "no" }
    )
}

/// Runs the shots sequence. Added by the app when `--shots` is given; `StreamingPlugin` has to be
/// present, because a shot moves the camera through [`switch_space`] and waits on streaming.
pub struct ShotsPlugin {
    pub run: ShotsRun,
}

impl Plugin for ShotsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.run.clone()).add_systems(
            Update,
            // Before the transition set, so the streaming plan (which runs after it) sees the
            // shot's `ActiveCell` in the frame the camera moves.
            run_shots.before(crate::transition::DoorTransition),
        );
    }
}

/// The state of a `--shots` run: what to render, where it goes, and how far it has got.
#[derive(Resource, Debug, Clone)]
pub struct ShotsRun {
    pub file: ShotsFile,
    pub output_dir: PathBuf,
    shot: usize,
    phase: Phase,
    /// Seconds in the current phase; the settle and capture timeouts read it.
    timer: f32,
    /// Seconds the current shot's view took to settle, for the log.
    settle_seconds: f32,
    quiet_frames: u32,
    resident_meshes: usize,
    timed_out: bool,
    log: String,
    written: bool,
    failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Move the camera into the shot's space and pose it, then settle.
    Move,
    /// Wait for the view to settle (or time out).
    Settle,
    /// The screenshot has been requested; wait for it on disk.
    Capture,
    /// Write the log and exit.
    Done,
}

impl ShotsRun {
    pub fn new(file: ShotsFile, output_dir: PathBuf) -> Self {
        Self {
            file,
            output_dir,
            shot: 0,
            phase: Phase::Move,
            timer: 0.0,
            settle_seconds: 0.0,
            quiet_frames: 0,
            resident_meshes: 0,
            timed_out: false,
            log: String::new(),
            written: false,
            failed: false,
        }
    }

    /// The window size the file asks for, in physical pixels: the scale factor is pinned to 1, so
    /// the window (and so the PNG) is exactly this many pixels whatever the display is set to.
    pub fn window_resolution(&self) -> WindowResolution {
        WindowResolution::new(self.file.width, self.file.height).with_scale_factor_override(1.0)
    }

    pub fn aspect(&self) -> f32 {
        self.file.aspect()
    }

    pub fn shot_path(&self, shot: &Shot) -> PathBuf {
        self.output_dir.join(format!("{}.png", shot.name))
    }

    pub fn log_path(&self) -> PathBuf {
        self.output_dir.join("shots.log")
    }

    /// Records a line in `shots.log` and in the engine log.
    fn note(&mut self, line: impl AsRef<str>) {
        let line = line.as_ref();
        info!(target: "shots", "{line}");
        self.log.push_str(line);
        self.log.push('\n');
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.timer = 0.0;
    }

    fn fail(&mut self, reason: impl AsRef<str>) {
        self.failed = true;
        self.note(format!("FAILED: {}", reason.as_ref()));
    }
}

#[allow(clippy::too_many_arguments)]
fn run_shots(
    mut commands: Commands,
    time: Res<Time>,
    mut run: ResMut<ShotsRun>,
    mut active: ResMut<ActiveCell>,
    mut origin: ResMut<RenderOrigin>,
    mut roots: Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    mut camera: Query<(&mut Transform, &mut Projection), With<StreamingCamera>>,
    streaming: Option<Res<StreamingWorld>>,
    metrics: Option<Res<StreamingMetrics>>,
    meshes: Query<(), With<Mesh3d>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok((mut transform, mut projection)) = camera.single_mut() else {
        return;
    };
    let delta = time.delta_secs();
    match run.phase {
        Phase::Move => {
            let Some(shot) = run.file.shots.get(run.shot).cloned() else {
                run.fail("there is no shot to render");
                run.enter(Phase::Done);
                return;
            };
            let Some(target) = shot.space() else {
                run.fail(format!("shot \"{}\" names no space", shot.name));
                run.enter(Phase::Done);
                return;
            };
            if let Some(window) = windows.iter().next()
                && (window.resolution.physical_width() != run.file.width
                    || window.resolution.physical_height() != run.file.height)
            {
                warn!(
                    target: "shots",
                    "the window is {}x{}, not the {}x{} the file asks for: the image will not \
                     match the reference's frame",
                    window.resolution.physical_width(),
                    window.resolution.physical_height(),
                    run.file.width,
                    run.file.height
                );
            }
            place_camera(
                &shot,
                run.aspect(),
                target,
                &mut active,
                &mut origin,
                &mut roots,
                &mut transform,
                &mut projection,
            );
            run.quiet_frames = 0;
            run.timed_out = false;
            run.settle_seconds = 0.0;
            run.enter(Phase::Settle);
        }
        Phase::Settle => {
            let Some(shot) = run.file.shots.get(run.shot).cloned() else {
                run.fail("there is no shot to render");
                run.enter(Phase::Done);
                return;
            };
            let Some(target) = shot.space() else {
                run.fail(format!("shot \"{}\" names no space", shot.name));
                run.enter(Phase::Done);
                return;
            };
            // Re-assert the pose every frame: a render-origin rebase moves the camera in render
            // space, and the Creation position is what has to be photographed.
            place_camera(
                &shot,
                run.aspect(),
                target,
                &mut active,
                &mut origin,
                &mut roots,
                &mut transform,
                &mut projection,
            );
            run.timer += delta;
            let counts = settle_counts(
                &shot,
                run.quiet_frames,
                streaming.as_deref(),
                metrics.as_deref(),
            );
            let settled = shots_settled(&counts);
            run.quiet_frames = if counts.is_quiet() {
                run.quiet_frames.saturating_add(1)
            } else {
                0
            };
            if settled || run.timer >= SETTLE_TIMEOUT_SECONDS {
                run.timed_out = !settled;
                run.settle_seconds = run.timer;
                run.resident_meshes = meshes.iter().count();
                if !settled {
                    run.note(format!(
                        "shot \"{}\" did not settle within {SETTLE_TIMEOUT_SECONDS:.0} s ({}); \
                         shooting anyway",
                        shot.name,
                        counts.describe()
                    ));
                }
                let path = run.shot_path(&shot);
                run.note(format!("screenshot {}", path.display()));
                // A PNG left by an earlier run must not pass for this shot's: with it out of the
                // way, the file appearing is proof that this screenshot reached the disk, which is
                // what the capture wait (and the exit after the last shot) relies on.
                if let Err(error) = std::fs::remove_file(&path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(
                        target: "shots",
                        "could not remove the previous {}: {error}; the shot may wait for that \
                         file instead of this screenshot",
                        path.display()
                    );
                }
                commands
                    .spawn(Screenshot::primary_window())
                    .observe(save_to_disk(path));
                run.enter(Phase::Capture);
            }
        }
        Phase::Capture => {
            let Some(shot) = run.file.shots.get(run.shot).cloned() else {
                run.fail("there is no shot to render");
                run.enter(Phase::Done);
                return;
            };
            run.timer += delta;
            let path = run.shot_path(&shot);
            // `save_to_disk` writes the file from its observer, so the image is complete the
            // moment the file exists: waiting for it is waiting for the shot, not for the request.
            let written = path.is_file();
            if written {
                let line = log_line(
                    &shot.name,
                    run.settle_seconds,
                    run.timed_out,
                    run.resident_meshes,
                );
                run.note(line);
                run.shot += 1;
                if run.shot < run.file.shots.len() {
                    run.enter(Phase::Move);
                } else {
                    run.enter(Phase::Done);
                }
            } else if run.timer >= CAPTURE_TIMEOUT_SECONDS {
                let line = log_line(
                    &shot.name,
                    run.settle_seconds,
                    run.timed_out,
                    run.resident_meshes,
                );
                run.note(format!(
                    "{line} FAILED: no screenshot at {} after {CAPTURE_TIMEOUT_SECONDS:.0} s",
                    path.display()
                ));
                run.failed = true;
                run.enter(Phase::Done);
            }
        }
        Phase::Done => {
            if !run.written {
                run.written = true;
                let path = run.log_path();
                if let Err(error) = std::fs::write(&path, &run.log) {
                    error!(target: "shots", "could not write {}: {error}", path.display());
                    run.failed = true;
                } else {
                    info!(target: "shots", "shots log written to {}", path.display());
                }
            }
            exit.write(if run.failed {
                AppExit::error()
            } else {
                AppExit::Success
            });
        }
    }
}

/// Moves the camera into the shot's space (as a door crossing does) and poses it there.
///
/// The camera sits at `position` exactly: unlike a crossing there is no eye-height offset, because
/// a reference pose is a camera, not a player's feet.
#[allow(clippy::too_many_arguments)]
fn place_camera(
    shot: &Shot,
    aspect: f32,
    target: SpaceTarget,
    active: &mut ActiveCell,
    origin: &mut RenderOrigin,
    roots: &mut Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    transform: &mut Transform,
    projection: &mut Projection,
) {
    transform.translation = switch_space(
        target,
        Vec3::from_array(shot.position),
        active,
        origin,
        roots,
    );
    transform.rotation = shot_camera_rotation(shot.yaw, shot.pitch);
    if let Projection::Perspective(perspective) = projection {
        perspective.fov = vertical_fov_degrees(shot.hfov, aspect).to_radians();
        perspective.aspect_ratio = aspect;
    }
}

/// What the streaming code says is still pending for this shot's view.
fn settle_counts(
    shot: &Shot,
    quiet_frames: u32,
    streaming: Option<&StreamingWorld>,
    metrics: Option<&StreamingMetrics>,
) -> SettleCounts {
    SettleCounts {
        space_resident: shot
            .space_key()
            .is_some_and(|key| streaming.is_some_and(|world| world.is_resident(&key))),
        loading_cells: metrics.map_or(0, |metrics| metrics.loading_cells),
        active_requests: metrics.map_or(0, |metrics| metrics.active_requests),
        pending_asset_instances: metrics.map_or(0, |metrics| metrics.pending_asset_instances),
        pending_surface_instances: metrics.map_or(0, |metrics| metrics.pending_surface_instances),
        quiet_frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{transition::arrival_camera_rotation, world::components::CELL_SIZE};
    use bevy::ecs::system::SystemState;
    use shared::coordinates::runtime_to_creation_vector;

    /// The shape of the example in `docs/design/reference-shots.md`, with a portable `reference`
    /// path, plus a field the engine must ignore.
    const DESIGN_EXAMPLE: &str = r#"{
      "width": 1400,
      "height": 1050,
      "shots": [
        {
          "name": "SR-place-Alftand_02",
          "worldspace_id": 60,
          "interior_cell_id": null,
          "position": [77000.0, 77500.0, -5200.0],
          "yaw": 135.0,
          "pitch": 20.0,
          "hfov": 75.0,
          "reference": "references/alftand-02.jpg",
          "note": "free text, ignored by the engine",
          "future_tool_field": {"anything": true}
        }
      ]
    }"#;

    fn interior_example() -> String {
        r#"{"width": 1050, "height": 1400, "shots": [
            {"name": "SR-map-Alftand01", "interior_cell_id": 86723,
             "position": [-947.0, 3958.0, 712.0], "yaw": 170.0, "pitch": -10.0, "hfov": 60.0}
        ]}"#
        .to_owned()
    }

    #[test]
    fn parses_the_design_example_and_ignores_unknown_fields() {
        let file = ShotsFile::from_json(DESIGN_EXAMPLE).unwrap();
        assert_eq!((file.width, file.height), (1400, 1050));
        assert!((file.aspect() - 4.0 / 3.0).abs() < 1.0e-6);
        assert_eq!(file.shots.len(), 1);
        let shot = &file.shots[0];
        assert_eq!(shot.name, "SR-place-Alftand_02");
        assert_eq!(shot.worldspace_id, Some(60));
        assert_eq!(shot.interior_cell_id, None);
        assert_eq!(shot.position, [77000.0, 77500.0, -5200.0]);
        assert_eq!(shot.yaw, 135.0);
        assert_eq!(shot.pitch, 20.0);
        assert_eq!(shot.hfov, 75.0);
        assert_eq!(shot.reference.as_deref(), Some("references/alftand-02.jpg"));
        assert_eq!(shot.space(), Some(SpaceTarget::Exterior(60)));
    }

    #[test]
    fn parses_an_interior_shot_without_the_exterior_field() {
        let file = ShotsFile::from_json(&interior_example()).unwrap();
        let shot = &file.shots[0];
        assert_eq!(shot.worldspace_id, None);
        assert_eq!(shot.interior_cell_id, Some(86723));
        assert_eq!(shot.space(), Some(SpaceTarget::Interior(86723)));
        assert_eq!(shot.space_key(), Some(CellKey::Interior(86723)));
        assert_eq!(shot.reference, None);
    }

    #[test]
    fn a_shot_must_name_exactly_one_space() {
        for (reason, shot) in [
            (
                "both",
                r#"{"name": "both", "worldspace_id": 60, "interior_cell_id": 86723,
                    "position": [0.0, 0.0, 0.0], "yaw": 0.0, "pitch": 0.0, "hfov": 75.0}"#,
            ),
            (
                "neither",
                r#"{"name": "neither", "position": [0.0, 0.0, 0.0],
                    "yaw": 0.0, "pitch": 0.0, "hfov": 75.0}"#,
            ),
        ] {
            let text = format!(r#"{{"width": 1400, "height": 1050, "shots": [{shot}]}}"#);
            let error = ShotsFile::from_json(&text).unwrap_err();
            assert!(
                error.message().contains("worldspace_id"),
                "{reason}: {error}"
            );
        }
    }

    #[test]
    fn a_shot_must_have_a_usable_name() {
        let missing = r#"{"width": 1400, "height": 1050, "shots": [
            {"worldspace_id": 60, "position": [0.0, 0.0, 0.0],
             "yaw": 0.0, "pitch": 0.0, "hfov": 75.0}]}"#;
        let error = ShotsFile::from_json(missing).unwrap_err();
        assert!(error.message().contains("name"), "{error}");

        // A name is a file stem in the output folder, never a path: "../evil" would write outside
        // the folder the caller asked for.
        for name in ["", "..", "sub/shot", "sub\\shot"] {
            let text = format!(
                r#"{{"width": 1400, "height": 1050, "shots": [
                    {{"name": "{name}", "worldspace_id": 60, "position": [0.0, 0.0, 0.0],
                      "yaw": 0.0, "pitch": 0.0, "hfov": 75.0}}]}}"#
            );
            assert!(
                ShotsFile::from_json(&text).is_err(),
                "name {name:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_pitch_outside_the_player_limit_is_rejected() {
        for pitch in ["95.0", "-95.0", "89.5"] {
            let text = format!(
                r#"{{"width": 1400, "height": 1050, "shots": [
                    {{"name": "steep", "worldspace_id": 60, "position": [0.0, 0.0, 0.0],
                      "yaw": 0.0, "pitch": {pitch}, "hfov": 75.0}}]}}"#
            );
            let error = ShotsFile::from_json(&text).unwrap_err();
            assert!(error.message().contains("pitch"), "pitch {pitch}: {error}");
        }
        // The limits themselves are allowed.
        for pitch in ["-89.0", "89.0"] {
            let text = format!(
                r#"{{"width": 1400, "height": 1050, "shots": [
                    {{"name": "steep", "worldspace_id": 60, "position": [0.0, 0.0, 0.0],
                      "yaw": 0.0, "pitch": {pitch}, "hfov": 75.0}}]}}"#
            );
            assert!(ShotsFile::from_json(&text).is_ok(), "pitch {pitch}");
        }
    }

    #[test]
    fn an_hfov_outside_the_render_range_is_rejected() {
        for hfov in ["9.0", "171.0", "0.0", "-75.0"] {
            let text = format!(
                r#"{{"width": 1400, "height": 1050, "shots": [
                    {{"name": "wide", "worldspace_id": 60, "position": [0.0, 0.0, 0.0],
                      "yaw": 0.0, "pitch": 0.0, "hfov": {hfov}}}]}}"#
            );
            let error = ShotsFile::from_json(&text).unwrap_err();
            assert!(error.message().contains("hfov"), "hfov {hfov}: {error}");
        }
    }

    #[test]
    fn a_number_that_is_not_finite_is_rejected() {
        // `1e40` is valid JSON but not a finite f32, and a NaN or an infinity in a pose renders
        // nothing at all.
        let text = r#"{"width": 1400, "height": 1050, "shots": [
            {"name": "overflow", "worldspace_id": 60, "position": [1e40, 0.0, 0.0],
             "yaw": 0.0, "pitch": 0.0, "hfov": 75.0}]}"#;
        let error = ShotsFile::from_json(text).unwrap_err();
        assert!(error.message().contains("finite"), "{error}");
    }

    #[test]
    fn a_file_needs_a_size_and_at_least_one_shot() {
        for text in [
            r#"{"width": 0, "height": 1050, "shots": []}"#,
            r#"{"width": 1400, "height": 1050, "shots": []}"#,
        ] {
            assert!(ShotsFile::from_json(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_default_output_folder_is_the_file_stem_beside_the_file() {
        assert_eq!(
            default_output_dir(Path::new("shots/tamriel.json")),
            PathBuf::from("shots/tamriel")
        );
        assert_eq!(
            default_output_dir(Path::new("shots.json")),
            PathBuf::from("shots")
        );
        assert_eq!(
            default_output_dir(Path::new("shots/poses")),
            PathBuf::from("shots/poses.shots"),
            "a file with no extension cannot name a folder the same as itself"
        );
    }

    /// The camera's forward in Creation coordinates, whatever the runtime basis is.
    fn creation_forward(rotation: Quat) -> Vec3 {
        Vec3::from_array(runtime_to_creation_vector(
            (rotation * Vec3::NEG_Z).to_array(),
        ))
    }

    #[test]
    fn a_shot_looks_along_the_same_heading_as_a_door_arrival() {
        // The yaw half of the pose is exactly the `XTEL` convention: if these ever disagree, every
        // shot is aimed at the wrong place.
        for yaw in [
            0.0_f32, 2.96989, -1.8708, 90.0, 135.0, 170.0, 180.0, 270.0, -45.0,
        ] {
            let shot = shot_camera_rotation(yaw, 0.0);
            let arrival = arrival_camera_rotation([0.0, 0.0, yaw.to_radians()]);
            assert!(
                shot.abs_diff_eq(arrival, 1.0e-5),
                "yaw {yaw}: {shot:?} != {arrival:?}"
            );
        }
    }

    #[test]
    fn yaw_zero_looks_north_and_ninety_looks_east() {
        assert!(
            creation_forward(shot_camera_rotation(0.0, 0.0)).abs_diff_eq(Vec3::Y, 1.0e-5),
            "yaw 0 is Creation +Y (north)"
        );
        assert!(
            creation_forward(shot_camera_rotation(90.0, 0.0)).abs_diff_eq(Vec3::X, 1.0e-5),
            "yaw 90 is Creation +X (east)"
        );
        assert!(
            creation_forward(shot_camera_rotation(180.0, 0.0)).abs_diff_eq(Vec3::NEG_Y, 1.0e-5)
        );
        assert!(
            creation_forward(shot_camera_rotation(270.0, 0.0)).abs_diff_eq(Vec3::NEG_X, 1.0e-5)
        );
    }

    #[test]
    fn positive_pitch_looks_down() {
        let forward = creation_forward(shot_camera_rotation(0.0, 30.0));
        assert!(
            (forward.z + 30.0_f32.to_radians().sin()).abs() < 1.0e-5,
            "Creation Z is {}, expected -sin 30",
            forward.z
        );
        assert!(
            (forward.y - 30.0_f32.to_radians().cos()).abs() < 1.0e-5,
            "the heading is unchanged by the pitch"
        );

        let up = creation_forward(shot_camera_rotation(0.0, -45.0));
        assert!(
            (up.z - 45.0_f32.to_radians().sin()).abs() < 1.0e-5,
            "a negative pitch looks up: {up:?}"
        );

        // Pitch turns the same forward up or down whatever the heading.
        for yaw in [0.0_f32, 45.0, 135.0, -170.0] {
            let forward = creation_forward(shot_camera_rotation(yaw, 20.0));
            assert!(
                (forward.z + 20.0_f32.to_radians().sin()).abs() < 1.0e-5,
                "yaw {yaw}: {forward:?}"
            );
        }
    }

    #[test]
    fn the_vertical_fov_of_a_four_three_view_is_narrower_than_the_horizontal_one() {
        // 75 degrees horizontal at 4:3 is 2*atan(tan(37.5 deg)/1.3333) = 59.84 degrees vertical.
        let vertical = vertical_fov_degrees(75.0, 4.0 / 3.0);
        assert!(
            (vertical - 59.84).abs() < 0.01,
            "vertical fov is {vertical}, expected about 59.84"
        );
        // A square view has equal fields of view, and a wide one a taller vertical field.
        assert!((vertical_fov_degrees(75.0, 1.0) - 75.0).abs() < 1.0e-4);
        assert!(vertical_fov_degrees(75.0, 16.0 / 9.0) < 75.0);
    }

    fn quiet_counts() -> SettleCounts {
        SettleCounts {
            space_resident: true,
            quiet_frames: SETTLE_QUIET_FRAMES,
            ..default()
        }
    }

    #[test]
    fn a_view_is_settled_only_when_nothing_is_pending_for_ten_frames() {
        assert!(shots_settled(&quiet_counts()));

        // One frame short of the quiet window.
        assert!(!shots_settled(&SettleCounts {
            quiet_frames: SETTLE_QUIET_FRAMES - 1,
            ..quiet_counts()
        }));

        // Every pending count holds the shot back on its own.
        assert!(
            !shots_settled(&SettleCounts {
                space_resident: false,
                ..quiet_counts()
            }),
            "the shot's own cell must be resident"
        );
        assert!(!shots_settled(&SettleCounts {
            loading_cells: 1,
            ..quiet_counts()
        }));
        assert!(!shots_settled(&SettleCounts {
            active_requests: 1,
            ..quiet_counts()
        }));
        assert!(!shots_settled(&SettleCounts {
            pending_asset_instances: 1,
            ..quiet_counts()
        }));
        assert!(!shots_settled(&SettleCounts {
            pending_surface_instances: 1,
            ..quiet_counts()
        }));

        assert!(
            !SettleCounts::default().is_quiet(),
            "nothing is resident yet"
        );
        assert!(quiet_counts().is_quiet());
    }

    #[test]
    fn a_log_line_carries_the_four_fields_of_the_design() {
        assert_eq!(
            log_line("SR-place-Alftand_02", 2.345, false, 1543),
            "SR-place-Alftand_02 settle=2.35s resident_meshes=1543 timed_out=no"
        );
        assert_eq!(
            log_line("SR-map-Blackreach", 30.0, true, 7),
            "SR-map-Blackreach settle=30.00s resident_meshes=7 timed_out=yes"
        );
    }

    #[test]
    fn a_shots_pose_lands_exactly_where_the_file_asked_and_switches_the_space() {
        // Exercised through the same `switch_space` a door crossing uses, on the shot's side.
        let file = ShotsFile::from_json(&interior_example()).unwrap();
        let shot = &file.shots[0];
        let mut active = ActiveCell {
            worldspace_id: 60,
            interior: None,
        };
        let mut origin = RenderOrigin(IVec2::new(18, 18));
        let mut world = World::new();
        world.spawn((ExteriorCellGrid(IVec2::new(18, 18)), Transform::default()));
        let mut state = SystemState::<
            Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
        >::new(&mut world);
        let mut roots = state.get_mut(&mut world).unwrap();

        let translation = switch_space(
            shot.space().unwrap(),
            Vec3::from_array(shot.position),
            &mut active,
            &mut origin,
            &mut roots,
        );

        assert_eq!(active.interior, Some(86723));
        assert_eq!(
            origin.0,
            IVec2::new(18, 18),
            "an interior is placed at absolute creation coordinates"
        );
        assert_eq!(
            translation,
            crate::streaming::creation_to_bevy(Vec3::from_array(shot.position)),
            "the camera is at the shot's position exactly, with no eye height"
        );
        let placed: Vec<_> = roots
            .iter()
            .map(|(grid, transform)| (grid.0, transform.translation))
            .collect();
        assert_eq!(
            placed,
            vec![(IVec2::new(18, 18), Vec3::ZERO)],
            "an interior shot does not move exterior roots"
        );
    }

    #[test]
    fn an_exterior_shot_takes_the_render_origin_to_its_own_cell() {
        let file = ShotsFile::from_json(DESIGN_EXAMPLE).unwrap();
        let shot = &file.shots[0];
        let mut active = ActiveCell {
            worldspace_id: 0x1EE62,
            interior: None,
        };
        let mut origin = RenderOrigin(IVec2::new(5, 4));
        let mut world = World::new();
        // The shot is at Creation 77000, 77500: grid (18, 18) of Tamriel.
        world.spawn((
            ExteriorCellGrid(IVec2::new(18, 18)),
            Transform::from_xyz(1.0, 0.0, 1.0),
        ));
        let mut state = SystemState::<
            Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
        >::new(&mut world);
        let mut roots = state.get_mut(&mut world).unwrap();

        let translation = switch_space(
            shot.space().unwrap(),
            Vec3::from_array(shot.position),
            &mut active,
            &mut origin,
            &mut roots,
        );

        assert_eq!(
            active,
            ActiveCell {
                worldspace_id: 60,
                interior: None
            }
        );
        assert_eq!(origin.0, IVec2::new(18, 18));
        assert_eq!(
            shot.space_key(),
            Some(CellKey::Exterior {
                worldspace_id: 60,
                grid_x: 18,
                grid_y: 18,
            })
        );
        assert_eq!(
            translation,
            Vec3::new(
                77000.0 - 18.0 * CELL_SIZE,
                -5200.0,
                -(77500.0 - 18.0 * CELL_SIZE)
            ),
            "the camera stands at the shot's position in the new render space"
        );
        let placed: Vec<_> = roots
            .iter()
            .map(|(grid, transform)| (grid.0, transform.translation))
            .collect();
        assert_eq!(
            placed,
            vec![(IVec2::new(18, 18), Vec3::ZERO)],
            "the shot's own cell now sits on the render origin"
        );
    }
}
