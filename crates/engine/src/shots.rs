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
//! # A shot that looks through an open doorway
//!
//! A shot may name a load door with `open_door` (a reference FormID, written as a hex string). Such
//! a shot opens that door through the engine's own door machinery before it settles - the
//! [`OpenDoor`] message a player's `E` writes and nothing else, never a second way to open a door -
//! and then waits for the doorway to be one a player would see: the door's state is open, its
//! destination is streamed in, the portal is drawing through it, and the door's own `Open` clip has
//! finished so the leaf stands where the swing left it. Only then does the shot settle and capture.
//! A door that never gets there fails **that shot** - the reason is in `shots.log` and the run
//! exits non-zero - while the shots after it are still rendered, because a shots file is usually a
//! sweep and one bad pose should not cost the others.
//!
//! The door is closed again once the shot is on disk, so a shot's frame does not depend on the
//! shots before it. A door whose model has no clip of its own cannot be closed by the engine at all
//! ([`crate::door_animation`] has no swing to play back) and stays open, which `shots.log` says. A
//! shot that names no door never enters any of this: no door is open in a run where none is asked
//! for, so its settle, its frames and its bytes are what they always were.
//!
//! [`arrival_camera_rotation`]: crate::transition::arrival_camera_rotation

use crate::{
    config::grid_of,
    doors::{DoorAnchor, DoorState, LoadDoor},
    portal::{MIN_PORTAL_DOOR_DISTANCE, PortalQuad, PortalTexture},
    streaming::{ActiveCell, RenderOrigin, StreamingMetrics, StreamingWorld},
    transition::{
        OpenDoor, SpaceTarget, destination_is_resident, distance_in_front_of_door, door_is_open,
        heading_to_rotation, source_doorway_centre, source_doorway_frame, switch_space,
    },
    world::{
        components::{ExteriorCellGrid, StreamingCamera},
        database::CellKey,
    },
};
use bevy::{
    animation::graph::AnimationGraphHandle,
    camera::RenderTarget,
    ecs::system::SystemParam,
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::{PrimaryWindow, WindowResolution},
};
use serde::{Deserialize, Deserializer};
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

/// How long a shot that names a door waits for it to open, for its destination to stream in and for
/// the portal to draw through it, before that shot fails and the run moves on to the next one.
///
/// The wait is the settle timeout's order of magnitude and for the same reason: a cell load, an
/// asset load and a door swing are all in it. It is the whole sequence that is bounded, not each
/// step, so a door that is found late still has the rest of the window to open in.
pub const DOOR_TIMEOUT_SECONDS: f32 = 30.0;

/// Frames of a quiet view after which a door whose own animation has not resolved is asked to open
/// anyway.
///
/// The question this settles is whether the door will swing or open as a static leaf: `E` on a load
/// door whose clips have not arrived - or which has none at all - opens the doorway in one frame,
/// with no leaf to draw and no way back, and a doorway shot of such a door is not the shot that was
/// asked for. A quiet view is the evidence that the clips are not still on their way: the door's
/// model is one of the references streaming counts as pending, so a view with nothing pending has
/// the model in hand.
///
/// The count is deliberately the whole of [`crate::door_animation`]'s own patience with a model that
/// has arrived and a scene that has not (`SCENE_WAIT_FRAMES`, 120 frames, on the same frame clock):
/// by the time this has passed, the engine has either given the door its clips or decided it has
/// none, and asking it then is asking the door a player would get. A shorter wait asks doors the
/// engine was still working on, and opens them as static leaves - which is what a run with a
/// two-second wait did to the Riverwood Trader's door on a cold cell load.
pub const DOOR_QUIET_FRAMES: u32 = 120;

/// How long a shot waits for the door it opened to close again before it moves on with a warning in
/// the log. The close belongs to the shot before, not to this one, and a door that will not close
/// costs the shots after it their independence rather than their render.
pub const DOOR_CLOSE_TIMEOUT_SECONDS: f32 = 10.0;

/// How far from a door reference the doorway quad may stand and still count as that door's own
/// ([`portal_renders_through_door`]).
///
/// The quad stands in the doorway the door's own model measures, so its distance from the reference
/// is the doorway's centre offset - tens of units for a house door, a couple of hundred for a
/// Dwemer gate - while no two doors of a route stand within hundreds of units of each other.
const QUAD_DOOR_MAX_DISTANCE: f32 = 512.0;

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
    /// A load door to open before this shot is taken, as a reference FormID: the shot is then of
    /// the doorway, from outside the open door, with the portal rendering the space beyond it.
    ///
    /// The engine opens it through the same [`OpenDoor`] message a player's `E` writes and waits
    /// for the doorway to be one a player would see; see the module documentation. `None` - the
    /// field absent - is a shot of a camera pose and nothing else, which is every shot a files
    /// written before this field existed contains.
    #[serde(default)]
    pub open_door: Option<HexFormId>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// A FormID as a shots file writes one: a hex string with a `0x` prefix, like `"0x0001CBB0"`, which
/// is how the project writes a reference FormID in JSON (the numbers `skyrim_world.db` stores,
/// `door_links.ref_id` among them).
///
/// The database's ids are the same numbers; this type only carries the file's notation, and rejects
/// anything else at load rather than reading a decimal number, a bare hex string or a mistyped id
/// as some other door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexFormId(pub u32);

