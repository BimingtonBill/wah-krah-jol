//! Capturing a camera pose by hand, and starting a run at one.
//!
//! Two halves of one loop. A person flies (or walks) the engine until a view looks like the
//! reference screenshot they are matching, presses `P`, and the pose is written down in the shots
//! file's own notation - position in Creation units, `yaw` clockwise from north, `pitch` positive
//! looking down, `hfov`: the contract in `docs/design/reference-shots.md`, the same one `--shots`
//! renders from. Later, `--start-shot <shots.json> <name>` puts the camera back at that pose to
//! refine it, and `P` again saves what the refining produced.
//!
//! # What `P` writes
//!
//! One JSON line per press, appended to [`MANUAL_POSES_PATH`] (`local/reference/manual-poses.jsonl`,
//! folders created), and the same line logged at `info`. The line is a shot of the contract plus two
//! fields the engine ignores when the line is loaded back: `saved_at` (RFC 3339, UTC) and `shot` (the
//! `--start-shot` name the run was started from, or `null`). A line has no `name`: the name is what a
//! shots file gives a shot, not what a camera has, so a line is pasted into a file's `shots` array
//! under a name of the reader's choosing (which is exactly what [`SavedPose::shot`] does).
//!
//! The pose is read from the camera itself, through the inverse of [`shot_camera_rotation`] rather
//! than a second convention: the heading and the pitch from the direction the camera looks, and the
//! Creation position from the render space the camera stands in - an exterior's is relative to the
//! [`RenderOrigin`], an interior's is absolute, because that is what [`switch_space`] puts it in.
//! `hfov` is the horizontal field of view of the run's own window, at the run's own aspect (the
//! contract's `hfov` is a horizontal field of view **at the file's aspect**), so a line is right for
//! a shots file whose `width`/`height` are the window's.
//!
//! # What `--start-shot` does
//!
//! Poses the camera once, in the first frame, in the shot's own space - exterior or interior, through
//! the same [`switch_space`] a door crossing and a `--shots` shot move the camera with - and then
//! leaves it to the run's own camera, which for this run is the free-flight one. The shot's own field
//! of view comes with it (`--shots`' conversion, [`vertical_fov_degrees`]), so a window with the
//! shot's file aspect shows the shot's frame exactly, and a window with another aspect is warned
//! about: it shows the shot's vertical framing at its own width.
//!
//! The flag is resolved before the window exists ([`start_shot_run`]), so a file that is not there,
//! a name the file does not have and a combination the run cannot carry out all fail with a message
//! instead of a window nobody posed. It is refused beside every other flag that poses the camera -
//! `--shots`, `--demo-tour`, `--start-position` (and so `--demo <name>`) - and beside `--walk`,
//! which hands the camera to the player controller that would take it away again.

use crate::{
    config::EngineConfig,
    shots::{
        MAX_PITCH_DEGREES, MIN_PITCH_DEGREES, Shot, ShotsFile, shot_camera_rotation,
        vertical_fov_degrees,
    },
    streaming::{ActiveCell, RenderOrigin},
    transition::switch_space,
    world::components::{CELL_SIZE, ExteriorCellGrid, StreamingCamera},
};
use bevy::{prelude::*, window::PrimaryWindow};
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Where `P` appends the poses it saves: one JSON line per press, in the shots file's notation
/// (`docs/design/reference-shots.md`), beside the reference material the poses are for.
///
/// Relative to the run's working directory, like every other path the engine is given.
pub const MANUAL_POSES_PATH: &str = "local/reference/manual-poses.jsonl";

/// The key that saves the camera's pose.
const SAVE_POSE_KEY: KeyCode = KeyCode::KeyP;

/// One saved pose, as the line `P` writes it: a shot of the shots-file contract, plus when it was
/// saved and which `--start-shot` run saved it.
///
/// Both space fields are written, the one the camera was not in as `null`, exactly as the design
/// document's example writes them, so a line reads as the shot it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPose {
    /// The worldspace of an exterior pose, or `null` for an interior one.
    pub worldspace_id: Option<u32>,
    /// The interior cell of an interior pose, or `null` for an exterior one.
    pub interior_cell_id: Option<u32>,
    /// The camera's eye, Creation units, absolute.
    pub position: [f32; 3],
    /// Skyrim heading in degrees: 0 looks north, 90 east, clockwise seen from above.
    pub yaw: f32,
    /// Skyrim player X angle in degrees: positive looks down, negative up.
    pub pitch: f32,
    /// Horizontal field of view in degrees, at the aspect the pose was saved from.
    pub hfov: f32,
    /// When `P` was pressed, RFC 3339 in UTC.
    pub saved_at: String,
    /// The shot `--start-shot` started the run at, or `null` when it started any other way.
    pub shot: Option<String>,
}

impl SavedPose {
    /// This pose as a shot of a shots file, under `name`: the line's own space, pose and field of
    /// view, with the two fields the contract has no room for carried into the shot's `note`.
    ///
    /// A saved line is not a shot on its own - it has no `name`, because a name is the file's word
    /// for a shot rather than the camera's - so this is how one becomes one: paste the line into a
    /// file's `shots` array with a `name` added, or call this.
    pub fn shot(&self, name: impl Into<String>) -> Shot {
        Shot {
            name: name.into(),
            worldspace_id: self.worldspace_id,
            interior_cell_id: self.interior_cell_id,
            position: self.position,
            yaw: self.yaw,
            pitch: self.pitch,
            hfov: self.hfov,
            // A saved pose is a camera, not a doorway: nothing was opened for it.
            open_door: None,
            reference: None,
            note: Some(format!("saved by hand at {}", self.saved_at)),
        }
    }

