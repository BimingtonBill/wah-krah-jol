//! Field notes: `F12` takes a screenshot, writes the camera's pose and a snapshot of the running
//! state, and asks for a note - so a person playing the demo can show us a bug directly instead of
//! describing it in words.
//!
//! The user's own report - "I walked in and out of Sven's house a few times and the shadows
//! stopped working outside" - could not be reproduced from the words alone. This borrows the shape
//! of the reference-pose tool ([`crate::pose_capture`]: a picture, a pose, a note) and adds what a
//! bug report needs beyond a reference shot: a state snapshot (which lights and cameras exist and
//! what they carry, which doors are near, the space the player stood in) and a chronological log of
//! door crossings, so "how many trips in and out" is in the file without anyone having counted.
//!
//! # What `F12` writes
//!
//! Into one folder per run, `<captures-dir>/<run start>/` (`--captures-dir`, default
//! [`crate::config::DEFAULT_CAPTURES_DIR`]):
//!
//! - `NN.png` - the frame as the player saw it. The screenshot is requested in the same frame `F12`
//!   is pressed; the note box is not shown until the frame after ([`promote_pending_note_box`]),
//!   so it is never in the picture.
//! - `NN.json` - the pose in the shots-file contract ([`crate::shots`]), `saved_at` (UTC) and
//!   `saved_at_local` (the local clock, with its offset), the note (null
//!   until the note box closes) and the state snapshot ([`StateSnapshot`]).
//! - `shots.json` - every capture of the run so far, as a shots file (`engine --shots
//!   <run>/shots.json` re-renders them). Rewritten on every capture.
//! - `notes.md` - the one file a reader opens first: a header (start time, command line, the
//!   engine binary's path and modified time), one line per capture once its note box closes, and
//!   one line per door crossing as it happens, whether or not `F12` was ever pressed.
//!
//! # The note box, and what still leaks
//!
//! Right after a capture the note box opens; typed characters, `Backspace`, `Enter` (save) and
//! `Esc` (skip, no note) work, and while it is open every other key is blocked
//! ([`block_input_while_typing`], in `PreUpdate` after Bevy's own input systems, resets
//! `ButtonInput<KeyCode>` and `ButtonInput<MouseButton>` before anything in `Update` - `player.rs`
//! among it - reads them). Mouse *look* is not a button: it reads
//! [`AccumulatedMouseMotion`], which this module zeroes for the same reason. What is not blocked -
//! because nothing public reaches it from here - is whatever else reads `KeyboardInput` or
//! `MouseButtonInput` messages directly rather than the button state; none of `player.rs`,
//! `door_animation.rs` or `transition.rs` do, so as of this writing nothing leaks.
//!
//! # What the state snapshot cannot reach
//!
//! [`crate::portal`]'s `PortalDestinationSun` and `PortalCamera` markers are private to that
//! module and outside this task's owned files, so "is this the portal's sun / camera" is read
//! instead from the public proxy every entity on [`crate::portal::DESTINATION_LAYER`] shares: its
//! `RenderLayers`. [`StateSnapshot::not_reached`] says so in the file itself.

use crate::{
    config::EngineConfig,
    demo_hud,
    doors::{DoorCrossed, DoorState, LoadDoor},
    streaming::{ActiveCell, RenderOrigin},
    world::components::StreamingCamera,
};
use bevy::{
    camera::visibility::RenderLayers,
    input::{ButtonState, InputSystems, keyboard::KeyboardInput, mouse::AccumulatedMouseMotion},
    light::CascadeShadowConfig,
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::PrimaryWindow,
};
use serde::Serialize;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// `F12`: take a capture.
const CAPTURE_KEY: KeyCode = KeyCode::F12;

/// Where the note box sits: centred, and a little above the middle of the screen so it does not
/// compete with the door prompt below it (`crate::player::DOOR_PROMPT_TOP`).
const NOTE_BOX_TOP: Val = Val::Percent(38.0);

/// How far from the camera a load door is still worth reporting in the state snapshot.
const DOOR_SNAPSHOT_RADIUS: f32 = 1000.0;

/// The window [`FrameTimeWindow`] averages over.
const FRAME_TIME_WINDOW_SECONDS: f64 = 1.0;

/// `--field-notes-test`: seconds to wait after the first door crossing before taking the test
/// capture.
const TEST_CAPTURE_DELAY_SECONDS: f32 = 3.0;

/// `--field-notes-test`: seconds to let the capture reach disk before the run exits.
const TEST_EXIT_GRACE_SECONDS: f32 = 1.0;

// ---------------------------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------------------------

/// Adds the field-notes capture to every windowed run that has opened the world
/// (`crate::app::run`, gated on `!EngineConfig::headless` alone): unlike
/// [`crate::pose_capture::PoseCapturePlugin`] this is not further gated on
/// [`EngineConfig::interactive`], because a person can be at the keyboard of a plain flight or a
/// `--shots` run too, not only a walk or a demo tour.
pub struct FieldNotesPlugin;