impl<'de> Deserialize<'de> for HexFormId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let text = String::deserialize(deserializer)?;
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .ok_or_else(|| {
                D::Error::custom(format!(
                    "a FormID is a hex string like \"0x0001CBB0\", not {text:?}"
                ))
            })?;
        u32::from_str_radix(digits, 16).map(Self).map_err(|error| {
            D::Error::custom(format!(
                "a FormID is a hex string like \"0x0001CBB0\": {text:?} is not one ({error})"
            ))
        })
    }
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
    // The yaw term is the shared heading rotation ([`heading_to_rotation`]), so a shot's camera
    // turns the same way an arrival camera or a door frame does; the pitch turns that same
    // forward down by `pitch` on top of it.
    heading_to_rotation(yaw_degrees.to_radians())
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
    /// Whether that cell's request failed and is not repeated (its landscape failed validation,
    /// say): it will never be resident, so the view is photographed with what did load instead of
    /// waiting out [`SETTLE_TIMEOUT_SECONDS`].
    pub space_failed: bool,
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
        (self.space_resident || self.space_failed)
            && self.loading_cells == 0
            && self.active_requests == 0
            && self.pending_asset_instances == 0
            && self.pending_surface_instances == 0
    }

    /// The pending work, for the log line of a shot that never settled.
    pub fn describe(&self) -> String {
        format!(
            "cell_resident={} cell_failed={} loading_cells={} active_requests={} pending_assets={} pending_surfaces={}",
            self.space_resident,
            self.space_failed,
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

// ---------------------------------------------------------------------------------------------
// The door sequence of a shot that names a door
// ---------------------------------------------------------------------------------------------

/// The facts a door shot reads each frame: everything its decision ([`door_step`]) is made of, so
/// that the decision is a pure function of them rather than of the world.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DoorFacts {
    /// The door reference is spawned in a cell that is resident **and carries its `DoorState`**:
    /// there is a door to ask, and the message asking it would be read by something.
    pub door_spawned: bool,
    /// The door may be asked to open: its own animation has resolved (its scene carries the graph
    /// [`crate::door_animation`] plays its clips with), or the view it stands in has had nothing
    /// left to load for [`DOOR_QUIET_FRAMES`] frames - and a door whose model carries no clip is
    /// never going to have one.
    pub activation_ready: bool,
    /// [`OpenDoor`] has been written for it: the door has been asked.
    pub asked: bool,
    /// [`DoorState::is_open`]: the doorway is a way through.
    pub open: bool,
    /// The space the door leads into is streamed in, so the portal has something to show.
    pub destination_resident: bool,
    /// The portal is drawing through this door's doorway this frame: the quad is up, it stands in
    /// this door's opening rather than another door's, and the portal camera is rendering.
    pub portal_drawn: bool,
    /// The door's own swing has finished, so the leaf stands where a player would see it: the clip
    /// that moves it has run to its end, or the door has no clip and no leaf to move.
    pub swing_finished: bool,
    /// The door's own clip is the thing that moves its leaf ([`DoorState::Opening`], `Closing` or
    /// `Open { animated: true }`). False for a door that opened with no clip of its own, whose
    /// whole model the portal hides instead of drawing a swung leaf.
    pub animated: bool,
    /// The camera stands in front of the door's own plane, near enough that the portal will render
    /// through the doorway (`MIN_PORTAL_DOOR_DISTANCE`). A shot taken inside the doorway, or from
    /// behind it, can never be a view through it: this says so rather than leaving the shot to time
    /// out on the portal alone.
    pub camera_in_front: bool,
}

impl DoorFacts {
    /// Whether the shot may settle and be photographed: the doorway is open, the space beyond it is
    /// streamed in, the portal is drawing through it, and the leaf has stopped moving.
    pub fn doorway_ready(&self) -> bool {
        self.door_spawned
            && self.open
            && self.destination_resident
            && self.portal_drawn
            && self.swing_finished
            && self.camera_in_front
    }

    /// The facts, for the log line of a door shot that never got there.
    pub fn describe(&self) -> String {
        format!(
            "door_spawned={} activation_ready={} asked={} open={} destination_resident={} \
             portal_drawn={} swing_finished={} animated={} camera_in_front={}",
            self.door_spawned,
            self.activation_ready,
            self.asked,
            self.open,
            self.destination_resident,
            self.portal_drawn,
            self.swing_finished,
            self.animated,
            self.camera_in_front
        )
    }

    /// The same line with the view's own counts on the end, which is what `activation_ready` is
    /// measured against and what a shot that never got past the ask has to show.
    pub fn describe_with(&self, counts: &SettleCounts) -> String {
        format!("{} view:{}", self.describe(), counts.describe())
    }
}

/// What a door shot does with the facts of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoorStep {
    /// Something the doorway needs is still missing: ask the door if it has not been asked, and
    /// look again next frame.
    Waiting,
    /// The doorway is one a player would see: settle the view and photograph it.
    Ready,
    /// The sequence has waited `timeout` seconds and is still missing something.
    TimedOut,
}

/// The door sequence's rule: ready the moment the doorway is complete, timed out once `waited`
/// seconds have passed without it, and waiting in between.
pub fn door_step(facts: &DoorFacts, waited: f32, timeout: f32) -> DoorStep {
    if facts.doorway_ready() {
        DoorStep::Ready
    } else if waited >= timeout {
        DoorStep::TimedOut
    } else {
        DoorStep::Waiting
    }
}