    /// The line this pose is saved as: one line of JSON, in the shots file's field order.
    pub fn line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// The pose a camera rotation looks along: `yaw` and `pitch` in degrees, the inverse of
/// [`shot_camera_rotation`].
///
/// The heading is the direction the camera looks measured clockwise from north, and the pitch is how
/// far down (positive) or up (negative) it looks - the same two angles a shots file writes, read
/// back from the camera's own forward (`-Z` of its rotation) rather than from an Euler convention
/// that would have to agree with it separately. The pitch is clamped to the shots file's own limit
/// ([`MIN_PITCH_DEGREES`]..[`MAX_PITCH_DEGREES`]): the engine's player camera stops at the same one,
/// and a rounding error at the very top of the range must not produce a line a shots file refuses.
pub fn camera_pose_angles(rotation: Quat) -> (f32, f32) {
    let forward = Vec3::from_array(shared::coordinates::runtime_to_creation_vector(
        (rotation * Vec3::NEG_Z).to_array(),
    ));
    let pitch = (-forward.z)
        .clamp(-1.0, 1.0)
        .asin()
        .to_degrees()
        .clamp(MIN_PITCH_DEGREES, MAX_PITCH_DEGREES);
    let yaw = forward.x.atan2(forward.y).to_degrees();
    (yaw, pitch)
}

/// The horizontal field of view in degrees for a vertical one at `aspect` (width / height): the
/// inverse of [`vertical_fov_degrees`], which is how a shots file's `hfov` becomes the camera's.
pub fn horizontal_fov_degrees(vertical_fov_degrees: f32, aspect: f32) -> f32 {
    (2.0 * ((vertical_fov_degrees.to_radians() * 0.5).tan() * aspect).atan()).to_degrees()
}

/// The Creation position a camera stands at, from where it is in render coordinates: the inverse of
/// `streaming::render_position`, which is where [`switch_space`] puts a camera at a Creation
/// position.
///
/// `origin` is the [`RenderOrigin`] the camera's space is rendered against: an exterior's own, and
/// for an interior the zero origin, because an interior is placed at its absolute Creation
/// coordinates and carries no cell grid.
pub fn creation_position(render_position: Vec3, origin: IVec2) -> Vec3 {
    let bevy = Vec3::new(
        render_position.x + origin.x as f32 * CELL_SIZE,
        render_position.y,
        render_position.z - origin.y as f32 * CELL_SIZE,
    );
    Vec3::from_array(shared::coordinates::runtime_to_creation_vector(
        bevy.to_array(),
    ))
}

/// A time as RFC 3339 in UTC, millisecond precision (`2026-09-24T21:33:12.345Z`), the form a saved
/// line's `saved_at` carries.
///
/// Written here rather than taken from a date library, which the engine does not carry: the civil
/// date of a Unix time is [Howard Hinnant's `civil_from_days`](https://howardhinnant.github.io/date_algorithms.html),
/// thirty years of proleptic Gregorian calendar in six lines. A clock that cannot be read at all (a
/// time before 1970, a broken system clock) is written as the Unix epoch rather than as nothing.
pub fn rfc3339(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = elapsed.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        second_of_day / 3_600,
        (second_of_day / 60) % 60,
        second_of_day % 60,
    );
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        elapsed.subsec_millis()
    )
}

/// The civil date of a count of days since 1970-01-01: Hinnant's `civil_from_days`, in tuples rather
/// than in C's out-parameters.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Days since 0000-03-01, the start of a 400-year era, whose first day is day 0 of the era.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    // The year of the era, counting the leap years of the era's own 400-year cycle.
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    // The day of the March-based year, and the March-based month it falls in (0 = March).
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * march_month + 2) / 5 + 1) as u32;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    } as u32;
    // January and February belong to the year after the one the era counted them in.
    (year + i64::from(month <= 2), month, day)
}

/// The `--start-shot` run a configuration asks for: the shot the flag names, or `None` when the run
/// asks for no start shot at all.
///
/// Everything that can be wrong with a request is one of the errors below, and every one of them is
/// raised before the window exists: a file that cannot be read or parsed, a name the file does not
/// have (the message lists the names it does have), a request that names no file or no shot, and a
/// combination with a flag that poses the camera too.
pub fn start_shot_run(config: &EngineConfig) -> Result<Option<StartShotRun>, StartShotError> {
    let portal = &config.portal;
    let Some(request) = portal.start_shot.as_ref() else {
        return Ok(None);
    };
    // The flags this one cannot be combined with, before the request's own contents: they are
    // mistakes about the run rather than about the file, and each names the other flag.
    if let Some(shots) = &portal.shots {
        return Err(StartShotError::new(format!(
            "--start-shot starts a run at one shot of a file; --shots ({}) renders a whole file and \
             poses the camera itself: give one or the other",
            shots.display()
        )));
    }
    if let Some(directory) = &portal.demo_tour {
        return Err(StartShotError::new(format!(
            "--start-shot poses the camera itself; --demo-tour ({}) walks a scripted route that \
             poses it too: give one or the other",
            directory.display()
        )));
    }
    if config.start_position.is_some() {
        return Err(StartShotError::new(
            "--start-shot starts the run at the shot's own position; --start-position (or the \
             --demo it came from) gives one of its own: give one or the other",
        ));
    }
    let files = request.file.as_ref().ok_or_else(|| {
        StartShotError::new("--start-shot needs a shots file: --start-shot <shots.json> <name>")
    })?;
    // Several files, comma-separated, make one list to step through with `N` and `B`: the order is
    // the files' own, each file's shots in its own order.
    let mut playlist = Vec::new();
    for file in files
        .to_string_lossy()
        .split(',')
        .map(str::trim)
        .filter(|file| !file.is_empty())
    {
        let file = PathBuf::from(file);
        let shots =
            ShotsFile::load(&file).map_err(|error| StartShotError::new(error.to_string()))?;
        let aspect = shots.aspect();
        playlist.extend(shots.shots.into_iter().map(|shot| PlaylistShot {
            shot,
            file: file.clone(),
            aspect,
        }));
    }
    let name = request.name.as_deref().ok_or_else(|| {
        StartShotError::new(format!(
            "--start-shot needs the name of a shot in {}: --start-shot <shots.json> <name>",
            files.display()
        ))
    })?;
    let index = playlist
        .iter()
        .position(|entry| entry.shot.name == name)
        .ok_or_else(|| {
            let names: Vec<&str> = playlist
                .iter()
                .map(|entry| entry.shot.name.as_str())
                .collect();
            StartShotError::new(format!(
                "{} has no shot named {name:?}; it has {}",
                files.display(),
                names.join(", ")
            ))
        })?;
    let current = playlist[index].clone();
    Ok(Some(StartShotRun {
        shot: current.shot,
        file: current.file,
        aspect: current.aspect,
        posed: false,
        flying: false,
        playlist,
        index,
    }))
}

/// Why a `--start-shot` request cannot be carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartShotError {
    message: String,
}

impl StartShotError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for StartShotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StartShotError {}