impl Plugin for FieldNotesPlugin {
    fn build(&self, app: &mut App) {
        let test_mode = app
            .world()
            .get_resource::<EngineConfig>()
            .is_some_and(|config| config.portal.field_notes_test);
        app.init_resource::<NoteBox>()
            .init_resource::<FrameTimeWindow>()
            .init_resource::<demo_hud::Notices>()
            .add_systems(
                Startup,
                (
                    start_run,
                    setup_hud,
                    demo_hud::spawn_notices_panel,
                    demo_hud::spawn_fps_panel,
                ),
            )
            .add_systems(PreUpdate, block_input_while_typing.after(InputSystems))
            .add_systems(
                Update,
                (
                    track_frame_time,
                    promote_pending_note_box.before(capture_on_key),
                    capture_on_key,
                    type_note.after(capture_on_key),
                    demo_hud::fade_notices,
                    demo_hud::sync_notices,
                    demo_hud::update_fps_panel,
                    sync_note_ui,
                    // After the crossing, so the arrival notice and the notes line land on the
                    // frame the player arrives in, not one later (impl-203).
                    record_door_crossings.after(crate::transition::DoorTransition),
                    notice_on_crossing
                        .after(crate::transition::DoorTransition)
                        .before(demo_hud::sync_notices),
                ),
            );
        if test_mode {
            app.init_resource::<FieldNotesTest>()
                .add_systems(Update, drive_field_notes_test.after(capture_on_key));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The run: folders, notes.md, shots.json, NN.json
// ---------------------------------------------------------------------------------------------

/// The local clock's offset from UTC right now, in minutes east of Greenwich (`+570` for
/// Adelaide in winter), daylight saving included.
///
/// The standard library has no local time, and the engine carries no date library, so on Windows
/// this asks the OS directly (`GetTimeZoneInformation`, kernel32, declared here rather than through
/// `windows-sys`, whose `Win32_System_Time` feature the crate does not enable). The call reports
/// which of the standard or daylight bias is in force now. Elsewhere, and whenever the call fails,
/// the offset is 0 and every "local" time below is UTC.
pub fn local_offset_minutes() -> i32 {
    #[cfg(windows)]
    {
        windows_local_offset_minutes().unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        0
    }
}

#[cfg(windows)]
fn windows_local_offset_minutes() -> Option<i32> {
    /// `SYSTEMTIME`: eight `WORD`s.
    #[repr(C)]
    struct SystemTimeRecord {
        _fields: [u16; 8],
    }
    /// `TIME_ZONE_INFORMATION`, field for field.
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTimeRecord,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTimeRecord,
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTimeZoneInformation(information: *mut TimeZoneInformation) -> u32;
    }
    const TIME_ZONE_ID_UNKNOWN: u32 = 0;
    const TIME_ZONE_ID_STANDARD: u32 = 1;
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

    let mut information = std::mem::MaybeUninit::<TimeZoneInformation>::zeroed();
    // SAFETY: the pointer is to a writable, correctly laid out `TIME_ZONE_INFORMATION`, which the
    // call only fills in; it is plain data, so the zeroed value is valid whatever the call writes.
    let which = unsafe { GetTimeZoneInformation(information.as_mut_ptr()) };
    let information = unsafe { information.assume_init() };
    // Bias is minutes to *add* to local time to reach UTC, so the offset is its negation.
    let bias = match which {
        TIME_ZONE_ID_UNKNOWN => information.bias,
        TIME_ZONE_ID_STANDARD => information.bias + information.standard_bias,
        TIME_ZONE_ID_DAYLIGHT => information.bias + information.daylight_bias,
        _ => return None,
    };
    Some(-bias)
}

/// A time as RFC 3339 in the local time of a clock `offset_minutes` east of UTC, millisecond
/// precision, with the offset written out: `2026-09-25T23:33:07.512+09:30`. An offset of 0 is
/// written `+00:00`, not `Z`, so a reader can tell "local, which happened to be UTC" from the
/// UTC stamps ([`crate::pose_capture::rfc3339`]).
pub fn rfc3339_local(time: SystemTime, offset_minutes: i32) -> String {
    let shift = std::time::Duration::from_secs(u64::from(offset_minutes.unsigned_abs()) * 60);
    let shifted = if offset_minutes >= 0 {
        time.checked_add(shift)
    } else {
        time.checked_sub(shift)
    }
    .unwrap_or(time);
    let stamp = crate::pose_capture::rfc3339(shifted);
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let magnitude = offset_minutes.unsigned_abs();
    format!(
        "{}{sign}{:02}:{:02}",
        stamp.trim_end_matches('Z'),
        magnitude / 60,
        magnitude % 60
    )
}

/// [`rfc3339_local`] with the local clock's offset now.
fn local_stamp(time: SystemTime) -> String {
    rfc3339_local(time, local_offset_minutes())
}

/// The folder name a run's captures go in: the start time on the local clock, filesystem-safe.
///
/// A colon is not a valid Windows filename character, so the local RFC 3339 stamp
/// ([`rfc3339_local`]) is truncated to the second and its colons swapped for dashes:
/// `2026-09-25T23:33:07.512+09:30` becomes `2026-09-25_23-33-07`.
pub fn run_folder_name(now: SystemTime, offset_minutes: i32) -> String {
    let stamp = rfc3339_local(now, offset_minutes);
    let date = &stamp[0..10];
    let time = &stamp[11..19];
    format!("{date}_{}", time.replace(':', "-"))
}

/// The `HH:MM:SS` of an RFC 3339 stamp (local or UTC: it is the clock as written), for a notes.md line that does not need the date or the
/// millisecond.
fn time_of_day(rfc3339: &str) -> String {
    rfc3339.get(11..19).unwrap_or(rfc3339).to_owned()
}

/// A space as a notes.md line names it: there is no friendly-name lookup available here (that is
/// the world database's, not a cheap component read), so this is deliberately plain.
fn format_space(worldspace_id: Option<u32>, interior_cell_id: Option<u32>) -> String {
    match (interior_cell_id, worldspace_id) {
        (Some(cell), _) => format!("interior 0x{cell:X}"),
        (None, Some(worldspace)) => format!("exterior 0x{worldspace:X}"),
        (None, None) => "unknown space".to_owned(),
    }
}

/// One line of `notes.md`: a capture, once its note box has closed, or a door crossing, as it
/// happens.
#[derive(Debug, Clone, PartialEq)]
enum NoteEntry {
    Capture {
        index: u32,
        time: String,
        space: String,
        note: String,
    },
    Crossing {
        time: String,
        from: String,
        to: String,
        label: String,
        count: u32,
    },
}

impl NoteEntry {
    fn line(&self) -> String {
        match self {
            NoteEntry::Capture {
                index,
                time,
                space,
                note,
            } => format!("{index:02}  {time}  {space}  {note}  ![]({index:02}.png)"),
            NoteEntry::Crossing {
                time,
                from,
                to,
                label,
                count,
            } => {
                let destination = if label.is_empty() {
                    to.clone()
                } else {
                    format!("{to} ({label})")
                };
                format!("- {time}  crossed from {from} to {destination} (crossing {count})")
            }
        }
    }
}

/// A capture written to disk but not yet closed: its note box is open, and `Enter` or `Esc`
/// rewrites [`Self::path`] with whatever note was typed.
struct PendingCapture {
    index: u32,
    path: PathBuf,
    record: CaptureRecord,
}

/// One run's worth of field notes: where they go, what has happened so far, and the one capture
/// whose note is still being typed.
#[derive(Resource)]
struct FieldNotesRun {
    directory: PathBuf,
    started_at: String,
    command_line: String,
    binary_path: String,
    binary_modified: String,
    next_index: u32,
    crossings: u32,
    entries: Vec<NoteEntry>,
    captures: Vec<CaptureShotEntry>,
    pending: Option<PendingCapture>,
}

impl FieldNotesRun {
    /// A run started at `now` on a clock `offset_minutes` east of UTC: its folder name and every
    /// time notes.md shows are on that local clock.
    fn new(config: &EngineConfig, now: SystemTime, offset_minutes: i32) -> Self {
        let directory = config
            .portal
            .captures_dir
            .join(run_folder_name(now, offset_minutes));
        let binary_path = std::env::current_exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_owned());
        let binary_modified = std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok())
            .map(|modified| rfc3339_local(modified, offset_minutes))
            .unwrap_or_else(|| "unknown".to_owned());
        Self {
            directory,
            started_at: rfc3339_local(now, offset_minutes),
            command_line: std::env::args().collect::<Vec<_>>().join(" "),
            binary_path,
            binary_modified,
            next_index: 1,
            crossings: 0,
            entries: Vec::new(),
            captures: Vec::new(),
            pending: None,
        }
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.directory)
    }

    fn capture_path(&self, index: u32, extension: &str) -> PathBuf {
        self.directory.join(format!("{index:02}.{extension}"))
    }

    /// `notes.md`'s header: the information that does not depend on any capture or crossing.
    fn header_text(&self) -> String {
        format!(
            "# Field notes - {}\n\ncommand: `{}`\nengine: {} (modified {})\n\n",
            self.started_at, self.command_line, self.binary_path, self.binary_modified
        )
    }

    /// The whole of `notes.md`: the header, then every entry in the order it happened.
    fn notes_md_text(&self) -> String {
        let mut text = self.header_text();
        for entry in &self.entries {
            text.push_str(&entry.line());
            text.push('\n');
        }
        text
    }

    /// Writes `notes.md` once the run has a capture. Until then crossings are only kept in
    /// [`Self::entries`], so a run nobody pressed `F12` in (a tour, `--shots`, a benchmark) leaves
    /// no folder behind, and the first capture's `notes.md` still lists every crossing before it.
    fn write_notes_md(&self) -> std::io::Result<()> {
        if self.captures.is_empty() {
            return Ok(());
        }
        self.ensure_dir()?;
        std::fs::write(self.directory.join("notes.md"), self.notes_md_text())
    }

    /// `shots.json`'s text: every capture so far, as a shots file at `width`x`height`.
    fn shots_file_text(&self, width: u32, height: u32) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&ShotsFileOut {
            width,
            height,
            shots: self.captures.clone(),
        })
    }

    fn write_shots_json(&self, width: u32, height: u32) -> std::io::Result<()> {
        self.ensure_dir()?;
        let text = self
            .shots_file_text(width, height)
            .map_err(std::io::Error::other)?;
        std::fs::write(self.directory.join("shots.json"), text)
    }

    /// Writes `NN.png` (via `commands`, asynchronously) and `NN.json` (synchronously - one small
    /// file), adds this capture to `shots.json`, and leaves it [`Self::pending`] until its note
    /// box closes. Returns the capture's index.
    fn capture(
        &mut self,
        commands: &mut Commands,
        pose: CapturePose,
        state: StateSnapshot,
        graphics: Option<crate::graphics_settings::GraphicsSettings>,
        (width, height): (u32, u32),
    ) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        if let Err(error) = self.ensure_dir() {
            warn!(
                target: "field_notes",
                "could not create {}: {error}",
                self.directory.display()
            );
        }
        let png_path = self.capture_path(index, "png");
        let saved = SystemTime::now();
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(png_path));
        let record = CaptureRecord {
            worldspace_id: pose.worldspace_id,
            interior_cell_id: pose.interior_cell_id,
            position: pose.position,
            yaw: pose.yaw,
            pitch: pose.pitch,
            hfov: pose.hfov,
            saved_at: crate::pose_capture::rfc3339(saved),
            saved_at_local: local_stamp(saved),
            note: None,
            state,
            graphics,
        };
        let json_path = self.capture_path(index, "json");
        match serde_json::to_string_pretty(&record) {
            Ok(text) => {
                if let Err(error) = std::fs::write(&json_path, text) {
                    warn!(target: "field_notes", "could not write {}: {error}", json_path.display());
                }
            }
            Err(error) => warn!(target: "field_notes", "could not encode {index:02}.json: {error}"),
        }
        self.pending = Some(PendingCapture {
            index,
            path: json_path,
            record,
        });
        self.captures.push(CaptureShotEntry {
            name: format!("{index:02}"),
            worldspace_id: pose.worldspace_id,
            interior_cell_id: pose.interior_cell_id,
            position: pose.position,
            yaw: pose.yaw,
            pitch: pose.pitch,
            hfov: pose.hfov,
        });
        if let Err(error) = self.write_shots_json(width, height) {
            warn!(target: "field_notes", "could not write shots.json: {error}");
        }
        info!(target: "field_notes", "capture {index:02} in {}", self.directory.display());
        index
    }

    /// `Enter` (`note = Some(text)`) or `Esc` (`note = None`) on the note box: rewrites the
    /// pending capture's `NN.json` with the note, and appends its line to `notes.md`.
    fn close_note(&mut self, note: Option<String>) -> std::io::Result<()> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        pending.record.note = note.clone();
        let text = serde_json::to_string_pretty(&pending.record).map_err(std::io::Error::other)?;
        std::fs::write(&pending.path, text)?;
        self.entries.push(NoteEntry::Capture {
            index: pending.index,
            time: time_of_day(&pending.record.saved_at_local),
            space: format_space(
                pending.record.worldspace_id,
                pending.record.interior_cell_id,
            ),
            note: note.unwrap_or_default(),
        });
        self.write_notes_md()
    }

    /// A door crossing, recorded the frame it happens whether or not a note box is open.
    fn record_crossing(
        &mut self,
        time: String,
        from: SpaceSnapshot,
        to: SpaceSnapshot,
        label: String,
    ) -> std::io::Result<()> {
        self.crossings += 1;
        self.entries.push(NoteEntry::Crossing {
            time,
            from: format_space(from.worldspace_id, from.interior_cell_id),
            to: format_space(to.worldspace_id, to.interior_cell_id),
            label,
            count: self.crossings,
        });
        self.write_notes_md()
    }
}