/// Where a shot goes once its camera is posed: into the door sequence when it names a door, and
/// straight to the settle when it does not - which is the run every shots file written before
/// `open_door` existed takes, frame for frame.
fn phase_after_move(shot: &Shot) -> Phase {
    if shot.open_door.is_some() {
        Phase::Door
    } else {
        Phase::Settle
    }
}

/// The door sequence of the shot being rendered: the door it names, the reference the sequence
/// found for it, and what it has asked that reference for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoorRun {
    /// The door reference's FormID, from the shot.
    pub ref_id: u32,
    /// The door reference the sequence found in the streamed cells, once it has found it: the door
    /// to ask, to watch and to close again. Looked up afresh every frame, because a cell that
    /// unloads takes its doors with it.
    pub door: Option<Entity>,
    /// The door has been asked for all it needs: either [`OpenDoor`] was written for it, or it was
    /// already open when the sequence found it and had nothing to ask for.
    asked: bool,
    /// [`OpenDoor`] has been written for an open door: it has been asked to close again.
    asked_close: bool,
}

impl DoorRun {
    fn new(ref_id: u32) -> Self {
        Self {
            ref_id,
            door: None,
            asked: false,
            asked_close: false,
        }
    }
}

/// Everything a door shot reads a frame through: the load doors in the streamed cells, the
/// animation [`crate::door_animation`] resolved for them, the portal's quad and camera, and the
/// message that opens a door.
///
/// They travel as one because a system function takes at most sixteen parameters and [`run_shots`]
/// is at that limit.
/// A load door as a door shot reads it: its placement, its link, the state its animation owns and
/// the doorway anchor the portal measures it by.
type DoorRow = (
    Entity,
    &'static GlobalTransform,
    &'static LoadDoor,
    Option<&'static DoorState>,
    Option<&'static DoorAnchor>,
);

#[derive(SystemParam)]
struct DoorWorld<'w, 's> {
    /// Every load door of the streamed cells, with the state its animation owns.
    doors: Query<'w, 's, DoorRow>,
    children: Query<'w, 's, &'static Children>,
    /// The scene roots whose animation has resolved: the loader's [`AnimationPlayer`] with the
    /// graph [`crate::door_animation`] attaches when it hands a door its clips.
    players: Query<'w, 's, &'static AnimationPlayer, With<AnimationGraphHandle>>,
    /// The doorway quad, which stands in the doorway of the door the portal picked
    /// ([`crate::portal`]).
    quad: Query<'w, 's, (&'static GlobalTransform, &'static Visibility), With<PortalQuad>>,
    cameras: Query<'w, 's, (&'static Camera, &'static RenderTarget)>,
    /// The portal target, which is what says which of the cameras is the portal's.
    portal_texture: Option<Res<'w, PortalTexture>>,
    /// The message a player's `E` writes: how a door is asked to open, and to close again.
    open_door: MessageWriter<'w, OpenDoor>,
}

impl DoorWorld<'_, '_> {
    /// The entity of the load door with this FormID, or `None` while the cells that might hold it
    /// are not resident. A door that is not streamed in cannot be opened, watched or closed.
    fn door_entity(&self, ref_id: u32) -> Option<Entity> {
        self.doors
            .iter()
            .find(|(_, _, door, ..)| door.ref_id == ref_id)
            .map(|(entity, ..)| entity)
    }

    /// The door's own animation: the [`AnimationPlayer`] the glTF loader put on the door's scene
    /// root, which [`crate::door_animation`] gives an [`AnimationGraphHandle`] when it has resolved
    /// the door's clips. `None` means the door's animation has not resolved yet - or that its model
    /// carries no clip at all, which is the one case a door cannot be asked to swing.
    fn door_player(&self, door: Entity) -> Option<&AnimationPlayer> {
        std::iter::once(door)
            .chain(self.children.iter_descendants(door))
            .find_map(|node| self.players.get(node).ok())
    }

    /// Asks a door to open, or an open one to close: exactly the message a player's `E` writes, and
    /// the only way this run changes a door.
    fn ask(&mut self, door: Entity) {
        self.open_door.write(OpenDoor { door });
    }