/// The `--start-shot` run: the shot the run starts at, and what posing the camera at it needs.
///
/// Built by [`start_shot_run`] before the window exists, so that a request the run cannot carry out
/// is an error and not a window with a camera nobody posed, and read once by
/// [`pose_at_start_shot`].
#[derive(Resource, Debug, Clone)]
pub struct StartShotRun {
    /// The shot, exactly as its file writes it.
    pub shot: Shot,
    /// The file the shot came from, for the log line.
    pub file: PathBuf,
    /// The aspect the shot's `hfov` is for: the file's `width` / `height`.
    pub aspect: f32,
    /// Whether the camera has been posed. The pose is applied in the first frame and never again:
    /// the run's own camera has the camera from then on.
    posed: bool,
    /// Whether the player's controller has been given the shot's heading and put in free flight.
    /// The controller attaches to the camera a frame or two after the run starts, so this is its own
    /// step ([`fly_from_start_shot`]).
    flying: bool,
    /// Every shot of the file (or files) the run was started from, in order: `N` and `B` step
    /// through them ([`step_through_shots`]).
    pub playlist: Vec<PlaylistShot>,
    /// Which of [`Self::playlist`] the camera is at.
    pub index: usize,
}

/// One shot of a `--start-shot` run's list, with the file it came from and that file's aspect.
#[derive(Debug, Clone)]
pub struct PlaylistShot {
    pub shot: Shot,
    pub file: PathBuf,
    pub aspect: f32,
}

impl StartShotRun {
    /// Moves to the shot `step` places along the list (wrapping), to be posed again from the next
    /// frame: the camera, the controller's heading, the reference picture and the panel follow it.
    pub fn step(&mut self, step: isize) {
        if self.playlist.is_empty() {
            return;
        }
        let count = self.playlist.len() as isize;
        self.index = ((self.index as isize + step).rem_euclid(count)) as usize;
        let entry = self.playlist[self.index].clone();
        self.shot = entry.shot;
        self.file = entry.file;
        self.aspect = entry.aspect;
        self.posed = false;
        self.flying = false;
    }
}

/// Saves the camera's pose on `P`, and starts a run at a saved one (`--start-shot`).
///
/// Added by `app::run` to the runs that are looked at rather than measured - the walks, the demo
/// tours, the shots runs and a `--start-shot` run - which are the runs with a camera a person drives
/// and a world to drive it in.
pub struct PoseCapturePlugin {
    /// The `--start-shot` run this run was started with, resolved by [`start_shot_run`].
    pub start: Option<StartShotRun>,
}

impl Plugin for PoseCapturePlugin {
    fn build(&self, app: &mut App) {
        if let Some(run) = self.start.clone() {
            app.insert_resource(run)
                .init_resource::<ReferenceView>()
                .add_systems(Startup, setup_start_shot_hud)
                .add_systems(
                    Update,
                    (
                        step_through_shots.before(pose_at_start_shot),
                        fly_from_start_shot.before(crate::player::PlayerInput),
                        show_current_shot,
                        reference_picture_keys,
                        fade_pose_saved_notice,
                    ),
                );
        }
        app.add_systems(
            Update,
            (pose_at_start_shot, save_pose_on_key)
                .chain()
                // Before the player's own systems, which re-apply the controller's heading to the
                // camera every frame, and before the transition set, so the streaming plan (which
                // runs after it) sees the space `--start-shot` has put the camera in in the frame
                // it is posed. A shots run orders its own camera the same way.
                .before(crate::player::PlayerInput)
                .before(crate::transition::DoorTransition),
        );
    }
}

/// Poses the camera at the shot `--start-shot` named, once, in the shot's own space.
///
/// The space is entered through [`switch_space`] - the same way a door crossing and a `--shots` shot
/// move the camera, so an exterior start takes the render origin to the shot's own cell and an
/// interior start streams the interior - and the rotation and field of view through the shots
/// module's own conversions, which is what makes the pose a shot's and not a second convention's.
///
/// Applied once: from the second frame the run's camera is the run's own, and a pose re-asserted
/// every frame would undo whatever the person at the keyboard did with it.
fn pose_at_start_shot(
    mut run: Option<ResMut<StartShotRun>>,
    mut active: ResMut<ActiveCell>,
    mut origin: ResMut<RenderOrigin>,
    mut roots: Query<(&ExteriorCellGrid, &mut Transform), Without<StreamingCamera>>,
    mut camera: Query<(&mut Transform, &mut Projection), With<StreamingCamera>>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    let Some(run) = run.as_deref_mut() else {
        return;
    };
    if run.posed {
        return;
    }
    let Ok((mut transform, mut projection)) = camera.single_mut() else {
        return;
    };
    // A shots file validates its shots when it is loaded, so a shot resolved from one always names
    // its space; the guard is here because a hand-built `Shot` cannot promise that.
    let Some(target) = run.shot.space() else {
        return;
    };
    transform.translation = switch_space(
        target,
        Vec3::from_array(run.shot.position),
        &mut active,
        &mut origin,
        &mut roots,
    );
    transform.rotation = shot_camera_rotation(run.shot.yaw, run.shot.pitch);
    if let Projection::Perspective(perspective) = &mut *projection {
        perspective.fov = vertical_fov_degrees(run.shot.hfov, run.aspect).to_radians();
        perspective.aspect_ratio = run.aspect;
    }
    run.posed = true;
    info!(
        target: "pose",
        "start-shot \"{}\" of {}: position {:?} yaw {} pitch {} hfov {}",
        run.shot.name,
        run.file.display(),
        run.shot.position,
        run.shot.yaw,
        run.shot.pitch,
        run.shot.hfov
    );
    // The camera keeps the shot's *vertical* field of view; the width it is shown at is the
    // window's, so a window that is not the shot file's aspect shows a wider (or narrower) frame
    // than the shot's own, which is worth saying once rather than leaving to be puzzled over.
    if let Some(window) = windows.iter().next() {
        let aspect = window.width() / window.height();
        if (aspect - run.aspect).abs() > 0.01 {
            warn!(
                target: "pose",
                "the window is {}x{} ({aspect:.2}:1) and {} is {:.2}:1: the field of view is the \
                 shot's, so this run shows the shot's vertical framing at the window's own aspect",
                window.width(),
                window.height(),
                run.file.display(),
                run.aspect
            );
        }
    }
}