/// The pose fields a capture writes, gathered the same way [`crate::pose_capture::save_pose_on_key`]
/// gathers them, so a capture's `NN.json` and a hand-saved pose read the same world the same way.
struct CapturePose {
    worldspace_id: Option<u32>,
    interior_cell_id: Option<u32>,
    position: [f32; 3],
    yaw: f32,
    pitch: f32,
    hfov: f32,
}

/// A capture's pose, read from the camera exactly as `P` reads it
/// ([`crate::pose_capture::camera_pose_angles`], [`crate::pose_capture::creation_position`],
/// [`crate::pose_capture::horizontal_fov_degrees`]) - the public helpers `pose_capture.rs` already
/// has; nothing there needed to change.
fn capture_pose(
    transform: &Transform,
    perspective: &PerspectiveProjection,
    active: &ActiveCell,
    origin: &RenderOrigin,
    window: Option<&Window>,
) -> CapturePose {
    let (yaw, pitch) = crate::pose_capture::camera_pose_angles(transform.rotation);
    let position = crate::pose_capture::creation_position(
        transform.translation,
        if active.interior.is_some() {
            IVec2::ZERO
        } else {
            origin.0
        },
    )
    .to_array();
    let aspect = window.map_or(perspective.aspect_ratio, |window| {
        window.width() / window.height()
    });
    let hfov = crate::pose_capture::horizontal_fov_degrees(perspective.fov.to_degrees(), aspect);
    CapturePose {
        worldspace_id: active.interior.is_none().then_some(active.worldspace_id),
        interior_cell_id: active.interior,
        position,
        yaw,
        pitch,
        hfov,
    }
}

// ---------------------------------------------------------------------------------------------
// shots.json and NN.json shapes
// ---------------------------------------------------------------------------------------------