    /// What a door shot can see about its door this frame, read from the world the frame runs in.
    /// `camera_position` is where the shot's camera stands in the same render space the door's
    /// `GlobalTransform` is in, which is what says whether the portal can reach the doorway at all.
    fn facts(
        &self,
        door: &DoorRun,
        camera_position: Vec3,
        streaming: Option<&StreamingWorld>,
        counts: &SettleCounts,
    ) -> DoorFacts {
        let spawned = door.door.and_then(|entity| self.doors.get(entity).ok());
        let (position, load_door, state, anchor) = match spawned {
            Some((_, global, load_door, state, anchor)) => (
                global.translation(),
                Some(load_door),
                state.copied(),
                anchor,
            ),
            None => (Vec3::ZERO, None, None, None),
        };
        let player = door.door.and_then(|entity| self.door_player(entity));
        DoorFacts {
            // A door is there to be asked once it carries its state: `activate_doors` reads
            // `DoorState`, and a message written for a reference without one is read by nobody.
            door_spawned: state.is_some(),
            // The graph is the door's animation resolved; a quiet view is the evidence that no
            // clip is still on its way, and the case left over is a model that has none.
            activation_ready: player.is_some()
                || (state.is_some()
                    && counts.is_quiet()
                    && counts.quiet_frames >= DOOR_QUIET_FRAMES),
            asked: door.asked,
            open: door_is_open(state.as_ref()),
            destination_resident: load_door.is_some_and(|door| {
                streaming.is_some_and(|streaming| {
                    destination_is_resident(&door.destination, anchor, streaming)
                })
            }),
            portal_drawn: portal_renders_through_door(
                position,
                self.quad.single().ok().map(|(transform, visibility)| {
                    (
                        transform.translation(),
                        !matches!(visibility, Visibility::Hidden),
                    )
                }),
                portal_camera_active(
                    self.portal_texture.as_ref().map(|texture| &texture.0),
                    &self.cameras,
                ),
            ),
            // A door with no player has no leaf that can move: its model carries no clip, or its
            // animation has not resolved - and in both cases nothing is swinging.
            swing_finished: player.is_none_or(AnimationPlayer::all_finished),
            animated: matches!(
                state,
                Some(DoorState::Opening | DoorState::Closing | DoorState::Open { animated: true })
            ),
            // The portal's own test of the camera's side of the doorway: nearer than
            // `MIN_PORTAL_DOOR_DISTANCE` - inside the doorway, or behind it - the window has no
            // content, and the portal drops the door (nothing says so better than the rule it
            // shares with [`crate::portal::select_portal_door`]), measured from the doorway the
            // portal measures from: the doorway anchor's where the door has one.
            camera_in_front: spawned.is_some_and(|(_, global, load_door, _, anchor)| {
                let pivot = match anchor {
                    Some(anchor) => source_doorway_centre(
                        global.translation(),
                        global.rotation(),
                        global.scale(),
                        anchor,
                    ),
                    None => global.translation(),
                };
                distance_in_front_of_door(
                    pivot,
                    source_doorway_frame(global.rotation(), load_door.outward, anchor),
                    camera_position,
                ) >= MIN_PORTAL_DOOR_DISTANCE
            }),
        }
    }
}

/// Whether the portal is drawing through this door's doorway: the quad is drawn, it stands in this
/// door's opening rather than in another door's, and the portal camera is rendering into the image
/// the quad samples.
///
/// The quad's distance from the door reference is what ties the portal to the door the shot named.
/// `PortalState::open_door` - the portal's own answer, which would say it exactly - is private to
/// [`crate::portal`]; this is the same fact read from the geometry the portal writes: the quad is
/// placed in the doorway of the door it picked, and no two doors are near enough for one to pass
/// for the other's ([`QUAD_DOOR_MAX_DISTANCE`]).
fn portal_renders_through_door(
    door_position: Vec3,
    quad: Option<(Vec3, bool)>,
    portal_camera_active: bool,
) -> bool {
    let Some((quad_position, quad_drawn)) = quad else {
        return false;
    };
    quad_drawn
        && portal_camera_active
        && quad_position.distance(door_position) <= QUAD_DOOR_MAX_DISTANCE
}