/// `P`: appends the camera's pose to [`MANUAL_POSES_PATH`], in the shots file's own notation, and
/// logs the line.
///
/// Nothing about a run changes when it fails: a folder the engine cannot write is an error in the
/// log, not the end of the run - the person at the keyboard is flying, and a pose is a note rather
/// than a result.
fn save_pose_on_key(
    keyboard: Res<ButtonInput<KeyCode>>,
    config: Res<EngineConfig>,
    active: Res<ActiveCell>,
    origin: Res<RenderOrigin>,
    camera: Query<(&Transform, &Projection), With<StreamingCamera>>,
    run: Option<Res<StartShotRun>>,
    mut notices: Query<(&mut PoseSavedNotice, &mut Text, &mut Node)>,
) {
    if !keyboard.just_pressed(SAVE_POSE_KEY) {
        return;
    }
    let Ok((transform, projection)) = camera.single() else {
        return;
    };
    let Projection::Perspective(perspective) = projection else {
        return;
    };
    let (yaw, pitch) = camera_pose_angles(transform.rotation);
    let saved = SavedPose {
        worldspace_id: active.interior.is_none().then_some(active.worldspace_id),
        interior_cell_id: active.interior,
        // An interior is rendered at its absolute Creation coordinates, an exterior relative to the
        // render origin - the two halves of `switch_space`, read backwards.
        position: creation_position(
            transform.translation,
            if active.interior.is_some() {
                IVec2::ZERO
            } else {
                origin.0
            },
        )
        .to_array(),
        yaw,
        pitch,
        // At the shots file's own aspect in a `--start-shot` run, not the window's: the camera keeps
        // the shot's vertical field of view, Bevy sets its aspect to the window's every frame, and a
        // shots file's `hfov` is for the file's `width` / `height`. Saved at the window's 16:9, a
        // 4:3 file's 75-degree shot came back as 91.3 and would render wider (the user's first six
        // poses, 2026-09-24).
        hfov: horizontal_fov_degrees(
            perspective.fov.to_degrees(),
            run.as_deref()
                .map_or(perspective.aspect_ratio, |run| run.aspect),
        ),
        saved_at: rfc3339(SystemTime::now()),
        // The shot the camera is at now, which `N` and `B` may have moved away from the one the run
        // was started at.
        shot: run.as_deref().map(|run| run.shot.name.clone()).or_else(|| {
            config
                .portal
                .start_shot
                .as_ref()
                .and_then(|start| start.name.clone())
        }),
    };
    let line = match saved.line() {
        Ok(line) => line,
        Err(error) => {
            error!(target: "pose", "could not write the camera's pose as a line: {error}");
            return;
        }
    };
    info!(target: "pose", "{line}");
    let shown = match append_line(Path::new(MANUAL_POSES_PATH), &line) {
        Ok(()) => {
            info!(target: "pose", "pose appended to {MANUAL_POSES_PATH}");
            format!("Pose saved to {MANUAL_POSES_PATH}")
        }
        Err(error) => {
            error!(
                target: "pose",
                "could not append to {MANUAL_POSES_PATH}: {error}"
            );
            format!("Could not save the pose: {error}")
        }
    };
    for (mut notice, mut text, mut node) in &mut notices {
        notice.remaining = SAVED_NOTICE_SECONDS;
        text.0 = shown.clone();
        node.display = Display::Flex;
    }
}

/// How long the "pose saved" line stays on screen after `P`.
const SAVED_NOTICE_SECONDS: f32 = 2.5;
/// The reference picture's width as a share of the window's, at the start and at its limits.
const REFERENCE_WIDTH_START: f32 = 0.25;
const REFERENCE_WIDTH_MIN: f32 = 0.1;
const REFERENCE_WIDTH_MAX: f32 = 0.6;
const REFERENCE_WIDTH_STEP: f32 = 0.05;

/// The controls of a `--start-shot` run, on screen for as long as it runs.
const START_SHOT_HELP: &str = "Mouse: look (click the window first)  |  WASD: move  |  Space / Shift: up / down  |  Ctrl: fast\n\
N / B: next / previous shot  |  P: save this pose  |  R: reference picture on / off  |  [ ]: picture smaller / bigger\n\
F: walk / fly  |  Esc: release the mouse";

/// The picture a shot is compared with: its `reference` field (relative to the repository, the
/// working directory a run is started from) when that file exists, and otherwise
/// `local/reference/uesp/<name>.jpg` - the same rule as `tools/compare_shots.py`.
pub fn reference_picture_path(shot: &Shot, repository: &Path) -> PathBuf {
    if let Some(reference) = &shot.reference {
        let candidate = if Path::new(reference).is_absolute() {
            PathBuf::from(reference)
        } else {
            repository.join(reference)
        };
        if candidate.exists() {
            return candidate;
        }
    }
    repository
        .join("local/reference/uesp")
        .join(format!("{}.jpg", shot.name))
}

/// How the reference picture is shown: its width as a share of the window's (`[` and `]`), and
/// whether it is hidden (`R`). Kept across shots, so stepping to the next shot keeps the choice.
#[derive(Resource)]
struct ReferenceView {
    width: f32,
    hidden: bool,
}

impl Default for ReferenceView {
    fn default() -> Self {
        Self {
            width: REFERENCE_WIDTH_START,
            hidden: false,
        }
    }
}

/// The reference picture in the top-right corner of a `--start-shot` run.
#[derive(Component)]
struct ReferencePicture {
    /// Height over width of the picture shown now, to keep its proportions as it is resized.
    aspect: f32,
    /// Whether the current shot has a picture at all.
    loaded: bool,
}

/// The line saying the current shot has no reference picture.
#[derive(Component)]
struct MissingReferenceNotice;

/// The controls panel's text: the current shot's name, its place in the list, and the keys.
#[derive(Component)]
struct StartShotPanelText;

/// The "pose saved" line, shown for a moment after `P`.
#[derive(Component)]
struct PoseSavedNotice {
    remaining: f32,
}

/// The controls panel, the reference picture, the missing-picture line and the saved-pose line of
/// a `--start-shot` run, spawned once; [`show_current_shot`] fills them for the shot the camera is
/// at.
fn setup_start_shot_hud(
    mut commands: Commands,
    mut player_help: Query<&mut Node, With<crate::player::HelpLine>>,
) {
    // The player's own hint lists the walking keys; this run starts in flight and has its own panel.
    for mut node in &mut player_help {
        node.display = Display::None;
    }
    let text = |value: String, size: f32| {
        (
            Text::new(value),
            TextFont::from_font_size(size),
            TextColor(Color::srgb(0.95, 0.92, 0.8)),
        )
    };
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(12.0),
            bottom: Val::Px(10.0),
            padding: UiRect::all(Val::Px(8.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        children![(StartShotPanelText, text(String::new(), 16.0))],
    ));
    commands.spawn((
        PoseSavedNotice { remaining: 0.0 },
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(12.0),
            top: Val::Px(12.0),
            padding: UiRect::all(Val::Px(6.0)),
            display: Display::None,
            ..default()
        },
        BackgroundColor(Color::srgba(0.05, 0.25, 0.05, 0.8)),
        text(String::new(), 20.0),
    ));
    commands.spawn((
        ReferencePicture {
            aspect: 0.75,
            loaded: false,
        },
        ImageNode::default(),
        Node {
            display: Display::None,
            ..reference_node(REFERENCE_WIDTH_START, 0.75)
        },
    ));
    commands.spawn((
        MissingReferenceNotice,
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(12.0),
            top: Val::Px(12.0),
            padding: UiRect::all(Val::Px(6.0)),
            display: Display::None,
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        text(String::new(), 16.0),
    ));
}