/// One shot of `shots.json`, in [`crate::shots::Shot`]'s own field names: a value serialized this
/// way loads back through [`crate::shots::ShotsFile`] unchanged (`shots.rs` is read-only to this
/// task, so this mirrors it rather than editing it - it has no `Serialize` of its own).
#[derive(Debug, Clone, Serialize)]
struct CaptureShotEntry {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    worldspace_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interior_cell_id: Option<u32>,
    position: [f32; 3],
    yaw: f32,
    pitch: f32,
    hfov: f32,
}

#[derive(Debug, Clone, Serialize)]
struct ShotsFileOut {
    width: u32,
    height: u32,
    shots: Vec<CaptureShotEntry>,
}

/// `NN.json`: the pose, when it was saved, the note (null until the box closes) and the state
/// snapshot.
#[derive(Debug, Clone, Serialize)]
struct CaptureRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    worldspace_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interior_cell_id: Option<u32>,
    position: [f32; 3],
    yaw: f32,
    pitch: f32,
    hfov: f32,
    /// When the capture was taken, RFC 3339 in UTC (the pose tool's form).
    saved_at: String,
    /// The same moment on the local clock, with its offset (`rfc3339_local`): what notes.md shows.
    saved_at_local: String,
    note: Option<String>,
    state: StateSnapshot,
    /// The graphics settings the capture was rendered with (`crate::graphics_settings`); absent
    /// on a run that has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    graphics: Option<crate::graphics_settings::GraphicsSettings>,
}

// ---------------------------------------------------------------------------------------------
// The state snapshot
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
struct SpaceSnapshot {
    worldspace_id: Option<u32>,
    interior_cell_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
struct LightSnapshot {
    /// `Entity`'s own `Debug` form (e.g. `12v1`): cheap, public, and unique for the run - there is
    /// no reason to reach for a numeric id this is only ever read back as text.
    entity: String,
    name: Option<String>,
    /// Read from [`RenderLayers`] rather than `crate::portal::PortalDestinationSun`, which is
    /// private to that module (see the module doc comment).
    is_portal_sun: bool,
    shadows_enabled: bool,
    illuminance: f32,
    render_layers: Vec<usize>,
    cascade_shadow_bounds: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize)]
struct CameraSnapshot {
    entity: String,
    is_active: bool,
    render_layers: Vec<usize>,
    /// Read from [`RenderLayers`] rather than `crate::portal::PortalCamera`, private to that
    /// module (see the module doc comment).
    is_portal_camera: bool,
    order: isize,
}

#[derive(Debug, Clone, Serialize)]
struct DoorSnapshot {
    form_id: String,
    state: String,
    distance: f32,
}

#[derive(Debug, Clone, Serialize)]
struct FrameTimeSnapshot {
    average_ms: Option<f64>,
    frames_in_window: usize,
}

#[derive(Debug, Clone, Serialize)]
struct StateSnapshot {
    active_space: SpaceSnapshot,
    crossings_so_far: u32,
    lights: Vec<LightSnapshot>,
    cameras: Vec<CameraSnapshot>,
    doors_within_1000: Vec<DoorSnapshot>,
    frame_time: FrameTimeSnapshot,
    /// What a read-only, public-only snapshot could not reach (the module doc comment says why);
    /// empty on a snapshot that hit nothing it had to work around.
    not_reached: Vec<String>,
}

type LightsQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static DirectionalLight,
        Option<&'static Name>,
        Option<&'static RenderLayers>,
        Option<&'static CascadeShadowConfig>,
    ),
>;
type CamerasQuery<'w, 's> = Query<'w, 's, (Entity, &'static Camera, Option<&'static RenderLayers>)>;
type DoorsQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static GlobalTransform,
        &'static LoadDoor,
        Option<&'static DoorState>,
    ),
>;

/// Builds the state snapshot from read-only queries: the active space and crossing count, every
/// directional light and camera in the world, the load doors within
/// [`DOOR_SNAPSHOT_RADIUS`] of `eye`, and the frame time [`FrameTimeWindow`] has averaged.
fn state_snapshot(
    active: &ActiveCell,
    crossings: u32,
    eye: Vec3,
    cameras: &CamerasQuery,
    lights: &LightsQuery,
    doors: &DoorsQuery,
    frame_time: &FrameTimeWindow,
) -> StateSnapshot {
    let destination_layer = RenderLayers::layer(crate::portal::DESTINATION_LAYER);
    let lights = lights
        .iter()
        .map(|(entity, light, name, layers, cascades)| {
            let layers = layers.cloned().unwrap_or_default();
            LightSnapshot {
                entity: format!("{entity:?}"),
                name: name.map(|name| name.as_str().to_owned()),
                is_portal_sun: layers.intersects(&destination_layer),
                shadows_enabled: light.shadow_maps_enabled,
                illuminance: light.illuminance,
                render_layers: layers.iter().collect(),
                cascade_shadow_bounds: cascades.map(|config| config.bounds.clone()),
            }
        })
        .collect();
    let cameras = cameras
        .iter()
        .map(|(entity, camera, layers)| {
            let layers = layers.cloned().unwrap_or_default();
            CameraSnapshot {
                entity: format!("{entity:?}"),
                is_active: camera.is_active,
                render_layers: layers.iter().collect(),
                is_portal_camera: layers.intersects(&destination_layer),
                order: camera.order,
            }
        })
        .collect();
    let doors_within_1000 = doors
        .iter()
        .filter_map(|(transform, door, state)| {
            let distance = transform.translation().distance(eye);
            (distance <= DOOR_SNAPSHOT_RADIUS).then(|| DoorSnapshot {
                form_id: format!("0x{:08X}", door.ref_id),
                state: state.map_or_else(|| "unresolved".to_owned(), |state| format!("{state:?}")),
                distance,
            })
        })
        .collect();
    StateSnapshot {
        active_space: SpaceSnapshot {
            worldspace_id: active.interior.is_none().then_some(active.worldspace_id),
            interior_cell_id: active.interior,
        },
        crossings_so_far: crossings,
        lights,
        cameras,
        doors_within_1000,
        frame_time: FrameTimeSnapshot {
            average_ms: frame_time.average_ms(),
            frames_in_window: frame_time.frame_count(),
        },
        not_reached: vec![
            "crate::portal::PortalDestinationSun and PortalCamera are private to portal.rs; \
             is_portal_sun/is_portal_camera are read from RenderLayers intersecting \
             DESTINATION_LAYER instead (see the module doc comment)."
                .to_owned(),
        ],
    }
}

/// A rolling window of frame times, kept for [`StateSnapshot::frame_time`] rather than read from
/// `FrameTimeDiagnosticsPlugin`'s own history (whose length is a smoothing choice, not a second).
#[derive(Resource, Default)]
struct FrameTimeWindow {
    samples: VecDeque<(f64, f64)>,
}