/// Whether the camera rendering into the portal target is drawing: the doorway image the quad
/// samples is the one the portal camera produced this frame.
///
/// The camera is found by its render target rather than by position in a list, so the water
/// reflection camera - the other camera of an engine run that draws into an image - cannot pass for
/// it.
fn portal_camera_active(
    portal_texture: Option<&Handle<Image>>,
    cameras: &Query<(&Camera, &RenderTarget)>,
) -> bool {
    let Some(texture) = portal_texture else {
        return false;
    };
    cameras.iter().any(|(camera, target)| {
        camera.is_active && matches!(target, RenderTarget::Image(image) if image.handle == *texture)
    })
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

/// Runs the shots sequence. Added by [`PortalPlugin`](crate::portal::PortalPlugin) when `--shots`
/// is given, which is the resource its run is handed over in; `StreamingPlugin` has to be present,
/// because a shot moves the camera through [`switch_space`] and waits on streaming.
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
    /// Seconds in the current phase; the settle, capture and door timeouts read it.
    timer: f32,
    /// Seconds the current shot's view took to settle, for the log.
    settle_seconds: f32,
    quiet_frames: u32,
    resident_meshes: usize,
    timed_out: bool,
    /// The door sequence of the shot being rendered: the door it names, and how far the sequence
    /// has got with it. `None` for a shot that names no door - the run a file without `open_door`
    /// takes, in which no door is ever asked, watched or closed.
    door: Option<DoorRun>,
    log: String,
    written: bool,
    failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Move the camera into the shot's space and pose it, then settle.
    Move,
    /// Open the door the shot names, if any, and wait for the doorway to be one a player sees.
    Door,
    /// Wait for the view to settle (or time out).
    Settle,
    /// The screenshot has been requested; wait for it on disk.
    Capture,
    /// Close the door the shot opened again, so the shots after it start from the world the file
    /// implies.
    CloseDoor,
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
            door: None,
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

    /// The path to show for one of this run's own output files, in `shots.log` and in the engine
    /// log: relative to `output_dir` rather than absolute, so a log or note never carries the
    /// user's file-system layout. `shot_path` and `log_path` only ever hand out children of
    /// `output_dir`, so this is ordinarily just the file name; the absolute path is the fallback
    /// for anything else, rather than a path with no name at all.
    fn relative_to_output<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.output_dir).unwrap_or(path)
    }

    /// Records a line of another module's in `shots.log` and in the engine log: the graphics
    /// settings the run renders with (`crate::graphics_settings`).
    pub fn note_line(&mut self, line: impl AsRef<str>) {
        self.note(line);
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

    /// Starts the settle for the shot being rendered, with the quiet window counting from scratch:
    /// for a door shot it counts from the doorway being open rather than from the frames the door
    /// sequence spent waiting for it.
    fn enter_settle(&mut self) {
        self.quiet_frames = 0;
        self.enter(Phase::Settle);
    }

    /// Moves on to the next shot in the file, or to the end when this was the last one. A shot
    /// that was given up on has been marked failed by its caller ([`ShotsRun::fail`]) before it
    /// gets here.
    fn next_shot(&mut self) {
        self.shot += 1;
        if self.shot < self.file.shots.len() {
            self.enter(Phase::Move);
        } else {
            self.enter(Phase::Done);
        }
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
    mut world: DoorWorld,
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
            // The door this shot names, if any: the sequence is about to open it. A shot that
            // names none has `None` here and never leaves the settle path.
            run.door = shot.open_door.map(|ref_id| DoorRun::new(ref_id.0));
            run.quiet_frames = 0;
            run.timed_out = false;
            run.settle_seconds = 0.0;
            run.enter(phase_after_move(&shot));
        }
        Phase::Door => {
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
            // Re-assert the pose every frame, as the settle does: a render-origin rebase moves the
            // camera in render space, and the Creation position is what has to be photographed.
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
            let Some(wanted) = shot.open_door else {
                // A shot that names no door never enters this phase ([`phase_after_move`]).
                run.enter_settle();
                return;
            };
            run.timer += delta;
            let mut door = run.door.unwrap_or(DoorRun::new(wanted.0));
            if let Some(entity) = world.door_entity(door.ref_id) {
                door.door = Some(entity);
            }
            // The view's own counts, over the same rule the settle uses: the door is asked once
            // the door's model - and everything else the view asked for - has loaded.
            let counts = settle_counts(
                shot.space_key(),
                run.quiet_frames,
                streaming.as_deref(),
                metrics.as_deref(),
            );
            run.quiet_frames = if counts.is_quiet() {
                run.quiet_frames.saturating_add(1)
            } else {
                0
            };
            let facts = world.facts(&door, transform.translation, streaming.as_deref(), &counts);
            // Ask the door once, as soon as it is there and can swing: asking one whose clips have
            // not resolved opens it the static way, with no leaf and no way back. An open door is
            // never asked - the message is the toggle a player's `E` is, so asking one that is
            // already open would close it - and a door another shot left open, or a shot whose own
            // `OpenDoor` landed a frame late, needs no asking at all.
            if !door.asked
                && facts.door_spawned
                && facts.activation_ready
                && let Some(entity) = door.door
            {
                if facts.open {
                    door.asked = true;
                } else {
                    world.ask(entity);
                    door.asked = true;
                    run.note(format!(
                        "shot \"{}\" opening door {:08X}",
                        shot.name, door.ref_id
                    ));
                }
            }
            run.door = Some(door);
            match door_step(&facts, run.timer, DOOR_TIMEOUT_SECONDS) {
                DoorStep::Waiting => {}
                DoorStep::Ready => {
                    run.note(format!(
                        "shot \"{}\" door {:08X} open, portal rendering through the doorway{}",
                        shot.name,
                        door.ref_id,
                        if facts.animated {
                            ""
                        } else {
                            " (the door has no clip of its own: the doorway is an opening with no \
                             leaf in it)"
                        }
                    ));
                    run.enter_settle();
                }
                DoorStep::TimedOut => {
                    run.fail(format!(
                        "shot \"{}\": door {:08X} is not a doorway a player could see through \
                         within {DOOR_TIMEOUT_SECONDS:.0} s ({})",
                        shot.name,
                        door.ref_id,
                        facts.describe_with(&counts)
                    ));
                    // The shot is not rendered, but the door may still be open: the close phase has
                    // the same job it has after a shot that worked, and then moves on.
                    run.enter(Phase::CloseDoor);
                }
            }
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
                shot.space_key(),
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
                let logged_path = run.relative_to_output(&path).display().to_string();
                run.note(format!("screenshot {logged_path}"));
                // A PNG left by an earlier run must not pass for this shot's: with it out of the
                // way, the file appearing is proof that this screenshot reached the disk, which is
                // what the capture wait (and the exit after the last shot) relies on.
                if let Err(error) = std::fs::remove_file(&path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(
                        target: "shots",
                        "could not remove the previous {logged_path}: {error}; the shot may wait \
                         for that file instead of this screenshot"
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
                if run.door.is_some() {
                    // Put the door this shot opened back before the next shot: see
                    // [`Phase::CloseDoor`]. `run.shot` still names this shot, which is the one the
                    // close belongs to.
                    run.enter(Phase::CloseDoor);
                } else {
                    run.next_shot();
                }
            } else if run.timer >= CAPTURE_TIMEOUT_SECONDS {
                let line = log_line(
                    &shot.name,
                    run.settle_seconds,
                    run.timed_out,
                    run.resident_meshes,
                );
                let logged_path = run.relative_to_output(&path).display().to_string();
                run.note(format!(
                    "{line} FAILED: no screenshot at {logged_path} after \
                     {CAPTURE_TIMEOUT_SECONDS:.0} s"
                ));
                run.failed = true;
                if run.door.is_some() {
                    run.enter(Phase::CloseDoor);
                } else {
                    run.enter(Phase::Done);
                }
            }
        }
        Phase::CloseDoor => {
            let Some(shot) = run.file.shots.get(run.shot).cloned() else {
                run.enter(Phase::Done);
                return;
            };
            let Some(door) = run.door else {
                // A shot that named no door, or one whose door sequence never ran: nothing was
                // opened, so there is nothing to put back.
                run.next_shot();
                return;
            };
            let state = door
                .door
                .and_then(|entity| world.doors.get(entity).ok())
                .and_then(|(_, _, _, state, _)| state.copied());
            let Some(state) = state else {
                // The door is not in the streamed cells any more: it went with its cell, and a
                // door that is not there is not open in front of anybody.
                if door.asked {
                    run.note(format!(
                        "shot \"{}\" door {:08X} unloaded before it could be closed",
                        shot.name, door.ref_id
                    ));
                }
                run.next_shot();
                return;
            };
            run.timer += delta;
            match state {
                // The engine cannot close this one: a door with no clip of its own has no swing to
                // play back, and `door_animation` has no path out of `Open { animated: false }`.
                // It stayed open for the same reason when a player opened it by hand.
                DoorState::Open { animated: false } => {
                    run.note(format!(
                        "shot \"{}\" door {:08X} has no clip of its own and cannot be closed; it \
                         stays open",
                        shot.name, door.ref_id
                    ));
                    run.next_shot();
                }
                DoorState::Closed => {
                    if door.asked_close {
                        run.note(format!(
                            "shot \"{}\" closed door {:08X}",
                            shot.name, door.ref_id
                        ));
                    }
                    run.next_shot();
                }
                // An open animated door, or one already on its way back: ask it to close once, and
                // wait for the clip to finish. Asking is `E` on an open door - the same message
                // that opened it - and a door that is still `Opening` cannot be told anything yet.
                DoorState::Open { animated: true } | DoorState::Opening | DoorState::Closing => {
                    let mut door = door;
                    if matches!(state, DoorState::Open { animated: true })
                        && !door.asked_close
                        && let Some(entity) = door.door
                    {
                        world.ask(entity);
                        door.asked_close = true;
                        run.note(format!(
                            "shot \"{}\" closing door {:08X}",
                            shot.name, door.ref_id
                        ));
                    }
                    run.door = Some(door);
                    if run.timer >= DOOR_CLOSE_TIMEOUT_SECONDS {
                        run.note(format!(
                            "shot \"{}\" warning: door {:08X} did not close within \
                             {DOOR_CLOSE_TIMEOUT_SECONDS:.0} s; the shots after this one may differ",
                            shot.name, door.ref_id
                        ));
                        run.next_shot();
                    }
                }
            }
        }
        Phase::Done => {
            if !run.written {
                run.written = true;
                let path = run.log_path();
                let logged_path = run.relative_to_output(&path).display().to_string();
                if let Err(error) = std::fs::write(&path, &run.log) {
                    error!(target: "shots", "could not write {logged_path}: {error}");
                    run.failed = true;
                } else {
                    info!(target: "shots", "shots log written to {logged_path}");
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

/// What the streaming code says is still pending for a view of the cell `key`.
///
/// The rule is shared: the shot runner reads it to decide when to photograph a pose, the door
/// sequence to decide when a view is ready to be asked to open its door, and the demo tour's own
/// settle to decide when a place has streamed in (`crate::demo_tour`).
pub(crate) fn settle_counts(
    key: Option<CellKey>,
    quiet_frames: u32,
    streaming: Option<&StreamingWorld>,
    metrics: Option<&StreamingMetrics>,
) -> SettleCounts {
    SettleCounts {
        space_resident: key
            .is_some_and(|key| streaming.is_some_and(|world| world.is_resident(&key))),
        space_failed: key.is_some_and(|key| streaming.is_some_and(|world| world.has_failed(&key))),
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

    /// A shot that names a load door: the field the door sequence runs on.
    fn door_example() -> String {
        r#"{"width": 1400, "height": 1050, "shots": [
            {"name": "sven-door-square", "worldspace_id": 60,
             "position": [20558.4, -46062.6, -2.1], "yaw": 161.5, "pitch": 5.2, "hfov": 75.0,
             "open_door": "0x0001CBB0"}
        ]}"#
        .to_owned()
    }

    /// Two shots, the first of them naming a door: enough to walk the run past a failed one.
    fn two_door_example() -> String {
        r#"{"width": 1400, "height": 1050, "shots": [
            {"name": "first", "worldspace_id": 60, "position": [0.0, 0.0, 0.0],
             "yaw": 0.0, "pitch": 0.0, "hfov": 75.0, "open_door": "0x0001CBB0"},
            {"name": "second", "worldspace_id": 60, "position": [0.0, 0.0, 0.0],
             "yaw": 0.0, "pitch": 0.0, "hfov": 75.0}
        ]}"#
        .to_owned()
    }

    #[test]
    fn the_open_door_field_is_optional_and_parses_a_hex_form_id() {
        let file = ShotsFile::from_json(&door_example()).unwrap();
        assert_eq!(file.shots[0].open_door, Some(HexFormId(0x0001_CBB0)));
        assert_eq!(file.shots[0].space(), Some(SpaceTarget::Exterior(60)));

        // The case of the hex digits and of the prefix do not matter.
        for text in ["0x1cbb0", "0X1CBB0", "0x0001CBB0"] {
            let json = door_example().replace("0x0001CBB0", text);
            assert_eq!(
                ShotsFile::from_json(&json).unwrap().shots[0].open_door,
                Some(HexFormId(0x0001_CBB0)),
                "{text}"
            );
        }

        // Absent, it is a shot of a camera pose and nothing else - which is every shot of every
        // file written before the field existed.
        let file = ShotsFile::from_json(DESIGN_EXAMPLE).unwrap();
        assert_eq!(file.shots[0].open_door, None);
        let file = ShotsFile::from_json(&interior_example()).unwrap();
        assert_eq!(file.shots[0].open_door, None);
    }

    /// The field a file without it sees is `None`: the shot the design example parses to is the
    /// shot it parsed to before, field for field. Comparisons against earlier renders depend on it.
    #[test]
    fn a_shot_without_open_door_is_the_shot_it_always_was() {
        let file = ShotsFile::from_json(DESIGN_EXAMPLE).unwrap();
        assert_eq!(
            file.shots[0],
            Shot {
                name: "SR-place-Alftand_02".to_owned(),
                worldspace_id: Some(60),
                interior_cell_id: None,
                position: [77000.0, 77500.0, -5200.0],
                yaw: 135.0,
                pitch: 20.0,
                hfov: 75.0,
                open_door: None,
                reference: Some("references/alftand-02.jpg".to_owned()),
                note: Some("free text, ignored by the engine".to_owned()),
            }
        );
    }

    #[test]
    fn a_malformed_open_door_is_rejected_at_load() {
        // Every one of these is an error rather than a FormID read as some other number: a bare
        // hex string without the prefix, digits that are not hex, nothing after the prefix,
        // nothing at all, and a value too wide for a FormID. The last is a JSON number, which the
        // field refuses because a FormID in this file is written as a hex string.
        for (value, wanted) in [
            (r#""1CBB0""#, "FormID"),
            (r#""0x0001CBB0x""#, "FormID"),
            (r#""0x""#, "FormID"),
            (r#""""#, "FormID"),
            (r#""0x100000000""#, "FormID"),
            ("117680", "string"),
        ] {
            let text = format!(
                r#"{{"width": 1400, "height": 1050, "shots": [
                    {{"name": "door", "worldspace_id": 60, "position": [0.0, 0.0, 0.0],
                      "yaw": 0.0, "pitch": 0.0, "hfov": 75.0, "open_door": {value}}}]}}"#
            );
            let error = ShotsFile::from_json(&text).unwrap_err();
            assert!(
                error.message().contains(wanted),
                "open_door {value}: {error}"
            );
        }
        // What a reader gets back names the field's own notation, so the fix is in the message.
        let error =
            ShotsFile::from_json(&door_example().replace("0x0001CBB0", "1CBB0")).unwrap_err();
        assert!(error.message().contains("0x0001CBB0"), "{error}");
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
        assert!(
            shots_settled(&SettleCounts {
                space_resident: false,
                space_failed: true,
                ..quiet_counts()
            }),
            "or have failed for good: nothing more is coming for it"
        );
        assert!(
            !shots_settled(&SettleCounts {
                space_resident: false,
                space_failed: true,
                loading_cells: 1,
                ..quiet_counts()
            }),
            "a failed cell still waits for the rest of the view"
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

    /// The facts of a door shot whose doorway is up: every condition the sequence waits for. The
    /// cases below override the one they are about.
    fn open_doorway() -> DoorFacts {
        DoorFacts {
            door_spawned: true,
            activation_ready: true,
            asked: true,
            open: true,
            destination_resident: true,
            portal_drawn: true,
            swing_finished: true,
            animated: true,
            camera_in_front: true,
        }
    }

    #[test]
    fn a_door_that_opens_is_waited_for_and_then_photographed() {
        // Asked and opened, with the space behind it still streaming in.
        let loading = DoorFacts {
            destination_resident: false,
            ..open_doorway()
        };
        assert_eq!(
            door_step(&loading, 0.5, DOOR_TIMEOUT_SECONDS),
            DoorStep::Waiting
        );
        // The destination arrives: the shot may settle, and what it settles for is the doorway.
        assert_eq!(
            door_step(&open_doorway(), 0.5, DOOR_TIMEOUT_SECONDS),
            DoorStep::Ready
        );

        // Every condition is load-bearing on its own - each of these is a doorway that is not one
        // a player would see, and the shot waits for it rather than photographing it.
        for (missing, facts) in [
            (
                "the door never spawned",
                DoorFacts {
                    door_spawned: false,
                    ..open_doorway()
                },
            ),
            (
                "the door is not open",
                DoorFacts {
                    open: false,
                    ..open_doorway()
                },
            ),
            (
                "the destination is not streamed in",
                DoorFacts {
                    destination_resident: false,
                    ..open_doorway()
                },
            ),
            (
                "the portal is not drawing through it",
                DoorFacts {
                    portal_drawn: false,
                    ..open_doorway()
                },
            ),
            (
                "the leaf is still swinging",
                DoorFacts {
                    swing_finished: false,
                    ..open_doorway()
                },
            ),
            (
                "the camera is not in front of the door",
                DoorFacts {
                    camera_in_front: false,
                    ..open_doorway()
                },
            ),
        ] {
            assert_eq!(
                door_step(&facts, 1.0, DOOR_TIMEOUT_SECONDS),
                DoorStep::Waiting,
                "{missing}"
            );
            assert_eq!(
                door_step(&facts, DOOR_TIMEOUT_SECONDS, DOOR_TIMEOUT_SECONDS),
                DoorStep::TimedOut,
                "{missing}"
            );
        }

        // The two facts the sequence does not read describe how the doorway was reached, not
        // whether it is one: a door that opened with no clip of its own is a doorway too.
        let unasked = DoorFacts {
            asked: false,
            animated: false,
            ..open_doorway()
        };
        assert_eq!(
            door_step(&unasked, 1.0, DOOR_TIMEOUT_SECONDS),
            DoorStep::Ready
        );
    }

    #[test]
    fn a_door_that_never_opens_times_out_with_the_reason() {
        // A door the run never found at all: a mistyped FormID, or one whose cells never streamed
        // in. It waits the whole window and then gives up rather than hanging the run.
        let nothing = DoorFacts::default();
        assert_eq!(
            door_step(&nothing, DOOR_TIMEOUT_SECONDS - 0.1, DOOR_TIMEOUT_SECONDS),
            DoorStep::Waiting
        );
        assert_eq!(
            door_step(&nothing, DOOR_TIMEOUT_SECONDS, DOOR_TIMEOUT_SECONDS),
            DoorStep::TimedOut
        );

        // The log line of that shot says which of the facts was missing, so the reader knows what
        // to fix: the FormID, the file's own pose, or the run's assets.
        let line = nothing.describe();
        for field in [
            "door_spawned=false",
            "open=false",
            "destination_resident=false",
            "portal_drawn=false",
            "camera_in_front=false",
        ] {
            assert!(line.contains(field), "{line}");
        }
        let stuck = DoorFacts {
            portal_drawn: false,
            ..open_doorway()
        };
        assert!(
            stuck.describe().contains("portal_drawn=false"),
            "{}",
            stuck.describe()
        );
    }

    #[test]
    fn the_doorway_quad_counts_only_for_the_door_it_stands_in() {
        let in_the_doorway = Some((Vec3::new(0.0, 88.0, 0.0), true));
        assert!(portal_renders_through_door(
            Vec3::ZERO,
            in_the_doorway,
            true
        ));

        // Any one of the three on its own says the portal is not rendering through this door: the
        // quad hidden, the quad standing in another door's opening, or the portal camera quiet.
        assert!(!portal_renders_through_door(
            Vec3::ZERO,
            Some((Vec3::new(0.0, 88.0, 0.0), false)),
            true
        ));
        assert!(!portal_renders_through_door(
            Vec3::ZERO,
            Some((Vec3::new(0.0, QUAD_DOOR_MAX_DISTANCE + 1.0, 0.0), true)),
            true
        ));
        assert!(!portal_renders_through_door(
            Vec3::ZERO,
            in_the_doorway,
            false
        ));
        assert!(!portal_renders_through_door(Vec3::ZERO, None, true));

        // The boundary: a doorway's own centre offset is tens of units and the doors of a route
        // are hundreds of units apart, so a quad exactly at the limit is still this door's.
        assert!(portal_renders_through_door(
            Vec3::ZERO,
            Some((Vec3::new(0.0, QUAD_DOOR_MAX_DISTANCE, 0.0), true)),
            true
        ));
    }

    #[test]
    fn a_shot_that_names_no_door_skips_the_sequence_entirely() {
        // No door is looked up, so none is asked, watched or closed: the shot goes from its camera
        // straight to the settle, which is the run every existing shots file takes.
        let file = ShotsFile::from_json(DESIGN_EXAMPLE).unwrap();
        let shot = &file.shots[0];
        assert_eq!(shot.open_door, None);
        assert_eq!(phase_after_move(shot), Phase::Settle);
        let run = ShotsRun::new(file, PathBuf::from("out"));
        assert!(run.door.is_none(), "nothing for a close phase to put back");
        assert_eq!(run.phase, Phase::Move);
    }

    #[test]
    fn a_shot_that_names_a_door_opens_it_before_it_settles() {
        let file = ShotsFile::from_json(&door_example()).unwrap();
        let shot = &file.shots[0];
        assert_eq!(shot.open_door, Some(HexFormId(0x0001_CBB0)));
        assert_eq!(phase_after_move(shot), Phase::Door);
        // Nothing is asked until the sequence has found the door in the streamed cells.
        let run = ShotsRun::new(file, PathBuf::from("out"));
        assert!(
            run.door.is_none(),
            "the sequence starts with the shot's own move"
        );
    }

    #[test]
    fn a_door_that_never_opens_fails_that_shot_and_not_the_rest_of_the_file() {
        // What the run does with a timed-out door: the reason goes in the log, this shot is given
        // up on, and the next shot of the file is rendered as if the failed one had never been in
        // it. The run is a failure - a shot that was asked for was not rendered - so it exits
        // non-zero after the others are on disk.
        let mut run = ShotsRun::new(
            ShotsFile::from_json(&two_door_example()).unwrap(),
            PathBuf::from("out"),
        );
        assert_eq!(run.file.shots.len(), 2);
        run.door = Some(DoorRun::new(0x0001_CBB0));
        run.fail(format!(
            "shot \"first\": door {:08X} is not a doorway a player could see through within {DOOR_TIMEOUT_SECONDS:.0} s (portal_drawn=false)",
            0x0001_CBB0
        ));
        assert!(run.failed);
        run.next_shot();
        assert_eq!(run.shot, 1, "the next shot is rendered");
        assert_eq!(run.phase, Phase::Move);
        run.next_shot();
        assert_eq!(run.shot, 2);
        assert_eq!(
            run.phase,
            Phase::Done,
            "and the run ends after the last shot"
        );
        assert!(run.log.contains("FAILED"), "{}", run.log);
        assert!(run.log.contains("portal_drawn=false"), "{}", run.log);
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