/// Reads a reference picture from disk - it lives in the repository, not in the converted assets
/// the asset server reads.
fn load_reference_picture(path: &Path) -> Result<Image, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("jpg")
        .to_ascii_lowercase();
    Image::from_buffer(
        &bytes,
        bevy::image::ImageType::Extension(&extension),
        bevy::image::CompressedImageFormats::NONE,
        true,
        bevy::image::ImageSampler::linear(),
        bevy::asset::RenderAssetUsages::default(),
    )
    .map_err(|error| error.to_string())
}

/// Fills the panel and the reference picture for the shot the camera is at, whenever that changes
/// (the first frame, and every `N` or `B`). A picture that cannot be read is a line saying so,
/// never the end of the run.
#[allow(clippy::type_complexity)]
fn show_current_shot(
    run: Res<StartShotRun>,
    view: Res<ReferenceView>,
    mut images: ResMut<Assets<Image>>,
    mut shown: Local<Option<usize>>,
    mut panel: Query<&mut Text, (With<StartShotPanelText>, Without<MissingReferenceNotice>)>,
    mut pictures: Query<(&mut ReferencePicture, &mut ImageNode, &mut Node)>,
    mut missing: Query<
        (&mut Text, &mut Node),
        (With<MissingReferenceNotice>, Without<ReferencePicture>),
    >,
) {
    if *shown == Some(run.index) {
        return;
    }
    let Ok(mut panel) = panel.single_mut() else {
        return;
    };
    let Ok((mut picture, mut image, mut node)) = pictures.single_mut() else {
        return;
    };
    let Ok((mut missing_text, mut missing_node)) = missing.single_mut() else {
        return;
    };
    *shown = Some(run.index);
    panel.0 = format!(
        "{}  ({} of {})\n{}",
        run.shot.name,
        run.index + 1,
        run.playlist.len().max(1),
        START_SHOT_HELP
    );
    let path = std::env::current_dir()
        .map(|directory| reference_picture_path(&run.shot, &directory))
        .unwrap_or_else(|_| reference_picture_path(&run.shot, Path::new(".")));
    match load_reference_picture(&path) {
        Ok(loaded) => {
            let aspect = loaded.height() as f32 / loaded.width().max(1) as f32;
            if picture.loaded {
                images.remove(&image.image);
            }
            image.image = images.add(loaded);
            picture.aspect = aspect;
            picture.loaded = true;
            *node = reference_node(view.width, aspect);
            node.display = if view.hidden {
                Display::None
            } else {
                Display::Flex
            };
            missing_node.display = Display::None;
        }
        Err(error) => {
            warn!(
                target: "pose",
                "no reference picture for {}: {} ({error})",
                run.shot.name,
                path.display()
            );
            picture.loaded = false;
            node.display = Display::None;
            missing_text.0 = format!("No reference picture at {}", path.display());
            missing_node.display = Display::Flex;
        }
    }
}

/// `N` steps to the next shot of the list and `B` to the previous one, wrapping at the ends.
fn step_through_shots(keyboard: Res<ButtonInput<KeyCode>>, mut run: ResMut<StartShotRun>) {
    if keyboard.just_pressed(KeyCode::KeyN) {
        run.step(1);
    } else if keyboard.just_pressed(KeyCode::KeyB) {
        run.step(-1);
    }
}

/// The reference picture's box: top-right, `width` of the window's width, its own proportions.
fn reference_node(width: f32, aspect: f32) -> Node {
    Node {
        position_type: PositionType::Absolute,
        right: Val::Px(12.0),
        top: Val::Px(12.0),
        width: Val::Vw(width * 100.0),
        height: Val::Vw(width * aspect * 100.0),
        ..default()
    }
}

/// `R` shows or hides the reference picture, `[` and `]` make it smaller or bigger.
fn reference_picture_keys(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut view: ResMut<ReferenceView>,
    mut pictures: Query<(&ReferencePicture, &mut Node)>,
) {
    let mut changed = false;
    if keyboard.just_pressed(KeyCode::BracketLeft) {
        view.width = (view.width - REFERENCE_WIDTH_STEP).max(REFERENCE_WIDTH_MIN);
        changed = true;
    }
    if keyboard.just_pressed(KeyCode::BracketRight) {
        view.width = (view.width + REFERENCE_WIDTH_STEP).min(REFERENCE_WIDTH_MAX);
        changed = true;
    }
    if keyboard.just_pressed(KeyCode::KeyR) {
        view.hidden = !view.hidden;
        changed = true;
    }
    if !changed {
        return;
    }
    for (picture, mut node) in &mut pictures {
        *node = reference_node(view.width, picture.aspect);
        node.display = if view.hidden || !picture.loaded {
            Display::None
        } else {
            Display::Flex
        };
    }
}

/// Hides the "pose saved" line again once it has been up for [`SAVED_NOTICE_SECONDS`].
fn fade_pose_saved_notice(time: Res<Time>, mut notices: Query<(&mut PoseSavedNotice, &mut Node)>) {
    for (mut notice, mut node) in &mut notices {
        if notice.remaining <= 0.0 {
            continue;
        }
        notice.remaining -= time.delta_secs();
        if notice.remaining <= 0.0 {
            node.display = Display::None;
        }
    }
}

/// Gives the player's controller the shot's heading and puts it in free flight, once, when the
/// controller has attached to the camera. The controller re-applies its own heading to the camera
/// every frame (`crate::player`), so without this the camera would turn back to wherever the
/// controller started; and a `--start-shot` run is for flying to a view, not for walking, so it
/// starts in flight with no gravity (`F` still switches to walking).
fn fly_from_start_shot(
    mut run: ResMut<StartShotRun>,
    mut camera: Query<(&mut Transform, &mut crate::player::Player), With<StreamingCamera>>,
) {
    if !run.posed || run.flying {
        return;
    }
    let Ok((mut transform, mut player)) = camera.single_mut() else {
        return;
    };
    let rotation = shot_camera_rotation(run.shot.yaw, run.shot.pitch);
    let (yaw, pitch, _) = rotation.to_euler(EulerRot::YXZ);
    player.yaw = yaw;
    player.pitch = pitch;
    player.mode = crate::player::PlayerMode::Fly;
    player.velocity = Vec3::ZERO;
    player.grounded = false;
    transform.rotation = player.look_rotation();
    run.flying = true;
}