impl FrameTimeWindow {
    fn push(&mut self, elapsed_secs: f64, delta_ms: f64) {
        self.samples.push_back((elapsed_secs, delta_ms));
        while let Some(&(oldest, _)) = self.samples.front() {
            if elapsed_secs - oldest > FRAME_TIME_WINDOW_SECONDS {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn average_ms(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        Some(self.samples.iter().map(|(_, ms)| ms).sum::<f64>() / self.samples.len() as f64)
    }

    fn frame_count(&self) -> usize {
        self.samples.len()
    }
}

fn track_frame_time(time: Res<Time>, mut window: ResMut<FrameTimeWindow>) {
    window.push(time.elapsed_secs_f64(), time.delta_secs_f64() * 1000.0);
}

// ---------------------------------------------------------------------------------------------
// The note box
// ---------------------------------------------------------------------------------------------

/// The note being typed for the most recent capture. The "Saved NN" line shown once it closes is
/// the shared [`demo_hud::Notices`] panel's, not this resource's own.
#[derive(Resource, Default)]
pub(crate) struct NoteBox {
    /// Set the frame after a capture ([`promote_pending_note_box`]), never the frame of the
    /// capture itself - see the module doc comment on why the box must not be drawn yet.
    pending_open: Option<u32>,
    open: bool,
    capture_index: Option<u32>,
    text: String,
}

impl NoteBox {
    fn push_char(&mut self, character: char) {
        if !character.is_control() {
            self.text.push(character);
        }
    }

    fn backspace(&mut self) {
        self.text.pop();
    }

    /// `Enter`: the trimmed text to save as the note, or `None` if nothing was typed.
    fn take(&mut self) -> Option<String> {
        let note = self.text.trim().to_owned();
        self.text.clear();
        (!note.is_empty()).then_some(note)
    }

    /// `Esc`: no note, whatever was typed is thrown away.
    fn cancel(&mut self) {
        self.text.clear();
    }
}

/// Promotes a capture's pending note box to open, one frame after the capture that asked for it -
/// never the same frame, so the capture's own screenshot is never taken with the box already
/// drawn.
fn promote_pending_note_box(mut note: ResMut<NoteBox>) {
    if let Some(index) = note.pending_open.take() {
        note.open = true;
        note.capture_index = Some(index);
        note.text.clear();
    }
}

/// `F12`: captures the frame, the pose and the state snapshot, and asks the note box to open next
/// frame. Blocked while a note box is already open or about to be (`note.open` is only ever false
/// on the frame a capture happens, since [`block_input_while_typing`] would otherwise have eaten
/// the key already).
#[allow(clippy::too_many_arguments)]
fn capture_on_key(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut run: ResMut<FieldNotesRun>,
    mut note: ResMut<NoteBox>,
    mut commands: Commands,
    active: Res<ActiveCell>,
    origin: Res<RenderOrigin>,
    camera: Query<(&Transform, &Projection), With<StreamingCamera>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: CamerasQuery,
    lights: LightsQuery,
    doors: DoorsQuery,
    frame_time: Res<FrameTimeWindow>,
    graphics: Option<Res<crate::graphics_settings::GraphicsSettings>>,
) {
    if !keyboard.just_pressed(CAPTURE_KEY) || note.open || note.pending_open.is_some() {
        return;
    }
    let Ok((transform, projection)) = camera.single() else {
        return;
    };
    let Projection::Perspective(perspective) = projection else {
        return;
    };
    let window = windows.iter().next();
    let pose = capture_pose(transform, perspective, &active, &origin, window);
    let state = state_snapshot(
        &active,
        run.crossings,
        transform.translation,
        &cameras,
        &lights,
        &doors,
        &frame_time,
    );
    let size = window.map_or((1600, 900), |window| {
        (window.width() as u32, window.height() as u32)
    });
    let graphics = graphics.map(|graphics| graphics.clone());
    let index = run.capture(&mut commands, pose, state, graphics, size);
    note.pending_open = Some(index);
}

/// The "Saved" notice's text: the capture's index and where its picture landed, so the shared
/// notices panel says where to look rather than just a number (the user, 2026-09-25).
fn saved_notice_text(index: u32, path: &Path) -> String {
    format!("Saved {index:02} ({})", path.display())
}

/// Reads the note being typed while the box is open: characters and `Backspace` from
/// `KeyboardInput` (matching [`crate::pose_capture::type_note`]'s own approach, already proven
/// correct here), `Enter` saves it and `Esc` throws it away - either way the capture's `NN.json`
/// and `notes.md` are updated and the shared notices panel ([`demo_hud::Notices`]) shows "Saved".
fn type_note(
    mut typed: MessageReader<KeyboardInput>,
    mut note: ResMut<NoteBox>,
    mut run: ResMut<FieldNotesRun>,
    mut notices: ResMut<demo_hud::Notices>,
) {
    if !note.open {
        typed.clear();
        return;
    }
    for input in typed.read() {
        if input.state != ButtonState::Pressed {
            continue;
        }
        let closing = match input.key_code {
            KeyCode::Enter | KeyCode::NumpadEnter => Some(note.take()),
            KeyCode::Escape => {
                note.cancel();
                Some(None)
            }
            KeyCode::Backspace => {
                note.backspace();
                None
            }
            _ => {
                if let Some(text) = &input.text {
                    for character in text.chars() {
                        note.push_char(character);
                    }
                }
                None
            }
        };
        if let Some(saved) = closing {
            note.open = false;
            let index = note.capture_index.take().unwrap_or_default();
            if let Err(error) = run.close_note(saved) {
                warn!(target: "field_notes", "could not save the note for {index:02}: {error}");
            }
            notices.show(saved_notice_text(index, &run.capture_path(index, "png")));
            break;
        }
    }
}

/// Shows the place just arrived at in the shared notices panel - the "Now in ..." line
/// `crate::demo_tour`'s old objective used to show, before the user asked for it gone
/// (2026-09-25: "the demo doesn't need an objective").
fn notice_on_crossing(
    mut crossed: MessageReader<DoorCrossed>,
    mut notices: ResMut<demo_hud::Notices>,
) {
    for event in crossed.read() {
        notices.show(arrival_notice_text(&event.label));
    }
}

/// The arrival notice's text: the place just crossed into, trimmed the way the label arrives.
fn arrival_notice_text(label: &str) -> String {
    label.trim().to_owned()
}

/// Blocks every other key and mouse button - and mouse *look*, which is not a button
/// ([`AccumulatedMouseMotion`]) - while the note box is open, in `PreUpdate` after Bevy's own
/// input systems and therefore before anything in `Update` (player, doors, the demo tour) reads
/// them.
pub(crate) fn block_input_while_typing(
    note: Res<NoteBox>,
    mut keyboard: ResMut<ButtonInput<KeyCode>>,
    mut mouse_buttons: ResMut<ButtonInput<MouseButton>>,
    mut mouse_motion: ResMut<AccumulatedMouseMotion>,
) {
    if !note.open {
        return;
    }
    keyboard.reset_all();
    mouse_buttons.reset_all();
    mouse_motion.delta = Vec2::ZERO;
}

// ---------------------------------------------------------------------------------------------
// The HUD
// ---------------------------------------------------------------------------------------------

/// The note box's own entity, in the HUD's shared style (`crate::demo_hud`): a title line, the
/// text being typed with a caret, and a hint line - not the field notes' own look any more, so it
/// reads as the same HUD as the controls panel and the door prompt.
#[derive(Component)]
struct NoteBoxPanel;

fn setup_hud(mut commands: Commands) {
    let (mut node, background) = demo_hud::panel_node(Display::None);
    node.max_width = Val::Vw(70.0);
    commands.spawn((
        demo_hud::centered_row(NOTE_BOX_TOP, Display::Flex),
        children![(
            NoteBoxPanel,
            node,
            background,
            demo_hud::text(String::new(), demo_hud::FONT_SIZE)
        )],
    ));
}

/// Fills and shows the note box while it is open, suppressed like the rest of the HUD in a run
/// whose screenshots must stay clean.
fn sync_note_ui(
    config: Res<EngineConfig>,
    note: Res<NoteBox>,
    mut boxes: Query<(&mut Text, &mut Node), With<NoteBoxPanel>>,
) {
    let Ok((mut text, mut node)) = boxes.single_mut() else {
        return;
    };
    if note.open && !demo_hud::hidden_for_this_run(&config) {
        let value = format!(
            "Note for {:02}\n{}_\nEnter: save  |  Esc: no note",
            note.capture_index.unwrap_or_default(),
            note.text
        );
        if text.as_str() != value {
            **text = value;
        }
        node.display = Display::Flex;
    } else {
        node.display = Display::None;
    }
}

// ---------------------------------------------------------------------------------------------
// Door crossings
// ---------------------------------------------------------------------------------------------

/// Appends a `notes.md` line the moment a door crossing happens, whether or not any capture ever
/// runs: "how many trips" is then in the file without anyone having counted.
///
/// The crossing's own message ([`DoorCrossed`]) carries the door and its destination's label but
/// not the space crossed *from*, so this keeps the previous frame's active space in a `Local` -
/// safe because nothing changes [`ActiveCell`] except a crossing (or the one-time `--start-shot`
/// placement, which fires no [`DoorCrossed`] to conflate it with).
fn record_door_crossings(
    mut crossed: MessageReader<DoorCrossed>,
    active: Res<ActiveCell>,
    mut run: ResMut<FieldNotesRun>,
    mut previous: Local<Option<SpaceSnapshot>>,
) {
    let current = SpaceSnapshot {
        worldspace_id: active.interior.is_none().then_some(active.worldspace_id),
        interior_cell_id: active.interior,
    };
    for event in crossed.read() {
        let from = previous.clone().unwrap_or_else(|| current.clone());
        let time = time_of_day(&local_stamp(SystemTime::now()));
        if let Err(error) = run.record_crossing(time, from, current.clone(), event.label.clone()) {
            warn!(target: "field_notes", "could not write notes.md: {error}");
        }
    }
    *previous = Some(current);
}

// ---------------------------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------------------------

fn start_run(mut commands: Commands, config: Res<EngineConfig>) {
    let run = FieldNotesRun::new(&config, SystemTime::now(), local_offset_minutes());
    info!(target: "field_notes", "F12 writes field notes to {}", run.directory.display());
    commands.insert_resource(run);
}

// ---------------------------------------------------------------------------------------------
// `--field-notes-test`
// ---------------------------------------------------------------------------------------------

/// `--field-notes-test`'s own state machine: wait for the first crossing, wait a few seconds more,
/// take a capture with the note "test", then exit - so a script can check a real capture without
/// a person at the keyboard (the acceptance run in the brief for this task).
#[derive(Resource, Default)]
struct FieldNotesTest {
    phase: TestPhase,
}

#[derive(Default)]
enum TestPhase {
    #[default]
    WaitingForCrossing,
    WaitingToCapture(f32),
    ClosingUp(f32),
}

#[allow(clippy::too_many_arguments)]
fn drive_field_notes_test(
    time: Res<Time>,
    mut crossed: MessageReader<DoorCrossed>,
    mut test: ResMut<FieldNotesTest>,
    mut note: ResMut<NoteBox>,
    mut run: ResMut<FieldNotesRun>,
    mut commands: Commands,
    active: Res<ActiveCell>,
    origin: Res<RenderOrigin>,
    camera: Query<(&Transform, &Projection), With<StreamingCamera>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: CamerasQuery,
    lights: LightsQuery,
    doors: DoorsQuery,
    frame_time: Res<FrameTimeWindow>,
    mut exit: MessageWriter<AppExit>,
) {
    let crossed_this_frame = crossed.read().next().is_some();
    match &mut test.phase {
        TestPhase::WaitingForCrossing => {
            if crossed_this_frame {
                test.phase = TestPhase::WaitingToCapture(0.0);
            }
        }
        TestPhase::WaitingToCapture(elapsed) => {
            *elapsed += time.delta_secs();
            if *elapsed < TEST_CAPTURE_DELAY_SECONDS {
                return;
            }
            if let Ok((transform, projection)) = camera.single() {
                if let Projection::Perspective(perspective) = projection {
                    let window = windows.iter().next();
                    let pose = capture_pose(transform, perspective, &active, &origin, window);
                    let state = state_snapshot(
                        &active,
                        run.crossings,
                        transform.translation,
                        &cameras,
                        &lights,
                        &doors,
                        &frame_time,
                    );
                    let size = window.map_or((1600, 900), |window| {
                        (window.width() as u32, window.height() as u32)
                    });
                    run.capture(&mut commands, pose, state, None, size);
                    if let Err(error) = run.close_note(Some("test".to_owned())) {
                        warn!(target: "field_notes", "--field-notes-test could not save the note: {error}");
                    }
                }
            } else {
                warn!(target: "field_notes", "--field-notes-test found no camera to capture");
            }
            note.open = false;
            note.pending_open = None;
            test.phase = TestPhase::ClosingUp(0.0);
        }
        TestPhase::ClosingUp(elapsed) => {
            *elapsed += time.delta_secs();
            if *elapsed >= TEST_EXIT_GRACE_SECONDS {
                exit.write(AppExit::Success);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_state() -> StateSnapshot {
        StateSnapshot {
            active_space: SpaceSnapshot {
                worldspace_id: Some(0x3c),
                interior_cell_id: None,
            },
            crossings_so_far: 2,
            lights: vec![LightSnapshot {
                entity: "3v1".to_owned(),
                name: Some("sun".to_owned()),
                is_portal_sun: false,
                shadows_enabled: true,
                illuminance: 10_000.0,
                render_layers: vec![0],
                cascade_shadow_bounds: Some(vec![50.0, 200.0, 800.0, 3_200.0]),
            }],
            cameras: vec![CameraSnapshot {
                entity: "1v1".to_owned(),
                is_active: true,
                render_layers: vec![0, 1],
                is_portal_camera: false,
                order: 0,
            }],
            doors_within_1000: vec![DoorSnapshot {
                form_id: "0x0001CBB0".to_owned(),
                state: "Closed".to_owned(),
                distance: 512.0,
            }],
            frame_time: FrameTimeSnapshot {
                average_ms: Some(16.7),
                frames_in_window: 58,
            },
            not_reached: vec!["nothing".to_owned()],
        }
    }

    #[test]
    fn the_run_folder_name_is_filesystem_safe_and_to_the_second() {
        // 2026-09-25T14:03:07.512Z (a leap-free date, chosen for a round number).
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_790_344_987_512);
        assert_eq!(run_folder_name(time, 0), "2026-09-25_14-03-07");
    }

    #[test]
    fn the_run_folder_name_is_on_the_local_clock() {
        // 14:03:07 UTC is 23:33:07 at +09:30 and 04:03:07 at -10:00.
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_790_344_987_512);
        assert_eq!(run_folder_name(time, 570), "2026-09-25_23-33-07");
        assert_eq!(run_folder_name(time, -600), "2026-09-25_04-03-07");
        // +10:30 crosses midnight into the next day, -14:30 back into the previous one.
        assert_eq!(run_folder_name(time, 630), "2026-09-26_00-33-07");
        assert_eq!(run_folder_name(time, -870), "2026-09-24_23-33-07");
    }

    #[test]
    fn a_local_stamp_is_shifted_by_the_offset_and_names_it() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_790_344_987_512);
        assert_eq!(rfc3339_local(time, 570), "2026-09-25T23:33:07.512+09:30");
        assert_eq!(rfc3339_local(time, 0), "2026-09-25T14:03:07.512+00:00");
        assert_eq!(rfc3339_local(time, -300), "2026-09-25T09:03:07.512-05:00");
        assert_eq!(rfc3339_local(time, -630), "2026-09-25T03:33:07.512-10:30");
        assert_eq!(
            time_of_day(&rfc3339_local(time, 570)),
            "23:33:07",
            "notes.md's times are the local clock"
        );
    }

    #[test]
    fn a_run_names_its_folder_and_header_on_the_local_clock() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_790_344_987_512);
        let run = FieldNotesRun::new(&EngineConfig::default(), time, 570);
        assert!(run.directory.ends_with("2026-09-25_23-33-07"));
        assert!(
            run.notes_md_text()
                .starts_with("# Field notes - 2026-09-25T23:33:07.512+09:30")
        );
    }

    #[test]
    fn the_local_offset_is_a_whole_real_time_zone() {
        // Whatever this machine's zone is, it is within UTC-12..UTC+14 and a multiple of 15 min.
        let offset = local_offset_minutes();
        assert!((-720..=840).contains(&offset), "{offset}");
        assert_eq!(offset % 15, 0, "{offset}");
    }

    #[test]
    fn time_of_day_takes_the_clock_out_of_an_rfc3339_stamp() {
        assert_eq!(time_of_day("2026-09-25T14:03:07.512Z"), "14:03:07");
    }

    #[test]
    fn format_space_prefers_the_interior_when_a_shot_has_neither_or_both() {
        assert_eq!(format_space(Some(0x3c), None), "exterior 0x3C");
        assert_eq!(format_space(None, Some(0x1EE62)), "interior 0x1EE62");
        assert_eq!(format_space(Some(0x3c), Some(0x1EE62)), "interior 0x1EE62");
        assert_eq!(format_space(None, None), "unknown space");
    }

    #[test]
    fn a_capture_line_names_the_index_time_space_note_and_picture() {
        let entry = NoteEntry::Capture {
            index: 3,
            time: "14:03:07".to_owned(),
            space: "exterior 0x3C".to_owned(),
            note: "the shadows vanished here".to_owned(),
        };
        assert_eq!(
            entry.line(),
            "03  14:03:07  exterior 0x3C  the shadows vanished here  ![](03.png)"
        );
    }