/// Appends one line to a JSONL file, creating the folders it needs. The file's earlier lines are
/// left where they are: a saved pose is added to what is already saved, never written over it.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?,
        "{line}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{streaming::render_position, transition::SpaceTarget};
    use std::time::Duration;

    /// The poses the round trip is exercised on: an exterior in the third cell east and fourth
    /// north, an interior at the origin, the compass points, both pitch limits and the pitch the
    /// shots file's own example uses.
    fn poses() -> Vec<SavedPose> {
        let saved = |worldspace_id, interior_cell_id, position, yaw, pitch, hfov| SavedPose {
            worldspace_id,
            interior_cell_id,
            position,
            yaw,
            pitch,
            hfov,
            saved_at: "2026-09-24T21:33:12.345Z".to_owned(),
            shot: None,
        };
        vec![
            saved(Some(60), None, [20558.4, -46062.6, -2.1], 161.5, 5.2, 75.0),
            saved(
                Some(60),
                None,
                [77000.0, 77500.0, -5200.0],
                135.0,
                20.0,
                75.0,
            ),
            saved(Some(0x1_EE62), None, [-100.0, 8200.0, 5.0], 0.0, 0.0, 90.0),
            saved(Some(60), None, [1.0, -4096.0, 0.0], -170.0, -45.0, 60.0),
            saved(
                None,
                Some(86723),
                [-947.0, 3958.0, 712.0],
                170.0,
                -10.0,
                60.0,
            ),
            saved(None, Some(0x1_33C9), [0.0, 0.0, 0.0], 270.0, 89.0, 10.0),
            saved(
                None,
                Some(0x1_33C9),
                [12.5, -3.25, 120.0],
                -89.5,
                -89.0,
                170.0,
            ),
        ]
    }

    /// The yaw difference in degrees, with the wrap at 180 taken out: a heading of 200 and one of
    /// -160 are the same heading.
    fn yaw_difference(left: f32, right: f32) -> f32 {
        let difference = (left - right).rem_euclid(360.0);
        difference.min(360.0 - difference)
    }

    #[test]
    fn a_saved_pose_round_trips_through_the_shots_conversion() {
        // The origin is deliberately not the pose's own cell: an exterior pose is stored relative
        // to whatever the render origin is, and has to come back at the same Creation position
        // from any origin.
        let origin = IVec2::new(3, -12);
        for saved in poses() {
            let shot = saved.shot("round-trip");
            // The camera a `--shots` run puts at this shot: `shot_camera_rotation`, and the render
            // position `switch_space` lands the camera on - an exterior's relative to the render
            // origin, an interior's at the interior's absolute Creation coordinates.
            let rotation = shot_camera_rotation(shot.yaw, shot.pitch);
            let interior = shot.interior_cell_id.is_some();
            let render = if interior {
                crate::streaming::creation_to_bevy(Vec3::from_array(shot.position))
            } else {
                render_position(Vec3::from_array(shot.position), origin)
            };

            let (yaw, pitch) = camera_pose_angles(rotation);
            assert!(
                yaw_difference(yaw, shot.yaw) < 0.01,
                "{}: yaw {yaw} is not {}",
                shot.name,
                shot.yaw
            );
            assert!(
                (pitch - shot.pitch).abs() < 0.01,
                "{}: pitch {pitch} is not {}",
                shot.name,
                shot.pitch
            );

            let position = creation_position(render, if interior { IVec2::ZERO } else { origin });
            for axis in 0..3 {
                assert!(
                    (position.to_array()[axis] - shot.position[axis]).abs() < 0.01,
                    "{}: position {position:?} is not {:?}",
                    shot.name,
                    shot.position
                );
            }

            // And the field of view, through the aspect the shots file would render at.
            let aspect = 1400.0 / 1050.0;
            let vertical = vertical_fov_degrees(shot.hfov, aspect);
            let hfov = horizontal_fov_degrees(vertical, aspect);
            assert!(
                (hfov - shot.hfov).abs() < 0.01,
                "{}: hfov {hfov} is not {}",
                shot.name,
                shot.hfov
            );
        }
    }

    /// The same round trip, but with the camera the engine actually builds: a `Transform` whose
    /// rotation came from the shots conversion, read back through the saved line's own angles. The
    /// two halves have to agree with the engine's camera, not only with each other.
    #[test]
    fn a_camera_posed_at_a_shot_saves_the_shot_back() {
        for saved in poses() {
            let rotation = shot_camera_rotation(saved.yaw, saved.pitch);
            let transform = Transform::from_rotation(rotation);
            let (yaw, pitch) = camera_pose_angles(transform.rotation);
            assert!(yaw_difference(yaw, saved.yaw) < 0.01, "yaw {yaw}");
            assert!((pitch - saved.pitch).abs() < 0.01, "pitch {pitch}");
        }
        // A camera looking along Creation +Y is a heading of 0, along +X one of 90, and so on round:
        // the convention a saved line is written in, spelled out once.
        let heading = |yaw: f32| {
            let forward = shot_camera_rotation(yaw, 0.0) * Vec3::NEG_Z;
            Vec3::from_array(shared::coordinates::runtime_to_creation_vector(
                forward.to_array(),
            ))
        };
        assert!(heading(0.0).abs_diff_eq(Vec3::Y, 1.0e-5), "north");
        assert!(heading(90.0).abs_diff_eq(Vec3::X, 1.0e-5), "east");
        assert!(heading(180.0).abs_diff_eq(Vec3::NEG_Y, 1.0e-5), "south");
        assert!(heading(270.0).abs_diff_eq(Vec3::NEG_X, 1.0e-5), "west");
        assert!(
            camera_pose_angles(shot_camera_rotation(0.0, 30.0)).1 > 0.0,
            "a positive pitch looks down"
        );
        assert!(camera_pose_angles(shot_camera_rotation(0.0, -30.0)).1 < 0.0);
    }

    #[test]
    fn a_pose_saved_at_the_pitch_limit_is_a_shot_a_file_can_carry() {
        // The engine's player camera stops at the same 89 degrees a shots file does, so a pose saved
        // at the limit is exactly there - and the rounding of `asin` must not push it over.
        for pitch in [MIN_PITCH_DEGREES, MAX_PITCH_DEGREES] {
            let (_, saved) = camera_pose_angles(shot_camera_rotation(0.0, pitch));
            assert!(saved.abs() <= MAX_PITCH_DEGREES, "pitch {saved}");
            let mut pose = poses().remove(0);
            pose.pitch = saved;
            pose.shot("saved").validate().unwrap();
        }
    }

    /// A line is a shot a shots file can carry: parsed as JSON, given a name, and loaded by the
    /// engine's own shots-file reader, with the pose and the space intact.
    #[test]
    fn a_saved_line_is_a_shot_a_shots_file_can_carry() {
        let saved = SavedPose {
            worldspace_id: None,
            interior_cell_id: Some(0x1_33C9),
            position: [-947.0, 3958.0, 712.0],
            yaw: 170.0,
            pitch: -10.0,
            hfov: 60.0,
            saved_at: "2026-09-24T21:33:12.345Z".to_owned(),
            shot: Some("RW-04-inn-front".to_owned()),
        };
        let line = saved.line().unwrap();
        // The two fields of the line that are not the contract's are ignored by a shots file, and
        // the space the camera was not in is written as `null`, as the design example writes it.
        assert!(line.contains(r#""worldspace_id":null"#), "{line}");
        assert!(line.contains(r#""interior_cell_id":78793"#), "{line}");
        assert!(
            line.contains(r#""saved_at":"2026-09-24T21:33:12.345Z""#),
            "{line}"
        );
        assert!(line.contains(r#""shot":"RW-04-inn-front""#), "{line}");

        let mut value: serde_json::Value = serde_json::from_str(&line).unwrap();
        value["name"] = serde_json::Value::String("manual-01".to_owned());
        let file = ShotsFile::from_json(&format!(
            r#"{{"width": 1600, "height": 900, "shots": [{value}]}}"#
        ))
        .expect("a saved line is a shot");
        let shot = &file.shots[0];
        assert_eq!(shot.interior_cell_id, saved.interior_cell_id);
        assert_eq!(shot.worldspace_id, None);
        assert_eq!(shot.position, saved.position);
        assert_eq!(shot.yaw, saved.yaw);
        assert_eq!(shot.pitch, saved.pitch);
        assert_eq!(shot.hfov, saved.hfov);
        assert_eq!(shot.open_door, None);
        assert_eq!(shot.space(), Some(SpaceTarget::Interior(0x1_33C9)));
        // [`SavedPose::shot`] is the same shot the engine's own reader builds, field for field -
        // `note` aside, which is the file's word about the shot rather than the camera's.
        let mut built = saved.shot("manual-01");
        built.note = None;
        assert_eq!(shot, &built);

        // Reading the line back as the pose it was written from: the engine writes a line and reads
        // it again, field for field.
        let parsed: SavedPose = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, saved);
    }

    #[test]
    fn a_time_is_written_as_rfc_3339_in_utc() {
        let at =
            |seconds: u64, millis: u32| UNIX_EPOCH + Duration::new(seconds, millis * 1_000_000);
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // A minute and an hour into the second day, across a month end.
        assert_eq!(rfc3339(at(2_682_061, 7)), "1970-02-01T01:01:01.007Z");
        // The second before a year ends, and a leap day - including 2000's, the century leap year
        // no four-year rule would get right.
        assert_eq!(rfc3339(at(946_684_799, 999)), "1999-12-31T23:59:59.999Z");
        assert_eq!(rfc3339(at(951_825_600, 0)), "2000-02-29T12:00:00.000Z");
        assert_eq!(rfc3339(at(1_709_164_800, 0)), "2024-02-29T00:00:00.000Z");
        // The day this was written, at the millisecond a saved line carries.
        assert_eq!(rfc3339(at(1_790_285_592, 345)), "2026-09-24T21:33:12.345Z");
        // A clock before the epoch is written as the epoch rather than as nonsense.
        assert_eq!(
            rfc3339(UNIX_EPOCH - Duration::from_secs(1)),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn a_saved_line_is_appended_to_what_the_file_already_has() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reference/manual-poses.jsonl");
        // The folders a line goes in are created, and a second press adds to the first line rather
        // than replacing it.
        assert!(!directory.path().join("reference").exists());
        append_line(&path, "first").unwrap();
        append_line(&path, "second").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().collect::<Vec<_>>(), vec!["first", "second"]);
    }

    /// A shots file with two shots, in a temporary folder: enough to resolve a name against.
    fn written_shots(directory: &Path) -> PathBuf {
        let path = directory.join("shots.json");
        fs::write(
            &path,
            r#"{"width": 1600, "height": 900, "shots": [
                {"name": "first", "worldspace_id": 60, "position": [20558.4, -46062.6, -2.1],
                 "yaw": 161.5, "pitch": 5.2, "hfov": 75.0},
                {"name": "inn-front", "interior_cell_id": 86723, "position": [-947.0, 3958.0, 712.0],
                 "yaw": 170.0, "pitch": -10.0, "hfov": 60.0}]}"#,
        )
        .unwrap();
        path
    }

    fn config_with(args: &[&str]) -> EngineConfig {
        EngineConfig::from_args(args.iter().map(|value| (*value).to_owned()))
    }

    /// Several shots files, comma-separated, make one list in their own order; the run starts at
    /// the named shot, and `N` / `B` step through the list, wrapping at both ends, each step to be
    /// posed afresh.
    #[test]
    fn a_start_shot_run_steps_through_every_shot_of_its_files() {
        let directory = tempfile::tempdir().unwrap();
        let first = written_shots(directory.path());
        let second_directory = directory.path().join("second");
        fs::create_dir_all(&second_directory).unwrap();
        let second = written_shots(&second_directory);
        let files = format!("{},{}", first.display(), second.display());
        let mut run = start_shot_run(&config_with(&["--start-shot", &files, "inn-front"]))
            .unwrap()
            .expect("a start shot");
        assert_eq!(run.playlist.len(), 4, "both files' shots, in order");
        assert_eq!(run.index, 1, "the first file's inn-front");
        assert_eq!(run.shot.name, "inn-front");

        run.posed = true;
        run.flying = true;
        run.step(1);
        assert_eq!((run.index, run.shot.name.as_str()), (2, "first"));
        assert_eq!(run.file, second, "the shot's own file");
        assert!(
            !run.posed && !run.flying,
            "a step is posed again, heading included"
        );

        run.step(2);
        assert_eq!(run.index, 0, "N past the end wraps to the start");
        run.step(-1);
        assert_eq!(run.index, 3, "B before the start wraps to the end");
    }

    /// The picture shown in the corner is the shot's own `reference` when that file exists, and the
    /// UESP picture named after the shot otherwise - the rule `tools/compare_shots.py` uses, so the
    /// tool and the comparison sheets show the same picture.
    #[test]
    fn the_reference_picture_is_the_shots_own_or_the_uesp_one_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut shot: Shot = serde_json::from_value(serde_json::json!({
            "name": "RW-03-trader-front",
            "worldspace_id": 60,
            "position": [0.0, 0.0, 0.0],
            "yaw": 0.0,
            "pitch": 0.0,
            "hfov": 75.0
        }))
        .unwrap();
        assert_eq!(
            reference_picture_path(&shot, root),
            root.join("local/reference/uesp/RW-03-trader-front.jpg"),
            "no reference field: the UESP picture named after the shot"
        );

        shot.reference = Some("local/reference/riverwood/trader.jpg".to_owned());
        assert_eq!(
            reference_picture_path(&shot, root),
            root.join("local/reference/uesp/RW-03-trader-front.jpg"),
            "a reference field naming a file that is not there falls back the same way"
        );

        let own = root.join("local/reference/riverwood/trader.jpg");
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, b"not really a jpeg").unwrap();
        assert_eq!(
            reference_picture_path(&shot, root),
            own,
            "a reference field naming a file that is there is the picture"
        );
    }

    #[test]
    fn a_start_shot_resolves_to_the_shot_its_file_names() {
        let directory = tempfile::tempdir().unwrap();
        let path = written_shots(directory.path());
        let config = config_with(&[
            "--assets",
            "converted",
            "--start-shot",
            &path.to_string_lossy(),
            "inn-front",
        ]);
        let run = start_shot_run(&config).unwrap().expect("a start shot");
        assert_eq!(run.shot.name, "inn-front");
        assert_eq!(run.shot.interior_cell_id, Some(86723));
        assert_eq!(run.shot.position, [-947.0, 3958.0, 712.0]);
        assert_eq!(run.shot.space(), Some(SpaceTarget::Interior(86723)));
        assert!((run.aspect - 1600.0 / 900.0).abs() < 1.0e-6);
        assert!(
            !run.posed,
            "the camera is posed by the first frame, not here"
        );

        // A run that asks for no start shot asks for nothing: most runs.
        assert!(start_shot_run(&config_with(&["--walk"])).unwrap().is_none());
    }

    #[test]
    fn a_request_that_names_no_file_or_no_shot_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = written_shots(directory.path());

        // `--start-shot` at the end of the command line: the flag's own arguments are missing.
        let error = start_shot_run(&config_with(&["--start-shot"])).unwrap_err();
        assert!(error.message().contains("shots file"), "{error}");
        assert!(
            error.message().contains("--start-shot <shots.json> <name>"),
            "{error}"
        );

        // A file with no name after it: nothing says which shot to start at, and the message names
        // the ones the file has.
        let error =
            start_shot_run(&config_with(&["--start-shot", &path.to_string_lossy()])).unwrap_err();
        assert!(error.message().contains("name of a shot"), "{error}");
        assert!(
            error.message().contains(path.to_string_lossy().as_ref()),
            "{error}"
        );

        // A name the file does not have.
        let error = start_shot_run(&config_with(&[
            "--start-shot",
            &path.to_string_lossy(),
            "nowhere",
        ]))
        .unwrap_err();
        assert!(error.message().contains("\"nowhere\""), "{error}");
        assert!(error.message().contains("first, inn-front"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_there_or_not_a_shots_file_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("not-there.json");
        let error = start_shot_run(&config_with(&[
            "--start-shot",
            &missing.to_string_lossy(),
            "shot",
        ]))
        .unwrap_err();
        assert!(
            error.message().contains("not-there.json"),
            "the message names the file: {error}"
        );

        let broken = directory.path().join("broken.json");
        fs::write(&broken, "{\"width\": 1600, \"height\": 900, \"shots\": []}").unwrap();
        let error = start_shot_run(&config_with(&[
            "--start-shot",
            &broken.to_string_lossy(),
            "shot",
        ]))
        .unwrap_err();
        assert!(
            error.message().contains("no shots"),
            "a file the engine cannot use says why: {error}"
        );
    }

    #[test]
    fn a_start_shot_is_refused_beside_every_flag_that_poses_the_camera_too() {
        let directory = tempfile::tempdir().unwrap();
        let path = written_shots(directory.path());
        let path = path.to_string_lossy().into_owned();
        let start = ["--start-shot", path.as_str(), "first"];

        // Each of these is a run that poses the camera itself, so the combination cannot mean
        // anything: the message names the other flag, so the reader knows which one to drop.
        let refusals = [
            ("--shots", vec!["--shots", "other.json"], "--shots"),
            (
                "--demo-tour",
                vec!["--demo-tour", "local/demo/tour"],
                "--demo-tour",
            ),
            (
                "--start-position",
                vec!["--start-position", "100", "200", "300"],
                "--start-position",
            ),
            // A demo start *is* a start position, and is refused for the same reason.
            ("--demo", vec!["--demo", "alftand"], "--start-position"),
        ];
        for (reason, other, wanted) in refusals {
            let mut args = other.clone();
            args.extend(start);
            let error = start_shot_run(&config_with(&args)).unwrap_err();
            assert!(
                error.message().contains(wanted),
                "{reason} is refused and said so: {error}"
            );
        }

        // And the flags that do not pose the camera are not refused: the assets, the worldspace,
        // the streaming radii and the headless switch all belong to a `--start-shot` run.
        let run = start_shot_run(&config_with(&[
            "--assets",
            "converted",
            "--grid-x",
            "4",
            "--terrain-radius",
            "4",
            "--start-shot",
            path.as_str(),
            "first",
        ]))
        .unwrap();
        assert_eq!(run.expect("a start shot").shot.name, "first");
    }

    /// The `RenderOrigin` a saved exterior pose is taken apart against is the one the camera is
    /// rendered in: subtracting a different one would move the pose by whole cells. The round trip
    /// holds from any origin, which is what makes a line absolute rather than cell-relative.
    #[test]
    fn an_exterior_pose_is_absolute_whatever_the_render_origin_is() {
        let position = Vec3::new(20558.4, -46062.6, -2.1);
        for origin in [IVec2::new(5, -12), IVec2::new(0, 0), IVec2::new(-3, 7)] {
            let render = render_position(position, origin);
            assert!(
                creation_position(render, origin).abs_diff_eq(position, 0.01),
                "origin {origin:?}: {render} back to {:?}",
                creation_position(render, origin)
            );
        }
        // And the origin the engine actually holds for that pose once it is standing in it: its own
        // cell, which is where `switch_space` puts the render origin.
        assert_eq!(crate::config::grid_of(position.x, position.y), (5, -12));
    }

    /// An interior is read at its absolute Creation coordinates and against the zero origin: an
    /// interior root sits at the render origin, whatever cell the run's exterior origin names.
    #[test]
    fn an_interior_pose_is_read_at_the_absolute_coordinates() {
        let position = Vec3::new(-947.0, 3958.0, 712.0);
        let render = crate::streaming::creation_to_bevy(position);
        assert!(creation_position(render, IVec2::ZERO).abs_diff_eq(position, 0.01));
    }
}