    #[test]
    fn a_crossing_line_names_the_time_the_spaces_and_the_running_count() {
        let entry = NoteEntry::Crossing {
            time: "14:04:00".to_owned(),
            from: "exterior 0x3C".to_owned(),
            to: "interior 0x1EE62".to_owned(),
            label: "Alftand Glacial Ruins".to_owned(),
            count: 1,
        };
        assert_eq!(
            entry.line(),
            "- 14:04:00  crossed from exterior 0x3C to interior 0x1EE62 (Alftand Glacial Ruins) \
             (crossing 1)"
        );
    }

    #[test]
    fn notes_md_carries_the_header_and_every_entry_in_order() {
        let mut run = FieldNotesRun::new(&EngineConfig::default(), SystemTime::UNIX_EPOCH, 0);
        run.started_at = "2026-09-25T14:00:00.000Z".to_owned();
        run.command_line = "engine --demo riverwood --walk".to_owned();
        run.binary_path = "target/quick/engine.exe".to_owned();
        run.binary_modified = "2026-09-25T13:00:00.000Z".to_owned();
        run.entries.push(NoteEntry::Crossing {
            time: "14:01:00".to_owned(),
            from: "exterior 0x3C".to_owned(),
            to: "interior 0x1EE62".to_owned(),
            label: String::new(),
            count: 1,
        });
        run.entries.push(NoteEntry::Capture {
            index: 1,
            time: "14:01:30".to_owned(),
            space: "interior 0x1EE62".to_owned(),
            note: "test".to_owned(),
        });
        let text = run.notes_md_text();
        assert!(text.starts_with("# Field notes - 2026-09-25T14:00:00.000Z"));
        assert!(text.contains("command: `engine --demo riverwood --walk`"));
        assert!(
            text.contains("engine: target/quick/engine.exe (modified 2026-09-25T13:00:00.000Z)")
        );
        let crossing_line = text
            .lines()
            .find(|line| line.contains("crossed from"))
            .unwrap();
        let capture_line = text.lines().find(|line| line.starts_with("01 ")).unwrap();
        assert!(
            text.find(crossing_line).unwrap() < text.find(capture_line).unwrap(),
            "the crossing happened before the capture, so its line comes first"
        );
    }

    #[test]
    fn the_shots_json_a_capture_writes_loads_back_with_the_right_names_and_poses() {
        let shots = ShotsFileOut {
            width: 1600,
            height: 900,
            shots: vec![
                CaptureShotEntry {
                    name: "01".to_owned(),
                    worldspace_id: Some(0x3c),
                    interior_cell_id: None,
                    position: [100.0, 200.0, 5.0],
                    yaw: 45.0,
                    pitch: -5.0,
                    hfov: 75.0,
                },
                CaptureShotEntry {
                    name: "02".to_owned(),
                    worldspace_id: None,
                    interior_cell_id: Some(0x1EE62),
                    position: [0.0, 0.0, 0.0],
                    yaw: 180.0,
                    pitch: 0.0,
                    hfov: 90.0,
                },
            ],
        };
        let text = serde_json::to_string_pretty(&shots).unwrap();
        let file = crate::shots::ShotsFile::from_json(&text).expect("a valid shots file");
        assert_eq!(file.width, 1600);
        assert_eq!(file.height, 900);
        let names: Vec<&str> = file.shots.iter().map(|shot| shot.name.as_str()).collect();
        assert_eq!(names, ["01", "02"]);
        assert_eq!(file.shots[0].worldspace_id, Some(0x3c));
        assert_eq!(file.shots[0].position, [100.0, 200.0, 5.0]);
        assert_eq!(file.shots[1].interior_cell_id, Some(0x1EE62));
        assert_eq!(file.shots[1].yaw, 180.0);
    }

    #[test]
    fn a_capture_record_serializes_the_pose_the_note_and_the_state() {
        let record = CaptureRecord {
            worldspace_id: Some(0x3c),
            interior_cell_id: None,
            position: [1.0, 2.0, 3.0],
            yaw: 10.0,
            pitch: -2.0,
            hfov: 75.0,
            saved_at: "2026-09-25T14:03:07.512Z".to_owned(),
            saved_at_local: "2026-09-25T23:33:07.512+09:30".to_owned(),
            note: None,
            state: sample_state(),
            graphics: Some(crate::graphics_settings::GraphicsSettings::bevy()),
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(value["note"], serde_json::Value::Null);
        assert_eq!(value["graphics"]["preset"], "bevy");
        assert_eq!(value["graphics"]["aa"], "smaa");
        assert_eq!(value["saved_at"], "2026-09-25T14:03:07.512Z");
        assert_eq!(value["saved_at_local"], "2026-09-25T23:33:07.512+09:30");
        assert_eq!(value["state"]["crossings_so_far"], 2);
        assert_eq!(value["state"]["active_space"]["worldspace_id"], 0x3c);
        assert_eq!(value["state"]["lights"][0]["is_portal_sun"], false);
        assert_eq!(
            value["state"]["lights"][0]["cascade_shadow_bounds"][2],
            800.0
        );
        assert_eq!(value["state"]["cameras"][0]["order"], 0);
        assert_eq!(
            value["state"]["doors_within_1000"][0]["form_id"],
            "0x0001CBB0"
        );
        assert_eq!(value["state"]["frame_time"]["frames_in_window"], 58);
        assert!(!value["state"]["not_reached"].as_array().unwrap().is_empty());
    }

    #[test]
    fn frame_time_window_keeps_only_the_last_second_and_averages_it() {
        let mut window = FrameTimeWindow::default();
        assert_eq!(window.average_ms(), None);
        window.push(0.0, 16.0);
        window.push(0.5, 20.0);
        // Past the one-second window from 0.0: it falls out, and the average is of what remains.
        window.push(1.2, 8.0);
        assert_eq!(window.frame_count(), 2);
        assert_eq!(window.average_ms(), Some(14.0));
    }

    #[test]
    fn the_saved_notice_names_the_capture_and_where_its_picture_landed() {
        assert_eq!(
            saved_notice_text(3, Path::new("local/captures/run/03.png")),
            "Saved 03 (local/captures/run/03.png)"
        );
    }

    #[test]
    fn the_arrival_notice_names_the_place_the_crossing_landed_in() {
        assert_eq!(
            arrival_notice_text(" Riverwood Sven's House \n"),
            "Riverwood Sven's House"
        );
    }

    #[test]
    fn the_note_box_types_backspaces_saves_and_skips() {
        let mut note = NoteBox::default();
        note.push_char('h');
        note.push_char('i');
        note.backspace();
        note.push_char('!');
        assert_eq!(note.text, "h!");
        // A control character (as `Enter`/`Backspace` would arrive if mishandled) is not text.
        note.push_char('\n');
        assert_eq!(note.text, "h!");
        assert_eq!(note.take(), Some("h!".to_owned()));
        assert_eq!(note.text, "");
        // Enter with nothing typed saves no note.
        assert_eq!(note.take(), None);
        // Esc always throws the draft away, whatever was typed.
        note.push_char('x');
        note.cancel();
        assert_eq!(note.text, "");
    }

    #[test]
    fn closing_a_capture_with_a_note_rewrites_its_json_and_appends_a_notes_md_line() {
        // `capture` itself needs `Commands` (to spawn the screenshot request), which needs a
        // `World` a plain unit test does not have; this exercises what happens once the note box
        // closes, which is the part with no `Commands` in it - `close_note` rewrites `NN.json`
        // and appends the `notes.md` line, exactly as `type_note` calls it.
        let directory =
            std::env::temp_dir().join(format!("field-notes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let mut run = FieldNotesRun::new(&EngineConfig::default(), SystemTime::UNIX_EPOCH, 0);
        run.directory = directory.clone();
        run.ensure_dir().expect("a temp directory");
        // A crossing before any capture is kept, but writes no `notes.md` yet.
        run.record_crossing(
            "14:02:00".to_owned(),
            SpaceSnapshot {
                worldspace_id: Some(0x3c),
                interior_cell_id: None,
            },
            SpaceSnapshot {
                worldspace_id: None,
                interior_cell_id: Some(0x1cb84),
            },
            "RiverwoodSvensHouse".to_owned(),
        )
        .unwrap();
        assert!(!run.directory.join("notes.md").exists());
        run.captures.push(CaptureShotEntry {
            name: "01".to_owned(),
            worldspace_id: Some(0x3c),
            interior_cell_id: None,
            position: [1.0, 2.0, 3.0],
            yaw: 0.0,
            pitch: 0.0,
            hfov: 75.0,
        });
        let record = CaptureRecord {
            worldspace_id: Some(0x3c),
            interior_cell_id: None,
            position: [1.0, 2.0, 3.0],
            yaw: 0.0,
            pitch: 0.0,
            hfov: 75.0,
            saved_at: "2026-09-25T14:03:07.512Z".to_owned(),
            saved_at_local: "2026-09-25T23:33:07.512+09:30".to_owned(),
            note: None,
            state: sample_state(),
            graphics: None,
        };
        let path = run.capture_path(1, "json");
        std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap()).unwrap();
        run.pending = Some(PendingCapture {
            index: 1,
            path,
            record,
        });

        run.close_note(Some("the shadows vanished here".to_owned()))
            .expect("closing the note writes NN.json and notes.md");

        let json_text = std::fs::read_to_string(run.capture_path(1, "json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_text).unwrap();
        assert_eq!(value["note"], "the shadows vanished here");

        let notes = std::fs::read_to_string(run.directory.join("notes.md")).unwrap();
        assert!(
            notes.contains("01  23:33:07  exterior 0x3C  the shadows vanished here"),
            "the capture line shows the local time (saved_at_local), not UTC"
        );
        assert!(
            notes.contains("crossing 1"),
            "the crossing before the capture is listed"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }
}
